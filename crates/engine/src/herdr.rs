//! Hermes-owned terminal identities and a private, per-profile Herdr Core.
mod wire;
use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::{
    digest,
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
};
use tempfile::NamedTempFile;
use tokio::process::Command;
pub(crate) use wire::Input as TerminalInput;
pub use wire::{Attach, Frame};

/// Core relaunches the same executable for its server and holder. Dispatch before Tokio or Iced.
pub fn dispatch_role() -> Option<i32> {
    #[cfg(feature="embedded-herdr")]
    if matches!(std::env::args_os().nth(1).as_deref().and_then(|s|s.to_str()), Some("connect"|"serve"|"hold"|"--hc-launchd"|"--licenses")) {
        return Some(herdr_core::cli::main());
    }
    None
}

pub const VERSION: &str = "0.1.22";

#[derive(Clone)]
pub struct Core {
    profile: PathBuf,
    identity: String,
    session: String,
    binary: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Terminal {
    pub id: String,
    pub cwd: PathBuf,
    pub shell: String,
    pub conversation_id: Option<String>,
    pub tool_call_id: Option<String>,
    name: String,
    #[serde(default)]
    closed: bool,
    #[serde(default, alias = "fingerprint")]
    argv_digest: Option<String>,
}

fn hash(bytes: &[u8]) -> Vec<u8> {
    digest::digest(&digest::SHA256, bytes).as_ref().to_vec()
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn random_id() -> Result<String> {
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("No terminal identity entropy"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
impl Core {
    pub fn new(profile_root: &Path) -> Result<Self> {
        let binary = match std::env::var_os("HERMES_HERDR_CORE") {
            Some(executable) => PathBuf::from(executable),
            #[cfg(feature="embedded-herdr")]
            None => std::env::current_exe()?,
            #[cfg(not(feature="embedded-herdr"))]
            None => std::env::current_exe()?
                .with_file_name(format!("herdr-core{}", std::env::consts::EXE_SUFFIX)),
        };
        ensure!(
            binary.is_absolute(),
            "HERMES_HERDR_CORE requires an absolute executable path"
        );
        let profile = fs::canonicalize(profile_root)?;
        // The session is a unique installation identity and survives gateway restarts.
        // Recreating a deleted profile, even at the same path, gets a different host.
        let directory = profile.join("terminal-host");
        fs::create_dir_all(&directory)?;
        let path = directory.join("identity");
        if !path.exists() {
            let mut tmp = NamedTempFile::new_in(&directory)?;
            tmp.write_all(random_id()?.as_bytes())?;
            tmp.as_file().sync_all()?;
            match tmp.persist_noclobber(&path) {
                Ok(_) => {}
                Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
        }
        let identity = fs::read_to_string(path)?;
        ensure!(valid_id(&identity), "Invalid terminal host identity");
        let session = format!("hm-{}", hex(&hash(identity.as_bytes())[..12]));
        Ok(Self {
            profile,
            binary,
            session,
            identity,
        })
    }

    pub fn owner_live(&self) -> bool {
        fs::read_to_string(self.profile.join("terminal-host/identity"))
            .is_ok_and(|id| id == self.identity)
    }
    pub(super) fn command(&self) -> Result<Command> {
        ensure!(self.owner_live(), "Terminal profile expired");
        let mut c = Command::new(&self.binary);
        c.env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for k in [
            "PATH",
            "PATHEXT",
            "SYSTEMROOT",
            "WINDIR",
            "SYSTEMDRIVE",
            "COMSPEC",
            "USERPROFILE",
            "APPDATA",
            "LOCALAPPDATA",
            "PROGRAMDATA",
            "PROGRAMFILES",
            "PROGRAMFILES(X86)",
            "PROGRAMW6432",
            "USERNAME",
            "USERDOMAIN",
            "TEMP",
            "TMP",
            "TMPDIR",
            "HOME",
            "USER",
            "LOGNAME",
            "SHELL",
            "LANG",
            "LC_ALL",
            "LC_CTYPE",
            "TERM",
            "XDG_RUNTIME_DIR",
            "DBUS_SESSION_BUS_ADDRESS",
        ] {
            if let Some(v) = std::env::var_os(k) {
                c.env(k, v);
            }
        }
        let home = self.profile.join("terminal-host");
        c.env("HC_SESSION", &self.session)
            .env("XDG_STATE_HOME", home.join("state"))
            .env("XDG_CONFIG_HOME", home.join("config"))
            .current_dir(&home);
        #[cfg(windows)]
        c.creation_flags(0x08000000);
        Ok(c)
    }
    fn record_path(&self, id: &str) -> Result<PathBuf> {
        ensure!(valid_id(id), "Terminal expired");
        Ok(self
            .profile
            .join("terminal-host")
            .join(format!("{id}.json")))
    }
    pub fn lookup(&self, id: &str) -> Result<Option<Terminal>> {
        if !valid_id(id) {
            return Ok(None);
        }
        let path = self.record_path(id)?;
        if path.with_extension("ended").exists() { return Ok(None); }
        match fs::File::open(path) {
            Ok(f) => {
                let t: Terminal = serde_json::from_reader(f)?;
                ensure!(t.id == id, "Damaged terminal record");
                Ok((!t.closed).then_some(t))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    pub(super) fn agent_terminals(&self, conversation: &str) -> Result<Vec<Terminal>> {
        let mut terminals = Vec::new();
        for entry in fs::read_dir(self.profile.join("terminal-host"))? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "json")
                && let Some(id) = path.file_stem().and_then(|s| s.to_str())
                && let Some(terminal) = self.lookup(id)?
                && terminal.conversation_id.as_deref() == Some(conversation) {
                terminals.push(terminal);
            }
        }
        Ok(terminals)
    }

    /// The profile identity namespaces the client-generated, durable launch key.
    pub fn shell_launch_id(&self, key: &str) -> String {
        let bytes = serde_json::to_vec(&("shell", &self.session, key)).expect("string tuple");
        URL_SAFE_NO_PAD.encode(hash(&bytes))
    }

    /// Cancel a pending launch even when its original request is still in flight.
    pub fn cancel_unseen_shell(&self, key: &str) -> Result<()> {
        let id = self.shell_launch_id(key);
        let path = self.record_path(&id)?;
        let closed = Terminal {
            id,
            cwd: PathBuf::new(),
            shell: String::new(),
            name: String::new(),
            conversation_id: None,
            tool_call_id: None,
            closed: true,
            argv_digest: None,
        };
        let mut file = NamedTempFile::new_in(path.parent().context("Missing terminal directory")?)?;
        serde_json::to_writer(&mut file, &closed)?;
        file.as_file().sync_all()?;
        match file.persist_noclobber(path) {
            Ok(_) => Ok(()),
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn launch_shell(
        &self,
        cwd: &Path,
        cols: u16,
        rows: u16,
        key: Option<&str>,
    ) -> Result<Terminal> {
        #[cfg(windows)]
        let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
        #[cfg(not(windows))]
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let id = match key {
            Some(key) => self.shell_launch_id(key),
            None => random_id()?,
        };
        self.launch(id, cwd, &[shell], None, None, cols, rows).await
    }
    pub async fn launch_agent_pty(
        &self,
        conversation_id: &str,
        tool_call_id: &str,
        cwd: &Path,
        argv: &[String],
    ) -> Result<String> {
        ensure!(
            !conversation_id.is_empty() && !tool_call_id.is_empty(),
            "Missing PTY launch owner"
        );
        let key = serde_json::to_vec(&(&self.session, conversation_id, tool_call_id))?;
        let id = URL_SAFE_NO_PAD.encode(hash(&key));
        Ok(self
            .launch(
                id,
                cwd,
                argv,
                Some(conversation_id),
                Some(tool_call_id),
                80,
                24,
            )
            .await?
            .id)
    }
    #[allow(clippy::too_many_arguments)]
    async fn launch(
        &self,
        id: String,
        cwd: &Path,
        argv: &[String],
        conversation: Option<&str>,
        call: Option<&str>,
        cols: u16,
        rows: u16,
    ) -> Result<Terminal> {
        let cwd = fs::canonicalize(cwd)?;
        // cmd.exe treats a verbatim local drive path as a network path and
        // silently starts in the Windows system directory.
        #[cfg(windows)]
        let cwd = {
            use std::path::{Component, Prefix};
            let mut parts = cwd.components();
            if let Some(Component::Prefix(prefix)) = parts.next()
                && let Prefix::VerbatimDisk(drive) = prefix.kind()
            {
                let mut ordinary = PathBuf::from(format!("{}:/", char::from(drive)));
                ordinary.extend(parts.filter(|c| !matches!(c, Component::RootDir)));
                ordinary
            } else {
                cwd
            }
        };
        ensure!(cwd.is_dir(), "Terminal working directory is unavailable");
        let shell = Path::new(argv.first().context("Empty terminal program")?)
            .file_name()
            .context("Empty terminal program")?
            .to_string_lossy()
            .into_owned();
        let name = hex(&hash(id.as_bytes())[..16]);
        let terminal = Terminal {
            id,
            cwd,
            shell,
            name,
            closed: false,
            argv_digest: Some(hex(&hash(&serde_json::to_vec(argv)?))),
            conversation_id: conversation.map(str::to_owned),
            tool_call_id: call.map(str::to_owned),
        };
        // Verify the runtime before reserving a durable external effect.
        let mut rpc = wire::Connection::open(self, false).await?;
        let path = self.record_path(&terminal.id)?;
        if !path.exists() {
            let active = rpc.call("terminal.list", json!({})).await?;
            ensure!(
                active["terminals"]
                    .as_array()
                    .context("Invalid Herdr listing")?
                    .iter()
                    .filter(|t| t["state"] == "live")
                    .count()
                    < 16,
                "Terminal capacity reached"
            );
        }
        let mut tmp = NamedTempFile::new_in(path.parent().context("Invalid terminal directory")?)?;
        serde_json::to_writer(&mut tmp, &terminal)?;
        tmp.as_file().sync_all()?;
        match tmp.persist_noclobber(path) {
            Ok(_) => {}
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                // After a lost acknowledgement never repeat a program. Recover the named
                // record from Core or surface an uncertain outcome to the caller.
                let existing = self.lookup(&terminal.id)?.context("Terminal was removed")?;
                ensure!(
                    existing.cwd == terminal.cwd
                        && existing.shell == terminal.shell
                        && existing
                            .argv_digest
                            .as_ref()
                            .is_none_or(|d| Some(d) == terminal.argv_digest.as_ref()),
                    "Conflicting terminal launch"
                );
                rpc.call("terminal.get", json!({"terminal":existing.name}))
                    .await
                    .context("Uncertain previous terminal launch")?;
                return Ok(existing);
            }
            Err(e) => return Err(e.into()),
        }
        rpc.call("terminal.run",json!({"name":terminal.name,"argv":argv,"cwd":terminal.cwd,"size":{"cols":cols,"rows":rows},"no_parent":true,"no_notify":true})).await?;
        Ok(terminal)
    }
    pub async fn attach(&self, t: &Terminal, cols: u16, rows: u16) -> Result<Attach> {
        wire::Attach::open(self, &t.name, cols, rows).await
    }
    pub(crate) async fn snapshot(&self, t: &Terminal) -> Result<String> {
        let value = wire::Connection::open(self, false)
            .await?
            .call("terminal.read", json!({"terminal":t.name,"lines":200}))
            .await?;
        Ok(value["text"]
            .as_str()
            .context("Missing terminal snapshot")?
            .to_owned())
    }
    pub async fn status(&self, t: &Terminal) -> Result<Value> {
        wire::Connection::open(self, false)
            .await?
            .call("terminal.get", json!({"terminal":t.name}))
            .await
    }
    pub async fn kill(&self, t: &Terminal) -> Result<()> {
        let mut rpc = wire::Connection::open(self, false).await?;
        let state = rpc.call("terminal.get", json!({"terminal":t.name})).await?;
        if state["state"] != "finished" {
            rpc.call("terminal.kill", json!({"terminal":t.name}))
                .await?;
        }
        // Retain the receipt so delayed retries cannot relaunch an ended process.
        let path = self.record_path(&t.id)?;
        let mut receipt = t.clone();
        receipt.closed = true;
        let mut file = NamedTempFile::new_in(path.parent().context("Missing terminal directory")?)?;
        serde_json::to_writer(&mut file, &receipt)?;
        file.as_file().sync_all()?;
        file.persist(path)?;
        Ok(())
    }
}
pub fn valid_id(s: &str) -> bool {
    s.len() == 43
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
