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

pub fn rename_profile(library: &Library, from: &str, to: &str) -> Result<Option<String>> {
    ensure!(from != "default", "The default profile can't be renamed");
    let home = library.profile_path(from)?;
    let _renames = owner::lock_profile_renames(&home)?;
    let active_check = owner::try_local_owner_lock(&home)?
        .context("Stop this profile's gateway before renaming the profile")?;
    drop(active_check);
    let Some(plugin) = memory_plugin() else {
        library.rename_profile(from, to)?;
        return Ok(None);
    };
    plugin.rename_profile(library, from, to)
}
