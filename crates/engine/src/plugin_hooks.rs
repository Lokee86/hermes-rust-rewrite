//! Hermes plugin lifecycle hooks around each tool call, for servers that declare them
//! (Python plugins in hermes-python-host). Same contracts as the Python agent:
//! - `pre_tool_call(tool_name, args, tool_call_id)`: `{"action": "block", "message"}` (or the
//!   `{"decision": "block", "reason"}` shape) stops the call; anything else lets it run. A hook
//!   server that fails blocks the call (the Python agent fails this policy hook closed).
//! - `transform_tool_result(tool_name, args, result, tool_call_id)`: the first string returned
//!   replaces the result.
//! - `post_tool_call(tool_name, args, result, tool_call_id)`: observer; returns are ignored.
use crate::history::ToolCall;
use crate::mcp::Mcp;
use serde_json::{Value, json};

fn kwargs(call: &ToolCall, result: Option<&str>) -> Value {
    let args: Value = serde_json::from_str(&call.arguments).unwrap_or(json!({}));
    let mut kwargs = json!({"tool_name": call.name, "args": args, "tool_call_id": call.id});
    if let Some(result) = result { kwargs["result"] = json!(result); }
    kwargs
}

/// The block message, if a plugin blocks this call.
pub(crate) async fn pre_tool_call(servers: &mut [Mcp], call: &ToolCall) -> Option<String> {
    for server in servers.iter_mut().filter(|s| s.has_hook("pre_tool_call")) {
        let results = match server.hook("pre_tool_call", kwargs(call, None)).await {
            Ok(results) => results,
            Err(e) => return Some(format!("A plugin's pre_tool_call hook failed, so the call was blocked: {e:#}")),
        };
        for result in results {
            let block = result["action"] == "block" || result["decision"] == "block";
            if block {
                let message = result["message"].as_str().or(result["reason"].as_str()).unwrap_or("Blocked by a plugin");
                return Some(message.to_owned());
            }
        }
    }
    None
}

/// The result after `transform_tool_result`, with `post_tool_call` observers told.
pub(crate) async fn after_tool_call(servers: &mut [Mcp], call: &ToolCall, mut result: String) -> String {
    for server in servers.iter_mut().filter(|s| s.has_hook("transform_tool_result")) {
        if let Ok(results) = server.hook("transform_tool_result", kwargs(call, Some(&result))).await
            && let Some(replacement) = results.iter().find_map(Value::as_str) {
            result = replacement.to_owned();
        }
    }
    for server in servers.iter_mut().filter(|s| s.has_hook("post_tool_call")) {
        if let Err(e) = server.hook("post_tool_call", kwargs(call, Some(&result))).await { eprintln!("post_tool_call hook failed: {e:#}"); }
    }
    result
}
