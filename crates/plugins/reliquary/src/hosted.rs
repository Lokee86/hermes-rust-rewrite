//! Execute Reliquary Switchboard routes through Hermes' authenticated inference.

use super::switchboard::switchboard_config;
use anyhow::{Context, Result};
use hermes_engine::{
    Config,
    credentials::EndpointStore,
    inference::{CompletionInference, EmbeddingInference, InferenceTarget},
};
use reliquary_memory::{
    ConfiguredDecisionEndpoint, ConfiguredRuntime, ConfiguredRuntimeError, EmbeddingEndpoint,
    EmbeddingEndpointError, EmbeddingNormalization, GeneralEndpoint, GeneralEndpointError,
    ModelSlot, SwitchboardEmbeddingMode,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, btree_map::Entry},
    path::Path,
    sync::Arc,
};
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

// Reliquary invokes these endpoints from its workers or Hermes' blocking pool.
struct Completion {
    inference: CompletionInference,
    executor: Handle,
}

impl GeneralEndpoint for Completion {
    fn model(&self) -> &str {
        &self.inference.route().model
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

struct Embedding {
    inference: EmbeddingInference,
    executor: Handle,
    normalization: EmbeddingNormalization,
}

impl EmbeddingEndpoint for Embedding {
    fn model(&self) -> Option<&str> {
        Some(&self.inference.route().model)
    }

    fn dimensions(&self) -> u32 {
        self.inference.dimensions()
    }

    fn normalization(&self) -> EmbeddingNormalization {
        self.normalization
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
        let mut vectors = self
            .executor
            .block_on(self.inference.embed(inputs, max))
            .map_err(|_| {
                EmbeddingEndpointError::Failure("Hermes embedding request failed".into())
            })?;
        if self.normalization == EmbeddingNormalization::L2 {
            for vector in &mut vectors {
                let norm = vector
                    .iter()
                    .map(|value| f64::from(*value).powi(2))
                    .sum::<f64>()
                    .sqrt();
                if !norm.is_finite() || norm == 0.0 {
                    return Err(EmbeddingEndpointError::InvalidResponse(
                        "zero or non-finite embedding",
                    ));
                }
                for value in vector {
                    *value = (f64::from(*value) / norm) as f32;
                }
            }
        }
        Ok(vectors)
    }
}

pub(super) fn from_profile(
    home: &Path,
    chat: &Config,
) -> Result<Option<(ConfiguredRuntime, EmbeddingIdentity)>> {
    let config = switchboard_config(home)?;
    let Some(route) = config.model(ModelSlot::Embedding) else {
        return Ok(None);
    };
    let settings = route
        .embedding
        .as_ref()
        .context("Missing Switchboard embedding settings")?;
    let executor = Handle::try_current().context("Hermes inference runtime is unavailable")?;
    let target = InferenceTarget::parse(&route.endpoint)?;
    let inference = executor.block_on(EmbeddingInference::open(
        home,
        &target,
        route.model.as_deref(),
        settings.dimensions,
    ))?;
    let resolved = inference.route();
    let metadata = EmbeddingIdentity {
        provider: resolved.provider.clone(),
        model: resolved.model.clone(),
        dimensions: inference.dimensions(),
        normalization: match settings.normalization {
            EmbeddingNormalization::None => "none",
            EmbeddingNormalization::L2 => "l2",
        }
        .into(),
        input_type_handling: "shared".into(),
    };
    let embedding = Arc::new(Embedding {
        inference,
        executor: executor.clone(),
        normalization: settings.normalization,
    });
    let mut completions = BTreeMap::<_, (String, Arc<dyn GeneralEndpoint>)>::new();
    for slot in ModelSlot::ALL {
        if slot == ModelSlot::Embedding {
            continue;
        }
        let Some(route) = config.model(slot) else {
            continue;
        };
        let key = (&route.endpoint, &route.model, &route.reasoning_effort);
        if let Entry::Vacant(binding) = completions.entry(key) {
            let target = InferenceTarget::parse(&route.endpoint)
                .with_context(|| format!("Invalid Hermes selection for {}", slot.object_key()))?;
            let inference = executor
                .block_on(CompletionInference::open(
                    home,
                    chat,
                    &target,
                    route.model.as_deref(),
                    route.reasoning_effort.as_deref(),
                ))
                .with_context(|| {
                    format!(
                        "Could not resolve Hermes inference for {}",
                        slot.object_key()
                    )
                })?;
            let provider = inference.route().provider.clone();
            binding.insert((
                provider,
                Arc::new(Completion {
                    inference,
                    executor: executor.clone(),
                }),
            ));
        }
    }
    let runtime = ConfiguredRuntime::hosted_switchboard(
        &config,
        |slot, route| {
            completions
                .get(&(&route.endpoint, &route.model, &route.reasoning_effort))
                .cloned()
                .ok_or(ConfiguredRuntimeError::Setup {
                    route: slot.object_key(),
                })
        },
        |_, _| Ok((metadata.provider.clone(), embedding.clone())),
    )?;
    Ok(Some((runtime, metadata)))
}

pub(super) fn with_native_perception(
    home: &Path,
    endpoint_id: &str,
    runtime: ConfiguredRuntime,
) -> Result<ConfiguredRuntime> {
    let config = EndpointStore::open(&home.join("providers"))?
        .config(endpoint_id)
        .context("The saved Perception endpoint is unavailable")?;
    config.validate()?;
    let endpoint =
        ConfiguredDecisionEndpoint::native(&config.base_url, &config.model, &config.api_key)?;
    let route = format!("mercury-native-jev-v1\0{endpoint_id}\0{}", config.base_url);
    let identity = blake3::hash(route.as_bytes()).to_hex();
    Ok(runtime.with_native_perception_decision(Arc::new(endpoint), identity.as_str())?)
}
