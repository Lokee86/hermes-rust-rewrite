# Native session library

`hermes-library` owns the desktop/gateway conversation catalog. It uses native
Rust filesystem and SQLite libraries and does not invoke the Python Hermes CLI.

```rust,no_run
use hermes_library::{HermesHistory, Library};
use std::path::Path;

let library = Library::open_default()?;
let project = library.add_project(Path::new("/chosen/workspace"))?;
library.select_project(Some(&project.id))?;
let chat = library.create_session(Some(&project.id), None)?;
let workspace = library.workspace_for_session(&chat.id)?.unwrap();
let mut session = hermes_engine::Session::open_in(&chat.directory, &workspace)?;
// Drive session.turn(...), then refresh the sidebar's title and activity:
library.touch_session(&chat.id)?;

// Only after the user chooses an existing database and a listed conversation:
let history = HermesHistory::open(Path::new("/chosen/hermes/state.db"))?;
let available = history.sessions()?;
if let Some(chosen) = available.first() {
    let result = library.import_hermes(&history, &chosen.id, Some(&project.id))?;
    // Display result.warnings and switch to result.session.
}
# Ok::<(), anyhow::Error>(())
```

## Storage and concurrency

The default root is `hermes_engine::Session::default_dir()` and can be overridden
with `HERMES_RUST_DATA_DIR`. Tests pass private temporary directories explicitly.

```
mercury.db                     # chats, gateway turn records, catalog, routines and runs (SQLite)
library.lock                   # brief cross-process catalog lock
sessions/<native-id>/
  session.lock                 # engine's exclusive conversation lock
  attachments/                 # files sent with the chat
  gateway.json, draft.txt, …   # settings and unsent text for the chat
  hermes-source.json           # imported upstream rows, when applicable
```

`mercury.db` is Mercury's own file ([`hermes_engine::db`](../engine/src/db.rs)). It is not a Hermes
profile's `state.db`, which Mercury only reads to import. Each turn and each gateway receipt is a
row, so a reply writes its own rows rather than the whole chat, and a chat's transcript and gateway
state are saved in one transaction. The database uses WAL, full syncing, and overwrites what is
deleted. It is readable only by the user's account.

Earlier versions kept a `conversation.json` per chat, `library.json` and `cron/jobs.json`. A chat
moves when its owner next opens it, the catalog when the library opens, and routines at their next
change. Each is written, read back where it is a chat, and only then is its file renamed to
`*.before-sqlite`. Those files are kept: Hermes does not read them again, and a chat's goes when the
chat is deleted. Until a chat moves, readers use its file. A version from before the database does
not see chats that have moved.

Projects are existing canonical folders; imported upstream `cwd` is
provenance and never automatically grants execution access. A selected native
conversation supplies its own workspace, so desktop and gateway share history
without depending on process cwd. The catalog lock is never kept for a model
turn. The engine separately rejects two owners opening the same conversation.

`set_archived(id, bool)` soft-hides or restores a conversation without opening
its engine or changing its transcript. Manual archive survives subsequent opens;
restoration is explicit. `set_pinned(id, bool)` persists append-order pins without
changing conversation activity time. Archived pins stay out of the active list
and retain their pin when restored.

`sessions(project_id)` lists active conversations. `list_sessions(&SessionQuery)`
combines a workspace, `ArchiveFilter::{Active, Archived, All}`, an optional pin
filter, and case-insensitive title/native-ID/imported-Hermes-ID search. Pinned
sessions appear first in durable pin order, followed by recent conversations.
`session(id)` remains available for direct lookup of an archived conversation.
Version-one catalogs written before these fields existed upgrade additively.

An original one-conversation Rust `conversation.json` at the library root is
copied into the catalog once. Its source file is retained unchanged.

## Manual Hermes import

The compatibility reader follows upstream `persistence/sqlite/_schema_ddl.py`
and `persistence/sqlite/conversation/_store/messages.py`. It validates real
`sessions` and `messages` tables, reads optional newer columns when present,
decodes `\0json:` content and JSON tool calls, and combines compression ancestry
and its preferred continuation without merging explicitly marked branches,
delegates or resets. It deduplicates copied display/message identities.

SQLite never opens the original database. Read-only file handles copy the main
database and its optional WAL into a private temporary directory. Source size,
mtime and inode are checked across the copy; changing sources are retried three
times. Active rollback journals are rejected. SQLite opens only the disposable
snapshot in read-only/query-only mode with trusted schema disabled. The source
database, WAL, and shared-memory file are not written or migrated.

The native copy supports text conversation and recorded tool transcripts.
Every selected lineage row, including inactive messages, multimodal JSON,
reasoning, and unknown columns, is retained in `hermes-source.json`. BLOBs use a
typed hexadecimal wrapper. Import does not install upstream system prompts or
provider settings, resolve attachments, or execute historical tools. Historical
tool entries use `approved=false` because native approval cannot be inferred
from an upstream transcript. The report explains text-only conversion and other
limitations; callers should display it.

Limits are 4 GiB of source database/WAL, 100,000 listed sessions / 64 MiB of
picker metadata, and 20,000
messages / 64 MiB of raw transcript JSON per imported conversation. Missing or
unsupported sources fail without creating or modifying them. Reimporting an
already copied conversation returns the existing native copy; later upstream
changes do not overwrite native conversation progress.

## Intentional follow-up scope

- Media attachments are preserved as upstream row references, not copied or
  rendered as native image/audio messages.
- Imported sessions are snapshots; there is no bidirectional SQLite sync or
  automatic import of a profile.
- Legacy unmarked compression forks and advanced upstream display/checkpoint
  reconstruction can differ from upstream's full presentation layer. The raw
  archive preserves the selected lineage for inspection.
- Catalog mutations do not include chat deletion, project
  removal, or project moves. These can be added without rewriting the engine.

Run `./work/cargo-local test -p hermes-library --offline` from the enclosing
task workspace. Tests use synthetic databases only, including a live WAL fixture
and comparisons proving source bytes and mtimes remain unchanged.
