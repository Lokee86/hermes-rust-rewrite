"""Stock Python extension surfaces backed by the Rust profile owner."""

from abc import ABC, abstractmethod
from contextlib import closing, contextmanager
from contextvars import ContextVar, copy_context
from dataclasses import dataclass
import errno
import json
import math
import os
from pathlib import Path
import re
import sqlite3
from stat import S_ISREG
import sys
import tempfile
import threading
import time
from types import ModuleType


_HOME = None
_PROFILE = None
_OVERRIDE = ContextVar("hermes_home_override", default=None)
_WORKSPACE = ContextVar("hermes_session_cwd", default="")
_MAX_PROFILE_FILE = 1024 * 1024
_HISTORY_ROWS = 128
_HISTORY_BYTES = 128 * 1024
_APPLICATION_ID = 0x48525048
_SCHEMA_VERSION = 1
_KEY = re.compile(r"[A-Za-z_][A-Za-z0-9_]*\Z")


def get_hermes_home():
    if _HOME is None:
        raise RuntimeError("No Hermes profile is bound to the plugin host")
    return _HOME


def profile_name_for_home(home):
    if home is None or _HOME is None:
        return None
    return _PROFILE if Path(home).resolve() == _HOME else None


def _profile_path(path):
    path = Path(path)
    home = get_hermes_home()
    if not path.is_absolute() or not path.is_relative_to(home) or path.resolve() != path:
        raise ValueError("Path is outside the bound Hermes profile")
    return path


def set_hermes_home_override(home):
    if profile_name_for_home(home) is None:
        raise ValueError("Cannot change the bound Hermes profile")
    return _OVERRIDE.set(str(_HOME))


def reset_hermes_home_override(token):
    _OVERRIDE.reset(token)


def get_default_hermes_root():
    home = get_hermes_home()
    return home.parent.parent if home.parent.name == "profiles" else home


def set_session_cwd(path):
    return _WORKSPACE.set(os.fspath(path) if path else "")


def resolve_context_cwd(*, include_session_override=True):
    path = _WORKSPACE.get() if include_session_override else ""
    return Path(path) if path else None


@contextmanager
def session_scope(context):
    token = set_session_cwd(
        context.get("cwd") or context.get("agent_workspace") or context.get("workspace")
    )
    try:
        yield
    finally:
        _WORKSPACE.reset(token)


def ctx_bound(fn):
    context = copy_context()
    return lambda *args, **kwargs: context.run(fn, *args, **kwargs)


def spawn_context_thread(target, *, name, daemon=True, args=(), kwargs=None):
    return threading.Thread(target=ctx_bound(target), name=name, daemon=daemon,
                            args=args, kwargs=kwargs)


def _read_profile_file(name):
    path = _profile_path(get_hermes_home() / name)
    with path.open("rb") as stream:
        if not S_ISREG(os.fstat(stream.fileno()).st_mode):
            raise ValueError("Profile data must be a regular file")
        data = stream.read(_MAX_PROFILE_FILE + 1)
    if len(data) > _MAX_PROFILE_FILE:
        raise ValueError("Profile data exceeds the compatibility limit")
    return data.decode("utf-8-sig")


def get_secret(name, default=None):
    if _HOME is None or not isinstance(name, str) or not _KEY.fullmatch(name):
        return default
    configured = os.environ.get(f"PLUGIN_CONFIG_{name.upper()}")
    if configured:
        return configured
    try:
        data = _read_profile_file(".env")
    except FileNotFoundError:
        return default
    value = default
    for line in data.splitlines():
        key, separator, raw = line.strip().removeprefix("export ").partition("=")
        if not separator or key.strip() != name:
            continue
        raw = raw.strip()
        if raw[:1] in {'"', "'"}:
            if len(raw) >= 2 and raw[-1] == raw[0]:
                value = raw[1:-1]
        else:
            value = raw.partition(" #")[0].rstrip()
    return value


