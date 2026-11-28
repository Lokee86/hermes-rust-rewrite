use crate::{Entry, History, Status, ToolCall};
use anyhow::{Result, anyhow, bail, ensure};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::{Client, RequestBuilder, Response, Url, header::HeaderValue};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    time::Duration,
};

pub const OPENROUTER: &str = "https://openrouter.ai/api/v1";
pub const OPENAI: &str = "https://api.openai.com/v1";
pub const ANTHROPIC: &str = "https://api.anthropic.com/v1";
pub const DEFAULT_MODEL: &str = "openrouter/free";

const MAX_REPLY_BYTES: usize = 1_048_576;
const MAX_TOOL_CALLS: usize = 8;
const MAX_ARGUMENT_BYTES: usize = 65_536;
const MAX_STREAM_BYTES: usize = 16 * 1_048_576;

mod inference;

#[derive(Clone)]
pub struct Config {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Protocol {
    OpenAi,
    Anthropic,
    /// OpenAI Responses API (ChatGPT / Codex subscription at chatgpt.com/backend-api/codex).
    Responses,
}

impl Config {
    pub fn from_env() -> Self {
        let base_url = std::env::var("HERMES_RUST_BASE_URL")
            .unwrap_or_else(|_| crate::portal::INFERENCE.into());
        // A custom endpoint must explicitly opt into its own credential. In
        // particular, never forward an official service's key to a local proxy.
        let key = std::env::var("HERMES_RUST_API_KEY")
            .ok()
            .or_else(|| official_key_env(&base_url).and_then(|name| std::env::var(name).ok()));
        let default_model = if base_url.trim_end_matches('/') == OPENROUTER {
            DEFAULT_MODEL
        } else {
            crate::portal::AUTO_MODEL
        };
        Self {
            base_url,
            model: std::env::var("HERMES_RUST_MODEL").unwrap_or_else(|_| default_model.into()),
            api_key: key.unwrap_or_default(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        let url = Url::parse(self.base_url.trim()).map_err(|_| anyhow!("Invalid provider URL"))?;
        ensure!(
            url.scheme() == "https" || (is_loopback(&url) && url.scheme() == "http"),
            "Use HTTPS or an HTTP loopback address"
        );
        ensure!(url.host_str().is_some(), "Enter a provider URL with a host");
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "Use a provider base URL without credentials, query, or fragment"
        );
        ensure!(!self.model.trim().is_empty(), "Enter a model ID");
        ensure!(
            HeaderValue::from_str(self.api_key.trim()).is_ok(),
            "The provider API key contains invalid characters"
        );
        if matches!(
            url.host_str(),
            Some("openrouter.ai" | "api.openai.com" | "api.anthropic.com")
        ) {
            ensure!(
                !self.api_key.trim().is_empty(),
                "Enter your provider API key in Provider settings"
            );
        }
        Ok(())
    }

    /// Validate and resolve an OpenAI-compatible provider route.
    pub fn openai_chat_url(&self) -> Result<Url> {
        self.validate()?;
        let (protocol, url) = self.endpoint()?;
        ensure!(protocol == Protocol::OpenAi, "profile requires an OpenAI-compatible inference endpoint");
        Ok(url)
    }

    fn endpoint(&self) -> Result<(Protocol, Url)> {
        let mut url =
            Url::parse(self.base_url.trim()).map_err(|_| anyhow!("Invalid provider URL"))?;
        let path = url.path().trim_end_matches('/').to_owned();
        if url.host_str() == Some("chatgpt.com") && path.starts_with("/backend-api/codex") {
            let path = if path.ends_with("/responses") { path } else { format!("{path}/responses") };
            url.set_path(&path);
            return Ok((Protocol::Responses, url));
        }
        // Anthropic-compatible gateways (MiniMax OAuth: https://api.minimax.io/anthropic).
        if path.ends_with("/anthropic") {
            url.set_path(&format!("{path}/v1/messages"));
            return Ok((Protocol::Anthropic, url));
        }
        let path = path.as_str();
        let protocol = if url.host_str() == Some("api.anthropic.com") || path.ends_with("/messages")
        {
            Protocol::Anthropic
        } else {
            Protocol::OpenAi
        };
        let suffix = match protocol {
            Protocol::OpenAi | Protocol::Responses => "/chat/completions",
            Protocol::Anthropic => "/messages",
        };
        let path = if path.ends_with(suffix) {
            path.to_owned()
        } else {
            let base = if path.is_empty() { "/v1" } else { path };
            format!("{base}{suffix}")
        };
        url.set_path(&path);
        Ok((protocol, url))
    }
}

fn is_loopback(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn official_key_env(base_url: &str) -> Option<&'static str> {
    let url = Url::parse(base_url.trim()).ok()?;
    if url.scheme() != "https"
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    match (url.host_str()?, url.path().trim_end_matches('/')) {
        ("openrouter.ai", "/api/v1" | "/api/v1/chat/completions") => Some("OPENROUTER_API_KEY"),
        ("api.openai.com", "" | "/v1" | "/v1/chat/completions") => Some("OPENAI_API_KEY"),
        ("api.anthropic.com", "" | "/v1" | "/v1/messages") => Some("ANTHROPIC_API_KEY"),
        _ => None,
    }
}

pub(crate) fn messages(history: &History, directory: &std::path::Path) -> Result<Vec<Value>> {
    let mut messages = vec![
        json!({"role":"system","content":"You are Hermes, a helpful desktop agent. Use available tools when needed to answer accurately or complete the user's request. Report actual tool results. Respect a tool denial and do not request the denied action again in the same turn. Do not claim to have read or changed files without a successful tool result."}),
    ];
    let mut skipped = 0;
    if let Some(summary) = history.active_summary() {
        skipped = summary.through;
        messages.push(json!({"role":"user","content":format!("[Summary of the earlier part of this conversation, written to fit the model's context. It is background, not a new request.]\n{}", summary.text)}));
        messages.push(json!({"role":"assistant","content":"Understood. I will continue from there."}));
    }
    for turn in &history.turns[skipped..] {
        // A running turn maps one entry to one message; callers rely on that.
        let stopped = match turn.status {
            Status::Complete | Status::Running => None,
            Status::Interrupted => Some("[The user stopped this reply before it finished.]"),
            Status::Failed(_) => Some("[This reply failed before it finished.]"),
        };
        let mut unanswered: Vec<&str> = Vec::new();
        let prompt = if turn.context.is_empty() { std::borrow::Cow::Borrowed(turn.prompt.as_str()) } else { std::borrow::Cow::Owned(format!("{}{}", turn.prompt, turn.context)) };
        messages.push(json!({"role":"user","content":crate::Attachment::user_content(directory,&prompt,&turn.attachments)?}));
        for entry in &turn.entries {
            match entry {
                Entry::User { text } => {
                    answer_unfinished(&mut messages, &mut unanswered);
                    messages.push(json!({"role":"user","content":text}));
                }
                Entry::Assistant { text, calls } => {
                    if stopped.is_some() {
                        answer_unfinished(&mut messages, &mut unanswered);
                        // A reply cut off while streaming can hold arguments that are not JSON yet.
                        let valid = calls.iter().all(|call| serde_json::from_str::<serde_json::Map<String, Value>>(&call.arguments).is_ok());
                        if !valid || (text.trim().is_empty() && calls.is_empty()) {
                            if !text.trim().is_empty() { messages.push(json!({"role":"assistant","content":text})); }
                            continue;
                        }
                        unanswered.extend(calls.iter().map(|call| call.id.as_str()));
                    }
                    let mut value = json!({"role":"assistant","content":text});
                    if !calls.is_empty() {
                        value["tool_calls"] = calls.iter().map(|call| json!({"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments}})).collect();
                    }
                    messages.push(value);
                }
                Entry::Tool { id, result, .. } => {
                    if stopped.is_some() {
                        let Some(position) = unanswered.iter().position(|call| call == id) else { continue };
                        unanswered.remove(position);
                    }
                    messages.push(json!({"role":"tool","tool_call_id":id,"content":result}))
                }
            }
        }
        if let Some(note) = stopped {
            answer_unfinished(&mut messages, &mut unanswered);
            match messages.last_mut() {
                Some(last) if last["role"] == "assistant" && last.get("tool_calls").is_none() => {
                    let text = last["content"].as_str().unwrap_or_default();
                    last["content"] = Value::String(format!("{text}\n\n{note}"));
                }
                _ => messages.push(json!({"role":"assistant","content":note})),
            }
        }
    }
    Ok(messages)
}

/// Every tool call sent to a provider needs a result, including calls that never ran.
fn answer_unfinished(messages: &mut Vec<Value>, unanswered: &mut Vec<&str>) {
    for id in unanswered.drain(..) {
        messages.push(json!({"role":"tool","tool_call_id":id,"content":"{\"error\":\"The reply stopped before this tool ran\"}"}));
    }
}

/// Anthropic reuses a prompt prefix it was asked to cache: the tools and system prompt, then the
/// conversation up to each of the last two user messages (the previous round's end and this one's).
fn cache_anthropic_prefix(body: &mut Value) {
    let mark = json!({"type":"ephemeral"});
    if let Some(text) = body["system"].as_str().filter(|text| !text.is_empty()) {
        body["system"] = json!([{"type":"text","text":text,"cache_control":mark}]);
    }
    if let Some(tool) = body["tools"].as_array_mut().and_then(|tools| tools.last_mut()) {
        tool["cache_control"] = mark.clone();
    }
    let users = body["messages"].as_array_mut().into_iter().flatten().rev().filter(|message| message["role"] == "user");
    for message in users.take(2) {
        if let Some(block) = message["content"].as_array_mut().and_then(|content| content.last_mut()) {
            block["cache_control"] = mark.clone();
        }
    }
}

/// The same models behind OpenRouter take the marks on OpenAI-style text parts.
fn cache_openrouter_prefix(endpoint: &Url, body: &mut Value) {
    let marked = body["model"].as_str().is_some_and(|model| model.contains("claude"));
    if !marked || endpoint.host_str() != Some("openrouter.ai") {
        return;
    }
    let messages = body["messages"].as_array_mut().map(Vec::as_mut_slice).unwrap_or_default();
    let last_user = messages.iter().rposition(|message| message["role"] == "user");
    for (index, message) in messages.iter_mut().enumerate() {
        if (index == 0 || Some(index) == last_user) && let Some(text) = message["content"].as_str().filter(|text| !text.is_empty()) {
            message["content"] = json!([{"type":"text","text":text,"cache_control":{"type":"ephemeral"}}]);
        }
    }
}

fn anthropic_body(model: &str, messages: Vec<Value>, tools: &[Value]) -> Result<Value> {
    let mut system = Vec::new();
    let mut native: Vec<Value> = Vec::new();
    for message in messages {
        let role = message["role"]
            .as_str()
            .ok_or_else(|| anyhow!("Invalid conversation role"))?;
        let text = message["content"].as_str().unwrap_or("");
        let (role, mut content) = match role {
            "system" | "developer" => {
                if !text.is_empty() {
                    system.push(text.to_owned());
                }
                continue;
            }
            "user" => ("user", crate::attachment::anthropic_user_content(&message["content"])?),
            "assistant" => {
                let mut content = Vec::new();
                if !text.is_empty() {
                    content.push(json!({"type":"text","text":text}));
                }
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    let arguments = call["function"]["arguments"].as_str().unwrap_or("{}");
                    let input: Value = serde_json::from_str(arguments)
                        .map_err(|_| anyhow!("Saved tool arguments are invalid JSON"))?;
                    ensure!(input.is_object(), "Saved tool arguments must be an object");
                    content.push(json!({"type":"tool_use","id":call["id"],"name":call["function"]["name"],"input":input}));
                }
                if content.is_empty() {
                    continue;
                }
                ("assistant", content)
            }
            "tool" => {
                let is_error = serde_json::from_str::<Value>(text)
                    .ok()
                    .is_some_and(|result| result.get("error").is_some());
                (
                    "user",
                    vec![
                        json!({"type":"tool_result","tool_use_id":message["tool_call_id"],"content":text,"is_error":is_error}),
                    ],
                )
            }
            _ => bail!("Unsupported conversation role"),
        };
        if let Some(previous) = native
            .last_mut()
            .filter(|previous| previous["role"] == role)
        {
            // Parallel results belong in one user message, before following text.
            previous["content"]
                .as_array_mut()
                .expect("native content array")
                .append(&mut content);
        } else {
            native.push(json!({"role":role,"content":content}));
        }
    }
    let mut native_tools = Vec::with_capacity(tools.len());
    for tool in tools {
        let function = &tool["function"];
        ensure!(
            tool["type"] == "function"
                && function["name"].is_string()
                && function["parameters"].is_object(),
            "Invalid registered tool schema"
        );
        native_tools.push(json!({"name":function["name"],"description":function["description"],"input_schema":function["parameters"]}));
    }
    let mut body = json!({"model":model,"system":system.join("\n\n"),"messages":native,"max_tokens":8192,"stream":true});
    if !native_tools.is_empty() {
        body["tools"] = json!(native_tools);
    }
    Ok(body)
}

#[derive(Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
    error: Option<Value>,
}
#[derive(Deserialize)]
struct Choice {
    index: usize,
    #[serde(default)]
    delta: Delta,
    finish_reason: Option<String>,
}
#[derive(Default, Deserialize)]
struct Delta {
    content: Option<String>,
    refusal: Option<String>,
    tool_calls: Option<Vec<CallDelta>>,
}
#[derive(Deserialize)]
struct CallDelta {
    index: usize,
    id: Option<String>,
    function: Option<FunctionDelta>,
}
#[derive(Deserialize)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Default)]
struct OpenAiStream {
    calls: BTreeMap<usize, ToolCall>,
    finish: Option<String>,
    content_bytes: usize,
}

impl OpenAiStream {
    fn accept(&mut self, data: &str, emit: &mut impl FnMut(&str) -> Result<()>) -> Result<bool> {
        if data.trim() == "[DONE]" {
            return Ok(true);
        }
        let chunk: Chunk =
            serde_json::from_str(data).map_err(|_| anyhow!("Invalid provider stream event"))?;
        if let Some(error) = chunk.error {
            bail!("{}", provider_error(&error));
        }
        for choice in chunk.choices.into_iter().filter(|choice| choice.index == 0) {
            if let Some(finish) = self.finish.as_ref() {
                // Routers can send a usage/metadata frame with choice 0 after
                // the final delta, before [DONE]. It must not change the reply.
                let empty = choice.delta.content.as_deref().unwrap_or_default().is_empty()
                    && choice.delta.refusal.as_deref().unwrap_or_default().is_empty()
                    && choice.delta.tool_calls.as_ref().is_none_or(Vec::is_empty);
                ensure!(empty && choice.finish_reason.as_ref().is_none_or(|next| next == finish),
                    "Provider sent conflicting response data after its finish reason");
                continue;
            }
            for text in [choice.delta.content, choice.delta.refusal]
                .into_iter()
                .flatten()
            {
                emit_text(&mut self.content_bytes, &text, emit)?;
            }
            for part in choice.delta.tool_calls.unwrap_or_default() {
                ensure!(
                    part.index < MAX_TOOL_CALLS,
                    "Too many tool calls in one response"
                );
                let call = self.calls.entry(part.index).or_insert_with(empty_call);
                if let Some(id) = part.id {
                    call.id.push_str(&id);
                }
                if let Some(function) = part.function {
                    if let Some(name) = function.name {
                        call.name.push_str(&name);
                    }
                    if let Some(arguments) = function.arguments {
                        call.arguments.push_str(&arguments);
                    }
                }
                check_call_size(call)?;
            }
            if choice.finish_reason.is_some() {
                self.finish = choice.finish_reason;
            }
        }
        Ok(false)
    }

    fn finish(self) -> Result<Vec<ToolCall>> {
        match self.finish.as_deref() {
            Some("stop") if self.calls.is_empty() && self.content_bytes > 0 => Ok(Vec::new()),
            Some("tool_calls") if !self.calls.is_empty() => {
                valid_calls(self.calls.into_values().collect())
            }
            Some("length") => {
                bail!("Provider reached its output token limit; the response is incomplete")
            }
            Some("content_filter") => {
                bail!("Provider stopped the response because of its content filter")
            }
            Some(reason) => bail!(
                "Provider ended with {} before returning a usable answer",
                safe_label(reason)
            ),
            None => bail!("Provider disconnected before completing its response"),
        }
    }
}

enum Block {
    Text,
    Tool {
        call: ToolCall,
        initial_input: Value,
        streamed_input: bool,
    },
    Other,
}

#[derive(Default)]
struct AnthropicStream {
    blocks: BTreeMap<usize, (Block, bool)>,
    started: bool,
    stopped: bool,
    finish: Option<String>,
    content_bytes: usize,
    tool_count: usize,
}

impl AnthropicStream {
    fn accept(&mut self, data: &str, emit: &mut impl FnMut(&str) -> Result<()>) -> Result<bool> {
        let value: Value =
            serde_json::from_str(data).map_err(|_| anyhow!("Invalid Anthropic stream event"))?;
        let kind = value["type"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing Anthropic event type"))?;
        match kind {
            "error" => bail!("{}", provider_error(&value["error"])),
            "ping" => {}
            "message_start" => {
                ensure!(!self.started, "Anthropic started the same message twice");
                ensure!(
                    value["message"]["role"] == "assistant",
                    "Invalid Anthropic message role"
                );
                self.started = true;
            }
            "content_block_start" => {
                ensure!(
                    self.started && self.finish.is_none(),
                    "Unexpected Anthropic content block"
                );
                let index = block_index(&value)?;
                ensure!(
                    self.blocks.len() < 256 && !self.blocks.contains_key(&index),
                    "Invalid Anthropic content block index"
                );
                let content = &value["content_block"];
                let block = match content["type"].as_str() {
                    Some("text") => {
                        emit_text(
                            &mut self.content_bytes,
                            content["text"].as_str().unwrap_or(""),
                            emit,
                        )?;
                        Block::Text
                    }
                    Some("tool_use") => {
                        self.tool_count += 1;
                        ensure!(
                            self.tool_count <= MAX_TOOL_CALLS,
                            "Too many tool calls in one response"
                        );
                        let call = ToolCall {
                            id: content["id"].as_str().unwrap_or("").to_owned(),
                            name: content["name"].as_str().unwrap_or("").to_owned(),
                            arguments: String::new(),
                        };
                        check_call_size(&call)?;
                        let initial_input =
                            content.get("input").cloned().unwrap_or_else(|| json!({}));
                        ensure!(
                            initial_input.is_object(),
                            "Anthropic tool input must be an object"
                        );
                        Block::Tool {
                            call,
                            initial_input,
                            streamed_input: false,
                        }
                    }
                    _ => Block::Other,
                };
                self.blocks.insert(index, (block, false));
            }
            "content_block_delta" => {
                let index = block_index(&value)?;
                let (block, closed) = self
                    .blocks
                    .get_mut(&index)
                    .ok_or_else(|| anyhow!("Anthropic sent a delta without a content block"))?;
                ensure!(
                    !*closed,
                    "Anthropic sent a delta after closing a content block"
                );
                let delta = &value["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        ensure!(
                            matches!(block, Block::Text),
                            "Anthropic text delta does not match its content block"
                        );
                        let text = delta["text"]
                            .as_str()
                            .ok_or_else(|| anyhow!("Invalid Anthropic text delta"))?;
                        emit_text(&mut self.content_bytes, text, emit)?;
                    }
                    Some("input_json_delta") => {
                        let Block::Tool {
                            call,
                            streamed_input,
                            ..
                        } = block
                        else {
                            bail!("Anthropic input delta does not match its tool block");
                        };
                        let partial = delta["partial_json"]
                            .as_str()
                            .ok_or_else(|| anyhow!("Invalid Anthropic tool input delta"))?;
                        *streamed_input = true;
                        call.arguments.push_str(partial);
                        check_call_size(call)?;
                    }
                    _ => {} // Thinking/signature and future deltas are not user-facing text.
                }
            }
            "content_block_stop" => {
                let index = block_index(&value)?;
                let (_, closed) = self
                    .blocks
                    .get_mut(&index)
                    .ok_or_else(|| anyhow!("Anthropic closed an unknown content block"))?;
                ensure!(!*closed, "Anthropic closed the same content block twice");
                *closed = true;
            }
            "message_delta" => {
                ensure!(
                    self.started,
                    "Anthropic sent a message delta without a message"
                );
                if let Some(reason) = value["delta"]["stop_reason"].as_str() {
                    ensure!(
                        self.finish.is_none(),
                        "Anthropic sent multiple stop reasons"
                    );
                    self.finish = Some(reason.to_owned());
                }
            }
            "message_stop" => {
                ensure!(
                    self.started,
                    "Anthropic stopped a message that was not started"
                );
                self.stopped = true;
                return Ok(true);
            }
            _ => {} // Anthropic's versioning contract permits new SSE event types.
        }
        Ok(false)
    }

    fn finish(self) -> Result<Vec<ToolCall>> {
        ensure!(
            self.stopped,
            "Anthropic disconnected before completing its response"
        );
        ensure!(
            self.blocks.values().all(|(_, closed)| *closed),
            "Anthropic disconnected before closing its content blocks"
        );
        match self.finish.as_deref() {
            Some("end_turn" | "stop_sequence" | "refusal")
                if self.tool_count == 0 && self.content_bytes > 0 =>
            {
                Ok(Vec::new())
            }
            Some("tool_use") if self.tool_count > 0 => {
                let mut calls = Vec::with_capacity(self.tool_count);
                for (block, _) in self.blocks.into_values() {
                    if let Block::Tool {
                        mut call,
                        initial_input,
                        streamed_input,
                    } = block
                    {
                        if !streamed_input {
                            call.arguments = initial_input.to_string();
                        }
                        calls.push(call);
                    }
                }
                valid_calls(calls)
            }
            Some("max_tokens") => {
                bail!("Anthropic reached its output token limit; the response is incomplete")
            }
            Some("refusal") => bail!("Anthropic declined this request"),
            Some("model_context_window_exceeded") => {
                bail!("Anthropic reached the model context window limit")
            }
            Some(reason) => bail!(
                "Anthropic ended with {} before returning a usable answer",
                safe_label(reason)
            ),
            None => bail!("Anthropic returned no stop reason"),
        }
    }
}

fn empty_call() -> ToolCall {
    ToolCall {
        id: String::new(),
        name: String::new(),
        arguments: String::new(),
    }
}

fn check_call_size(call: &ToolCall) -> Result<()> {
    ensure!(
        call.id.len() <= 1024
            && call.name.len() <= 256
            && call.arguments.len() <= MAX_ARGUMENT_BYTES,
        "Tool request exceeded the maximum size"
    );
    Ok(())
}

fn valid_calls(mut calls: Vec<ToolCall>) -> Result<Vec<ToolCall>> {
    let mut ids = HashSet::new();
    for call in &mut calls {
        check_call_size(call)?;
        ensure!(
            !call.id.is_empty() && !call.name.is_empty() && ids.insert(call.id.clone()),
            "Provider returned invalid tool-call IDs or names"
        );
        // Several local OpenAI-compatible servers omit an empty argument body.
        if call.arguments.trim().is_empty() {
            call.arguments = "{}".into();
        }
        let arguments: Value = serde_json::from_str(&call.arguments)
            .map_err(|_| anyhow!("Provider returned invalid JSON tool arguments"))?;
        ensure!(
            arguments.is_object(),
            "Provider tool arguments must be a JSON object"
        );
    }
    Ok(calls)
}

fn block_index(value: &Value) -> Result<usize> {
    value["index"]
        .as_u64()
        .filter(|index| *index < 256)
        .map(|index| index as usize)
        .ok_or_else(|| anyhow!("Invalid Anthropic content block index"))
}

fn emit_text(
    bytes: &mut usize,
    text: &str,
    emit: &mut impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    *bytes += text.len();
    ensure!(*bytes <= MAX_REPLY_BYTES, "Reply exceeded the maximum size");
    if !text.is_empty() {
        emit(text)?;
    }
    Ok(())
}

fn provider_error(value: &Value) -> String {
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| value.as_str())
        .unwrap_or("Unknown provider error");
    format!("Provider stream error — {message}")
}

fn safe_label(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
        .take(64)
        .collect()
}

/// Fetches a current token for a managed sign-in, refreshing it when near expiry.
pub(crate) type Renew = std::sync::Arc<dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send>> + Send + Sync>;

pub(crate) struct Provider {
    client: Client,
    config: Config,
    protocol: Protocol,
    endpoint: Url,
    options: crate::models::GenerationOptions,
    /// A managed sign-in is renewed before every model request: a turn can outlast its token
    /// while tools run or an approval waits.
    renew: Option<Renew>,
    key: std::sync::Mutex<Option<String>>,
}

impl Provider {
    #[cfg(test)]
    pub fn new(config: Config) -> Result<Self> {
        Self::new_with_options(config,crate::models::GenerationOptions::default())
    }

