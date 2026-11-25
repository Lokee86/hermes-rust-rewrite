//! Profile-owned placement, enrollment and project reconciliation for the stock plugin.

use super::memory::{
    EmbeddedMemory, ensure_directory, read_state, regular_file, save_new_state, save_state,
};
use anyhow::{Context, Result, ensure};
use hermes_library::{Library, Project};
use reliquary_memory::{OwnerId, OwnerKind, Phylactery, Rel};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io,
    path::{Component, Path, PathBuf},
};
use uuid::Uuid;

mod deletion;
pub(crate) use deletion::purge_profile_data;

const ENROLLMENT: &str = "capture-enrollment-v1.json";
const TOPOLOGY: &str = "topology-v1.json";
const LOCATION: &str = "reliquary-storage.json";
const LEGACY_RELOCATION: &str = "legacy-profile-relocation-v1.json";

#[cfg(test)]
mod tests;

#[derive(Deserialize, Serialize)]
pub(super) struct Enrollment {
    version: u8,
    pub(super) cutoff: f64,
}

fn now() -> f64 {
    chrono::Utc::now().timestamp_micros() as f64 / 1e6
}

impl Enrollment {
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1
                && self.cutoff.is_finite()
                && self.cutoff > 0.0
                && self.cutoff <= now(),
            "Invalid Reliquary enrollment cutoff"
        );
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
pub(super) struct Topology {
    version: u8,
    pub(super) profile_id: String,
    pub(super) principal_id: String,
    pub(super) main_rel: OwnerId,
    pub(super) active_phy: OwnerId,
    pub(super) projects: BTreeMap<String, ProjectOwner>,
    pub(super) owners: BTreeMap<OwnerId, RegisteredOwner>,
    #[serde(default)]
    managed: bool,
}

#[derive(Deserialize, Serialize)]
pub(super) struct ProjectOwner {
    pub(super) rel_id: OwnerId,
}

#[derive(Deserialize, Serialize)]
pub(super) struct RegisteredOwner {
    pub(super) kind: OwnerKind,
    path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_path: Option<PathBuf>,
}

impl Topology {
    fn load(directory: &Path, profile_id: &str, managed: bool) -> Result<Self> {
        let topology: Self = read_state(
            regular_file(&directory.join(TOPOLOGY))?
                .context("Reliquary profile has no owner topology")?,
        )?;
        ensure!(
            topology.version == 2
                && !topology.principal_id.is_empty()
                && topology.profile_id == profile_id
                && topology.managed == managed,
            "Reliquary topology belongs to a different profile or format"
        );
        Ok(topology)
    }

    pub(super) fn owner_path(&self, directory: &Path, owner: &RegisteredOwner) -> Result<PathBuf> {
        ensure!(
            owner.pending_path.is_none(),
            "Reliquary owner move is still pending"
        );
        if self.managed {
            let path = managed_path(directory, &owner.path)?;
            ensure!(
                regular_file(&path)?.is_some(),
                "Registered Reliquary owner is missing"
            );
            return Ok(path);
        }
        ensure!(
            owner.path.is_absolute(),
            "Invalid registered Reliquary owner path"
        );
        Ok(owner.path.clone())
    }
}

/// A locator in the Hermes profile ties the shared store to this profile, even when
/// another data directory uses the same display name.
#[derive(Deserialize, Serialize)]
struct Location {
    id: Uuid,
    home: PathBuf,
    root: PathBuf,
    folder: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    staging_folder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rename: Option<ProfileRename>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deletion: Option<deletion::Progress>,
}

#[derive(Deserialize, Serialize)]
struct ProfileRename {
    home: PathBuf,
    folder: String,
}

/// Durable identity and in-flight folder move for pre-managed Reliquary enrollments.
/// The profile ID remains the value already used to derive copied-source identities.
#[derive(Deserialize, Serialize)]
struct LegacyRelocation {
    version: u8,
    profile_id: String,
    current_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending: Option<LegacyProfileMove>,
}

#[derive(Deserialize, Serialize)]
struct LegacyProfileMove {
    from_home: PathBuf,
    to_home: PathBuf,
    to_name: String,
}