@contextmanager
def _file_lock(lock_path, holder, timeout_seconds, timeout_message):
    path = _profile_path(lock_path)
    held = getattr(holder, "held", set())
    if path in held:
        yield
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    flags = os.O_RDWR | os.O_CREAT | getattr(os, "O_NOFOLLOW", 0)
    with os.fdopen(os.open(path, flags, 0o600), "r+b", buffering=0) as handle:
        if not S_ISREG(os.fstat(handle.fileno()).st_mode):
            raise ValueError("Profile lock must be a regular file")
        if os.name == "nt":
            import msvcrt

            def lock():
                handle.seek(0)
                msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
        else:
            import fcntl

            def lock():
                fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        deadline = time.monotonic() + max(0, timeout_seconds)
        while True:
            try:
                lock()
                break
            except OSError as error:
                if error.errno not in (errno.EACCES, errno.EAGAIN, errno.EDEADLK):
                    raise
                if time.monotonic() >= deadline:
                    raise TimeoutError(timeout_message) from error
                time.sleep(0.05)
        holder.held = held
        held.add(path)
        try:
            yield
        finally:
            held.remove(path)
            # Closing also releases the kernel lock on either platform.


def atomic_json_write(path, value, *, mode=0o600, fsync_dir=False):
    destination = _profile_path(path)
    destination.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temp = tempfile.mkstemp(dir=destination.parent)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as output:
            if hasattr(os, "fchmod"):
                os.fchmod(output.fileno(), mode)
            json.dump(value, output, ensure_ascii=False, allow_nan=False)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temp, destination)
        if fsync_dir and os.name != "nt":
            with _directory_handle(destination.parent) as directory:
                os.fsync(directory)
    finally:
        Path(temp).unlink(missing_ok=True)


