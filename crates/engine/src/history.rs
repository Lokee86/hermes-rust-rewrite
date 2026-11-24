use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{fs::{self, File, OpenOptions}, io::Write, path::{Path, PathBuf}};
use tempfile::NamedTempFile;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Entry {
    Assistant { text: String, calls: Vec<ToolCall> },
    Tool { id: String, name: String, approved: bool, result: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
pub enum Status { Running, Complete, Interrupted, Failed(String) }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Turn {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<crate::Attachment>,
    pub prompt: String,
    /// What the prompt's @-references expanded to when it was sent (agent/context_references.py
    /// "--- Attached Context ---"). Frozen with the turn so later rounds resend identical bytes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub context: String,
    pub model: String,
    pub entries: Vec<Entry>,
    pub status: Status,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct History {
    version: u32,
    pub turns: Vec<Turn>,
}

impl Default for History {
    fn default() -> Self { Self { version: 1, turns: Vec::new() } }
}

/// Serialized alongside the transcript. Gateway participants coordinate through one
/// handle so a streaming checkpoint cannot replace a newly admitted prompt.
#[derive(Clone, Default, Serialize, Deserialize)]
struct NativeRecord {
    #[serde(flatten)]
    history: History,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    authority: Option<serde_json::Value>,
}

pub struct Store {
    dir: PathBuf,
    _lock: File,
    record: std::sync::Mutex<NativeRecord>,
}

impl Store {
    pub fn default_dir() -> Result<PathBuf> {
        if let Some(path) = std::env::var_os("HERMES_RUST_DATA_DIR") { return Ok(path.into()); }
        let dirs = ProjectDirs::from("org", "HermesRust", "Hermes Rust Desktop")
            .context("Could not locate the application data directory")?;
        Ok(dirs.data_local_dir().to_path_buf())
    }

    pub fn open(dir: &Path) -> Result<(Self, History)> {
        fs::create_dir_all(dir).context("Could not create the conversation directory")?;
        let lock = OpenOptions::new().create(true).truncate(false).read(true).write(true)
            .open(dir.join("session.lock")).context("Could not open the conversation lock")?;
        lock.try_lock_exclusive().context("This conversation is already open in another window or cannot be locked")?;
        let path = dir.join("conversation.json");
        let mut record: NativeRecord = match File::open(&path) {
            Ok(file) => serde_json::from_reader(file).context("Could not read the saved conversation")?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => NativeRecord::default(),
            Err(err) => return Err(err).context("Could not open the saved conversation"),
        };
        if record.history.version != 1 { bail!("Unsupported conversation format {}", record.history.version); }
        let mut recovered = false;
        // Only the gateway can resolve the disposition of a claimed admission.
        if record.authority.is_none() {
            for turn in &mut record.history.turns {
                if turn.status == Status::Running {
                    turn.status = Status::Interrupted;
                    recovered = true;
                }
            }
        }
        let history = record.history.clone();
        let store = Self { dir: dir.to_path_buf(), _lock: lock, record: record.into() };
        if recovered { store.save(&history)?; }
        Ok((store, history))
    }

    pub fn authority(&self) -> Option<serde_json::Value> {
        self.record.lock().expect("conversation writer poisoned").authority.clone()
    }

    pub fn commit(&self, history: Option<&History>, authority: &serde_json::Value) -> Result<()> {
        self.replace(history, Some(authority))
    }

    pub fn save(&self, history: &History) -> Result<()> {
        self.replace(Some(history), None)
    }

    fn replace(&self, history: Option<&History>, authority: Option<&serde_json::Value>) -> Result<()> {
        let mut current = self.record.lock().expect("conversation writer poisoned");
        let next = NativeRecord {
            history: history.unwrap_or(&current.history).clone(),
            authority: authority.or(current.authority.as_ref()).cloned(),
        };
        let mut file = NamedTempFile::new_in(&self.dir)?;
        serde_json::to_writer_pretty(&mut file, &next)?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        persist_with_retry(file, &self.dir.join("conversation.json"))
            .context("Could not replace the saved conversation")?;
        *current = next;
        #[cfg(unix)]
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }
}

#[cfg(not(windows))]
pub(crate) fn persist_with_retry(file: NamedTempFile, path: &Path) -> Result<File, tempfile::PersistError> {
    file.persist(path)
}

#[cfg(windows)]
pub(crate) fn persist_with_retry(mut file: NamedTempFile, path: &Path) -> Result<File, tempfile::PersistError> {
    // Windows may briefly deny replacing a file held by a reader. Retry the same
    // synced file, keeping the previous version intact on every failure.
    for delay_ms in [5, 10, 20, 40, 80, 160] {
        match file.persist(path) {
            Err(error) if matches!(error.error.raw_os_error(), Some(5 | 32 | 33)) => {
                file = error.file;
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            }
            result => return result,
        }
    }
    file.persist(path)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;

    #[test]
    fn atomic_conversation_replace_retries_a_reader_and_preserves_the_previous_record_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversation.json");
        fs::write(&path, b"old").unwrap();
        let hold_reader = || OpenOptions::new().read(true).share_mode(1).open(&path).unwrap();
        let pending = |data: &[u8]| {
            let mut file = NamedTempFile::new_in(dir.path()).unwrap();
            file.write_all(data).unwrap();
            file.as_file().sync_all().unwrap();
            file
        };
        let reader = hold_reader();
        let denied = pending(b"new").persist(&path).unwrap_err();
        assert!(matches!(denied.error.raw_os_error(), Some(5 | 32 | 33)));
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            drop(reader);
        });
        persist_with_retry(denied.file, &path).unwrap();
        release.join().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");

        let reader = hold_reader();
        let denied = persist_with_retry(pending(b"next"), &path).unwrap_err();
        assert!(matches!(denied.error.raw_os_error(), Some(5 | 32 | 33)));
        drop(reader);
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(fs::read(denied.file.path()).unwrap(), b"next");
    }
}
