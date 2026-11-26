use crate::db;
use anyhow::{Context, Result, bail, ensure};
use directories::ProjectDirs;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fs::{self, File, OpenOptions}, path::{Path, PathBuf}};
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
    User { text: String },
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

    /// Keep the first `turns` turns. A summary of removed turns goes with them; left in place it
    /// would apply again once the chat had grown past them.
    pub fn rewind(&mut self, turns: usize) {
        self.turns.truncate(turns);
        if self.active_summary().is_none() {
            self.summary = None;
        }
    }
}

/// Where chats were kept before `mercury.db`. Read until the chat is next opened by its owner.
const LEGACY: &str = "conversation.json";
/// The running turn between two full saves of `LEGACY`.
const LEGACY_CHECKPOINT: &str = "conversation.running.json";

#[derive(Serialize, Deserialize)]
struct Checkpoint {
    /// The full save this belongs to; any other checkpoint is stale.
    revision: u64,
    index: usize,
    turn: Turn,
}

/// The saved conversation.
pub fn read_history(dir: &Path) -> Result<History> {
    Ok(read_record(dir)?.unwrap_or_default().history)
}

/// The saved conversation and the gateway state stored with it.
pub fn read_conversation(dir: &Path) -> Result<(History, Option<Value>)> {
    Ok(saved_conversation(dir)?.unwrap_or_default())
}

/// As `read_conversation`, and whether there is one.
pub fn saved_conversation(dir: &Path) -> Result<Option<(History, Option<Value>)>> {
    Ok(read_record(dir)?.map(|record| (record.history, record.authority)))
}

pub fn has_conversation(dir: &Path) -> Result<bool> {
    let (root, id) = db::place(dir);
    if let Some(db) = db::open_existing(&root)? && stored(&db, &id)? {
        return Ok(true);
    }
    Ok(dir.join(LEGACY).is_file())
}

/// Save a chat that does not exist yet.
pub fn create_conversation(dir: &Path, history: &History) -> Result<()> {
    let (root, id) = db::place(dir);
    let mut db = db::open(&root)?;
    let change = db::writing(&mut db)?;
    ensure!(!stored(&change, &id)?, "This conversation already exists");
    write_changes(&change, &id, None, &NativeRecord { history: history.clone(), authority: None })?;
    change.commit().context("Could not save the conversation")
}

/// The chat in the JSON form that backups use.
pub fn conversation_json(dir: &Path) -> Result<Option<Value>> {
    read_record(dir)?.map(|record| serde_json::to_value(record).context("Could not write the conversation")).transpose()
}

/// Replace a chat nobody has open with its JSON form.
pub fn restore_conversation(dir: &Path, saved: Value) -> Result<()> {
    let record: NativeRecord = serde_json::from_value(saved).context("Could not read the saved conversation")?;
    ensure!(record.history.version == 1, "Unsupported conversation format {}", record.history.version);
    let (root, id) = db::place(dir);
    let mut db = db::open(&root)?;
    let change = db::writing(&mut db)?;
    change.execute("DELETE FROM conversations WHERE id = ?1", [&id])?;
    write_changes(&change, &id, None, &record)?;
    change.commit().context("Could not save the conversation")
}

/// Remove a chat from its profile's database. Its directory is the caller's.
pub fn delete_conversation(dir: &Path) -> Result<()> {
    let (root, id) = db::place(dir);
    if let Some(db) = db::open_existing(&root)? {
        if db.execute("DELETE FROM conversations WHERE id = ?1", [&id])? > 0 {
            db::settle(&db)?;
        }
    }
    Ok(())
}

fn read_record(dir: &Path) -> Result<Option<NativeRecord>> {
    let (root, id) = db::place(dir);
    let load = || -> Result<Option<NativeRecord>> {
        match db::open_existing(&root)? {
            Some(mut db) => load_record(&mut db, &id),
            None => Ok(None),
        }
    };
    if let Some(record) = load()? {
        return Ok(Some(record));
    }
    match read_legacy(dir)? {
        Some(record) => Ok(Some(record)),
        // Its owner may have moved it into the database in between.
        None => load(),
    }
}