impl Location {
    fn load(home: &Path) -> Result<Option<Self>> {
        let Some(file) = regular_file(&location_path(home))? else {
            return Ok(None);
        };
        let location: Self = read_state(file)?;
        ensure!(
            location.root.is_absolute(),
            "Invalid Reliquary storage root"
        );
        checked_folder(&location.folder)?;
        if let Some(staging) = &location.staging_folder {
            let name = staging
                .strip_prefix(".enroll-")
                .context("Invalid Reliquary enrollment directory")?;
            checked_folder(name)?;
        }
        if let Some(rename) = &location.rename {
            checked_folder(&rename.folder)?;
        }
        Ok(Some(location))
    }

    fn belongs_to(&self, home: &Path) -> bool {
        self.home == home
            || self
                .rename
                .as_ref()
                .is_some_and(|rename| rename.home == home)
    }

    fn profile_directory(&self) -> PathBuf {
        self.root.join(&self.folder)
    }

    fn profile_id(&self) -> String {
        self.id.simple().to_string()
    }
}

fn location_path(home: &Path) -> PathBuf {
    home.join("plugin-data").join(LOCATION)
}

fn legacy_relocation_path(directory: &Path) -> PathBuf {
    directory.join(LEGACY_RELOCATION)
}

/// Keep the lock outside the profile folder so it survives the Windows rename.
/// Both legacy mounts/recovery and profile moves take this lock.
fn legacy_rename_lock(home: &Path) -> Result<Option<File>> {
    // The default profile cannot be renamed, so its legacy enrollment must not
    // acquire a new parent-directory write requirement merely to mount.
    let Some(parent) = home
        .parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == "profiles"))
        .filter(|_| home.file_name().is_none_or(|name| name != "default"))
    else {
        return Ok(None);
    };
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(parent.join(".reliquary-legacy-renames.lock"))?;
    file.lock()?;
    Ok(Some(file))
}

fn load_legacy_relocation(directory: &Path) -> Result<Option<LegacyRelocation>> {
    let Some(file) = regular_file(&legacy_relocation_path(directory))? else {
        return Ok(None);
    };
    let relocation: LegacyRelocation = read_state(file)?;
    ensure!(
        relocation.version == 1
            && !relocation.profile_id.is_empty()
            && hermes_library::profiles::valid_name(&relocation.current_name),
        "Invalid legacy Reliquary relocation record"
    );
    if let Some(pending) = &relocation.pending {
        ensure!(
            pending.from_home.is_absolute()
                && pending.to_home.is_absolute()
                && hermes_library::profiles::valid_name(&pending.to_name)
                && pending.to_name != relocation.current_name,
            "Invalid legacy Reliquary relocation intent"
        );
    }
    Ok(Some(relocation))
}

fn legacy_profile_id(directory: &Path, requested_name: &str) -> Result<String> {
    match load_legacy_relocation(directory)? {
        Some(relocation) => {
            ensure!(
                relocation.pending.is_none() && relocation.current_name == requested_name,
                "Legacy Reliquary profile rename requires recovery"
            );
            Ok(relocation.profile_id)
        }
        None => Ok(requested_name.to_owned()),
    }
}

/// Recover a folder rename before legacy owner paths are opened. The intent moves
/// with the profile directory, so it can finish an interrupted post-rename path update.
fn recover_legacy_relocation_locked(home: &Path, requested_name: &str) -> Result<()> {
    let directory = home.join("reliquary");
    let Some(mut relocation) = load_legacy_relocation(&directory)? else {
        return Ok(());
    };
    let Some(pending) = relocation.pending.take() else {
        ensure!(
            relocation.current_name == requested_name,
            "Legacy Reliquary enrollment belongs to a different profile name"
        );
        return Ok(());
    };
    let current_home = fs::canonicalize(home)?;
    let from_home = normalized_absolute_path(&pending.from_home)?;
    let to_home = normalized_absolute_path(&pending.to_home)?;
    if current_home == from_home && requested_name == relocation.current_name {
        ensure!(
            !to_home.exists(),
            "Legacy Reliquary rename destination already exists"
        );
        let _service = lease(&directory, "service.lock")?;
        // The folder move did not happen; retain any older identity mapping and
        // discard only this incomplete intent.
        save_state(&legacy_relocation_path(&directory), &relocation, true)?;
        return Ok(());
    }
    ensure!(
        current_home == to_home && requested_name == pending.to_name,
        "Legacy Reliquary relocation does not match this profile"
    );
    let _service = lease(&directory, "service.lock")?;
    let mut topology = Topology::load(&directory, &relocation.profile_id, false)?;
    relocate_legacy_owner_paths(&mut topology, &from_home, &to_home);
    save_state(&directory.join(TOPOLOGY), &topology, true)?;
    relocation.current_name = pending.to_name;
    relocation.pending = None;
    save_state(&legacy_relocation_path(&directory), &relocation, true)
}

