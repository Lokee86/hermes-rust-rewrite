//! A profile's chat database, `mercury.db`: chats, the gateway's turn records, the chat and project
//! catalog, and routines with their runs. Mercury's own file; it never opens a Hermes profile's
//! `state.db`. Settings, drafts, attachments and credentials stay in their own files.
//!
//! Connections are short-lived, except that an open chat keeps one. Writers hold the same file locks
//! as before (`session.lock`, `library.lock`, `cron/.jobs.lock`); SQLite makes each change atomic.
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use rusqlite::{Connection, config::DbConfig, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params};
use std::{
    collections::HashMap,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Duration,
};

pub const FILE: &str = "mercury.db";
/// Appended to a JSON file whose contents are now in the database.
pub const MIGRATED: &str = "before-sqlite";
const SCHEMA_VERSION: i64 = 1;
const SCHEMA: &str = "
CREATE TABLE conversations (
    id TEXT PRIMARY KEY NOT NULL,
    format INTEGER NOT NULL,
    summary TEXT,
    authority TEXT
);
CREATE TABLE turns (
    conversation TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    body TEXT NOT NULL,
    PRIMARY KEY (conversation, position)
);
CREATE TABLE receipts (
    conversation TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    body TEXT NOT NULL,
    PRIMARY KEY (conversation, position)
);
CREATE TABLE documents (
    name TEXT PRIMARY KEY NOT NULL,
    revision INTEGER NOT NULL,
    body TEXT NOT NULL
);
CREATE TABLE records (
    document TEXT NOT NULL REFERENCES documents(name) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    id TEXT NOT NULL,
    position INTEGER NOT NULL,
    body TEXT NOT NULL,
    PRIMARY KEY (document, kind, id)
);
";

/// The profile a chat directory belongs to, and the chat's name in that profile's database.
pub(crate) fn place(directory: &Path) -> (PathBuf, String) {
    if let (Some(parent), Some(name)) = (directory.parent(), directory.file_name())
        && let Some(kind) = parent.file_name().and_then(|kind| kind.to_str())
        && matches!(kind, "sessions" | "subagents")
        && let Some(root) = parent.parent()
    {
        return (root.to_path_buf(), format!("{kind}/{}", name.to_string_lossy()));
    }
    (directory.to_path_buf(), ".".into())
}

fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(metadata.file_type().is_file(), "The chat database must be a regular file");
            Ok(true)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("Could not open the chat database"),
    }
}

/// The profile's database, if it has one yet.
pub fn open_existing(root: &Path) -> Result<Option<Connection>> {
    let path = root.join(FILE);
    if !exists(&path)? {
        return Ok(None);
    }
    Ok(Some(connect(&path)?))
}

/// The profile's database, created if needed.
pub fn open(root: &Path) -> Result<Connection> {
    let path = root.join(FILE);
    if !exists(&path)? {
        let mut options = fs::OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600); // SQLite gives its companion files the same permissions.
        }
        options.open(&path).context("Could not create the chat database")?;
    }
    connect(&path)
}

fn connect(path: &Path) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    // NOFOLLOW refuses a link anywhere in the path, and a system's temporary folder may be one.
    let path = path.canonicalize().context("Could not open the chat database")?;
    let mut db = Connection::open_with_flags(&path, flags).context("Could not open the chat database")?;
    db.busy_timeout(Duration::from_secs(15))?;
    db.pragma_update(None, "foreign_keys", true)?;
    // Deleted chats are overwritten, and a saved turn survives a power cut.
    db.pragma_update(None, "secure_delete", true)?;
    db.pragma_update(None, "synchronous", "FULL")?;
    db.pragma_update(None, "fullfsync", true)?;
    db.pragma_update(None, "checkpoint_fullfsync", true)?;
    // Most connections last for one small change. Moving the log into the database at each close
    // would flush the disk several more times; SQLite does it when the log has grown.
    db.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
    if version(&db)? != SCHEMA_VERSION {
        // One process sets the database up while others wait.
        let setup = fs::OpenOptions::new().create(true).truncate(false).read(true).write(true)
            .open(path.with_extension("db.setup")).context("Could not set up the chat database")?;
        setup.lock_exclusive().context("Could not set up the chat database")?;
        db.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
        let change = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        match version(&change)? {
            0 => {
                change.execute_batch(SCHEMA).context("Could not set up the chat database")?;
                change.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            SCHEMA_VERSION => {}
            newer => anyhow::bail!("The chat database was made by a newer version of Hermes (format {newer})"),
        }
        change.commit()?;
    }
    Ok(db)
}

fn version(db: &Connection) -> Result<i64> {
    db.query_row("PRAGMA user_version", [], |row| row.get(0)).context("Could not read the chat database")
}