fn stored(db: &rusqlite::Connection, id: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS (SELECT 1 FROM conversations WHERE id = ?1)", [id], |row| row.get(0))?)
}

fn load_record(db: &mut rusqlite::Connection, id: &str) -> Result<Option<NativeRecord>> {
    // One transaction, so that the parts belong to the same save.
    let read = db.transaction()?;
    let Some((version, summary, authority)) = read
        .query_row("SELECT format, summary, authority FROM conversations WHERE id = ?1", [id], |row| {
            Ok((row.get::<_, u32>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, Option<String>>(2)?))
        })
        .optional()?
    else {
        return Ok(None);
    };
    let bodies = |table: &str| -> Result<Vec<String>> {
        Ok(read.prepare(&format!("SELECT body FROM {table} WHERE conversation = ?1 ORDER BY position"))?
            .query_map([id], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?)
    };
    const DAMAGED: &str = "Could not read the saved conversation";
    let turns = bodies("turns")?.iter().map(|body| serde_json::from_str(body)).collect::<serde_json::Result<_>>().context(DAMAGED)?;
    let summary = summary.map(|text| serde_json::from_str(&text)).transpose().context(DAMAGED)?;
    let mut authority: Option<Value> = authority.map(|text| serde_json::from_str(&text)).transpose().context(DAMAGED)?;
    if let Some(Value::Object(authority)) = &mut authority
        && authority.get("receipts").is_some_and(Value::is_null)
    {
        let receipts = bodies("receipts")?.iter().map(|body| serde_json::from_str(body)).collect::<serde_json::Result<_>>().context(DAMAGED)?;
        authority.insert("receipts".into(), Value::Array(receipts));
    }
    Ok(Some(NativeRecord { history: History { version, turns, summary }, authority }))
}

/// The authority without its receipts, which are stored one by one.
fn split(authority: Option<&Value>) -> (Option<Value>, &[Value]) {
    let Some(Value::Array(receipts)) = authority.and_then(|authority| authority.get("receipts")) else {
        return (authority.cloned(), &[]);
    };
    let head = authority.and_then(Value::as_object).expect("has receipts").iter()
        .map(|(key, value)| (key.clone(), if key == "receipts" { Value::Null } else { value.clone() })).collect();
    (Some(Value::Object(head)), receipts)
}

/// Write what differs from `before`, what the database holds now.
fn write_changes(change: &rusqlite::Transaction<'_>, id: &str, before: Option<&NativeRecord>, after: &NativeRecord) -> Result<()> {
    let (head, receipts) = split(after.authority.as_ref());
    let (old_head, old_receipts) = split(before.and_then(|record| record.authority.as_ref()));
    if before.is_none_or(|before| before.history.version != after.history.version || before.history.summary != after.history.summary || old_head != head) {
        change.execute(
            "INSERT INTO conversations (id, format, summary, authority) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (id) DO UPDATE SET format = excluded.format, summary = excluded.summary, authority = excluded.authority",
            rusqlite::params![
                id, after.history.version,
                after.history.summary.as_ref().map(serde_json::to_string).transpose()?,
                head.as_ref().map(serde_json::to_string).transpose()?,
            ],
        )?;
    }
    let old_turns = before.map_or(&[][..], |record| &record.history.turns);
    write_rows(change, "turns", id, old_turns, &after.history.turns)?;
    write_rows(change, "receipts", id, old_receipts, receipts)
}

fn write_rows<T: Serialize + PartialEq>(change: &rusqlite::Transaction<'_>, table: &str, id: &str, before: &[T], after: &[T]) -> Result<()> {
    let mut save = change.prepare_cached(&format!(
        "INSERT INTO {table} (conversation, position, body) VALUES (?1, ?2, ?3)
         ON CONFLICT (conversation, position) DO UPDATE SET body = excluded.body"
    ))?;
    for (position, row) in after.iter().enumerate().filter(|(position, row)| before.get(*position) != Some(*row)) {
        save.execute(rusqlite::params![id, i64::try_from(position)?, serde_json::to_string(row)?])?;
    }
    if after.len() < before.len() {
        change.execute(&format!("DELETE FROM {table} WHERE conversation = ?1 AND position >= ?2"),
            rusqlite::params![id, i64::try_from(after.len())?])?;
    }
    Ok(())
}

fn read_legacy(dir: &Path) -> Result<Option<NativeRecord>> {
    #[derive(Deserialize)]
    struct Saved {
        #[serde(flatten)]
        record: NativeRecord,
        #[serde(default)]
        revision: u64,
    }
    let Saved { mut record, revision } = match File::open(dir.join(LEGACY)) {
        Ok(file) => serde_json::from_reader(std::io::BufReader::new(file)).context("Could not read the saved conversation")?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).context("Could not open the saved conversation"),
    };
    // A checkpoint was written whole or not at all; one that cannot be read loses only its turn's progress.
    if let Ok(bytes) = fs::read(dir.join(LEGACY_CHECKPOINT))
        && let Ok(saved) = serde_json::from_slice::<Checkpoint>(&bytes)
        && saved.revision == revision
        && saved.index + 1 == record.history.turns.len()
    {
        record.history.turns[saved.index] = saved.turn;
    }
    Ok(Some(record))
}

