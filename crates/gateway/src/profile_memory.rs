//! Resolve profile selection before binding, querying or capturing a chat.
use super::*;

const ENDED: &str = "gateway-memory-ended";

/// An archive is permanent for memory capture even if the chat is later restored.
/// Keep this fence in the native conversation while its provider is turned off.
#[cfg(feature = "reliquary")]
pub(super) fn source_ended(directory: &Path) -> bool {
    directory.join(ENDED).try_exists().unwrap_or(true)
}

impl Shared {
    pub(super) async fn memory_source_terminal(self: &Arc<Self>, session_id: &str) -> Result<()> {
        let shared = self.clone();
        let session_id = session_id.to_owned();
        tokio::task::spawn_blocking(move || -> Result<()> {
            if let Ok(directory) = shared.library.session_dir(&session_id)
                && Settings::read(&directory)?.memory_binding.is_some()
            {
                let path = directory.join(ENDED);
                match File::create_new(path) {
                    Ok(file) => file.sync_all()?,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
                    Err(error) => return Err(error.into()),
                }
            }
            #[cfg(feature = "reliquary")]
            shared
                .with_selected_memory(|memory| memory.terminal_source(&session_id))
                .transpose()?;
            Ok(())
        })
        .await?
    }
}

#[cfg(feature = "reliquary")]
impl Shared {
    /// Serialize a native operation against deselection, including cancellation of
    /// active processing jobs. Explicit alternative providers never open a REL.
    pub(super) fn with_selected_memory<T>(
        &self,
        operation: impl FnOnce(&Arc<reliquary::EmbeddedMemory>) -> T,
    ) -> Option<T> {
        use hermes_engine::settings::{MemoryProvider, ProfileSettings};
        let mut mounted = lock(&self.memory);
        let configured =
            ProfileSettings::load(self.library.root()).map(|settings| settings.memory_provider);
        if !matches!(
            configured,
            Ok(MemoryProvider::Auto | MemoryProvider::Reliquary)
        ) {
            if let Some(previous) = mounted.take() {
                previous.stop_processing();
            }
            return None;
        }
        if mounted.is_none() {
            match reliquary::EmbeddedMemory::open(self.library.root(), &self.profile_name) {
                Ok(Some(memory)) => {
                    *mounted = Some(Arc::new(memory));
                    self.memory_wake.notify_one();
                }
                Ok(None) => return None,
                Err(error) => {
                    eprintln!("Could not open profile memory: {error:#}");
                    return None;
                }
            }
        }
        mounted.as_ref().map(operation)
    }

    pub(super) fn native_memory_available(&self) -> bool {
        self.with_selected_memory(|_| ()).is_some()
    }

    pub(super) fn new_memory_binding(&self, summary: &SessionSummary) -> Option<CaptureBinding> {
        self.with_selected_memory(|memory| memory.binding_for_new_session(summary))
            .flatten()
    }

    pub(super) fn configure_turn_memory(
        &self,
        target: &mut Session,
        session: &LiveSession,
        admission: &mut Receipt,
    ) {
        let summary = lock(&session.summary).clone();
        if summary.bot.is_some() {
            target.set_native_memory(false, None);
            return;
        }
        let binding = lock(&session.settings).memory_binding.clone();
        let available = self.with_selected_memory(|memory| {
            if !admission.ordinary_chat
                || summary.archived
                || source_ended(&session.dir)
                || !admission.attachments.is_empty()
                || !hermes_engine::references::parse(&admission.text).is_empty()
            {
                return None;
            }
            let query = reliquary_query::GatewayMemoryQuery::new(
                memory.clone(),
                self.library.clone(),
                binding?,
                summary.id.clone(),
                admission.id.clone(),
            );
            Some(Arc::new(query) as Arc<dyn hermes_engine::native_memory::MemoryQuery>)
        });
        // A queued turn must be eligible at both admission and execution. This
        // in-memory snapshot also fences a switch back before the active turn commits.
        admission.ordinary_chat &= available.is_some();
        target.set_native_memory(available.is_some(), available.flatten());
    }
}

#[cfg(not(feature = "reliquary"))]
impl Shared {
    pub(super) fn native_memory_available(&self) -> bool {
        false
    }
    pub(super) fn new_memory_binding(&self, _: &SessionSummary) -> Option<CaptureBinding> {
        None
    }
    pub(super) fn configure_turn_memory(
        &self,
        target: &mut Session,
        _: &LiveSession,
        _: &mut Receipt,
    ) {
        target.set_native_memory(false, None);
    }
}