fn normalized_absolute_path(path: &Path) -> Result<PathBuf> {
    ensure!(
        path.is_absolute(),
        "Expected an absolute legacy profile path"
    );
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                ensure!(
                    normalized.pop() && normalized.is_absolute(),
                    "Legacy profile path escapes its filesystem root"
                );
            }
        }
    }
    ensure!(
        normalized.is_absolute(),
        "Expected an absolute legacy profile path"
    );
    Ok(normalized)
}

fn relocate_legacy_owner_paths(topology: &mut Topology, from: &Path, to: &Path) {
    let from = legacy_comparison_path(from);
    for owner in topology.owners.values_mut() {
        let Ok(path) = normalized_absolute_path(&owner.path) else {
            continue;
        };
        if let Ok(relative) = legacy_comparison_path(&path).strip_prefix(&from) {
            if !relative.as_os_str().is_empty() {
                owner.path = to.join(relative);
            }
        }
    }
}

fn legacy_comparison_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    if let Some(text) = path.to_str() {
        if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{unc}"));
        }
        if let Some(dos) = text.strip_prefix(r"\\?\")
            && Path::new(dos).is_absolute()
        {
            return PathBuf::from(dos);
        }
    }
    path.to_owned()
}

fn checked_folder(name: &str) -> Result<()> {
    ensure!(
        !name.starts_with('.')
            && Path::new(name)
                .components()
                .eq([Component::Normal(name.as_ref())]),
        "Invalid Reliquary profile directory"
    );
    Ok(())
}

fn lease(directory: &Path, name: &str) -> Result<File> {
    open_lock(directory, name, false)
}

fn lock_root(directory: &Path) -> Result<File> {
    open_lock(directory, ".profiles.lock", true)
}

fn open_lock(directory: &Path, name: &str, wait: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let path = directory.join(name);
    let _ = regular_file(&path)?;
    let file = options.open(path)?;
    if wait {
        file.lock()?;
    } else {
        file.try_lock()
            .with_context(|| format!("Reliquary storage is in use ({name})"))?;
    }
    Ok(file)
}

fn check_directory(path: &Path) -> Result<()> {
    ensure!(
        path.is_dir() && fs::canonicalize(path)? == path,
        "Unexpected Reliquary directory {}",
        path.display()
    );
    Ok(())
}

pub(super) fn open(library: &Library, name: &str, enroll: bool) -> Result<Option<EmbeddedMemory>> {
    open_at(library, name, enroll, &data_root(library.root()))
}

fn open_at(
    library: &Library,
    name: &str,
    enroll: bool,
    root: &Path,
) -> Result<Option<EmbeddedMemory>> {
    let home = library.root();
    let managed_location = Location::load(home)?.filter(|location| location.belongs_to(home));
    let legacy_enrollment = regular_file(&home.join("reliquary").join(ENROLLMENT))?.is_some();
    let _legacy_rename_lock = if managed_location.is_none() && legacy_enrollment {
        let lock = legacy_rename_lock(home)?;
        ensure!(
            lock.is_some()
                || load_legacy_relocation(&home.join("reliquary"))?
                    .is_none_or(|relocation| relocation.pending.is_none()),
            "Default legacy Reliquary profile cannot recover a folder relocation"
        );
        recover_legacy_relocation_locked(home, name)?;
        lock
    } else {
        None
    };
    let mut location = match Location::load(home)?.filter(|location| location.belongs_to(home)) {
        Some(location) => location,
        None => {
            if let Some(existing) = legacy(home, name)? {
                return Ok(Some(existing));
            }
            if !enroll {
                return Ok(None);
            }
            provision(home, name, root)?
        }
    };
    let projects = library.projects()?;
    let (directory, topology, service) =
        sync_managed_projects(home, name, &mut location, &projects)?;
    let enrollment = read_state(
        regular_file(&directory.join(ENROLLMENT))?
            .context("Missing Reliquary capture enrollment")?,
    )?;
    EmbeddedMemory::mount(
        home.to_owned(),
        directory,
        enrollment,
        topology,
        Some(projects),
        service,
    )
    .map(Some)
}

