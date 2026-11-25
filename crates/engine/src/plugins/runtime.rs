//! Profile-owned activations of the embedded Python plugin host.
use super::{
    InstallationLock, Installed, config, config_schema, connect_mcp, connect_python_bridge, dir, installed,
    lock_plugin_startup, prefix, python_host, python_packages, valid_name, yaml_field,
};
use crate::{
    Config,
    mcp::{Mcp, Stdio},
    plugin_worker::Worker,
    settings::{MemoryProvider, ProfileSettings},
};
use anyhow::{Context, Result, ensure};
use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, atomic::{AtomicBool, Ordering}},
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, OnceCell};
use tokio_util::sync::CancellationToken;
#[cfg(test)]
use tokio::sync::Notify;

#[derive(Clone, Deserialize, Serialize)]
pub struct PluginCommand {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub args_hint: String,
    pub argument_mode: Option<String>,
}

pub struct PluginStatus {
    pub id: String,
    pub title: String,
    pub commands: Vec<PluginCommand>,
    pub provider: Option<String>,
    pub issue: Option<String>,
    pub unsupported: Vec<String>,
    pub handle: Option<PythonPlugin>,
}

#[derive(Clone, Default, Deserialize)]
struct Registration {
    #[serde(default)]
    commands: Vec<PluginCommand>,
    provider: Option<String>,
    #[serde(default)]
    unsupported: Vec<String>,
}

#[derive(Clone)]
pub struct PythonPlugin(Arc<Activation>);

impl PythonPlugin {
    pub fn id(&self) -> &str {
        &self.0.id
    }

    pub fn same_activation(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        ensure!(self.0.active.load(Ordering::Acquire), "Plugin activation has ended");
        let result = self.0.running.get().context("Plugin activation is still starting")?;
        let running = result.as_ref().map_err(|error| anyhow::anyhow!("{error}"))?;
        running.worker.request(method, params).await
    }
}

#[derive(Clone)]
pub struct PythonMemory {
    pub plugin: PythonPlugin,
    pub session_id: String,
    pub selection_revision: u64,
}

pub struct PluginRuntime {
    root: PathBuf,
    name: String,
    chat: Option<Config>,
    activations: Mutex<ActivationRegistry>,
    closed: AtomicBool,
}

#[derive(Default)]
struct ActivationRegistry {
    active: BTreeMap<String, Arc<Activation>>,
    retired: Vec<Arc<Activation>>,
}

#[derive(PartialEq)]
struct Revision {
    sha: String,
    revision: u64,
    python_package_generation: Option<String>,
    env: Vec<(String, String)>,
}

struct Activation {
    id: String,
    key: Revision,
    active: AtomicBool,
    created: Instant,
    running: OnceCell<Result<Running, String>>,
    lifecycle: Mutex<()>,
    worker: Mutex<Option<Worker>>,
    startup_cancel: CancellationToken,
    #[cfg(test)]
    test_start_gate: Option<Arc<Notify>>,
    #[cfg(test)]
    test_unload_gate: Option<(Arc<Notify>, Arc<Notify>)>,
}

struct Running {
    worker: Worker,
    connection: Mcp,
    described: Mutex<Option<Registration>>,
}

impl Running {
    /// A worker that is busy in a callback keeps its last known registrations.
    async fn describe(&self) -> Result<Registration> {
        let request = self.worker.request("hermes/describe", json!({}));
        match tokio::time::timeout(Duration::from_secs(5), request).await {
            Ok(reply) => {
                let registration: Registration = serde_json::from_value(reply?).context("Invalid plugin registration")?;
                *self.described.lock().await = Some(registration.clone());
                Ok(registration)
            }
            Err(_) => self.described.lock().await.clone().context("The plugin is busy"),
        }
    }
}

struct Candidate {
    installed: Installed,
    exclusive: bool,
    activation: Option<Arc<Activation>>,
}

impl PluginRuntime {
    pub fn new(root: PathBuf, chat: Option<Config>) -> Arc<Self> {
        let name = root.parent().and_then(Path::file_name)
            .filter(|part| *part == "profiles")
            .and_then(|_| root.file_name()).and_then(|name| name.to_str())
            .unwrap_or("default").to_owned();
        Self::for_profile(root, name, chat)
    }