@contextmanager
def _directory_handle(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        yield descriptor
    finally:
        os.close(descriptor)


@dataclass(frozen=True)
class RecallStatus:
    provider_label: str
    count: int
    glyph: str = "🧠"


class MemoryProvider(ABC):
    @property
    @abstractmethod
    def name(self):
        """Return the provider's stable name."""

    @abstractmethod
    def is_available(self):
        """Report whether this provider can be initialized."""

    @abstractmethod
    def initialize(self, session_id, **kwargs):
        """Initialize a provider object for a specific session."""

    @abstractmethod
    def get_tool_schemas(self):
        """Return function schemas for the provider's tools."""

    def unavailable_reason(self):
        return ""

    def system_prompt_block(self):
        return ""

    def prefetch(self, query, *, session_id=""):
        return ""

    def queue_prefetch(self, query, *, session_id=""):
        pass

    def recall_status(self):
        return None

    def sync_turn(self, user_content, assistant_content, *,
                  session_id="", messages=None, turn_author=None):
        pass

    def handle_tool_call(self, tool_name, args, **kwargs):
        raise NotImplementedError(f"Provider {self.name} does not handle tool {tool_name}")

    def on_turn_start(self, turn_number, message, **kwargs):
        pass

    def on_session_end(self, messages):
        pass

    def shutdown(self):
        pass


def _provider_id(value):
    if not isinstance(value, str) or not value:
        raise ValueError("Missing memory selection")
    return value if value.startswith("plugin:") else f"plugin:{value}"


def _configured_selection():
    state = json.loads(_read_profile_file("agent-settings.json"))
    revision = state.get("memory_provider_revision", 0)
    provider = state.get("memory_provider")
    if not isinstance(provider, str) or not provider.startswith("plugin:") \
            or type(revision) is not int or revision < 0:
        raise ValueError("Python memory provider is not selected")
    return provider, revision


def _canonical_db(path):
    expected = get_hermes_home() / "state.db"
    if Path(path) != expected:
        raise sqlite3.OperationalError("Session store is outside the bound Hermes profile")
    info = expected.lstat()
    if not S_ISREG(info.st_mode) or expected.resolve(strict=True) != expected:
        raise sqlite3.OperationalError("Session store is not a canonical regular file")
    return expected


def _open_owned(path, *, read_only):
    mode = "ro" if read_only else "rw"
    db = sqlite3.connect(_canonical_db(path).as_uri() + f"?mode={mode}", uri=True, timeout=5)
    try:
        app_id = db.execute("PRAGMA application_id").fetchone()[0]
        version = db.execute("PRAGMA user_version").fetchone()[0]
        if app_id != _APPLICATION_ID or version != _SCHEMA_VERSION:
            raise sqlite3.OperationalError("Existing state.db is not an owned Hermes Rust projection")
        owner = db.execute("SELECT home FROM projection_owner").fetchone()
        if owner != (str(get_hermes_home()),):
            raise sqlite3.OperationalError("Session projection belongs to a different profile")
        return db
    except BaseException:
        db.close()
        raise


def _ensure_projection(path):
    if os.path.lexists(path):
        with closing(_open_owned(path, read_only=True)):
            return
    # Publish a fully initialized database without replacing a competing or foreign file.
    with tempfile.TemporaryDirectory(prefix=".state-projection-", dir=path.parent) as temp:
        staging = Path(temp) / "state.db"
        with closing(sqlite3.connect(staging)) as db, db:
            db.executescript(f"""
                PRAGMA application_id={_APPLICATION_ID};
                PRAGMA user_version={_SCHEMA_VERSION};
                CREATE TABLE projection_owner (home TEXT NOT NULL);
                CREATE TABLE projection_sessions (
                    id TEXT PRIMARY KEY, provider TEXT NOT NULL,
                    selection_revision TEXT NOT NULL, revision TEXT NOT NULL,
                    session TEXT NOT NULL, messages TEXT NOT NULL, complete INTEGER NOT NULL
                );
                CREATE TABLE projection_terminal (id TEXT PRIMARY KEY);
            """)
            db.execute("INSERT INTO projection_owner VALUES (?)", (str(get_hermes_home()),))
        if os.name != "nt":
            staging.chmod(0o600)
        try:
            if os.name == "nt":
                # Windows rename fails when the destination exists.
                staging.rename(path)
            else:
                os.link(staging, path)
        except FileExistsError:
            pass
    with closing(_open_owned(path, read_only=True)):
        return


def _encode(projected):
    return json.dumps(projected, ensure_ascii=False, allow_nan=False, separators=(",", ":"))


def _snapshot_messages(messages):
    if not isinstance(messages, list) or len(messages) > _HISTORY_ROWS:
        raise ValueError("Projected history exceeds the row limit")
    ids = set()
    for row in messages:
        uid, content, timestamp = row["message_uid"], row["content"], row["timestamp"]
        if (not isinstance(uid, str) or not uid or uid in ids
                or row["role"] not in ("user", "assistant")
                or not isinstance(content, str) or len(content.encode()) > 32768
                or type(timestamp) not in (int, float)
                or not math.isfinite(timestamp) or timestamp <= 0):
            raise ValueError("Invalid committed message for session projection")
        ids.add(uid)
    text = _encode(messages)
    if len(text.encode()) > _HISTORY_BYTES:
        raise ValueError("Projected history exceeds the byte limit")
    return text


def project(snapshot):
    """Update one read model from the gateway's durable transcript or terminal fence."""
    path = get_hermes_home() / "state.db"
    _ensure_projection(path)
    if "delete" in snapshot:
        session_id = snapshot["delete"]
        if not isinstance(session_id, str) or not session_id:
            raise ValueError("Invalid terminal session identity")
        with closing(_open_owned(path, read_only=False)) as db, db:
            db.execute("INSERT OR IGNORE INTO projection_terminal VALUES (?)", (session_id,))
            db.execute("DELETE FROM projection_sessions WHERE id=?", (session_id,))
        return
    session = snapshot["session"]
    session_id = session["id"]
    if not isinstance(session_id, str) or not session_id or session["profile_name"] != _PROFILE:
        raise ValueError("Invalid session identity in profile projection")
    if session["archived"]:
        project({"delete": session_id})
        return
    selection = (_provider_id(snapshot["provider"]), snapshot["selection_revision"])
    revision = snapshot["revision"]
    if (type(selection[1]) is not int or selection != _configured_selection()
            or type(revision) is not int or revision < 0):
        raise ValueError("Session projection requires the current provider selection")
    messages = _snapshot_messages(snapshot["messages"])
    with closing(_open_owned(path, read_only=False)) as db, db:
        db.execute("BEGIN IMMEDIATE")
        if db.execute("SELECT 1 FROM projection_terminal WHERE id=?", (session_id,)).fetchone():
            return
        previous = db.execute(
            "SELECT selection_revision, revision FROM projection_sessions WHERE id=?",
            (session_id,),
        ).fetchone()
        if previous is not None and tuple(map(int, previous)) > (selection[1], revision):
            return
        db.execute("""
            INSERT INTO projection_sessions VALUES (?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (id) DO UPDATE SET provider=excluded.provider,
                selection_revision=excluded.selection_revision, revision=excluded.revision,
                session=excluded.session, messages=excluded.messages, complete=excluded.complete
        """, (session_id, selection[0], str(selection[1]), str(revision),
              _encode(session), messages, bool(snapshot.get("complete", False))))


class SessionDB:
    """The bounded stock history interface for the current provider selection."""

    HISTORY_EXPORT_VERSION = 1

    def __init__(self, path, *, read_only=False):
        if not read_only:
            raise sqlite3.OperationalError("The Hermes Rust history projection is read-only")
        self._db = _open_owned(path, read_only=True)

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()

    def close(self):
        self._db.close()

    def _row(self, session_id):
        try:
            provider, revision = _configured_selection()
        except (OSError, ValueError):
            return None
        return self._db.execute("""
            SELECT session, messages, complete FROM projection_sessions
            WHERE id=? AND provider=? AND selection_revision=?
        """, (session_id, provider, str(revision))).fetchone()

    def get_session(self, session_id):
        row = self._row(session_id)
        return json.loads(row[0]) if row is not None else None

    def export_history(self, session_id, *, profile_id, max_messages=_HISTORY_ROWS):
        def unavailable(reason):
            return {"status": "unavailable", "reason": reason}
        if profile_id != _PROFILE or (row := self._row(session_id)) is None:
            return unavailable("history_session_unavailable")
        session = json.loads(row[0])
        if session["hidden"] or session["archived"] or not row[2]:
            return unavailable("history_identity_unavailable")
        if type(max_messages) is not int or not 0 < max_messages <= _HISTORY_ROWS:
            return unavailable("history_unsupported")
        messages = json.loads(row[1])
        if len(messages) > max_messages:
            return unavailable("history_too_large")
        return {"status": "ready", "profile_id": profile_id, "session_id": session_id,
                "started_at": session["started_at"], "messages": messages}


def _module(name, **exports):
    module = ModuleType(name)
    module.__dict__.update(exports)
    sys.modules[name] = module
    parent, _, child = name.rpartition(".")
    if parent:
        setattr(sys.modules[parent], child, module)


def install(home):
    """Install the supported stock imports before executing the plugin package."""
    global _HOME, _PROFILE
    if home:
        path = Path(home)
        if not path.is_absolute() or not path.is_dir():
            raise ValueError("Hermes plugin host needs an existing absolute profile home")
        resolved = path.resolve(strict=True)
        if _HOME is not None and _HOME != resolved:
            raise ValueError("Cannot rebind the plugin host to another Hermes profile")
        _HOME = resolved
        inferred = _HOME.name if _HOME.parent.name == "profiles" else "default"
        _PROFILE = os.environ.get("HERMES_PROFILE_NAME") or inferred
    _module("hermes_constants", get_hermes_home=get_hermes_home,
            get_hermes_home_override=_OVERRIDE.get,
            get_routing_process_hermes_home=get_hermes_home,
            get_default_hermes_root=get_default_hermes_root,
            set_hermes_home_override=set_hermes_home_override,
            reset_hermes_home_override=reset_hermes_home_override,
            profile_name_for_home=profile_name_for_home)
    _module("hermes_state", SessionDB=SessionDB)
    _module("agent", __path__=[])
    _module("agent.memory_provider", MemoryProvider=MemoryProvider, RecallStatus=RecallStatus,
            spawn_context_thread=spawn_context_thread, ctx_bound=ctx_bound)
    _module("agent.secret_scope", get_secret=get_secret,
            current_secret_scope_home=lambda: str(_HOME) if _HOME is not None else None,
            is_multiplex_active=lambda: False)
    _module("agent.runtime_cwd", set_session_cwd=set_session_cwd,
            reset_session_cwd=_WORKSPACE.reset, scoped_session_cwd=_WORKSPACE.get,
            clear_session_cwd=lambda: _WORKSPACE.set(""),
            scope_terminal_cwd=_WORKSPACE.get, resolve_context_cwd=resolve_context_cwd,
            resolve_agent_cwd=lambda: resolve_context_cwd() or get_hermes_home())
    _module("hermes_cli", __path__=[])
    _module("hermes_cli.auth", _file_lock=_file_lock)
    _module("utils", atomic_json_write=atomic_json_write)