/// The transcript and, in the gateway, its state: saved together.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct NativeRecord {
    #[serde(flatten)]
    history: History,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    authority: Option<Value>,
}

/// Gateway participants coordinate through one handle so a streaming save cannot replace a newly
/// admitted prompt.
pub struct Store {
    id: String,
    saved: std::sync::Mutex<Saved>,
    _lock: File,
}

struct Saved {
    db: rusqlite::Connection,
    /// What the database holds; `None` before the first save. Only this handle writes the chat.
    record: Option<NativeRecord>,
}

impl Store {
    pub fn default_dir() -> Result<PathBuf> {
        if let Some(path) = std::env::var_os("HERMES_RUST_DATA_DIR") { return Ok(path.into()); }
        Self::application_dir()
    }

    pub(crate) fn application_dir() -> Result<PathBuf> {
        let dirs = ProjectDirs::from("org", "HermesRust", "Hermes Rust Desktop")
            .context("Could not locate the application data directory")?;
        Ok(dirs.data_local_dir().to_path_buf())
    }

    pub fn open(dir: &Path) -> Result<(Self, History)> {
        fs::create_dir_all(dir).context("Could not create the conversation directory")?;
        let lock = OpenOptions::new().create(true).truncate(false).read(true).write(true)
            .open(dir.join("session.lock")).context("Could not open the conversation lock")?;
        crate::file_lock::exclusive(&lock).context("This conversation is already open in another window or cannot be locked")?;
        let (root, id) = db::place(dir);
        let mut db = db::open(&root)?;
        let mut record = load_record(&mut db, &id)?;
        if record.is_none() && let Some(legacy) = read_legacy(dir)? {
            ensure!(legacy.history.version == 1, "Unsupported conversation format {}", legacy.history.version);
            let change = db::writing(&mut db)?;
            write_changes(&change, &id, None, &legacy)?;
            change.commit().context("Could not move the conversation into the chat database")?;
            let moved = load_record(&mut db, &id)?;
            ensure!(moved.as_ref() == Some(&legacy), "The conversation did not move into the chat database intact; its file was kept");
            db::retire(&dir.join(LEGACY));
            db::retire(&dir.join(LEGACY_CHECKPOINT));
            record = moved;
        }
        let mut history = record.as_ref().map(|record| record.history.clone()).unwrap_or_default();
        if history.version != 1 { bail!("Unsupported conversation format {}", history.version); }
        let mut recovered = false;
        // Only the gateway can resolve the disposition of a claimed admission.
        if record.as_ref().is_none_or(|record| record.authority.is_none()) {
            for turn in &mut history.turns {
                if turn.status == Status::Running {
                    turn.status = Status::Interrupted;
                    recovered = true;
                }
            }
        }
        let store = Self { id, saved: Saved { db, record }.into(), _lock: lock };
        if recovered { store.save(&history)?; }
        Ok((store, history))
    }

