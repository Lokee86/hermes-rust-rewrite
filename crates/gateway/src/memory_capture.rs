//! Reconcile copied-source receipts from the committed native record.
use super::*;
use hermes_engine::memory_capture::CapturedReasoningSummary;
use hermes_library::{
    SessionSource,
    memory_plugin::{MemorySources, MemoryStore},
};

#[derive(Deserialize)]
pub(super) struct CommittedRecord {
    #[serde(flatten)]
    pub(super) history: History,
    pub(super) authority: Option<Authority>,
}

/// Written in the same native record as the shortened transcript.
#[derive(Clone, Deserialize, Serialize)]
pub(super) struct CommittedRewind {
    request_id: String,
    event_id: String,
    kept_turns: usize,
    generation: u64,
    revision: u64,
    preceding_head: Option<[u8; 32]>,
    last_surviving_message_uid: Option<String>,
    #[serde(default)]
    surviving_capture_generation: u64,
    removed_message_uids: Vec<String>,
    #[serde(default)]
    acknowledged: bool,
    #[serde(default)]
    resulting_head: Option<[u8; 32]>,
}

impl Shared {
    pub(super) async fn rewind_committed(
        &self,
        session: &LiveSession,
        params: &Value,
        peer: &Peer,
    ) -> RpcResult {
        if !peer.local_native {
            return Err(RpcError::new(
                4403,
                "permission_denied",
                "Rewind requires the private profile client",
            ));
        }
        let request = required_string(params, "request_id")?;
        if request.is_empty() || request.len() > 256 {
            return Err(RpcError::invalid("request_id must contain 1-256 bytes"));
        }
        let kept = params["payload"]["keep_turns"]
            .as_u64()
            .and_then(|count| usize::try_from(count).ok())
            .ok_or_else(|| RpcError::invalid("keep_turns is required"))?;
        let _order = self.catalog_edits.lock().await;
        let id = {
            let summary = lock(&session.summary);
            if !matches!(summary.source, SessionSource::Native) {
                return Err(RpcError::unsupported(
                    "Rewind requires a native conversation",
                ));
            }
            summary.id.clone()
        };
        let bound_source = lock(&session.settings).memory_binding.is_some();
        let response = {
            let mut view = lock(&session.view);
            let describe = |change: &CommittedRewind| {
                json!({
                    "session_id":id, "operation":"rewind", "revision":change.revision,
                    "execution_generation":change.generation, "kept_turns":change.kept_turns,
                    "memory_synced":change.acknowledged, "ok":true,
                })
            };
            if let Some(old) = view
                .authority
                .source_rewinds
                .iter()
                .find(|old| old.request_id == request)
            {
                if old.kept_turns != kept {
                    return Err(RpcError::new(
                        4001,
                        "request_conflict",
                        "Rewind request identity was reused",
                    ));
                }
                return Ok(describe(old));
            }
            if params["expected_revision"].as_u64() != Some(view.revision)
                || params["expected_generation"].as_u64() != Some(view.generation)
            {
                return Err(RpcError::fenced(
                    "revision_conflict",
                    "Session changed; resume before rewinding",
                    &view,
                ));
            }
            if view.running
                || view
                    .authority
                    .receipts
                    .iter()
                    .any(|row| row.status != "terminal")
            {
                return Err(RpcError::fenced(
                    "session_busy",
                    "Finish or cancel outstanding turns before rewinding",
                    &view,
                ));
            }
            if view.authority.receipts.iter().any(|row| {
                row.capture
                    .as_ref()
                    .is_some_and(|marker| marker.staging && marker.receipt.is_none())
            }) {
                self.memory.wake.notify_one();
                return Err(RpcError::fenced(
                    "memory_capture_pending",
                    "Memory is staging a copied turn. Retry the rewind after it is recorded.",
                    &view,
                ));
            }
            if kept >= view.history.turns.len() {
                return Err(RpcError::invalid(
                    "Rewind must remove at least one committed turn",
                ));
            }
            let generation = view
                .generation
                .checked_add(1)
                .ok_or_else(|| RpcError::internal("generation overflow"))?;
            let revision = view
                .revision
                .checked_add(1)
                .ok_or_else(|| RpcError::internal("revision overflow"))?;
            let mut authority = view.authority.clone();
            let previous_generation = authority
                .source_rewinds
                .last()
                .map_or(0, |row| row.generation);
            let mut change = CommittedRewind {
                request_id: request.to_owned(),
                event_id: format!("rewind-{}", new_id()),
                kept_turns: kept,
                generation,
                revision,
                preceding_head: None,
                last_surviving_message_uid: None,
                surviving_capture_generation: 0,
                removed_message_uids: Vec::new(),
                acknowledged: !bound_source,
                resulting_head: None,
            };
            for admission in &mut authority.receipts {
                let Some(index) = admission.turn_index else {
                    continue;
                };
                if let Some(marker) = &admission.capture {
                    let original = view.history.turns.get(index).ok_or_else(|| {
                        RpcError::internal("capture points outside committed transcript")
                    })?;
                    let rows = marker.verified_rows(original).map_err(RpcError::internal)?;
                    if let Some(receipt) = &marker.receipt {
                        if admission.generation > previous_generation {
                            change.preceding_head = Some(receipt.source_sha256);
                        }
                        if index < kept {
                            change.last_surviving_message_uid =
                                rows.last().map(|row| row.message_uid.clone());
                            change.surviving_capture_generation = admission.generation;
                        } else {
                            change
                                .removed_message_uids
                                .extend(rows.into_iter().map(|row| row.message_uid));
                        }
                    }
                }
                if index >= kept {
                    // Retain submission identity so an old delivery cannot regenerate a removed turn.
                    admission.turn_index = None;
                    admission.capture = None;
                    admission.text.clear();
                    admission.attachments.clear();
                    admission.outcome = Some("rewound".into());
                    admission.result = Some(json!({"status":"rewound"}));
                }
            }
            authority.generation = generation;
            let response = describe(&change);
            authority.source_rewinds.push(change);
            let mut history = view.history.clone();
            history.turns.truncate(kept);
            authority
                .persist(&session.writer, Some(&history))
                .map_err(RpcError::internal)?;
            view.history = history;
            view.authority = authority;
            view.generation = generation;
            view.revision = revision;
            view.replay.clear();
            view.replay_bytes = 0;
            view.evicted_through = view.sequence;
            response
        };
        self.memory.wake.notify_one();
        self.emit(session, "session.updated", response.clone());
        self.emit(session, "session.info", self.info(session));
        Ok(response)
    }

