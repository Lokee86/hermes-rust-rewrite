//! Profile storage for Reliquary's host-bound model configuration.

use super::memory::regular_file;
use anyhow::{Context, Result, ensure};
use hermes_engine::{
    credentials::EndpointStore, inference::InferenceTarget, settings::ProfileSettings,
};
use std::{fs::OpenOptions, path::Path};

pub use reliquary_memory::{
    EmbeddingNormalization, HostedEmbeddingSettings, HostedModelRoute, HostedSwitchboardConfig,
    ModelSlot,
};

/// Read the profile's Switchboard routes. Old embedding selections are carried
/// forward in memory until the first Switchboard edit.
pub fn switchboard_config(home: &Path) -> Result<HostedSwitchboardConfig> {
    let path = home.join("reliquary.cfg");
    if regular_file(&path)?.is_some() {
        return Ok(HostedSwitchboardConfig::open(&path)?);
    }

    let mut config = HostedSwitchboardConfig::new(path);
    let chat = HostedModelRoute {
        endpoint: InferenceTarget::Chat.key(),
        model: None,
        reasoning_effort: None,
        embedding: None,
    };
    config.set_model(ModelSlot::General, Some(chat.clone()))?;
    // Native import needs an Entity Resolution route, even with no overrides.
    config.set_model(ModelSlot::EntityResolution, Some(chat))?;
    if let Some(legacy) = ProfileSettings::load(home)?.memory_embedding {
        config.set_model(
            ModelSlot::Embedding,
            Some(HostedModelRoute {
                endpoint: InferenceTarget::Endpoint(legacy.endpoint_id).key(),
                model: None,
                reasoning_effort: None,
                embedding: Some(HostedEmbeddingSettings {
                    dimensions: legacy.dimensions,
                    normalization: EmbeddingNormalization::None,
                }),
            }),
        )?;
    }
    Ok(config)
}

/// Update only the edited slots, then atomically save the credential-free CVCFG.
pub fn save_switchboard_config(
    home: &Path,
    edits: &[(ModelSlot, Option<HostedModelRoute>)],
) -> Result<HostedSwitchboardConfig> {
    let lock_path = home.join("reliquary.cfg.lock");
    regular_file(&lock_path)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    lock.lock()
        .context("Could not lock the Reliquary Switchboard settings")?;

    let mut config = switchboard_config(home)?;
    for (slot, route) in edits {
        if let Some(route) = route {
            let target = InferenceTarget::parse(&route.endpoint)?;
            ensure!(
                *slot != ModelSlot::Embedding || matches!(&target, InferenceTarget::Endpoint(_)),
                "Select a saved Hermes endpoint for embeddings"
            );
            if let InferenceTarget::Endpoint(id) = target {
                let saved = EndpointStore::open(&home.join("providers"))?.config(&id)?;
                if *slot == ModelSlot::Embedding {
                    saved.openai_chat_url()?;
                }
            }
        }
        config.set_model(*slot, route.clone())?;
    }
    config.save()?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_engine::Config;
    use serde_json::json;

    #[test]
    fn first_switchboard_edit_keeps_legacy_routes_and_saved_config_takes_precedence() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let initial = switchboard_config(home).unwrap();
        assert!(initial.model(ModelSlot::Embedding).is_none());
        assert_eq!(
            initial.model(ModelSlot::EntityResolution).unwrap().endpoint,
            "chat"
        );

        let endpoint = EndpointStore::open(&home.join("providers"))
            .unwrap()
            .save(
                Some("old-embedding"),
                "Old embedding",
                &Config {
                    base_url: "http://127.0.0.1:3150/v1".into(),
                    model: "old-model".into(),
                    api_key: "existing-hermes-fixture-key".into(),
                },
            )
            .unwrap();
        let settings = json!({"memory_embedding":{"endpoint_id":endpoint.id,"dimensions":8}});
        ProfileSettings::update(home, settings.as_object().unwrap()).unwrap();

        let config = switchboard_config(home).unwrap();
        assert!(!home.join("reliquary.cfg").exists());
        assert_eq!(config.model(ModelSlot::General).unwrap().endpoint, "chat");
        assert_eq!(
            config.model(ModelSlot::EntityResolution).unwrap().endpoint,
            "chat"
        );
        let migrated = config.model(ModelSlot::Embedding).unwrap().clone();
        assert_eq!(migrated.endpoint, "endpoint:old-embedding");
        assert_eq!(migrated.embedding.as_ref().unwrap().dimensions, 8);

        let dream = HostedModelRoute {
            endpoint: "chat".into(),
            model: Some("dream-model".into()),
            reasoning_effort: None,
            embedding: None,
        };
        save_switchboard_config(home, &[(ModelSlot::Dream, Some(dream))]).unwrap();
        let reopened = switchboard_config(home).unwrap();
        assert_eq!(
            reopened.model(ModelSlot::Dream).unwrap().model.as_deref(),
            Some("dream-model")
        );
        assert_eq!(
            reopened.model(ModelSlot::Embedding).unwrap().endpoint,
            migrated.endpoint
        );
        let stored = std::fs::read(home.join("reliquary.cfg")).unwrap();
        assert!(stored.starts_with(b"CVCFG\0\r\n"));
        assert!(!String::from_utf8_lossy(&stored).contains("existing-hermes-fixture-key"));

        save_switchboard_config(home, &[(ModelSlot::Embedding, None)]).unwrap();
        assert!(
            switchboard_config(home)
                .unwrap()
                .model(ModelSlot::Embedding)
                .is_none()
        );
        assert!(
            ProfileSettings::load(home)
                .unwrap()
                .memory_embedding
                .is_some()
        );
    }
}
