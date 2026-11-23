"""Compare a native composer session on the pinned stock and Rust gateway owners.

Use the installed Hermes venv, e.g.:
  python scripts/verify_stock_composer_shell.py --source-checkout ../hermes-agent \
      --gateway-exe target/debug/hermes-gateway

On Windows pass hermes-gateway.exe. Omit --gateway-exe for a stock-only baseline.
Both owners run in the foreground in isolated temporary profiles, and requests use
the pinned Python client's normal discovery, private-control ticket, and WebSocket.
"""
from __future__ import annotations

import argparse
import asyncio
from contextlib import AsyncExitStack, ExitStack, contextmanager
import hashlib
import json
import os
import shlex
import shutil
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import time


PINNED_STOCK_SHA = "caafe5389ae585ef77387a962aee05b1bcf17029"
WINDOWS = os.name == "nt"
SYNTHETIC_SECRET = "sk-or-v1-" + "f" * 48
OMITTED = object()
PROBE = r"""
import json
import os
import sys
import time

mode = sys.argv[1]
out = sys.stdout.buffer
err = sys.stderr.buffer
if mode == "args":
    print(json.dumps(sys.argv[2:], ensure_ascii=True))
elif mode == "stdin":
    print(len(sys.stdin.buffer.read()))
elif mode == "cwd-env":
    print(json.dumps({
        "cwd": os.getcwd(),
        "fixture_env": os.getenv("COMPOSER_FIXTURE_ENV"),
        "owner_key_inherited": "OPENROUTER_API_KEY" in os.environ,
    }, sort_keys=True))
elif mode == "streams":
    out.write(b"out\r\nline\rfinal\n")
    err.write(b"err\r\nlast\r")
    sys.exit(23)
elif mode == "bytes":
    out.write("café 雪 😀".encode() + b"\x00\xff\xe2\x82x")
    err.write(b"\xed\xa0\x80\xff")
elif mode == "controls":
    out.write(b"left\x00\x01\x1b[31mright\x1b[0m\n")
    err.write(b"\x07\x08end")
elif mode == "redaction":
    out.write(b"key=sk-or-v1-" + b"f" * 48 + b"\n")
    err.write(b"https://fixture-user:fixture-password@fixture.invalid/path\n")
elif mode == "large":
    # Fill both pipes past their OS buffer capacities. The final slices must be
    # sized in Unicode characters and redaction has to precede those slices.
    credential = b"sk-or-v1-" + b"f" * 48
    out.write(b"x" * 393216 + b" " + credential + b" " + ("雪" * 3980 + " stdout").encode())
    err.write(b"y" * 393216 + b" " + credential + b" " + ("é" * 1980 + " stderr").encode())
elif mode == "timeout":
    with open("timeout-started", "wb"):
        pass
    out.write(b"partial fixture output\n")
    out.flush()
    time.sleep(31.5)
else:
    raise SystemExit(99)
"""

CONFIG = """gateway:
  multiplex_profiles: false
  standalone: true
model:
  provider: custom
  default: no-inference
  context_length: 131072
  base_url: http://127.0.0.1:1/v1
auxiliary:
  title_generation:
    enabled: false
terminal:
  backend: {backend}
"""


def stock_source(checkout, scratch):
    """Use archive content so unrelated edits to the caller's checkout are immaterial."""
    archive = scratch / "stock.tar"
    subprocess.run(["git", "archive", "--format=tar", f"--output={archive}", PINNED_STOCK_SHA],
                   cwd=checkout, check=True)
    source = scratch / "stock"
    source.mkdir()
    native_tar = shutil.which("tar")
    if native_tar:
        subprocess.run([native_tar, "-xf", str(archive), "-C", str(source)], check=True)
    else:
        with tarfile.open(archive) as tar:
            tar.extractall(source, filter="data")
    archive.unlink()
    return source