    pub(super) fn pending_memory_capture(
        &self,
        session: &LiveSession,
        admission: &Receipt,
        index: usize,
        history: &History,
        reasoning_summaries: Vec<CapturedReasoningSummary>,
    ) -> Option<PendingMemoryCapture> {
        if !admission.ordinary_chat
            || admission.turn_index != Some(index)
            || source_ended(&session.dir)
        {
            return None;
        }
        let turn = history.turns.get(index)?;
        if !turn.attachments.is_empty() || !turn.context.is_empty() {
            return None;
        }
        let summary = lock(&session.summary);
        if summary.archived {
            return None;
        }
        let binding = lock(&session.settings).memory_binding.clone()?;
        let committed_ns = chrono::Utc::now()
            .timestamp_nanos_opt()?
            .max(admission.admitted_ns);
        let capture = PendingMemoryCapture::new(
            &binding,
            &summary.id,
            &admission.id,
            admission.generation,
            index,
            admission.admitted_ns,
            committed_ns,
            turn,
        )
        .ok()?
        .with_reasoning_summaries(turn, reasoning_summaries)
        .ok()?;
        let config = lock(&session.config);
        if !config.api_key.is_empty()
            && capture
                .verified_rows(turn)
                .ok()?
                .iter()
                .any(|row| row.content.contains(&config.api_key))
        {
            return None;
        }
        if !config.api_key.is_empty()
            && capture.verified_echo(turn).ok()?.iter().any(|event| {
                [&event.content, &event.name, &event.correlation_id]
                    .iter()
                    .any(|value| value.contains(&config.api_key))
            })
        {
            return None;
        }
        Some(capture)
    }

