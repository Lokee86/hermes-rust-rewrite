//! Catalog plugins the Rust runtime can run: Agent Plugins v1 packages
//! (plugin.json + mcp.json + skills/), the format hermes_cli/agent_plugins.py
//! loads. `install` clones the catalog's repository at its exact pinned SHA
//! (README rule 2) and copies the entry's subdir to <root>/plugins/<name>;
//! its skills join the skill store and its MCP servers (streamable-http or
//! stdio) are connected for every turn while the plugin is enabled. Python
//! (`plugin.yaml` + `__init__.py`) plugins run in hermes-python-host, which
//! embeds the CPython runtime shipped with the app and implements PluginHost;
//! their `python_dependencies` are installed (wheels only) into a per-install
//! package generation in the plugin's data folder. Plugins that import Hermes
//! internals fall back to the installed Hermes Python agent. The bridge loads them with
//! Hermes's own PluginManager.
use crate::mcp::{Mcp, Stdio};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fs, path::{Component, Path, PathBuf}, process::Command};

mod python_packages;
mod runtime;
pub use runtime::{PluginCommand, PluginRuntime, PluginStatus, PythonMemory, PythonPlugin};

const PLUGIN_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json";
const MCP_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json";

/// One catalog entry, as the plugin-catalog YAML pins it.
#[derive(Clone, Debug, Default)]
pub struct CatalogEntry { pub name: String, pub title: String, pub repo: String, pub sha: String, pub subdir: String }

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Installed {
    pub name: String, pub title: String, pub version: String, pub description: String, pub sha: String,
    pub enabled: bool, pub servers: Vec<String>, pub skills: Vec<String>,
    #[serde(default)]
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python_package_generation: Option<String>,
}

fn dir(root: &Path) -> PathBuf { root.join("plugins") }
fn index(root: &Path) -> PathBuf { dir(root).join("installed.json") }
pub fn installed(root: &Path) -> Result<Vec<Installed>> {
    match fs::read(index(root)) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).context("Invalid plugins/installed.json")?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(e.into()),
    }
}
fn open_install_lock(root: &Path) -> Result<fs::File> {
    Ok(fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .open(dir(root).join(".install.lock"))?)
}

struct InstallationLock(fs::File);

impl Drop for InstallationLock {
    fn drop(&mut self) {
        // A concurrently spawned child can retain the open file description
        // until exec. Release the lock before closing our own reference.
        let _ = self.0.unlock();
    }
}

fn profile_parent(home: &Path) -> Option<&Path> {
    home.parent().filter(|parent| parent.file_name().is_some_and(|name| name == "profiles"))
}

fn open_profile_install_fence(home: &Path, parent: &Path) -> Result<fs::File> {
    let mut name = std::ffi::OsString::from(".plugin-install-");
    name.push(home.file_name().context("The plugin profile has no name")?);
    name.push(".lock");
    Ok(fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .open(parent.join(name))?)
}

/// Fence profile deletion against dependency staging and plugin publication.
/// Keep the guard until deletion finishes. The file is outside the profile.
pub fn lock_profile_plugin_deletion(home: &Path) -> Result<Option<fs::File>> {
    let home = match home.canonicalize() {
        Ok(home) => home,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let Some(parent) = profile_parent(home) else {
                return Ok(None);
            };
            let parent = match parent.canonicalize() {
                Ok(parent) => parent,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            parent.join(home.file_name().context("The plugin profile has no name")?)
        }
        Err(error) => return Err(error.into()),
    };
    let Some(parent) = profile_parent(&home) else {
        return Ok(None);
    };
    let lock = open_profile_install_fence(&home, parent)?;
    lock.try_lock().context("A plugin installation is in progress for this profile")?;
    Ok(Some(lock))
}

fn lock_profile_plugin_install(home: &Path) -> Result<Vec<fs::File>> {
    let mut scopes = Vec::new();
    let mut current = home;
    while let Some(parent) = profile_parent(current) {
        scopes.push((current, parent));
        // A bot can be inside another profile. Its install also fences deletion
        // of any enclosing profile.
        current = parent.parent().context("The profile store has no parent")?;
    }
    let locks = scopes.into_iter().rev().map(|(home, parent)| {
        let lock = open_profile_install_fence(home, parent)?;
        lock.try_lock_shared().context("The profile is being deleted")?;
        Ok(lock)
    }).collect::<Result<Vec<_>>>()?;
    ensure!(home.is_dir(), "The plugin profile no longer exists");
    Ok(locks)
}

fn lock_installation(root: &Path) -> Result<InstallationLock> {
    let lock = open_install_lock(root)?;
    lock.try_lock().context("Another plugin operation is using the installed packages")?;
    Ok(InstallationLock(lock))
}

fn wait_for_installation(root: &Path) -> Result<InstallationLock> {
    use std::time::{Duration, Instant};

    let lock = open_install_lock(root)?;
    let started = Instant::now();
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(InstallationLock(lock)),
            Err(fs::TryLockError::WouldBlock) if started.elapsed() < Duration::from_secs(45) => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error).context("Could not lock the installed plugin packages for publication"),
        }
    }
}

fn lock_plugin_startup(root: &Path) -> Result<InstallationLock> {
    let lock = open_install_lock(root)?;
    lock.try_lock_shared().context("The plugin is being installed or changed")?;
    Ok(InstallationLock(lock))
}

fn save(root: &Path, list: &[Installed]) -> Result<()> {
    fs::create_dir_all(dir(root))?;
    let temp = tempfile::NamedTempFile::new_in(dir(root))?;
    serde_json::to_writer_pretty(temp.as_file(), list)?;
    temp.persist(index(root))?;
    Ok(())
}

pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && !name.contains("--") && !name.contains("..")
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
        && name.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
}
fn inside(base: &Path, relative: &str) -> Option<PathBuf> {
    let path = Path::new(relative);
    (!relative.is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))).then(|| base.join(path))
}

/// Clone `entry` at its pinned commit and install it. Blocking (git): run off the UI thread.
pub fn install(root: &Path, entry: &CatalogEntry) -> Result<Installed> {
    ensure!(valid_name(&entry.name), "That plugin name is not valid");
    ensure!(entry.sha.len() == 40 && entry.sha.bytes().all(|b| b.is_ascii_hexdigit()), "The catalog entry has no exact commit pin");
    let repo = reqwest::Url::parse(&entry.repo).context("The catalog entry has no valid repository")?;
    ensure!(repo.scheme() == "https" && repo.username().is_empty() && repo.password().is_none(), "Plugins install only from an https repository");
    let home = root.canonicalize().context("The plugin profile no longer exists")?;
    let root = home.as_path();
    let _profiles = lock_profile_plugin_install(root)?;
    fs::create_dir_all(dir(root))?;
    let staging = tempfile::tempdir_in(dir(root))?;
    let git = |args: &[&str]| -> Result<()> {
        let out = Command::new("git").args(args).current_dir(staging.path()).env("GIT_TERMINAL_PROMPT", "0").output().context("git is not available on this computer")?;
        ensure!(out.status.success(), "git {} failed: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim());
        Ok(())
    };
    git(&["init", "-q"])?;
    git(&["fetch", "-q", "--depth", "1", repo.as_str(), &entry.sha])?;
    git(&["checkout", "-q", "FETCH_HEAD"])?;
    let source = if entry.subdir.trim().is_empty() { staging.path().to_path_buf() } else { inside(staging.path(), entry.subdir.trim()).context("The catalog subdir is not valid")? };
    let package = read_package(&source)?;
    if package.servers.contains_key("python") && python_host().is_none() && python_hermes().is_none() {
        bail!("This is a Python Hermes plugin, and this build of Hermes has no Python runtime (hermes-python-host is missing next to the app).");
    }
    install_checkout(root, entry, &source, package, install_python_dependencies)
}

fn install_python_dependencies(target: &Path, requirements: &[String]) -> Result<()> {
    let host = python_host().context("This plugin needs Python packages, and this build of Hermes has no Python runtime")?;
    let output = Command::new(host)
        .arg("install")
        .arg(target)
        .args(requirements)
        .output()
        .context("Could not start hermes-python-host")?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let details = stderr.lines().chain(stdout.lines())
            .filter(|line| line.contains("ERROR") || line.contains("rror:"))
            .collect::<Vec<_>>()
            .join(" ");
        bail!("Could not install the plugin's Python packages ({}): {}", requirements.join(", "), details.trim());
    }
    Ok(())
}