def fixture_env(profile, user):
    system_root = os.environ.get("SYSTEMROOT", r"C:\Windows")
    paths = [str(Path(sys.executable).parent)]
    if WINDOWS:
        paths.extend([str(Path(system_root) / "System32"), system_root])
    else:
        paths.extend(["/usr/bin", "/bin"])
    user.mkdir()
    home_vars = {
        "HOME": str(user),
        "USERPROFILE": str(user),
        "LOCALAPPDATA": str(user / "local"),
        "APPDATA": str(user / "roaming"),
        "TMP": str(user),
        "TEMP": str(user),
        "TMPDIR": str(user),
    }
    env = {
        "PATH": os.pathsep.join(paths),
        "SYSTEMROOT": system_root,
        "WINDIR": system_root,
        "COMSPEC": str(Path(system_root) / "System32" / "cmd.exe"),
        "PYTHONUTF8": "1",
        "PYTHONDONTWRITEBYTECODE": "1",
        "HERMES_HOME": str(profile),
        "HERMES_RUST_DATA_DIR": str(profile),
        "OPENROUTER_API_KEY": SYNTHETIC_SECRET,
        "HERMES_RUST_API_KEY": SYNTHETIC_SECRET,
        "HERMES_RUST_BASE_URL": "http://127.0.0.1:1/v1",
        "HERMES_RUST_MODEL": "no-inference",
        "COMPOSER_FIXTURE_ENV": "ordinary fixture value",
        **home_vars,
    }
    return env


@contextmanager
def owner(name, source, gateway_exe, root, backend):
    home, user, work = (root / name / part for part in ("profile", "user", "working dir"))
    home.mkdir(parents=True)
    work.mkdir()
    (home / "config.yaml").write_text(CONFIG.format(backend=backend), encoding="utf-8")
    (work / "probe.py").write_text(PROBE, encoding="utf-8")
    (work.parent / "shared notes.txt").write_bytes(b"sibling fixture text\n")
    env = fixture_env(home, user)

    if gateway_exe is None:
        argv = [sys.executable, "-m", "hermes_cli.main", "gateway", "run", "--no-supervise"]
    else:
        launcher = [] if gateway_exe.stem == "hermes-gateway" else ["gateway", "run"]
        argv = [str(gateway_exe), *launcher, "--ephemeral-token", "--host", "127.0.0.1",
                "--port", "0", "--data-dir", str(home)]
    log = root / f"{name}.log"
    # A regular file keeps child logs flowing without introducing extra reader threads.
    with log.open("wb") as output:
        process = subprocess.Popen(argv, cwd=source, env=env, stdin=subprocess.DEVNULL,
                                   stdout=output, stderr=subprocess.STDOUT)
        try:
            yield home, work, process
        except BaseException:
            tail = log.read_bytes()[-6000:].decode("utf-8", errors="replace")
            print(f"{name} gateway log:\n{tail}", file=sys.stderr)
            raise
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)


async def discover(home, process):
    from hermes_cli.gateway_runtime import discover_gateway_endpoint

    deadline = time.monotonic() + 50
    while True:
        if process.poll() is not None:
            raise RuntimeError(f"gateway exited during startup with code {process.returncode}")
        found = await asyncio.to_thread(discover_gateway_endpoint, home)
        if found.state == "ready" and found.endpoint is not None:
            return found.endpoint
        if found.state == "incompatible":
            raise RuntimeError(f"gateway discovery rejected {home}: {found.reason_code}")
        if time.monotonic() >= deadline:
            raise TimeoutError(f"gateway startup timed out for {home}: {found}")
        await asyncio.sleep(0.1)


def snapshot_state(snapshot):
    messages = snapshot["messages"]
    assert isinstance(messages, list)
    return {
        "messages": messages,
        "running": snapshot["running"],
        "pending": snapshot["pending"],
        "prompts": snapshot["prompts"],
        "open_requests": snapshot["open_requests"],
        "revision": snapshot["revision"],
        "last_sequence": snapshot["last_sequence"],
    }


async def capture(client, session_id=OMITTED, command=OMITTED, **extra):
    from hermes_cli.gateway_client import GatewayRPCError

    params = dict(extra)
    if session_id is not OMITTED:
        params["session_id"] = session_id
    if command is not OMITTED:
        params["command"] = command
    try:
        return await client.rpc("shell.exec", _timeout=45, **params)
    except GatewayRPCError as exc:
        return {"error": {"code": exc.code, "message": str(exc), "data": exc.data}}


