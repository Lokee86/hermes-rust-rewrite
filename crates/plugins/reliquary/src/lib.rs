//! Stock Reliquary memory plugin for Hermes profiles.
#![cfg(feature = "native")]

mod hosted;
mod memory;
mod query;
mod storage;

use anyhow::Result;
use hermes_library::{
    Library,
    memory_plugin::{MemoryPlugin, MemoryStore},
};
use std::sync::Arc;

pub struct Reliquary;

impl MemoryPlugin for Reliquary {
    fn open(
        &self,
        library: &Library,
        profile_name: &str,
        enroll: bool,
    ) -> Result<Option<Arc<dyn MemoryStore>>> {
        Ok(storage::open(library, profile_name, enroll)?
            .map(|memory| Arc::new(memory) as Arc<dyn MemoryStore>))
    }

    fn sync_inactive_projects(&self, library: &Library, profile_name: &str) -> Result<()> {
        storage::sync_inactive_projects(library, profile_name)
    }

    fn rename_profile(&self, library: &Library, from: &str, to: &str) -> Result<Option<String>> {
        storage::rename_profile(library, from, to)
    }
}