fn install_checkout(
    root: &Path,
    entry: &CatalogEntry,
    source: &Path,
    package: Package,
    install_dependencies: impl FnOnce(&Path, &[String]) -> Result<()>,
) -> Result<Installed> {
    let data = root.join("plugin-data").join(&entry.name);
    fs::create_dir_all(&data)?;
    let staged = if package.servers.contains_key("python") {
        let staged = python_packages::stage(&data)?;
        if !package.python_dependencies.is_empty() {
            install_dependencies(staged.path(), &package.python_dependencies)?;
        }
        Some(staged)
    } else {
        None
    };
    let python_package_generation = staged.as_ref().map(python_packages::selector).transpose()?;
    // Catalog installs run off the UI thread, so wait for any Python startup
    // to finish loading the current checkout before publishing the replacement.
    let _lock = wait_for_installation(root)?;
    let staged_package = read_package(source).context("The staged plugin checkout is unavailable")?;
    ensure!(
        staged_package.python_dependencies == package.python_dependencies
            && staged_package.servers.contains_key("python") == staged.is_some(),
        "The staged plugin's dependency requirements changed"
    );
    if let Some(generation) = &python_package_generation {
        python_packages::selected(&data, Some(generation))?;
    }
    let mut list = installed(root)?;
    let revision = list.iter().find(|plugin| plugin.name == entry.name).map_or(0, |plugin| plugin.revision);
    // A running copy holds its files open on Windows. The gateway ends a disabled plugin.
    let was_enabled = list.iter().any(|plugin| plugin.name == entry.name && plugin.enabled);
    if was_enabled {
        for plugin in list.iter_mut().filter(|plugin| plugin.name == entry.name) { plugin.enabled = false; plugin.revision = plugin.revision.saturating_add(1); }
        save(root, &list)?;
    }
    let target = dir(root).join(&entry.name);
    // Package generations stay available to workers that already loaded them.
    if let Err(error) = remove_dir(&target) {
        if was_enabled {
            for plugin in list.iter_mut().filter(|plugin| plugin.name == entry.name) { plugin.enabled = true; }
            save(root, &list)?;
        }
        return Err(error);
    }
    if list.iter().any(|plugin| plugin.name == entry.name) {
        crate::settings::ProfileSettings::invalidate_plugin_memory(root, &entry.name)?;
    }
    copy_tree(source, &target)?;
    let docs = package.skills.iter().map(|(skill, doc)| (format!("plugin/{}/{skill}", entry.name), doc.clone())).collect();
    crate::skill_catalog::put_documents(root, &format!("plugin/{}/", entry.name), docs)?;
    let record = Installed { name: entry.name.clone(), title: if entry.title.is_empty() { package.name.clone() } else { entry.title.clone() },
        version: package.version, description: package.description, sha: entry.sha.clone(), enabled: true,
        servers: package.servers.keys().cloned().collect(), skills: package.skills.into_iter().map(|(s, _)| s).collect(),
        revision: revision.saturating_add(1), python_package_generation };
    list.retain(|p| p.name != entry.name);
    list.push(record.clone());
    save(root, &list)?;
    if let Some(staged) = staged {
        // Published generations may be mapped in another process. Only
        // unpublished generations are automatically removed by TempDir.
        let _ = staged.keep();
    }
    Ok(record)
}

pub fn set_enabled(root: &Path, name: &str, on: bool) -> Result<()> {
    let _lock = lock_installation(root)?;
    let mut list = installed(root)?;
    let plugin = list.iter_mut().find(|p| p.name == name).context("That plugin is not installed")?;
    if plugin.enabled != on {
        crate::settings::ProfileSettings::invalidate_plugin_memory(root, name)?;
        plugin.revision = plugin.revision.saturating_add(1);
        plugin.enabled = on;
    }
    let ids: Vec<String> = plugin.skills.iter().map(|s| format!("plugin/{name}/{s}")).collect();
    save(root, &list)?;
    for id in ids { crate::skill_catalog::enable(root, &id, on)?; }
    Ok(())
}

/// The streamable-http servers of an installed plugin, for sign-in: (server name, url).
pub fn http_servers(root: &Path, name: &str) -> Result<Vec<(String, String)>> {
    let package = read_package(&dir(root).join(name))?;
    Ok(package.servers.into_iter().filter_map(|(n, s)| match s { Server::Http { url, .. } => Some((n, url)), _ => None }).collect())
}
/// Browser sign-in for every HTTP server of a plugin (MCP OAuth 2.1).
pub async fn sign_in(root: &Path, name: &str, open: impl Fn(&str) + Clone) -> Result<usize> {
    ensure!(valid_name(name), "That plugin name is not valid");
    let data = root.join("plugin-data").join(name);
    let servers = http_servers(root, name)?;
    ensure!(!servers.is_empty(), "This plugin has no servers that sign in");
    for (server, url) in &servers { crate::mcp_oauth::sign_in(&data, server, url, open.clone()).await.with_context(|| format!("{server}"))?; }
    Ok(servers.len())
}
pub fn signed_in(root: &Path, name: &str) -> bool {
    let data = root.join("plugin-data").join(name);
    http_servers(root, name).is_ok_and(|s| !s.is_empty() && s.iter().all(|(n, _)| crate::mcp_oauth::token_path(&data, n).is_file()))
}