    pub fn authority(&self) -> Option<Value> {
        self.saved.lock().expect("conversation writer poisoned").record.as_ref()?.authority.clone()
    }

    pub fn commit(&self, history: Option<&History>, authority: &Value) -> Result<()> {
        self.replace(history, Some(authority))
    }

    pub fn save(&self, history: &History) -> Result<()> {
        self.replace(Some(history), None)
    }

    fn replace(&self, history: Option<&History>, authority: Option<&Value>) -> Result<()> {
        let mut saved = self.saved.lock().expect("conversation writer poisoned");
        let Saved { db, record } = &mut *saved;
        let next = NativeRecord {
            history: history.or(record.as_ref().map(|record| &record.history)).cloned().unwrap_or_default(),
            authority: authority.or(record.as_ref().and_then(|record| record.authority.as_ref())).cloned(),
        };
        if record.as_ref() == Some(&next) {
            return Ok(());
        }
        let change = db::writing(db)?;
        write_changes(&change, &self.id, record.as_ref(), &next)?;
        change.commit().context("Could not save the conversation")?;
        *record = Some(next);
        Ok(())
    }
}

#[cfg(test)]
mod storage_tests {
    use super::*;
    use serde_json::json;

    fn running(text: &str) -> Turn {
        Turn { attachments: vec![], prompt: "p".into(), context: String::new(), model: "m".into(),
            entries: vec![Entry::Assistant { text: text.into(), calls: vec![] }], status: Status::Running }
    }

