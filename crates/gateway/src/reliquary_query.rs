//! Explicit read-only model queries against one admitted native conversation.
use super::{Config, Library, Result, Settings, ensure, reliquary::EmbeddedMemory};
use anyhow::Context;
use hermes_engine::{
    memory_capture::{CaptureBinding, CaptureReceipt, contains_inline_data},
    native_memory::{MemoryHit, MemoryQuery, MemorySelection, redact_text},
    settings::{MemoryProvider, ProfileSettings},
};
use hermes_library::{SessionSource, SessionSummary};
use reliquary_memory::{
    InspectionItem, InspectionQuery, Instance, MemoryId, OwnerId, RetrievalMode, RetrievalPreview,
    SourceAvailability,
};
use std::sync::Arc;
use tokio::sync::Semaphore;

// A timed-out provider can still run within its transport deadline.
static QUERY_SLOTS: Semaphore = Semaphore::const_new(4);

#[derive(Clone)]
pub(super) struct GatewayMemoryQuery {
    memory: Arc<EmbeddedMemory>,
    library: Library,
    binding: CaptureBinding,
    session_id: String,
    admission_id: String,
}

impl GatewayMemoryQuery {
    pub(super) fn new(
        memory: Arc<EmbeddedMemory>,
        library: Library,
        binding: CaptureBinding,
        session_id: String,
        admission_id: String,
    ) -> Self {
        Self {
            memory,
            library,
            binding,
            session_id,
            admission_id,
        }
    }

    fn check_session(&self) -> Result<()> {
        let profile = ProfileSettings::load(self.library.root())?;
        ensure!(
            profile.memory_tool
                && profile.memory_provider.resolve(true) == MemoryProvider::Reliquary,
            "native memory is disabled"
        );
        let summary = self.library.session(&self.session_id)?;
        ensure!(
            !summary.archived
                && !super::profile_memory::source_ended(&summary.directory)
                && summary.bot.is_none()
                && matches!(summary.source, SessionSource::Native)
                && summary.project_id == self.binding.project_id
                && Settings::read(&summary.directory)?.memory_binding.as_ref()
                    == Some(&self.binding),
            "native session binding is unavailable"
        );
        Ok(())
    }

    fn check_disclosure(&self, preview: Option<&RetrievalPreview>) -> Result<()> {
        self.check_session()?;
        if let Some(preview) = preview.filter(|preview| !preview.memories.is_empty()) {
            let (owner, instance) = self.memory.route(&self.binding)?;
            check_provenance(
                preview,
                owner,
                instance,
                &self.memory,
                &self.library,
                &self.binding.profile_id,
            )?;
        }
        Ok(())
    }
}

impl MemoryQuery for GatewayMemoryQuery {
    fn query(&self, query: &str, config: &Config) -> Result<MemorySelection> {
        let _permit = QUERY_SLOTS
            .try_acquire()
            .context("Memory query capacity reached")?;
        self.check_session()?;
        let (owner, instance) = self.memory.route(&self.binding)?;
        let runtime = self
            .memory
            .runtime(owner, instance, config)?
            .context("Missing memory model route")?;
        let request = format!("hermes-{}-{}", self.session_id, self.admission_id);
        let session = instance.begin_retrieval(owner, &request, RetrievalMode::Inspect)?;
        let outcome = instance.memory_query(&session, query, &runtime);
        let _ = instance.cancel_retrieval(&session);
        let preview = outcome?.preview;
        self.check_disclosure(preview.as_ref())?;
        let hits: Vec<_> = preview
            .iter()
            .flat_map(|preview| &preview.memories)
            .map(|row| MemoryHit {
                id: row.memory_id.clone(),
                revision: row.revision,
                content: row.content.clone(),
            })
            .collect();
        let encoded = serde_json::to_string(&hits)?;
        ensure!(
            encoded.len() <= 16_384
                && (config.api_key.is_empty() || !encoded.contains(&config.api_key)),
            "Memory result exceeds the permitted disclosure"
        );
        let gateway = self.clone();
        Ok(MemorySelection {
            hits,
            check_disclosure: Box::new(move || gateway.check_disclosure(preview.as_ref())),
        })
    }
}

fn check_provenance(
    preview: &RetrievalPreview,
    owner: OwnerId,
    instance: &Instance,
    memory: &EmbeddedMemory,
    library: &Library,
    profile_id: &str,
) -> Result<()> {
    ensure!(
        !preview.truncated && !preview.memories.is_empty() && preview.memories.len() <= 10,
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
                    profile_id.to_owned(),
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

pub(super) fn copied_message_receipt(
    summary: &SessionSummary,
    binding: &CaptureBinding,
    message_uid: &str,
) -> Result<Option<CaptureReceipt>> {
    let Some(record) = super::reliquary_capture::read_committed(summary)? else {
        return Ok(None);
    };
    let Some(authority) = record.authority else {
        return Ok(None);
    };
    for admission in &authority.receipts {
        let Some(marker) = &admission.capture else {
            continue;
        };
        let Some(receipt) = &marker.receipt else {
            continue;
        };
        if ![
            format!("generation-{}-user", marker.generation),
            format!("generation-{}-assistant", marker.generation),
        ]
        .iter()
        .any(|id| id == message_uid)
        {
            continue;
        }
        ensure!(
            &marker.binding == binding
                && marker.session_id == summary.id
                && marker.admission_id == admission.id
                && marker.generation == admission.generation
                && Some(marker.turn_index) == admission.turn_index
                && admission.status == "terminal",
            "copied memory does not match its native admission"
        );
        let turn = record
            .history
            .turns
            .get(marker.turn_index)
            .context("memory source turn disappeared")?;
        return Ok(marker
            .verified_rows(turn)?
            .iter()
            .any(|row| row.message_uid == message_uid)
            .then(|| receipt.clone()));
    }
    Ok(None)
}