    pub(super) async fn run_memory_capture(self: Arc<Self>) {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => break,
                _ = self.memory.wake.notified() => {},
                _ = interval.tick(), if self.memory.active() => {},
            }
            let owner = self.clone();
            let summaries = match tokio::task::spawn_blocking(move || -> Result<_> {
                owner.memory.refresh_projects(false)?;
                if !owner.memory.active() {
                    return Ok(Vec::new());
                }
                for id in owner.library.pending_memory_deletions()? {
                    if matches!(
                        owner
                            .memory
                            .with_selected(|memory| memory.terminal_source(&id, &owner.provider)),
                        Some(Ok(true))
                    ) {
                        owner.library.acknowledge_memory_deletion(&id)?;
                    }
                }
                owner.library.list_sessions(&hermes_library::SessionQuery {
                    archive: hermes_library::ArchiveFilter::All,
                    ..Default::default()
                })
            })
            .await
            {
                Ok(Ok(summaries)) => summaries,
                _ => continue,
            };
            for summary in summaries {
                if self.shutdown.is_cancelled() || !self.memory.active() {
                    break;
                }
                let config = self
                    .sessions
                    .lock()
                    .await
                    .get(&summary.id)
                    .map(|session| lock(&session.config).clone())
                    .unwrap_or_else(|| self.provider.clone());
                let owner = self.clone();
                let library = self.library.clone();
                let id = summary.id.clone();
                let staged = tokio::task::spawn_blocking(move || -> Result<_> {
                    let summary = library.session(&id)?;
                    let Some(record) = read_committed(&summary)? else {
                        return Ok(None);
                    };
                    let Some(authority) = record.authority.as_ref() else {
                        return Ok(None);
                    };
                    Ok(owner
                        .memory
                        .with_selected(|memory| {
                            stage_next(
                                memory.as_ref(),
                                &summary,
                                &record.history,
                                authority,
                                &config,
                            )
                        })
                        .transpose()?
                        .flatten())
                })
                .await;
                let Ok(Ok(Some(outcome))) = staged else {
                    continue;
                };

                // Re-read the owner after staging. If it is live, the receipt
                // merges into the latest authority under the same mutex as admission.
                let staging_marked =
                    matches!(&outcome, SourceUpdate::Capture(_, CaptureUpdate::Staging));
                let sessions = self.sessions.lock().await;
                let persisted = if let Some(session) = sessions.get(&summary.id).cloned() {
                    drop(sessions);
                    tokio::task::spawn_blocking(move || -> Result<()> {
                        let mut view = lock(&session.view);
                        let mut next = view.authority.clone();
                        install_source_update(&mut next, outcome)?;
                        next.persist(&session.writer, None)?;
                        view.authority = next;
                        Ok(())
                    })
                    .await
                } else {
                    // Briefly acquire the native writer while the session map
                    // fences a concurrent open. Root staging has already finished.
                    let Ok((writer, _)) = ConversationWriter::open(&summary.directory) else {
                        continue;
                    };
                    drop(sessions);
                    tokio::task::spawn_blocking(move || -> Result<()> {
                        let Some(value) = writer.authority() else {
                            return Ok(());
                        };
                        let mut authority: Authority = serde_json::from_value(value)?;
                        install_source_update(&mut authority, outcome)?;
                        authority.persist(&writer, None)
                    })
                    .await
                };
                if staging_marked && matches!(persisted, Ok(Ok(()))) {
                    self.memory.wake.notify_one();
                }
            }
        }
        let slot = self.memory.clone();
        let _ = tokio::task::spawn_blocking(move || slot.shutdown()).await;
    }
}

