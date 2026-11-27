//! Explicit read-only model queries against one admitted native conversation.
use super::memory::{EmbeddedMemory, hex};
use anyhow::{Context, Result, ensure};
use hermes_engine::{
    Config,
    memory_capture::{CaptureBinding, contains_inline_data},
    native_memory::{
        MemoryQuery, MemoryRequest, MemorySelection, MemoryTool, SearchTimeBasis, redact_text,
    },
};
use hermes_library::memory_plugin::{MemorySources, MemoryTurn};
use reliquary_memory::{
    ChatRetrieval, HermesCopiedSourceTurn, InspectionItem, InspectionQuery, Instance, MemoryId,
    OwnerId, ProductErrorKind, RetrievalPreview, RetrievalRankedHit, SourceAvailability,
    TemporalBasis,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};
use tokio::sync::Semaphore;

// A timed-out provider can still run within its transport deadline.
static QUERY_SLOTS: Semaphore = Semaphore::const_new(4);
const MEMORY_RESULT_LIMIT: usize = 10;

#[derive(Clone)]
pub(super) struct GatewayMemoryQuery {
    memory: Arc<EmbeddedMemory>,
    sources: Arc<dyn MemorySources>,
    binding: CaptureBinding,
    turn: Arc<MemoryTurn>,
    retrieval: Arc<Mutex<Option<ChatRetrieval>>>,
}

impl GatewayMemoryQuery {
    pub(super) fn new(
        memory: Arc<EmbeddedMemory>,
        sources: Arc<dyn MemorySources>,
        binding: CaptureBinding,
        turn: MemoryTurn,
    ) -> Self {
        Self {
            memory,
            sources,
            binding,
            turn: Arc::new(turn),
            retrieval: Arc::new(Mutex::new(None)),
        }
    }

    fn check_session(&self) -> Result<()> {
        self.memory.check_recall_scope()?;
        self.sources
            .check_session(&self.turn.session_id, &self.binding)
    }

    fn check_disclosure(&self, preview: Option<&RetrievalPreview>) -> Result<()> {
        self.check_session()?;
        if let Some(preview) = preview
            .filter(|preview| !preview.memories.is_empty() || !preview.observations.is_empty())
        {
            let (owner, instance) = self.memory.route(&self.binding)?;
            check_provenance(
                preview,
                owner,
                instance,
                &self.memory,
                self.sources.as_ref(),
                &self.binding.profile_id,
            )?;
        }
        Ok(())
    }
}

impl MemoryQuery for GatewayMemoryQuery {
    fn tools(&self) -> Vec<MemoryTool> {
        let mut tools = vec![
            MemoryTool::Query,
            MemoryTool::Refine,
            MemoryTool::Traverse,
            MemoryTool::ArchiveSearch,
        ];
        if self.retrieval.try_lock().is_ok_and(|state| {
            state
                .as_ref()
                .is_some_and(ChatRetrieval::has_archive_references)
        }) {
            tools.extend([MemoryTool::ArchiveRead, MemoryTool::EchoRead]);
        }
        tools
    }