pub(super) fn sync_inactive_projects(library: &Library, name: &str) -> Result<()> {
    if let Some(mut location) =
        Location::load(library.root())?.filter(|location| location.belongs_to(library.root()))
    {
        sync_managed_projects(library.root(), name, &mut location, &library.projects()?)?;
    }
    Ok(())
}

fn sync_managed_projects(
    home: &Path,
    name: &str,
    location: &mut Location,
    projects: &[Project],
) -> Result<(PathBuf, Topology, File)> {
    check_directory(&location.root)?;
    let _root_lock = lock_root(&location.root)?;
    ensure!(
        location.deletion.is_none(),
        "Retry deleting this profile to finish Reliquary cleanup"
    );
    finish_enrollment(location)?;
    finish_profile_rename(home, name, location)?;
    let directory = location.profile_directory();
    check_directory(&directory)?;
    let service = lease(&directory, "service.lock")?;
    let mut topology = Topology::load(&directory, &location.profile_id(), true)?;
    reconcile_projects(&directory, &mut topology, projects)?;
    Ok((directory, topology, service))
}

pub(super) fn legacy(home: &Path, name: &str) -> Result<Option<EmbeddedMemory>> {
    let home = fs::canonicalize(home)?;
    let directory = home.join("reliquary");
    let Some(enrollment) = regular_file(&directory.join(ENROLLMENT))? else {
        return Ok(None);
    };
    check_directory(&directory)?;
    let service = lease(&directory, "service.lock")?;
    let profile_id = legacy_profile_id(&directory, name)?;
    let topology = Topology::load(&directory, &profile_id, false)?;
    EmbeddedMemory::mount(
        home,
        directory,
        read_state(enrollment)?,
        topology,
        None,
        service,
    )
    .map(Some)
}

/// The store lives in the Hermes data area, shared by its profiles. Files beside the executable
/// are lost when an update replaces the app folder. Plugin names cannot contain `_`, so no
/// plugin's own data folder has this name.
fn data_root(home: &Path) -> PathBuf {
    home.parent()
        .filter(|parent| parent.file_name().is_some_and(|part| part == "profiles"))
        .and_then(Path::parent)
        .unwrap_or(home)
        .join("plugin-data")
        .join("reliquary_store")
}

fn open_root(root: &Path) -> Result<(PathBuf, File)> {
    ensure_directory(root)?;
    let directory = fs::canonicalize(root)?;
    let lock = lock_root(&directory)?;
    Ok((directory, lock))
}

fn provision(home: &Path, name: &str, root: &Path) -> Result<Location> {
    ensure_directory(&home.join("plugin-data"))?;
    ensure_directory(root.parent().context("Reliquary storage has no parent directory")?)?;
    let (root, _root_lock) = open_root(root)?;
    let replace_locator = match Location::load(home)? {
        Some(location) if location.belongs_to(home) => return Ok(location),
        Some(_) => true,
        None => false,
    };
    let id = Uuid::new_v4();
    let staged = tempfile::Builder::new()
        .prefix(".enroll-")
        .tempdir_in(&root)?;
    let mut location = Location {
        id,
        home: home.to_owned(),
        folder: unique_name(
            &root,
            &portable_stem(name),
            "",
            &id.simple().to_string(),
            None,
        )?,
        staging_folder: Some(
            staged
                .path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        ),
        root,
        rename: None,
        deletion: None,
    };
    // Stage a complete initial enrollment before exposing it to the gateway.
    let general = PathBuf::from("general.rel");
    let private = PathBuf::from(format!("{}.phy", portable_stem(name)));
    let main_rel = Rel::create(staged.path().join(&general))?.owner_id();
    let active_phy = Phylactery::create(staged.path().join(&private))?.owner_id();
    let profile_id = location.profile_id();
    let topology = Topology {
        version: 2,
        profile_id: profile_id.clone(),
        principal_id: profile_id,
        main_rel,
        active_phy,
        projects: BTreeMap::new(),
        owners: BTreeMap::from([
            (
                main_rel,
                RegisteredOwner {
                    kind: OwnerKind::Rel,
                    path: general,
                    pending_path: None,
                },
            ),
            (
                active_phy,
                RegisteredOwner {
                    kind: OwnerKind::Phylactery,
                    path: private,
                    pending_path: None,
                },
            ),
        ]),
        managed: true,
    };
    save_new_state(&staged.path().join(TOPOLOGY), &topology)?;
    save_new_state(
        &staged.path().join(ENROLLMENT),
        &Enrollment {
            version: 1,
            cutoff: now(),
        },
    )?;
    // Profile exports can carry the locator but do not grant access to the original store.
    save_state(&location_path(home), &location, replace_locator)?;
    let _ = staged.keep();
    finish_enrollment(&mut location)?;
    Ok(location)
}

