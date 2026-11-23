# Windows linked-role smoke for a staged Hermes.exe and its packaged ConPTY helpers.
# Install websockets==15.0.1 in the test interpreter, then pass the absolute Hermes.exe path.
import asyncio
import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import sys
import tempfile
from urllib.parse import urlencode
from urllib.request import Request, urlopen

from websockets import connect

META = "\0HERMES_TERMINAL_META:"
NEW_WINDOW_FLAGS = getattr(subprocess, "CREATE_NO_WINDOW", 0)

async def start_gateway(exe, profile, token):
    env = dict(os.environ)
    env.pop("HERMES_HERDR_CORE", None)
    env["HERMES_RUST_BASE_URL"] = "http://127.0.0.1:1/v1"
    env["HERMES_RUST_MODEL"] = "packaging-smoke"
    env["HERMES_RUST_API_KEY"] = ""
    env["HERMES_RUST_GATEWAY_TOKEN"] = token
    child = await asyncio.create_subprocess_exec(
        str(exe), "gateway", "run", "--port", "0", "--data-dir", str(profile),
        cwd=exe.parent, env=env, stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE, creationflags=NEW_WINDOW_FLAGS,
    )
    try:
        line = await asyncio.wait_for(child.stdout.readline(), 18)
        if not line:
            diagnostic = await asyncio.wait_for(child.stderr.read(4000), 2)
            raise RuntimeError(f"Gateway never published readiness: {diagnostic.decode(errors='replace')}")
        ready = json.loads(line)
        assert ready["implementation"] == "hermes-rust", ready
        return child, ready["http_url"]
    except BaseException:
        await stop_gateway(child)
        raise

async def stop_gateway(child):
    if child.returncode is None:
        child.terminate()
    await asyncio.wait_for(child.wait(), 10)

def ticket(base, token):
    request = Request(base + "/api/webapp/socket-ticket?socket=host-terminal",
                      method="POST", headers={"Authorization": "Bearer " + token})
    with urlopen(request, timeout=8) as response:
        return json.load(response)["ticket"]

async def terminal(base, token, params, origin=None):
    params = dict(params)
    params["ticket"] = await asyncio.to_thread(ticket, base, token)
    return await connect(base.replace("http:", "ws:", 1) + "/api/host-terminal?" + urlencode(params),
                         origin=origin, open_timeout=10, close_timeout=2)

async def meta(ws):
    frame = await asyncio.wait_for(ws.recv(), 45)
    assert isinstance(frame, str) and frame.startswith(META), repr(frame)[:500]
    return json.loads(frame[len(META):])

async def output(ws, marker):
    text = bytearray()
    async with asyncio.timeout(20):
        while len(text) < 1 << 20:
            frame = await ws.recv()
            if isinstance(frame, bytes):
                text.extend(frame)
            if marker in text:
                return bytes(text)
    raise AssertionError(f"Missing terminal evidence: {marker!r}")

def embedded_role_pids(exe):
    env = dict(os.environ, HERMES_PACKAGED_SMOKE_EXE=str(exe))
    query = """
$records = Get-CimInstance Win32_Process -Filter "Name='Hermes.exe'" |
    Where-Object { $_.ExecutablePath -eq $env:HERMES_PACKAGED_SMOKE_EXE } |
    Select-Object ProcessId,CommandLine
ConvertTo-Json -InputObject @($records) -Compress
"""
    response = subprocess.run(["powershell.exe", "-NoProfile", "-Command", query],
                              env=env, capture_output=True, check=True, timeout=8,
                              creationflags=NEW_WINDOW_FLAGS)
    return {role: sorted(item["ProcessId"] for item in json.loads(response.stdout)
                         if re.search(r'\b' + role + r'\b', item["CommandLine"]))
            for role in ("serve", "hold")}

async def wait_for_exit(pid):
    async with asyncio.timeout(10):
        while True:
            probe = await asyncio.to_thread(
                subprocess.run,
                ["tasklist.exe", "/FI", f"PID eq {pid}", "/FO", "CSV", "/NH"],
                capture_output=True, check=True, timeout=5, creationflags=NEW_WINDOW_FLAGS,
            )
            if f'"{pid}"'.encode() not in probe.stdout:
                return
            await asyncio.sleep(0.1)