/// One `extensions.com.nousresearch.hermes.config_schema` field.
#[derive(Clone, Debug, PartialEq)]
pub struct Field { pub key: String, pub kind: String, pub description: String, pub default: String, pub secret: bool }
pub fn config_schema(root: &Path, name: &str) -> Vec<Field> {
    let Ok(bytes) = fs::read(dir(root).join(name).join("plugin.json")) else { return vec![] };
    let Ok(manifest) = serde_json::from_slice::<Value>(&bytes) else { return vec![] };
    manifest.pointer("/extensions/com.nousresearch.hermes/config_schema").and_then(Value::as_object).map(|schema| schema.iter().filter_map(|(key, spec)| {
        let spec = spec.as_object()?;
        let kind = spec.get("type").and_then(Value::as_str).unwrap_or("str").to_lowercase();
        let default = match spec.get("default") { Some(Value::String(s)) => s.clone(), Some(Value::Null) | None => String::new(), Some(v) => v.to_string() };
        let secret = spec.get("secret").and_then(Value::as_bool).unwrap_or(false) || ["key", "token", "secret", "password"].iter().any(|w| key.to_lowercase().contains(w));
        Some(Field { key: key.clone(), kind, description: spec.get("description").and_then(Value::as_str).unwrap_or_default().into(), default, secret })
    }).collect()).unwrap_or_default()
}
fn config_path(root: &Path, name: &str) -> PathBuf { root.join("plugin-data").join(name).join("config.json") }
fn saved_config(root: &Path, name: &str) -> serde_json::Map<String, Value> {
    fs::read(config_path(root, name)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}
/// A secret that cannot be opened right now is left out.
pub fn config(root: &Path, name: &str) -> serde_json::Map<String, Value> {
    saved_config(root, name).into_iter().filter_map(|(key, value)| match value.as_str() {
        Some(text) if crate::secret::is_sealed(text) => Some((key, crate::secret::unseal(text).ok()?.into())),
        _ => Some((key, value)),
    }).collect()
}
pub fn set_config(root: &Path, name: &str, values: &serde_json::Map<String, Value>) -> Result<()> {
    ensure!(valid_name(name), "That plugin name is not valid");
    let path = config_path(root, name);
    if config(root, name) != *values {
        crate::settings::ProfileSettings::invalidate_plugin_memory(root, name)?;
    }
    fs::create_dir_all(path.parent().context("No data directory")?)?;
    let temp = tempfile::NamedTempFile::new_in(path.parent().context("No data directory")?)?;
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o600))?; }
    serde_json::to_writer_pretty(temp.as_file(), &sealed_config(root, name, values)?)?;
    temp.persist(path)?;
    Ok(())
}
/// `values` with the plugin's secrets encrypted. One that could not be opened is kept as it is.
fn sealed_config(root: &Path, name: &str, values: &serde_json::Map<String, Value>) -> Result<serde_json::Map<String, Value>> {
    let mut sealed = values.clone();
    let before = saved_config(root, name);
    for field in config_schema(root, name).into_iter().filter(|field| field.secret) {
        match sealed.get(&field.key).and_then(Value::as_str).filter(|text| !text.is_empty()) {
            Some(text) => {
                let text = crate::secret::seal(root, text)?;
                sealed.insert(field.key, text.into());
            }
            None => if let Some(kept) = before.get(&field.key).filter(|value| value.as_str().is_some_and(|text| crate::secret::is_sealed(text) && crate::secret::unseal(text).is_err())) {
                sealed.insert(field.key, kept.clone());
            }
        }
    }
    Ok(sealed)
}
/// The pinned commit the catalog now offers differs from the installed one.
pub fn update_available(installed: &Installed, catalog_sha: &str) -> bool { !catalog_sha.is_empty() && installed.sha != catalog_sha }

pub fn uninstall(root: &Path, name: &str) -> Result<()> {
    ensure!(valid_name(name), "That plugin name is not valid");
    let _lock = lock_installation(root)?;
    let mut list = installed(root)?;
    let plugin = list.iter_mut().find(|p| p.name == name).context("That plugin is not installed")?;
    plugin.enabled = false;
    plugin.revision = plugin.revision.saturating_add(1);
    crate::settings::ProfileSettings::invalidate_plugin_memory(root, name)?;
    save(root, &list)?;
    crate::skill_catalog::put_documents(root, &format!("plugin/{name}/"), vec![])?;
    remove_dir(&dir(root).join(name))?;
    list.retain(|p| p.name != name);
    save(root, &list)
}

/// Remove a folder. Windows refuses while a program works in it or has a library from it loaded,
/// so there this waits for the gateway to end the plugins it runs from the folder.
pub fn remove_dir(path: &Path) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(if cfg!(windows) { 20 } else { 0 });
    loop {
        match fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(250)),
            Err(error) => return Err(error).with_context(|| format!("Could not remove {}. Close anything that uses it and try again", path.display())),
        }
    }
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if entry.file_name() == ".git" || kind.is_symlink() { continue; }
        let dest = to.join(entry.file_name());
        if kind.is_dir() { copy_tree(&entry.path(), &dest)?; } else if kind.is_file() { fs::copy(entry.path(), dest)?; }
    }
    Ok(())
}

// ---------------------------------------------------------------- package
#[derive(Debug)]
enum Server { Http { url: String, headers: Vec<(String, String)> }, Stdio { command: String, args: Vec<String>, env: Vec<(String, String)>, cwd: Option<String> }, Python }

const BRIDGE: &str = include_str!("../assets/python_plugin_bridge.py");

/// hermes-python-host, next to the running executable (Hermes.app/Contents/MacOS, the Linux
/// bin directory, the Windows install folder); for tests, the profile directory above the
/// test binary (target/debug/deps/, or target/debug/build/<crate>/<hash>/out/).
pub fn python_host() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let name = format!("hermes-python-host{}", std::env::consts::EXE_SUFFIX);
    let depth = if cfg!(debug_assertions) { 5 } else { 1 };
    exe.ancestors().skip(1).take(depth).map(|dir| dir.join(&name)).find(|p| p.is_file())
}

/// The installed Hermes Python agent: (interpreter, hermes-agent checkout). The `hermes`
/// launcher on PATH names both (`exec <python> -I -c '... sys.path.insert(0, <root>)'`),
/// possibly behind one `exec <launcher> "$@"` hop.
pub fn python_hermes() -> Option<(PathBuf, PathBuf)> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from);
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).map(|d| d.join("hermes")).collect()).unwrap_or_default();
    if let Some(home) = &home { candidates.push(home.join(".local/bin/hermes")); }
    let exec = regex::Regex::new(r#"(?m)^exec (\S+) "\$@""#).ok()?;
    let python = regex::Regex::new(r#"exec (\S*python[0-9.]*) -I -c"#).ok()?;
    let root = regex::Regex::new(r#"sys\.path\.insert\(0, '"'"'(.+?)'"'"'\)"#).ok()?;
    for start in candidates.into_iter().filter(|c| c.is_file()) {
        let mut launcher = start;
        for _ in 0..3 {
            let Ok(text) = fs::read_to_string(&launcher) else { break };
            if let (Some(py), Some(agent)) = (python.captures(&text), root.captures(&text)) {
                let (py, agent) = (PathBuf::from(&py[1]), PathBuf::from(&agent[1]));
                if py.is_file() && agent.join("hermes_cli").is_dir() { return Some((py, agent)); }
                break;
            }
            match exec.captures(&text) { Some(next) => launcher = PathBuf::from(&next[1]), None => break }
        }
    }
    let agent = home?.join(".hermes/hermes-agent");
    [".venv/bin/python", "venv/bin/python", ".venv/Scripts/python.exe"].iter().map(|p| agent.join(p)).find(|p| p.is_file()).map(|py| (py, agent))
}

/// `name:` / `version:` / `description:` from a plugin.yaml (flat scalars only).
fn yaml_field(text: &str, key: &str) -> String {
    text.lines().find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix(':'))).map(|v| v.trim().trim_matches(['"', '\'']).to_owned()).unwrap_or_default()
}
struct Package { name: String, version: String, description: String, skills: Vec<(String, String)>, servers: std::collections::BTreeMap<String, Server>, python_dependencies: Vec<String> }

