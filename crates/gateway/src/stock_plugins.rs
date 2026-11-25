//! Native stock plugins linked into this installation.

use crate::owner;
use anyhow::{Context, Result, ensure};
use hermes_library::{Library, memory_plugin::MemoryPlugin};

pub fn has_memory_plugin() -> bool {
    memory_plugin().is_some()
}

pub(super) fn memory_plugin() -> Option<&'static dyn MemoryPlugin> {
    #[cfg(feature = "reliquary")]
    return Some(&hermes_reliquary_plugin::Reliquary);
    #[cfg(not(feature = "reliquary"))]
    None
}

pub fn delete_profile(library: &Library, name: &str) -> Result<()> {
    let home = library.profile_path_for_deletion(name)?;
    let _profiles = owner::lock_profile_renames(&home)?;
    let _installation = hermes_engine::plugins::lock_profile_plugin_deletion(&home)?;
    let active_check = owner::try_local_owner_lock(&home)?
        .context("Stop this profile's gateway before deleting the profile")?;
    // Startup takes the parent fence, so the owner file can be closed before
    // recursively deleting the profile on Windows.
    drop(active_check);
    library.delete_profile_with_cleanup(name, |home| purge_profile_data(home, name))
}

pub(crate) fn delete_bot(library: &Library, name: &str) -> Result<hermes_library::BotDeletion> {
    library.delete_bot_with_cleanup(name, |home| purge_profile_data(home, name))
}

pub(crate) fn purge_profile_data(home: &std::path::Path, name: &str) -> Result<()> {
    if let Some(plugin) = memory_plugin() {
        return plugin.purge_profile_data(home, name);
    }
    match std::fs::symlink_metadata(home.join("plugin-data").join("reliquary-storage.json")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => {
            anyhow::bail!("Delete this profile using a build that includes its Reliquary plugin")
        }
    }
}

pub fn rename_profile(library: &Library, from: &str, to: &str) -> Result<Option<String>> {
    ensure!(from != "default", "The default profile can't be renamed");
    let home = library.profile_path(from)?;
    let _renames = owner::lock_profile_renames(&home)?;
    let _installation = hermes_engine::plugins::lock_profile_plugin_deletion(&home)?;
    let active_check = owner::try_local_owner_lock(&home)?
        .context("Stop this profile's gateway before renaming the profile")?;
    drop(active_check);
    let Some(plugin) = memory_plugin() else {
        library.rename_profile(from, to)?;
        return Ok(None);
    };
    plugin.rename_profile(library, from, to)
}

#[cfg(all(test, not(feature = "reliquary")))]
mod tests {
    use super::*;

    #[test]
    fn profile_with_reliquary_registration_can_be_retried_in_a_build_with_the_plugin() {
        let temp = tempfile::tempdir().unwrap();
        let library = Library::open(temp.path()).unwrap();
        let profile = library.create_profile("work").unwrap();
        let ordinary = library.create_profile("ordinary").unwrap();
        let plugin_data = profile.path.join("plugin-data");
        std::fs::create_dir(&plugin_data).unwrap();
        std::fs::write(plugin_data.join("reliquary-storage.json"), b"{}").unwrap();

        assert!(delete_profile(&library, &profile.name).is_err());
        assert!(plugin_data.join("reliquary-storage.json").is_file());
        delete_profile(&library, &ordinary.name).unwrap();
        assert!(!ordinary.path.exists());
    }
}
