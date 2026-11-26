use super::{
    sessions,
    store::{self, Candidate, PendingDelivery, Store},
    types::*,
};
use crate::{Shared, lock};
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    path::Path,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::{
    sync::{Mutex as AsyncMutex, Notify},
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;

pub(crate) const MAX_TEXT: usize = 256 * 1024;
pub(super) const MAX_BATCH: usize = 100;
const MAX_PARTS: usize = 128;
const MAX_FILE: usize = 4 * 1024 * 1024;
const POLL: Duration = Duration::from_millis(100);

pub(crate) fn valid_id(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
}

pub(super) fn valid_advance(advance: Option<&Advance>) -> bool {
    let token = |value: &str| !value.is_empty() && value.len() <= 64 * 1024 && !value.contains('\0');
    advance.is_none_or(|advance| {
        valid_id(&advance.stream)
            && token(&advance.checkpoint)
            && advance.previous.as_deref().is_none_or(token)
    })
}

pub(crate) struct Host {
    store: Arc<Store>,
    slots: AsyncMutex<BTreeMap<String, Managed>>,
}

struct Managed {
    slot: Arc<Slot>,
    stop: CancellationToken,
    task: JoinHandle<()>,
}

struct Slot {
    name: String,
    descriptor: Descriptor,
    state: Mutex<State>,
    changed: Notify,
}

pub(super) struct State {
    pub policy: AccessPolicy,
    pub(super) revision: i64,
    generation: Option<String>,
    identity: Option<String>,
    accepting: bool,
    health: Health,
    gap: bool,
}

/// The only host handle a connected adapter receives. Its generation is
/// fenced on disconnect and it carries no profile administration interface.
#[derive(Clone)]
pub struct Ingress {
    slot: Arc<Slot>,
    pub(super) store: Arc<Store>,
    pub(super) connection: String,
    pub(super) generation: String,
}

impl Ingress {
    pub(super) fn checked<T>(
        &self,
        accepting: bool,
        action: impl FnOnce(&State) -> Result<T, GatewayError>,
    ) -> Result<T, GatewayError> {
        let state = lock(&self.slot.state);
        if state.generation.as_deref() != Some(&self.generation) || (accepting && !state.accepting)
        {
            return Err(GatewayError::StaleConnection);
        }
        action(&state)
    }

    pub(super) fn platform(&self) -> &str {
        &self.slot.descriptor.platform
    }

    pub fn capabilities(&self) -> Capabilities {
        self.slot.descriptor.capabilities
    }

    pub(super) fn notify(&self) {
        self.slot.changed.notify_one();
    }

    pub async fn checkpoint(&self, stream: String) -> Result<Option<String>, GatewayError> {
        if !valid_id(&stream) {
            return Err(GatewayError::Invalid);
        }
        let scope = self.clone();
        blocking(move || {
            scope.checked(true, |_| {
                db(scope.store.checkpoint(&scope.connection, &stream))
            })
        })
        .await
    }

    pub async fn submit(
        &self,
        events: Vec<InboundEvent>,
        advance: Option<Advance>,
    ) -> Result<IntakeResult, GatewayError> {
        if events.len() > MAX_BATCH
            || events.iter().any(|event| !valid_id(&event.id))
            || !valid_advance(advance.as_ref())
            || events.iter().any(|event| matches!(event.body, InboundBody::Wake))
        {
            return Err(GatewayError::Invalid);
        }
        let scope = self.clone();
        let result = blocking(move || {
            scope.checked(true, |state| {
                let mut candidates = Vec::with_capacity(events.len());
                for event in events {
                    let decision = scope.route_event(&state.policy, &event)?;
                    candidates.push(Candidate { event, decision });
                }
                db(scope.store.intake(
                    &scope.connection,
                    state.revision,
                    candidates,
                    advance.as_ref(),
                ))?
                .ok_or(GatewayError::CheckpointChanged)
            })
        })
        .await?;
        self.notify();
        Ok(result)
    }

    pub(super) fn route_event(
        &self,
        policy: &AccessPolicy,
        event: &InboundEvent,
    ) -> Result<Result<store::Route, Disposition>, GatewayError> {
        self.route_candidate(policy, event, false)
    }

    pub(super) fn route_candidate(
        &self,
        policy: &AccessPolicy,
        event: &InboundEvent,
        preparing: bool,
    ) -> Result<Result<store::Route, Disposition>, GatewayError> {
        if !valid_id(&event.room.chat)
            || !valid_id(&event.sender.id)
            || event.room.thread.as_ref().is_some_and(|v| !valid_id(v))
            || event.reply_anchor.as_ref().is_some_and(|v| !valid_id(v))
            || event.sender.roles.len() > 128
            || event.sender.roles.iter().any(|id| !valid_id(id))
        {
            return Ok(Err(Disposition::Invalid));
        }
        let caps = self.capabilities();
        if event.room.thread.is_some() && !caps.threads {
            return Ok(Err(Disposition::Unsupported));
        }
        let Some(room) = policy.room_policy(&event.room) else {
            return Ok(Err(Disposition::Forbidden));
        };
        if !policy.allows(&event.sender) {
            return Ok(Err(Disposition::Forbidden));
        }
        match &event.body {
            InboundBody::Message(message) => {
                if !caps.text_intake {
                    return Ok(Err(Disposition::Unsupported));
                }
                if !(valid_message(message)
                    || preparing && message.text.is_empty() && message.attachments.is_empty())
                {
                    return Ok(Err(Disposition::Invalid));
                }
                if !message.attachments.is_empty() && !caps.attachments {
                    return Ok(Err(Disposition::Unsupported));
                }
                if room.mention_required && !event.mentioned {
                    return Ok(Err(Disposition::Forbidden));
                }
            }
            InboundBody::Control(Control::Respond { .. }) if !caps.interactive_prompts => {
                return Ok(Err(Disposition::Unsupported));
            }
            InboundBody::Control(control) if !valid_control(control) => {
                return Ok(Err(Disposition::Invalid));
            }
            InboundBody::Control(_) => {}
            InboundBody::Command(command) => {
                if !caps.text_intake {
                    return Ok(Err(Disposition::Unsupported));
                }
                if !command.valid() {
                    return Ok(Err(Disposition::Invalid));
                }
            }
            InboundBody::Wake => return Ok(Err(Disposition::Unsupported)),
        }
        let participant = room.per_participant.then_some(event.sender.id.as_str());
        db(store::Route::new(
            &self.connection,
            event.room.clone(),
            participant,
        ))
        .map(Ok)
    }

    pub async fn stage_attachment(
        &self,
        name: String,
        bytes: Vec<u8>,
    ) -> Result<AttachmentRef, GatewayError> {
        if !self.capabilities().attachments {
            return Err(GatewayError::Unsupported);
        }
        if bytes.is_empty()
            || bytes.len() > MAX_FILE
            || !valid_id(&name)
            || name.contains(['/', '\\'])
            || name == "."
            || name == ".."
            || hermes_engine::Attachment::validate_bytes(&name, &bytes).is_err()
        {
            return Err(GatewayError::Invalid);
        }
        let scope = self.clone();
        blocking(move || {
            scope.checked(true, |_| {
                db(scope.store.put_attachment(&scope.connection, name, &bytes))
            })
        })
        .await
    }

    /// Read only a gateway-issued artifact in the current delivery attempt.
    pub async fn reply_artifact(
        &self,
        attempt: &SendAttempt,
        artifact: &AttachmentRef,
    ) -> Result<Vec<u8>, GatewayError> {
        let (scope, attempt, artifact) = (self.clone(), attempt.clone(), artifact.clone());
        blocking(move || {
            scope.checked(false, |_| {
                db(scope.store.reply_artifact(
                    &scope.connection,
                    &scope.generation,
                    &attempt,
                    &artifact,
                ))?
                .ok_or(GatewayError::Forbidden)
            })
        })
        .await
    }

    /// List attempts from this authenticated account that need platform evidence.
    pub async fn uncertain_deliveries(&self) -> Result<Vec<UncertainDelivery>, GatewayError> {
        let scope = self.clone();
        blocking(move || {
            scope.checked(true, |_| {
                db(scope.store.uncertain_deliveries(&scope.connection))
            })
        })
        .await
    }

    /// Resolve an exact uncertain attempt using positive platform evidence.
    /// A retryable result must prove that the platform did not accept the attempt.
    pub async fn reconcile_delivery(
        &self,
        delivery: UncertainDelivery,
        evidence: DeliveryResult,
    ) -> Result<bool, GatewayError> {
        match &evidence {
            DeliveryResult::Confirmed { receipt, .. } if valid_id(receipt) => {}
            DeliveryResult::Retryable { .. } => {}
            _ => return Err(GatewayError::Invalid),
        }
        let scope = self.clone();
        let changed = blocking(move || {
            scope.checked(true, |_| {
                db(scope.store.record(
                    &scope.connection,
                    None,
                    &delivery.delivery_id,
                    (delivery.part, delivery.attempt),
                    evidence,
                    now(),
                ))
            })
        })
        .await?;
        if changed {
            self.notify();
        }
        Ok(changed)
    }

    pub fn report_gap(&self) -> Result<(), GatewayError> {
        let mut state = lock(&self.slot.state);
        if state.generation.as_deref() != Some(&self.generation) || !state.accepting {
            return Err(GatewayError::StaleConnection);
        }
        db(self.store.report_gap(&self.connection))?;
        state.gap = true;
        Ok(())
    }

    fn drain(&self) {
        let mut state = lock(&self.slot.state);
        if state.generation.as_deref() == Some(&self.generation) {
            state.accepting = false;
        }
    }

    fn revoke(&self) {
        let mut state = lock(&self.slot.state);
        if state.generation.as_deref() == Some(&self.generation) {
            state.generation = None;
            state.accepting = false;
        }
    }
}

fn valid_message(message: &Message) -> bool {
    message.text.len() <= MAX_TEXT
        && (!message.text.trim().is_empty() || !message.attachments.is_empty())
        && message.attachments.len() <= 8
        && message
            .attachments
            .iter()
            .all(|file| valid_id(&file.id) && valid_id(&file.name))
}

fn valid_control(control: &Control) -> bool {
    let valid_text = |text: &str| !text.trim().is_empty() && text.len() <= MAX_TEXT;
    match control {
        Control::Respond {
            action_id,
            response,
        } => {
            valid_id(action_id)
                && match response {
                    Response::Clarify(answer) => valid_text(answer),
                    Response::Approve | Response::Deny => true,
                }
        }
        Control::ReplaceQueued { event_id, text } => valid_id(event_id) && valid_text(text),
        Control::Steer { text, .. } | Control::Redirect { text, .. } => valid_text(text),
        Control::Interrupt { .. } => true,
    }
}

pub(super) fn db<T>(result: Result<T>) -> Result<T, GatewayError> {
    result.map_err(|_| GatewayError::TemporarilyUnavailable)
}

pub(super) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, GatewayError> + Send + 'static,
) -> Result<T, GatewayError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| GatewayError::TemporarilyUnavailable)?
}