/// A plugin.yaml list (`key: [a, b]` or `key:` then `- a` lines) of requirement strings.
fn yaml_list(text: &str, key: &str) -> Vec<String> {
    let clean = |v: &str| v.trim().trim_matches(['"', '\'']).to_owned();
    let mut lines = text.lines();
    let Some(rest) = lines.find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix(':'))) else { return vec![] };
    let rest = rest.trim();
    let items: Vec<String> = if let Some(inline) = rest.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        // Split on commas outside quotes: "pkg>=1,<2" is one requirement.
        let (mut items, mut current, mut quote) = (vec![], String::new(), None);
        for c in inline.chars() {
            match (c, quote) {
                ('"' | '\'', None) => quote = Some(c),
                (q, Some(open)) if q == open => quote = None,
                (',', None) => items.push(std::mem::take(&mut current).trim().to_owned()),
                _ => current.push(c),
            }
        }
        items.push(current.trim().to_owned());
        items
    } else {
        lines.take_while(|l| l.trim().is_empty() || l.starts_with([' ', '\t', '-'])).filter_map(|l| l.trim().strip_prefix('-').map(clean)).collect()
    };
    // A leading '-' would be a pip option (--index-url, -e), not a requirement.
    items.into_iter().filter(|r| !r.is_empty() && !r.starts_with('-') && !r.contains('\0')).collect()
}

fn package_skills(root: &Path) -> Vec<(String, String)> {
    let mut skills = vec![];
    if let Ok(children) = fs::read_dir(root.join("skills")) {
        let mut children: Vec<_> = children.flatten().collect();
        children.sort_by_key(|c| c.file_name());
        for child in children {
            let skill = child.file_name().to_string_lossy().into_owned();
            let path = child.path().join("SKILL.md");
            if child.file_type().is_ok_and(|t| t.is_dir()) && !fs::symlink_metadata(&path).is_ok_and(|m| m.is_symlink())
                && let Ok(doc) = fs::read_to_string(&path) && doc.len() <= 512 * 1024 { skills.push((skill, doc)); }
        }
    }
    skills
}

fn read_package(root: &Path) -> Result<Package> {
    let manifest_path = root.join("plugin.json");
    if !manifest_path.is_file() {
        let yaml = ["plugin.yaml", "plugin.yml"].iter().map(|f| root.join(f)).find(|f| f.is_file());
        let Some(yaml) = yaml else { bail!("This repository has no plugin.json or plugin.yaml at the pinned commit") };
        ensure!(root.join("__init__.py").is_file(), "This Python plugin has no __init__.py");
        let text = fs::read_to_string(&yaml)?;
        let name = yaml_field(&text, "name");
        ensure!(valid_name(&name.replace('_', "-")), "plugin.yaml has no valid name");
        let mut servers = std::collections::BTreeMap::new();
        servers.insert("python".to_owned(), Server::Python);
        let mut python_dependencies = yaml_list(&text, "python_dependencies");
        python_dependencies.extend(yaml_list(&text, "pip_dependencies"));
        return Ok(Package { name, version: yaml_field(&text, "version"), description: yaml_field(&text, "description"), skills: package_skills(root), servers, python_dependencies });
    }
    let manifest: Value = serde_json::from_slice(&fs::read(&manifest_path)?).context("plugin.json is not valid JSON")?;
    if let Some(schema) = manifest["$schema"].as_str() { ensure!(schema == PLUGIN_SCHEMA, "plugin.json declares an unsupported schema"); }
    let name = manifest["name"].as_str().filter(|n| valid_name(n)).context("plugin.json has no valid name")?.to_owned();
    let skills = package_skills(root);
    let mut servers = std::collections::BTreeMap::new();
    if let Ok(bytes) = fs::read(root.join("mcp.json")) {
        let config: Value = serde_json::from_slice(&bytes).context("mcp.json is not valid JSON")?;
        ensure!(config["$schema"].as_str().is_none_or(|s| s == MCP_SCHEMA), "mcp.json declares an unsupported schema");
        for (server, spec) in config["mcpServers"].as_object().context("mcp.json has no mcpServers")? {
            let strings = |v: &Value| v.as_object().map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned()))).collect()).unwrap_or_default();
            let parsed = match spec["type"].as_str() {
                Some("streamable-http") => Server::Http { url: spec["url"].as_str().context("A server has no url")?.to_owned(), headers: strings(&spec["headers"]) },
                Some("stdio") => {
                    let command = spec["command"].as_str().filter(|c| !c.is_empty() && !c.contains('\0')).context("A stdio server has no command")?.to_owned();
                    ensure!(command.starts_with("./") || !(command.contains('/') || command.contains('\\') || command.contains(char::is_whitespace)), "A stdio command must be a bare executable or begin with ./");
                    let env: Vec<(String, String)> = strings(&spec["env"]);
                    ensure!(!env.iter().any(|(k, _)| k == "PLUGIN_ROOT" || k == "PLUGIN_DATA"), "PLUGIN_ROOT and PLUGIN_DATA are reserved");
                    Server::Stdio { command, args: spec["args"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default(),
                        env, cwd: spec["cwd"].as_str().map(str::to_owned) }
                }
                _ => continue, // sse and unknown types are not portable
            };
            servers.insert(server.clone(), parsed);
        }
    }
    Ok(Package { name, version: manifest["version"].as_str().unwrap_or_default().to_owned(), description: manifest["description"].as_str().unwrap_or_default().to_owned(), skills, servers, python_dependencies: vec![] })
}

fn prefix(server: &str) -> String {
    let clean: String = server.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).take(20).collect();
    format!("mcp_{clean}")
}

#[cfg(test)]
async fn connect_all(root: &Path) -> (Vec<Mcp>, Vec<String>) {
    PluginRuntime::new(root.to_path_buf(), None).tool_connections().await
}

