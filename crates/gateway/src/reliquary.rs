//! Bind newly enrolled gateway chats to the selected profile's Reliquary owners.
use anyhow::{Context, Result, ensure};
use hermes_engine::{
    Config, Turn,
    memory_capture::{CaptureBinding, CaptureReceipt, PendingMemoryCapture},
};
use hermes_library::{SessionSource, SessionSummary};
use reliquary_memory::{
    Gateway, HermesCopiedMessage, HermesCopiedRole, HermesCopiedSnapshot, HermesCopyScope,
    HostCredentialBinding, Instance, NativeImportPlan, NativeImportState,
    OwnerGrant, OwnerId, OwnerKind, OwnerProcessingJob, Phylactery, ProcessOptions, Rel,
    TrustedHostAdmission,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};
use uuid::Uuid;

#[derive(Deserialize)]
struct Enrollment {
    version: u8,
    cutoff: f64,
}

/// The v2 registry is authoritative after the user switches a REL or PHY.
/// Only the currently assigned owner paths are opened.
#[derive(Deserialize)]
struct Topology {
    version: u8,
    profile_id: String,
    principal_id: String,
    main_rel: OwnerId,
    active_phy: OwnerId,
    projects: BTreeMap<String, ProjectOwner>,
    owners: BTreeMap<OwnerId, RegisteredOwner>,
}

#[derive(Deserialize)]
struct ProjectOwner {
    rel_id: OwnerId,
}

#[derive(Deserialize)]
struct RegisteredOwner {
    kind: OwnerKind,
    path: PathBuf,
}

pub(crate) struct EmbeddedMemory {
    profile_id: String,
    principal_id: String,
    cutoff: f64,
    general: OwnerId,
    phy: OwnerId,
    projects: BTreeMap<String, OwnerId>,
    instances: BTreeMap<OwnerId, Instance>,
    home: PathBuf,
    jobs: Mutex<BTreeMap<OwnerId, SourceJob>>,
    embeddings: Mutex<()>,
    source_index: Mutex<()>,
    // The stock host takes the same lease. Drop after dropping the instances.
    _lease: File,
}

struct SourceJob {
    session_id: String,
    task: OwnerProcessingJob,
}