impl Host {
    pub fn open(root: &Path) -> Result<Self> {
        Ok(Self {
            store: Arc::new(Store::open(root)?),
            slots: AsyncMutex::default(),
        })
    }

    pub fn validate(config: &AdapterConfig) -> Result<Descriptor> {
        let descriptor = config.factory.descriptor();
        ensure!(
            valid_id(&config.name) && valid_id(&descriptor.platform),
            "Invalid messaging adapter identity"
        );
        ensure!(
            descriptor.version == CONTRACT_VERSION,
            "Unsupported gateway adapter contract version"
        );
        ensure!(
            if descriptor.capabilities.wake {
                !descriptor.capabilities.text_intake && !descriptor.capabilities.text_delivery
            } else {
                descriptor.capabilities.text_intake && descriptor.capabilities.text_delivery
            },
            "Gateway adapters require text intake and delivery or wake-only capability"
        );
        Ok(descriptor)
    }

    pub async fn configure(&self, owner: &Arc<Shared>, config: AdapterConfig) -> Result<()> {
        let descriptor = Self::validate(&config)?;
        let mut slots = self.slots.lock().await;
        ensure!(!owner.shutdown.is_cancelled(), "Gateway is shutting down");
        if let Some(previous) = slots.remove(&config.name) {
            previous.stop.cancel();
            let _ = previous.task.await;
        }
        let stop = owner.shutdown.child_token();
        let slot = Arc::new(Slot {
            name: config.name.clone(),
            descriptor,
            state: Mutex::new(State {
                policy: config.policy,
                revision: 0,
                generation: None,
                accepting: false,
                identity: None,
                health: Health::Connecting,
                gap: false,
            }),
            changed: Notify::new(),
        });
        let task = tokio::spawn(supervise(
            Arc::downgrade(owner),
            self.store.clone(),
            owner.library.root().to_owned(),
            slot.clone(),
            config.factory,
            stop.clone(),
        ));
        slots.insert(config.name, Managed { slot, stop, task });
        Ok(())
    }

