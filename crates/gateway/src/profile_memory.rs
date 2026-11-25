//! Select and run the profile's stock memory plugin.
use super::*;
use hermes_engine::settings::{MemoryProvider, ProfileSettings};
use hermes_library::memory_plugin::{MemoryStore, MemoryTurn};

pub(super) struct NativeSlot {
    library: Library,
    profile_name: String,
    provider: Config,
    mounted: StdMutex<Option<Arc<dyn MemoryStore>>>,
    active: std::sync::atomic::AtomicBool,
    pub(super) wake: tokio::sync::Notify,
}

impl NativeSlot {
    pub(super) fn new(library: Library, profile: &str, provider: Config) -> Arc<Self> {
        Arc::new(Self {
            library,
            profile_name: profile.to_owned(),
            provider,
            mounted: StdMutex::new(None),
            active: false.into(),
            wake: tokio::sync::Notify::new(),
        })
    }

    pub(super) fn active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    fn open_selected(
        &self,
        mounted: &mut Option<Arc<dyn MemoryStore>>,
        refresh_runtime: bool,
    ) -> Result<()> {
        let Some(plugin) = stock_plugins::memory_plugin() else {
            return Ok(());
        };
        let configured = ProfileSettings::load(self.library.root())?.memory_provider;
        let opening = mounted.is_none();
        if !matches!(configured, MemoryProvider::Auto | MemoryProvider::Reliquary) {
            self.unmount(mounted);
        } else if mounted.is_none() {
            // Enrollment replaces the profile's Markdown notes and starts copying its chats, so
            // only an explicit choice enrolls. Automatic uses an enrollment that already exists.
            let enroll = configured == MemoryProvider::Reliquary;
            *mounted = plugin.open(&self.library, &self.profile_name, enroll)?;
            self.active.store(mounted.is_some(), Ordering::Release);
            if mounted.is_some() {
                self.wake.notify_one();
            }
        }
        if (opening || refresh_runtime)
            && let Some(store) = mounted.as_ref()
            && let Err(error) = store.configure_profile_runtime(&self.provider)
        {
            if refresh_runtime {
                return Err(error);
            }
            eprintln!("Could not configure the profile's managed memory runtime: {error:#}");
        }
        Ok(())
    }

    /// Always run on the blocking pool. Joins can wait for hosted model calls.
    pub(super) fn with_selected<T>(
        &self,
        operation: impl FnOnce(&Arc<dyn MemoryStore>) -> T,
    ) -> Option<T> {
        let mut mounted = lock(&self.mounted);
        if let Err(error) = self.open_selected(&mut mounted, false) {
            eprintln!("Could not open profile memory: {error:#}");
            return None;
        }
        mounted.as_ref().map(operation)
    }

    /// Refresh assignments once outstanding tool queries release their store.
    pub(super) fn refresh_projects(&self, sync_inactive: bool) -> Result<bool> {
        let mut mounted = lock(&self.mounted);
        self.open_selected(&mut mounted, sync_inactive)?;
        if let Some(store) = mounted.as_ref()
            && store.projects_changed(&self.library.projects()?)
        {
            if Arc::strong_count(store) > 1 {
                return Ok(false);
            }
            self.unmount(&mut mounted);
            self.open_selected(&mut mounted, false)?;
        }
        if sync_inactive
            && mounted.is_none()
            && let Some(plugin) = stock_plugins::memory_plugin()
        {
            plugin.sync_inactive_projects(&self.library, &self.profile_name)?;
        }
        Ok(true)
    }

    fn unmount(&self, mounted: &mut Option<Arc<dyn MemoryStore>>) {
        self.active.store(false, Ordering::Release);
        if let Some(memory) = mounted.take() {
            memory.stop_processing();
        }
    }

    pub(super) fn shutdown(&self) {
        self.unmount(&mut lock(&self.mounted));
    }
}

#[derive(Default)]
pub(super) struct Selected {
    native: Option<Arc<dyn MemoryStore>>,
}

