use super::{Event, Sender, ToolCall, TurnCorrection};
use anyhow::{Context, Result};
use async_channel::Receiver;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

pub(super) async fn receive(receiver: Option<&Receiver<TurnCorrection>>) -> TurnCorrection {
    if let Some(receiver) = receiver
        && let Ok(input) = receiver.recv().await
    {
        return input;
    }
    std::future::pending().await
}

pub(super) fn clarification_spec() -> Value {
    json!({"type":"function","function":{
        "name":"clarify","description":"Ask the user a question before continuing this turn.",
        "parameters":{"type":"object","properties":{
            "question":{"type":"string"},
            "choices":{"type":"array","items":{"type":"string"}}
        },"required":["question"]}
    }})
}

#[derive(Deserialize)]
struct Question {
    question: String,
    #[serde(default)]
    choices: Vec<String>,
}

pub(super) async fn clarify(
    call: &ToolCall,
    events: &Sender<Event>,
    timeout: u64,
) -> Result<(bool, String)> {
    let Ok(question) = serde_json::from_str::<Question>(&call.arguments) else {
        return Ok((
            false,
            json!({"error":"Expected a question and optional choices"}).to_string(),
        ));
    };
    if question.question.trim().is_empty()
        || question.question.len() > 8192
        || question.choices.len() > 12
        || question
            .choices
            .iter()
            .any(|s| s.trim().is_empty() || s.len() > 1024)
    {
        return Ok((
            false,
            json!({"error":"The clarification is too large or empty"}).to_string(),
        ));
    }
    let (answer, response) = async_channel::bounded(1);
    events
        .send(Event::Clarification {
            question: question.question,
            choices: question.choices,
            answer,
        })
        .await
        .context("Conversation window closed")?;
    match tokio::time::timeout(Duration::from_secs(timeout), response.recv()).await {
        Ok(Ok(answer)) => Ok((true, json!({"answer":answer}).to_string())),
        _ => Ok((false, json!({"error":"Clarification expired"}).to_string())),
    }
}
