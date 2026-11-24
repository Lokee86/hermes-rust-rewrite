"""Opt-in Windows interoperability smoke for the pinned stock Hermes GatewayClient.

Run from the rewrite repo with the stock checkout containing commit caafe538:
  <installed-Hermes-python.exe> scripts/verify_stock_gateway_client.py --source-checkout C:\\!bin\\workspace\\hermes-agent --gateway-exe <integrated-hermes-gateway.exe>

The script archives the pinned stock source into a temporary directory under ignored target/, starts the local Rust
gateway and an in-process deterministic OpenAI-compatible mock, then drives a manual approval,
disconnect/reconnect, retry, and history read through the stock Python client module.

The fixture starts the integrated Rust gateway owner with an isolated HERMES_HOME and
HERMES_RUST_DATA_DIR, then uses the stock runtime discovery, Windows control pipe, one-use ticket,
and connect_gateway path without monkeypatches.
"""
from __future__ import annotations

import argparse
import asyncio
import json
import os
from pathlib import Path
import queue
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from verify_stock_composer_shell import stock_source


def _sse(delta: dict, finish: str | None) -> bytes:
    body = {"choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
    return ("data: " + json.dumps(body, separators=(",", ":")) + "\n\n").encode()


class ProviderHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self) -> None:  # noqa: N802 - stdlib callback
        try:
            length = int(self.headers.get("Content-Length", "0"))
            request = json.loads(self.rfile.read(length))
            messages = request.get("messages", [])
            last = messages[-1] if messages else {}
            if last.get("role") == "tool":
                chunks = [_sse({"content": "Clock read successfully."}, "stop")]
            elif "clock" in str(last.get("content", "")).casefold():
                chunks = [
                    _sse({"content": "The clock needs approval."}, None),
                    _sse(
                        {
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "id": "stock_clock_call",
                                    "function": {
                                        "name": "get_current_time",
                                        "arguments": "{}",
                                    },
                                }
                            ]
                        },
                        "tool_calls",
                    ),
                ]
            else:
                chunks = [_sse({"content": "Unexpected fixture prompt."}, "stop")]
            payload = b"".join(chunks) + b"data: [DONE]\n\n"
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(payload)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(payload)
            self.wfile.flush()
        except Exception as exc:  # surface mock errors in the test log
            self.send_error(500, str(exc))

    def log_message(self, fmt: str, *args: object) -> None:
        return


def _import_pinned_gateway_client(checkout: Path, scratch: Path):
    source = stock_source(checkout, scratch)
    sys.path.insert(0, str(source))
    from hermes_cli import gateway_client
    return gateway_client


async def _next_event(client, event_type: str, timeout_seconds: float = 20):
    deadline = time.monotonic() + timeout_seconds
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError(f"timed out waiting for {event_type}")
        item = await asyncio.wait_for(client.events.get(), remaining)
        if isinstance(item, BaseException):
            raise item
        params = item.get("params", {}) if isinstance(item, dict) else {}
        if item.get("method") == "event" and params.get("type") == event_type:
            return params


async def _exercise(gateway_client, profile_home: Path, temp_root: Path) -> dict:
    os.environ["HERMES_HOME"] = str(temp_root / "hermes-home")
    os.environ["HERMES_RUST_DATA_DIR"] = str(profile_home)
    from hermes_cli.gateway_runtime import ensure_gateway_runtime

    discovery = await asyncio.to_thread(
        ensure_gateway_runtime,
        profile_home,
        timeout=10,
        idle_exit=True,
    )
    if discovery.state != "ready" or discovery.endpoint is None:
        raise AssertionError(f"stock owner discovery failed: {discovery}")

    create = {
        "request_id": "stock-create-once",
        "source": "gui",
        "title": "Stock Python GatewayClient",
        "cwd": str(temp_root),
        "model": "fixture-model",
    }
    submission = {
        "session_id": None,
        "submission_id": "stock-turn-once",
        "text": "Read the clock",
    }

    async with gateway_client.connect_gateway() as first:
        created = await first.rpc("session.create", **create)
        session_id = created["session_id"]
        submission["session_id"] = session_id
        receipt = await first.rpc("prompt.submit", **submission)
        if receipt["status"] != "started":
            raise AssertionError(f"stock prompt admission was {receipt}")
        approval = await _next_event(first, "approval.request")
        prompt_id = approval["payload"]["prompt_id"]
        generation = approval["execution_generation"]
        if set(approval["payload"]["choices"]) != {"once", "deny"}:
            raise AssertionError(f"unexpected approval choices: {approval['payload']}")
        if not (await first.rpc("gateway.ping"))["ok"]:
            raise AssertionError("gateway.ping failed while the tool awaited approval")

    # A second stock client resumes the disconnected session and resolves the same approval.
    async with gateway_client.connect_gateway() as second:
        snapshot = await second.rpc("session.resume", session_id=session_id)
        if snapshot.get("running") is not True:
            raise AssertionError(f"resumed session is not running: {snapshot}")
        if not any(prompt.get("prompt_id") == prompt_id for prompt in snapshot.get("prompts", [])):
            raise AssertionError(f"resumed approval is missing: {snapshot.get('prompts')}")
        repeated = await second.rpc("prompt.submit", **submission)
        if repeated["admission_id"] != receipt["admission_id"]:
            raise AssertionError("retry admitted a duplicate turn")
        resolved = await second.rpc(
            "approval.respond",
            session_id=session_id,
            prompt_id=prompt_id,
            execution_generation=generation,
            choice="once",
        )
        if resolved.get("status") != "resolved":
            raise AssertionError(f"approval was not resolved: {resolved}")
        complete = await _next_event(second, "message.complete")
        if complete["payload"].get("status") != "complete":
            raise AssertionError(f"turn did not complete: {complete['payload']}")
        if complete["payload"].get("text") != "Clock read successfully.":
            raise AssertionError(f"unexpected deterministic reply: {complete['payload']}")
        history = await second.rpc("session.history", session_id=session_id)
        if history.get("count") != 4:
            raise AssertionError(f"expected 4 history messages: {history}")
        if history["messages"][2].get("name") != "get_current_time":
            raise AssertionError(f"tool result missing from history: {history['messages']}")
        return {
            "session_id": session_id,
            "admission_id": receipt["admission_id"],
            "approval_prompt_id": prompt_id,
            "history_count": history["count"],
            "assistant_reply": complete["payload"]["text"],
        }


