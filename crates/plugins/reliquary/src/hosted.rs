//! Inference through the gateway's selected model and a profile-owned embedding endpoint.
use anyhow::{Context, Result, ensure};
use hermes_engine::{Config, credentials::EndpointStore, settings::ProfileSettings};
use reliquary_memory::{
    ConfiguredRuntime, EmbeddingEndpoint, EmbeddingEndpointError, EmbeddingNormalization,
    GeneralEndpoint, GeneralEndpointError, HostedEmbeddingInputSemantics, HostedInferenceIdentity,
    SwitchboardEmbeddingMode,
};
use reqwest::{Url, blocking::Client, redirect::Policy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{io::Read, path::Path, sync::Arc, time::Duration};

const LIMIT: usize = 8 * 1024 * 1024;

/// Persist the same non-secret compatibility identity as the stock Hermes integration.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct EmbeddingIdentity {
    pub(super) provider: String,
    model: String,
    dimensions: u32,
    input_type_handling: String,
    normalization: String,
}

struct Endpoint {
    url: Url,
    model: String,
    secret: String,
    client: Client,
}

impl Endpoint {
    fn from_config(config: &Config, operation: &str) -> Result<Self> {
        let mut url = config.openai_chat_url()?;
        if operation == "embeddings" {
            let base = url
                .path()
                .strip_suffix("/chat/completions")
                .context("unsupported OpenAI-compatible base")?;
            url.set_path(&format!("{base}/embeddings"));
        }
        Ok(Self {
            url,
            model: config.model.clone(),
            secret: config.api_key.clone(),
            client: Client::builder()
                .timeout(Duration::from_secs(60))
                .redirect(Policy::none())
                .build()?,
        })
    }

    fn request(&self, body: Value, max_bytes: usize) -> Result<Value> {
        ensure!(max_bytes > 0, "no response budget");
        let mut r = self.client.post(self.url.clone()).json(&body);
        if !self.secret.is_empty() {
            r = r.bearer_auth(&self.secret);
        }
        let response = r.send().context("host inference request failed")?;
        ensure!(
            response.status().is_success(),
            "host inference returned HTTP {}",
            response.status().as_u16()
        );
        let bound = max_bytes.min(LIMIT);
        let mut bytes = Vec::new();
        response.take(bound as u64 + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= bound,
            "host inference response exceeded its budget"
        );
        Ok(serde_json::from_slice(&bytes)?)
    }
}

impl GeneralEndpoint for Endpoint {
    fn model(&self) -> &str {
        &self.model
    }
    fn complete_json(
        &self,
        system: &str,
        user: &str,
        name: &str,
        schema: &Value,
    ) -> Result<Value, GeneralEndpointError> {
        let value = self.request(json!({"model":self.model, "messages":[
            {"role":"system","content":system},{"role":"user","content":user}],
            "response_format":{"type":"json_schema","json_schema":{"name":name,"strict":true,"schema":schema}}}), LIMIT)
            .map_err(|_| GeneralEndpointError::Failure("profile structured completion failed".into()))?;
        let text = value["choices"][0]["message"]["content"].as_str().ok_or(
            GeneralEndpointError::InvalidResponse("missing structured content"),
        )?;
        serde_json::from_str(text)
            .map_err(|_| GeneralEndpointError::InvalidResponse("invalid structured JSON"))
    }
}

struct Embed {
    endpoint: Endpoint,
    dimensions: u32,
}

impl EmbeddingEndpoint for Embed {
    fn model(&self) -> Option<&str> {
        Some(&self.endpoint.model)
    }
    fn dimensions(&self) -> u32 {
        self.dimensions
    }
    fn normalization(&self) -> EmbeddingNormalization {
        EmbeddingNormalization::None
    }
    fn supports_bounded_embedding(&self) -> bool {
        true
    }
    fn embed(
        &self,
        mode: SwitchboardEmbeddingMode,
        inputs: &[String],
    ) -> Result<Vec<Vec<f32>>, EmbeddingEndpointError> {
        self.embed_bounded(mode, inputs, LIMIT)
    }
    fn embed_bounded(
        &self,
        _mode: SwitchboardEmbeddingMode,
        inputs: &[String],
        max: usize,
    ) -> Result<Vec<Vec<f32>>, EmbeddingEndpointError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        if inputs.len() > 128 || inputs.iter().map(String::len).sum::<usize>() > 1024 * 1024 {
            return Err(EmbeddingEndpointError::InvalidConfiguration(
                "embedding batch exceeds bounds",
            ));
        }
        let result = self
            .endpoint
            .request(
                json!({"model":self.endpoint.model,"input":inputs,
            "dimensions":self.dimensions,"encoding_format":"float"}),
                max,
            )
            .map_err(|_| {
                EmbeddingEndpointError::Failure("profile embedding request failed".into())
            })?;
        let invalid = || EmbeddingEndpointError::InvalidResponse("invalid embedding response");
        if result["model"]
            .as_str()
            .is_some_and(|s| s != self.endpoint.model)
        {
            return Err(invalid());
        }
        let data = result["data"].as_array().ok_or_else(invalid)?;
        if data.len() != inputs.len() {
            return Err(invalid());
        }
        let mut rows = vec![None; inputs.len()];
        for item in data {
            let index = item["index"]
                .as_u64()
                .and_then(|i| usize::try_from(i).ok())
                .ok_or_else(invalid)?;
            let slot = rows.get_mut(index).ok_or_else(invalid)?;
            let vec: Vec<f32> =
                serde_json::from_value(item["embedding"].clone()).map_err(|_| invalid())?;
            if slot.is_some()
                || vec.len() != self.dimensions as usize
                || !vec.iter().all(|n| n.is_finite())
            {
                return Err(invalid());
            }
            *slot = Some(vec);
        }
        rows.into_iter()
            .map(|row| row.ok_or_else(invalid))
            .collect()
    }
}

pub(super) fn from_profile(
    home: &Path,
    chat: &Config,
) -> Result<Option<(ConfiguredRuntime, EmbeddingIdentity)>> {
    let Some(selected) = ProfileSettings::load(home)?.memory_embedding else {
        return Ok(None);
    };
    let embed = EndpointStore::open(&home.join("providers"))?.config(&selected.endpoint_id)?;
    let general = Endpoint::from_config(chat, "chat")?;
    let endpoint = Endpoint::from_config(&embed, "embeddings")?;
    let metadata = EmbeddingIdentity {
        provider: hermes_engine::credentials::scope(&embed.base_url)?,
        model: embed.model.clone(),
        dimensions: selected.dimensions,
        normalization: "none".into(),
        input_type_handling: "shared".into(),
    };
    let identity = HostedInferenceIdentity::new(
        hermes_engine::credentials::scope(&chat.base_url)?,
        chat.model.clone(),
        metadata.provider.clone(),
        embed.model,
        metadata.dimensions,
        HostedEmbeddingInputSemantics::Shared,
        EmbeddingNormalization::None,
    )?;
    let runtime = ConfiguredRuntime::hosted(
        Arc::new(general),
        Arc::new(Embed {
            endpoint,
            dimensions: metadata.dimensions,
        }),
        identity,
    )?;
    Ok(Some((runtime, metadata)))
}
