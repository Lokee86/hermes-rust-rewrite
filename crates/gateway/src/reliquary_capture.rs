//! Reconcile copied-source receipts from the committed native record.
use super::*;

#[derive(Deserialize)]
struct CommittedRecord {
    #[serde(flatten)]
    history: History,
    authority: Option<Authority>,
}

impl Shared {
    pub(super) fn pending_memory_capture(
        &self,
        session: &LiveSession,
        admission: &Receipt,
        index: usize,
        history: &History,
    ) -> Option<PendingMemoryCapture> {
        self.memory.as_ref()?;
        if !admission.ordinary_chat || admission.turn_index != Some(index) {
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
        Some(capture)
    }

    /// Observe a committed archive or deletion before its gateway reply.
    pub(super) async fn memory_source_terminal(&self, session_id: &str) -> Result<()> {
        let Some(memory) = self.memory.clone() else {
            return Ok(());
        };
        let session_id = session_id.to_owned();
        tokio::task::spawn_blocking(move || memory.terminal_source(&session_id)).await??;
        self.memory_wake.notify_one();
        Ok(())
    }

    pub(super) async fn run_memory_capture(self: Arc<Self>) {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => break,
                _ = self.memory_wake.notified() => {},
                _ = interval.tick() => {},
            }
            let Ok(summaries) = self.library.sessions(None) else {
                continue;
            };
            let memory = self.memory.clone().expect("enrolled memory worker");
            let summaries = match tokio::task::spawn_blocking(move || {
                let _ = memory.reconcile_tombstones(&summaries);
                summaries
            })
            .await
            {
                Ok(summaries) => summaries,
                Err(_) => continue,
            };
            for summary in summaries {
                if self.shutdown.is_cancelled() {
                    break;
                }
                let config = self
                    .sessions
                    .lock()
                    .await
                    .get(&summary.id)
                    .map(|session| lock(&session.config).clone())
                    .unwrap_or_else(|| self.provider.clone());
                let memory = self.memory.clone().expect("enrolled memory worker");
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
                    stage_next(&memory, &summary, &record.history, authority, &config)
                })
                .await;
                let Ok(Ok(Some((marker, outcome)))) = staged else {
                    continue;
                };

                // Re-read the owner after staging. If it is live, the receipt
                // merges into the latest authority under the same mutex as admission.
                let sessions = self.sessions.lock().await;
                if let Some(session) = sessions.get(&summary.id).cloned() {
                    drop(sessions);
                    let _ = tokio::task::spawn_blocking(move || -> Result<()> {
                        let mut view = lock(&session.view);
                        let mut next = view.authority.clone();
                        install_capture_update(&mut next, &marker, outcome)?;
                        next.persist(&session.writer, None)?;
                        view.authority = next;
                        Ok(())
                    })
                    .await;
                } else {
                    // Briefly acquire the native writer while the session map
                    // fences a concurrent open. Root staging has already finished.
                    let Ok((writer, _)) = ConversationWriter::open(&summary.directory) else {
                        continue;
                    };
                    drop(sessions);
                    let _ = tokio::task::spawn_blocking(move || -> Result<()> {
                        let Some(value) = writer.authority() else {
                            return Ok(());
                        };
                        let mut authority: Authority = serde_json::from_value(value)?;
                        install_capture_update(&mut authority, &marker, outcome)?;
                        authority.persist(&writer, None)
                    })
                    .await;
                }
            }
        }
        if let Some(memory) = self.memory.clone() {
            let _ = tokio::task::spawn_blocking(move || memory.stop_processing()).await;
        }
    }
}

fn read_committed(summary: &SessionSummary) -> Result<Option<CommittedRecord>> {
    if summary.archived || Settings::read(&summary.directory)?.memory_binding.is_none() {
        return Ok(None);
    }
    let path = summary.directory.join("conversation.json");
    match File::open(path) {
        Ok(file) => Ok(Some(serde_json::from_reader(file)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn stage_next(
    memory: &reliquary::EmbeddedMemory,
    summary: &SessionSummary,
    history: &History,
    authority: &Authority,
    config: &Config,
) -> Result<Option<(PendingMemoryCapture, CaptureUpdate)>> {
    if summary.archived || !matches!(summary.source, hermes_library::SessionSource::Native) {
        return Ok(None);
    }
    let Some(binding) = Settings::read(&summary.directory)?.memory_binding else {
        return Ok(None);
    };
    let mut previous = None;
    for admission in &authority.receipts {
        let Some(marker) = &admission.capture else {
            continue;
        };
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
                    .then(|| (marker.clone(), CaptureUpdate::Settled)));
            }
            previous = Some(receipt);
        } else {
            let receipt = memory.stage(marker, turn, previous)?;
            return Ok(Some((marker.clone(), CaptureUpdate::Staged(receipt))));
        }
    }
    Ok(None)
}

pub(super) fn copied_message_receipt(
    summary: &SessionSummary,
    binding: &CaptureBinding,
    message_uid: &str,
) -> Result<Option<hermes_engine::memory_capture::CaptureReceipt>> {
    let Some(record) = read_committed(summary)? else {
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

enum CaptureUpdate {
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
        CaptureUpdate::Staged(receipt) => saved.receipt = Some(receipt),
        CaptureUpdate::Settled => saved.settled = true,
    }
    Ok(())
}
