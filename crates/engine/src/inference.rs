//! Profile-owned structured completion and embeddings for stock plugins.

use crate::{
    Config,
    credentials::{EndpointStore, scope},
    provider::Provider,
    settings::ProfileSettings,
};
use anyhow::{Result, ensure};
use serde_json::Value;
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelIdentity {
    pub provider: String,
    pub model: String,
}

pub struct ProfileInference {
    chat: Provider,
    embedding: Provider,
    embedding_url: reqwest::Url,
    chat_route: ModelIdentity,
    embedding_route: ModelIdentity,
    dimensions: u32,
}

impl ProfileInference {
    /// Resolve the active Hermes chat route and the saved profile embedding route.
    pub async fn open(home: &Path, chat: &Config) -> Result<Option<Self>> {
        let Some(selected) = ProfileSettings::load(home)?.memory_embedding else {
            return Ok(None);
        };
        ensure!(
            selected.dimensions > 0,
            "Embedding dimensions must be positive"
        );
        let selected_endpoint =
            EndpointStore::open(&home.join("providers"))?.config(&selected.endpoint_id)?;
        let chat = Provider::resolve_config(chat.clone()).await?;
        let embed = Provider::resolve_config(selected_endpoint).await?;
        let chat_route = ModelIdentity {
            provider: scope(&chat.base_url)?,
            model: chat.model.clone(),
        };
        let embedding_route = ModelIdentity {
            provider: scope(&embed.base_url)?,
            model: embed.model.clone(),
        };
        let chat = Provider::new_with_options(chat, Default::default())?;
        let embedding = Provider::new_with_options(embed, Default::default())?;
        let embedding_url = embedding.embedding_url()?;
        Ok(Some(Self {
            chat,
            embedding,
            embedding_url,
            chat_route,
            embedding_route,
            dimensions: selected.dimensions,
        }))
    }

    pub fn chat_route(&self) -> &ModelIdentity {
        &self.chat_route
    }

    pub fn embedding_route(&self) -> &ModelIdentity {
        &self.embedding_route
    }

    pub fn dimensions(&self) -> u32 {
        self.dimensions
    }

    pub async fn complete_json(
        &self,
        system: &str,
        input: &str,
        name: &str,
        schema: &Value,
    ) -> Result<Value> {
        self.chat.complete_json(system, input, name, schema).await
    }

    pub async fn embed(
        &self,
        inputs: &[String],
        max_response_bytes: usize,
    ) -> Result<Vec<Vec<f32>>> {
        self.embedding
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