    pub(crate) async fn resolve_config(config: Config) -> Result<Config> {
        let config = crate::portal::resolve(config).await?;
        crate::accounts::resolve(config).await
    }

    pub fn new_with_options(config:Config,options:crate::models::GenerationOptions)->Result<Self> {
        options.validate()?;
        config.validate()?;
        let (protocol, endpoint) = config.endpoint()?;
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            // Silence, not length: a long reply may stream for many minutes.
            .read_timeout(Duration::from_secs(180))
            .user_agent(concat!("HermesRust/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| anyhow!("Could not initialize the model provider connection"))?;
        Ok(Self {
            client,
            config,
            protocol,
            endpoint,
            options,
            renew: None,
            key: std::sync::Mutex::new(None),
        })
    }

    /// Renew the token for `original`'s managed sign-in (an account or Nous Portal chosen with no
    /// key of its own) before each model request.
    pub(crate) fn renewing(self, original: &Config) -> Self {
        if !original.api_key.trim().is_empty() {
            return self;
        }
        let renew: Option<Renew> = if crate::portal::is_portal(&original.base_url) {
            Some(std::sync::Arc::new(|| Box::pin(crate::portal::access_token())))
        } else {
            crate::accounts::for_base_url(&original.base_url)
                .map(|account| -> Renew { std::sync::Arc::new(move || Box::pin(crate::accounts::access_token(account.id))) })
        };
        self.with_renewal(renew)
    }

    pub(crate) fn with_renewal(mut self, renew: Option<Renew>) -> Self {
        self.renew = renew;
        self
    }

    fn key(&self) -> String {
        self.key.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone().unwrap_or_else(|| self.config.api_key.clone())
    }

    pub(crate) fn config(&self) -> &Config { &self.config }
    pub(crate) fn options(&self) -> &crate::models::GenerationOptions { &self.options }

    fn error_message(&self, raw: &str) -> String {
        let mut text = raw.to_owned();
        for key in [self.config.api_key.clone(), self.key()] {
            let key = key.trim();
            if !key.is_empty() {
                text = text.replace(key, "[redacted]");
            }
        }
        text.chars()
            .filter(|character| !character.is_control() || *character == '\n')
            .take(480)
            .collect()
    }

    #[cfg(test)]
    pub async fn stream(
        &self,
        messages: Vec<Value>,
        emit: impl FnMut(&str) -> Result<()>,
    ) -> Result<Vec<ToolCall>> {
        self.stream_with_tools(messages, &crate::tools::tool_specs(), emit)
            .await
    }

    /// This future owns the request and response stream. Dropping it (as the
    /// session does when cancelled) stops reading and drops the HTTP I/O.
    #[cfg(test)]
    pub async fn stream_with_tools(
        &self,
        messages: Vec<Value>,
        tools: &[Value],
        emit: impl FnMut(&str) -> Result<()>,
    ) -> Result<Vec<ToolCall>> {
        Ok(self.stream_with_evidence(messages, tools, emit).await?.calls)
    }

    pub(crate) async fn stream_with_evidence(
        &self,
        messages: Vec<Value>,
        tools: &[Value],
        mut emit: impl FnMut(&str) -> Result<()>,
    ) -> Result<ProviderOutput> {
        let result = self.stream_inner(messages, tools, &mut emit).await;
        result.map_err(|error| anyhow!("{}", self.error_message(&format!("{error:#}"))))
    }

    async fn stream_inner(
        &self,
        messages: Vec<Value>,
        tools: &[Value],
        emit: &mut impl FnMut(&str) -> Result<()>,
    ) -> Result<ProviderOutput> {
        let mut body = match self.protocol {
            Protocol::OpenAi => {
                let mut body =
                    json!({"model":self.config.model.trim(),"messages":messages,"stream":true});
                if !tools.is_empty() {
                    body["tools"] = json!(tools);
                }
                body
            }
            Protocol::Anthropic => anthropic_body(self.config.model.trim(), messages, tools)?,
            Protocol::Responses => responses_body(self.config.model.trim(), messages, tools),
        };
        match self.protocol {
            Protocol::Anthropic => cache_anthropic_prefix(&mut body),
            Protocol::OpenAi => cache_openrouter_prefix(&self.endpoint, &mut body),
            Protocol::Responses => {}
        }
        if self.protocol == Protocol::Responses {
            self.options.apply_responses_reasoning(&mut body)?;
        } else {
            self.options.apply(&mut body, self.protocol == Protocol::Anthropic)?;
        }
        if let Some(renew) = &self.renew {
            let key = renew().await?;
            *self.key.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(key);
        }
        let request = self.post(&self.endpoint, &body, "text/event-stream");
        let response = Self::send_request(request).await?;
        self.read_stream(response, emit).await
    }

    fn post(&self, endpoint: &Url, body: &Value, accept: &str) -> RequestBuilder {
        let request = self
            .client
            .post(endpoint.clone())
            .header("accept", accept)
            .json(body);
        self.authenticate(request, endpoint)
    }

    pub(crate) fn catalog_requests(&self) -> Result<Vec<RequestBuilder>> {
        let suffix = match self.protocol {
            Protocol::OpenAi => "/chat/completions",
            Protocol::Anthropic => "/messages",
            Protocol::Responses => "/responses",
        };
        let base = self.endpoint.path().strip_suffix(suffix)
            .ok_or_else(|| anyhow!("Invalid model catalog endpoint"))?;
        let mut endpoint = self.endpoint.clone();
        endpoint.set_path(&format!("{base}/models"));
        // The subscription catalog gates models on CLI compatibility, independently of
        // this desktop's release. An empty catalog can try the legacy sentinel.
        let versions: &[Option<&str>] = if self.protocol == Protocol::Responses {
            &[Some("99.0.0"), Some("0.0.0")]
        } else {
            &[None]
        };
        Ok(versions.iter().map(|version| {
            let mut endpoint = endpoint.clone();
            if let Some(version) = version {
                endpoint.query_pairs_mut().append_pair("client_version", version);
            }
            let request = self.client.get(endpoint.clone())
                .header("accept", "application/json")
                .timeout(Duration::from_secs(35));
            self.authenticate(request, &endpoint)
        }).collect())
    }

    fn authenticate(&self, mut request: RequestBuilder, endpoint: &Url) -> RequestBuilder {
        let key = self.key();
        let key = key.trim();
        match self.protocol {
            Protocol::Anthropic => {
                request = request.header("anthropic-version", "2023-06-01");
                if !key.is_empty() {
                    request = request.header("x-api-key", key);
                    // OAuth gateways (MiniMax) authenticate the bearer token.
                    if endpoint.path().contains("/anthropic/") { request = request.bearer_auth(key); }
                }
            }
            Protocol::Responses => {
                request = request.bearer_auth(key).header("originator", "hermes-agent")
                    .header("User-Agent", concat!("HermesAgent/", env!("CARGO_PKG_VERSION")));
                for (name, value) in codex_account_headers(key) { request = request.header(name, value); }
            }
            Protocol::OpenAi => {
                if !key.is_empty() {
                    request = request.bearer_auth(key);
                }
            }
        }
        request
    }

    async fn send_request(request: RequestBuilder) -> Result<Response> {
        let response = request.send().await.map_err(|error| {
            if error.is_timeout() {
                anyhow!("Model provider connection timed out")
            } else {
                anyhow!("Could not reach model provider")
            }
        })?;
        if !response.status().is_success() {
            return Err(http_error(response).await);
        }
        Ok(response)
    }

    async fn read_stream(
        &self,
        response: Response,
        emit: &mut impl FnMut(&str) -> Result<()>,
    ) -> Result<ProviderOutput> {
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        ensure!(
            content_type
                .split(';')
                .next()
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream")),
            "Provider did not return a server-sent event stream; check its endpoint and streaming support"
        );
        let mut streamed_bytes = 0;
        let bytes = response.bytes_stream().map(move |chunk| -> Result<_> {
            let chunk = chunk.map_err(|_| anyhow!("Provider stream interrupted"))?;
            streamed_bytes += chunk.len();
            ensure!(
                streamed_bytes <= MAX_STREAM_BYTES,
                "Provider stream exceeded the maximum size"
            );
            Ok(chunk)
        });
        let mut events = bytes.eventsource();
        let mut openai = OpenAiStream::default();
        let mut anthropic = AnthropicStream::default();
        let mut responses = ResponsesStream::default();
        while let Some(event) = events.next().await {
            // Do not expose parser/transport error bodies reflected by a proxy.
            let event = event.map_err(|_| {
                anyhow!("Provider stream interrupted or contained invalid event framing")
            })?;
            if event.data.trim().is_empty() {
                continue;
            }
            if event.event == "error" && self.protocol == Protocol::OpenAi {
                let error: Value = serde_json::from_str(&event.data).unwrap_or(Value::Null);
                bail!("{}", provider_error(error.get("error").unwrap_or(&error)));
            }
            let done = match self.protocol {
                Protocol::OpenAi => openai.accept(&event.data, emit)?,
                Protocol::Anthropic => anthropic.accept(&event.data, emit)?,
                Protocol::Responses => responses.accept(&event.data, emit)?,
            };
            if done {
                break;
            }
        }
        let reasoning_summary = std::mem::take(&mut responses.reasoning_summary);
        let calls = match self.protocol {
            Protocol::OpenAi => openai.finish(),
            Protocol::Anthropic => anthropic.finish(),
            Protocol::Responses => responses.finish(),
        }?;
        Ok(ProviderOutput { calls, reasoning_summary })
    }
}

/// Workspace headers the Codex backend reads from the OAuth JWT (agent/codex_headers.py).
fn codex_account_headers(token: &str) -> Vec<(&'static str, String)> {
    use base64::Engine;
    let Some(payload) = token.split('.').nth(1) else { return vec![] };
    let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')) else { return vec![] };
    let Ok(claims) = serde_json::from_slice::<Value>(&bytes) else { return vec![] };
    let auth = &claims["https://api.openai.com/auth"];
    let mut headers = vec![];
    if let Some(id) = auth["chatgpt_account_id"].as_str().filter(|s| !s.is_empty()) { headers.push(("ChatGPT-Account-ID", id.to_owned())); }
    if let Some(r) = auth["chatgpt_data_residency"].as_str().or(auth["chatgpt_compute_residency"].as_str()).filter(|s| !s.is_empty()) {
        headers.push(("x-openai-internal-codex-residency", r.to_owned()));
    }
    headers
}

/// Chat-completions messages → a Responses request (instructions + input items).
fn responses_body(model: &str, messages: Vec<Value>, tools: &[Value]) -> Value {
    let mut instructions = String::new();
    let mut input = vec![];
    for message in messages {
        match message["role"].as_str().unwrap_or_default() {
            "system" => { if !instructions.is_empty() { instructions.push_str("\n\n"); } instructions.push_str(message["content"].as_str().unwrap_or_default()); }
            "user" => {
                let content = match &message["content"] {
                    Value::String(text) => vec![json!({"type":"input_text","text":text})],
                    Value::Array(parts) => parts.iter().filter_map(|part| match part["type"].as_str() {
                        Some("text") => Some(json!({"type":"input_text","text":part["text"]})),
                        Some("image_url") => Some(json!({"type":"input_image","image_url":part["image_url"]["url"]})),
                        _ => None,
                    }).collect(),
                    _ => vec![],
                };
                input.push(json!({"type":"message","role":"user","content":content}));
            }
            "assistant" => {
                if let Some(text) = message["content"].as_str().filter(|t| !t.is_empty()) {
                    input.push(json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}));
                }
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    input.push(json!({"type":"function_call","call_id":call["id"],"name":call["function"]["name"],"arguments":call["function"]["arguments"]}));
                }
            }
            "tool" => input.push(json!({"type":"function_call_output","call_id":message["tool_call_id"],"output":message["content"]})),
            _ => {}
        }
    }
    let tools: Vec<Value> = tools.iter().map(|t| json!({"type":"function","name":t["function"]["name"],"description":t["function"]["description"],"parameters":t["function"]["parameters"]})).collect();
    let mut body = json!({"model":model,"instructions":instructions,"input":input,"stream":true,"store":false,"parallel_tool_calls":true});
    if !tools.is_empty() { body["tools"] = json!(tools); body["tool_choice"] = json!("auto"); }
    body
}