fn finish_enrollment(location: &mut Location) -> Result<()> {
    let Some(staging) = &location.staging_folder else {
        return Ok(());
    };
    let staged = location.root.join(staging);
    let mut directory = location.profile_directory();
    if staged.try_exists()? {
        check_directory(&staged)?;
        Topology::load(&staged, &location.profile_id(), true)?;
        if directory.try_exists()? {
            location.folder = unique_name(
                &location.root,
                &location.folder,
                "",
                &location.profile_id(),
                None,
            )?;
            save_state(&location_path(&location.home), location, true)?;
            directory = location.profile_directory();
        }
        fs::rename(staged, &directory)?;
        sync_directory(&location.root)?;
    }
    check_directory(&directory)?;
    Topology::load(&directory, &location.profile_id(), true)?;
    location.staging_folder = None;
    save_state(&location_path(&location.home), location, true)
}

pub(super) fn rename_profile(library: &Library, from: &str, to: &str) -> Result<Option<String>> {
    ensure!(from != "default", "The default profile can't be renamed");
    let old_home = fs::canonicalize(library.profile_path(from)?)?;
    let _legacy_rename_lock = legacy_rename_lock(&old_home)?;
    recover_legacy_relocation_locked(&old_home, from)?;
    let Some(mut location) =
        Location::load(&old_home)?.filter(|location| location.belongs_to(&old_home))
    else {
        if regular_file(&old_home.join("reliquary").join(ENROLLMENT))?.is_some() {
            return rename_legacy_profile(library, from, to, &old_home);
        }
        library.rename_profile(from, to)?;
        return Ok(None);
    };
    ensure!(
        hermes_library::profiles::valid_name(to),
        "Invalid new profile name"
    );
    check_directory(&location.root)?;
    let _root_lock = lock_root(&location.root)?;
    ensure!(
        location.deletion.is_none(),
        "Retry deleting this profile to finish Reliquary cleanup"
    );
    finish_enrollment(&mut location)?;
    finish_profile_rename(&old_home, from, &mut location)?;
    let directory = location.profile_directory();
    check_directory(&directory)?;
    drop(lease(&directory, "service.lock")?);
    let target = library.profile_path(to)?;
    let folder = unique_name(
        &location.root,
        &portable_stem(to),
        "",
        &location.profile_id(),
        Some(&location.folder),
    )?;
    location.rename = Some(ProfileRename {
        home: target.clone(),
        folder,
    });
    save_state(&location_path(&old_home), &location, true)?;
    library.rename_profile(from, to)?;
    let notice = finish_profile_rename(&target, to, &mut location).err()
        .map(|error| format!("Profile renamed. Reliquary will retry moving its files when the profile opens. {error:#}"));
    Ok(notice)
}