    pub async fn policy(&self, name: &str, policy: AccessPolicy) -> Result<()> {
        let slots = self.slots.lock().await;
        let slot = &slots
            .get(name)
            .context("No such messaging connection")?
            .slot;
        let mut state = lock(&slot.state);
        state.revision = state
            .revision
            .checked_add(1)
            .context("Messaging policy revision exhausted")?;
        state.policy = policy;
        slot.changed.notify_one();
        Ok(())
    }

    pub async fn stop(&self, name: &str) -> bool {
        let mut slots = self.slots.lock().await;
        let Some(managed) = slots.remove(name) else {
            return false;
        };
        managed.stop.cancel();
        let _ = managed.task.await;
        true
    }

    pub async fn shutdown(&self) {
        let slots = std::mem::take(&mut *self.slots.lock().await);
        for managed in slots.values() {
            managed.stop.cancel();
        }
        for managed in slots.into_values() {
            let _ = managed.task.await;
        }
    }

    pub async fn status(&self, profile: &str) -> Result<serde_json::Value> {
        let connections: Vec<_> = self
            .slots
            .lock()
            .await
            .values()
            .map(|managed| {
                let slot = &managed.slot;
                let state = lock(&slot.state);
                ConnectionStatus {
                    name: slot.name.clone(),
                    platform: slot.descriptor.platform.clone(),
                    identity: state.identity.clone(),
                    health: state.health,
                    gap: state.gap,
                }
            })
            .collect();
        let store = self.store.clone();
        let (input, deliveries) = tokio::task::spawn_blocking(move || -> Result<_> {
            Ok((store.input_status()?, store.delivery_status()?))
        })
        .await??;
        Ok(
            serde_json::json!({"profile_id":profile,"connections":connections,"input":input,"deliveries":deliveries}),
        )
    }