    fn invoke(&self, request: MemoryRequest, config: &Config) -> Result<MemorySelection> {
        let _permit = QUERY_SLOTS
            .try_acquire()
            .context("Memory query capacity reached")?;
        self.check_session()?;
        let (owner, instance) = self.memory.route(&self.binding)?;
        let mut state = self
            .retrieval
            .lock()
            .map_err(|_| anyhow::anyhow!("Memory turn lock unavailable"))?;
        if state.is_none() {
            let id = format!("hermes-{}-{}", self.turn.session_id, self.turn.admission_id);
            *state = Some(instance.begin_scoped_chat_retrieval(
                Some(owner),
                self.memory.recall.clone(),
                &id,
            )?);
        }
        let retrieval = state.as_mut().context("Memory turn unavailable")?;
        let echo = matches!(&request, MemoryRequest::EchoRead { .. });
        if let MemoryRequest::ArchiveRead { reference } | MemoryRequest::EchoRead { reference } =
            &request
        {
            let rows = retrieval.read_archive_reference(reference)?;
            self.check_archive_rows(&rows)?;
            let content = if echo {
                json!({"reference":reference, "echo":rows.iter().flat_map(|row| &row.echo).collect::<Vec<_>>()})
            } else {
                json!({"reference":reference, "messages":archive_messages(&rows)})
            };
            check_payload(&content, config)?;
            let backend = self.clone();
            let reference = reference.clone();
            return Ok(MemorySelection::Result {
                count: rows.len(),
                content,
                check_disclosure: Box::new(move || {
                    backend.check_session()?;
                    let state = backend
                        .retrieval
                        .lock()
                        .map_err(|_| anyhow::anyhow!("Memory turn lock unavailable"))?;
                    let rows = state
                        .as_ref()
                        .context("Memory turn unavailable")?
                        .read_archive_reference(&reference)?;
                    backend.check_archive_rows(&rows)
                }),
            });
        }
        if let MemoryRequest::ArchiveSearch { query, basis } = &request {
            let basis = match basis {
                SearchTimeBasis::Source => TemporalBasis::Source,
                SearchTimeBasis::Mentioned => TemporalBasis::Mentioned,
            };
            if let Err(error) = reliquary_memory::resolve_archive_temporal_query(
                query,
                basis,
                self.turn.admitted_ns,
                &self.turn.calendar_zone,
            ) {
                return Ok(temporal_required(error.to_string()));
            }
            let runtime = self
                .memory
                .runtime(owner, instance, config)?
                .context("Missing memory model route")?;
            let result = match retrieval.search_chat_archive(
                query,
                basis,
                self.turn.admitted_ns,
                &self.turn.calendar_zone,
                &runtime,
            ) {
                Ok(result) => result,
                Err(error) if error.kind() == ProductErrorKind::TemporalQueryRequired => {
                    return Ok(temporal_required(error.message().to_owned()));
                }
                Err(error) => return Err(error.into()),
            };
            let mut hits = Vec::new();
            let mut references = Vec::new();
            for hit in result.hits {
                let Ok(rows) = retrieval.preview_archive_reference(&hit.reference) else {
                    continue;
                };
                if self.check_archive_rows(&rows).is_err() {
                    continue;
                }
                let messages = archive_messages(&rows);
                let hit_payload =
                    json!({"reference":hit.reference, "score":hit.score, "messages":messages});
                if serde_json::to_vec(&hit_payload)?.len() > 4096 {
                    continue;
                }
                hits.push(hit_payload);
                references.push(hit.reference);
                if hits.len() == 3 {
                    break;
                }
            }
            let content = json!({"temporal_filter":result.temporal_filter,"hits":hits});
            check_payload(&content, config)?;
            let backend = self.clone();
            return Ok(MemorySelection::Result {
                count: references.len(),
                content,
                check_disclosure: Box::new(move || {
                    backend.check_session()?;
                    let mut state = backend
                        .retrieval
                        .lock()
                        .map_err(|_| anyhow::anyhow!("Memory turn lock unavailable"))?;
                    let retrieval = state.as_mut().context("Memory turn unavailable")?;
                    for reference in &references {
                        backend
                            .check_archive_rows(&retrieval.preview_archive_reference(reference)?)?;
                    }
                    retrieval.accept_archive_results(&references)?;
                    Ok(())
                }),
            });
        }
        let runtime = self
            .memory
            .runtime(owner, instance, config)?
            .context("Missing memory model route")?;
        let result = match request {
            MemoryRequest::Query { query } => {
                retrieval.query_memory_limited(&query, &runtime, MEMORY_RESULT_LIMIT)?
            }
            MemoryRequest::Refine {
                query,
                search_id,
                selected_ids,
            } => retrieval.refine_memory(&search_id, &selected_ids, &query, &runtime)?,
            MemoryRequest::Traverse {
                search_id,
                result_id,
                seed_ids,
            } => retrieval.traverse_memory(&search_id, &result_id, &seed_ids, &runtime)?,
            _ => unreachable!("Archive operations return above"),
        };
        self.check_disclosure(result.preview.as_ref())?;
        let hits: Vec<_> = result
            .preview
            .iter()
            .flat_map(|preview| &preview.memories)
            .map(|row| {
                json!({
                    "id": format!("{}:{}", row.owner_id, row.memory_id),
                    "reference": row.reference(),
                    "content": row.content,
                })
            })
            .collect();
        let memories: Vec<_> = result
            .preview
            .iter()
            .flat_map(|preview| &preview.memories)
            .map(|row| row.reference())
            .collect();
        let observations = result
            .preview
            .iter()
            .flat_map(|preview| &preview.observations);
        let citations: Vec<_> = observations.clone().map(|row| row.claim.citation).collect();
        retrieval.validate_scoped_memory_result(&result.result_id, &memories, &citations)?;
        let mut content =
            json!({"memories":hits,"search_id":result.search_id,"result_id":result.result_id});
        if !citations.is_empty() {
            content["observations"] = Value::Array(
                observations
                    .map(|row| {
                        json!({
                            "citation": row.claim.citation,
                            "content": row.claim.proposition,
                            "status": row.claim.status,
                            "time_scope": row.claim.time_scope,
                            "evidence": row.claim.support,
                        })
                    })
                    .collect(),
            );
        }
        if let Some(preview) = &result.preview {
            // Positions refer to the typed payload arrays and preserve one Core ranking.
            content["ranked"] = Value::Array(
                preview
                    .ranked
                    .iter()
                    .map(|hit| match hit {
                        RetrievalRankedHit::Memory { reference } => memories
                            .iter()
                            .position(|row| row == reference)
                            .map(|index| json!({"kind":"memory", "index":index})),
                        RetrievalRankedHit::Observation { citation } => citations
                            .iter()
                            .position(|row| row == citation)
                            .map(|index| json!({"kind":"observation", "index":index})),
                    })
                    .collect::<Option<Vec<_>>>()
                    .context("invalid ranked Memory result")?,
            );
        }
        check_payload(&content, config)?;
        let backend = self.clone();
        Ok(MemorySelection::Result {
            count: memories.len() + citations.len(),
            content,
            check_disclosure: Box::new(move || {
                backend.check_disclosure(result.preview.as_ref())?;
                let mut state = backend
                    .retrieval
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Memory turn lock unavailable"))?;
                state
                    .as_mut()
                    .context("Memory turn unavailable")?
                    .accept_scoped_memory_result(&result.result_id, &memories, &citations)?;
                Ok(())
            }),
        })
    }
}