/// Connect Agent Plugins v1 servers. Embedded Python has its own profile-owned activation.
async fn connect_mcp(root: &Path) -> (Vec<Mcp>, Vec<String>) {
    let (mut live, mut errors) = (vec![], vec![]);
    for plugin in installed(root).unwrap_or_default().into_iter().filter(|p| p.enabled && !p.servers.is_empty() && !p.servers.iter().any(|kind| kind == "python") && valid_name(&p.name)) {
        let plugin_root = dir(root).join(&plugin.name);
        let data = root.join("plugin-data").join(&plugin.name);
        let package = match read_package(&plugin_root) { Ok(p) => p, Err(e) => { errors.push(format!("{}: {e:#}", plugin.title)); continue } };
        let values = config(root, &plugin.name);
        let defaults: Vec<Field> = config_schema(root, &plugin.name);
        let setting = |key: &str| values.get(key).map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned)).or_else(|| defaults.iter().find(|f| f.key == key).map(|f| f.default.clone())).unwrap_or_default();
        let expand = |v: &str| {
            let mut out = v.replace("${PLUGIN_ROOT}", &plugin_root.to_string_lossy()).replace("${PLUGIN_DATA}", &data.to_string_lossy());
            for field in &defaults { out = out.replace(&format!("${{config.{}}}", field.key), &setting(&field.key)); }
            out
        };
        let config_env: Vec<(String, String)> = defaults.iter().map(|f| (format!("PLUGIN_CONFIG_{}", f.key.to_uppercase().replace(|c: char| !c.is_ascii_alphanumeric(), "_")), setting(&f.key))).collect();
        for (name, server) in package.servers {
            let connected = match server {
                Server::Http { url, headers } => {
                    let (url, headers) = (expand(&url), headers.into_iter().map(|(k, v)| (k, expand(&v))).collect());
                    let token = crate::mcp_oauth::token(&data, &name).await.ok().flatten();
                    let result = Mcp::connect_http(&url, token, headers, &prefix(&name)).await;
                    if let Err(e) = &result && format!("{e:#}").contains("401") { errors.push(format!("{} › {name}: needs sign-in (Capabilities › Plugins › Sign in)", plugin.title)); continue }
                    result
                }
                Server::Python => continue,
                Server::Stdio { command, args, mut env, cwd } => {
                    let command = match command.strip_prefix("./") { Some(rel) => match inside(&plugin_root, rel) { Some(p) => p.to_string_lossy().into_owned(), None => { errors.push(format!("{name}: bad command path")); continue } }, None => command };
                    let cwd = match cwd { Some(c) => PathBuf::from(expand(&c)), None => plugin_root.clone() };
                    let scoped = cwd.canonicalize().ok().is_some_and(|c| c.starts_with(plugin_root.canonicalize().unwrap_or_default()) || c.starts_with(data.canonicalize().unwrap_or_default()));
                    if !scoped { errors.push(format!("{name}: cwd must stay inside the plugin")); continue }
                    env.iter_mut().for_each(|(_, v)| *v = expand(v));
                    env.extend(config_env.iter().cloned());
                    env.push(("PLUGIN_ROOT".into(), plugin_root.to_string_lossy().into_owned()));
                    env.push(("PLUGIN_DATA".into(), data.to_string_lossy().into_owned()));
                    Mcp::connect_stdio(Stdio { command, args: args.iter().map(|a| expand(a)).collect(), env, cwd }, &prefix(&name)).await
                }
            };
            match connected { Ok(mcp) => live.push(mcp), Err(e) => errors.push(format!("{} › {name}: {e:#}", plugin.title)) }
        }
    }
    (live, errors)
}