def commands(skip_timeout):
    python = "python.exe" if WINDOWS else shlex.quote(Path(sys.executable).name)
    probe = f"{python} probe.py"
    cases = {
        "quoted_arguments": f'{probe} args "two words" "left|right" ""',
        "pipe_and_redirect": ('echo fixture-pipe | findstr /C:fixture-pipe > "captured file.txt"'
                              if WINDOWS else 'printf "%s\\n" "fixture-pipe" | cat > "captured file.txt"'),
        "shell_environment": ('echo %COMPOSER_FIXTURE_ENV%' if WINDOWS
                              else 'printf "%s\\n" "$COMPOSER_FIXTURE_ENV"'),
        "closed_stdin": f"{probe} stdin",
        "nonzero_and_crlf": f"{probe} streams",
        "frozen_cwd_and_env": f"{probe} cwd-env",
        "unicode_and_invalid_utf8": f"{probe} bytes",
        "control_characters": f"{probe} controls",
        "synthetic_redaction": f"{probe} redaction",
        "large_redacted_tails": f"{probe} large",
        "empty_command": "  ",
        "nonstring_command": 17,
    }
    if not skip_timeout:
        cases["timeout"] = f"{probe} timeout"
    return cases


def normalize(case, reply, work):
    if case != "frozen_cwd_and_env" or reply.get("code") != 0:
        return reply
    # Each owner sees a different absolute root. Substitute only that root
    # and preserve the rest of the literal JSON reply and its newline.
    cwd = json.loads(reply["stdout"])["cwd"]
    if Path(cwd).resolve() != work.resolve():
        return reply
    stdout = reply["stdout"].replace(json.dumps(cwd)[1:-1], "<fixture cwd>", 1)
    return {**reply, "stdout": stdout}


async def attach(attachments, home, process, attach_lock):
    from hermes_cli.gateway_client import connect_gateway

    endpoint = await discover(home, process)
    # The stock client uses HERMES_HOME at ticket minting. Serialize just that
    # handshake so both independent captures can reach their timeout together.
    async with attach_lock:
        previous_home = os.environ.get("HERMES_HOME")
        os.environ["HERMES_HOME"] = str(home)
        try:
            return await attachments.enter_async_context(connect_gateway(endpoint=endpoint))
        finally:
            if previous_home is None:
                os.environ.pop("HERMES_HOME", None)
            else:
                os.environ["HERMES_HOME"] = previous_home


async def remove_fixture_directory(directory):
    deadline = time.monotonic() + 4
    while True:
        try:
            directory.rmdir()
            return None
        except OSError as exc:
            # Windows may briefly keep the launch directory open while
            # session startup's workspace probe exits.
            if getattr(exc, "winerror", None) != 32 or time.monotonic() >= deadline:
                return {"fixture_error": "could not remove working directory", "errno": exc.errno,
                        "winerror": getattr(exc, "winerror", None)}
            await asyncio.sleep(0.1)


async def capture_timeout(client, sid, command, work):
    # Exercise the stock client's WebSocket keepalive while its shell RPC is pending.
    async with asyncio.TaskGroup() as group:
        pending = group.create_task(capture(client, sid, command))
        pong_during_capture = False
        try:
            async with asyncio.timeout(15):
                while not (work / "timeout-started").exists() and not pending.done():
                    await asyncio.sleep(0.05)
                if not pending.done():
                    pong = await client.websocket.ping()
                    await pong
                    pong_during_capture = not pending.done()
        except TimeoutError:
            pass
    return pending.result(), pong_during_capture


