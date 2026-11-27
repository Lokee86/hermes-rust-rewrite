//! Native profile Identity, standing context and scoped Pins.

use crate::{
    Config, Event, ToolCall, context, models::GenerationOptions, request_approval,
    settings::ProfileSettings,
};
use anyhow::{Result, ensure};
use async_channel::Sender;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{ops::Range, path::Path, sync::Arc};

pub trait EgoAccess: Send + Sync {
    fn context(&self, query: &str, budget: usize) -> Result<EgoPackage>;
    fn pin(&self, request: PinRequest) -> Result<Value>;
    fn identity(&self) -> Result<Option<String>>;
    fn prepare_identity(&self, text: String) -> Result<IdentityReview>;
}

pub struct EgoPackage {
    pub identity: Option<String>,
    pub standing_context: String,
}

pub struct IdentityReview {
    pub review: Value,
    pub apply: Box<dyn FnOnce() -> Result<()> + Send>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinScope {
    Rel,
    Phy,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinPriority {
    High,
    Normal,
    Low,
    Raise,
    Lower,
}

#[derive(Deserialize)]
#[serde(
    tag = "tool",
    content = "arguments",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum PinRequest {
    ListPins {
        scope: PinScope,
    },
    PinAnchor {
        scope: PinScope,
        text: String,
        priority: Option<PinPriority>,
    },
    ModifyAnchor {
        scope: PinScope,
        id: String,
        revision: u64,
        text: Option<String>,
        priority: Option<PinPriority>,
    },
    DeleteAnchor {
        scope: PinScope,
        id: String,
        revision: u64,
    },
}

pub(crate) fn is_pin_tool(tool: &str) -> bool {
    matches!(
        tool,
        "list_pins" | "pin_anchor" | "modify_anchor" | "delete_anchor"
    )
}

pub(crate) fn specs() -> Vec<Value> {
    let scope = json!({"type":"string", "enum":["rel","phy"]});
    let text = json!({"type":"string", "minLength":1, "maxLength":8192});
    let id = json!({"type":"string", "pattern":"^[0-9a-f]{32}$"});
    let revision = json!({"type":"integer", "minimum":1});
    let fixed = json!({"type":"string","enum":["high","normal","low"]});
    let change = json!({"type":"string","enum":["high","normal","low","raise","lower"]});
    [
        ("list_pins", "Read current Pins in the specified REL or PHY, including Pins absent from the prompt.", json!({"scope":scope}), vec!["scope"]),
        ("pin_anchor", "Save a standing instruction in the specified REL or PHY when the user asks to pin it. Omit priority for Normal.", json!({"scope":scope,"text":text,"priority":fixed}), vec!["scope","text"]),
        ("modify_anchor", "Update a Pin by its current revision. On a clear reaffirmation or softening of the same instruction, use raise or lower without changing the text. Honor explicitly named priorities.", json!({"scope":scope,"id":id,"revision":revision,"text":text,"priority":change}), vec!["scope","id","revision"]),
        ("delete_anchor", "Remove the matching Pin at its current revision when the user asks to remove it.", json!({"scope":scope,"id":id,"revision":revision}), vec!["scope","id","revision"]),
    ].into_iter().map(|(name, description, properties, required)| {
        json!({"type":"function","function":{"name":name,"description":description,
            "parameters":{"type":"object","additionalProperties":false,"properties":properties,"required":required}}})
    }).collect()
}

pub(crate) async fn invoke(
    ego: Arc<dyn EgoAccess>,
    call: &ToolCall,
    config: &Config,
    settings: &ProfileSettings,
    events: &Sender<Event>,
) -> Result<(bool, String)> {
    fn failed(error: &str) -> (bool, String) {
        (false, json!({"error": error}).to_string())
    }
    if !settings.memory_tool
        || call.arguments.len() > 10_000
        || (!config.api_key.is_empty() && call.arguments.contains(&config.api_key))
        || crate::memory_capture::contains_inline_data(&call.arguments)
    {
        return Ok(failed(
            "Pin tool is unavailable or its arguments are invalid",
        ));
    }
    let Ok(arguments) = serde_json::from_str::<Value>(&call.arguments) else {
        return Ok(failed("Invalid Pin arguments"));
    };
    if let Some(text) = arguments.get("text").and_then(Value::as_str)
        && (text.trim().is_empty()
            || text.len() > 8192
            || crate::privacy::redact_capture(text, &[&config.api_key]) != text)
    {
        return Ok(failed("Pin text is invalid"));
    }
    let Ok(request) =
        serde_json::from_value::<PinRequest>(json!({"tool":call.name,"arguments":arguments}))
    else {
        return Ok(failed("Invalid Pin arguments"));
    };
    let changes = !matches!(request, PinRequest::ListPins { .. });
    if settings.needs_approval(&call.name, changes, false)
        && !request_approval(call.clone(), events, settings.approval_timeout).await?
    {
        return Ok(failed("Pin action denied or approval expired"));
    }
    match tokio::task::spawn_blocking(move || ego.pin(request)).await {
        Ok(Ok(content)) => Ok((true, content.to_string())),
        _ => Ok(failed(
            "Pin action unavailable or its revision changed. Look up the current Pin before retrying.",
        )),
    }
}

/// Bound package text with a conservative byte-count while keeping Mercury's
/// ordinary request-size estimator and reserved output room.
pub fn count_context(text: &str) -> usize {
    text.len()
}

pub(crate) struct PersonaSlot(pub Range<usize>);

pub(crate) async fn apply(
    ego: &Arc<dyn EgoAccess>,
    query: &str,
    config: &Config,
    options: &GenerationOptions,
    profile: &Path,
    messages: &mut [Value],
    tools: &[Value],
    host_prompt: &str,
    slot: &PersonaSlot,
) {
    if let Some(system) = messages.first_mut() {
        system["content"] = json!(host_prompt);
    }

    if !ProfileSettings::load(profile).is_ok_and(|settings| {
        settings.memory_ego_enabled
            && settings.memory_provider.resolve(true) == crate::settings::MemoryProvider::Reliquary
    }) {
        return;
    }
    let remaining = context::ego_budget(config, options, context::estimate(messages, tools));
    let budget = remaining.saturating_sub(4);
    if budget == 0 {
        return;
    }
    let query = if query.len() <= 8192
        && !crate::memory_capture::contains_inline_data(query)
        && crate::references::parse(query).is_empty()
        && (config.api_key.is_empty() || !query.contains(&config.api_key))
    {
        crate::native_memory::redact_text(query)
    } else {
        String::new()
    };
    let ego = ego.clone();
    let result = tokio::task::spawn_blocking(move || ego.context(&query, budget)).await;
    if let Ok(Ok(package)) = result
        && let Ok(prompt) = assemble(host_prompt, slot, package, budget)
        && let Some(system) = messages.first_mut()
    {
        system["content"] = json!(prompt);
    }
}

fn assemble(host: &str, slot: &PersonaSlot, package: EgoPackage, budget: usize) -> Result<String> {
    let cost = package.identity.as_deref().map_or(0, count_context)
        + count_context(&package.standing_context);
    ensure!(cost <= budget, "Ego context exceeds the reserved budget");
    let mut rendered = host.to_owned();
    if let Some(identity) = package.identity {
        ensure!(
            rendered.get(slot.0.clone()).is_some(),
            "Missing host persona slot"
        );
        rendered.replace_range(slot.0.clone(), &format!("\n\n{identity}"));
    }
    if !package.standing_context.is_empty() {
        rendered.push_str("\n\n");
        rendered.push_str(&package.standing_context);
    }
    Ok(rendered)
}