impl GatewayMemoryQuery {
    fn check_archive_rows(&self, rows: &[HermesCopiedSourceTurn]) -> Result<()> {
        ensure!(!rows.is_empty(), "Archive range is empty");
        for row in rows {
            let source = &row.source;
            ensure!(
                source.profile_id == self.binding.profile_id,
                "Archive belongs to a different profile"
            );
            ensure!(
                !self
                    .memory
                    .source_change_pending(self.sources.as_ref(), &source.session_id)?,
                "Archive source removal is still syncing"
            );
            ensure!(
                !contains_inline_data(&row.archive.content)
                    && redact_text(&row.archive.content) == row.archive.content,
                "Archive contains excluded content"
            );
        }
        Ok(())
    }
}

fn archive_messages(rows: &[HermesCopiedSourceTurn]) -> Vec<Value> {
    rows.iter()
        .map(|row| {
            json!({"role":row.archive.role,"timestamp_ns":row.archive.timestamp_ns,
        "message_id":row.source.message_uid,"content":row.archive.content})
        })
        .collect()
}

fn check_payload(payload: &Value, config: &Config) -> Result<()> {
    let encoded = serde_json::to_string(payload)?;
    ensure!(
        encoded.len() <= 16_384
            && (config.api_key.is_empty() || !encoded.contains(&config.api_key)),
        "Memory result exceeds permitted disclosure"
    );
    Ok(())
}

fn temporal_required(message: String) -> MemorySelection {
    MemorySelection::Rejected {
        code: "temporal_query_required",
        message,
    }
}

fn check_provenance(
    preview: &RetrievalPreview,
    owner: OwnerId,
    instance: &Instance,
    memory: &EmbeddedMemory,
    sources: &dyn MemorySources,
    profile_id: &str,
) -> Result<()> {
    ensure!(
        !preview.truncated
            && preview.memories.len() + preview.observations.len() <= MEMORY_RESULT_LIMIT
            && preview.ranked.len() == preview.memories.len() + preview.observations.len(),
        "invalid or incomplete retrieval selection"
    );
    let management = instance.workspace_management(owner)?;
    let check_memory = |id: MemoryId, display_id: &str, revision: u64, body: &str| -> Result<()> {
        let page = management.inspect(
            &InspectionQuery::Provenance {
                id,
                revision: Some(revision),
            },
            None,
            1,
        )?;
        let [InspectionItem::Provenance(evidence)] = page.items.as_slice() else {
            anyhow::bail!("retrieval evidence unavailable");
        };
        ensure!(
            !page.truncated
                && evidence.memory_id == display_id
                && evidence.body_id == body
                && evidence.revision == revision
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
                        && !memory.source_change_pending(sources, &imported.conversation_id)?,
                    "retrieval source is no longer eligible"
                );
            }
        }
        Ok(())
    };
    let rel_id = owner.to_string();
    let phy_id = memory.phy.to_string();
    for entry in &preview.memories {
        ensure!(
            !entry.truncated
                && (entry.owner_id == rel_id || entry.owner_id == phy_id)
                && !contains_inline_data(&entry.content)
                && redact_text(&entry.content) == entry.content,
            "retrieval contains incomplete or excluded content"
        );
        if entry.owner_id == rel_id
            && (entry.source.episode_id.is_some()
                || entry.source.content_conversation_id.is_some()
                || entry.source.grounding_conversation_id.is_some())
        {
            check_memory(
                memory_id(&entry.memory_id)?,
                &entry.memory_id,
                entry.revision,
                &entry.body_id,
            )?;
        }
    }
    let mut lineage = BTreeSet::new();
    for row in &preview.observations {
        let claim = &row.claim;
        let claim_owner = OwnerId::from_bytes(claim.citation.owner_id).to_string();
        ensure!(
            (claim_owner == rel_id || claim_owner == phy_id)
                && !contains_inline_data(&claim.proposition)
                && redact_text(&claim.proposition) == claim.proposition,
            "Observation is unavailable for this disclosure"
        );
        if claim_owner == rel_id {
            lineage.extend(claim.lineage.iter().copied());
        }
    }
    for evidence in lineage {
        check_memory(
            MemoryId(evidence.memory_id),
            &hex(&evidence.memory_id),
            evidence.revision,
            &hex(&evidence.body_id),
        )?;
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