pub(super) fn read_committed(summary: &SessionSummary) -> Result<Option<CommittedRecord>> {
    if Settings::read(&summary.directory)?.memory_binding.is_none() {
        return Ok(None);
    }
    let path = summary.directory.join("conversation.json");
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn stage_next(
    memory: &dyn MemoryStore,
    summary: &SessionSummary,
    history: &History,
    authority: &Authority,
    config: &Config,
) -> Result<Option<SourceUpdate>> {
    if !matches!(summary.source, hermes_library::SessionSource::Native) {
        return Ok(None);
    }
    let Some(binding) = Settings::read(&summary.directory)?.memory_binding else {
        return Ok(None);
    };
    if source_ended(&summary.directory) {
        if !release_legacy_archive_fence(memory, &binding, summary, history, authority)? {
            return Ok(None);
        }
        match fs::remove_file(summary.directory.join(ENDED)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let latest_rewind = authority
        .source_rewinds
        .iter()
        .rev()
        .find(|row| row.acknowledged);
    if let Some(rewind) = authority
        .source_rewinds
        .iter()
        .find(|row| !row.acknowledged)
    {
        let resulting_head = match rewind
            .preceding_head
            .or(latest_rewind.and_then(|row| row.resulting_head))
        {
            Some(head) if !rewind.removed_message_uids.is_empty() => Some(memory.rewind_source(
                &binding,
                &summary.id,
                &rewind.event_id,
                head,
                rewind.last_surviving_message_uid.as_deref(),
                config,
            )?),
            head => head,
        };
        return Ok(Some(SourceUpdate::Rewind {
            event_id: rewind.event_id.clone(),
            resulting_head,
        }));
    }
    let mut previous = latest_rewind.and_then(|row| row.resulting_head);
    let surviving_capture_generation =
        latest_rewind.map_or(0, |row| row.surviving_capture_generation);
    for admission in &authority.receipts {
        let Some(marker) = &admission.capture else {
            continue;
        };
        if latest_rewind.is_some() && admission.generation <= surviving_capture_generation {
            continue;
        }
        ensure!(
            marker.binding == binding
                && marker.session_id == summary.id
                && marker.admission_id == admission.id
                && marker.generation == admission.generation
                && Some(marker.turn_index) == admission.turn_index
                && admission.status == "terminal",
            "saved memory capture does not match the native admission"
        );
        let turn = history
            .turns
            .get(marker.turn_index)
            .context("memory capture has no native turn")?;
        marker.verified_rows(turn)?;
        if let Some(receipt) = &marker.receipt {
            if !marker.settled {
                let mut config = config.clone();
                config.model.clone_from(&admission.model);
                return Ok(memory
                    .ensure_processing(&binding, &summary.id, receipt, &config)?
                    .then(|| SourceUpdate::Capture(marker.clone(), CaptureUpdate::Settled)));
            }
            previous = Some(receipt.source_sha256);
        } else {
            if !marker.staging {
                if summary.archived
                    || !release_legacy_archive_fence(memory, &binding, summary, history, authority)?
                {
                    return Ok(None);
                }
                return Ok(Some(SourceUpdate::Capture(
                    marker.clone(),
                    CaptureUpdate::Staging,
                )));
            }
            let receipt = memory.stage(marker, turn, previous)?;
            return Ok(Some(SourceUpdate::Capture(
                marker.clone(),
                CaptureUpdate::Staged(receipt),
            )));
        }
    }
    Ok(None)
}

fn release_legacy_archive_fence(
    memory: &dyn MemoryStore,
    binding: &CaptureBinding,
    summary: &SessionSummary,
    history: &History,
    authority: &Authority,
) -> Result<bool> {
    for capture in authority
        .receipts
        .iter()
        .filter_map(|row| row.capture.as_ref())
    {
        if let Some(receipt) = &capture.receipt {
            let turn = history
                .turns
                .get(capture.turn_index)
                .context("copied turn is unavailable")?;
            let rows = capture.verified_rows(turn)?;
            return memory.release_legacy_archive_fence(
                binding,
                &summary.id,
                rows.first().map(|row| (receipt, row.message_uid.as_str())),
            );
        }
    }
    memory.release_legacy_archive_fence(binding, &summary.id, None)
}

enum SourceUpdate {
    Capture(PendingMemoryCapture, CaptureUpdate),
    Rewind {
        event_id: String,
        resulting_head: Option<[u8; 32]>,
    },
}

fn install_source_update(authority: &mut Authority, update: SourceUpdate) -> Result<()> {
    match update {
        SourceUpdate::Capture(marker, update) => install_capture_update(authority, &marker, update),
        SourceUpdate::Rewind {
            event_id,
            resulting_head,
        } => {
            let pending = authority
                .source_rewinds
                .iter_mut()
                .find(|row| !row.acknowledged)
                .filter(|row| row.event_id == event_id)
                .context("committed rewind changed")?;
            pending.acknowledged = true;
            pending.resulting_head = resulting_head;
            Ok(())
        }
    }
}

enum CaptureUpdate {
    Staging,
    Staged(hermes_engine::memory_capture::CaptureReceipt),
    Settled,
}

fn install_capture_update(
    authority: &mut Authority,
    marker: &PendingMemoryCapture,
    update: CaptureUpdate,
) -> Result<()> {
    let saved = authority
        .receipts
        .iter_mut()
        .find(|admission| admission.id == marker.admission_id)
        .and_then(|admission| admission.capture.as_mut())
        .context("committed memory marker was removed")?;
    ensure!(
        saved == marker,
        "committed memory marker changed during staging"
    );
    match update {
        CaptureUpdate::Staging => saved.staging = true,
        CaptureUpdate::Staged(receipt) => {
            saved.receipt = Some(receipt);
            saved.staging = false;
        }
        CaptureUpdate::Settled => saved.settled = true,
    }
    Ok(())
}

const ENDED: &str = "gateway-memory-ended";

/// Keep the terminal capture fence even when the plugin is disabled.
pub(super) fn source_ended(directory: &Path) -> bool {
    directory.join(ENDED).try_exists().unwrap_or(true)
}

impl Shared {
    pub(super) async fn deliver_memory_deletion(
        self: &Arc<Self>,
        session_id: &str,
    ) -> Result<bool> {
        let shared = self.clone();
        let session_id = session_id.to_owned();
        tokio::task::spawn_blocking(move || -> Result<bool> {
            if !shared
                .library
                .pending_memory_deletions()?
                .contains(&session_id)
            {
                return Ok(true);
            }
            let synced = shared
                .memory
                .with_selected(|memory| memory.terminal_source(&session_id, &shared.provider))
                .transpose()?
                .unwrap_or(false);
            if synced {
                shared.library.acknowledge_memory_deletion(&session_id)?;
            }
            Ok(synced)
        })
        .await?
    }
}

pub(super) struct CaptureSources(pub Library);

fn eligible_source(summary: &SessionSummary, binding: &CaptureBinding) -> bool {
    !source_ended(&summary.directory)
        && summary.bot.is_none()
        && matches!(summary.source, SessionSource::Native)
        && summary.project_id == binding.project_id
}

impl MemorySources for CaptureSources {
    fn check_session(&self, session_id: &str, binding: &CaptureBinding) -> Result<()> {
        use hermes_engine::settings::{MemoryProvider, ProfileSettings};
        let settings = ProfileSettings::load(self.0.root())?;
        ensure!(
            settings.memory_tool
                && settings.memory_provider.resolve(true) == MemoryProvider::Reliquary,
            "Native memory is disabled"
        );
        let summary = self.0.session(session_id)?;
        ensure!(
            !summary.archived
                && eligible_source(&summary, binding)
                && Settings::read(&summary.directory)?.memory_binding.as_ref() == Some(binding),
            "Native session binding is unavailable"
        );
        Ok(())
    }

    fn copied_source_change_pending(&self, session_id: &str) -> Result<bool> {
        if self
            .0
            .pending_memory_deletions()?
            .iter()
            .any(|id| id == session_id)
        {
            return Ok(true);
        }
        let Ok(summary) = self.0.session(session_id) else {
            return Ok(false);
        };
        Ok(read_committed(&summary)?
            .and_then(|record| record.authority)
            .is_some_and(|authority| {
                authority
                    .source_rewinds
                    .iter()
                    .any(|change| !change.acknowledged)
            }))
    }
}