    pub async fn resolve(
        &self,
        connection: String,
        delivery: String,
        part: u32,
        attempt: u32,
        outcome: DeliveryResolution,
    ) -> Result<bool> {
        let store = self.store.clone();
        Ok(tokio::task::spawn_blocking(move || {
            store.resolve(&connection, &delivery, part, attempt, outcome)
        })
        .await??)
    }

    pub async fn bind_conversation(&self, destination: &str, binding: &str) -> Result<()> {
        let destination = self.store.route(destination)?.context("Unknown messaging destination")?;
        let binding = self.store.route(binding)?.context("Unknown messaging conversation")?;
        let slots = self.slots.lock().await;
        let state = slots.values().find_map(|managed| {
            let state = lock(&managed.slot.state);
            (state.accepting && state.identity.as_deref() == Some(&destination.connection)).then_some(state)
        }).context("Messaging connection is unavailable")?;
        ensure!(
            state.policy.room_policy(&destination.room).is_some()
                && state.policy.room_policy(&binding.room).is_some(),
            "Messaging conversation policy changed"
        );
        self.store.share_conversation(&destination, &binding)
    }

    pub async fn set_wake_target(&self, name: &str, target: super::WakeTarget) -> Result<()> {
        let slots = self.slots.lock().await;
        let slot = &slots.get(name).context("Unknown messaging connection")?.slot;
        let state = lock(&slot.state);
        ensure!(slot.descriptor.capabilities.wake && state.accepting, "Wake connection is unavailable");
        let identity = state.identity.as_deref().context("Unverified wake connection")?;
        self.store.set_wake_target(identity, &target)
    }

