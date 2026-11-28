//! Profile-scoped access to the native Reliquary Ego APIs.

use super::memory::EmbeddedMemory;
use anyhow::{Context, Result, ensure};
use hermes_engine::{
    memory_capture::CaptureBinding,
    native_ego::{
        EgoAccess, EgoPackage, IdentityReview, PinPriority, PinRequest, PinScope, count_context,
    },
    settings::{MemoryProvider, ProfileSettings},
};
use hermes_library::memory_plugin::MemorySources;
use reliquary_memory::{
    EgoIdentity, EgoIdentityDraft, EgoPin, EgoPinId, EgoPinPatch, EgoPinPriority, EgoScope,
    Instance, OwnerId,
};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::Ordering};

pub(super) fn access(
    memory: Arc<EmbeddedMemory>,
    sources: Arc<dyn MemorySources>,
    session_id: String,
    binding: Option<CaptureBinding>,
) -> Result<Option<Arc<dyn EgoAccess>>> {
    if memory.ego_authority.is_none() {
        return Ok(None);
    }
    let rel = binding
        .as_ref()
        .map(|binding| memory.route(binding).map(|(id, _)| id))
        .transpose()?;
    let access = Access {
        memory,
        sources,
        session_id,
        binding,
        rel,
    };
    Ok(Some(Arc::new(access)))
}

struct Access {
    memory: Arc<EmbeddedMemory>,
    sources: Arc<dyn MemorySources>,
    session_id: String,
    binding: Option<CaptureBinding>,
    rel: Option<OwnerId>,
}

impl Access {
    fn authority(&self) -> Result<&Instance> {
        let settings = ProfileSettings::load(&self.memory.home)?;
        ensure!(
            self.memory.ego_online.load(Ordering::Acquire)
                && settings.memory_ego_enabled
                && settings.memory_provider.resolve(true) == MemoryProvider::Reliquary,
            "Native Ego is no longer active for this profile"
        );
        self.sources.check_ego_session(&self.session_id, self.binding.as_ref())?;
        self.memory.check_active_phy_scope()?;
        self.memory
            .ego_authority
            .as_ref()
            .context("Native Ego authority is unavailable")
    }

    fn scope(&self, scope: PinScope) -> Result<(&Instance, EgoScope, OwnerId)> {
        let authority = self.authority()?;
        Ok(match scope {
            PinScope::Phy => (authority, EgoScope::Phy(self.memory.phy), self.memory.phy),
            PinScope::Rel => {
                let id = self.rel.context("This chat has no mounted REL scope")?;
                let instance = self
                    .memory
                    .instances
                    .get(&id)
                    .context("The REL scope is unavailable")?;
                (instance, EgoScope::Rel(id), id)
            }
        })
    }

    fn rebind(&self) -> Self {
        Self {
            memory: self.memory.clone(),
            sources: self.sources.clone(),
            session_id: self.session_id.clone(),
            binding: self.binding.clone(),
            rel: self.rel,
        }
    }
}

fn active(instance: &Instance, owner: OwnerId) -> Result<(u64, Option<EgoIdentity>)> {
    let (selection, records) = instance.ego_identities(owner)?;
    let revision = selection.as_ref().map_or(0, |selection| selection.revision);
    let active = selection
        .and_then(|selection| selection.id)
        .and_then(|id| records.into_iter().find(|entry| entry.id == id));
    Ok((revision, active))
}

impl EgoAccess for Access {
    fn context(&self, query: &str, budget: usize) -> Result<EgoPackage> {
        let package = self.authority()?.ego_context(
            self.rel,
            self.memory.phy,
            query,
            None,
            budget,
            count_context,
        )?;
        self.authority()?;
        Ok(EgoPackage {
            identity: package.identity,
            standing_context: package.standing_context,
        })
    }

