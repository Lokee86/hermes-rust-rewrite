//! Selected Python memory and replay of committed provider handoffs.
use super::*;
use hermes_engine::{
    plugins::{PythonMemory, PythonPlugin},
    settings::{MemoryProvider, ProfileSettings},
};
use std::collections::{BTreeMap, BTreeSet};

use crate::memory_capture::CommittedRecord;
use crate::python_history::{project_session, provider_messages};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(super) struct PythonSelection {
    pub(super) id: String,
    pub(super) revision: u64,
}

impl PythonSelection {
    fn configured(settings: &ProfileSettings) -> Option<Self> {
        let MemoryProvider::Plugin(id) = &settings.memory_provider else {
            return None;
        };
        Some(Self {
            id: id.clone(),
            revision: settings.memory_provider_revision,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum PythonDelivery {
    Pending,
    Dispatching,
    Acknowledged,
    Uncertain,
    Ineligible,
}

impl PythonDelivery {
    fn needs_work(self) -> bool {
        matches!(self, Self::Pending | Self::Dispatching)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TerminalReason {
    Archive,
    Delete,
}

impl TerminalReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Archive => "archive",
            Self::Delete => "delete",
        }
    }
}

/// Keep the source fence after the chat's directory can be removed. Hook
/// acknowledgements are separate from the fence: a disabled provider cannot
/// receive work, but a later reactivation must still find its terminal events.
#[derive(Default, Serialize, Deserialize)]
struct TerminalFence {
    events: BTreeMap<TerminalReason, BTreeMap<String, bool>>,
}

impl TerminalFence {
    fn has_pending(&self) -> bool {
        self.events
            .values()
            .any(|providers| providers.values().any(|done| !done))
    }
}

struct ActiveProvider {
    selection: PythonSelection,
    handle: PythonMemory,
}

#[derive(Default)]
struct ProviderSession {
    active: Option<ActiveProvider>,
}

impl ProviderSession {
    async fn close(&mut self) {
        if let Some(active) = self.active.take() {
            let _ = active
                .handle
                .plugin
                .request(
                    "hermes/memory.close",
                    json!({"session_id":active.handle.session_id}),
                )
                .await;
        }
    }
}

#[derive(Default)]
pub(super) struct PythonSlot {
    sessions: Mutex<HashMap<String, Arc<Mutex<ProviderSession>>>>,
    pending: StdMutex<BTreeSet<String>>,
    terminal: StdMutex<BTreeSet<String>>,
    fence_files: StdMutex<()>,
    provider_failure: StdMutex<Option<(PythonSelection, String)>>,
    delivery_warning: StdMutex<Option<PythonSelection>>,
    recovery: StdMutex<Option<Result<(), String>>>,
    recovered: tokio::sync::Notify,
    task: Mutex<Option<JoinHandle<()>>>,
    wake: tokio::sync::Notify,
}

impl PythonSlot {
    async fn session(&self, id: &str) -> Arc<Mutex<ProviderSession>> {
        self.sessions
            .lock()
            .await
            .entry(id.into())
            .or_default()
            .clone()
    }

    async fn wait_for_recovery(&self, shutdown: &CancellationToken) -> bool {
        loop {
            let ready = self.recovered.notified();
            if let Some(recovery) = lock(&self.recovery).as_ref() {
                return recovery.is_ok();
            }
            tokio::select! {
                _ = ready => {},
                _ = shutdown.cancelled() => return false,
            }
        }
    }
}

struct PythonSource {
    summary: SessionSummary,
    settings: Settings,
    history: History,
    authority: Authority,
    secret: String,
}

impl PythonSource {
    fn protected(&self) -> [&str; 1] {
        [&self.secret]
    }

    fn history_for(&self, selection: &PythonSelection, through: Option<usize>) -> Authority {
        let mut authority = self.authority.clone();
        authority.receipts.retain(|receipt| {
            receipt.python_selection.as_ref() == Some(selection)
                && through.is_none_or(|index| receipt.turn_index.is_some_and(|turn| turn <= index))
        });
        authority
    }

    fn projection(
        &self,
        profile: &str,
        selection: &PythonSelection,
        history: &Authority,
    ) -> Result<Value> {
        let mut snapshot = project_session(
            profile,
            &self.summary,
            &self.settings,
            &self.history,
            history,
            &self.protected(),
        )?;
        snapshot["selection_revision"] = json!(selection.revision);
        snapshot["provider"] = json!(selection.id);
        Ok(snapshot)
    }
}

impl Shared {
    pub(super) async fn python_memory_providers(&self, peer: &Peer) -> RpcResult {
        if !peer.local_native {
            return Err(RpcError::new(
                4403,
                "permission_denied",
                "Memory settings require the private profile client",
            ));
        }
        let settings = ProfileSettings::load(self.library.root()).map_err(RpcError::internal)?;
        let native_available = self.selected_memory().await.native();
        let selected = settings.memory_provider.as_id().into_owned();
        let mut providers = [
            MemoryProvider::Auto,
            MemoryProvider::Notes,
            MemoryProvider::Reliquary,
            MemoryProvider::External,
            MemoryProvider::Off,
        ]
        .into_iter()
        .map(|choice| {
            if choice == MemoryProvider::Reliquary && !native_available {
                json!({"id":choice.as_id(),"label":choice.to_string(),"status":"unavailable",
                    "reason":"Native Reliquary is unavailable for this profile"})
            } else {
                json!({"id":choice.as_id(),"label":choice.to_string(),"status":"available"})
            }
        })
        .collect::<Vec<_>>();

        let failure = lock(&self.python_memory.provider_failure).clone();
        let configured = PythonSelection::configured(&settings);
        let warning = configured.as_ref().is_some_and(|selection| {
            lock(&self.python_memory.delivery_warning).as_ref() == Some(selection)
        });
        let recovery_error = lock(&self.python_memory.recovery)
            .as_ref()
            .and_then(|result| result.as_ref().err())
            .cloned();
        let mut selected_ready = false;
        for plugin in self.plugins.inventory().await {
            let id = MemoryProvider::Plugin(plugin.id).as_id().into_owned();
            if plugin.provider.is_none() && id != selected {
                continue;
            }
            let failed = failure
                .as_ref()
                .filter(|(pin, _)| id == selected && Some(pin) == configured.as_ref());
            let available = plugin.provider.is_some()
                && plugin.issue.is_none()
                && failed.is_none()
                && recovery_error.is_none();
            let label = plugin.provider.unwrap_or(plugin.title);
            // An installed exclusive provider can be chosen before it runs.
            selected_ready |= id == selected && available && plugin.handle.is_some();
            if available {
                let mut option = json!({"id":id,"label":label,"status":"available"});
                if id == selected && warning {
                    option["reason"] =
                        json!("A committed memory handoff has an uncertain outcome.");
                }
                providers.push(option);
            } else {
                let reason = failed
                    .map(|(_, reason)| reason.clone())
                    .or(plugin.issue)
                    .or_else(|| recovery_error.clone())
                    .unwrap_or_else(|| "Plugin did not register a memory provider".into());
                providers
                    .push(json!({"id":id,"label":label,"status":"unavailable","reason":reason}));
            }
        }
        if let MemoryProvider::Plugin(id) = &settings.memory_provider
            && !providers
                .iter()
                .any(|row| row["id"].as_str() == Some(selected.as_str()))
        {
            providers.push(json!({"id":selected,"label":id,"status":"unavailable",
                "reason":"Plugin is not installed or enabled"}));
        }
        let effective = match settings.memory_provider.resolve(native_available) {
            MemoryProvider::Plugin(_) if !selected_ready => None,
            MemoryProvider::Reliquary if !native_available => None,
            MemoryProvider::External if !settings.external_memory_connector => None,
            MemoryProvider::Off => None,
            provider => Some(provider.as_id().into_owned()),
        };
        Ok(json!({"selected":selected,"effective":effective,"providers":providers}))
    }

    pub(super) async fn set_python_memory_provider(
        &self,
        params: &Value,
        peer: &Peer,
    ) -> RpcResult {
        if !peer.local_native {
            return Err(RpcError::new(
                4403,
                "permission_denied",
                "Memory settings require the private profile client",
            ));
        }
        let id = required_string(params, "provider")?;
        let choice =
            MemoryProvider::from_id(id).map_err(|error| RpcError::invalid(error.to_string()))?;
        if let MemoryProvider::Plugin(id) = &choice
            && !self
                .plugins
                .inventory()
                .await
                .iter()
                .any(|plugin| &plugin.id == id && plugin.provider.is_some())
        {
            return Err(RpcError::invalid(
                "Choose an installed Python memory provider",
            ));
        }
        let root = self.library.root().to_owned();
        let value = serde_json::to_value(&choice).map_err(RpcError::internal)?;
        tokio::task::spawn_blocking(move || {
            ProfileSettings::update(
                &root,
                &serde_json::Map::from_iter([("memory_provider".into(), value)]),
            )
        })
        .await
        .map_err(RpcError::internal)?
        .map_err(RpcError::internal)?;
        if choice == MemoryProvider::Reliquary {
            // The choice remains saved if enrollment cannot presently be mounted.
            let _ = self.refresh_memory().await;
        }
        self.python_memory.wake.notify_one();
        self.python_memory_providers(peer).await
    }

    pub(super) fn python_source_is_active(
        &self,
        summary: &SessionSummary,
        settings: &Settings,
    ) -> bool {
        matches!(summary.source, hermes_library::SessionSource::Native)
            && summary.bot.is_none()
            && !summary.archived
            && !summary
                .directory
                .join("backup-source.json")
                .try_exists()
                .unwrap_or(true)
            && matches!(
                settings.source.as_str(),
                "gui" | "desktop" | "cli" | "tui" | "acp"
            )
            && !self.python_source_ended(&summary.id)
    }

    fn configured_python(&self) -> Option<PythonSelection> {
        PythonSelection::configured(&ProfileSettings::load(self.library.root()).ok()?)
    }

    pub(super) fn python_selection_at_admission(&self) -> Option<PythonSelection> {
        let selection = self.configured_python()?;
        // A prompt receipt must not wait for Python worker startup. The
        // configured ID is pinned here, and preparation verifies its handle.
        hermes_engine::plugins::installed(self.library.root())
            .ok()?
            .iter()
            .any(|plugin| {
                plugin.name == selection.id
                    && plugin.enabled
                    && plugin.servers.iter().any(|kind| kind == "python")
            })
            .then_some(selection)
    }

    async fn python_plugin(&self, id: &str) -> Option<PythonPlugin> {
        self.plugins
            .inventory()
            .await
            .into_iter()
            .find(|plugin| plugin.id == id && plugin.provider.is_some() && plugin.issue.is_none())?
            .handle
    }

    pub(super) async fn prepare_python_memory(
        &self,
        session: &LiveSession,
        admission: &Receipt,
    ) -> Option<PythonMemory> {
        let selection = admission.python_selection.as_ref()?;
        let id = lock(&session.summary).id.clone();
        if self.configured_python().as_ref() != Some(selection) || self.python_source_ended(&id) {
            return None;
        }
        if !self.python_memory.wait_for_recovery(&self.shutdown).await {
            return None;
        }
        let lane = self.python_memory.session(&id).await;
        let mut lane = lane.lock().await;
        // The next turn may start immediately after message.complete. Deliver
        // the preceding committed turn before this turn asks for recall.
        if let Err(error) = self.drain_python_session(&id, &mut lane).await {
            eprintln!("Python memory handoff for {id}: {error:#}");
        }
        let source = self.read_python_source(&id).await.ok()??;
        match self.open_python_memory(&source, selection, &mut lane).await {
            Ok(handle) => {
                if handle.is_some() {
                    *lock(&self.python_memory.provider_failure) = None;
                }
                handle
            }
            Err(error) => {
                *lock(&self.python_memory.provider_failure) =
                    Some((selection.clone(), format!("{error:#}")));
                eprintln!("Python memory for {id} is unavailable: {error:#}");
                None
            }
        }
    }

    async fn open_python_memory(
        &self,
        source: &PythonSource,
        selection: &PythonSelection,
        lane: &mut ProviderSession,
    ) -> Result<Option<PythonMemory>> {
        if self.configured_python().as_ref() != Some(selection)
            || !self.python_source_is_active(&source.summary, &source.settings)
            || self.python_terminal_pending(&selection.id)
        {
            return Ok(None);
        }

        if let Some(active) = &lane.active
            && active.selection == *selection
            && self.plugins.is_current(&active.handle.plugin).await
        {
            return Ok(Some(active.handle.clone()));
        }
        lane.close().await;
        let Some(plugin) = self.python_plugin(&selection.id).await else {
            return Ok(None);
        };
        let history = source.history_for(selection, None);
        plugin
            .request(
                "hermes/history",
                source.projection(&self.profile_name, selection, &history)?,
            )
            .await?;
        let id = &source.summary.id;
        let scope = json!({
            "hermes_home": self.library.root(), "profile": self.profile_name,
            "agent_context": "primary", "platform": source.settings.source,
            "agent_workspace": source.settings.cwd, "resumed": !source.history.turns.is_empty(),
            "message_count": source.history.turns.len().saturating_mul(2),
            "session_project_id": source.summary.project_id,
        });
        if self.configured_python().as_ref() != Some(selection) || self.python_source_ended(id) {
            return Ok(None);
        }
        let ready = plugin
            .request(
                "hermes/memory.open",
                json!({"session_id":id, "context":scope}),
            )
            .await?;
        ensure!(
            ready["available"] == true,
            "{}",
            ready["reason"]
                .as_str()
                .unwrap_or("Provider declined initialization")
        );
        let handle = PythonMemory {
            plugin,
            session_id: id.clone(),
            selection_revision: selection.revision,
        };
        lane.active = Some(ActiveProvider {
            selection: selection.clone(),
            handle: handle.clone(),
        });
        Ok(Some(handle))
    }

    pub(super) fn pending_python_delivery(
        &self,
        session: &LiveSession,
        admission: &Receipt,
        index: usize,
        history: &History,
        committed_ns: Option<i64>,
        started: Option<&PythonMemory>,
    ) -> Option<PythonDelivery> {
        let pin = admission.python_selection.as_ref()?;
        let handle = started?;
        let finished = committed_ns?;
        let summary = lock(&session.summary);
        let turn = history.turns.get(index)?;
        let config = lock(&session.config);
        if handle.session_id != summary.id
            || handle.selection_revision != pin.revision
            || self.configured_python().as_ref() != Some(pin)
            || !self.python_source_is_active(&summary, &lock(&session.settings))
            || admission.turn_index != Some(index)
            || admission.admitted_ns <= 0
            || finished < admission.admitted_ns
            || !matches!(turn.status, Status::Complete)
            || !turn.attachments.is_empty()
            || !turn.context.is_empty()
            || (!config.api_key.is_empty() && turn.prompt.contains(&config.api_key))
        {
            return None;
        }
        Some(PythonDelivery::Pending)
    }

    pub(super) fn schedule_python_delivery(&self, id: &str) {
        lock(&self.python_memory.pending).insert(id.to_owned());
        self.python_memory.wake.notify_one();
    }

    async fn read_python_source(&self, id: &str) -> Result<Option<PythonSource>> {
        let Ok(summary) = self.library.session(id) else {
            return Ok(None);
        };
        let secret = self
            .sessions
            .lock()
            .await
            .get(id)
            .map(|live| lock(&live.config).api_key.clone())
            .unwrap_or_else(|| self.provider.api_key.clone());
        let settings = Settings::read(&summary.directory)?;
        let path = summary.directory.join("conversation.json");
        let file = match File::open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let record: CommittedRecord = serde_json::from_reader(file)?;
        Ok(record.authority.map(|authority| PythonSource {
            summary,
            settings,
            history: record.history,
            authority,
            secret,
        }))
    }

    async fn update_python_delivery(
        &self,
        source: &SessionSummary,
        receipt: &Receipt,
        from: PythonDelivery,
        to: PythonDelivery,
    ) -> Result<bool> {
        fn update(
            authority: &mut Authority,
            receipt: &Receipt,
            from: PythonDelivery,
            to: PythonDelivery,
        ) -> bool {
            let Some(saved) = authority.receipts.iter_mut().find(|saved| {
                saved.id == receipt.id
                    && saved.generation == receipt.generation
                    && saved.turn_index == receipt.turn_index
                    && saved.python_selection == receipt.python_selection
                    && saved.python_delivery == Some(from)
            }) else {
                return false;
            };
            saved.python_delivery = Some(to);
            true
        }

        let sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get(&source.id).cloned() {
            drop(sessions);
            let mut view = lock(&session.view);
            let mut next = view.authority.clone();
            if !update(&mut next, receipt, from, to) {
                return Ok(false);
            }
            next.persist(&session.writer, None)?;
            view.authority = next;
        } else {
            let (writer, _) = ConversationWriter::open(&source.directory)?;
            drop(sessions);
            let mut authority: Authority =
                serde_json::from_value(writer.authority().context("Committed authority missing")?)?;
            if !update(&mut authority, receipt, from, to) {
                return Ok(false);
            }
            authority.persist(&writer, None)?;
        }
        Ok(true)
    }

    async fn drain_python_session(&self, id: &str, lane: &mut ProviderSession) -> Result<bool> {
        loop {
            if self.shutdown.is_cancelled() {
                return Ok(false);
            }
            let Some(source) = self.read_python_source(id).await? else {
                return Ok(true);
            };
            let Some(receipt) = source.authority.receipts.iter().find(|receipt| {
                receipt
                    .python_delivery
                    .is_some_and(PythonDelivery::needs_work)
            }) else {
                return Ok(true);
            };
            if receipt.python_delivery == Some(PythonDelivery::Dispatching) {
                // An earlier process may have invoked sync_turn. The generic
                // Python contract contains no durable provider acknowledgement.
                self.update_python_delivery(
                    &source.summary,
                    receipt,
                    PythonDelivery::Dispatching,
                    PythonDelivery::Uncertain,
                )
                .await?;
                if let Some(selection) = &receipt.python_selection {
                    *lock(&self.python_memory.delivery_warning) = Some(selection.clone());
                }
                continue;
            }
            let selection = receipt
                .python_selection
                .as_ref()
                .context("Missing committed Python provider selection")?;
            let index = receipt
                .turn_index
                .context("Missing committed Python memory turn")?;
            if self.configured_python().as_ref() != Some(selection)
                || !self.python_source_is_active(&source.summary, &source.settings)
            {
                self.update_python_delivery(
                    &source.summary,
                    receipt,
                    PythonDelivery::Pending,
                    PythonDelivery::Ineligible,
                )
                .await?;
                continue;
            }
            let history = source.history_for(selection, Some(index));
            let messages = provider_messages(&source.history, &history, index, &source.protected());
            let user_uid = format!("generation-{}-user", receipt.generation);
            let assistant_uid = source
                .history
                .turns
                .get(index)
                .and_then(|turn| {
                    turn.entries
                        .iter()
                        .rposition(|entry| matches!(entry, Entry::Assistant { .. }))
                })
                .map(|entry| format!("generation-{}-assistant-{entry}", receipt.generation));
            let committed_text = |uid: &str| {
                messages
                    .iter()
                    .find(|message| message["message_uid"].as_str() == Some(uid))
                    .and_then(|message| message["content"].as_str())
            };
            let (Some(user), Some(assistant)) = (
                committed_text(&user_uid),
                assistant_uid.as_deref().and_then(committed_text),
            ) else {
                self.update_python_delivery(
                    &source.summary,
                    receipt,
                    PythonDelivery::Pending,
                    PythonDelivery::Ineligible,
                )
                .await?;
                continue;
            };
            let Some(handle) = self.open_python_memory(&source, selection, lane).await? else {
                return Ok(false);
            };
            handle
                .plugin
                .request(
                    "hermes/history",
                    source.projection(&self.profile_name, selection, &history)?,
                )
                .await?;
            if self.configured_python().as_ref() != Some(selection) || self.python_source_ended(id)
            {
                continue;
            }
            if !self.plugins.is_current(&handle.plugin).await {
                lane.close().await;
                return Ok(false);
            }
            if !self
                .update_python_delivery(
                    &source.summary,
                    receipt,
                    PythonDelivery::Pending,
                    PythonDelivery::Dispatching,
                )
                .await?
            {
                continue;
            }
            if self.configured_python().as_ref() != Some(selection) || self.python_source_ended(id)
            {
                self.update_python_delivery(
                    &source.summary,
                    receipt,
                    PythonDelivery::Dispatching,
                    PythonDelivery::Ineligible,
                )
                .await?;
                continue;
            }
            let result = provider_call(
                &handle,
                "sync_turn",
                json!([user, assistant]),
                json!({"session_id":id,"messages":messages}),
            )
            .await;
            let outcome = if result.is_ok() {
                PythonDelivery::Acknowledged
            } else {
                PythonDelivery::Uncertain
            };
            self.update_python_delivery(
                &source.summary,
                receipt,
                PythonDelivery::Dispatching,
                outcome,
            )
            .await?;
            if let Err(error) = result {
                *lock(&self.python_memory.delivery_warning) = Some(selection.clone());
                eprintln!(
                    "Python memory handoff {id}/{} has uncertain delivery: {error:#}",
                    receipt.id
                );
            } else if self.configured_python().as_ref() == Some(selection)
                && !self.python_source_ended(id)
            {
                let _ = provider_call(
                    &handle,
                    "queue_prefetch",
                    json!([user]),
                    json!({"session_id":id}),
                )
                .await;
            }
        }
    }

    pub(super) async fn start_python_memory(self: &Arc<Self>) {
        let owner = self.clone();
        *self.python_memory.task.lock().await = Some(tokio::spawn(async move {
            let recovery = owner.clone();
            let result = tokio::task::spawn_blocking(move || recovery.recover_python_work()).await;
            let ready = matches!(result, Ok(Ok(())));
            *lock(&owner.python_memory.recovery) = Some(if ready {
                Ok(())
            } else {
                eprintln!("Could not recover Python memory tasks: {result:?}");
                Err("Committed Python memory work could not be recovered.".into())
            });
            owner.python_memory.recovered.notify_waiters();
            if ready {
                owner.run_python_memory().await;
            }
        }));
    }

    pub(super) async fn stop_python_memory(&self) {
        if let Some(task) = self.python_memory.task.lock().await.take() {
            let _ = task.await;
        }
        let sessions = std::mem::take(&mut *self.python_memory.sessions.lock().await);
        for session in sessions.into_values() {
            session.lock().await.close().await;
        }
    }

    async fn run_python_memory(&self) {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => return,
                _ = self.python_memory.wake.notified() => {}
                _ = interval.tick() => {}
            }
            // A plugin the user turned off must not keep running until the next turn.
            self.plugins.retire_stale().await;
            let others: Vec<_> = self.other_plugin_profiles.lock().await.values().cloned().collect();
            for runtime in others {
                runtime.retire_stale().await;
            }
            let terminal = lock(&self.python_memory.terminal).clone();
            for id in terminal {
                if self.shutdown.is_cancelled() {
                    return;
                }
                if self.deliver_python_terminal(&id).await.unwrap_or(false) {
                    let _order = lock(&self.python_memory.fence_files);
                    if self
                        .read_python_fence(&id)
                        .is_ok_and(|fence| fence.is_none_or(|fence| !fence.has_pending()))
                    {
                        lock(&self.python_memory.terminal).remove(&id);
                    }
                }
            }
            let pending = std::mem::take(&mut *lock(&self.python_memory.pending));
            for id in pending {
                if self.shutdown.is_cancelled() {
                    return;
                }
                let lane = self.python_memory.session(&id).await;
                let outcome = self
                    .drain_python_session(&id, &mut *lane.lock().await)
                    .await;
                if !matches!(outcome, Ok(true)) {
                    lock(&self.python_memory.pending).insert(id);
                }
            }
            self.prune_python_sessions().await;
        }
    }

    fn recover_python_work(&self) -> Result<()> {
        let summaries = self.library.authority_sessions()?;
        let configured = self.configured_python();
        let existing = summaries
            .iter()
            .map(|summary| summary.id.clone())
            .collect::<BTreeSet<_>>();
        for summary in summaries {
            let path = summary.directory.join("conversation.json");
            let Ok(file) = File::open(path) else {
                continue;
            };
            let Ok(record) = serde_json::from_reader::<_, CommittedRecord>(file) else {
                continue;
            };
            if let Some(selection) = &configured
                && record.authority.as_ref().is_some_and(|authority| {
                    authority.receipts.iter().any(|receipt| {
                        receipt.python_selection.as_ref() == Some(selection)
                            && receipt.python_delivery == Some(PythonDelivery::Uncertain)
                    })
                })
            {
                *lock(&self.python_memory.delivery_warning) = Some(selection.clone());
            }
            if summary.archived
                && record.authority.as_ref().is_some_and(|authority| {
                    authority
                        .receipts
                        .iter()
                        .any(|receipt| receipt.python_selection.is_some())
                })
                && !self.python_source_ended(&summary.id)
            {
                self.record_python_fence(&summary.id, TerminalReason::Archive)?;
            }
            if record.authority.is_some_and(|authority| {
                authority.receipts.iter().any(|receipt| {
                    receipt
                        .python_delivery
                        .is_some_and(PythonDelivery::needs_work)
                })
            }) {
                lock(&self.python_memory.pending).insert(summary.id);
            }
        }
        match fs::read_dir(self.python_fence_dir()) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry?;
                    let file = entry.path();
                    if file.extension().and_then(|value| value.to_str()) == Some("json")
                        && let Some(id) = file.file_stem().and_then(|value| value.to_str())
                        && let Ok(Some(fence)) = self.read_python_fence(id)
                    {
                        if !fence.events.contains_key(&TerminalReason::Delete)
                            && !existing.contains(id)
                        {
                            self.record_python_fence(id, TerminalReason::Delete)?;
                        }
                        if fence.has_pending() {
                            lock(&self.python_memory.terminal).insert(id.to_owned());
                        }
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    async fn prune_python_sessions(&self) {
        let wanted = self.configured_python();
        let live = self
            .sessions
            .lock()
            .await
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        let current = self
            .python_memory
            .sessions
            .lock()
            .await
            .iter()
            .map(|(id, lane)| (id.clone(), lane.clone()))
            .collect::<Vec<_>>();
        for (id, lane) in current {
            let Ok(mut state) = lane.try_lock() else {
                continue;
            };
            if state.active.as_ref().is_some_and(|active| {
                wanted.as_ref() != Some(&active.selection) || !live.contains(&id)
            }) {
                state.close().await;
            }
            if state.active.is_none() {
                let mut sessions = self.python_memory.sessions.lock().await;
                if Arc::strong_count(&lane) == 2
                    && sessions.get(&id).is_some_and(|v| Arc::ptr_eq(v, &lane))
                {
                    sessions.remove(&id);
                }
            }
        }
    }

    fn python_fence_dir(&self) -> PathBuf {
        self.library.root().join("python-memory").join("terminal")
    }

    fn python_fence_path(&self, id: &str) -> Result<PathBuf> {
        ensure!(
            !id.is_empty()
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "Invalid conversation identity"
        );
        Ok(self.python_fence_dir().join(format!("{id}.json")))
    }

    fn read_python_fence(&self, id: &str) -> Result<Option<TerminalFence>> {
        match File::open(self.python_fence_path(id)?) {
            Ok(file) => Ok(Some(serde_json::from_reader(file)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn python_source_ended(&self, id: &str) -> bool {
        self.python_fence_path(id)
            .and_then(|path| Ok(path.try_exists()?))
            .unwrap_or(true)
    }

    fn python_terminal_pending(&self, provider: &str) -> bool {
        let terminal = lock(&self.python_memory.terminal).clone();
        terminal.iter().any(|id| match self.read_python_fence(id) {
            Ok(Some(fence)) => fence
                .events
                .values()
                .any(|providers| providers.get(provider) == Some(&false)),
            Ok(None) => false,
            Err(_) => true,
        })
    }

    fn save_python_fence(&self, id: &str, fence: &TerminalFence) -> Result<()> {
        let dir = self.python_fence_dir();
        fs::create_dir_all(&dir)?;
        let mut file = NamedTempFile::new_in(&dir)?;
        serde_json::to_writer(&mut file, fence)?;
        file.as_file().sync_all()?;
        file.persist(self.python_fence_path(id)?)?;
        #[cfg(unix)]
        File::open(dir)?.sync_all()?;
        Ok(())
    }

    pub(super) async fn fence_python_source(
        self: &Arc<Self>,
        id: &str,
        reason: TerminalReason,
    ) -> Result<()> {
        let owner = self.clone();
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || owner.record_python_fence(&id, reason)).await?
    }

    fn record_python_fence(&self, id: &str, reason: TerminalReason) -> Result<()> {
        // Do not add a fence for arbitrary missing user-supplied session IDs.
        let summary = self.library.session(id).ok();
        let mut providers = BTreeSet::new();
        if let Some(summary) = &summary {
            match File::open(summary.directory.join("conversation.json")) {
                Ok(file) => {
                    let committed: CommittedRecord = serde_json::from_reader(file)?;
                    if let Some(authority) = committed.authority {
                        providers.extend(authority.receipts.into_iter().filter_map(|receipt| {
                            receipt.python_selection.map(|selection| selection.id)
                        }));
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
        }
        let _order = lock(&self.python_memory.fence_files);
        let prior = self.read_python_fence(id)?;
        if summary.is_none() && prior.is_none() {
            return Ok(());
        }
        let mut fence = prior.unwrap_or_default();
        providers.extend(
            fence
                .events
                .values()
                .flat_map(|providers| providers.keys())
                .cloned(),
        );
        let event = fence.events.entry(reason).or_default();
        for provider in providers {
            event.entry(provider).or_insert(false);
        }
        self.save_python_fence(id, &fence)?;
        lock(&self.python_memory.terminal).insert(id.to_owned());
        self.python_memory.wake.notify_one();
        Ok(())
    }

    async fn deliver_python_terminal(&self, id: &str) -> Result<bool> {
        let gate = self.python_memory.session(id).await;
        let mut lane = gate.lock().await;
        let Some(fence) = self.read_python_fence(id)? else {
            return Ok(true);
        };

        let live = lane
            .active
            .as_ref()
            .map(|active| (active.selection.clone(), active.handle.clone()));
        if let Some((selection, handle)) = live {
            let messages = match self.read_python_source(id).await? {
                Some(source) => provider_messages(
                    &source.history,
                    &source.history_for(&selection, None),
                    source.history.turns.len().saturating_sub(1),
                    &source.protected(),
                ),
                None => Vec::new(),
            };
            let _ = provider_call(&handle, "on_session_end", json!([messages]), json!({})).await;
            lane.close().await;
        }

        let mut done = true;
        for (reason, providers) in fence.events {
            for (plugin_id, delivered) in providers {
                if delivered {
                    continue;
                }
                let Some(plugin) = self.python_plugin(&plugin_id).await else {
                    done = false;
                    continue;
                };
                plugin
                    .request("hermes/history", json!({"delete":id}))
                    .await?;
                plugin.request(
                    "hermes/hook",
                    json!({"name":"on_session_delete", "kwargs":{
                        "session_id":id, "profile":self.profile_name, "reason":reason.as_str(), "deleted_ids":[id],
                    }}),
                ).await?;
                let _order = lock(&self.python_memory.fence_files);
                if let Some(mut current) = self.read_python_fence(id)?
                    && let Some(delivered) = current
                        .events
                        .get_mut(&reason)
                        .and_then(|map| map.get_mut(&plugin_id))
                {
                    *delivered = true;
                    self.save_python_fence(id, &current)?;
                }
            }
        }
        Ok(done)
    }
}

async fn provider_call(
    handle: &PythonMemory,
    method: &str,
    args: Value,
    kwargs: Value,
) -> Result<Value> {
    handle
        .plugin
        .request(
            "hermes/memory.call",
            json!({"session_id":handle.session_id,"method":method,"args":args,"kwargs":kwargs}),
        )
        .await
}

#[cfg(test)]
#[path = "python_memory_tests.rs"]
mod tests;