    pub fn manual_notification(&self, route: &str, request: &str, text: String) -> Result<String> {
        ensure!(valid_id(route) && valid_id(request), "Invalid messaging destination or request identity");
        let route = self.store.route(route)?.context("Unknown messaging destination")?;
        ensure!(!route.blocked, "Messaging destination is blocked");
        let producer = store::key("manual", &(&route.id, request))?;
        self.store.notification(&route, &producer, text)
    }

    pub async fn restore_route(&self, route: String) -> Result<bool> {
        let store = self.store.clone();
        Ok(tokio::task::spawn_blocking(move || store.restore_route(&route)).await??)
    }

    pub(crate) fn authorized_route(&self, id: &str) -> Result<bool> {
        Ok(self.store.route(id)?.is_some_and(|route| !route.blocked))
    }

    pub(crate) fn notification_text(
        &self,
        route: &str,
        producer: &impl serde::Serialize,
    ) -> Result<Option<String>> {
        self.store
            .notification_text(route, &store::key("notification", producer)?)
    }

    /// Called by the authoritative durable producer before it records completion.
    pub(crate) fn notify_route(
        &self,
        id: &str,
        producer: &impl serde::Serialize,
        text: String,
    ) -> Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        // A route might be deleted or blocked after the producer was configured.
        // Preserve the intent for any known route, with delivery held by routing policy.
        let Some(route) = self.store.route(id)? else {
            return Ok(());
        };
        let id = store::key("notification", producer)?;
        self.store.enqueue(
            &route,
            &DeliveryIntent {
                id,
                room: route.room.clone(),
                reply_anchor: None,
                edit_target: None,
                body: DeliveryBody::Reply(Message::text(text)),
            },
        )
    }
}

