//! Remove a profile's managed copies before the host deletes its locator.
use super::{
    ENROLLMENT, Location, Topology, check_directory, data_root, finish_enrollment,
    finish_profile_rename, lease, legacy_rename_lock, location_path, lock_root, regular_file,
    save_state, sync_directory,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{fs, io::ErrorKind, path::Path};

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Progress {
    Detach,
    Purge,
}

pub(crate) fn purge_profile_data(home: &Path, name: &str) -> Result<()> {
    // Legacy profile-local storage goes away with the home. A legacy topology
    // can register external owners, which are outside this deletion.
    let _legacy_moves = legacy_rename_lock(home)?;
    let legacy = home.join("reliquary");
    if regular_file(&legacy.join(ENROLLMENT))?.is_some() {
        check_directory(&legacy)?;
        drop(lease(&legacy, "service.lock")?);
    }

    let Some(location) = Location::load(home)?.filter(|location| location.belongs_to(home)) else {
        return Ok(());
    };
    check_directory(&home.join("plugin-data"))?;
    // Older builds could put their managed store beside the installation.
    ensure!(
        location.root == data_root(home)
            || location
                .root
                .file_name()
                .is_some_and(|name| name == "reliquary"),
        "Unexpected Reliquary storage root"
    );
    if !directory_present(&location.root)? {
        return Ok(());
    }
    let _root_lock = lock_root(&location.root)?;
    let mut current = Location::load(home)?.context("Missing Reliquary storage registration")?;
    ensure!(
        current.id == location.id && current.root == location.root && current.belongs_to(home),
        "Reliquary storage registration changed during deletion"
    );
    purge_managed(home, name, &mut current)
}

fn purge_managed(home: &Path, name: &str, location: &mut Location) -> Result<()> {
    if location.deletion.is_none() {
        finish_enrollment(location)?;
        finish_profile_rename(home, name, location)?;
    }
    let detached = location
        .root
        .join(format!(".delete-{}", location.profile_id()));
    if location.deletion != Some(Progress::Purge) {
        if directory_present(&detached)? {
            ensure!(
                location.deletion == Some(Progress::Detach),
                "Unexpected Reliquary deletion directory"
            );
            Topology::load(&detached, &location.profile_id(), true)?;
        } else {
            let directory = location.profile_directory();
            if !directory_present(&directory)? {
                return Ok(());
            }
            Topology::load(&directory, &location.profile_id(), true)?;
            // The root lock fences new mounts. Close the lease before moving the
            // directory so Windows can rename it.
            drop(lease(&directory, "service.lock")?);
            location.deletion = Some(Progress::Detach);
            save_state(&location_path(home), location, true)?;
            fs::rename(&directory, &detached).context("Could not detach the Reliquary profile")?;
            sync_directory(&location.root)?;
        }
        // Persist proof of the verified move before recursive deletion can remove
        // the topology. A retry can finish a partially removed directory.
        location.deletion = Some(Progress::Purge);
        save_state(&location_path(home), location, true)?;
    }
    if directory_present(&detached)? {
        fs::remove_dir_all(&detached).context("Could not remove the profile's Reliquary data")?;
        sync_directory(&location.root)?;
    }
    Ok(())
}

fn directory_present(path: &Path) -> Result<bool> {
    match path.symlink_metadata() {
        Ok(metadata) => {
            ensure!(
                metadata.is_dir() && !metadata.is_symlink(),
                "Expected a regular Reliquary storage directory"
            );
            check_directory(path)?;
            Ok(true)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}