impl Shared {
    pub(super) async fn user_archive_search(&self, params: &Value, peer: &Peer) -> RpcResult {
        if !peer.local_native {
            return Err(RpcError::new(
                4403,
                "permission_denied",
                "Archive commands require the private profile client",
            ));
        }
        if params.as_object().is_none_or(|fields| {
            fields.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "session_id" | "project_id" | "query" | "include_related"
                )
            }) || fields
                .get("include_related")
                .is_some_and(|value| !value.is_boolean())
        }) {
            return Err(RpcError::invalid("Invalid Archive search options"));
        }
        let query = required_string(params, "query")?.trim().to_owned();
        if query.is_empty()
            || query.len() > 4096
            || hermes_engine::memory_capture::contains_inline_data(&query)
        {
            return Err(RpcError::invalid("Enter a bounded Archive search query"));
        }
        let requested_project = optional_string(params, "project_id")?;
        let (project_id, config) = if optional_string(params, "session_id")?.is_some() {
            if requested_project.is_some() {
                return Err(RpcError::invalid(
                    "A session supplies its own Archive scope",
                ));
            }
            let session = self.resolve(params).await?;
            let project_id = lock(&session.summary).project_id.clone();
            let config = lock(&session.config).clone();
            (project_id, config)
        } else if params.get("project_id").is_some() {
            (requested_project.map(str::to_owned), self.provider.clone())
        } else {
            (
                self.library
                    .selected_project()
                    .map_err(RpcError::internal)?
                    .map(|project| project.id),
                self.provider.clone(),
            )
        };
        let include_related = params["include_related"].as_bool().unwrap_or(false);
        let slot = self.memory.clone();
        let sources = memory_capture::CaptureSources(self.library.clone());
        tokio::task::spawn_blocking(move || {
            slot.refresh_projects(false)?;
            let projects = slot.library.projects()?;
            let results = slot
                .with_selected(|store| {
                    store.search_user_archive(
                        &sources,
                        &projects,
                        project_id.as_deref(),
                        include_related,
                        &query,
                        &config,
                    )
                })
                .context("The Reliquary stock plugin is unavailable for this profile")??;
            Ok::<_, anyhow::Error>(json!({"hits":results}))
        })
        .await
        .map_err(RpcError::internal)?
        .map_err(RpcError::internal)
    }

    pub(super) async fn selected_memory(&self) -> Selected {
        if !stock_plugins::has_memory_plugin() {
            return Selected::default();
        }
        let slot = self.memory.clone();
        let native = tokio::task::spawn_blocking(move || slot.with_selected(Arc::clone))
            .await
            .ok()
            .flatten();
        Selected { native }
    }

    pub(super) async fn refresh_memory(&self) -> Result<Selected> {
        if !stock_plugins::has_memory_plugin() {
            return Ok(Selected::default());
        }
        let slot = self.memory.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(error) = slot.refresh_projects(false) {
                if ProfileSettings::load(slot.library.root())?.memory_provider
                    == MemoryProvider::Auto
                {
                    eprintln!("Could not refresh profile memory: {error:#}");
                    return Ok(Selected::default());
                }
                return Err(error);
            }
            Ok(Selected {
                native: slot.with_selected(Arc::clone),
            })
        })
        .await?
    }

    pub(super) async fn sync_memory_plugin(&self, peer: &Peer) -> RpcResult {
        if !peer.local_native {
            return Err(RpcError::new(
                4403,
                "permission_denied",
                "Plugin settings require the private profile client",
            ));
        }
        let slot = self.memory.clone();
        tokio::task::spawn_blocking(move || {
            let projects_ready = slot.refresh_projects(true)?;
            Ok::<_, anyhow::Error>(
                json!({"enrolled":slot.active(), "projects_ready":projects_ready}),
            )
        })
        .await
        .map_err(RpcError::internal)?
        .map_err(RpcError::internal)
    }
}

impl Selected {
    pub(super) fn native(&self) -> bool {
        self.native.is_some()
    }

    pub(super) fn binding(&self, summary: &SessionSummary) -> Result<Option<CaptureBinding>> {
        match &self.native {
            Some(store) => store.binding_for_new_session(summary),
            None => Ok(None),
        }
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
                || memory_capture::source_ended(&session.dir)
                || !admission.attachments.is_empty()
                || !hermes_engine::references::parse(&admission.text).is_empty()
            {
                return None;
            }
            let binding = lock(&session.settings).memory_binding.clone()?;
            Some(
                memory.clone().query(
                    Arc::new(memory_capture::CaptureSources(library.clone())),
                    binding,
                    MemoryTurn {
                        session_id: summary.id,
                        admission_id: admission.id.clone(),
                        admitted_ns: admission.admitted_ns,
                        calendar_zone: ProfileSettings::load(library.root())
                            .ok()
                            .map(|settings| settings.timezone)
                            .filter(|zone| !zone.is_empty())
                            .unwrap_or_else(|| "UTC".into()),
                    },
                ),
            )
        });
        target.set_native_memory(self.native(), query);
    }
}