    pub fn for_profile(root: PathBuf, name: String, chat: Option<Config>) -> Arc<Self> {
        Arc::new(Self {
            root,
            name,
            chat,
            activations: Mutex::new(ActivationRegistry::default()),
            closed: AtomicBool::new(false),
        })
    }

    async fn reconcile(&self) -> Vec<Candidate> {
        let mut registry = self.activations.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Vec::new();
        }
        let active = &mut registry.active;
        // A file that cannot be read right now says nothing about what is installed. Keep the
        // running plugins instead of ending their sessions.
        let (Ok(listed), Ok(settings)) = (installed(&self.root), ProfileSettings::load(&self.root)) else {
            return Vec::new();
        };
        let selected = match settings.memory_provider {
            MemoryProvider::Plugin(id) => Some(id),
            _ => None,
        };
        let mut present = BTreeSet::new();
        let mut retired = Vec::new();
        let mut candidates = Vec::new();
        for installed in listed {
            if !valid_name(&installed.name) || !installed.servers.iter().any(|kind| kind == "python") {
                continue;
            }
            let id = installed.name.clone();
            let plugin_root = dir(&self.root).join(&id);
            let exclusive = ["plugin.yaml", "plugin.yml"].iter().find_map(|file| {
                fs::read_to_string(plugin_root.join(file)).ok()
            }).is_some_and(|manifest| yaml_field(&manifest, "kind") == "exclusive");
            let activation = if installed.enabled && (!exclusive || selected.as_ref() == Some(&id)) {
                present.insert(id.clone());
                let key = Revision {
                    sha: installed.sha.clone(),
                    revision: installed.revision,
                    python_package_generation: installed.python_package_generation.clone(),
                    env: python_env(&self.root, &id),
                };
                let reusable = active.get(&id).filter(|slot| slot.key == key && slot.reusable()).cloned();
                Some(reusable.unwrap_or_else(|| {
                    let activation = Arc::new(Activation {
                        id: id.clone(),
                        key,
                        active: true.into(),
                        created: Instant::now(),
                        running: OnceCell::new(),
                        lifecycle: Mutex::new(()),
                        worker: Mutex::new(None),
                        startup_cancel: CancellationToken::new(),
                        #[cfg(test)]
                        test_start_gate: None,
                        #[cfg(test)]
                        test_unload_gate: None,
                    });
                    if let Some(previous) = active.insert(id.clone(), activation.clone()) {
                        retired.push(previous);
                    }
                    activation
                }))
            } else {
                None
            };
            candidates.push(Candidate { installed, exclusive, activation });
        }
        active.retain(|id, activation| {
            if present.contains(id) {
                return true;
            }
            retired.push(activation.clone());
            false
        });
        for slot in &retired {
            slot.revoke();
        }
        registry.retired.extend(retired.iter().cloned());
        drop(registry);
        let _ = self.unload_slots(retired).await;
        candidates.sort_by(|a, b| a.installed.name.cmp(&b.installed.name));
        candidates
    }

    /// Stop the workers of plugins that were disabled, replaced or removed.
    pub async fn retire_stale(&self) {
        if !self.activations.lock().await.active.is_empty() {
            self.reconcile().await;
        }
    }

    pub async fn is_current(&self, plugin: &PythonPlugin) -> bool {
        self.reconcile().await.iter().any(|candidate| {
            candidate.activation.as_ref().is_some_and(|slot| Arc::ptr_eq(slot, &plugin.0))
        })
    }

    pub async fn inventory(&self) -> Vec<PluginStatus> {
        stream::iter(self.reconcile().await).map(|candidate| async move {
            let installed = candidate.installed;
            let mut status = PluginStatus {
                id: installed.name,
                title: installed.title,
                commands: vec![],
                provider: None,
                issue: (!installed.enabled).then(|| "Plugin is disabled".into()),
                unsupported: vec![],
                handle: None,
            };
            if candidate.exclusive {
                status.provider = Some(status.title.clone());
            }
            if let Some(slot) = candidate.activation {
                let result = async {
                    let running = slot.open(self).await?;
                    running.describe().await
                }.await;
                match result {
                    Ok(registration) => {
                        status.commands = registration.commands;
                        status.provider = registration.provider;
                        status.unsupported = registration.unsupported;
                        status.handle = Some(PythonPlugin(slot));
                    }
                    Err(error) => status.issue = Some(error.to_string()),
                }
            }
            status
        }).buffered(4).collect().await
    }

    pub(crate) async fn tool_connections(&self) -> (Vec<Mcp>, Vec<String>) {
        let candidates = self.reconcile().await;
        let (mut live, mut errors) = connect_mcp(&self.root).await;
        for candidate in candidates {
            let Some(slot) = candidate.activation else { continue };
            match slot.open(self).await {
                Ok(running) => {
                    if let Some(connection) = running.connection.plugin_view().await {
                        live.push(connection);
                    }
                }
                Err(error) if !candidate.exclusive => {
                    match slot.bridge(self).await {
                        Ok(bridge) => live.push(bridge),
                        Err(bridge) => errors.push(format!("{}: {error}; {bridge}", candidate.installed.title)),
                    }
                }
                Err(error) => errors.push(format!("{}: {error}", candidate.installed.title)),
            }
        }
        (live, errors)
    }

    pub async fn unload(&self) {
        let _ = self.unload_all().await;
    }

    /// Permanently closes this profile's embedded Python workers. Stale runtime
    /// and activation handles cannot start another worker after this returns.
    pub async fn close(&self) -> Result<()> {
        self.closed.store(true, Ordering::Release);
        self.unload_all()
            .await
            .context("Could not confirm all embedded Python workers exited")
    }

    async fn unload_all(&self) -> Result<()> {
        let retiring = {
            let mut registry = self.activations.lock().await;
            let slots = std::mem::take(&mut registry.active);
            for slot in slots.values() {
                slot.revoke();
            }
            registry.retired.extend(slots.into_values());
            registry.retired.clone()
        };
        self.unload_slots(retiring).await
    }

    async fn unload_slots(&self, slots: Vec<Arc<Activation>>) -> Result<()> {
        let results = stream::iter(slots)
            .map(|slot| async move {
                let result = slot.unload().await;
                (slot, result)
            })
            .buffer_unordered(4)
            .collect::<Vec<_>>()
            .await;
        let mut first_error = None;
        let mut registry = self.activations.lock().await;
        for (slot, result) in results {
            match result {
                Ok(()) => registry.retired.retain(|candidate| !Arc::ptr_eq(candidate, &slot)),
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl Activation {
    fn reusable(&self) -> bool {
        match self.running.get() {
            Some(Ok(running)) => running.worker.is_live(),
            Some(Err(_)) => self.created.elapsed() < Duration::from_secs(5),
            None => true,
        }
    }

    async fn open(&self, runtime: &PluginRuntime) -> Result<&Running> {
        let _activation_lifecycle = self.lifecycle.lock().await;
        ensure!(!runtime.closed.load(Ordering::Acquire), "Plugin runtime is closed");
        ensure!(self.active.load(Ordering::Acquire), "Plugin activation has ended");
        let result = self.running.get_or_init(|| async {
            self.start(runtime).await.map_err(|error| error.to_string())
        }).await;
        result.as_ref().map_err(|error| anyhow::anyhow!("{error}"))
    }

    fn lock_packages(&self, root: &Path) -> Result<(InstallationLock, PathBuf)> {
        ensure!(self.active.load(Ordering::Acquire), "Plugin activation has ended");
        let lock = lock_plugin_startup(root)?;
        // The checkout is mutable across installs. Read the selected revision
        // again under the publication lock before either Python host loads it.
        ensure!(
            installed(root)?.iter().any(|plugin| plugin.name == self.id
                && plugin.enabled
                && plugin.sha == self.key.sha
                && plugin.revision == self.key.revision
                && plugin.python_package_generation == self.key.python_package_generation),
            "Plugin installation changed before activation"
        );
        let data = root.join("plugin-data").join(&self.id);
        let packages = python_packages::selected(&data, self.key.python_package_generation.as_deref())?;
        Ok((lock, packages))
    }

    async fn start(&self, runtime: &PluginRuntime) -> Result<Running> {
        ensure!(self.active.load(Ordering::Acquire), "Plugin activation has ended");
        #[cfg(test)]
        if let Some(started) = &self.test_start_gate {
            started.notify_one();
            self.startup_cancel.cancelled().await;
            anyhow::bail!("Plugin activation has ended");
        }
        let mut worker_slot = self.worker.lock().await;
        if let Some(previous) = worker_slot.clone() {
            previous.stop_and_wait().await.context("Could not join an abandoned Python plugin startup")?;
            *worker_slot = None;
        }
        let (_installation, packages) = self.lock_packages(&runtime.root)?;
        let path = python_host().context("The bundled Python host is unavailable")?;
        let plugin = dir(&runtime.root).join(&self.id);
        let data = runtime.root.join("plugin-data").join(&self.id);
        fs::create_dir_all(&data)?;
        let mut env = self.key.env.clone();
        env.extend([
            ("PLUGIN_ROOT".into(), plugin.to_string_lossy().into_owned()),
            ("PLUGIN_DATA".into(), data.to_string_lossy().into_owned()),
            ("HERMES_HOME".into(), runtime.root.to_string_lossy().into_owned()),
            ("HERMES_PROFILE_NAME".into(), runtime.name.clone()),
        ]);
        if runtime.chat.is_some() {
            env.push(("HERMES_PLUGIN_SERVICES".into(), "1".into()));
        }
        let worker = Worker::start(Stdio {
            command: path.to_string_lossy().into_owned(),
            args: vec!["serve".into(), plugin.to_string_lossy().into_owned(), packages.to_string_lossy().into_owned()],
            env,
            // Windows cannot replace or remove a folder that a running process works in, and
            // this worker outlives the turn: updating the plugin must be able to.
            cwd: data,
        }, runtime.root.clone(), runtime.chat.clone())?;
        *worker_slot = Some(worker.clone());
        drop(worker_slot);
        let plugin_prefix = prefix(&self.id);
        let connection = match tokio::select! {
            _ = self.startup_cancel.cancelled() => {
                worker.stop_and_wait().await.context("Could not stop a revoked Python plugin startup")?;
                *self.worker.lock().await = None;
                anyhow::bail!("Plugin activation has ended");
            }
            result = Mcp::connect_plugin(worker.clone(), &plugin_prefix) => result,
        } {
            Ok(connection) => connection,
            Err(error) => {
                worker.stop_and_wait().await.context("Could not stop a failed Python plugin startup")?;
                *self.worker.lock().await = None;
                return Err(error);
            }
        };
        if !self.active.load(Ordering::Acquire) {
            worker.stop_and_wait().await.context("Could not stop a retired Python plugin startup")?;
            *self.worker.lock().await = None;
            anyhow::bail!("Plugin activation has ended");
        }
        Ok(Running { worker, connection, described: Mutex::new(None) })
    }

    async fn bridge(&self, runtime: &PluginRuntime) -> Result<Mcp> {
        let _lifecycle = self.lifecycle.lock().await;
        ensure!(!runtime.closed.load(Ordering::Acquire), "Plugin runtime is closed");
        ensure!(self.active.load(Ordering::Acquire), "Plugin activation has ended");
        let (_installation, packages) = self.lock_packages(&runtime.root)?;
        let data = runtime.root.join("plugin-data").join(&self.id);
        let bridge = connect_python_bridge(
            &dir(&runtime.root).join(&self.id),
            &data,
            &packages,
            &self.key.env,
            &prefix(&self.id),
        )
        .await?;
        ensure!(!runtime.closed.load(Ordering::Acquire), "Plugin runtime is closed");
        ensure!(self.active.load(Ordering::Acquire), "Plugin activation has ended");
        Ok(bridge)
    }

    fn revoke(&self) {
        self.active.store(false, Ordering::Release);
        self.startup_cancel.cancel();
    }

    async fn unload(&self) -> Result<()> {
        let _lifecycle = self.lifecycle.lock().await;
        #[cfg(test)]
        if let Some((started, resume)) = &self.test_unload_gate {
            started.notify_one();
            resume.notified().await;
        }
        let worker = self.worker.lock().await.clone();
        if let Some(worker) = worker {
            if let Some(Ok(running)) = self.running.get() {
                let _ = tokio::time::timeout(Duration::from_secs(3), running.worker.request("hermes/unload", json!({}))).await;
            }
            worker.stop_and_wait().await?;
            *self.worker.lock().await = None;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use tokio::time::timeout;

    fn activation(id: &str) -> Arc<Activation> {
        Arc::new(Activation {
            id: id.into(),
            key: Revision {
                sha: "test".into(),
                revision: 0,
                python_package_generation: None,
                env: vec![],
            },
            active: true.into(),
            created: Instant::now(),
            running: OnceCell::new(),
            lifecycle: Mutex::new(()),
            worker: Mutex::new(None),
            startup_cancel: CancellationToken::new(),
            test_start_gate: None,
            test_unload_gate: None,
        })
    }

    #[tokio::test]
    async fn unload_revokes_startup_and_stale_handles_cannot_restart_it() {
        let home = tempdir().unwrap();
        let runtime = PluginRuntime::for_profile(home.path().into(), "test".into(), None);
        let started = Arc::new(Notify::new());
        let slot = Arc::new(Activation {
            test_start_gate: Some(started.clone()),
            ..Arc::try_unwrap(activation("example")).ok().unwrap()
        });
        runtime.activations.lock().await.active.insert(slot.id.clone(), slot.clone());
        let stale = PythonPlugin(slot.clone());

        let opening = {
            let slot = slot.clone();
            let runtime = runtime.clone();
            tokio::spawn(async move { slot.open(&runtime).await.map(|_| ()) })
        };
        started.notified().await;
        let unloading = {
            let runtime = runtime.clone();
            tokio::spawn(async move { runtime.unload().await })
        };
        assert!(timeout(Duration::from_secs(2), opening).await.unwrap().unwrap().is_err());
        unloading.await.unwrap();
        assert!(stale.request("tools/list", json!({})).await.is_err());
        assert!(slot.open(&runtime).await.is_err());
    }

    #[tokio::test]
    async fn close_waits_for_a_retirement_already_in_progress() {
        let home = tempdir().unwrap();
        let runtime = PluginRuntime::for_profile(home.path().into(), "test".into(), None);
        let started = Arc::new(Notify::new());
        let resume = Arc::new(Notify::new());
        let mut slot = Arc::try_unwrap(activation("retired")).ok().unwrap();
        slot.active.store(false, Ordering::Release);
        slot.test_unload_gate = Some((started.clone(), resume.clone()));
        runtime.activations.lock().await.retired.push(Arc::new(slot));

        let retiring = {
            let runtime = runtime.clone();
            tokio::spawn(async move { runtime.unload().await })
        };
        started.notified().await;
        let closing = {
            let runtime = runtime.clone();
            tokio::spawn(async move { runtime.close().await })
        };
        tokio::task::yield_now().await;
        assert!(!closing.is_finished(), "close must wait for the retirement lock");

        resume.notify_one();
        resume.notify_one();
        retiring.await.unwrap();
        closing.await.unwrap().unwrap();
        assert!(runtime.closed.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn close_permanently_rejects_stale_activation_handles() {
        let home = tempdir().unwrap();
        let runtime = PluginRuntime::for_profile(home.path().into(), "test".into(), None);
        let slot = activation("example");
        runtime.activations.lock().await.active.insert(slot.id.clone(), slot.clone());
        let stale = PythonPlugin(slot.clone());

        runtime.close().await.unwrap();

        assert!(stale.request("tools/list", json!({})).await.is_err());
        assert!(slot.open(&runtime).await.is_err());
        assert!(runtime.reconcile().await.is_empty());
    }

}

fn python_env(root: &Path, id: &str) -> Vec<(String, String)> {
    let defaults = config_schema(root, id);
    let mut values: BTreeMap<String, String> = defaults.into_iter()
        .map(|field| (field.key, field.default)).collect();
    for (name, value) in config(root, id) {
        let text = value.as_str().map_or_else(|| value.to_string(), str::to_owned);
        if !text.contains('\0') {
            values.insert(name, text);
        }
    }
    values.into_iter().map(|(name, value)| {
        let key = name.to_uppercase().replace(|c: char| !c.is_ascii_alphanumeric(), "_");
        (format!("PLUGIN_CONFIG_{key}"), value)
    }).collect()
}
