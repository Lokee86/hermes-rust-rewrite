"""Python half of hermes-python-host: the pieces that are plain Python semantics.

The Rust host owns the protocol, the plugin ``ctx`` and every registration; this module only
loads the plugin package, calls plugin callables the way Hermes does (keyword arguments a
callable does not accept are dropped, coroutines are run) and runs pip for dependencies.
"""

import asyncio
import importlib.util
import inspect
import json
import os
import sys
import types

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


def _accepted(fn, kwargs):
    try:
        parameters = inspect.signature(fn).parameters.values()
    except (TypeError, ValueError):
        return kwargs
    if any(p.kind is p.VAR_KEYWORD for p in parameters):
        return kwargs
    names = {p.name for p in parameters if p.kind in (p.POSITIONAL_OR_KEYWORD, p.KEYWORD_ONLY)}
    return {k: v for k, v in kwargs.items() if k in names}


def call(fn, args, kwargs):
    result = fn(*args, **_accepted(fn, kwargs))
    if inspect.isawaitable(result):
        result = asyncio.run(_wait(result))
    return result


async def _wait(awaitable):
    return await awaitable


def dumps(value):
    return value if isinstance(value, str) else json.dumps(value, default=str)


def install(target, requirements):
    # Wheels only: an sdist build would run setup.py through sys.executable, which here is the
    # host, and no plugin code should run at install time anyway.
    from pip._internal.cli.main import main

    return main(["install", "--disable-pip-version-check", "--no-input", "--only-binary=:all:",
                 "--upgrade", "--target", target, *requirements])