fn rename_legacy_profile(
    library: &Library,
    from: &str,
    to: &str,
    old_home: &Path,
) -> Result<Option<String>> {
    ensure!(
        hermes_library::profiles::valid_name(to) && to != from,
        "Invalid new profile name"
    );
    let target = library.profile_path(to)?;
    let target_home = fs::canonicalize(
        target
            .parent()
            .context("Legacy profile destination has no parent")?,
    )?
    .join(
        target
            .file_name()
            .context("Legacy profile destination has no name")?,
    );
    ensure!(
        !target_home.exists(),
        "A profile with that name already exists"
    );
    let directory = old_home.join("reliquary");
    let service = lease(&directory, "service.lock")?;
    let mut relocation = match load_legacy_relocation(&directory)? {
        Some(relocation) => {
            ensure!(
                relocation.pending.is_none() && relocation.current_name == from,
                "Legacy Reliquary profile rename requires recovery"
            );
            relocation
        }
        None => LegacyRelocation {
            version: 1,
            profile_id: from.to_owned(),
            current_name: from.to_owned(),
            pending: None,
        },
    };
    let mut topology = Topology::load(&directory, &relocation.profile_id, false)?;
    relocation.pending = Some(LegacyProfileMove {
        from_home: old_home.to_owned(),
        to_home: target_home.clone(),
        to_name: to.to_owned(),
    });
    save_state(&legacy_relocation_path(&directory), &relocation, true)?;
    // Windows cannot rename a profile folder while a service-lock handle inside
    // it is open. The stable parent lock fences new opens during this gap.
    drop(service);
    if let Err(error) = library.rename_profile(from, to) {
        let _service = lease(&directory, "service.lock")?;
        relocation.pending = None;
        // If this save fails, the next open clears the intent while the original
        // profile still exists and the destination remains absent.
        let _ = save_state(&legacy_relocation_path(&directory), &relocation, true);
        return Err(error);
    }
    relocate_legacy_owner_paths(&mut topology, old_home, &target_home);
    let target_directory = target_home.join("reliquary");
    let _service = lease(&target_directory, "service.lock")?;
    let result = (|| -> Result<()> {
        save_state(&target_directory.join(TOPOLOGY), &topology, true)?;
        relocation.current_name = to.to_owned();
        relocation.pending = None;
        save_state(
            &legacy_relocation_path(&target_directory),
            &relocation,
            true,
        )
    })();
    Ok(result.err().map(|error| {
        format!(
            "Profile renamed. Reliquary will retry relocating legacy owner paths when the profile opens. {error:#}"
        )
    }))
}

/// The rename intent moves with the Hermes profile. An interrupted move can be
/// completed at the next open, using the same owner and copied-source identities.
fn finish_profile_rename(home: &Path, name: &str, location: &mut Location) -> Result<()> {
    let Some(rename) = &location.rename else {
        return Ok(());
    };
    if home == location.home {
        location.rename = None;
        return save_state(&location_path(home), location, true);
    }
    ensure!(
        home == rename.home,
        "Unexpected destination for Reliquary profile rename"
    );
    ensure!(
        regular_file(&location_path(&location.home))?.is_none(),
        "The original Hermes profile still holds this Reliquary registration"
    );
    let from = location.profile_directory();
    let to = location.root.join(&rename.folder);
    let profile_id = location.profile_id();
    if from != to && from.try_exists()? {
        check_directory(&from)?;
        Topology::load(&from, &profile_id, true)?;
        ensure!(
            !to.try_exists()?,
            "Reliquary profile destination already exists"
        );
        fs::rename(&from, &to)?;
        sync_directory(&location.root)?;
    }
    check_directory(&to)?;
    let mut topology = Topology::load(&to, &profile_id, true)?;
    let phy = topology.active_phy;
    move_owner(
        &to,
        &mut topology,
        phy,
        PathBuf::from(format!("{}.phy", portable_stem(name))),
    )?;
    location.home = home.to_owned();
    location.folder = rename.folder.clone();
    location.rename = None;
    save_state(&location_path(home), location, true)
}

fn managed_path(directory: &Path, path: &Path) -> Result<PathBuf> {
    let components: Vec<_> = path.components().collect();
    ensure!(
        matches!(components.as_slice(), [Component::Normal(_)])
            || matches!(components.as_slice(), [Component::Normal(parent), Component::Normal(_)] if *parent == "projects"),
        "Invalid relative Reliquary owner path"
    );
    let path = directory.join(path);
    check_directory(
        path.parent()
            .context("Reliquary owner path has no parent")?,
    )?;
    Ok(path)
}