#[derive(Default)]
struct ResponsesStream { calls: Vec<ToolCall>, content_bytes: usize, done: bool, reasoning_summary: String }

pub(crate) struct ProviderOutput {
    pub calls: Vec<ToolCall>,
    pub reasoning_summary: String,
}
impl ResponsesStream {
    fn accept(&mut self, data: &str, emit: &mut impl FnMut(&str) -> Result<()>) -> Result<bool> {
        let event: Value = serde_json::from_str(data).map_err(|_| anyhow!("Invalid provider stream event"))?;
        match event["type"].as_str().unwrap_or_default() {
            // A refusal is the provider's answer: shown, not reported as an empty response.
            "response.output_text.delta" | "response.refusal.delta" => if let Some(delta) = event["delta"].as_str() { emit_text(&mut self.content_bytes, delta, emit)?; },
            "response.reasoning_summary_text.delta" => if let Some(delta) = event["delta"].as_str() {
                let available = (8 * 1024usize).saturating_sub(self.reasoning_summary.len());
                self.reasoning_summary.push_str(&delta[..delta.floor_char_boundary(available.min(delta.len()))]);
            },
            "response.output_item.done" if event["item"]["type"] == "function_call" => {
                ensure!(self.calls.len() < MAX_TOOL_CALLS, "Too many tool calls in one response");
                let item = &event["item"];
                self.calls.push(ToolCall { id: item["call_id"].as_str().unwrap_or_default().to_owned(),
                    name: item["name"].as_str().unwrap_or_default().to_owned(), arguments: item["arguments"].as_str().unwrap_or("{}").to_owned() });
            }
            "response.completed" | "response.done" => { self.done = true; return Ok(true); }
            "response.failed" | "error" | "response.error" => {
                let error = event.pointer("/response/error").or(event.get("error")).unwrap_or(&event);
                bail!("{}", provider_error(error));
            }
            "response.incomplete" => bail!("The provider stopped this response early ({})", event.pointer("/response/incomplete_details/reason").and_then(Value::as_str).unwrap_or("incomplete")),
            _ => {}
        }
        Ok(false)
    }
    fn finish(self) -> Result<Vec<ToolCall>> {
        ensure!(self.done, "Provider stream ended before the response completed");
        // The same contract as the other protocols: a usable answer, and well-formed calls.
        ensure!(
            !self.calls.is_empty() || self.content_bytes > 0,
            "Provider completed without returning a usable answer"
        );
        valid_calls(self.calls)
    }
}