/// The installed-agent compatibility path serves legacy tools.
async fn connect_python_bridge(plugin_root: &Path, data: &Path, packages: &Path, config_env: &[(String, String)], prefix: &str) -> Result<Mcp> {
    let mut env = config_env.to_vec();
    env.push(("PLUGIN_ROOT".into(), plugin_root.to_string_lossy().into_owned()));
    env.push(("PLUGIN_DATA".into(), data.to_string_lossy().into_owned()));
    let (python, agent) = python_hermes().context("The installed Hermes agent is unavailable")?;
    let bridge = data.join("python_plugin_bridge.py");
    if fs::read_to_string(&bridge).ok().as_deref() != Some(BRIDGE) { fs::create_dir_all(data)?; fs::write(&bridge, BRIDGE)?; }
    let args = vec!["-I".to_owned(), bridge.to_string_lossy().into_owned(), agent.to_string_lossy().into_owned(), plugin_root.to_string_lossy().into_owned(), packages.to_string_lossy().into_owned()];
    Mcp::connect_stdio(Stdio { command: python.to_string_lossy().into_owned(), args, env, cwd: plugin_root.to_path_buf() }, prefix).await
        .context("Installed Hermes agent tool bridge failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn installation_guards_unlock_while_duplicate_file_references_remain_open() {
        let fixture = tempfile::tempdir().unwrap();
        fs::create_dir(dir(fixture.path())).unwrap();
        let writer = lock_installation(fixture.path()).unwrap();
        let _writer_reference = writer.0.try_clone().unwrap();
        assert!(lock_plugin_startup(fixture.path()).is_err());
        assert!(lock_installation(fixture.path()).is_err());
        drop(writer);

        let first = lock_plugin_startup(fixture.path()).unwrap();
        let _reader_reference = first.0.try_clone().unwrap();
        let second = lock_plugin_startup(fixture.path()).unwrap();
        assert!(lock_installation(fixture.path()).is_err());
        drop(first);
        assert!(lock_installation(fixture.path()).is_err());
        drop(second);
        let _writer = lock_installation(fixture.path()).unwrap();
    }

    #[test]
    fn profile_deletion_and_nested_bot_install_share_canonical_fences() {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("profiles/work");
        let bot = home.join("profiles/bot");
        fs::create_dir_all(&bot).unwrap();
        let home = home.canonicalize().unwrap();
        let bot = bot.canonicalize().unwrap();
        let alias = bot.join("..").join("bot");
        let installation = lock_profile_plugin_install(&alias.canonicalize().unwrap()).unwrap();
        assert!(lock_profile_plugin_deletion(&alias).is_err());
        assert!(lock_profile_plugin_deletion(&home).is_err());
        drop(installation);

        let deleting = lock_profile_plugin_deletion(&home).unwrap().unwrap();
        assert!(lock_profile_plugin_install(&bot).is_err());
        assert!(!bot.join("plugins").exists());

        // A second installer may have canonicalized this name before deletion
        // but must not recreate it after the deletion fence is released.
        let empty_profile = fixture.path().join("profiles/empty");
        fs::create_dir(&empty_profile).unwrap();
        let empty_profile = empty_profile.canonicalize().unwrap();
        fs::remove_dir(&empty_profile).unwrap();
        assert!(lock_profile_plugin_install(&empty_profile).is_err());
        assert!(!empty_profile.exists());
        let retry = lock_profile_plugin_deletion(&empty_profile).unwrap().unwrap();
        assert!(lock_profile_plugin_deletion(&empty_profile).is_err());
        drop(retry);
        assert!(!empty_profile.exists());
        drop(deleting);
    }

    #[test]
    fn python_updates_stage_a_new_package_generation_without_touching_loaded_files() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("profile");
        let source = fixture.path().join("checkout");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("plugin.yaml"), "name: fixture\npython_dependencies: [local-fixture]\n").unwrap();
        fs::write(source.join("__init__.py"), "new checkout").unwrap();

        let data = root.join("plugin-data/fixture");
        let legacy = data.join(python_packages::LEGACY);
        fs::create_dir_all(&legacy).unwrap();
        fs::write(data.join("config.json"), r#"{"keep":true}"#).unwrap();
        let native = legacy.join("native.pyd");
        fs::write(&native, "old native image").unwrap();
        let installed_checkout = dir(&root).join("fixture");
        fs::create_dir_all(&installed_checkout).unwrap();
        fs::write(installed_checkout.join("__init__.py"), "old checkout").unwrap();
        let old = Installed {
            name: "fixture".into(),
            enabled: true,
            servers: vec!["python".into()],
            revision: 3,
            ..Default::default()
        };
        save(&root, &[old.clone()]).unwrap();
        let reopened = installed(&root).unwrap().remove(0);
        assert_eq!(reopened.python_package_generation, None);
        assert_eq!(python_packages::selected(&data, None).unwrap(), legacy);

        // Windows denies writes and deletion while this synthetic native
        // image is open, like a .pyd mapped by another process.
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0);
        }
        let held = options.open(&native).unwrap();
        #[cfg(windows)]
        assert!(fs::OpenOptions::new().write(true).open(&native).is_err());

        let entry = CatalogEntry { name: "fixture".into(), sha: "a".repeat(40), ..Default::default() };
        let mut failed_stage = PathBuf::new();
        let result = install_checkout(&root, &entry, &source, read_package(&source).unwrap(), |stage, _| {
            failed_stage = stage.to_owned();
            fs::write(stage.join("native.pyd"), "partial install")?;
            bail!("synthetic dependency failure")
        });
        assert!(result.is_err());
        assert!(!failed_stage.exists());
        assert_eq!(installed(&root).unwrap(), vec![old]);
        assert_eq!(fs::read_to_string(installed_checkout.join("__init__.py")).unwrap(), "old checkout");

        let missing = install_checkout(&root, &entry, &source, read_package(&source).unwrap(), |stage, _| {
            fs::remove_dir(stage)?;
            Ok(())
        });
        assert!(missing.is_err());
        assert_eq!(installed(&root).unwrap()[0].revision, 3);
        assert_eq!(fs::read_to_string(installed_checkout.join("__init__.py")).unwrap(), "old checkout");

        let first = std::thread::scope(|threads| {
            let startup = lock_plugin_startup(&root).unwrap();
            let (staged, stage_ready) = std::sync::mpsc::channel();
            let (finished, published) = std::sync::mpsc::channel();
            let (root, entry, source, legacy) = (&root, &entry, &source, &legacy);
            let installer = threads.spawn(move || {
                let result = install_checkout(root, entry, source, read_package(source).unwrap(), |stage, requirements| {
                    assert_eq!(requirements, ["local-fixture"]);
                    assert_ne!(stage, legacy);
                    fs::write(stage.join("native.pyd"), "new native image")?;
                    staged.send(()).unwrap();
                    Ok(())
                });
                finished.send(result).unwrap();
            });
            stage_ready.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            assert!(matches!(
                published.recv_timeout(std::time::Duration::from_millis(100)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ));
            drop(startup);
            let first = published.recv_timeout(std::time::Duration::from_secs(5)).unwrap().unwrap();
            installer.join().unwrap();
            first
        });
        assert_eq!(first.revision, 4);
        assert_eq!(installed(&root).unwrap(), vec![first.clone()]);
        let first_path = python_packages::selected(&data, first.python_package_generation.as_deref()).unwrap();
        assert_eq!(fs::read_to_string(first_path.join("native.pyd")).unwrap(), "new native image");
        drop(held);
        assert_eq!(fs::read_to_string(&native).unwrap(), "old native image");

        let held = options.open(first_path.join("native.pyd")).unwrap();
        fs::write(source.join("plugin.yaml"), "name: fixture\n").unwrap();
        let second = install_checkout(&root, &entry, &source, read_package(&source).unwrap(), |_, _| {
            panic!("a plugin without dependencies does not run the package installer")
        }).unwrap();
        let second_path = python_packages::selected(&data, second.python_package_generation.as_deref()).unwrap();
        assert_ne!(second_path, first_path);
        assert_eq!(fs::read_dir(second_path).unwrap().count(), 0);
        assert!(first_path.is_dir());
        assert_eq!(config(&root, "fixture")["keep"], true);
        drop(held);
        assert_eq!(fs::read_to_string(first_path.join("native.pyd")).unwrap(), "new native image");
        assert!(python_packages::selected(&data, Some("../python-packages")).is_err());
        assert!(python_packages::selected(&data, Some("generation-0000000000000000")).is_err());
    }

    #[test]
    fn reads_agent_plugin_packages_and_python_ones() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        fs::create_dir_all(pkg.join("skills/safe-tx")).unwrap();
        fs::write(pkg.join("plugin.json"), format!(r#"{{"$schema":"{PLUGIN_SCHEMA}","name":"aave-mcp","version":"1.0.0","description":"Aave"}}"#)).unwrap();
        fs::write(pkg.join("skills/safe-tx/SKILL.md"), "---\nname: safe-tx\ndescription: Safe transactions\n---\nBody").unwrap();
        fs::write(pkg.join("mcp.json"), format!(r#"{{"$schema":"{MCP_SCHEMA}","mcpServers":{{"aave":{{"type":"streamable-http","url":"https://mcp.aave.com"}},"local":{{"type":"stdio","command":"./bin/server","args":["${{PLUGIN_DATA}}"]}},"old":{{"type":"sse","url":"https://x"}}}}}}"#)).unwrap();
        let package = read_package(&pkg).unwrap();
        assert_eq!(package.name, "aave-mcp");
        assert_eq!(package.skills[0].0, "safe-tx");
        assert_eq!(package.servers.keys().collect::<Vec<_>>(), ["aave", "local"], "sse is not portable");
        // Commands must be bare or package-relative.
        fs::write(pkg.join("mcp.json"), r#"{"mcpServers":{"x":{"type":"stdio","command":"/bin/sh"}}}"#).unwrap();
        assert!(read_package(&pkg).is_err());
        let py = dir.path().join("py");
        fs::create_dir_all(&py).unwrap();
        fs::write(py.join("plugin.yaml"), "name: hello_py\nversion: 0.2.0\n").unwrap();
        assert!(format!("{:#}", read_package(&py).err().unwrap()).contains("__init__.py"));
        fs::write(py.join("__init__.py"), "def register(ctx): pass\n").unwrap();
        let package = read_package(&py).unwrap();
        assert_eq!((package.name.as_str(), package.version.as_str()), ("hello_py", "0.2.0"));
        assert_eq!(package.servers.keys().collect::<Vec<_>>(), ["python"]);
        assert!(inside(&pkg, "../etc").is_none() && inside(&pkg, "/etc").is_none() && inside(&pkg, "plugins/mcp").is_some());
        assert!(!valid_name("../x") && valid_name("aave-mcp"));
    }
    #[tokio::test]
    async fn an_enabled_plugin_runs_its_stdio_server_and_a_disabled_one_does_not() {
        let root = tempfile::tempdir().unwrap();
        let pkg = root.path().join("plugins/echo-kit");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("plugin.json"), r#"{"name":"echo-kit","version":"0.1.0"}"#).unwrap();
        let python = if cfg!(windows) { "python" } else { "python3" };
        let manifest = serde_json::json!({"mcpServers":{"echo":{"type":"stdio","command":python,"args":["${PLUGIN_ROOT}/server.py"]}}});
        fs::write(pkg.join("mcp.json"), manifest.to_string()).unwrap();
        fs::write(pkg.join("server.py"), r#"
import json, sys, os
for line in sys.stdin:
    m = json.loads(line)
    if "id" not in m: continue
    r = {"initialize": {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}},
         "tools/list": {"tools": [{"name": "echo", "description": "Echo", "inputSchema": {"type": "object"}}]},
         "tools/call": {"content": [{"type": "text", "text": m.get("params", {}).get("arguments", {}).get("v", "") + "|" + os.path.basename(os.environ["PLUGIN_DATA"])}]}}[m["method"]]
    print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}), flush=True)
"#).unwrap();
        fs::create_dir_all(root.path().join("plugin-data/echo-kit")).unwrap();
        let mut record = Installed { name: "echo-kit".into(), title: "Echo".into(), enabled: true, servers: vec!["echo".into()], ..Default::default() };
        save(root.path(), std::slice::from_ref(&record)).unwrap();
        let (mut live, errors) = connect_all(root.path()).await;
        assert!(errors.is_empty(), "{errors:?}");
        assert!(live[0].contains("mcp_echo__echo"));
        let reply = live[0].call("mcp_echo__echo", &serde_json::json!({"v": "hi"})).await.unwrap();
        assert_eq!(reply["content"][0]["text"], "hi|echo-kit");
        live[0].close().await;
        record.enabled = false;
        save(root.path(), &[record]).unwrap();
        assert!(connect_all(root.path()).await.0.is_empty());
    }
    #[test]
    fn reads_python_dependencies_from_plugin_yaml() {
        let yaml = "name: x\npython_dependencies:\n  - httpx>=0.27,<1\n  - \"rich\"\n  - --index-url=https://evil\nversion: 1\npip_dependencies: ['a; python_version > \"3.10\"', b]\n";
        assert_eq!(yaml_list(yaml, "python_dependencies"), ["httpx>=0.27,<1", "rich"], "pip options are not requirements");
        assert_eq!(yaml_list(yaml, "pip_dependencies"), ["a; python_version > \"3.10\"", "b"]);
        assert!(yaml_list(yaml, "requires_env").is_empty());
    }

    const SHOUT: &str = r#"
import json

def _shout(args, **_):
    return json.dumps({"text": str(args.get("text", "")).upper() + "!"})

def _no_secrets(tool_name, args, **_):
    if "secret" in json.dumps(args):
        return {"action": "block", "message": "no secrets"}

def _tag(tool_name, result, **_):
    return result + " [seen]"

def register(ctx):
    ctx.register_tool("shout_text", "shout", {"name": "shout_text", "description": "Shout text",
        "parameters": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}}, _shout)
    ctx.register_hook("pre_tool_call", _no_secrets)
    ctx.register_hook("transform_tool_result", _tag)
