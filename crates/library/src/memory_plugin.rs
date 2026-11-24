//! Contract between the gateway's committed conversation log and a stock memory plugin.

use crate::{Library, Project, SessionSummary};
use anyhow::Result;
use hermes_engine::{
    Config, Turn,
    memory_capture::{CaptureBinding, CaptureReceipt, PendingMemoryCapture},
    native_memory::MemoryQuery,
};
use std::sync::Arc;

pub trait MemoryPlugin: Send + Sync {
    fn open(
        &self,
        library: &Library,
        profile_name: &str,
        enroll: bool,
    ) -> Result<Option<Arc<dyn MemoryStore>>>;

    /// Update an existing installation's project files while capture is disabled.
    fn sync_inactive_projects(&self, library: &Library, profile_name: &str) -> Result<()>;

    /// A warning means the host rename was committed and plugin recovery remains pending.
    fn rename_profile(&self, library: &Library, from: &str, to: &str) -> Result<Option<String>>;
}

pub trait MemoryStore: Send + Sync {
    /// Called before binding a new chat. Existing chats retain their stored binding.
    fn binding_for_new_session(&self, summary: &SessionSummary) -> Result<Option<CaptureBinding>>;
    fn projects_changed(&self, projects: &[Project]) -> bool;
    fn stage(
        &self,
        capture: &PendingMemoryCapture,
        turn: &Turn,
        previous: Option<&CaptureReceipt>,
    ) -> Result<CaptureReceipt>;
    fn ensure_processing(
        &self,
        binding: &CaptureBinding,
        session_id: &str,
        receipt: &CaptureReceipt,
        config: &Config,
    ) -> Result<bool>;
    fn stop_processing(&self);
    fn reconcile_tombstones(&self, summaries: &[SessionSummary]) -> Result<()>;
    fn terminal_source(&self, session_id: &str) -> Result<()>;
    fn search_user_archive(
        &self,
        sources: &dyn MemorySources,
        projects: &[Project],
        project_id: Option<&str>,
        include_related: bool,
        query: &str,
        config: &Config,
    ) -> Result<Vec<UserArchiveHit>>;
    fn query(
        self: Arc<Self>,
        sources: Arc<dyn MemorySources>,
        binding: CaptureBinding,
        turn: MemoryTurn,
    ) -> Arc<dyn MemoryQuery>;
}

#[derive(serde::Serialize)]
pub struct UserArchiveHit {
    pub rel: String,
    pub text: String,
}

pub struct MemoryTurn {
    pub session_id: String,
    pub admission_id: String,
    pub admitted_ns: i64,
    pub calendar_zone: String,
}

/// The host verifies current access and receipts against its own conversation log.
pub trait MemorySources: Send + Sync {
    fn check_session(&self, session_id: &str, binding: &CaptureBinding) -> Result<()>;
    fn copied_message_receipt(
        &self,
        session_id: &str,
        binding: &CaptureBinding,
        message_uid: &str,
    ) -> Result<Option<CaptureReceipt>>;
}