/// Remove what was deleted from the log too.
pub(crate) fn settle(db: &Connection) -> Result<()> {
    // A reader can hold the log. What it keeps is overwritten once it has been used again.
    db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(())).context("Could not settle the chat database")
}

pub(crate) fn writing(db: &mut Connection) -> Result<Transaction<'_>> {
    db.transaction_with_behavior(TransactionBehavior::Immediate).context("The chat database is busy")
}

/// Keep a JSON file that has been read into the database. Failing to rename it changes nothing: the
/// database is read first.
pub fn retire(path: &Path) {
    let mut name = path.as_os_str().to_owned();
    name.push(".");
    name.push(MIGRATED);
    let _ = fs::rename(path, name);
}

/// A consistent copy of a profile's database, for an export. `destination` must not exist yet.
pub fn snapshot(root: &Path, destination: &Path) -> Result<bool> {
    let Some(db) = open_existing(root)? else { return Ok(false) };
    // The copy is opened as the database is, refusing a link anywhere in its path.
    let folder = destination.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let destination = folder.canonicalize().context("Could not copy the chat database")?
        .join(destination.file_name().context("Choose a file for the database copy")?);
    let name = destination.to_str().context("The export path is not valid text")?;
    // The copy holds the chats, so it is private before anything is written to it. VACUUM INTO
    // fills an empty file and keeps its permissions.
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(&destination).context("Could not copy the chat database")?;
    db.execute("VACUUM INTO ?1", [name]).context("Could not copy the chat database")?;
    Ok(true)
}

/// A small document and its ordered records, such as the chat catalog.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Document {
    pub body: String,
    /// `(kind, id, body)`, in order within a kind.
    pub records: Vec<(String, String, String)>,
}

/// Changes whenever the document does.
pub fn document_revision(root: &Path, name: &str) -> Result<Option<i64>> {
    let Some(db) = open_existing(root)? else { return Ok(None) };
    Ok(db.query_row("SELECT revision FROM documents WHERE name = ?1", [name], |row| row.get(0)).optional()?)
}

pub fn record_count(root: &Path, document: &str, kind: &str) -> Result<Option<usize>> {
    let Some(db) = open_existing(root)? else { return Ok(None) };
    if document_body(&db, document)?.is_none() {
        return Ok(None);
    }
    let count: i64 = db.query_row("SELECT count(*) FROM records WHERE document = ?1 AND kind = ?2", [document, kind], |row| row.get(0))?;
    Ok(Some(usize::try_from(count)?))
}

fn document_body(db: &Connection, name: &str) -> Result<Option<String>> {
    Ok(db.query_row("SELECT body FROM documents WHERE name = ?1", [name], |row| row.get(0)).optional()?)
}

pub fn read_document(db: &mut Connection, name: &str) -> Result<Option<Document>> {
    let read = db.transaction()?;
    let Some(body) = document_body(&read, name)? else { return Ok(None) };
    let records = read
        .prepare("SELECT kind, id, body FROM records WHERE document = ?1 ORDER BY kind, position")?
        .query_map([name], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Some(Document { body, records }))
}