    /// A chat as an earlier version saved it.
    fn legacy(dir: &Path, history: &History, authority: Option<Value>, revision: u64) {
        let mut saved = serde_json::to_value(history).unwrap();
        if let Some(authority) = authority { saved["authority"] = authority; }
        saved["revision"] = json!(revision);
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(LEGACY), serde_json::to_vec(&saved).unwrap()).unwrap();
    }

    fn rows_written(store: &Store, save: impl FnOnce()) -> u64 {
        let count = || store.saved.lock().unwrap().db.total_changes();
        let before = count();
        save();
        count() - before
    }

    #[test]
    fn saves_write_only_the_rows_that_changed_and_survive_a_crash() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("sessions/chat");
        let (store, mut history) = Store::open(&dir).unwrap();
        for n in 0..50 {
            history.turns.push(Turn { status: Status::Complete, ..running(&format!("old {n}")) });
        }
        history.turns.push(running(""));
        store.save(&history).unwrap();
        history.turns[50] = running("partial reply");
        assert_eq!(rows_written(&store, || store.save(&history).unwrap()), 1);
        assert_eq!(rows_written(&store, || store.save(&history).unwrap()), 0);
        assert_eq!(read_history(&dir).unwrap(), history);
        drop(store);

        // Reopening after a crash keeps what had streamed.
        let (store, mut reopened) = Store::open(&dir).unwrap();
        assert_eq!(reopened.turns[50].status, Status::Interrupted);
        assert!(matches!(&reopened.turns[50].entries[0], Entry::Assistant { text, .. } if text == "partial reply"));
        reopened.turns.truncate(2);
        store.save(&reopened).unwrap();
        assert_eq!(read_history(&dir).unwrap(), reopened);
        assert!(!dir.join(LEGACY).exists());
    }

    #[test]
    fn a_commit_saves_the_transcript_and_the_gateway_state_together() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("sessions/chat");
        let (store, mut history) = Store::open(&dir).unwrap();
        history.turns.push(running("a"));
        let mut authority = json!({"version":1,"generation":4,"receipts":[{"id":"one"},{"id":"two"}]});
        store.commit(Some(&history), &authority).unwrap();
        assert_eq!(read_conversation(&dir).unwrap(), (history.clone(), Some(authority.clone())));
        authority["receipts"][1]["status"] = json!("terminal");
        assert_eq!(rows_written(&store, || store.commit(None, &authority).unwrap()), 1);
        history.turns[0] = running("ab");
        store.save(&history).unwrap();
        for changed in [json!({"receipts":[{"id":"two"}]}), json!({"receipts":[]}), json!({"receipts":null}), json!({"other":1})] {
            store.commit(None, &changed).unwrap();
            let (saved, read) = read_conversation(&dir).unwrap();
            assert_eq!(saved, history);
            // A missing list and an empty one mean the same to the gateway.
            let expected = if changed["receipts"].is_null() && changed.get("receipts").is_some() { json!({"receipts":[]}) } else { changed };
            assert_eq!(read.unwrap(), expected);
        }
        drop(store);
        // The owner, not the engine, decides about a turn that was running.
        assert_eq!(Store::open(&dir).unwrap().1.turns[0].status, Status::Running);
    }

    #[test]
    fn a_json_chat_moves_once_with_its_running_turn_and_keeps_its_file() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("sessions/chat");
        let mut history = History { summary: Some(Summary { through: 1, text: "earlier".into() }), ..Default::default() };
        history.turns = vec![Turn { status: Status::Complete, ..running("done") }, running("")];
        let authority = json!({"version":1,"receipts":[{"id":"r","n":[1,2]}],"staged":{"x":1}});
        legacy(&dir, &history, Some(authority.clone()), 7);
        let checkpoint = Checkpoint { revision: 7, index: 1, turn: running("streamed") };
        fs::write(dir.join(LEGACY_CHECKPOINT), serde_json::to_vec(&checkpoint).unwrap()).unwrap();
        let original = fs::read(dir.join(LEGACY)).unwrap();
        history.turns[1] = running("streamed");

        // Reading does not move it.
        assert!(has_conversation(&dir).unwrap());
        assert_eq!(read_conversation(&dir).unwrap(), (history.clone(), Some(authority.clone())));
        assert!(!root.path().join(db::FILE).exists());

        let (store, opened) = Store::open(&dir).unwrap();
        assert_eq!(opened, history);
        assert_eq!(store.authority(), Some(authority.clone()));
        assert!(!dir.join(LEGACY).exists() && !dir.join(LEGACY_CHECKPOINT).exists());
        assert_eq!(fs::read(dir.join(format!("{LEGACY}.{}", db::MIGRATED))).unwrap(), original);
        drop(store);

        // A file that could not be renamed, or that an older version left, does not replace the chat.
        legacy(&dir, &History::default(), None, 1);
        assert_eq!(Store::open(&dir).unwrap().1, history);
        assert_eq!(read_conversation(&dir).unwrap().1, Some(authority));
    }

    /// A process that dies while saving leaves the last whole save.
    #[test]
    fn a_killed_writer_leaves_whole_saves() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("sessions/chat");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "history::storage_tests::child_saves_until_killed", "--nocapture"])
            .env("HERMES_TEST_SAVES", &dir).stdout(std::process::Stdio::null()).spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while read_history(&dir).map_or(0, |history| history.turns.len()) < 5 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        child.kill().unwrap();
        child.wait().unwrap();
        let (history, authority) = read_conversation(&dir).unwrap();
        assert!(history.turns.len() >= 5);
        // Each save adds one turn and one receipt. Neither is ever without the other.
        assert_eq!(authority.unwrap()["receipts"].as_array().unwrap().len(), history.turns.len());
        assert_eq!(Store::open(&dir).unwrap().1, history);
    }

    #[test]
    fn child_saves_until_killed() {
        let Some(dir) = std::env::var_os("HERMES_TEST_SAVES") else { return };
        let (store, mut history) = Store::open(Path::new(&dir)).unwrap();
        let mut receipts = Vec::new();
        loop {
            history.turns.push(Turn { status: Status::Complete, ..running(&"x".repeat(20_000)) });
            receipts.push(json!({"turn": history.turns.len(), "text": "y".repeat(20_000)}));
            store.commit(Some(&history), &json!({"receipts": receipts})).unwrap();
        }
    }

    #[test]
    fn a_long_chat_does_not_rewrite_itself_for_one_more_reply() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("sessions/chat");
        let (store, mut history) = Store::open(&dir).unwrap();
        let mut authority = json!({"generation": 0, "receipts": []});
        for n in 0..400 {
            history.turns.push(Turn { status: Status::Complete, ..running(&"z".repeat(2_000)) });
            authority["receipts"].as_array_mut().unwrap().push(json!({"n": n, "text": "r".repeat(2_000)}));
        }
        store.commit(Some(&history), &authority).unwrap();
        history.turns.push(running(""));
        authority["generation"] = json!(1);
        authority["receipts"].as_array_mut().unwrap().push(json!({"n": 400}));
        assert_eq!(rows_written(&store, || store.commit(Some(&history), &authority).unwrap()), 3, "the turn, its receipt and the state");
        assert_eq!(read_conversation(&dir).unwrap(), (history, Some(authority)));
    }

    #[test]
    fn a_damaged_json_chat_is_reported_and_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("sessions/chat");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(LEGACY), b"{\"version\":1,\"turns\":[").unwrap();
        assert!(Store::open(&dir).is_err());
        assert_eq!(fs::read(dir.join(LEGACY)).unwrap(), b"{\"version\":1,\"turns\":[");
        legacy(&dir, &History { version: 2, ..Default::default() }, None, 1);
        assert!(Store::open(&dir).is_err());
        assert!(dir.join(LEGACY).is_file());
        // A checkpoint from another save is not applied.
        let mut history = History::default();
        history.turns.push(Turn { status: Status::Complete, ..running("saved") });
        legacy(&dir, &history, None, 2);
        let stale = Checkpoint { revision: 1, index: 0, turn: running("stale") };
        fs::write(dir.join(LEGACY_CHECKPOINT), serde_json::to_vec(&stale).unwrap()).unwrap();
        assert_eq!(Store::open(&dir).unwrap().1, history);
    }

    #[test]
    fn chats_share_a_database_and_are_created_backed_up_and_deleted_apart() {
        let root = tempfile::tempdir().unwrap();
        let (a, b) = (root.path().join("sessions/a"), root.path().join("sessions/b"));
        let mut history = History::default();
        history.turns.push(Turn { status: Status::Complete, ..running("kept") });
        create_conversation(&a, &history).unwrap();
        assert!(create_conversation(&a, &history).is_err());
        let (open_a, _) = Store::open(&a).unwrap();
        assert!(Store::open(&a).is_err(), "one owner at a time");
        restore_conversation(&b, conversation_json(&a).unwrap().unwrap()).unwrap();
        let (open_b, mut other) = Store::open(&b).unwrap();
        other.turns.push(Turn { status: Status::Complete, ..running("only b") });
        open_b.save(&other).unwrap();
        assert_eq!(read_history(&a).unwrap(), history);
        assert_eq!(read_history(&b).unwrap(), other);
        drop((open_a, open_b));
        delete_conversation(&a).unwrap();
        assert!(!has_conversation(&a).unwrap() && conversation_json(&a).unwrap().is_none());
        assert_eq!(read_history(&b).unwrap(), other);
        assert!(restore_conversation(&a, json!({"version":3,"turns":[]})).is_err());
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
    use std::{io::Write, os::windows::fs::OpenOptionsExt};

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
