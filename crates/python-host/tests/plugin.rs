//! The host runs a real plugin in embedded CPython and speaks MCP stdio to it.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

const PLUGIN: &str = r#"
import asyncio, json

print("plugins may print; this must not reach the protocol")

def _shout(args, **_):
    return json.dumps({"text": str(args.get("text", "")).upper() + "!"})

async def _later(args):
    await asyncio.sleep(0)
    return {"greeting": "hi " + args.get("name", "")}

def _broken(args):
    raise RuntimeError("boom")

def _guard(tool_name, args, **_):
    if tool_name == "terminal" and "rm -rf" in args.get("command", ""):
        return {"action": "block", "message": "not on my watch"}

def register(ctx):
    schema = lambda name: {"name": name, "description": name + " tool", "parameters": {"type": "object", "properties": {}}}
    ctx.register_tool("shout_text", "shout", schema("shout_text"), _shout)
    ctx.register_tool("greet", "shout", schema("greet"), _later, is_async=True)
    ctx.register_tool("broken", "shout", schema("broken"), _broken)
    ctx.register_tool("hidden", "shout", schema("hidden"), _shout, check_fn=lambda: False)
    ctx.register_tool("needs_key", "shout", schema("needs_key"), _shout, requires_env=["HERMES_TEST_UNSET_KEY"])
    ctx.register_hook("pre_tool_call", _guard)
    ctx.register_command("shout", lambda raw: raw)  # not supported in the desktop app: skipped
    ctx.register_tool("config_echo", "shout", schema("config_echo"), lambda args: ctx.get_config("volume", "quiet"))
"#;

struct Host { child: std::process::Child, out: BufReader<std::process::ChildStdout>, next: u64 }
impl Host {
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": self.next, "method": method, "params": params})).unwrap();
        let mut line = String::new();
        self.out.read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap_or_else(|_| panic!("not JSON-RPC: {line:?}"));
        assert_eq!(reply["id"], self.next);
        reply
    }
}

#[test]
fn a_plugin_registers_tools_and_hooks_through_the_rust_ctx() {
    let dir = tempfile::tempdir().unwrap();
    let plugin = dir.path().join("shout");
    std::fs::create_dir_all(&plugin).unwrap();
    std::fs::write(plugin.join("plugin.yaml"), "name: shout\n").unwrap();
    std::fs::write(plugin.join("__init__.py"), PLUGIN).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_hermes-python-host")).arg("serve").arg(&plugin)
        .env("PLUGIN_CONFIG_VOLUME", "loud").env_remove("HERMES_TEST_UNSET_KEY")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let out = BufReader::new(child.stdout.take().unwrap());
    let mut host = Host { child, out, next: 0 };

    let hello = host.request("initialize", json!({"protocolVersion": "2025-06-18"}));
    assert_eq!(hello["result"]["capabilities"]["experimental"]["hermes"]["hooks"], json!(["pre_tool_call"]));

    let tools = host.request("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["shout_text", "greet", "broken", "config_echo"], "check_fn and requires_env hide tools");

    let text = |reply: &Value| reply["result"]["content"][0]["text"].as_str().unwrap().to_owned();
    let shout = host.request("tools/call", json!({"name": "shout_text", "arguments": {"text": "hi"}}));
    assert_eq!(text(&shout), r#"{"text": "HI!"}"#);
    assert_eq!(text(&host.request("tools/call", json!({"name": "greet", "arguments": {"name": "ana"}}))), r#"{"greeting": "hi ana"}"#);
    assert_eq!(text(&host.request("tools/call", json!({"name": "config_echo", "arguments": {}}))), "loud");
    let broken = host.request("tools/call", json!({"name": "broken", "arguments": {}}));
    assert_eq!(broken["result"]["isError"], true);
    assert!(text(&broken).contains("boom"));
    assert!(host.request("tools/call", json!({"name": "hidden", "arguments": {}}))["error"].is_object());

    let blocked = host.request("hermes/hook", json!({"name": "pre_tool_call", "kwargs": {"tool_name": "terminal", "args": {"command": "rm -rf /"}, "tool_call_id": "c1"}}));
    assert_eq!(blocked["result"]["results"], json!([{"action": "block", "message": "not on my watch"}]));
    let allowed = host.request("hermes/hook", json!({"name": "pre_tool_call", "kwargs": {"tool_name": "terminal", "args": {"command": "ls"}}}));
    assert_eq!(allowed["result"]["results"], json!([]));

    drop(host.child.stdin.take());
    assert!(host.child.wait().unwrap().success());
}

#[test]
fn a_plugin_that_fails_to_load_exits_with_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let plugin = dir.path().join("needs-hermes");
    std::fs::create_dir_all(&plugin).unwrap();
    std::fs::write(plugin.join("__init__.py"), "from hermes_cli.config import load_config\ndef register(ctx): pass\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_hermes-python-host")).arg("serve").arg(&plugin).stdin(Stdio::null()).output().unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("hermes_cli"));
}