"#;

    #[test]
    fn plugin_secrets_are_encrypted_and_other_settings_are_not() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(".sealed-in-tests"), "").unwrap();
        let package = dir(root.path()).join("kit");
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("plugin.json"), r#"{"name":"kit","extensions":{"com.nousresearch.hermes":{"config_schema":{"api_key":{"type":"str"},"volume":{"type":"str"}}}}}"#).unwrap();
        let values = serde_json::json!({"api_key":"fixture-value","volume":"loud"});
        set_config(root.path(), "kit", values.as_object().unwrap()).unwrap();
        let text = fs::read_to_string(config_path(root.path(), "kit")).unwrap();
        assert!(!text.contains("fixture-value") && text.contains("loud"), "{text}");
        assert_eq!(Value::Object(config(root.path(), "kit")), values);
        // A secret that cannot be opened is not lost when another setting is saved.
        let damaged = text.replace("hermes-sealed:1:", "hermes-sealed:1:AAAA");
        fs::write(config_path(root.path(), "kit"), &damaged).unwrap();
        assert!(!config(root.path(), "kit").contains_key("api_key"));
        set_config(root.path(), "kit", serde_json::json!({"volume":"quiet"}).as_object().unwrap()).unwrap();
        assert_eq!(saved_config(root.path(), "kit")["api_key"], serde_json::from_str::<Value>(&damaged).unwrap()["api_key"]);
    }

    fn shout_plugin(root: &Path, requirements: &str) {
        let pkg = root.join("plugins/shout");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("plugin.yaml"), format!("name: shout\nversion: 1.0.0\ndescription: Shouts\n{requirements}")).unwrap();
        fs::write(pkg.join("__init__.py"), SHOUT).unwrap();
        save(root, &[Installed { name: "shout".into(), title: "Shout".into(), enabled: true, servers: vec!["python".into()], ..Default::default() }]).unwrap();
    }

    /// Runs in the app's own embedded Python (hermes-python-host, built by `cargo test --workspace`).
    #[tokio::test]
    async fn a_python_plugin_runs_in_the_built_in_python_with_its_hooks() {
        assert!(python_host().is_some(), "build hermes-python-host first (cargo test --workspace builds it)");
        let root = tempfile::tempdir().unwrap();
        shout_plugin(root.path(), "");
        let data = root.path().join("plugin-data/shout");
        let legacy = data.join(python_packages::LEGACY);
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("mercury_fixture_marker.py"), "SUFFIX = '?'").unwrap();
        let packages = python_packages::stage(&data).unwrap();
        fs::write(packages.path().join("mercury_fixture_marker.py"), "SUFFIX = '!'").unwrap();
        let mut list = installed(root.path()).unwrap();
        list[0].python_package_generation = Some(python_packages::selector(&packages).unwrap());
        save(root.path(), &list).unwrap();
        let _ = packages.keep();
        let code = SHOUT.replace("+ \"!\"", "+ SUFFIX");
        fs::write(
            root.path().join("plugins/shout/__init__.py"),
            format!("from mercury_fixture_marker import SUFFIX\n{code}"),
        ).unwrap();
        let (mut live, errors) = connect_all(root.path()).await;
        assert!(errors.is_empty(), "{errors:?}");
        assert!(live[0].contains("mcp_shout__shout_text"), "{:?}", live[0].specs());
        let reply = live[0].call("mcp_shout__shout_text", &serde_json::json!({"text": "hi"})).await.unwrap();
        assert_eq!(reply["content"][0]["text"], r#"{"text": "HI!"}"#);
        let call = |args: &str| crate::history::ToolCall { id: "c1".into(), name: "terminal".into(), arguments: args.into() };
        assert_eq!(crate::plugin_hooks::pre_tool_call(&mut live, &call(r#"{"command":"cat secret.txt"}"#)).await.as_deref(), Some("no secrets"));
        assert_eq!(crate::plugin_hooks::pre_tool_call(&mut live, &call(r#"{"command":"ls"}"#)).await, None);
        assert_eq!(crate::plugin_hooks::after_tool_call(&mut live, &call("{}"), "ok".into()).await, "ok [seen]");
        live[0].close().await;
    }

    #[cfg(windows)]
    #[test]
    fn removing_a_folder_waits_for_a_program_that_still_uses_it() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("in-use");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("native.pyd"), "x").unwrap();
        let held = fs::OpenOptions::new().read(true).share_mode(0).open(folder.join("native.pyd")).unwrap();
        let release = std::thread::spawn(move || { std::thread::sleep(std::time::Duration::from_secs(1)); drop(held); });
        remove_dir(&folder).unwrap();
        release.join().unwrap();
        assert!(!folder.exists());
    }

    #[tokio::test]
    async fn one_slow_tool_does_not_hold_up_the_plugin() {
        let root = tempfile::tempdir().unwrap();
        shout_plugin(root.path(), "");
        fs::write(root.path().join("plugins/shout/__init__.py"), r#"
import threading
gate = threading.Event()
def _slow(args, **_):
    return "released" if gate.wait(20) else "timed out"
def _open(args, **_):
    gate.set()
    return "opened"
def register(ctx):
    schema = lambda name: {"name": name, "description": name, "parameters": {"type": "object"}}
    ctx.register_tool("slow", "shout", schema("slow"), _slow)
    ctx.register_tool("open", "shout", schema("open"), _open)
    ctx.register_hook("pre_tool_call", lambda **_: None)
"#).unwrap();
        let runtime = PluginRuntime::new(root.path().to_path_buf(), None);
        let (mut first, _) = runtime.tool_connections().await;
        let (mut second, _) = runtime.tool_connections().await;
        let slow = tokio::spawn(async move { first[0].call("mcp_shout__slow", &serde_json::json!({})).await });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let call = crate::history::ToolCall { id: "c".into(), name: "terminal".into(), arguments: "{}".into() };
        assert_eq!(crate::plugin_hooks::pre_tool_call(&mut second, &call).await, None);
        second[0].call("mcp_shout__open", &serde_json::json!({})).await.unwrap();
        assert_eq!(slow.await.unwrap().unwrap()["content"][0]["text"], "released");
        runtime.unload().await;
    }

    #[tokio::test]
    async fn a_shared_worker_follows_tool_checks_and_stops_when_its_plugin_is_disabled() {
        let root = tempfile::tempdir().unwrap();
        let flag = root.path().join("signed-in");
        shout_plugin(root.path(), "");
        fs::write(root.path().join("plugins/shout/__init__.py"), format!(r#"
import os
def _noisy(args, **_):
    os.write(1, b"not a protocol frame\n")
    return os.getcwd()
def register(ctx):
    schema = lambda name: {{"name": name, "description": name, "parameters": {{"type": "object"}}}}
    ctx.register_tool("noisy", "shout", schema("noisy"), _noisy)
    ctx.register_tool("later", "shout", schema("later"), _noisy, check_fn=lambda: os.path.exists({:?}))
"#, flag.to_string_lossy())).unwrap();
        let runtime = PluginRuntime::new(root.path().to_path_buf(), None);
        let (mut live, errors) = runtime.tool_connections().await;
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!live[0].contains("mcp_shout__later"));
        // Output from a child program does not end the plugin, and the worker stays out of the
        // plugin folder so an update can replace it.
        let reply = live[0].call("mcp_shout__noisy", &serde_json::json!({})).await.unwrap();
        let cwd = PathBuf::from(reply["content"][0]["text"].as_str().unwrap()).canonicalize().unwrap();
        assert_eq!(cwd, root.path().join("plugin-data/shout").canonicalize().unwrap());
        fs::write(&flag, "").unwrap();
        let (mut live, _) = runtime.tool_connections().await;
        assert!(live[0].contains("mcp_shout__later"), "{:?}", live[0].specs());

        // An unreadable index is not an uninstall.
        let saved = fs::read(index(root.path())).unwrap();
        fs::write(index(root.path()), "{").unwrap();
        runtime.retire_stale().await;
        assert!(live[0].call("mcp_shout__noisy", &serde_json::json!({})).await.is_ok());
        fs::write(index(root.path()), saved).unwrap();

        set_enabled(root.path(), "shout", false).unwrap();
        runtime.retire_stale().await;
        assert!(live[0].call("mcp_shout__noisy", &serde_json::json!({})).await.is_err());
        uninstall(root.path(), "shout").unwrap();
        assert!(!root.path().join("plugins/shout").exists());
    }

    /// Network (PyPI): `cargo test -- --ignored python_dependencies_install`
    #[test] #[ignore]
    fn python_dependencies_install_into_the_plugin_data_folder() {
        let host = python_host().expect("build hermes-python-host first");
        let data = tempfile::tempdir().unwrap();
        let out = Command::new(host).arg("install").arg(data.path().join(python_packages::LEGACY)).arg("six==1.17.0").output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(data.path().join(python_packages::LEGACY).join("six.py").is_file());
    }

    /// A plugin that imports Hermes internals falls back to the installed Hermes Python agent.
    /// Needs that agent on this machine: `cargo test -- --ignored python_plugin`
    #[tokio::test] #[ignore]
    async fn a_python_plugin_that_imports_hermes_runs_through_the_installed_hermes() {
        assert!(python_hermes().is_some(), "no Hermes Python agent found");
        let root = tempfile::tempdir().unwrap();
        let pkg = root.path().join("plugins/shout");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("plugin.yaml"), "name: shout\nversion: 1.0.0\ndescription: Shouts\n").unwrap();
        fs::write(pkg.join("__init__.py"), r#"
import json
from hermes_constants import get_hermes_home  # Hermes internals: not in the built-in Python

def _shout(args, **_):
    return json.dumps({"text": str(args.get("text", "")).upper() + "!"})

def register(ctx):
    ctx.register_tool("shout_text", "shout", {"name": "shout_text", "description": "Shout text",
        "parameters": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}}, _shout)
"#).unwrap();
        save(root.path(), &[Installed { name: "shout".into(), title: "Shout".into(), enabled: true, servers: vec!["python".into()], ..Default::default() }]).unwrap();
        let (mut live, errors) = connect_all(root.path()).await;
        assert!(errors.is_empty(), "{errors:?}");
        assert!(live[0].contains("mcp_shout__shout_text"), "{:?}", live[0].specs());
        let reply = live[0].call("mcp_shout__shout_text", &serde_json::json!({"text": "hi"})).await.unwrap();
        assert!(reply["content"][0]["text"].as_str().unwrap().contains("HI!"), "{reply}");
        live[0].close().await;
    }
    /// Network: installs the real catalog entry at its pin. `cargo test -- --ignored live_install`
    #[test] #[ignore]
    fn live_install_of_a_catalog_agent_plugin() {
        let root = tempfile::tempdir().unwrap();
        let entry = CatalogEntry { name: "aave-mcp".into(), title: "Aave".into(), repo: "https://github.com/aave/skills".into(), sha: "7132f6e952f9100f16577c33774a1e63948f7fab".into(), subdir: "plugins/mcp".into() };
        let record = install(root.path(), &entry).unwrap();
        eprintln!("{record:?}");
        assert!(!record.servers.is_empty() && !record.skills.is_empty());
        assert!(crate::skill_catalog::status(root.path()).unwrap().installed.iter().any(|id| id.starts_with("plugin/aave-mcp/")));
        let (live, errors) = tokio::runtime::Runtime::new().unwrap().block_on(connect_all(root.path()));
        eprintln!("servers={} errors={errors:?} tools={:?}", live.len(), live.iter().flat_map(|m| m.specs()).filter_map(|s| s["function"]["name"].as_str().map(str::to_owned)).take(6).collect::<Vec<_>>());
        assert_eq!(live.len(), 1, "{errors:?}");
        uninstall(root.path(), "aave-mcp").unwrap();
        assert!(installed(root.path()).unwrap().is_empty() && !root.path().join("plugins/aave-mcp").exists());
    }
}