fn account_lease(root: &Path, account: &str) -> Result<Option<File>> {
    let install = root
        .parent()
        .filter(|dir| dir.file_name().is_some_and(|name| name == "profiles"))
        .and_then(Path::parent)
        .unwrap_or(root);
    let dir = install.join("gateway-adapter-leases");
    fs::create_dir_all(&dir)?;
    ensure!(
        fs::symlink_metadata(&dir)?.is_dir() && dir.canonicalize()? == dir,
        "Invalid adapter lease directory"
    );
    let lease = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(account))?;
    match lease.try_lock() {
        Ok(()) => Ok(Some(lease)),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

async fn supervise(
    owner: Weak<Shared>,
    store: Arc<Store>,
    root: std::path::PathBuf,
    slot: Arc<Slot>,
    factory: Arc<dyn AdapterFactory>,
    stop: CancellationToken,
) {
    let mut wait = Duration::from_millis(250);
    while !stop.is_cancelled() && owner.strong_count() != 0 {
        lock(&slot.state).health = Health::Connecting;
        let child = stop.child_token();
        let verified = tokio::select! {
            biased;
            _ = stop.cancelled() => break,
            result = factory.connect(child.clone()) => result,
        };
        let health = match verified {
            Ok(verified) if valid_id(&verified.account) => {
                let identity = match store::key(
                    "connection",
                    &(&slot.descriptor.platform, &verified.account),
                ) {
                    Ok(identity) => identity,
                    Err(_) => break,
                };
                let lease = account_lease(&root, &identity);
                match lease {
                    Ok(Some(_lease)) => {
                        let Ok(gap) = store
                            .recover_sends(&identity)
                            .and_then(|()| store.gap(&identity))
                        else {
                            lock(&slot.state).health = Health::Backoff;
                            tokio::select! {
                                _ = stop.cancelled() => break,
                                _ = tokio::time::sleep(wait) => continue,
                            }
                        };
                        wait = Duration::from_millis(250);
                        let ingress = Ingress {
                            slot: slot.clone(),
                            store: store.clone(),
                            connection: identity.clone(),
                            generation: crate::new_id(),
                        };
                        {
                            let mut state = lock(&slot.state);
                            state.identity = Some(identity);
                            state.generation = Some(ingress.generation.clone());
                            state.accepting = true;
                            state.health = Health::Ready;
                            state.gap = gap;
                        }
                        let result = drive(
                            owner.clone(),
                            ingress.clone(),
                            verified.adapter,
                            child.clone(),
                        )
                        .await;
                        ingress.revoke();
                        let _ = store.recover_sends(&ingress.connection);
                        failure_health(result)
                    }
                    Ok(None) => Health::AccountInUse,
                    Err(_) => Health::Configuration,
                }
            }
            Ok(_) => Health::Credentials,
            Err(failure) => failure_health(failure),
        };
        child.cancel();
        lock(&slot.state).health = health;
        if matches!(
            health,
            Health::Configuration | Health::Credentials | Health::Stopped
        ) {
            return;
        }
        tokio::select! {
            _ = stop.cancelled() => break,
            _ = tokio::time::sleep(wait) => wait = (wait * 2).min(Duration::from_secs(5)),
        }
    }
    lock(&slot.state).health = Health::Stopped;
}

fn failure_health(failure: ConnectionFailure) -> Health {
    match failure {
        ConnectionFailure::Credentials => Health::Credentials,
        ConnectionFailure::Configuration => Health::Configuration,
        ConnectionFailure::Retryable => Health::Backoff,
        ConnectionFailure::Stopped => Health::Stopped,
    }
}

async fn drive(
    owner: Weak<Shared>,
    scope: Ingress,
    adapter: Arc<dyn Adapter>,
    stop: CancellationToken,
) -> ConnectionFailure {
    let (transport, work) = tokio::join!(
        async {
            let outcome = tokio::select! {
                biased;
                _ = stop.cancelled() => ConnectionFailure::Stopped,
                outcome = adapter.listen(scope.clone(), stop.clone()) => outcome,
            };
            scope.drain();
            stop.cancel();
            outcome
        },
        async {
            let outcome = work_loop(owner, &scope, &adapter, &stop).await;
            scope.drain();
            stop.cancel();
            outcome
        }
    );
    if work.is_err() {
        ConnectionFailure::Retryable
    } else {
        transport
    }
}

async fn work_loop(
    owner: Weak<Shared>,
    scope: &Ingress,
    adapter: &Arc<dyn Adapter>,
    stop: &CancellationToken,
) -> Result<(), GatewayError> {
    let mut tick = tokio::time::interval(POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut sends = JoinSet::new();
    let mut result = loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => break Ok(()),
            Some(sent) = sends.join_next(), if !sends.is_empty() => {
                if let Err(error) = sent.unwrap_or(Err(GatewayError::TemporarilyUnavailable)) { break Err(error); }
            }
            _ = scope.slot.changed.notified() => {}
            _ = tick.tick() => {}
        }
        let Some(shared) = owner.upgrade() else {
            break Ok(());
        };
        if let Err(error) = sessions::advance(&shared, scope, stop).await {
            break if error == GatewayError::StaleConnection {
                Ok(())
            } else {
                Err(error)
            };
        }
        if let Err(error) = deliver(scope, adapter, stop, &mut sends).await {
            break if error == GatewayError::StaleConnection {
                Ok(())
            } else {
                Err(error)
            };
        }
    };
    scope.drain();
    stop.cancel();
    while let Some(sent) = sends.join_next().await {
        result = result.and(sent.unwrap_or(Err(GatewayError::TemporarilyUnavailable)));
    }
    result
}

