//! Prepare one bounded selection and record use only after the model responds.
use super::*;
use hermes_engine::memory_capture::{TurnRecall, contains_inline_data, redact_text};
use reliquary_memory::{
    HandoffReceipt, HostCancelOutcome, HostCapabilityContract, HostConnection, HostOperation,
    HostOperationStatus, HostQueueGuarantees, HostReconcileStatus, InspectionItem, InspectionQuery,
    Instance, MemoryDeliveryOperation, MemoryDeliveryReceipt, MemoryDeliveryStatus, MemoryId,
    OwnerId, PreparedMemoryDelivery, RetrievalMode, RetrievalPreview, RetrievalSession,
    RuntimeCapability, SourceAvailability, TrustedHostAdapter,
};
use std::{collections::BTreeSet, sync::OnceLock};

const QUERY_BYTES: usize = 8192;
const CONTEXT_BYTES: usize = 16384;
const MAX_MEMORIES: usize = 10;
const PREPARE_TIMEOUT: Duration = Duration::from_secs(4);

impl Shared {
    pub(super) async fn prepare_turn_recall(
        &self,
        session: &LiveSession,
        admission: &Receipt,
        config: &Config,
        cancel: &CancellationToken,
    ) -> Option<(TurnRecall, JoinHandle<()>)> {
        let memory = self.memory.clone()?;
        if !admission.ordinary_chat
            || !admission.attachments.is_empty()
            || admission.text.len() > QUERY_BYTES
            || admission.text.trim().is_empty()
            || contains_inline_data(&admission.text)
            || !hermes_engine::references::parse(&admission.text).is_empty()
            || (!config.api_key.is_empty() && admission.text.contains(&config.api_key))
        {
            return None;
        }
        let binding = lock(&session.settings).memory_binding.clone()?;
        let session_id = lock(&session.summary).id.clone();
        let request = format!("hermes-{session_id}-{}", admission.id);
        let config = config.clone();
        let library = self.library.clone();
        let query = redact_text(&admission.text);
        let permit = self.memory_slots.clone().try_acquire_owned().ok()?;
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            if !library.session(&session_id).is_ok_and(|summary| {
                !summary.archived
                    && summary.bot.is_none()
                    && matches!(summary.source, hermes_library::SessionSource::Native)
            }) {
                return Ok(None);
            }
            PreparedRecall::prepare(memory, library, &binding, &request, &query, &config)
        });
        let prepared = tokio::select! {
            biased;
            _ = cancel.cancelled() => return None,
            result = tokio::time::timeout(PREPARE_TIMEOUT, task) => result.ok()?.ok()?.ok()??,
        };
        let (recall, used) = TurnRecall::new(prepared.context.clone());
        let confirmation = tokio::spawn(async move {
            let delivered = used.await.is_ok();
            let _ = tokio::task::spawn_blocking(move || {
                if delivered {
                    let _ = prepared.confirm();
                }
            })
            .await;
        });
        Some((recall, confirmation))
    }
}

struct PreparedRecall {
    memory: Arc<reliquary::EmbeddedMemory>,
    library: Library,
    instance: Instance,
    session: RetrievalSession,
    prepared: PreparedMemoryDelivery,
    context: String,
    confirmed: bool,
}