def _start_gateway(gateway_exe: Path, data_dir: Path, provider_url: str):
    env_names = ("PATH", "SYSTEMROOT", "WINDIR", "TEMP", "TMP", "LOCALAPPDATA", "APPDATA", "USERPROFILE")
    env = {name: os.environ[name] for name in env_names if name in os.environ}
    env.update(
        HERMES_RUST_BASE_URL=provider_url,
        HERMES_RUST_MODEL="fixture-model",
        HERMES_RUST_API_KEY="local-fixture-key",
        HERMES_HOME=str(data_dir),
        HERMES_RUST_DATA_DIR=str(data_dir),
    )
    process = subprocess.Popen(
        [
            str(gateway_exe),
            "gateway",
            "run",
            "--ephemeral-token",
            "--host",
            "127.0.0.1",
            "--port",
            "0",
            "--data-dir",
            str(data_dir),
        ],
        cwd=gateway_exe.parent.parent.parent,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        errors="replace",
        bufsize=1,
    )
    output: queue.Queue[str] = queue.Queue()

    def collect_stdout():
        assert process.stdout is not None
        for line in process.stdout:
            output.put(line)

    threading.Thread(target=collect_stdout, daemon=True).start()
    try:
        line = output.get(timeout=20)
        info = json.loads(line)
        return process, info
    except Exception:
        stderr = ""
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=5)
        if process.stderr is not None:
            stderr = process.stderr.read()
        raise RuntimeError(f"gateway failed to start; stdout={list(output.queue)!r}; stderr={stderr}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source-checkout", type=Path, required=True)
    parser.add_argument("--gateway-exe", type=Path, required=True)
    args = parser.parse_args()
    repo_root = Path(__file__).resolve().parents[1]
    gateway_exe = args.gateway_exe.resolve()
    if not gateway_exe.is_file():
        raise SystemExit(f"Rust gateway executable not found: {gateway_exe}")
    if not (args.source_checkout / ".git").exists():
        raise SystemExit(f"stock source checkout is not a Git worktree: {args.source_checkout}")

    provider = ThreadingHTTPServer(("127.0.0.1", 0), ProviderHandler)
    provider_thread = threading.Thread(target=provider.serve_forever, daemon=True)
    provider_thread.start()
    ignored_target = repo_root / "target"
    ignored_target.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="herdr-stock-gateway-", dir=ignored_target) as temp:
        temp_root = Path(temp)
        gateway_client = _import_pinned_gateway_client(args.source_checkout.resolve(), temp_root)
        profile_home = temp_root / "hermes-home"
        profile_home.mkdir()
        (profile_home / "agent-settings.json").write_text('{"approval_mode":"manual"}\n', encoding="utf-8")
        process = None
        try:
            provider_url = f"http://127.0.0.1:{provider.server_port}/v1"
            process, _info = _start_gateway(gateway_exe, profile_home, provider_url)
            result = asyncio.run(_exercise(gateway_client, profile_home, temp_root))
            print(json.dumps({"result": "pass", "transport": "pinned-hermes_cli.gateway_client", "stock_discovery": "ready", **result}, indent=2))
            return 0
        finally:
            if process is not None and process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
            provider.shutdown()
            provider.server_close()
            provider_thread.join(timeout=2)


if __name__ == "__main__":
    raise SystemExit(main())