async fn deliver(
    scope: &Ingress,
    adapter: &Arc<dyn Adapter>,
    stop: &CancellationToken,
    sends: &mut JoinSet<Result<(), GatewayError>>,
) -> Result<(), GatewayError> {
    let stored = scope.clone();
    let ready =
        blocking(move || db(stored.store.ready_deliveries(&stored.connection, now()))).await?;
    for mut pending in ready {
        if stop.is_cancelled() {
            return Ok(());
        }
        if !scope.checked(true, |state| delivery_authorized(scope, state, &pending))? {
            continue;
        }
        if !pending.planned {
            if scope.capabilities().editing && matches!(pending.intent.body, DeliveryBody::Reply(_))
            {
                pending.intent.edit_target = db(scope
                    .store
                    .editable_progress(&pending.route, &pending.intent.id))?;
            }
            match adapter.prepare(&pending.intent) {
                Ok(parts) if valid_plan(scope.capabilities(), &pending.intent, &parts) => {
                    db(scope
                        .store
                        .plan(&scope.connection, &pending.intent.id, &parts))?;
                }
                failure => {
                    let reason = match failure {
                        Err(PreparationFailure::Unsupported) => "unsupported_delivery",
                        Err(PreparationFailure::Rejected) => "delivery_rejected",
                        Ok(_) => "invalid_delivery_plan",
                    };
                    db(scope
                        .store
                        .reject(&scope.connection, &pending.intent.id, reason))?;
                    continue;
                }
            }
        }
        let attempt = scope.checked(true, |state| {
            if !delivery_authorized(scope, state, &pending)? {
                return Ok(None);
            }
            db(scope
                .store
                .claim(&scope.connection, &scope.generation, &pending, now()))
        })?;
        let Some(attempt) = attempt else { continue };
        let scope = scope.clone();
        let adapter = adapter.clone();
        let cancel = stop.child_token();
        sends.spawn(async move {
            let authorized = scope.checked(false, |state| {
                Ok(state.accepting && delivery_authorized(&scope, state, &pending)?)
            })?;
            if cancel.is_cancelled() || !authorized {
                return scope.checked(false, |_| {
                    db(scope
                        .store
                        .release(&scope.connection, &scope.generation, &attempt))
                });
            }
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => DeliveryResult::Uncertain,
                result = adapter.send(attempt.clone(), scope.clone(), cancel.clone()) => result,
            };
            let stored = scope.clone();
            blocking(move || {
                stored.checked(false, |_| {
                    db(stored.store.record(
                        &stored.connection,
                        Some(&stored.generation),
                        &attempt.delivery_id,
                        (attempt.part, attempt.attempt),
                        result,
                        now(),
                    ))
                })
            })
            .await?;
            scope.notify();
            Ok(())
        });
    }
    Ok(())
}

fn delivery_authorized(
    scope: &Ingress,
    state: &State,
    pending: &PendingDelivery,
) -> Result<bool, GatewayError> {
    let Some(policy) = state.policy.room_policy(&pending.intent.room) else {
        return Ok(false);
    };
    let Some(route) = db(scope.store.route(&pending.route))? else {
        return Ok(false);
    };
    if route.blocked || route.connection != scope.connection || route.room != pending.intent.room
        || !db(scope.store.result_released(&scope.connection, &pending.intent.id))?
    {
        return Ok(false);
    }
    if let Some(sender) = &pending.sender {
        let participant = policy.per_participant.then_some(sender.id.as_str());
        let current = db(store::Route::new(
            &scope.connection,
            route.room,
            participant,
        ))?;
        return Ok(state.policy.allows(sender) && current.id == route.id);
    }
    db(scope.store.allows_notification(&route.id, now()))
}

fn valid_plan(caps: Capabilities, intent: &DeliveryIntent, plan: &[DeliveryPart]) -> bool {
    if plan.is_empty() || plan.len() > MAX_PARTS || intent.room.thread.is_some() && !caps.threads {
        return false;
    }
    plan.iter().enumerate().all(|(index, part)| {
        if part.room != intent.room
            || part.edit_target.is_some()
                && (index != 0 || !caps.editing || part.edit_target != intent.edit_target)
        {
            return false;
        }
        match (&part.body, &intent.body) {
            (DeliveryBody::Reply(part), DeliveryBody::Reply(original)) => {
                valid_message(part)
                    && (part.attachments.is_empty() || caps.attachments)
                    && part
                        .attachments
                        .iter()
                        .all(|file| original.attachments.contains(file))
            }
            (DeliveryBody::Prompt { .. }, DeliveryBody::Prompt { .. }) => {
                caps.interactive_prompts && plan.len() == 1 && part.body == intent.body
            }
            (DeliveryBody::Progress { .. }, DeliveryBody::Progress { .. }) => {
                caps.progress && plan.len() == 1 && part.body == intent.body
            }
            _ => false,
        }
    })
}

fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
