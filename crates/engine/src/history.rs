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
    /// Stands in for the oldest turns in model requests (context.rs). The turns themselves stay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) summary: Option<Summary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Summary {
    /// How many leading turns `text` covers.
    pub through: usize,
    pub text: String,
}

impl Default for History {
    fn default() -> Self { Self { version: 1, turns: Vec::new(), summary: None } }
}

impl History {
    /// The summary, unless the chat was rewound to before the turns it covers.
    pub fn active_summary(&self) -> Option<&Summary> {
        self.summary.as_ref().filter(|summary| summary.through <= self.turns.len())
    }
}

const CONVERSATION: &str = "conversation.json";
/// The running turn between two full saves. A long chat is not rewritten for every streamed chunk.
const CHECKPOINT: &str = "conversation.running.json";

#[derive(Serialize, Deserialize)]
struct Checkpoint {
    /// The full save this belongs to; any other checkpoint is stale.
    revision: u64,
    index: usize,
    turn: Turn,
}

/// The saved conversation with the running turn's latest checkpoint applied.
pub fn read_history(dir: &Path) -> Result<History> {
    Ok(read_record(dir)?.0.history)
}

/// The saved conversation and the gateway state stored beside it.
pub fn read_conversation(dir: &Path) -> Result<(History, Option<serde_json::Value>)> {
    let (record, _) = read_record(dir)?;
    Ok((record.history, record.authority))
}

/// Also reports whether a saved file exists.
fn read_record(dir: &Path) -> Result<(NativeRecord, bool)> {
    let mut record: NativeRecord = match File::open(dir.join(CONVERSATION)) {
        Ok(file) => serde_json::from_reader(std::io::BufReader::new(file)).context("Could not read the saved conversation")?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok((NativeRecord::default(), false)),
        Err(err) => return Err(err).context("Could not open the saved conversation"),
    };
    // A checkpoint is written whole or not at all; one that cannot be read loses only its turn's progress.
    if let Ok(bytes) = fs::read(dir.join(CHECKPOINT))
        && let Ok(saved) = serde_json::from_slice::<Checkpoint>(&bytes)
        && saved.revision == record.revision
        && saved.index + 1 == record.history.turns.len()
    {
        record.history.turns[saved.index] = saved.turn;
    }
    Ok((record, true))
}

/// Serialized alongside the transcript. Gateway participants coordinate through one
/// handle so a streaming checkpoint cannot replace a newly admitted prompt.
#[derive(Clone, Default, Serialize, Deserialize)]
struct NativeRecord {
    #[serde(flatten)]
    history: History,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    authority: Option<serde_json::Value>,
    /// Counts full saves.
    #[serde(default)]
    revision: u64,
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
        let (mut record, _) = read_record(dir)?;
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
        if let (Some(history), None) = (history, authority)
            && current.revision > 0
            && let Some(index) = running_turn_only(&current.history, history)
        {
            let saved = Checkpoint { revision: current.revision, index, turn: history.turns[index].clone() };
            let mut file = NamedTempFile::new_in(&self.dir)?;
            serde_json::to_writer(&mut file, &saved)?;
            file.as_file().sync_all()?;
            persist_with_retry(file, &self.dir.join(CHECKPOINT)).context("Could not save the running reply")?;
            current.history.turns[index] = saved.turn;
            return Ok(());
        }
        let next = NativeRecord {
            history: history.unwrap_or(&current.history).clone(),
            authority: authority.or(current.authority.as_ref()).cloned(),
            revision: current.revision + 1,
        };
        let mut file = NamedTempFile::new_in(&self.dir)?;
        serde_json::to_writer(std::io::BufWriter::new(&mut file), &next)?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        persist_with_retry(file, &self.dir.join(CONVERSATION))
            .context("Could not replace the saved conversation")?;
        *current = next;
        // Its revision no longer matches, so one that cannot be removed is ignored.
        let _ = fs::remove_file(self.dir.join(CHECKPOINT));
        #[cfg(unix)]
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }
}

/// The index of the last turn, if it is running and nothing before it changed.
fn running_turn_only(current: &History, next: &History) -> Option<usize> {
    let index = next.turns.len().checked_sub(1)?;
    (current.turns.len() == next.turns.len()
        && current.summary == next.summary
        && current.turns[index].status == Status::Running
        && next.turns[index].status == Status::Running
        && current.turns[..index] == next.turns[..index])
        .then_some(index)
}

#[cfg(test)]
mod checkpoint_tests {
    use super::*;

    fn running(text: &str) -> Turn {
        Turn { attachments: vec![], prompt: "p".into(), context: String::new(), model: "m".into(),
            entries: vec![Entry::Assistant { text: text.into(), calls: vec![] }], status: Status::Running }
    }

    #[test]
    fn streaming_saves_rewrite_only_the_running_turn_and_survive_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let (store, mut history) = Store::open(dir.path()).unwrap();
        for n in 0..50 {
            history.turns.push(Turn { status: Status::Complete, ..running(&format!("old {n}")) });
        }
        history.turns.push(running(""));
        store.save(&history).unwrap();
        let full = fs::read(dir.path().join(CONVERSATION)).unwrap();
        history.turns[50] = running("partial reply");
        store.save(&history).unwrap();
        assert_eq!(fs::read(dir.path().join(CONVERSATION)).unwrap(), full);
        assert!(fs::metadata(dir.path().join(CHECKPOINT)).unwrap().len() < 400);
        assert_eq!(read_history(dir.path()).unwrap(), history);
        drop(store);

        // Reopening after a crash keeps what had streamed.
        let (store, reopened) = Store::open(dir.path()).unwrap();
        assert_eq!(reopened.turns[50].status, Status::Interrupted);
        assert!(matches!(&reopened.turns[50].entries[0], Entry::Assistant { text, .. } if text == "partial reply"));
        assert!(!dir.path().join(CHECKPOINT).exists());

        // A checkpoint left from an earlier save is not applied to a later one.
        let stale = Checkpoint { revision: 1, index: 50, turn: running("stale") };
        fs::write(dir.path().join(CHECKPOINT), serde_json::to_vec(&stale).unwrap()).unwrap();
        assert_eq!(read_history(dir.path()).unwrap(), reopened);
        drop(store);
    }

    #[test]
    fn a_gateway_commit_is_always_a_full_save() {
        let dir = tempfile::tempdir().unwrap();
        let (store, mut history) = Store::open(dir.path()).unwrap();
        history.turns.push(running("a"));
        store.commit(Some(&history), &serde_json::json!({"receipts":[]})).unwrap();
        history.turns[0] = running("ab");
        store.save(&history).unwrap();
        store.commit(None, &serde_json::json!({"receipts":[1]})).unwrap();
        assert!(!dir.path().join(CHECKPOINT).exists());
        let (saved, authority) = read_conversation(dir.path()).unwrap();
        assert_eq!(saved, history);
        assert_eq!(authority.unwrap()["receipts"][0], 1);
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