async def exercise(name, fixture, backend, skip_timeout, attach_lock):
    home, work, process = fixture
    async with AsyncExitStack() as attachments:
        client = await attach(attachments, home, process, attach_lock)
        created = await client.rpc("session.create", request_id=f"composer-{name}", source="tui",
                                   cwd=str(work), toolsets=[])
        sid = created["session_id"]

        if backend != "local":
            reply = await capture(client, sid, 'echo unexpected > backend-marker.txt')
            return {"unsupported_backend": reply,
                    "backend_marker": (work / "backend-marker.txt").exists()}

        before = snapshot_state(await client.rpc("session.resume", session_id=sid))
        reader = "type" if WINDOWS else "cat"
        shared = work.parent / "shared notes.txt"
        relative = str(Path("..") / shared.name)
        missing = str(Path("..") / "missing notes.txt")
        quote = (lambda arg: f'"{arg}"') if WINDOWS else shlex.quote
        result = {}
        for label, path in (("parent_relative", relative), ("parent_absolute", str(shared)),
                            ("parent_missing", missing)):
            result[label] = await capture(client, sid, f"{reader} {quote(path)}")
        for case, command in commands(skip_timeout).items():
            if case == "timeout":
                reply, result["websocket_pong_during_capture"] = await capture_timeout(client, sid, command, work)
            else:
                reply = await capture(client, sid, command)
            result[case] = normalize(case, reply, work)
            if case == "pipe_and_redirect":
                output = work / "captured file.txt"
                result["redirected_file"] = output.read_bytes().hex() if output.is_file() else None
        echo = "echo fixture"
        inputs = {
            "missing_command": {"session_id": sid},
            "null_command": {"session_id": sid, "command": None},
            "missing_session": {"command": echo},
            "null_session": {"session_id": None, "command": echo},
            "blank_session": {"session_id": "", "command": echo},
            "malformed_session": {"session_id": 17, "command": echo},
            "unknown_session": {"session_id": "missing-fixture-session", "command": echo},
            "null_profile": {"session_id": sid, "command": echo, "profile": None},
            "empty_profile": {"session_id": sid, "command": echo, "profile": ""},
            "malformed_profile": {"session_id": sid, "command": echo, "profile": 17},
            "mismatched_profile": {"session_id": sid, "command": echo, "profile": "not-the-fixture"},
            "unknown_key": {"session_id": sid, "command": echo, "unexpected": True},
        }
        for case, params in inputs.items():
            result[case] = await capture(client, **params)

        second = await attach(attachments, home, process, attach_lock)
        result["private_client_without_subscription"] = await capture(second, sid, echo)
        removed = work / "vanishing checkout"
        removed.mkdir()
        vanished = await client.rpc("session.create", request_id=f"composer-{name}-vanished", source="tui",
                                    cwd=str(removed), toolsets=[])
        removal_error = await remove_fixture_directory(removed)
        result["removed_cwd"] = removal_error or await capture(client, vanished["session_id"], echo)
        after = snapshot_state(await client.rpc("session.resume", session_id=sid))
        result["session_unchanged"] = before == after
        result["session_quiet"] = all(not after[key] for key in
                                      ("messages", "running", "pending", "prompts", "open_requests"))
        return result


def summary(value):
    if not isinstance(value, dict) or "stdout" not in value:
        return value
    result = dict(value)
    for stream in ("stdout", "stderr"):
        text = result[stream]
        if len(text) > 150:
            result[stream] = {
                "characters": len(text),
                "first_24_characters": text[:24],
                "sha256": hashlib.sha256(text.encode()).hexdigest(),
            }
    return result


def refusal(reason, **data):
    return {"error": {"code": 4001, "message": reason, "data": {"reason": reason, **data}}}


def reference_failures(stock, skip_timeout):
    success = {"stdout": "fixture\n", "stderr": "", "code": 0}
    expected = {
        "quoted_arguments": {"stdout": '["two words", "left|right", ""]\n', "stderr": "", "code": 0},
        "closed_stdin": {"stdout": "0\n", "stderr": "", "code": 0},
        "parent_relative": {"stdout": "sibling fixture text\n", "stderr": "", "code": 0},
        "parent_absolute": {"stdout": "sibling fixture text\n", "stderr": "", "code": 0},
        "nonzero_and_crlf": {"stdout": "out\nline\nfinal\n", "stderr": "err\nlast\n", "code": 23},
        "unicode_and_invalid_utf8": {"stdout": "café 雪 😀\x00��x", "stderr": "����", "code": 0},
        "control_characters": {
            "stdout": "left\x00\x01\x1b[31mright\x1b[0m\n",
            "stderr": "\x07\x08end",
            "code": 0,
        },
        "synthetic_redaction": {
            "stdout": "key=sk-or-...ffff\n",
            "stderr": "https://***:***@fixture.invalid/path\n",
            "code": 0,
        },
        "shell_environment": {"stdout": "ordinary fixture value\n", "stderr": "", "code": 0},
        "pipe_and_redirect": {"stdout": "", "stderr": "", "code": 0},
        "redirected_file": (b"fixture-pipe \r\n" if WINDOWS else b"fixture-pipe\n").hex(),
        "empty_command": refusal("invalid_params"),
        "nonstring_command": refusal("invalid_params", fields=["command"]),
        "missing_command": refusal("invalid_params"),
        "null_command": refusal("invalid_params", fields=["command"]),
        "missing_session": refusal("invalid_params"),
        "null_session": refusal("invalid_params", fields=["session_id"]),
        "blank_session": refusal("invalid_params"),
        "malformed_session": refusal("invalid_params", fields=["session_id"]),
        "unknown_session": refusal("not_found"),
        "null_profile": success,
        "empty_profile": success,
        "malformed_profile": refusal("invalid_params", fields=["profile"]),
        "mismatched_profile": refusal("profile_mismatch"),
        "unknown_key": refusal("invalid_params", fields=["unexpected"]),
        "private_client_without_subscription": success,
        "removed_cwd": refusal("cwd_unavailable"),
        "session_unchanged": True,
        "session_quiet": True,
        "unsupported_backend": refusal("unsupported_terminal_backend", field="terminal.backend", detail="docker"),
        "backend_marker": False,
    }
    if not skip_timeout:
        expected["timeout"] = refusal("command_timeout")
        expected["websocket_pong_during_capture"] = True
    problems = [name for name, want in expected.items() if stock.get(name) != want]
    missing = stock["parent_missing"]
    if not missing.get("code") or not missing.get("stderr"):
        problems.append("parent_missing")
    env = stock["frozen_cwd_and_env"]
    if env.get("code") != 0 or json.loads(env["stdout"]) != {
        "cwd": "<fixture cwd>", "fixture_env": "ordinary fixture value", "owner_key_inherited": False,
    }:
        problems.append("frozen_cwd_and_env")
    tails = stock["large_redacted_tails"]
    if (tails.get("code") != 0 or len(tails["stdout"]) != 4000 or len(tails["stderr"]) != 2000
            or any("f" * 10 in tails[stream] for stream in ("stdout", "stderr"))):
        problems.append("large_redacted_tails")
    return problems