async fn http_error(mut response: Response) -> anyhow::Error {
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("unspecified")
        .to_owned();
    let mut data = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        let remaining = 65_536usize.saturating_sub(data.len());
        data.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if data.len() >= 65_536 {
            break;
        }
    }
    let value = serde_json::from_slice::<Value>(&data).ok();
    let reason = value.as_ref().and_then(|value| {
        value
            .pointer("/error/message")
            .and_then(Value::as_str)
            .or_else(|| value.pointer("/error/detail").and_then(Value::as_str))
            .or_else(|| value.get("error").and_then(Value::as_str))
            .or_else(|| value.get("message").and_then(Value::as_str))
            .or_else(|| value.get("detail").and_then(Value::as_str))
    });
    if let Some(reason) = reason {
        return anyhow!("Provider HTTP {status} — {reason}");
    }
    let hint = match status.as_u16() {
        401 => "Authentication rejected; sign in again or check this endpoint's API key",
        403 => {
            "Inference access denied; check sign-in, inference scope, and access to the selected model"
        }
        404 => "Endpoint or model not found; check the provider URL and model ID",
        429 => "Provider rate or usage limit reached; check the account quota",
        500..=599 => "Provider or gateway is unavailable; try again shortly",
        _ => "Request failed; check provider settings and credentials",
    };
    let format = if value.is_some() {
        "JSON error without a message"
    } else {
        "non-JSON error"
    };
    anyhow!("Provider HTTP {status} — {hint} ({format}; content type: {content_type})")
}

#[cfg(test)]
mod tests;
