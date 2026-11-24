//! Route embedded plugin inference through the bound Hermes profile.
use crate::{
    Config,
    inference::{CompletionInference, EmbeddingInference, InferenceTarget, ModelIdentity},
    models::GenerationOptions,
    settings::ProfileSettings,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

const MAX_INPUT_BYTES: usize = 1024 * 1024;

pub(crate) async fn serve(home: &Path, chat: Option<&Config>, params: &Value) -> Result<Value> {
    let kwargs = params.get("kwargs").filter(|value| value.is_object())
        .context("Expected plugin inference options")?;
    let timeout = kwargs["timeout"].as_f64().unwrap_or(30.0);
    ensure!(timeout.is_finite() && timeout > 0.0 && timeout <= 300.0, "Invalid inference timeout");
    tokio::time::timeout(Duration::from_secs_f64(timeout), async {
        match params["operation"].as_str() {
            Some("aembed") => embed(home, &params["args"], kwargs).await,
            Some("acomplete_structured") => complete(home, chat, kwargs).await,
            _ => bail!("Unsupported plugin inference operation"),
        }
    }).await.context("inference_deadline")?
}

fn identity(route: &ModelIdentity) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, route.provider.as_bytes());
    let suffix: String = digest.as_ref()[..16].iter().map(|byte| format!("{byte:02x}")).collect();
    format!("hermes-{suffix}")
}

async fn embed(home: &Path, args: &Value, kwargs: &Value) -> Result<Value> {
    let config = ProfileSettings::load(home)?.memory_embedding.context("embedding_unconfigured")?;
    ensure!(kwargs["input_type"].as_str().is_none_or(|kind| matches!(kind, "query" | "document")), "Unsupported embedding input type");
    let inputs = args.get(0).or_else(|| kwargs.get("inputs")).and_then(Value::as_array)
        .context("Expected embedding input list")?;
    ensure!((1..=128).contains(&inputs.len()), "Invalid embedding batch size");
    let inputs: Vec<String> = inputs.iter().map(|value| {
        value.as_str().map(str::to_owned).context("Expected embedding input text")
    }).collect::<Result<_>>()?;
    ensure!(inputs.iter().map(String::len).sum::<usize>() <= MAX_INPUT_BYTES, "Embedding batch exceeds bounds");
    let inference = EmbeddingInference::open(
        home,
        &InferenceTarget::Endpoint(config.endpoint_id),
        None,
        config.dimensions,
    ).await.map_err(|_| anyhow::anyhow!("embedding_unsupported"))?;
    let vectors = inference.embed(&inputs, MAX_INPUT_BYTES).await
        .map_err(|_| anyhow::anyhow!("embedding_failed"))?;
    Ok(json!({
        "provider":identity(inference.route()), "model":inference.route().model,
        "vectors":vectors, "dimensions":inference.dimensions(),
        "audit":{"input_type_handling":"shared","normalization":"none"}
    }))
}

async fn complete(home: &Path, chat: Option<&Config>, kwargs: &Value) -> Result<Value> {
    let chat = chat.context("inference_unavailable")?;
    let instructions = kwargs["instructions"].as_str().context("Expected inference instructions")?;
    let system = match kwargs["system_prompt"].as_str() {
        Some(system) if !system.is_empty() => format!("{system}\n\n{instructions}"),
        _ => instructions.to_owned(),
    };
    let blocks = kwargs["input"].as_array().context("Expected structured inference input")?;
    ensure!((1..=128).contains(&blocks.len()), "Invalid number of input blocks");
    let mut text = Vec::with_capacity(blocks.len());
    for block in blocks {
        ensure!(block["type"] == "text", "Only text input is available");
        text.push(block["text"].as_str().context("Expected input text")?);
    }
    let input = text.join("\n\n");
    let schema = &kwargs["json_schema"];
    ensure!(schema.is_object(), "Expected a structured inference schema");
    ensure!(system.len() + input.len() + serde_json::to_vec(schema)?.len() <= MAX_INPUT_BYTES,
        "Structured inference request exceeds bounds");
    let name = kwargs["schema_name"].as_str().filter(|name| !name.is_empty() && name.len() <= 128)
        .context("Expected a schema name")?;
    let options = GenerationOptions {
        temperature: kwargs["temperature"].as_f64(),
        max_output_tokens: kwargs["max_tokens"].as_u64().map(u32::try_from).transpose()?,
        ..Default::default()
    };
    let inference = CompletionInference::with_options(home, chat, &InferenceTarget::Chat, None, options)
        .await.map_err(|_| anyhow::anyhow!("inference_unavailable"))?;
    let parsed = inference.complete_json(&system, &input, name, schema).await
        .map_err(|_| anyhow::anyhow!("inference_failed"))?;
    ensure!(serde_json::to_vec(&parsed)?.len() <= MAX_INPUT_BYTES, "inference_result_limit");
    Ok(json!({
        "provider":identity(inference.route()), "model":inference.route().model,
        "content_type":"json", "parsed":parsed
    }))
}