async def verify(exe):
    lic = subprocess.run([str(exe), "--licenses"], cwd=exe.parent, timeout=20,
                         capture_output=True, creationflags=NEW_WINDOW_FLAGS)
    assert lic.returncode == 0 and len(lic.stdout) > 100, (lic.returncode, lic.stderr[:400])
    print(f"Embedded --licenses exit=0, output bytes={len(lic.stdout)}", flush=True)
    profile = Path(tempfile.mkdtemp(prefix="hermes-linked-profile-", dir=exe.parent.parent))
    script = profile / "child.py"
    script.write_text(
        "import os\n"
        "print(f'CHILD={os.getpid()} SHELL={os.getppid()} READY', flush=True)\n"
        "line = input()\n"
        "print(f'RESULT={os.getpid()}:{line}', flush=True)\n",
        encoding="utf-8",
    )
    token = secrets.token_urlsafe(32)
    owner, base = await start_gateway(exe, profile, token)
    old_gateway_pid = owner.pid
    launch_key = secrets.token_hex(16)
    terminal_id = None
    closed_cleanly = False
    try:
        initial = await terminal(base, token, {"cwd": str(profile), "launch_key": launch_key}, base)
        try:
            terminal_id = (await meta(initial))["terminalId"]
            await initial.send(b"cd\r")
            await output(initial, os.fsencode(profile))
            await initial.send("\x1b[RESIZE:93;31]")
            await initial.send(b"set HERMES_PACKAGE_SMOKE=retained\r")
            await initial.send(f'"{sys.executable}" -u child.py\r'.encode())
            evidence = await output(initial, b" READY")
            fingerprint = re.search(rb"CHILD=(\d+) SHELL=(\d+) READY", evidence)
            assert fingerprint, repr(evidence[-800:])
            child, shell = map(int, fingerprint.groups())
            roles = await asyncio.to_thread(embedded_role_pids, exe)
            assert roles["serve"] and roles["hold"], roles
        finally:
            await initial.close()
        await stop_gateway(owner)
        owner, base = await start_gateway(exe, profile, token)
        assert owner.pid != old_gateway_pid
        resumed = await terminal(base, token, {"attach": terminal_id}, base)
        try:
            restored = await meta(resumed)
            assert restored["terminalId"] == terminal_id and restored["reconnected"], restored
            await output(resumed, f"CHILD={child} SHELL={shell} READY".encode())
            await resumed.send(b"continue-package-smoke\r")
            await output(resumed, f"RESULT={child}:continue-package-smoke".encode())
            await resumed.send(b"echo STATE=%HERMES_PACKAGE_SMOKE%\r")
            await output(resumed, b"STATE=retained")
            assert await asyncio.to_thread(embedded_role_pids, exe) == roles
            close = await terminal(base, token, {"attach": terminal_id, "action": "close"}, base)
            try:
                closed = await meta(close)
                assert closed["terminalId"] == terminal_id and closed["closed"], closed
                closed_cleanly = True
            finally:
                await close.close()
            await wait_for_exit(shell)
        finally:
            await resumed.close()
        print(f"Same staged Hermes.exe ran gateway {old_gateway_pid}->{owner.pid}, Core {roles['serve']}, "
              f"holder {roles['hold']}, shell {shell}, and foreground child {child}; reattach and explicit close passed.",
              flush=True)
    finally:
        if not closed_cleanly and owner.returncode is None:
            try:
                address = {"attach": terminal_id} if terminal_id else {"launch_key": launch_key}
                cleanup = await terminal(base, token, {**address, "action": "close"}, base)
                try:
                    await asyncio.wait_for(meta(cleanup), 8)
                finally:
                    await cleanup.close()
            except Exception as error:
                print(f"Could not close the isolated test terminal: {error}", file=sys.stderr)
        await stop_gateway(owner)

asyncio.run(verify(Path(sys.argv[1]).resolve(strict=True)))
