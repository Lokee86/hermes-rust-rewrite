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
                .memory
                .with_selected(|memory| memory.terminal_source(&session_id))
                .transpose()?;
            Ok(())
        })
        .await?
    }
}

/// Profile-owned native handles can mount files, stop and join processing jobs.
#[cfg(feature = "reliquary")]
pub(super) struct NativeSlot {
    home: PathBuf,
    profile_name: String,
    mounted: StdMutex<Option<Arc<reliquary::EmbeddedMemory>>>,
    active: std::sync::atomic::AtomicBool,
    pub(super) wake: tokio::sync::Notify,
}

#[cfg(feature = "reliquary")]
impl NativeSlot {
    pub(super) fn new(home: &Path, profile: &str) -> Arc<Self> {
        Arc::new(Self {
            home: home.to_owned(),
            profile_name: profile.to_owned(),
            mounted: StdMutex::new(None),
            active: false.into(),
            wake: tokio::sync::Notify::new(),
        })
    }

    pub(super) fn active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// Always run on the blocking pool. Joins can wait for hosted model calls.
    pub(super) fn with_selected<T>(
        &self,
        operation: impl FnOnce(&Arc<reliquary::EmbeddedMemory>) -> T,
    ) -> Option<T> {
        use hermes_engine::settings::{MemoryProvider, ProfileSettings};
        let mut mounted = lock(&self.mounted);
        let configured = ProfileSettings::load(&self.home).map(|settings| settings.memory_provider);
        if !matches!(
            configured,
            Ok(MemoryProvider::Auto | MemoryProvider::Reliquary)
        ) {
            if let Some(previous) = mounted.take() {
                self.active.store(false, Ordering::Release);
                previous.stop_processing();
            }
            return None;
        }
        if mounted.is_none() {
            match reliquary::EmbeddedMemory::open(&self.home, &self.profile_name) {
                Ok(Some(memory)) => {
                    *mounted = Some(Arc::new(memory));
                    self.active.store(true, Ordering::Release);
                    self.wake.notify_one();
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

    pub(super) fn shutdown(&self) {
        self.active.store(false, Ordering::Release);
        if let Some(memory) = lock(&self.mounted).take() {
            memory.stop_processing();
        }
    }
}

#[derive(Default)]
pub(super) struct Selected {
    #[cfg(feature = "reliquary")]
    native: Option<Arc<reliquary::EmbeddedMemory>>,
}

impl Shared {
    pub(super) async fn selected_memory(&self) -> Selected {
        #[cfg(feature = "reliquary")]
        {
            let slot = self.memory.clone();
            let native = tokio::task::spawn_blocking(move || slot.with_selected(Arc::clone))
                .await
                .ok()
                .flatten();
            Selected { native }
        }
        #[cfg(not(feature = "reliquary"))]
        {
            Selected::default()
        }
    }
}

#[cfg(feature = "reliquary")]
impl Selected {
    pub(super) fn native(&self) -> bool {
        self.native.is_some()
    }
    pub(super) fn binding(&self, summary: &SessionSummary) -> Option<CaptureBinding> {
        self.native.as_ref()?.binding_for_new_session(summary)
    }
    pub(super) fn configure_turn(
        &self,
        target: &mut Session,
        library: &Library,
        session: &LiveSession,
        admission: &mut Receipt,
    ) {
        let summary = lock(&session.summary).clone();
        if summary.bot.is_some() {
            target.set_native_memory(false, None);
            return;
        }
        // Require consent both when the client submits and when its turn runs.
        admission.ordinary_chat &= self.native();
        let query = self.native.as_ref().and_then(|memory| {
            if !admission.ordinary_chat
                || summary.archived
                || source_ended(&session.dir)
                || !admission.attachments.is_empty()
                || !hermes_engine::references::parse(&admission.text).is_empty()
            {
                return None;
            }
            let binding = lock(&session.settings).memory_binding.clone()?;
            let query = reliquary_query::GatewayMemoryQuery::new(
                memory.clone(),
                library.clone(),
                binding,
                summary.id,
                admission.id.clone(),
            );
            Some(Arc::new(query) as Arc<dyn hermes_engine::native_memory::MemoryQuery>)
        });
        target.set_native_memory(self.native(), query);
    }
}

#[cfg(not(feature = "reliquary"))]
impl Selected {
    pub(super) fn native(&self) -> bool {
        false
    }
    pub(super) fn binding(&self, _: &SessionSummary) -> Option<CaptureBinding> {
        None
    }
    pub(super) fn configure_turn(
        &self,
        target: &mut Session,
        _: &Library,
        _: &LiveSession,
        _: &mut Receipt,
    ) {
        target.set_native_memory(false, None);
    }
}
