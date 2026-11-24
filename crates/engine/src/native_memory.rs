//! Explicit, read-only memory requests supplied by a profile gateway.
use crate::{Config, Event, ToolCall, privacy, settings::ProfileSettings};
use anyhow::{Context, Result, ensure};
use async_channel::Sender;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

/// Implemented by the host for one admitted turn and its frozen owner binding.
/// A preview does not acknowledge external consumption or reinforce Freshness.
pub trait MemoryQuery: Send + Sync {
    fn tools(&self) -> Vec<MemoryTool>;
    fn invoke(&self, request: MemoryRequest, config: &Config) -> Result<MemorySelection>;
}

/// The model gets bounded content only after the host confirms current source access.
pub enum MemorySelection {
    Result {
        count: usize,
        content: Value,
        check_disclosure: Box<dyn FnOnce() -> Result<()> + Send>,
    },
    Rejected {
        code: &'static str,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryTool {
    Query,
    Refine,
    Traverse,
    ArchiveSearch,
    ArchiveRead,
    EchoRead,
}

impl MemoryTool {
    pub fn name(self) -> &'static str {
        match self {
            Self::Query => "memory_query",
            Self::Refine => "refine_query",
            Self::Traverse => "traverse_graph",
            Self::ArchiveSearch => "archive_search",
            Self::ArchiveRead => "archive_read",
            Self::EchoRead => "echo_read",
        }
    }

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        [
            Self::Query,
            Self::Refine,
            Self::Traverse,
            Self::ArchiveSearch,
            Self::ArchiveRead,
            Self::EchoRead,
        ]
        .into_iter()
        .find(|tool| tool.name() == name)
    }

    pub(crate) fn spec(self) -> Value {
        let query = json!({"type":"string","minLength":1,"maxLength":8192});
        let token = json!({"type":"string","minLength":1,"maxLength":128});
        let ids = json!({"type":"array","items":token,"maxItems":10,"uniqueItems":true});
        let (description, properties, required) = match self {
            Self::Query => (
                "Search processed memories in the assigned REL.",
                json!({"query":query}),
                vec!["query"],
            ),
            Self::Refine => (
                "Refine the current search using IDs returned in this turn.",
                json!({"query":query,"search_id":token,"selected_ids":ids}),
                vec!["query", "search_id", "selected_ids"],
            ),
            Self::Traverse => (
                "Explore neighbors of Memory seeds returned at the current cut.",
                json!({"search_id":token,"result_id":token,"seed_ids":ids}),
                vec!["search_id", "result_id", "seed_ids"],
            ),
            Self::ArchiveSearch => (
                "Search Archive messages in the assigned REL. Include a resolvable time reference in the query.",
                json!({"query":query,"basis":{"type":"string","enum":["source","mentioned"]}}),
                vec!["query"],
            ),
            Self::ArchiveRead => (
                "Read an exact Archive fragment returned earlier in this turn.",
                json!({"reference":token}),
                vec!["reference"],
            ),
            Self::EchoRead => (
                "Read execution evidence attached to an exact source range returned earlier in this turn.",
                json!({"reference":token}),
                vec!["reference"],
            ),
        };
        json!({"type":"function","function":{"name":self.name(),"description":description,
            "parameters":{"type":"object","properties":properties,"required":required,"additionalProperties":false}}})
    }
}

#[derive(Deserialize)]
#[serde(
    tag = "tool",
    content = "arguments",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum MemoryRequest {
    #[serde(rename = "memory_query")]
    Query {
        query: String,
    },
    #[serde(rename = "refine_query")]
    Refine {
        query: String,
        search_id: String,
        selected_ids: Vec<String>,
    },
    #[serde(rename = "traverse_graph")]
    Traverse {
        search_id: String,
        result_id: String,
        seed_ids: Vec<String>,
    },
    ArchiveSearch {
        query: String,
        #[serde(default)]
        basis: SearchTimeBasis,
    },
    ArchiveRead {
        reference: String,
    },
    EchoRead {
        reference: String,
    },
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchTimeBasis {
    #[default]
    Source,
    Mentioned,
}

pub fn redact_text(text: &str) -> String {
    let mut text = text.to_owned();
    privacy::redact(&mut text);
    text
}

fn safe_query(query: &str, config: &Config) -> Result<String> {
    let query = query.trim();
    ensure!(
        !query.is_empty()
            && query.len() <= 8192
            && crate::references::parse(query).is_empty()
            && !crate::memory_capture::contains_inline_data(query)
            && (config.api_key.is_empty() || !query.contains(&config.api_key)),
        "invalid query"
    );
    Ok(redact_text(query))
}

fn parse(call: &ToolCall, config: &Config) -> Result<(MemoryRequest, String)> {
    let mut arguments: Value = serde_json::from_str(&call.arguments)?;
    let object = arguments
        .as_object_mut()
        .context("Memory arguments must be an object")?;
    for (name, value) in object {
        if name == "query" {
            *value = json!(safe_query(
                value.as_str().context("Invalid memory query")?,
                config
            )?);
        } else if let Some(items) = value.as_array() {
            ensure!(
                items.len() <= 10
                    && items
                        .iter()
                        .all(|item| item.as_str().is_some_and(safe_token)),
                "Invalid memory IDs"
            );
        } else {
            ensure!(
                value.as_str().is_some_and(safe_token),
                "Invalid memory reference"
            );
        }
    }
    let reviewed = arguments.to_string();
    let request = serde_json::from_value(json!({"tool":call.name,"arguments":arguments}))?;
    Ok((request, reviewed))
}

fn safe_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 128
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte))
}

