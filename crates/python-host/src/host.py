"""Run the registrations and callbacks in one embedded Python plugin process."""

import asyncio
import importlib.util
import inspect
import json
import os
import sys
import types
from contextlib import nullcontext, suppress

# The protocol owns stdout; anything a plugin prints goes to stderr.
sys.stdout = sys.stderr


def load(plugin_dir, name):
    if "hermes_plugins" not in sys.modules:
        namespace = types.ModuleType("hermes_plugins")
        namespace.__path__ = []
        sys.modules["hermes_plugins"] = namespace
    qualified = f"hermes_plugins.{name}"
    spec = importlib.util.spec_from_file_location(
        qualified, os.path.join(plugin_dir, "__init__.py"), submodule_search_locations=[plugin_dir])
    module = importlib.util.module_from_spec(spec)
    sys.modules[qualified] = module
    spec.loader.exec_module(module)
    return module


def _accepted(fn, args, kwargs):
    try:
        parameters = inspect.signature(fn).parameters.values()
    except (TypeError, ValueError):
        return kwargs
    positional = [p for p in parameters if p.kind in (p.POSITIONAL_ONLY, p.POSITIONAL_OR_KEYWORD)]
    bound = {p.name for p in positional[:len(args)] if p.kind is p.POSITIONAL_OR_KEYWORD}
    any_keyword = any(p.kind is p.VAR_KEYWORD for p in parameters)
    names = {p.name for p in parameters if p.kind in (p.POSITIONAL_OR_KEYWORD, p.KEYWORD_ONLY)}
    return {k: v for k, v in kwargs.items() if k not in bound and (any_keyword or k in names)}


def call(fn, args, kwargs):
    result = fn(*args, **_accepted(fn, args, kwargs))
    if inspect.isawaitable(result):
        result = asyncio.run(_wait(result))
    return result


async def _wait(awaitable):
    return await awaitable


def dumps(value):
    return value if isinstance(value, str) else json.dumps(value, default=str)


_TOOL_HOOKS = {"pre_tool_call", "transform_tool_result", "post_tool_call"}
_LIFECYCLE_HOOKS = {"on_session_end", "on_session_delete"}
_MEMORY_METHODS = {
    "on_turn_start", "system_prompt_block", "prefetch", "recall_status", "get_tool_schemas",
    "handle_tool_call", "sync_turn", "queue_prefetch", "on_session_end",
}


class PluginRegistration:
    def __init__(self, plugin_key, kind, key, release):
        self.plugin_key = plugin_key
        self.kind = kind
        self.key = key
        self._release = release

    @property
    def active(self):
        return self._release is not None

    def dispose(self):
        release, self._release = self._release, None
        if release is not None:
            release()


class _Inference:
    def __init__(self, service):
        self._service = service

    async def _request(self, operation, args, kwargs):
        payload = json.dumps({"args": args, "kwargs": kwargs}, allow_nan=False)
        return json.loads(await asyncio.to_thread(self._service.request, operation, payload))

    async def aembed(self, *args, **kwargs):
        return await self._request("aembed", args, kwargs)

    async def acomplete_structured(self, *args, **kwargs):
        return await self._request("acomplete_structured", args, kwargs)


class _ProviderCollector:
    """Capture a fresh provider while leaving process-level registrations with activation."""

    def __init__(self, host):
        self.plugin_id = host.plugin_id
        self.get_config = host.get_config
        self.provider = None

    def register_memory_provider(self, provider):
        self.provider = provider

    def on_unload(self, callback):
        pass

    def __getattr__(self, name):
        if name.startswith("register_"):
            return lambda *args, **kwargs: None
        raise AttributeError(name)