impl PreparedRecall {
    fn prepare(
        memory: Arc<reliquary::EmbeddedMemory>,
        library: Library,
        binding: &CaptureBinding,
        turn_id: &str,
        query: &str,
        config: &Config,
    ) -> Result<Option<Self>> {
        let (owner, instance) = memory.route(binding)?;
        let instance = instance.clone();
        let Some(runtime) = memory.runtime(owner, &instance, config)? else {
            return Ok(None);
        };
        let session = instance.begin_retrieval(owner, turn_id, RetrievalMode::Deliver)?;
        let selection = (|| -> Result<_> {
            let result = instance.memory_query(&session, query, &runtime)?;
            let Some(prepared) = result.prepared else {
                return Ok(None);
            };
            let preview = instance.preview_retrieval(&session, &prepared)?;
            check_provenance(&preview, owner, &instance, &memory, &library)?;
            let context = serde_json::to_string(
                &preview
                    .memories
                    .iter()
                    .map(|row| row.content.as_str())
                    .collect::<Vec<_>>(),
            )?;
            ensure!(
                context.len() <= CONTEXT_BYTES
                    && (config.api_key.is_empty() || !context.contains(&config.api_key)),
                "retrieval does not fit the prompt context budget"
            );
            Ok(Some((prepared, context)))
        })();
        match selection {
            Ok(Some((prepared, context))) => Ok(Some(Self {
                memory,
                library,
                instance,
                session,
                prepared,
                context,
                confirmed: false,
            })),
            other => {
                let _ = instance.cancel_retrieval(&session);
                other.map(|_| None)
            }
        }
    }

    fn confirm(mut self) -> Result<()> {
        let preview = self
            .instance
            .preview_retrieval(&self.session, &self.prepared)?;
        check_provenance(
            &preview,
            self.session.owner(),
            &self.instance,
            &self.memory,
            &self.library,
        )?;
        ensure!(
            self.context
                == serde_json::to_string(
                    &preview
                        .memories
                        .iter()
                        .map(|row| row.content.as_str())
                        .collect::<Vec<_>>()
                )?,
            "retrieval changed after model delivery"
        );
        let router = self
            .memory
            .host_admission()
            .connect_runtime_host(Arc::new(ExactUse {
                delivery_id: self.prepared.stable_id().to_owned(),
                operation: OnceLock::new(),
            }));
        self.instance
            .deliver_retrieval(&self.session, &self.prepared, &router)?;
        self.confirmed = true;
        Ok(())
    }
}

impl Drop for PreparedRecall {
    fn drop(&mut self) {
        if !self.confirmed {
            let _ = self.instance.cancel_retrieval(&self.session);
        }
    }
}

fn check_provenance(
    preview: &RetrievalPreview,
    owner: OwnerId,
    instance: &Instance,
    memory: &reliquary::EmbeddedMemory,
    library: &Library,
) -> Result<()> {
    ensure!(
        !preview.truncated
            && !preview.memories.is_empty()
            && preview.memories.len() <= MAX_MEMORIES,
        "invalid or incomplete retrieval selection"
    );
    let management = instance.workspace_management(owner)?;
    for entry in &preview.memories {
        ensure!(
            !entry.truncated
                && !contains_inline_data(&entry.content)
                && redact_text(&entry.content) == entry.content,
            "retrieval contains incomplete or excluded content"
        );
        if entry.source.episode_id.is_none()
            && entry.source.content_conversation_id.is_none()
            && entry.source.grounding_conversation_id.is_none()
        {
            continue;
        }
        let page = management.inspect(
            &InspectionQuery::Provenance {
                id: memory_id(&entry.memory_id)?,
                revision: Some(entry.revision),
            },
            None,
            1,
        )?;
        let [InspectionItem::Provenance(evidence)] = page.items.as_slice() else {
            anyhow::bail!("retrieval evidence unavailable");
        };
        ensure!(
            !page.truncated
                && evidence.memory_id == entry.memory_id
                && evidence.body_id == entry.body_id
                && evidence.revision == entry.revision
                && matches!(
                    evidence.availability,
                    SourceAvailability::Available | SourceAvailability::Direct
                ),
            "retrieval evidence is stale or unavailable"
        );
        for turn in [
            &evidence.source_turn,
            &evidence.content_source,
            &evidence.grounding_source,
        ]
        .into_iter()
        .flatten()
        {
            ensure!(
                matches!(
                    turn.availability,
                    SourceAvailability::Available | SourceAvailability::Direct
                ),
                "retrieval source is stale or unavailable"
            );
            if turn.conversation_id.starts_with("hermes-copy:") {
                let imported = turn
                    .imported
                    .as_ref()
                    .filter(|row| {
                        row.format == "hermes-source-copied-snapshot-v1"
                            && row.message_id == turn.node_id
                    })
                    .context("missing copied-source provenance")?;
                let scope = reliquary_memory::HermesCopyScope::new(
                    memory.profile_id().to_owned(),
                    imported.conversation_id.clone(),
                )?;
                ensure!(
                    scope.conversation_id() == turn.conversation_id
                        && memory.source_available(
                            &imported.conversation_id,
                            &imported.message_id,
                            owner,
                            library
                        )?,
                    "retrieval source is no longer eligible"
                );
            }
        }
    }
    Ok(())
}