async def exercise_owners(fixtures, backend, skip_timeout):
    lock = asyncio.Lock()
    # Let the other owner finish its real command and release its Windows cwd on failure.
    calls = (exercise(name, fixture, backend, skip_timeout, lock) for name, fixture in fixtures.items())
    receipts = dict(zip(fixtures, await asyncio.gather(*calls, return_exceptions=True), strict=True))
    failures = []
    for name, receipt in receipts.items():
        if isinstance(receipt, Exception):
            process = fixtures[name][2]
            receipt.add_note(f"{name}-{backend} gateway PID {process.pid}, exit code {process.poll()}")
            failures.append(receipt)
    if failures:
        raise ExceptionGroup("Composer comparison failed", failures)
    return receipts


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-checkout", type=Path, required=True)
    parser.add_argument("--gateway-exe", type=Path)
    parser.add_argument("--skip-timeout", action="store_true", help="omit the approximately 30s case")
    args = parser.parse_args()
    gateway_exe = args.gateway_exe.resolve() if args.gateway_exe else None
    if gateway_exe is not None and not gateway_exe.is_file():
        parser.error(f"Rust gateway executable not found: {gateway_exe}")
    with tempfile.TemporaryDirectory(prefix="composer-shell-") as temporary:
        root = Path(temporary).resolve()
        source = stock_source(args.source_checkout.resolve(), root)
        sys.path.insert(0, str(source))
        owners = {"stock": None}
        if gateway_exe is not None:
            owners["rust"] = gateway_exe
        results = {name: {} for name in owners}
        for backend in ("local", "docker"):
            with ExitStack() as stack:
                fixtures = {
                    name: stack.enter_context(owner(f"{name}-{backend}", source, executable, root, backend))
                    for name, executable in owners.items()
                }
                receipts = asyncio.run(exercise_owners(fixtures, backend, args.skip_timeout))
            for name, receipt in receipts.items():
                results[name].update(receipt)
        stock = results["stock"]
        rust = results.get("rust", {})
        differences = [key for key in stock.keys() | rust.keys()
                       if rust and stock.get(key) != rust.get(key)]
        fixture_problems = reference_failures(stock, args.skip_timeout)
        print(json.dumps({
            "os": sys.platform,
            "stock_commit": PINNED_STOCK_SHA,
            "timeout_exercised": not args.skip_timeout,
            "results": {name: {case: summary(value) for case, value in result.items()}
                        for name, result in results.items()},
            "differences": sorted(differences),
            "reference_fixture_failures": fixture_problems,
        }, ensure_ascii=True, indent=2), flush=True)
        return int(bool(differences or fixture_problems))


if __name__ == "__main__":
    raise SystemExit(main())