class PluginHost:
    def __init__(self, plugin_id, compat, service, version):
        self.plugin_id = plugin_id
        self._compat = compat
        self._register = None
        self._commands = {}
        self._tools = {}
        self._hooks = []
        self._unloads = []
        self._unsupported = set()
        self._sessions = {}
        self._provider = None
        self._closed = False
        self._host_version = version
        if service is not None:
            self.llm = _Inference(service)

    def activate(self, plugin_dir):
        module_name = "".join(ch if ch.isascii() and ch.isalnum() else "_" for ch in self.plugin_id)
        self._register = load(plugin_dir, module_name).register
        call(self._register, (self,), {})

    def get_config(self, key, default=None):
        name = "".join(ch.upper() if ch.isascii() and ch.isalnum() else "_" for ch in key)
        return os.environ.get(f"PLUGIN_CONFIG_{name}", default)

    def __getattr__(self, name):
        if name.startswith("register_"):
            def skipped(*args, **kwargs):
                self._unsupported.add(name)

            return skipped
        raise AttributeError(f"PluginHost.{name} is unavailable")

    def register_tool(self, name, toolset, schema, handler, check_fn=None, requires_env=None,
                      is_async=False, description="", emoji="", override=False):
        if not name or not all(ch.isascii() and (ch.isalnum() or ch in "_-") for ch in name):
            raise ValueError(f"Tool name {name!r} must be letters, digits, _ or -")
        entry = (schema, handler, check_fn, [key for key in requires_env or [] if isinstance(key, str)], description)
        self._tools.pop(name, None)
        self._tools[name] = entry

        def remove():
            if self._tools.get(name) is entry:
                del self._tools[name]

        return PluginRegistration(self.plugin_id, "tool", name, remove)

    def register_command(self, name, handler, description="", args_hint="", argument_mode=None):
        name = name.lower().strip().lstrip("/").replace(" ", "-")
        if not name or not name.isprintable() or any(ch.isspace() for ch in name):
            self._unsupported.add("register_command:invalid-name")
            return None
        if not callable(handler):
            raise TypeError("Slash command handler must be callable")
        hint = (args_hint or "").strip()
        if argument_mode not in {"options", "text", "mixed"}:
            argument_mode = "text" if hint else None
        entry = ({"name": name, "description": description or "Plugin command", "args_hint": hint,
                  "argument_mode": argument_mode}, handler)
        registrations = self._commands.setdefault(name, [])
        registrations.append(entry)

        def remove():
            registrations.remove(entry)
            if not registrations:
                del self._commands[name]

        return PluginRegistration(self.plugin_id, "command", name, remove)

    def register_hook(self, hook_name, callback, hold_stream=False):
        if hook_name not in _TOOL_HOOKS | _LIFECYCLE_HOOKS:
            self._unsupported.add(f"hook:{hook_name}")
            return None
        entry = (hook_name, callback)
        self._hooks.append(entry)
        return PluginRegistration(self.plugin_id, "hook", hook_name, lambda: self._hooks.remove(entry))

    def on_unload(self, callback):
        self._unloads.append(callback)
        return PluginRegistration(self.plugin_id, "on_unload", "on_unload", lambda: self._unloads.remove(callback))

    def register_memory_provider(self, provider):
        if not callable(getattr(provider, "initialize", None)):
            raise TypeError("Memory provider must provide initialize()")
        if self._provider is not None:
            raise ValueError("Plugin already registered a memory provider")
        self._provider = provider

    def _scope(self, context):
        return self._compat.session_scope(context) if self._compat is not None else nullcontext()

    def _tool_available(self, entry):
        _, _, check, requires_env, _ = entry
        if any(not os.environ.get(key) for key in requires_env):
            return False
        try:
            return check is None or bool(check())
        except Exception:
            return False

    def _list_tools(self):
        tools = []
        for name, entry in list(self._tools.items()):
            if self._tool_available(entry):
                schema, _, _, _, description = entry
                if isinstance(schema, str):
                    with suppress(ValueError):
                        schema = json.loads(schema)
                if not isinstance(schema, dict):
                    schema = {}
                parameters = schema.get("parameters")
                tools.append({"name": name, "description": schema.get("description") or description,
                              "inputSchema": parameters if isinstance(parameters, dict) else {"type": "object"}})
        return {"tools": tools}

    def _call_tool(self, params):
        name = params.get("name", "")
        entry = self._tools.get(name)
        if entry is None or not self._tool_available(entry):
            raise ValueError(f"Unknown tool: {name}")
        try:
            text = dumps(call(entry[1], (params.get("arguments", {}),), {}))
            try:
                decoded = json.loads(text)
            except ValueError:
                decoded = None
            failed = isinstance(decoded, dict) and "error" in decoded
        except Exception as exc:
            text, failed = json.dumps({"error": str(exc)}), True
        return {"content": [{"type": "text", "text": text}], "isError": failed}

    def _describe(self):
        return {"commands": [items[-1][0] for items in self._commands.values()],
                "provider": str(getattr(self._provider, "name", self.plugin_id)) if self._provider is not None else None,
                "unsupported": sorted(self._unsupported)}

    def _command(self, params):
        name = params["name"]
        handlers = self._commands.get(name)
        if not handlers:
            raise ValueError(f"Unknown command: /{name}")
        context = params.get("context", {})
        accepted = {key: value for key, value in context.items() if key in {"source", "session_id", "session_key", "platform"}}
        if isinstance(accepted.get("source"), dict):
            accepted["source"] = types.SimpleNamespace(**accepted["source"])
        with self._scope(context):
            result = call(handlers[-1][1], (params.get("arguments", ""),), accepted)
        return None if result is None else str(result)

    def _run_hook(self, params):
        name = params["name"]
        results = []
        with self._scope(params.get("context", {})):
            for hook, callback in self._hooks[:]:
                if hook != name:
                    continue
                try:
                    result = call(callback, (), params.get("kwargs", {}))
                    if result is not None:
                        text = dumps(result)
                        try:
                            results.append(json.loads(text))
                        except ValueError:
                            results.append(text)
                except Exception as exc:
                    if name in _LIFECYCLE_HOOKS:
                        raise
                    print(f"{self.plugin_id}: {name} hook failed: {exc}", file=sys.stderr)
        return {"results": results}

    def _memory_open(self, session_id, context):
        if session_id in self._sessions:
            return {"available": True}
        if self._provider is None:
            return {"available": False, "reason": "The plugin did not register a memory provider"}
        context = {**context, "session_id": session_id}
        with self._scope(context):
            collector = _ProviderCollector(self)
            call(self._register, (collector,), {})
            provider = collector.provider
            if provider is None or provider is self._provider or any(
                provider is existing for existing, _ in self._sessions.values()
            ):
                raise ValueError("Memory registration must supply a fresh provider instance for each session")
            if hasattr(provider, "is_available") and not call(provider.is_available, (), {}):
                reason = getattr(provider, "unavailable_reason", lambda: None)()
                return {"available": False, "reason": reason or "The provider is unavailable"}
            # The caller supplies identity and workspace fields, and the worker pins the profile.
            kwargs = dict(context)
            kwargs.pop("session_id")
            if os.environ.get("HERMES_HOME"):
                kwargs["hermes_home"] = os.environ["HERMES_HOME"]
            try:
                call(provider.initialize, (session_id,), kwargs)
            except Exception:
                self._shutdown_provider(provider)
                raise
        self._sessions[session_id] = (provider, context)
        return {"available": True}

    def _memory_call(self, params):
        method = params["method"]
        if method not in _MEMORY_METHODS:
            raise ValueError(f"Unsupported provider method: {method}")
        ready = self._sessions.get(params["session_id"])
        if ready is None:
            raise ValueError("The session's memory provider is unavailable")
        provider, context = ready
        callback = getattr(provider, method, None)
        if callback is None:
            return None
        with self._scope(context):
            result = call(callback, tuple(params.get("args", ())), params.get("kwargs", {}))
        if method == "recall_status" and result is not None and not isinstance(result, dict):
            return {"provider_label": result.provider_label, "count": result.count, "glyph": result.glyph}
        return result

    def _shutdown_provider(self, provider):
        shutdown = getattr(provider, "shutdown", None)
        if shutdown is not None:
            call(shutdown, (), {})

    def _memory_close(self, session_id):
        ready = self._sessions.pop(session_id, None)
        if ready is not None:
            provider, context = ready
            with self._scope(context):
                self._shutdown_provider(provider)

    def close(self):
        if self._closed:
            return
        self._closed = True
        errors = []
        for session_id in list(self._sessions):
            try:
                self._memory_close(session_id)
            except Exception as exc:
                errors.append(str(exc))
        for callback in reversed(self._unloads[:]):
            try:
                call(callback, (), {})
            except Exception as exc:
                errors.append(str(exc))
        if errors:
            raise RuntimeError("; ".join(errors))

    def _dispatch(self, method, params):
        if method == "initialize":
            return {"protocolVersion": params.get("protocolVersion", "2025-06-18"),
                    "capabilities": {"tools": {}, "experimental": {"hermes": {
                        "hooks": sorted({name for name, _ in self._hooks}),
                    }}}, "serverInfo": {"name": self.plugin_id, "version": self._host_version}}
        if method == "ping":
            return {}
        if method == "tools/list":
            return self._list_tools()
        if method == "tools/call":
            return self._call_tool(params)
        if method == "hermes/describe":
            return self._describe()
        if method == "hermes/command":
            return self._command(params)
        if method == "hermes/hook":
            return self._run_hook(params)
        if method == "hermes/memory.open":
            return self._memory_open(params["session_id"], params.get("context", {}))
        if method == "hermes/memory.call":
            return self._memory_call(params)
        if method == "hermes/memory.close":
            return self._memory_close(params["session_id"])
        if method == "hermes/history" and self._compat is not None:
            return self._compat.project(params)
        if method == "hermes/unload":
            return self.close()
        raise ValueError(f"Unsupported method: {method}")

    def dispatch_json(self, method, encoded):
        return json.dumps(self._dispatch(method, json.loads(encoded)), default=str, allow_nan=False)


def start(plugin_dir, name, packages, home, compat, service, version):
    if packages:
        sys.path.insert(0, packages)
    if home:
        compat.install(home)
    host = PluginHost(name, compat if home else None, service, version)
    try:
        host.activate(plugin_dir)
    except BaseException:
        with suppress(Exception):
            host.close()
        raise
    return host


def install(target, requirements):
    # Wheels only: an sdist build would run setup.py through sys.executable, which here is the
    # host, and no plugin code should run at install time anyway.
    from pip._internal.cli.main import main

    status = main(["install", "--disable-pip-version-check", "--no-input", "--only-binary=:all:",
                   "--upgrade", "--target", target, *requirements])
    if status == 0:
        import sysconfig
        from pathlib import Path

        suffix = sysconfig.get_config_var("EXT_SUFFIX")
        if sys.implementation.name == "cpython" and suffix:
            marker_path = Path(target) / ".hermes-python-abi"
            marker_path.unlink(missing_ok=True)
            with marker_path.open("x", encoding="ascii") as marker:
                marker.write(suffix)
    return status