fn reconcile_projects(
    directory: &Path,
    topology: &mut Topology,
    projects: &[Project],
) -> Result<()> {
    let assignments = directory.join("projects");
    ensure_directory(&assignments)?;
    let pending: Vec<_> = topology
        .owners
        .iter()
        .filter_map(|(id, owner)| owner.pending_path.as_ref().map(|_| *id))
        .collect();
    for id in pending {
        finish_owner_move(directory, topology, id)?;
    }
    for project in projects {
        let current = topology.projects.get(&project.id).map(|owner| owner.rel_id);
        let current_name = current
            .and_then(|id| topology.owners.get(&id))
            .and_then(|owner| owner.path.file_name()?.to_str());
        let desired = unique_name(
            &assignments,
            &portable_stem(&project.name),
            ".rel",
            &project.id,
            current_name,
        )?;
        let desired = PathBuf::from("projects").join(desired);
        if let Some(id) = current {
            move_owner(directory, topology, id, desired)?;
        } else {
            let staged =
                PathBuf::from("projects").join(format!(".create-{}.rel", Uuid::new_v4().simple()));
            let id = Rel::create(directory.join(&staged))?.owner_id();
            topology
                .projects
                .insert(project.id.clone(), ProjectOwner { rel_id: id });
            topology.owners.insert(
                id,
                RegisteredOwner {
                    kind: OwnerKind::Rel,
                    path: staged,
                    pending_path: Some(desired),
                },
            );
            save_state(&directory.join(TOPOLOGY), topology, true)?;
            finish_owner_move(directory, topology, id)?;
        }
    }
    Ok(())
}

fn move_owner(
    directory: &Path,
    topology: &mut Topology,
    id: OwnerId,
    desired: PathBuf,
) -> Result<()> {
    finish_owner_move(directory, topology, id)?;
    let owner = topology
        .owners
        .get_mut(&id)
        .context("Missing registered Reliquary owner")?;
    if owner.path != desired {
        owner.pending_path = Some(desired);
        save_state(&directory.join(TOPOLOGY), topology, true)?;
        finish_owner_move(directory, topology, id)?;
    }
    Ok(())
}

fn finish_owner_move(directory: &Path, topology: &mut Topology, id: OwnerId) -> Result<()> {
    let owner = topology
        .owners
        .get_mut(&id)
        .context("Missing registered Reliquary owner")?;
    let Some(target) = &owner.pending_path else {
        return Ok(());
    };
    let from = managed_path(directory, &owner.path)?;
    let to = managed_path(directory, target)?;
    let old_exists = regular_file(&from)?.is_some();
    let new_exists = regular_file(&to)?.is_some();
    if old_exists {
        ensure!(
            !new_exists || fs::canonicalize(&from)? == fs::canonicalize(&to)?,
            "Reliquary owner destination already exists"
        );
        fs::rename(&from, &to)?;
        sync_directory(to.parent().context("Reliquary owner path has no parent")?)?;
    } else {
        ensure!(new_exists, "Missing Reliquary owner during rename");
    }
    let actual = match owner.kind {
        OwnerKind::Rel => Rel::open(&to)?.owner_id(),
        OwnerKind::Phylactery => Phylactery::open(&to)?.owner_id(),
    };
    ensure!(
        id == actual,
        "Reliquary owner identity changed during rename"
    );
    owner.path = target.clone();
    owner.pending_path = None;
    save_state(&directory.join(TOPOLOGY), topology, true)
}

fn sync_directory(_directory: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(_directory)?.sync_all()?;
    Ok(())
}

fn portable_stem(name: &str) -> String {
    let mut stem = String::new();
    for ch in name.chars() {
        if stem.len() + ch.len_utf8() > 100 {
            break;
        }
        stem.push(if ch.is_control() || r#"<>:"/\|?*"#.contains(ch) {
            '_'
        } else {
            ch
        });
    }
    let stem = stem.trim_matches([' ', '.']);
    let first = stem
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if stem.is_empty() {
        "unnamed".into()
    } else if matches!(
        first.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        first
            .strip_prefix(prefix)
            .is_some_and(|suffix| "123456789¹²³".contains(suffix) && suffix.chars().count() == 1)
    }) {
        format!("_{stem}")
    } else {
        stem.into()
    }
}

fn unique_name(
    directory: &Path,
    stem: &str,
    extension: &str,
    id: &str,
    current: Option<&str>,
) -> Result<String> {
    let plain = format!("{stem}{extension}");
    let suffixed = format!("{stem}-{id}{extension}");
    if current == Some(&suffixed) {
        return Ok(suffixed);
    }
    let occupied = fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<Vec<_>>>()?;
    let available = |candidate: &str| {
        !occupied.iter().any(|name| {
            let name = name.to_string_lossy();
            Some(name.as_ref()) != current && name.to_lowercase() == candidate.to_lowercase()
        })
    };
    if available(&plain) {
        return Ok(plain);
    }
    ensure!(
        available(&suffixed),
        "Reliquary owner name is already in use"
    );
    Ok(suffixed)
}