/// Only the receipt-sized summary is recorded in conversation history. The
/// selected content lives in the requesting turn for its next model round.
pub(crate) struct Reply {
    pub approved: bool,
    pub summary: String,
    pub model_content: Option<PendingDisclosure>,
}
pub(crate) struct PendingDisclosure {
    content: String,
    check: Box<dyn FnOnce() -> Result<()> + Send>,
}
impl Reply {
    fn error(reason: &str) -> Self {
        Self {
            approved: false,
            summary: json!({"error":reason}).to_string(),
            model_content: None,
        }
    }
}

pub(crate) async fn invoke(
    backend: Arc<dyn MemoryQuery>,
    call: &ToolCall,
    config: &Config,
    events: &Sender<Event>,
    settings: &ProfileSettings,
) -> Result<Reply> {
    let Ok((request, reviewed_arguments)) = parse(call, config) else {
        return Ok(Reply::error("Invalid memory tool arguments"));
    };
    let reviewed = ToolCall {
        arguments: reviewed_arguments,
        ..call.clone()
    };
    if settings.needs_approval(&call.name, false, false)
        && !crate::request_approval(reviewed, events, settings.approval_timeout).await?
    {
        return Ok(Reply::error("Memory query denied or approval expired"));
    }
    let config = config.clone();
    let task = tokio::task::spawn_blocking(move || backend.invoke(request, &config));
    let Ok(Ok(Ok(selection))) = tokio::time::timeout(Duration::from_secs(60), task).await else {
        return Ok(Reply::error("Memory query unavailable"));
    };
    match selection {
        MemorySelection::Rejected { code, message } => Ok(Reply {
            approved: false,
            summary: json!({"error":code,"message":message}).to_string(),
            model_content: None,
        }),
        MemorySelection::Result {
            count,
            content,
            check_disclosure,
        } => {
            let encoded = content.to_string();
            if encoded.len() > settings.tool_output_bytes {
                return Ok(Reply::error(
                    "Memory result exceeds the configured tool output limit",
                ));
            }
            Ok(Reply {
                approved: true,
                summary: json!({"returned":count}).to_string(),
                model_content: Some(PendingDisclosure {
                    content: encoded,
                    check: check_disclosure,
                }),
            })
        }
    }
}

pub(crate) async fn attach_to_request(
    messages: &mut [Value],
    current_entries: usize,
    results: Vec<(usize, PendingDisclosure)>,
) -> Result<()> {
    let start = messages
        .len()
        .checked_sub(current_entries)
        .context("Missing current turn")?;
    for (index, disclosure) in results {
        let message = messages
            .get_mut(start + index)
            .context("Missing memory tool result")?;
        ensure!(message["role"] == "tool", "Invalid memory tool position");
        // Leave the durable count in the message if authorization changed.
        if matches!(
            tokio::task::spawn_blocking(disclosure.check).await,
            Ok(Ok(()))
        ) {
            message["content"] = Value::String(disclosure.content);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_accepts_only_bounded_text_without_references_or_provider_credentials() {
        let config = Config {
            model: String::new(),
            base_url: String::new(),
            api_key: "provider-credential".into(),
        };
        let parse_args = |args: Value| {
            parse(
                &ToolCall {
                    id: "id".into(),
                    name: "memory_query".into(),
                    arguments: args.to_string(),
                },
                &config,
            )
            .map(|(request, _)| match request {
                MemoryRequest::Query { query } => query,
                _ => unreachable!(),
            })
        };
        for args in [
            json!({"query":"  "}),
            json!({"query":"@file:secret.txt"}),
            json!({"query":"data:text/plain;base64,Zm9v"}),
            json!({"query":"use provider-credential"}),
            json!({"query":"x".repeat(8193)}),
            json!({"query":"color","owner":"another"}),
        ] {
            assert!(parse_args(args).is_err());
        }
        assert_eq!(
            parse_args(json!({"query":"  which color?  "})).unwrap(),
            "which color?"
        );
        assert_eq!(
            parse_args(json!({"query":"the password=example"})).unwrap(),
            "the password=[REDACTED]"
        );
    }

    #[tokio::test]
    async fn disclose_only_the_current_authorized_tool_result_even_for_repeated_ids() {
        let receipt = json!({"role":"tool","tool_call_id":"reused","content":"count"});
        let mut messages = [
            receipt.clone(), // A previous turn.
            json!({"role":"assistant"}),
            receipt.clone(), // Current turn, authorized.
            receipt.clone(), // Current turn, revoked while another tool ran.
        ];
        let allowed = PendingDisclosure {
            content: "first".into(),
            check: Box::new(|| Ok(())),
        };
        let revoked = PendingDisclosure {
            content: "second".into(),
            check: Box::new(|| anyhow::bail!("revoked")),
        };
        attach_to_request(&mut messages, 3, vec![(1, allowed), (2, revoked)])
            .await
            .unwrap();
        assert_eq!(messages[0], receipt);
        assert_eq!(messages[2]["content"], "first");
        assert_eq!(messages[3], receipt);
    }
}
