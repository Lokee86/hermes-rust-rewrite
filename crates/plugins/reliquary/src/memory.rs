//! Capture, processing and retrieval against a mounted Reliquary profile.
use crate::storage::{Enrollment, Topology};
use anyhow::{Context, Result, ensure};
use hermes_engine::{
    Config, Turn,
    memory_capture::{CaptureBinding, CaptureReceipt, CapturedEchoKind, PendingMemoryCapture},
    native_memory::MemoryQuery,
    settings::ProfileSettings,
};
use hermes_library::{
    Project, SessionSource, SessionSummary,
    memory_plugin::{MemorySources, MemoryStore, MemoryTurn, UserArchiveHit},
};
use reliquary_memory::{
    Gateway, HermesCopiedEcho, HermesCopiedEchoKind, HermesCopiedMessage, HermesCopiedRole,
    HermesCopiedSnapshot, HermesCopiedToolOutcome, HermesCopyScope, HostCredentialBinding,
    Instance, NativeImportPlan, NativeImportState, OwnerGrant, OwnerId, OwnerKind,
    OwnerProcessingJob, Phylactery, ProcessOptions, Rel, TrustedHostAdmission,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{ErrorKind, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use uuid::Uuid;

pub(super) struct EmbeddedMemory {
    profile_id: String,
    principal_id: String,
    cutoff: f64,
    general: OwnerId,
    phy: OwnerId,
    projects: BTreeMap<String, OwnerId>,
    instances: BTreeMap<OwnerId, Instance>,
    gateway: Gateway,
    _managers: Vec<Instance>,
    home: PathBuf,
    directory: PathBuf,
    managed_projects: Option<Vec<Project>>,
    jobs: Mutex<BTreeMap<OwnerId, OwnerProcessingJob>>,
    embeddings: Mutex<()>,
    source_index: Mutex<()>,
    // The stock host takes the same lease. Drop after dropping the instances.
    _lease: File,
}

pub(super) fn regular_file(path: &Path) -> Result<Option<File>> {
    match path.symlink_metadata() {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
        Ok(meta) => {
            ensure!(
                meta.is_file() && !meta.is_symlink(),
                "expected a regular Reliquary state file"
            );
            Ok(Some(File::open(path)?))
        }
    }
}

pub(super) fn read_state<T: DeserializeOwned>(file: File) -> Result<T> {
    const MAX_BYTES: u64 = 1024 * 1024;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "oversized Reliquary state file"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

impl EmbeddedMemory {
    pub(super) fn mount(
        home: PathBuf,
        directory: PathBuf,
        enrollment: Enrollment,
        topology: Topology,
        managed_projects: Option<Vec<Project>>,
        lease: File,
    ) -> Result<Self> {
        enrollment.validate()?;
        HermesCopyScope::new(topology.profile_id.clone(), "profile-check".into())?;
        let gateway = Gateway::new();
        let owner_path = |id: OwnerId, kind: OwnerKind| -> Result<PathBuf> {
            let owner = topology
                .owners
                .get(&id)
                .context("missing registered owner")?;
            ensure!(owner.kind == kind, "invalid registered owner");
            topology.owner_path(&directory, owner)
        };
        let phy = gateway.mount_phylactery(
            &Phylactery::open(owner_path(topology.active_phy, OwnerKind::Phylactery)?)?,
            false,
        )?;
        ensure!(phy == topology.active_phy, "private owner identity changed");
        let projects: BTreeMap<_, _> = topology
            .projects
            .iter()
            .map(|(id, v)| (id.clone(), v.rel_id))
            .collect();
        let mut instances = BTreeMap::new();
        let mut credentials = Vec::new();
        let mut tokens = BTreeMap::new();
        for id in std::iter::once(topology.main_rel).chain(projects.values().copied()) {
            if tokens.contains_key(&id) {
                continue;
            }
            let mounted = gateway.mount_rel(&Rel::open(owner_path(id, OwnerKind::Rel)?)?, true)?;
            ensure!(mounted == id, "project owner identity changed");
            let token = Uuid::new_v4().simple().to_string();
            let manager = Uuid::new_v4().simple().to_string();
            credentials.extend([
                HostCredentialBinding::new(
                    &topology.principal_id,
                    &token,
                    [OwnerGrant::OperateRel(id), OwnerGrant::ViewPhylactery(phy)],
                ),
                HostCredentialBinding::new(
                    &topology.principal_id,
                    &manager,
                    [OwnerGrant::ManageRel(id)],
                ),
            ]);
            tokens.insert(id, (token, manager));
        }
        let (adapter, _) = TrustedHostAdmission::new(credentials).install(&gateway)?;
        let mut managers = Vec::with_capacity(tokens.len());
        for (id, (token, manager)) in tokens {
            instances.insert(id, adapter.authenticate(&token)?);
            managers.push(adapter.authenticate(&manager)?);
        }
        Ok(Self {
            profile_id: topology.profile_id,
            principal_id: topology.principal_id,
            cutoff: enrollment.cutoff,
            general: topology.main_rel,
            phy,
            projects,
            instances,
            gateway,
            _managers: managers,
            home,
            directory,
            managed_projects,
            jobs: Mutex::new(BTreeMap::new()),
            embeddings: Mutex::new(()),
            source_index: Mutex::new(()),
            _lease: lease,
        })
    }

    pub(super) fn route(&self, binding: &CaptureBinding) -> Result<(OwnerId, &Instance)> {
        ensure!(
            binding.principal_id == self.principal_id
                && binding.profile_id == self.profile_id
                && binding.phy_id == self.phy.to_string(),
            "memory belongs to another principal or private owner"
        );
        let owner = binding.rel_id.parse()?;
        let instance = self
            .instances
            .get(&owner)
            .context("original memory owner unavailable")?;
        Ok((owner, instance))
    }

    pub(super) fn runtime(
        &self,
        owner: OwnerId,
        instance: &Instance,
        config: &Config,
    ) -> Result<Option<reliquary_memory::ConfiguredRuntime>> {
        let Some((runtime, identity)) = super::hosted::from_profile(&self.home, config)? else {
            return Ok(None);
        };
        self.check_embedding_identity(owner, instance, &identity)?;
        Ok(Some(runtime))
    }

    fn check_embedding_identity(
        &self,
        owner: OwnerId,
        instance: &Instance,
        identity: &super::hosted::EmbeddingIdentity,
    ) -> Result<()> {
        let _guard = self.embeddings.lock().expect("embedding identity lock");
        let dir = self.directory.join("embedding-identities-v1");
        ensure_directory(&dir)?;
        let path = dir.join(format!("{owner}.json"));
        if let Some(file) = regular_file(&path)? {
            let recorded: super::hosted::EmbeddingIdentity = read_state(file)?;
            ensure!(
                &recorded == identity,
                "incompatible memory embedding profile"
            );
        } else {
            ensure!(
                !instance.has_active_vectors(owner)?,
                "unbound memory embedding profile"
            );
            save_new_state(&path, identity)?;
        }
        Ok(())
    }
}

/// First staged import identity, retained after the native chat is deleted.
/// A terminal record fences all later extensions of this source.
#[derive(Serialize, Deserialize)]
struct CopiedSource {
    binding: CaptureBinding,
    session_id: String,
    source_sha256: [u8; 32],
    import_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    latest_import: Option<String>,
    #[serde(default)]
    terminal: bool,
    #[serde(default)]
    tombstoned: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum SourceFence {
    CommittedDelete { session_id: String },
    LegacyArchive(String),
}

impl EmbeddedMemory {
    fn source_dir(&self) -> PathBuf {
        self.directory.join("native-capture-sources-v1")
    }

    fn source_path(&self, session_id: &str) -> Result<PathBuf> {
        let scope = HermesCopyScope::new(self.profile_id.clone(), session_id.to_owned())?;
        let key = scope.conversation_id();
        let digest = key
            .strip_prefix("hermes-copy:")
            .context("invalid copied source identity")?;
        ensure!(
            digest.len() == 64 && digest.bytes().all(|c| c.is_ascii_hexdigit()),
            "invalid copied source digest"
        );
        Ok(self.source_dir().join(format!("{digest}.json")))
    }

    fn remember_source(
        &self,
        capture: &PendingMemoryCapture,
        source_sha256: [u8; 32],
        import_id: &str,
    ) -> Result<()> {
        let path = self.source_path(&capture.session_id)?;
        ensure_directory(&self.source_dir())?;
        ensure!(
            regular_file(&path.with_extension("terminal"))?.is_none(),
            "memory source is terminal"
        );
        if let Some(file) = regular_file(&path)? {
            let mut saved: CopiedSource = read_state(file)?;
            ensure!(
                saved.session_id == capture.session_id
                    && saved.binding == capture.binding
                    && !saved.terminal,
                "memory source is terminal or has changed its route"
            );
            if saved.latest_import.as_deref() != Some(import_id) {
                saved.latest_import = Some(import_id.to_owned());
                save_state(&path, &saved, true)?;
            }
            return Ok(());
        }
        save_new_state(
            &path,
            &CopiedSource {
                binding: capture.binding.clone(),
                session_id: capture.session_id.clone(),
                source_sha256,
                import_id: import_id.to_owned(),
                latest_import: Some(import_id.to_owned()),
                terminal: false,
                tombstoned: false,
            },
        )
    }

    /// Called while holding the source lock. Mark the host fence before asking
    /// Root to reconcile the precise copied source.
    fn tombstone_source(
        &self,
        path: &Path,
        source: &mut CopiedSource,
        config: &Config,
    ) -> Result<bool> {
        if source.tombstoned {
            return Ok(true);
        }
        if !source.terminal {
            source.terminal = true;
            save_state(path, source, true)?;
        }
        let Ok((owner, instance)) = self.route(&source.binding) else {
            return Ok(false);
        };
        self.resume_before_removal(owner, instance, source, config)?;
        if let Some(status) = instance.native_import_status(owner, &source.import_id)? {
            if status.source_key != hex(&source.source_sha256) || status.receipt_digest.is_none() {
                return Ok(false);
            }
            let scope = HermesCopyScope::new(self.profile_id.clone(), source.session_id.clone())?;
            // An open Root import epoch may defer this operation. The host
            // fence already prevents further source extensions.
            if instance
                .apply_hermes_copy_source_change(
                    owner,
                    &scope,
                    source.source_sha256,
                    &format!("delete-{}", source.session_id),
                    reliquary_memory::HermesCopySourceChange::Delete,
                )
                .is_err()
            {
                return Ok(false);
            }
        }
        source.tombstoned = true;
        save_state(path, source, true)?;
        Ok(true)
    }

    /// The host fence of a removal that Core has not acknowledged.
    fn removal_pending(&self, path: &Path, source: &CopiedSource) -> Result<bool> {
        Ok(!source.tombstoned
            && (source.terminal || regular_file(&path.with_extension("terminal"))?.is_some()))
    }

    /// Whether a removal of this source has yet to reach Core. Its copies stay undisclosed until then.
    pub(super) fn source_change_pending(
        &self,
        sources: &dyn MemorySources,
        session_id: &str,
    ) -> Result<bool> {
        {
            let _guard = self.source_index.lock().expect("memory source index lock");
            let path = self.source_path(session_id)?;
            if let Some(file) = regular_file(&path)?
                && self.removal_pending(&path, &read_state(file)?)?
            {
                return Ok(true);
            }
        }
        sources.copied_source_change_pending(session_id)
    }

    fn resume_before_removal(
        &self,
        owner: OwnerId,
        instance: &Instance,
        source: &CopiedSource,
        config: &Config,
    ) -> Result<()> {
        let import_id = source.latest_import.as_deref().unwrap_or(&source.import_id);
        if let Some(status) = instance.native_import_status(owner, import_id)?
            && matches!(
                status.state,
                NativeImportState::Staged | NativeImportState::Settled
            )
            && status.receipt_digest.is_some()
        {
            self.start_import(owner, instance, import_id, config)?;
        }
        Ok(())
    }

    fn start_import(
        &self,
        owner: OwnerId,
        instance: &Instance,
        import_id: &str,
        config: &Config,
    ) -> Result<()> {
        let mut jobs = self.jobs.lock().expect("memory jobs lock");
        if jobs.get(&owner).is_some_and(|job| !job.is_finished()) {
            return Ok(());
        }
        if let Some(job) = jobs.remove(&owner) {
            let _ = job.join();
        }
        if let Some(runtime) = self.runtime(owner, instance, config)? {
            let job = instance.start_processing(
                owner,
                &runtime,
                ProcessOptions {
                    import_id: Some(import_id.to_owned()),
                    ..ProcessOptions::default()
                },
            )?;
            jobs.insert(owner, job);
        }
        Ok(())
    }
}

impl MemoryStore for EmbeddedMemory {
    fn configure_profile_runtime(&self, config: &Config) -> Result<()> {
        self.gateway.set_perception_runtime(None)?;
        let Some(endpoint_id) = ProfileSettings::load(&self.home)?.memory_perception_endpoint
        else {
            return Ok(());
        };
        let Some((runtime, identity)) = super::hosted::from_profile(&self.home, config)? else {
            anyhow::bail!("Configure the Reliquary embedding route before enabling Perception");
        };
        let runtime = super::hosted::with_native_perception(&self.home, &endpoint_id, runtime)?;
        for (&owner, instance) in &self.instances {
            self.check_embedding_identity(owner, instance, &identity)?;
        }
        self.gateway.set_perception_runtime(Some(&runtime))?;
        Ok(())
    }

    fn projects_changed(&self, projects: &[Project]) -> bool {
        self.managed_projects
            .as_deref()
            .is_some_and(|mounted| mounted != projects)
    }

    /// Restoring or opening existing chats uses their saved binding.
    fn binding_for_new_session(&self, summary: &SessionSummary) -> Result<Option<CaptureBinding>> {
        if summary.archived
            || summary.bot.is_some()
            || !matches!(summary.source, SessionSource::Native)
        {
            return Ok(None);
        }
        let created = chrono::DateTime::parse_from_rfc3339(&summary.created_at)?;
        if created
            .timestamp_nanos_opt()
            .context("Invalid chat creation time")? as f64
            / 1e9
            <= self.cutoff
        {
            return Ok(None);
        }
        let rel = match summary.project_id.as_ref() {
            Some(project) => match self.projects.get(project) {
                Some(owner) => *owner,
                None if self.managed_projects.is_none() => self.general,
                None => anyhow::bail!("Project memory is being prepared. Retry creating the chat."),
            },
            None => self.general,
        };
        Ok(Some(CaptureBinding {
            principal_id: self.principal_id.clone(),
            profile_id: self.profile_id.clone(),
            project_id: summary.project_id.clone(),
            rel_id: rel.to_string(),
            phy_id: self.phy.to_string(),
        }))
    }

    fn stage(
        &self,
        capture: &PendingMemoryCapture,
        turn: &Turn,
        previous: Option<[u8; 32]>,
    ) -> Result<CaptureReceipt> {
        let (rel, instance) = self.route(&capture.binding)?;
        let rows = capture.verified_rows(turn)?;
        let snapshot = HermesCopiedSnapshot {
            scope: HermesCopyScope::new(self.profile_id.clone(), capture.session_id.clone())?,
            branch_id: None,
            source_revision: Some(format!("generation-{}", capture.generation)),
            messages: rows
                .into_iter()
                .map(|row| HermesCopiedMessage {
                    message_uid: row.message_uid,
                    role: if row.role == "user" {
                        HermesCopiedRole::User
                    } else if row.commentary {
                        HermesCopiedRole::Commentary
                    } else {
                        HermesCopiedRole::Assistant
                    },
                    timestamp_ns: row.timestamp_ns,
                    content: row.content,
                    author: None,
                })
                .collect(),
        };
        let echo = capture
            .verified_echo(turn)?
            .into_iter()
            .map(|event| {
                Ok(HermesCopiedEcho {
                    timestamp_ns: capture.committed_ns,
                    message_uid: event.message_uid,
                    sequence: event.sequence.try_into()?,
                    model_round: event.model_round.try_into()?,
                    kind: match event.kind {
                        CapturedEchoKind::ModelRoundStarted => {
                            HermesCopiedEchoKind::ModelRoundStarted
                        }
                        CapturedEchoKind::ModelRoundCompleted => {
                            HermesCopiedEchoKind::ModelRoundCompleted
                        }
                        CapturedEchoKind::ToolCall => HermesCopiedEchoKind::ToolCall {
                            call_id: event.correlation_id,
                            tool_name: event.name,
                            redacted_arguments: event.content,
                        },
                        CapturedEchoKind::ToolResult => HermesCopiedEchoKind::ToolResult {
                            call_id: event.correlation_id,
                            outcome: if event.succeeded {
                                HermesCopiedToolOutcome::Succeeded
                            } else {
                                HermesCopiedToolOutcome::Failed
                            },
                            redacted_result: event.content,
                        },
                        CapturedEchoKind::ReasoningSummary => {
                            HermesCopiedEchoKind::ReasoningSummary {
                                redacted_summary: event.content,
                            }
                        }
                    },
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let plan = match (previous, echo.is_empty()) {
            (Some(previous), true) => {
                NativeImportPlan::from_hermes_copy_extension(snapshot, previous)?
            }
            (None, true) => NativeImportPlan::from_hermes_copy(snapshot)?,
            (Some(previous), false) => {
                NativeImportPlan::from_hermes_copy_extension_with_echo(snapshot, previous, echo)?
            }
            (None, false) => NativeImportPlan::from_hermes_copy_with_echo(snapshot, echo)?,
        };
        let source_sha256 = plan.source_sha256();
        let import_id = plan.import_id(rel);
        let _guard = self.source_index.lock().expect("memory source index lock");
        self.remember_source(capture, source_sha256, &import_id)?;
        if let Some(status) = instance.native_import_status(rel, &import_id)? {
            ensure!(
                status.source_key == hex(&source_sha256),
                "stored capture source differs"
            );
            if let Some(receipt_digest) = status.receipt_digest {
                return Ok(CaptureReceipt {
                    source_sha256,
                    import_id,
                    receipt_digest,
                });
            }
        }
        let staged = instance.stage_native_import(rel, plan)?;
        ensure!(
            staged.import_id == import_id && staged.source_sha256 == source_sha256,
            "staged receipt differs from the committed source"
        );
        Ok(CaptureReceipt {
            source_sha256,
            import_id,
            receipt_digest: staged.receipt_digest,
        })
    }

    fn ensure_processing(
        &self,
        binding: &CaptureBinding,
        session_id: &str,
        receipt: &CaptureReceipt,
        config: &Config,
    ) -> Result<bool> {
        let _guard = self.source_index.lock().expect("memory source index lock");
        let path = self.source_path(session_id)?;
        if let Some(state) = regular_file(&path)? {
            let state: CopiedSource = read_state(state)?;
            if state.terminal || regular_file(&path.with_extension("terminal"))?.is_some() {
                return Ok(false);
            }
        }
        let (owner, instance) = self.route(binding)?;
        let status = instance
            .native_import_status(owner, &receipt.import_id)?
            .context("committed capture receipt has no native import")?;
        ensure!(
            status.source_key == hex(&receipt.source_sha256)
                && status.receipt_digest == Some(receipt.receipt_digest),
            "committed memory and archive receipts differ"
        );
        if status.state == NativeImportState::Completed {
            if let Some(runtime) = self.runtime(owner, instance, config)? {
                instance.publish_archive_vectors(owner, &runtime, None)?;
            }
            return Ok(true);
        }
        self.start_import(owner, instance, &receipt.import_id, config)?;
        Ok(false)
    }

    fn stop_processing(&self) {
        let _ = self.gateway.set_perception_runtime(None);
        let mut jobs = self.jobs.lock().expect("memory jobs lock");
        for job in jobs.values() {
            job.request_stop();
        }
        jobs.clear();
    }

    fn release_legacy_archive_fence(
        &self,
        binding: &CaptureBinding,
        session_id: &str,
        initial_proof: Option<(&CaptureReceipt, &str)>,
    ) -> Result<bool> {
        let _guard = self.source_index.lock().expect("memory source index lock");
        let path = self.source_path(session_id)?;
        let marker = path.with_extension("terminal");
        let fenced = match regular_file(&marker)?.map(read_state).transpose()? {
            Some(SourceFence::CommittedDelete { .. }) => return Ok(false),
            Some(SourceFence::LegacyArchive(id)) => {
                ensure!(id == session_id, "invalid copied source fence");
                true
            }
            None => false,
        };
        if let Some(file) = regular_file(&path)? {
            let mut source: CopiedSource = read_state(file)?;
            ensure!(
                &source.binding == binding && source.session_id == session_id,
                "invalid copied source record"
            );
            if fenced || source.terminal {
                let (owner, instance) = self.route(binding)?;
                if let Some(status) = instance.native_import_status(owner, &source.import_id)? {
                    ensure!(
                        status.source_key == hex(&source.source_sha256),
                        "invalid copied source receipt"
                    );
                    if let Some(digest) = status.receipt_digest {
                        let Some((proof, uid)) = initial_proof else {
                            return Ok(false);
                        };
                        if proof.source_sha256 != source.source_sha256
                            || proof.receipt_digest != digest
                        {
                            return Ok(false);
                        }
                        let scope =
                            HermesCopyScope::new(self.profile_id.clone(), session_id.to_owned())?;
                        if instance
                            .read_hermes_copy_source(owner, &scope, proof.source_sha256, uid, uid)?
                            .is_none()
                        {
                            return Ok(false);
                        }
                    }
                }
                source.terminal = false;
                source.tombstoned = false;
                save_state(&path, &source, true)?;
            }
        }
        if fenced {
            fs::remove_file(marker)?;
        }
        Ok(true)
    }

    fn terminal_source(&self, session_id: &str, config: &Config) -> Result<bool> {
        let _guard = self.source_index.lock().expect("memory source index lock");
        let path = self.source_path(session_id)?;
        ensure_directory(&self.source_dir())?;
        let fence = path.with_extension("terminal");
        let existing = regular_file(&fence)?
            .map(read_state::<SourceFence>)
            .transpose()?;
        if let Some(SourceFence::CommittedDelete { session_id: id }) = &existing {
            ensure!(id == session_id, "invalid copied source fence");
        } else {
            save_state(
                &fence,
                &SourceFence::CommittedDelete {
                    session_id: session_id.to_owned(),
                },
                existing.is_some(),
            )?;
        }
        if let Some(file) = regular_file(&path)? {
            let mut source: CopiedSource = read_state(file)?;
            ensure!(
                source.session_id == session_id,
                "invalid copied source record"
            );
            return self.tombstone_source(&path, &mut source, config);
        }
        Ok(true)
    }

    fn unfinished_removals(&self) -> Result<Vec<String>> {
        let _guard = self.source_index.lock().expect("memory source index lock");
        let entries = match self.source_dir().read_dir() {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut unfinished = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let Some(file) = regular_file(&path)? else {
                continue;
            };
            let source: CopiedSource = read_state(file)?;
            ensure!(
                path == self.source_path(&source.session_id)?,
                "invalid copied source record"
            );
            if self.removal_pending(&path, &source)? {
                unfinished.push(source.session_id);
            }
        }
        Ok(unfinished)
    }

    fn rewind_source(
        &self,
        binding: &CaptureBinding,
        session_id: &str,
        event_id: &str,
        preceding_head: [u8; 32],
        last_surviving_message_uid: Option<&str>,
        config: &Config,
    ) -> Result<[u8; 32]> {
        use reliquary_memory::HermesCopySourceChange;
        let _guard = self.source_index.lock().expect("memory source index lock");
        let (owner, instance) = self.route(binding)?;
        if let Some(file) = regular_file(&self.source_path(session_id)?)? {
            let source: CopiedSource = read_state(file)?;
            ensure!(
                &source.binding == binding && source.session_id == session_id,
                "invalid copied source record"
            );
            self.resume_before_removal(owner, instance, &source, config)?;
        }
        let scope = HermesCopyScope::new(self.profile_id.clone(), session_id.to_owned())?;
        instance
            .apply_hermes_copy_source_change(
                owner,
                &scope,
                preceding_head,
                event_id,
                HermesCopySourceChange::Rewind {
                    last_surviving_message_uid: last_surviving_message_uid.map(str::to_owned),
                },
            )?
            .resulting_head_sha256
            .context("rewind did not retain a copied source head")
    }
    fn search_user_archive(
        &self,
        sources: &dyn MemorySources,
        projects: &[Project],
        project_id: Option<&str>,
        include_related: bool,
        query: &str,
        config: &Config,
    ) -> Result<Vec<UserArchiveHit>> {
        ensure!(
            config.api_key.is_empty() || !query.contains(&config.api_key),
            "Invalid Archive query"
        );
        let mut routes = Vec::new();
        if project_id.is_none() || include_related {
            routes.push((None, self.general, "General"));
        }
        if let Some(project_id) = project_id {
            let project = projects
                .iter()
                .find(|project| project.id == project_id)
                .context("The current project is unavailable")?;
            if let Some(owner) = self.projects.get(project_id) {
                routes.push((Some(project_id), *owner, project.name.as_str()));
            } else if self.managed_projects.is_none() {
                routes.push((Some(project_id), self.general, "General"));
            } else {
                anyhow::bail!("The current project has no assigned Archive");
            }
        } else if include_related {
            routes.extend(projects.iter().filter_map(|project| {
                self.projects
                    .get(&project.id)
                    .map(|owner| (Some(project.id.as_str()), *owner, project.name.as_str()))
            }));
        }
        let mut hits = Vec::new();
        for (project, owner, name) in routes {
            let binding = CaptureBinding {
                profile_id: self.profile_id.clone(),
                principal_id: self.principal_id.clone(),
                project_id: project.map(str::to_owned),
                rel_id: owner.to_string(),
                phy_id: self.phy.to_string(),
            };
            let (_, instance) = self.route(&binding)?;
            let runtime = self
                .runtime(owner, instance, config)?
                .context("Configure an Archive embedding endpoint first")?;
            let options = reliquary_memory::ArchiveSearchOptions {
                limit: Some(3),
                embedding_route: None,
            };
            let search = match instance.search_archive(
                owner,
                &hermes_engine::native_memory::redact_text(query),
                options,
                &runtime,
            ) {
                Ok(search) => search,
                Err(error) if error.kind() == reliquary_memory::ProductErrorKind::Unavailable => {
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            for hit in search.hits {
                let Some(rows) = instance.read_hermes_archive_range(
                    owner,
                    &hit.conversation_id,
                    &hit.start_node_id,
                    &hit.end_node_id,
                    search.source_archive_version,
                )?
                else {
                    continue;
                };
                let mut pending = false;
                for row in &rows {
                    if row.source.profile_id != binding.profile_id
                        || self.source_change_pending(sources, &row.source.session_id)?
                    {
                        pending = true;
                        break;
                    }
                }
                if pending {
                    continue;
                }
                let mut excerpt = rows
                    .into_iter()
                    .map(|row| row.archive.content)
                    .collect::<Vec<_>>()
                    .join("\n");
                if hermes_engine::memory_capture::contains_inline_data(&excerpt)
                    || (!config.api_key.is_empty() && excerpt.contains(&config.api_key))
                {
                    continue;
                }
                excerpt = hermes_engine::native_memory::redact_text(&excerpt);
                excerpt.truncate(excerpt.floor_char_boundary(excerpt.len().min(2000)));
                hits.push((
                    hit.score,
                    UserArchiveHit {
                        rel: name.into(),
                        text: excerpt,
                    },
                ));
            }
        }
        hits.sort_by(|left, right| right.0.total_cmp(&left.0));
        Ok(hits.into_iter().take(10).map(|(_, hit)| hit).collect())
    }

    fn query(
        self: Arc<Self>,
        sources: Arc<dyn MemorySources>,
        binding: CaptureBinding,
        turn: MemoryTurn,
    ) -> Arc<dyn MemoryQuery> {
        Arc::new(crate::query::GatewayMemoryQuery::new(
            self, sources, binding, turn,
        ))
    }
}

pub(super) fn ensure_directory(dir: &Path) -> Result<()> {
    #[allow(unused_mut)]
    let mut builder = fs::DirBuilder::new();
    // Copied conversations are private to the user.
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    match builder.create(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let meta = dir.symlink_metadata()?;
    ensure!(
        meta.is_dir() && !meta.is_symlink(),
        "invalid Reliquary state directory"
    );
    Ok(())
}

pub(super) fn save_new_state<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    save_state(path, value, false)
}

pub(super) fn save_state<T: Serialize>(path: &Path, value: &T, replace: bool) -> Result<()> {
    let dir = path.parent().context("Reliquary state has no directory")?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    if replace {
        file.persist(path).map_err(|error| error.error)?;
    } else {
        file.persist_noclobber(path).map_err(|error| error.error)?;
    }
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    Ok(())
}

pub(super) fn hex(digest: &[u8; 32]) -> String {
    use std::fmt::Write;
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        write!(encoded, "{byte:02x}").expect("write to String");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::legacy;
    use hermes_engine::{Entry, Status};
    use serde_json::json;
    use std::fs;

    #[test]
    fn committed_turn_replays_real_archive_receipt_under_frozen_project_and_principal() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("reliquary");
        fs::create_dir(&dir).unwrap();
        let general = Rel::create(tmp.path().join("general.rel")).unwrap();
        let project = Rel::create(tmp.path().join("project.rel")).unwrap();
        let phy = Phylactery::create(tmp.path().join("user.phy")).unwrap();
        let mut owners = serde_json::Map::new();
        for rel in [&general, &project] {
            owners.insert(
                rel.owner_id().to_string(),
                json!({"kind":"rel","path":rel.path()}),
            );
        }
        owners.insert(
            phy.owner_id().to_string(),
            json!({"kind":"phylactery","path":phy.path()}),
        );
        let topology = json!({"version":2,"profile_id":"default","principal_id":"owner",
            "main_rel":general.owner_id(),"active_phy":phy.owner_id(),
            "projects":{"example":{"rel_id":project.owner_id()}},"owners":owners});
        fs::write(
            dir.join("capture-enrollment-v1.json"),
            br#"{"version":1,"cutoff":1}"#,
        )
        .unwrap();
        fs::write(
            dir.join("topology-v1.json"),
            serde_json::to_vec(&topology).unwrap(),
        )
        .unwrap();
        assert!(legacy(tmp.path(), "other").is_err());
        let mem = legacy(tmp.path(), "default").unwrap().unwrap();
        let library = hermes_library::Library::open(tmp.path().join("library")).unwrap();
        let mut summary = library.create_session(None, Some("native")).unwrap();
        assert_eq!(
            mem.binding_for_new_session(&summary)
                .unwrap()
                .unwrap()
                .rel_id,
            general.owner_id().to_string()
        );
        summary.project_id = Some("example".to_owned());
        let binding = mem.binding_for_new_session(&summary).unwrap().unwrap();
        assert_eq!(binding.rel_id, project.owner_id().to_string());
        let turn = Turn {
            prompt: "remember this project".into(),
            entries: vec![Entry::Assistant {
                text: "confirmed".into(),
                calls: vec![],
            }],
            status: Status::Complete,
            model: "mock".into(),
            attachments: vec![],
            context: String::new(),
        };
        let capture =
            PendingMemoryCapture::new(&binding, &summary.id, "admission", 1, 0, 10, 11, &turn)
                .unwrap();
        let receipt = mem.stage(&capture, &turn, None).unwrap();
        let mut foreign = binding.clone();
        foreign.principal_id = "other".into();
        let denied =
            PendingMemoryCapture::new(&foreign, &summary.id, "admission", 1, 0, 10, 11, &turn)
                .unwrap();
        assert!(mem.stage(&denied, &turn, None).is_err());
        drop(mem);
        let reopened = legacy(tmp.path(), "default").unwrap().unwrap();
        assert_eq!(receipt, reopened.stage(&capture, &turn, None).unwrap());
        let (_, scoped) = reopened.route(&binding).unwrap();
        assert!(scoped.phylactery_info(phy.owner_id()).is_ok());
        let other = Gateway::new();
        let ungranted = other.mount_phylactery(&phy, false).unwrap();
        let (adapter, _) = TrustedHostAdmission::new([HostCredentialBinding::new(
            "other",
            "different-profile-fixture-credential",
            [],
        )])
        .install(&other)
        .unwrap();
        let foreign = adapter
            .authenticate("different-profile-fixture-credential")
            .unwrap();
        assert!(foreign.phylactery_info(ungranted).is_err());

        library.set_archived(&summary.id, true).unwrap();
        assert!(library.pending_memory_deletions().unwrap().is_empty());
        assert_eq!(receipt, reopened.stage(&capture, &turn, None).unwrap());
        fs::write(
            summary.directory.join("gateway.json"),
            serde_json::to_vec(&serde_json::json!({"memory_binding": binding})).unwrap(),
        )
        .unwrap();
        library.delete_archived(&summary.id).unwrap();
        drop(reopened);
        let reopened = legacy(tmp.path(), "default").unwrap().unwrap();
        assert_eq!(
            library.pending_memory_deletions().unwrap(),
            [summary.id.clone()]
        );
        // The open import can delay acknowledgment while the explicit deletion fences capture.
        let config = Config {
            base_url: String::new(),
            model: String::new(),
            api_key: String::new(),
        };
        reopened.terminal_source(&summary.id, &config).unwrap();
        assert!(reopened.stage(&capture, &turn, None).is_err());
        assert!(library.session(&summary.id).is_err());

        // A deletion made before the journal existed left a fence and no acknowledgment. It is
        // still withheld and still retried.
        struct NoJournal;
        impl MemorySources for NoJournal {
            fn check_session(&self, _: &str, _: &CaptureBinding) -> Result<()> {
                Ok(())
            }
            fn copied_source_change_pending(&self, _: &str) -> Result<bool> {
                Ok(false)
            }
        }
        let path = reopened.source_path(&summary.id).unwrap();
        let mut source: CopiedSource = read_state(File::open(&path).unwrap()).unwrap();
        source.terminal = true;
        source.tombstoned = false;
        save_state(&path, &source, true).unwrap();
        save_state(&path.with_extension("terminal"), &summary.id, true).unwrap();
        assert!(reopened.source_change_pending(&NoJournal, &summary.id).unwrap());
        assert_eq!(reopened.unfinished_removals().unwrap(), [summary.id.clone()]);
        source.tombstoned = true;
        save_state(&path, &source, true).unwrap();
        assert!(!reopened.source_change_pending(&NoJournal, &summary.id).unwrap());
        assert!(reopened.unfinished_removals().unwrap().is_empty());

        let retired = library
            .create_session(None, Some("archive before capture"))
            .unwrap();
        let preempted = PendingMemoryCapture::new(
            &reopened.binding_for_new_session(&retired).unwrap().unwrap(),
            &retired.id,
            "admission-before-archive",
            1,
            0,
            12,
            13,
            &turn,
        )
        .unwrap();
        reopened.terminal_source(&retired.id, &config).unwrap();
        assert!(reopened.stage(&preempted, &turn, None).is_err());

        summary.archived = true;
        assert!(
            reopened
                .binding_for_new_session(&summary)
                .unwrap()
                .is_none()
        );
        summary.archived = false;
        summary.created_at = "1970-01-01T00:00:00Z".into();
        assert!(
            reopened
                .binding_for_new_session(&summary)
                .unwrap()
                .is_none()
        );
    }
}
