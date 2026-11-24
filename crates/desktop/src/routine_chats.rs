//! Read-only lookup of routine chats owned and published by the gateway.
use hermes_library::Library;
use std::collections::BTreeMap;

pub(super) fn job_of(library: &Library, session: &str) -> Option<String> {
    let bytes = std::fs::read(library.root().join("routine-chats.json")).ok()?;
    let index: BTreeMap<String, String> = serde_json::from_slice(&bytes).ok()?;
    index.into_iter().find(|(_, id)| id == session).map(|(job, _)| job)
}
