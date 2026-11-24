//! Profile-owned structured completion and embeddings for stock plugins.

use crate::{
    Config, accounts,
    credentials::{EndpointStore, scope},
    models::GenerationOptions,
    portal,
    provider::Provider,
};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelIdentity {
    pub provider: String,
    pub model: String,
}

/// A Hermes-owned provider and authentication selection for one inference route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InferenceTarget {
    Chat,
    Endpoint(String),
    Account(String),
}

impl InferenceTarget {
    pub fn parse(key: &str) -> Result<Self> {
        if key == "chat" {
            return Ok(Self::Chat);
        }
        if let Some(id) = key.strip_prefix("endpoint:") {
            validate_endpoint_id(id)?;
            return Ok(Self::Endpoint(id.into()));
        }
        if let Some(id) = key.strip_prefix("account:") {
            account_endpoint(id)?;
            return Ok(Self::Account(id.into()));
        }
        anyhow::bail!("Invalid Hermes inference target")
    }

    pub fn key(&self) -> String {
        match self {
            Self::Chat => "chat".into(),
            Self::Endpoint(id) => format!("endpoint:{id}"),
            Self::Account(id) => format!("account:{id}"),
        }
    }

    fn config(&self, home: &Path, chat: Option<&Config>, model: Option<&str>) -> Result<Config> {
        let mut config = match self {
            Self::Chat => chat
                .cloned()
                .context("Select an explicit embedding endpoint")?,
            Self::Endpoint(id) => {
                validate_endpoint_id(id)?;
                EndpointStore::open(&home.join("providers"))?.config(id)?
            }
            Self::Account(id) => {
                let (base_url, default_model) = account_endpoint(id)?;
                Config {
                    base_url: base_url.into(),
                    model: default_model.into(),
                    api_key: String::new(),
                }
            }
        };
        config.base_url = scope(&config.base_url)?;
        config.model = model.unwrap_or(&config.model).trim().to_owned();
        config.validate()?;
        Ok(config)
    }
}

fn validate_endpoint_id(id: &str) -> Result<()> {
    ensure!(
        !id.trim().is_empty() && !id.chars().any(char::is_control),
        "Invalid Hermes endpoint ID"
    );
    Ok(())
}

fn account_endpoint(id: &str) -> Result<(&'static str, &'static str)> {
    if id == "nous" {
        return Ok((portal::INFERENCE, portal::AUTO_MODEL));
    }
    let account = accounts::provider(id)
        .filter(|account| {
            accounts::for_base_url(account.base_url).is_some_and(|managed| managed.id == id)
        })
        .context("Hermes does not provide managed inference for this account")?;
    Ok((account.base_url, account.default_model))
}

struct TaskProvider {
    provider: Provider,
    identity: ModelIdentity,
}

impl TaskProvider {
    async fn open(mut config: Config, options: GenerationOptions) -> Result<Self> {
        // Portal can use separate inference origins and resolve an automatic model.
        // Pin the effective route, keeping its managed credential out of the binding.
        let identity = if portal::is_portal(&config.base_url) && config.api_key.is_empty() {
            let resolved = Provider::resolve_config(config.clone()).await?;
            config.model.clone_from(&resolved.model);
            model_identity(&resolved)?
        } else {
            model_identity(&config)?
        };
        let provider = Provider::new_with_options(config, options)?;
        Ok(Self { provider, identity })
    }

    async fn refreshed(&self) -> Result<Option<Provider>> {
        let provider = self.provider.resolve_for_inference().await?;
        if let Some(provider) = &provider {
            ensure!(
                model_identity(provider.config())? == self.identity,
                "The Hermes provider route changed after it was bound"
            );
        }
        Ok(provider)
    }
}

fn model_identity(config: &Config) -> Result<ModelIdentity> {
    Ok(ModelIdentity {
        provider: scope(&config.base_url)?,
        model: config.model.trim().to_owned(),
    })
}

pub struct CompletionInference(TaskProvider);

impl CompletionInference {
    pub async fn open(
        home: &Path,
        chat: &Config,
        target: &InferenceTarget,
        model: Option<&str>,
        reasoning_effort: Option<&str>,
    ) -> Result<Self> {
        let options = GenerationOptions {
            reasoning_effort: reasoning_effort.map(str::to_owned),
            ..Default::default()
        };
        Self::with_options(home, chat, target, model, options).await
    }

    pub(crate) async fn with_options(
        home: &Path,
        chat: &Config,
        target: &InferenceTarget,
        model: Option<&str>,
        mut options: GenerationOptions,
    ) -> Result<Self> {
        let config = target.config(home, Some(chat), model)?;
        options.reasoning_object =
            reqwest::Url::parse(&config.base_url)?.host_str() == Some("openrouter.ai");
        options.validate()?;
        let route = TaskProvider::open(config, options).await?;
        route.provider.validate_structured_options()?;
        Ok(Self(route))
    }

    pub fn route(&self) -> &ModelIdentity {
        &self.0.identity
    }

    pub async fn complete_json(
        &self,
        system: &str,
        input: &str,
        name: &str,
        schema: &Value,
    ) -> Result<Value> {
        let refreshed = self.0.refreshed().await?;
        refreshed
            .as_ref()
            .unwrap_or(&self.0.provider)
            .complete_json(system, input, name, schema)
            .await
    }
}

pub struct EmbeddingInference {
    task: TaskProvider,
    embedding_url: reqwest::Url,
    dimensions: u32,
}

impl EmbeddingInference {
    pub async fn open(
        home: &Path,
        target: &InferenceTarget,
        model: Option<&str>,
        dimensions: u32,
    ) -> Result<Self> {
        ensure!(dimensions > 0, "Embedding dimensions must be positive");
        let config = target.config(home, None, model)?;
        ensure!(
            !portal::is_portal(&config.base_url) || config.model != portal::AUTO_MODEL,
            "Select an explicit Nous embedding model"
        );
        let task = TaskProvider::open(config, Default::default()).await?;
        let embedding_url = task.provider.embedding_url()?;
        Ok(Self {
            task,
            embedding_url,
            dimensions,
        })
    }

    pub fn route(&self) -> &ModelIdentity {
        &self.task.identity
    }

    pub fn dimensions(&self) -> u32 {
        self.dimensions
    }

    pub async fn embed(
        &self,
        inputs: &[String],
        max_response_bytes: usize,
    ) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(provider) = self.task.refreshed().await? {
            let url = provider.embedding_url()?;
            return provider
                .embed(&url, self.dimensions, inputs, max_response_bytes)
                .await;
        }
        self.task
            .provider
            .embed(
                &self.embedding_url,
                self.dimensions,
                inputs,
                max_response_bytes,
            )
            .await
    }
}

#[cfg(test)]
mod tests;