    fn pin(&self, request: PinRequest) -> Result<Value> {
        let scope = match &request {
            PinRequest::ListPins { scope }
            | PinRequest::PinAnchor { scope, .. }
            | PinRequest::ModifyAnchor { scope, .. }
            | PinRequest::DeleteAnchor { scope, .. } => *scope,
        };
        let (instance, target, owner) = self.scope(scope)?;
        Ok(match request {
            PinRequest::ListPins { .. } => {
                let pins = instance
                    .ego_pins(target)?
                    .into_iter()
                    .map(pin_value)
                    .collect::<Vec<_>>();
                self.authority()?;
                json!({"owner": owner, "pins": pins})
            }
            PinRequest::PinAnchor { text, priority, .. } => {
                let priority = priority
                    .map(|value| fixed_priority(value, None))
                    .transpose()?;
                json!({"owner": owner, "pin": pin_value(instance.pin_anchor(target, text, priority)?)})
            }
            PinRequest::ModifyAnchor {
                id,
                revision,
                text,
                priority,
                ..
            } => {
                let id = pin_id(&id)?;
                let pin = instance
                    .ego_pins(target)?
                    .into_iter()
                    .find(|pin| pin.id == id)
                    .context("Pin is unavailable in this owner")?;
                ensure!(pin.revision == revision, "Pin revision changed");
                ensure!(text.is_some() || priority.is_some(), "Specify a Pin change");
                let priority = priority
                    .map(|value| fixed_priority(value, Some(pin.priority)))
                    .transpose()?;
                self.authority()?;
                let updated = if text.is_none() && priority == Some(pin.priority) {
                    pin
                } else {
                    instance.modify_anchor(target, id, revision, EgoPinPatch { text, priority })?
                };
                json!({"owner": owner, "pin": pin_value(updated)})
            }
            PinRequest::DeleteAnchor { id, revision, .. } => {
                instance.delete_anchor(target, pin_id(&id)?, revision)?;
                json!({"owner": owner, "removed": id})
            }
        })
    }

    fn identity(&self) -> Result<Option<String>> {
        let (_, identity) = active(self.authority()?, self.memory.phy)?;
        self.authority()?;
        Ok(identity.map(|identity| identity.text))
    }

    fn prepare_identity(&self, text: String) -> Result<IdentityReview> {
        ensure!(
            !text.trim().is_empty() && text.len() <= 8192,
            "Enter a bounded Identity draft"
        );
        let instance = self.authority()?;
        let owner = self.memory.phy;
        let (selection, identity) = active(instance, owner)?;
        let access = self.rebind();
        if let Some(identity) = identity {
            let prepared = instance.prepare_ego_identity(
                owner,
                EgoIdentityDraft {
                    id: identity.id,
                    revision: identity.revision,
                    name: identity.name,
                    text,
                },
            )?;
            let draft = prepared.draft();
            let review = json!({
                "phy": owner, "id": hex(&draft.id.0), "revision": draft.revision,
                "name": draft.name, "text": draft.text
            });
            Ok(IdentityReview {
                review,
                apply: Box::new(move || {
                    let instance = access.authority()?;
                    let (current_selection, _) = active(instance, owner)?;
                    ensure!(
                        current_selection == selection,
                        "Active Identity changed after review"
                    );
                    access.authority()?.approve_ego_identity(owner, prepared)?;
                    Ok(())
                }),
            })
        } else {
            let name = "Profile Identity".to_owned();
            let review =
                json!({"phy": owner, "expected_selection": selection, "name": name, "text": text});
            Ok(IdentityReview {
                review,
                apply: Box::new(move || {
                    let instance = access.authority()?;
                    let (current, active) = active(instance, owner)?;
                    ensure!(
                        current == selection && active.is_none(),
                        "Active Identity changed after review"
                    );
                    let instance = access.authority()?;
                    let identity = instance.create_ego_identity(owner, name, text)?;
                    instance.select_ego_identity(
                        owner,
                        selection,
                        Some((identity.id, identity.revision)),
                    )?;
                    Ok(())
                }),
            })
        }
    }
}

fn fixed_priority(value: PinPriority, prior: Option<EgoPinPriority>) -> Result<EgoPinPriority> {
    use EgoPinPriority::{High, Low, Normal};
    Ok(match value {
        PinPriority::High => High,
        PinPriority::Normal => Normal,
        PinPriority::Low => Low,
        PinPriority::Raise => match prior.context("Look up a Pin before raising its priority")? {
            Low => Normal,
            Normal | High => High,
        },
        PinPriority::Lower => match prior.context("Look up a Pin before lowering its priority")? {
            High => Normal,
            Normal | Low => Low,
        },
    })
}

fn pin_value(pin: EgoPin) -> Value {
    let priority = match pin.priority {
        EgoPinPriority::High => "high",
        EgoPinPriority::Normal => "normal",
        EgoPinPriority::Low => "low",
    };
    json!({"id": hex(&pin.id.0), "revision": pin.revision, "text": pin.text, "priority": priority})
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn pin_id(value: &str) -> Result<EgoPinId> {
    ensure!(
        value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Invalid Pin ID"
    );
    let mut bytes = [0; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)?;
    }
    Ok(EgoPinId(bytes))
}