fn memory_id(value: &str) -> Result<MemoryId> {
    ensure!(
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid memory ID"
    );
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)?;
    }
    Ok(MemoryId(bytes))
}

/// The prepared manifest is already in the gateway's model request. This
/// short-lived host adapter only confirms a matching delivery of that manifest.
struct ExactUse {
    delivery_id: String,
    operation: OnceLock<MemoryDeliveryOperation>,
}

const DELIVERY: RuntimeCapability = RuntimeCapability::MemoryDelivery;

impl TrustedHostAdapter for ExactUse {
    fn configured_capabilities(&self) -> BTreeSet<RuntimeCapability> {
        [DELIVERY].into()
    }

    fn connect(&self) -> Result<HostConnection, String> {
        Ok(HostConnection {
            capabilities: [(
                DELIVERY,
                HostCapabilityContract::pending_queue(self.queue_guarantees(DELIVERY)),
            )]
            .into(),
        })
    }

    fn queue_guarantees(&self, capability: RuntimeCapability) -> HostQueueGuarantees {
        if capability != DELIVERY {
            return HostQueueGuarantees::default();
        }
        // Acceptance is durable at the enclosing Root commit. There is no
        // separate queue or handoff for this in-process delivery receipt.
        HostQueueGuarantees {
            durable_acceptance: true,
            stable_submission_ids: true,
            linearizable_cancel_dequeue: true,
            queryable_operation_status: true,
            idempotent_handoff: true,
        }
    }

    fn reconcile(&self, _: RuntimeCapability) -> Result<HostReconcileStatus, String> {
        Ok(HostReconcileStatus::NoOutstandingWork)
    }

    fn handoff(&self, _: RuntimeCapability, _: &HostOperation) -> Result<HandoffReceipt, String> {
        Err("memory confirmation has no queue".into())
    }

    fn status(&self, _: RuntimeCapability, _: &str) -> Result<HostOperationStatus, String> {
        Ok(HostOperationStatus::Unknown)
    }

    fn cancel(&self, _: RuntimeCapability, _: &str) -> Result<HostCancelOutcome, String> {
        Ok(HostCancelOutcome::Unknown)
    }

    fn deliver_memory(
        &self,
        operation: &MemoryDeliveryOperation,
    ) -> Result<MemoryDeliveryReceipt, String> {
        let digest = ring::digest::digest(&ring::digest::SHA256, &operation.payload);
        if operation.stable_id != self.delivery_id
            || operation.payload_digest.as_slice() != digest.as_ref()
        {
            return Err("memory confirmation has incorrect delivery evidence".into());
        }
        if self.operation.get_or_init(|| operation.clone()) != operation {
            return Err("memory confirmation has different delivery evidence".into());
        }
        Ok(MemoryDeliveryReceipt {
            stable_id: operation.stable_id.clone(),
            payload_digest: operation.payload_digest,
            confirmed_consumption: true,
        })
    }

    fn memory_delivery_status(&self, stable_id: &str) -> Result<MemoryDeliveryStatus, String> {
        if stable_id != self.delivery_id {
            return Ok(MemoryDeliveryStatus::Unknown);
        }
        Ok(self.operation.get().cloned().map_or(
            MemoryDeliveryStatus::NotFound,
            MemoryDeliveryStatus::Confirmed,
        ))
    }
}