fn regular_file(path: &Path) -> Result<Option<File>> {
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

fn read_state<T: DeserializeOwned>(file: File) -> Result<T> {
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
    /// Enrollment and topology are maintained by the profile's existing setup.
    /// A profile without enrollment starts ordinary gateway chat immediately.
    pub(crate) fn open(home: &Path, expected_profile: &str) -> Result<Option<Self>> {
        let home = home.canonicalize()?;
        let dir = home.join("reliquary");
        let Some(enrollment_file) = regular_file(&dir.join("capture-enrollment-v1.json"))? else {
            return Ok(None);
        };
        ensure!(
            dir.canonicalize()? == dir,
            "unexpected Reliquary state directory"
        );
        let path = dir.join("service.lock");
        if let Ok(meta) = path.symlink_metadata() {
            ensure!(
                meta.is_file() && !meta.is_symlink(),
                "invalid Reliquary service lease"
            );
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lease = options.open(path)?;
        lease
            .try_lock()
            .context("Reliquary profile is already in use")?;
        let enrollment: Enrollment = read_state(enrollment_file)?;
        ensure!(
            enrollment.version == 1
                && enrollment.cutoff.is_finite()
                && enrollment.cutoff > 0.0
                && enrollment.cutoff <= chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
            "invalid enrollment cutoff"
        );
        let topology: Topology = read_state(
            regular_file(&dir.join("topology-v1.json"))?
                .context("enrolled Reliquary profile has no registered owner topology")?,
        )?;
        ensure!(
            topology.version == 2 && !topology.principal_id.is_empty(),
            "unsupported Reliquary topology"
        );
        ensure!(topology.profile_id == expected_profile, "Reliquary topology belongs to a different profile");
        HermesCopyScope::new(topology.profile_id.clone(), "profile-check".into())?;
        let gateway = Gateway::new();
        let owner_path = |id: OwnerId, kind: OwnerKind| -> Result<&Path> {
            let owner = topology
                .owners
                .get(&id)
                .context("missing registered owner")?;
            ensure!(
                owner.kind == kind && owner.path.is_absolute(),
                "invalid registered owner"
            );
            Ok(&owner.path)
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
            credentials.push(HostCredentialBinding::new(
                &topology.principal_id,
                &token,
                [OwnerGrant::OperateRel(id), OwnerGrant::ViewPhylactery(phy)],
            ));
            tokens.insert(id, token);
        }
        let (adapter, _) = TrustedHostAdmission::new(credentials).install(&gateway)?;
        for (id, token) in tokens {
            instances.insert(id, adapter.authenticate(&token)?);
        }
        Ok(Some(Self {
            profile_id: topology.profile_id,
            principal_id: topology.principal_id,
            cutoff: enrollment.cutoff,
            general: topology.main_rel,
            phy,
            projects,
            instances,
            home,
            jobs: Mutex::new(BTreeMap::new()),
            embeddings: Mutex::new(()),
            source_index: Mutex::new(()),
            _lease: lease,
        }))
    }

    /// Call only while creating an ordinary root chat. Restoring or opening an
    /// existing chat must use the binding saved in its own native record.
    pub(crate) fn binding_for_new_session(
        &self,
        summary: &SessionSummary,
    ) -> Option<CaptureBinding> {
        if summary.archived
            || summary.bot.is_some()
            || !matches!(summary.source, SessionSource::Native)
        {
            return None;
        }
        let created = chrono::DateTime::parse_from_rfc3339(&summary.created_at).ok()?;
        if created.timestamp_nanos_opt()? as f64 / 1e9 <= self.cutoff {
            return None;
        }
        let rel = summary
            .project_id
            .as_ref()
            .and_then(|id| self.projects.get(id))
            .copied()
            .unwrap_or(self.general);
        Some(CaptureBinding {
            principal_id: self.principal_id.clone(),
            profile_id: self.profile_id.clone(),
            project_id: summary.project_id.clone(),
            rel_id: rel.to_string(),
            phy_id: self.phy.to_string(),
        })
    }

    pub(crate) fn stage(
        &self,
        capture: &PendingMemoryCapture,
        turn: &Turn,
        previous: Option<&CaptureReceipt>,
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
                    } else {
                        HermesCopiedRole::Assistant
                    },
                    timestamp_ns: row.timestamp_ns,
                    content: row.content,
                    author: None,
                })
                .collect(),
        };
        let plan = if let Some(previous) = previous {
            NativeImportPlan::from_hermes_copy_extension(snapshot, previous.source_sha256)?
        } else {
            NativeImportPlan::from_hermes_copy(snapshot)?
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

    fn route(&self, binding: &CaptureBinding) -> Result<(OwnerId, &Instance)> {
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

    pub(super) fn ensure_processing(
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
            return Ok(true);
        }
        let mut jobs = self.jobs.lock().expect("memory jobs lock");
        if jobs.get(&owner).is_some_and(|job| !job.task.is_finished()) {
            return Ok(false);
        }
        if let Some(job) = jobs.remove(&owner) {
            let _ = job.task.join();
        }
        if let Some(runtime) = self.runtime(owner, instance, config)? {
            let job = instance.start_processing(
                owner,
                &runtime,
                ProcessOptions {
                    import_id: Some(receipt.import_id.clone()),
                    ..ProcessOptions::default()
                },
            )?;
            jobs.insert(
                owner,
                SourceJob {
                    session_id: session_id.into(),
                    task: job,
                },
            );
        }
        Ok(false)
    }

    pub(super) fn stop_processing(&self) {
        let mut jobs = self.jobs.lock().expect("memory jobs lock");
        for job in jobs.values() {
            job.task.request_stop();
        }
        jobs.clear();
    }

    fn runtime(
        &self,
        owner: OwnerId,
        instance: &Instance,
        config: &Config,
    ) -> Result<Option<reliquary_memory::ConfiguredRuntime>> {
        let Some((runtime, identity)) = super::reliquary_hosted::from_profile(&self.home, config)?
        else {
            return Ok(None);
        };
        let _guard = self.embeddings.lock().expect("embedding identity lock");
        let dir = self.home.join("reliquary").join("embedding-identities-v1");
        ensure_directory(&dir)?;
        let path = dir.join(format!("{owner}.json"));
        if let Some(file) = regular_file(&path)? {
            let recorded: super::reliquary_hosted::EmbeddingIdentity = read_state(file)?;
            ensure!(
                recorded == identity,
                "incompatible memory embedding profile"
            );
        } else {
            ensure!(
                !instance.has_active_vectors(owner)?,
                "unbound memory embedding profile"
            );
            save_new_state(&path, &identity)?;
        }
        Ok(Some(runtime))
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
    #[serde(default)]
    terminal: bool,
    #[serde(default)]
    tombstoned: bool,
}

impl EmbeddedMemory {
    fn source_dir(&self) -> PathBuf {
        self.home
            .join("reliquary")
            .join("native-capture-sources-v1")
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
            let saved: CopiedSource = read_state(file)?;
            ensure!(
                saved.session_id == capture.session_id
                    && saved.binding == capture.binding
                    && !saved.terminal,
                "memory source is terminal or has changed its route"
            );
            return Ok(());
        }
        save_new_state(
            &path,
            &CopiedSource {
                binding: capture.binding.clone(),
                session_id: capture.session_id.clone(),
                source_sha256,
                import_id: import_id.to_owned(),
                terminal: false,
                tombstoned: false,
            },
        )
    }

    pub(super) fn reconcile_tombstones(&self, summaries: &[SessionSummary]) -> Result<()> {
        let _guard = self.source_index.lock().expect("memory source index lock");
        let dir = self.source_dir();
        match dir.symlink_metadata() {
            Ok(meta) => ensure!(
                meta.is_dir() && !meta.is_symlink(),
                "invalid memory source directory"
            ),
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        let active: std::collections::BTreeSet<_> = summaries
            .iter()
            .filter(|summary| !summary.archived)
            .map(|summary| summary.id.as_str())
            .collect();
        for entry in dir.read_dir()? {
            let path = entry?.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let Some(file) = regular_file(&path)? else {
                continue;
            };
            let mut source: CopiedSource = read_state(file)?;
            ensure!(
                path == self.source_path(&source.session_id)?,
                "invalid copied source record"
            );
            if source.tombstoned {
                continue;
            }
            if source.terminal
                || !active.contains(source.session_id.as_str())
                || regular_file(&path.with_extension("terminal"))?.is_some()
            {
                self.tombstone_source(&path, &mut source)?;
            }
        }
        Ok(())
    }

    pub(super) fn terminal_source(&self, session_id: &str) -> Result<()> {
        let _guard = self.source_index.lock().expect("memory source index lock");
        let path = self.source_path(session_id)?;
        ensure_directory(&self.source_dir())?;
        let fence = path.with_extension("terminal");
        if regular_file(&fence)?.is_none() {
            save_new_state(&fence, &session_id)?;
        }
        if let Some(file) = regular_file(&path)? {
            let mut source: CopiedSource = read_state(file)?;
            ensure!(
                source.session_id == session_id,
                "invalid copied source record"
            );
            self.tombstone_source(&path, &mut source)?;
        }
        Ok(())
    }

    /// Called while holding the source lock. Mark the host fence before asking
    /// Root to reconcile the precise copied source.
    fn tombstone_source(&self, path: &Path, source: &mut CopiedSource) -> Result<()> {
        if source.tombstoned {
            return Ok(());
        }
        if !source.terminal {
            source.terminal = true;
            save_state(path, source, true)?;
        }
        let Ok((owner, instance)) = self.route(&source.binding) else {
            return Ok(());
        };
        if let Some(job) = self.jobs.lock().expect("memory jobs lock").get(&owner)
            && job.session_id == source.session_id
        {
            job.task.request_stop();
        }
        if let Some(status) = instance.native_import_status(owner, &source.import_id)? {
            if status.source_key != hex(&source.source_sha256) || status.receipt_digest.is_none() {
                return Ok(());
            }
            let scope = HermesCopyScope::new(self.profile_id.clone(), source.session_id.clone())?;
            // An open Root import epoch may defer this operation. The host
            // fence already prevents further source extensions.
            if instance
                .tombstone_hermes_copy(owner, &scope, source.source_sha256)
                .is_err()
            {
                return Ok(());
            }
        }
        source.tombstoned = true;
        save_state(path, source, true)
    }
}

fn ensure_directory(dir: &Path) -> Result<()> {
    match fs::create_dir(dir) {
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

fn save_new_state<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    save_state(path, value, false)
}

fn save_state<T: Serialize>(path: &Path, value: &T, replace: bool) -> Result<()> {
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

fn hex(digest: &[u8; 32]) -> String {
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
        assert!(EmbeddedMemory::open(tmp.path(), "other").is_err());
        let mem = EmbeddedMemory::open(tmp.path(), "default").unwrap().unwrap();
        let library = hermes_library::Library::open(tmp.path().join("library")).unwrap();
        let mut summary = library.create_session(None, Some("native")).unwrap();
        assert_eq!(
            mem.binding_for_new_session(&summary).unwrap().rel_id,
            general.owner_id().to_string()
        );
        summary.project_id = Some("example".to_owned());
        let binding = mem.binding_for_new_session(&summary).unwrap();
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
        let reopened = EmbeddedMemory::open(tmp.path(), "default").unwrap().unwrap();
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
        reopened
            .reconcile_tombstones(&library.sessions(None).unwrap())
            .unwrap();
        let path = reopened.source_path(&summary.id).unwrap();
        let deleted: CopiedSource = read_state(File::open(&path).unwrap()).unwrap();
        assert!(deleted.terminal);
        library.delete_archived(&summary.id).unwrap();
        drop(reopened);
        let reopened = EmbeddedMemory::open(tmp.path(), "default").unwrap().unwrap();
        reopened
            .reconcile_tombstones(&library.sessions(None).unwrap())
            .unwrap();
        assert!(reopened.stage(&capture, &turn, None).is_err());
        assert!(library.session(&summary.id).is_err());

        let retired = library
            .create_session(None, Some("archive before capture"))
            .unwrap();
        let preempted = PendingMemoryCapture::new(
            &reopened.binding_for_new_session(&retired).unwrap(),
            &retired.id,
            "admission-before-archive",
            1,
            0,
            12,
            13,
            &turn,
        )
        .unwrap();
        reopened.terminal_source(&retired.id).unwrap();
        assert!(reopened.stage(&preempted, &turn, None).is_err());

        summary.archived = true;
        assert!(reopened.binding_for_new_session(&summary).is_none());
        summary.archived = false;
        summary.created_at = "1970-01-01T00:00:00Z".into();
        assert!(reopened.binding_for_new_session(&summary).is_none());
    }
}
