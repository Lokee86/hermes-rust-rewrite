//! Supply Hermes-owned inference to Reliquary's hosted Switchboard.
use anyhow::{Context, Result};
use hermes_engine::{Config, inference::ProfileInference};
use reliquary_memory::{
    ConfiguredRuntime, EmbeddingEndpoint, EmbeddingEndpointError, EmbeddingNormalization,
    GeneralEndpoint, GeneralEndpointError, HostedEmbeddingInputSemantics, HostedInferenceIdentity,
    SwitchboardEmbeddingMode,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{path::Path, sync::Arc};
use tokio::runtime::Handle;

/// Persist the non-secret identity of the selected embedding space.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct EmbeddingIdentity {
    pub(super) provider: String,
    model: String,
    dimensions: u32,
    input_type_handling: String,
    normalization: String,
}

// Reliquary invokes these ports from its processing workers or Hermes' blocking pool.
struct Hosted {
    inference: ProfileInference,
    executor: Handle,
}

impl GeneralEndpoint for Hosted {
    fn model(&self) -> &str {
        &self.inference.chat_route().model
    }

    fn complete_json(
        &self,
        system: &str,
        user: &str,
        name: &str,
        schema: &Value,
    ) -> Result<Value, GeneralEndpointError> {
        self.executor
            .block_on(self.inference.complete_json(system, user, name, schema))
            .map_err(|_| {
                GeneralEndpointError::Failure("Hermes structured completion failed".into())
            })
    }
}

impl EmbeddingEndpoint for Hosted {
    fn model(&self) -> Option<&str> {
        Some(&self.inference.embedding_route().model)
    }

    fn dimensions(&self) -> u32 {
        self.inference.dimensions()
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
        self.embed_bounded(mode, inputs, usize::MAX)
    }

    fn embed_bounded(
        &self,
        _: SwitchboardEmbeddingMode,
        inputs: &[String],
        max: usize,
    ) -> Result<Vec<Vec<f32>>, EmbeddingEndpointError> {
        self.executor
            .block_on(self.inference.embed(inputs, max))
            .map_err(|_| EmbeddingEndpointError::Failure("Hermes embedding request failed".into()))
    }
}

pub(super) fn from_profile(
    home: &Path,
    chat: &Config,
) -> Result<Option<(ConfiguredRuntime, EmbeddingIdentity)>> {
    let executor = Handle::try_current().context("Hermes inference runtime is unavailable")?;
    let Some(inference) = executor.block_on(ProfileInference::open(home, chat))? else {
        return Ok(None);
    };
    let embedding_route = inference.embedding_route();
    let chat_route = inference.chat_route();
    let metadata = EmbeddingIdentity {
        provider: embedding_route.provider.clone(),
        model: embedding_route.model.clone(),
        dimensions: inference.dimensions(),
        normalization: "none".into(),
        input_type_handling: "shared".into(),
    };
    let identity = HostedInferenceIdentity::new(
        &chat_route.provider,
        &chat_route.model,
        &metadata.provider,
        &metadata.model,
        metadata.dimensions,
        HostedEmbeddingInputSemantics::Shared,
        EmbeddingNormalization::None,
    )?;
    let host = Arc::new(Hosted {
        inference,
        executor,
    });
    let runtime = ConfiguredRuntime::hosted(host.clone(), host, identity)?;
    Ok(Some((runtime, metadata)))
}