/// Write what differs from `before`, which the caller read under its own lock.
pub fn write_document(db: &mut Connection, name: &str, before: Option<&Document>, after: &Document) -> Result<()> {
    if before == Some(after) {
        return Ok(());
    }
    let positioned = |document: &Document| {
        let mut next = HashMap::<String, i64>::new();
        document.records.iter().map(|(kind, id, body)| {
            let position = next.entry(kind.clone()).or_default();
            *position += 1;
            ((kind.clone(), id.clone()), (*position, body.clone()))
        }).collect::<HashMap<_, _>>()
    };
    let old = before.map(positioned).unwrap_or_default();
    let new = positioned(after);
    ensure!(new.len() == after.records.len(), "Duplicate record in {name}");
    let change = writing(db)?;
    change.execute(
        "INSERT INTO documents (name, revision, body) VALUES (?1, 1, ?2)
         ON CONFLICT (name) DO UPDATE SET revision = revision + 1, body = excluded.body",
        params![name, after.body],
    )?;
    {
        let mut remove = change.prepare("DELETE FROM records WHERE document = ?1 AND kind = ?2 AND id = ?3")?;
        for (kind, id) in old.keys().filter(|key| !new.contains_key(*key)) {
            remove.execute([name, kind, id])?;
        }
        let mut save = change.prepare(
            "INSERT INTO records (document, kind, id, position, body) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (document, kind, id) DO UPDATE SET position = excluded.position, body = excluded.body",
        )?;
        for ((kind, id), (position, body)) in &new {
            if old.get(&(kind.clone(), id.clone())) != Some(&(*position, body.clone())) {
                save.execute(params![name, kind, id, position, body])?;
            }
        }
    }
    change.commit().context("Could not save to the chat database")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_keeps_its_order_and_writes_only_what_changed() {
        let dir = tempfile::tempdir().unwrap();
        let row = |kind: &str, id: &str, body: &str| (kind.to_owned(), id.to_owned(), body.to_owned());
        let first = Document { body: "{}".into(), records: vec![row("a", "1", "x"), row("a", "2", "y"), row("b", "1", "z")] };
        let mut db = open(dir.path()).unwrap();
        assert_eq!(read_document(&mut db, "d").unwrap(), None);
        write_document(&mut db, "d", None, &first).unwrap();
        assert_eq!(read_document(&mut db, "d").unwrap().as_ref(), Some(&first));
        assert_eq!(document_revision(dir.path(), "d").unwrap(), Some(1));
        write_document(&mut db, "d", Some(&first), &first).unwrap();
        assert_eq!(document_revision(dir.path(), "d").unwrap(), Some(1));
        let second = Document { body: "{}".into(), records: vec![row("a", "2", "y2"), row("a", "1", "x"), row("c", "9", "new")] };
        write_document(&mut db, "d", Some(&first), &second).unwrap();
        assert_eq!(read_document(&mut db, "d").unwrap().as_ref(), Some(&second));
        assert_eq!(document_revision(dir.path(), "d").unwrap(), Some(2));
        assert_eq!(record_count(dir.path(), "d", "a").unwrap(), Some(2));
        let twice = Document { body: "{}".into(), records: vec![row("a", "1", "x"), row("a", "1", "x")] };
        assert!(write_document(&mut db, "d", Some(&second), &twice).is_err());
        assert_eq!(read_document(&mut db, "d").unwrap(), Some(second));
    }

    #[test]
    fn a_newer_or_damaged_database_is_refused_and_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        open(dir.path()).unwrap().pragma_update(None, "user_version", 99).unwrap();
        assert!(open(dir.path()).unwrap_err().to_string().contains("newer version"));
        let other = tempfile::tempdir().unwrap();
        fs::write(other.path().join(FILE), b"this is not a database, and is long enough to be checked").unwrap();
        let before = fs::read(other.path().join(FILE)).unwrap();
        assert!(open(other.path()).is_err());
        assert_eq!(fs::read(other.path().join(FILE)).unwrap(), before);
    }

    #[test]
    fn many_first_users_set_up_one_database() {
        let dir = tempfile::tempdir().unwrap();
        let threads: Vec<_> = (0..8).map(|n| {
            let root = dir.path().to_owned();
            std::thread::spawn(move || {
                let document = Document { body: "{}".into(), records: vec![] };
                write_document(&mut open(&root).unwrap(), &format!("d{n}"), None, &document).unwrap();
            })
        }).collect();
        for thread in threads { thread.join().unwrap(); }
        for n in 0..8 { assert_eq!(document_revision(dir.path(), &format!("d{n}")).unwrap(), Some(1)); }
    }

    #[cfg(unix)]
    #[test]
    fn the_database_is_private_and_not_a_link() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let db = open(dir.path()).unwrap();
        db.execute("INSERT INTO documents VALUES ('d', 1, '{}')", []).unwrap();
        for file in fs::read_dir(dir.path()).unwrap() {
            let file = file.unwrap();
            assert!(file.file_name() == "mercury.db.setup" || file.metadata().unwrap().permissions().mode() & 0o077 == 0, "{file:?}");
        }
        drop(db);
        let linked = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(dir.path().join(FILE), linked.path().join(FILE)).unwrap();
        assert!(open(linked.path()).is_err());
    }

    #[test]
    fn a_copy_is_private_and_may_be_reached_through_a_link() {
        let dir = tempfile::tempdir().unwrap();
        open(dir.path()).unwrap().execute("INSERT INTO documents VALUES ('d', 1, '{}')", []).unwrap();
        let out = tempfile::tempdir().unwrap();
        let target = out.path().join("real");
        fs::create_dir(&target).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, out.path().join("link")).unwrap();
        #[cfg(not(unix))]
        let _ = fs::create_dir(out.path().join("link"));
        let copy = out.path().join("link").join("copy.db");
        assert!(snapshot(dir.path(), &copy).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&copy).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(snapshot(dir.path(), &copy).is_err(), "an existing file is not replaced");
        let saved = Connection::open(&copy).unwrap().query_row("SELECT body FROM documents WHERE name = 'd'", [], |row| row.get::<_, String>(0)).unwrap();
        assert_eq!(saved, "{}");
    }

    #[test]
    fn chats_are_filed_under_their_profile() {
        assert_eq!(place(Path::new("/p/sessions/chat-1")), ("/p".into(), "sessions/chat-1".into()));
        assert_eq!(place(Path::new("/p/subagents/a")), ("/p".into(), "subagents/a".into()));
        assert_eq!(place(Path::new("/p")), ("/p".into(), ".".into()));
    }
}
