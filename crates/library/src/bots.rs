//! Bot Mode (plugins/hermes-bots): one bot = one profile with a persistent
//! identity and ONE canonical forever-chat, identified by NAME. The chat's
//! identity is (bot, session titled exactly "Bot Chat"); there is no stored
//! session-id pointer and recency never picks it. Bot chats are hidden from
//! the Sessions list, so the bot row is the only door.
use super::*;
use std::collections::BTreeMap;

pub const BOT_CHAT_TITLE: &str = "Bot Chat";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Bot {
    /// Profile name: lowercase letters, digits, `-` and `_`.
    pub name: String,
    pub display_name: String,
    #[serde(default)]
    pub emoji: String,
    #[serde(default)]
    pub description: String,
    pub created_at: String,
}

pub struct BotDeletion {
    pub retained_profile: Option<PathBuf>,
}

#[derive(Default, Serialize, Deserialize)]
struct Roster {
    #[serde(default)] bots: Vec<Bot>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    profile_owners: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    profile_deletions: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize)]
struct ProfileOwner {
    name: String,
    id: String,
}

const PROFILE_OWNER: &str = ".hermes-bot-owner.json";

fn retry_profile_operation<T>(mut operation: impl FnMut() -> Result<T>) -> Result<T> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(if cfg!(windows) { 20 } else { 0 });
    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(_) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(250)),
            Err(error) => return Err(error),
        }
    }
}

fn remove_owned_profile(profile: &Path, name: &str, expected: &str) -> Result<()> {
    retry_profile_operation(|| remove_owned_profile_once(profile, name, expected))
}

fn remove_owned_profile_once(profile: &Path, name: &str, expected: &str) -> Result<()> {
    let metadata = match fs::symlink_metadata(profile) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    ensure!(metadata.file_type().is_dir(), "Bot deletion directory must be a regular directory");
    let marker = profile.join(PROFILE_OWNER);
    match fs::symlink_metadata(&marker) {
        Ok(metadata) => {
            ensure!(metadata.file_type().is_file(), "Bot ownership marker must be a regular file");
            let owner: ProfileOwner = serde_json::from_reader(File::open(&marker)?)?;
            ensure!(owner.name == name && owner.id == expected, "Bot profile ownership changed; its files were retained");
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // A previous attempt may have removed the marker immediately before
            // the final directory removal. Only an empty directory is safe here.
            fs::remove_dir(profile).context("Bot profile ownership is unavailable; its files were retained")?;
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    }
    for entry in fs::read_dir(profile)? {
        let entry = entry?;
        if entry.file_name() == PROFILE_OWNER { continue; }
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    fs::remove_file(marker)?;
    fs::remove_dir(profile)?;
    Ok(())
}

/// "Research Helper" → "research-helper".
fn profile_name(display: &str) -> String {
    let mut out = String::new();
    for c in display.trim().chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() { out.push(c); } else if !out.ends_with('-') && !out.is_empty() { out.push('-'); }
    }
    out.trim_end_matches('-').chars().take(48).collect()
}

/// The SOUL a bot is born with (soul.ts), written to its profile's SOUL.md.
fn soul(bot: &Bot, instructions: &str) -> String {
    let mut text = format!("# {}\n\nYou are {}, a named Hermes agent.", bot.display_name, bot.display_name);
    if !bot.description.trim().is_empty() { text.push_str(&format!(" {}", bot.description.trim())); }
    if !instructions.trim().is_empty() { text.push_str(&format!("\n\n## How you work\n\n{}", instructions.trim())); }
    text.push('\n');
    text
}

impl Library {
    fn roster_path(&self) -> PathBuf { self.root.join("bots.json") }
    fn lock_roster(&self) -> Result<File> {
        let path = self.root.join(".bots.lock");
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            ensure!(metadata.file_type().is_file(), "Bot roster lock must be a regular file");
        }
        let lock = OpenOptions::new().create(true).truncate(false).read(true).write(true).open(path)?;
        lock.lock_exclusive()?;
        Ok(lock)
    }
    fn read_roster(&self) -> Result<Roster> {
        match fs::read(self.roster_path()) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("Could not read the bot roster"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Roster::default()),
            Err(e) => Err(e.into()),
        }
    }
    fn checked_bot_profile(&self, roster: &Roster, name: &str) -> Result<Option<(PathBuf, bool)>> {
        ensure!(roster.bots.iter().any(|bot| bot.name == name), "This bot no longer exists");
        let profile = self.bot_profile(name)?;
        self.ensure_profiles_dir()?;
        let metadata = match fs::symlink_metadata(&profile) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        ensure!(metadata.file_type().is_dir(), "Bot profile must be a regular directory");
        ensure!(fs::canonicalize(&profile)? == profile, "Bot profile path does not match its library-owned location");
        let marker = profile.join(PROFILE_OWNER);
        let owned = match (roster.profile_owners.get(name), fs::symlink_metadata(&marker)) {
            (Some(expected), Ok(metadata)) if metadata.file_type().is_file() => {
                let owner: ProfileOwner = serde_json::from_reader(File::open(marker)?)?;
                ensure!(owner.name == name && &owner.id == expected, "Bot profile ownership changed; its files were retained");
                true
            }
            // An old roster cannot prove that its directory was created for the bot.
            (None, Err(error)) if error.kind() == std::io::ErrorKind::NotFound => false,
            _ => bail!("Bot profile ownership is unavailable; its files were retained"),
        };
        Ok(Some((profile, owned)))
    }
    /// The bot's own profile folder: SOUL.md, settings, memory and skills.
    pub fn bot_profile(&self, name: &str) -> Result<PathBuf> {
        ensure!(profiles::valid_name(name), "Invalid bot name");
        Ok(self.root.join("profiles").join(name))
    }
    /// Verify a bot profile is an exact canonical direct child of this library.
    /// Destructive cleanup separately requires its saved ownership marker.
    /// `None` is allowed after a prior directory-removal attempt, while the
    /// roster still provides the bot's ownership record for retrying cleanup.
    pub fn canonical_bot_profile(&self, name: &str) -> Result<Option<PathBuf>> {
        let expected = self.bot_profile(name)?;
        let metadata = match fs::symlink_metadata(&expected) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        ensure!(metadata.file_type().is_dir(), "Bot profile path is not a directory");
        let canonical = fs::canonicalize(&expected).context("Could not verify bot profile ownership")?;
        ensure!(canonical == expected, "Bot profile path does not match its library-owned location");
        Ok(Some(canonical))
    }
    pub fn bots(&self) -> Result<Vec<Bot>> { Ok(self.read_roster()?.bots) }

    pub fn create_bot(&self, display_name: &str, description: &str, instructions: &str, emoji: &str) -> Result<Bot> {
        let display_name = display_name.trim();
        ensure!(!display_name.is_empty(), "Give the bot a name");
        ensure!(display_name.chars().count() <= 60, "A bot name must contain at most 60 characters");
        let name = profile_name(display_name);
        ensure!(!name.is_empty() && safe_id(&name), "Use letters or digits in the bot's name");
        ensure!(name != "default", "\"default\" is reserved for your main profile");
        let _profiles_lock = self.lock_profiles()?;
        let _roster_lock = self.lock_roster()?;
        let mut roster = self.read_roster()?;
        ensure!(!roster.bots.iter().any(|bot| bot.name == name), "A bot named {display_name} already exists");
        let bot = Bot { name: name.clone(), display_name: display_name.into(), emoji: emoji.trim().chars().take(4).collect(), description: description.trim().into(), created_at: now() };
        let profile = self.bot_profile(&name)?;
        self.ensure_profiles_dir()?;
        fs::create_dir(&profile).context("A profile with that name already exists")?;
        let owner = new_id("bot-owner");
        let initialized = (|| -> Result<()> {
            atomic_json(&profile.join(PROFILE_OWNER), &ProfileOwner { name: name.clone(), id: owner.clone() })?;
            fs::write(profile.join("SOUL.md"), soul(&bot, instructions))?;
            // A bot starts from the main profile's settings, then owns its own copy.
            let main_settings = self.root.join("agent-settings.json");
            if main_settings.is_file() { fs::copy(main_settings, profile.join("agent-settings.json"))?; }
            Ok(())
        })();
        if let Err(error) = initialized {
            // Remove only files initialized here; unrelated files are retained.
            for file in [PROFILE_OWNER, "SOUL.md", "agent-settings.json"] { let _ = fs::remove_file(profile.join(file)); }
            let _ = fs::remove_dir(&profile);
            return Err(error);
        }
        roster.bots.push(bot.clone());
        roster.profile_owners.insert(name.clone(), owner);
        atomic_json(&self.roster_path(), &roster)?;
        Ok(bot)
    }

    pub fn update_bot(&self, name: &str, display_name: &str, description: &str, emoji: &str) -> Result<Bot> {
        let _lock = self.lock_roster()?;
        let mut roster = self.read_roster()?;
        ensure!(!roster.profile_deletions.contains_key(name), "Retry deleting this bot before changing it");
        let bot = roster.bots.iter_mut().find(|bot| bot.name == name).context("This bot no longer exists")?;
        ensure!(!display_name.trim().is_empty(), "Give the bot a name");
        bot.display_name = display_name.trim().into(); bot.description = description.trim().into(); bot.emoji = emoji.trim().chars().take(4).collect();
        let updated = bot.clone();
        atomic_json(&self.roster_path(), &roster)?;
        Ok(updated)
    }

    pub fn bot_soul(&self, name: &str) -> Result<String> {
        Ok(fs::read_to_string(self.bot_profile(name)?.join("SOUL.md")).unwrap_or_default())
    }
    pub fn set_bot_soul(&self, name: &str, text: &str) -> Result<()> {
        let _profiles_lock = self.lock_profiles()?;
        let _roster_lock = self.lock_roster()?;
        let (profile, _) = self.checked_bot_profile(&self.read_roster()?, name)?
            .context("The bot profile is unavailable")?;
        let mut file = NamedTempFile::new_in(&profile)?; file.write_all(text.as_bytes())?; file.persist(profile.join("SOUL.md"))?;
        Ok(())
    }

    /// Removes the bot and its owned profile. Legacy directories are retained.
    pub fn delete_bot(&self, name: &str) -> Result<()> {
        self.delete_bot_with_outcome(name).map(|_| ())
    }

    /// Reports an unverified legacy profile retained after removing the roster entry.
    pub fn delete_bot_with_outcome(&self, name: &str) -> Result<BotDeletion> {
        self.delete_bot_with_cleanup(name, |_| Ok(()))
    }

    /// Run host cleanup only for a verified owned profile, before removing it.
    /// The caller must stop profile services and hold their lifecycle fences.
    pub fn delete_bot_with_cleanup(
        &self,
        name: &str,
        cleanup: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<BotDeletion> {
        let _profiles_lock = self.lock_profiles()?;
        let _roster_lock = self.lock_roster()?;
        let mut roster = self.read_roster()?;
        ensure!(roster.bots.iter().any(|bot| bot.name == name), "This bot no longer exists");
        let profile = if let Some(owner) = roster.profile_deletions.get(name) {
            ensure!(roster.profile_owners.get(name) == Some(owner), "Invalid bot profile deletion record");
            Some((self.bot_profile(name)?, true))
        } else {
            self.checked_bot_profile(&roster, name)?
        };
        let retained_profile = match profile {
            Some((profile, true)) => {
                let owner = roster.profile_owners.get(name).context("Missing bot profile owner")?.clone();
                ensure!(safe_id(&owner), "Invalid bot profile owner");
                let staging = self.ensure_profiles_dir()?.join(format!(".deleting-bot-{name}-{owner}"));
                if !roster.profile_deletions.contains_key(name) {
                    cleanup(&profile)?;
                    roster.profile_deletions.insert(name.to_owned(), owner.clone());
                    atomic_json(&self.roster_path(), &roster)?;
                }
                if !staging.try_exists()? && fs::symlink_metadata(&profile).is_ok() {
                    retry_profile_operation(|| {
                        self.checked_bot_profile(&roster, name)?.context("The bot profile is unavailable")?;
                        fs::rename(&profile, &staging).context("Could not prepare bot profile removal; retry deleting the bot")
                    })?;
                }
                remove_owned_profile(&staging, name, &owner)
                    .context("Could not remove bot profile; retry deleting the bot")?;
                #[cfg(unix)]
                File::open(self.ensure_profiles_dir()?)?.sync_all()?;
                None
            }
            Some((profile, false)) => Some(profile),
            None => None,
        };
        self.with_catalog(true, |catalog| {
            for record in catalog.sessions.iter_mut().filter(|r| r.bot.as_deref() == Some(name)) { record.archived = true; record.bot = None; record.title = format!("{} (deleted bot)", record.title); }
            Ok(())
        })?;
        let before = roster.bots.len();
        roster.bots.retain(|bot| bot.name != name);
        roster.profile_owners.remove(name);
        roster.profile_deletions.remove(name);
        ensure!(roster.bots.len() < before, "This bot no longer exists");
        atomic_json(&self.roster_path(), &roster)?;
        Ok(BotDeletion { retained_profile })
    }

    /// Resolve the registry every time: the session titled "Bot Chat" owned by
    /// this bot. Created (adopt-before-mint, under the catalog lock) when missing.
    pub fn bot_chat(&self, name: &str) -> Result<SessionSummary> {
        let _lock = self.lock_roster()?;
        let roster = self.read_roster()?;
        ensure!(roster.bots.iter().any(|bot| bot.name == name), "This bot no longer exists");
        ensure!(!roster.profile_deletions.contains_key(name), "Retry deleting this bot before opening it");
        self.with_catalog(true, |catalog| {
            if let Some(record) = catalog.sessions.iter_mut().find(|r| r.bot.as_deref() == Some(name) && r.title == BOT_CHAT_TITLE) {
                record.archived = false;
                let summary = self.summary(record);
                catalog.active_session = Some(summary.id.clone());
                return Ok(summary);
            }
            let mut record = self.new_record(None, Some(BOT_CHAT_TITLE), SessionSource::Native);
            record.bot = Some(name.to_owned());
            let directory = self.create_directory(&record.id)?;
            if let Err(error) = hermes_engine::create_conversation(&directory, &History::default()) {
                let _ = fs::remove_dir(&directory);
                return Err(error);
            }
            catalog.active_session = Some(record.id.clone());
            let summary = self.summary(&record);
            catalog.sessions.push(record);
            Ok(summary)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bot_creation_does_not_adopt_an_existing_profile() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        let profile = library.create_profile("work").unwrap();
        fs::write(profile.path.join("SOUL.md"), "Existing profile instructions").unwrap();
        fs::write(profile.path.join("agent-settings.json"), "{\"max_turns\":7}").unwrap();
        fs::write(library.root().join("agent-settings.json"), "{\"max_turns\":9}").unwrap();

        assert!(library.create_bot("Work", "", "New bot instructions", "").is_err());
        assert!(library.bots().unwrap().is_empty());
        assert_eq!(fs::read_to_string(profile.path.join("SOUL.md")).unwrap(), "Existing profile instructions");
        assert_eq!(fs::read_to_string(profile.path.join("agent-settings.json")).unwrap(), "{\"max_turns\":7}");
    }

    #[test]
    fn concurrent_bot_and_profile_creation_reserve_one_directory() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let profile_library = library.clone();
        let profile_barrier = barrier.clone();
        let profile = std::thread::spawn(move || {
            profile_barrier.wait();
            profile_library.create_profile("shared").is_ok()
        });
        let bot = std::thread::spawn(move || {
            barrier.wait();
            library.create_bot("Shared", "", "", "").is_ok()
        });
        assert_ne!(profile.join().unwrap(), bot.join().unwrap());
    }

    #[test]
    fn deleting_a_legacy_bot_retains_the_preexisting_profile_and_archives_its_chat() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        let profile = library.create_profile("work").unwrap();
        fs::write(profile.path.join("important.txt"), "Existing profile data").unwrap();
        fs::write(profile.path.join("SOUL.md"), "Legacy instructions").unwrap();
        let bot = Bot { name: "work".into(), display_name: "Work".into(), emoji: String::new(), description: String::new(), created_at: now() };
        // This is the old format, including the state left by a past collision.
        atomic_json(&library.roster_path(), &serde_json::json!({"bots":[bot]})).unwrap();
        let chat = library.bot_chat("work").unwrap();
        library.set_bot_soul("work", "Updated legacy instructions").unwrap();

        let outcome = library.delete_bot_with_cleanup("work", |_| bail!("Legacy profile cleanup must not run")).unwrap();
        assert_eq!(outcome.retained_profile.as_deref(), Some(profile.path.as_path()));
        assert!(library.bots().unwrap().is_empty());
        assert_eq!(fs::read_to_string(profile.path.join("important.txt")).unwrap(), "Existing profile data");
        assert_eq!(fs::read_to_string(profile.path.join("SOUL.md")).unwrap(), "Updated legacy instructions");
        let retained = library.profiles().unwrap().into_iter().find(|profile| profile.name == "work").unwrap();
        assert!(!retained.is_bot);
        let archived = library.session(&chat.id).unwrap();
        assert!(archived.archived);
        assert!(archived.bot.is_none());
    }

    #[test]
    fn deleting_a_replaced_owned_profile_retains_its_files_and_roster() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        let bot = library.create_bot("Helper", "", "", "").unwrap();
        let profile = library.bot_profile(&bot.name).unwrap();
        fs::rename(&profile, dir.path().join("original-profile")).unwrap();
        fs::create_dir(&profile).unwrap();
        fs::write(profile.join("important.txt"), "Replacement profile data").unwrap();

        assert!(library.delete_bot(&bot.name).is_err());
        assert_eq!(fs::read_to_string(profile.join("important.txt")).unwrap(), "Replacement profile data");
        assert_eq!(library.bots().unwrap().len(), 1);
    }

    #[test]
    fn an_interrupted_owned_profile_deletion_remains_retryable() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        let bot = library.create_bot("Helper", "", "", "").unwrap();
        let chat = library.bot_chat(&bot.name).unwrap();
        let profile = library.bot_profile(&bot.name).unwrap();
        let mut roster = library.read_roster().unwrap();
        let owner = roster.profile_owners[&bot.name].clone();
        roster.profile_deletions.insert(bot.name.clone(), owner.clone());
        atomic_json(&library.roster_path(), &roster).unwrap();
        let staging = profile.parent().unwrap().join(format!(".deleting-bot-{}-{owner}", bot.name));
        fs::rename(profile, &staging).unwrap();
        // A missing marker with remaining data cannot authorize deletion.
        fs::remove_file(staging.join(PROFILE_OWNER)).unwrap();
        assert!(library.delete_bot(&bot.name).is_err());
        assert_eq!(library.bots().unwrap().len(), 1);
        assert!(library.session(&chat.id).unwrap().bot.is_some());
        assert!(library.create_profile(&bot.name).is_err());
        atomic_json(&staging.join(PROFILE_OWNER), &ProfileOwner { name: bot.name.clone(), id: owner }).unwrap();

        let reopened = Library::open(library.root()).unwrap();
        assert!(reopened.delete_bot_with_outcome(&bot.name).unwrap().retained_profile.is_none());
        assert!(reopened.bots().unwrap().is_empty());
        assert!(!staging.exists());
        assert!(reopened.session(&chat.id).unwrap().archived);
    }

    #[cfg(windows)]
    #[test]
    fn a_locked_profile_keeps_a_retryable_bot_deletion() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        let bot = library.create_bot("Helper", "", "", "").unwrap();
        let profile = library.bot_profile(&bot.name).unwrap();
        let held = fs::OpenOptions::new().create_new(true).write(true).share_mode(0).open(profile.join("held.txt")).unwrap();
        assert!(library.delete_bot(&bot.name).is_err());
        assert_eq!(library.bots().unwrap().len(), 1);
        drop(held);
        assert!(library.delete_bot_with_outcome(&bot.name).unwrap().retained_profile.is_none());
        assert!(library.bots().unwrap().is_empty());
    }

    #[test]
    fn bot_owns_one_hidden_canonical_chat() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        let bot = library.create_bot("Research Helper", "Finds papers.", "Cite sources.", "🔬").unwrap();
        assert_eq!(bot.name, "research-helper");
        assert!(library.bot_soul(&bot.name).unwrap().contains("Cite sources."));
        assert!(library.create_bot("research helper", "", "", "").is_err(), "names are unique");
        let chat = library.bot_chat(&bot.name).unwrap();
        assert_eq!(chat.title, BOT_CHAT_TITLE);
        // Same row every time, even after the user renames other chats or time passes.
        assert_eq!(library.bot_chat(&bot.name).unwrap().id, chat.id);
        // Hidden from the Sessions list, and never reused as a "New chat" draft.
        assert!(library.list_sessions(&SessionQuery::default()).unwrap().is_empty());
        assert_ne!(library.create_session(None, None).unwrap().id, chat.id);
        library.delete_bot(&bot.name).unwrap();
        assert!(library.bots().unwrap().is_empty());
        assert!(!library.bot_profile(&bot.name).unwrap().exists());
        assert!(library.bot_chat(&bot.name).is_err());
    }

    #[test]
    fn failed_profile_removal_keeps_roster_and_chat_retryable() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        let bot = library.create_bot("Retry Helper", "", "", "🤖").unwrap();
        let chat = library.bot_chat(&bot.name).unwrap();
        let profile = library.bot_profile(&bot.name).unwrap();
        let owner = fs::read(profile.join(PROFILE_OWNER)).unwrap();
        fs::remove_dir_all(&profile).unwrap();
        fs::write(&profile, b"not a profile directory").unwrap();

        assert!(library.delete_bot(&bot.name).is_err());
        assert!(library.bots().unwrap().iter().any(|entry| entry.name == bot.name));
        let retained = library.session(&chat.id).unwrap();
        assert!(!retained.archived);
        assert_eq!(retained.bot.as_deref(), Some(bot.name.as_str()));

        fs::remove_file(&profile).unwrap();
        fs::create_dir_all(&profile).unwrap();
        fs::write(profile.join(PROFILE_OWNER), owner).unwrap();
        library.delete_bot(&bot.name).unwrap();
        assert!(library.bots().unwrap().iter().all(|entry| entry.name != bot.name));
        assert!(!profile.exists());
        let archived = library.session(&chat.id).unwrap();
        assert!(archived.archived);
        assert_eq!(archived.bot, None);
        assert!(archived.title.contains("deleted bot"));
    }

    #[test]
    fn failed_host_cleanup_keeps_owned_bot_deletion_retryable() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        let bot = library.create_bot("Helper", "", "", "").unwrap();
        let chat = library.bot_chat(&bot.name).unwrap();
        let profile = library.bot_profile(&bot.name).unwrap();
        assert!(library.delete_bot_with_cleanup(&bot.name, |_| bail!("Cleanup is busy")).is_err());
        assert!(profile.is_dir());
        assert_eq!(library.bots().unwrap().len(), 1);
        assert!(!library.session(&chat.id).unwrap().archived);
        assert!(library.read_roster().unwrap().profile_deletions.is_empty());
        let mut cleaned = false;
        library.delete_bot_with_cleanup(&bot.name, |home| {
            assert_eq!(home, profile);
            cleaned = true;
            Ok(())
        }).unwrap();
        assert!(cleaned);
        assert!(!profile.exists());
        assert!(library.session(&chat.id).unwrap().archived);
    }
}

/// A group chat (plugins/hermes-bots group-chat*): a named room of bots that
/// answer the user in turn. Its chat is a hidden session owned by "group:<id>".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Group { pub id: String, pub name: String, pub members: Vec<String> }

#[derive(Default, Serialize, Deserialize)]
struct Groups { #[serde(default)] groups: Vec<Group> }

impl Library {
    fn groups_path(&self) -> PathBuf { self.root.join("groups.json") }
    pub fn groups(&self) -> Result<Vec<Group>> {
        match fs::read(self.groups_path()) {
            Ok(bytes) => Ok(serde_json::from_slice::<Groups>(&bytes).context("Could not read group chats")?.groups),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }
    pub fn create_group(&self, name: &str, members: &[String]) -> Result<Group> {
        let name = name.trim();
        ensure!(!name.is_empty(), "Name the group chat");
        let bots = self.bots()?;
        ensure!(members.len() >= 2, "A group chat needs at least two bots");
        ensure!(members.iter().all(|m| bots.iter().any(|b| &b.name == m)), "A selected bot no longer exists");
        let mut all = self.groups()?;
        let group = Group { id: new_id("group"), name: name.into(), members: members.to_vec() };
        all.push(group.clone());
        atomic_json(&self.groups_path(), &Groups { groups: all })?;
        Ok(group)
    }
    pub fn delete_group(&self, id: &str) -> Result<()> {
        let mut all = self.groups()?;
        all.retain(|g| g.id != id);
        atomic_json(&self.groups_path(), &Groups { groups: all })?;
        let owner = format!("group:{id}");
        self.with_catalog(true, |catalog| { for r in catalog.sessions.iter_mut().filter(|r| r.bot.as_deref() == Some(owner.as_str())) { r.archived = true; r.bot = None; } Ok(()) })
    }
    /// The group's one chat, resolved by owner every time (created when missing).
    pub fn group_chat(&self, id: &str) -> Result<SessionSummary> {
        let group = self.groups()?.into_iter().find(|g| g.id == id).context("This group chat no longer exists")?;
        let owner = format!("group:{id}");
        self.with_catalog(true, |catalog| {
            if let Some(record) = catalog.sessions.iter().find(|r| r.bot.as_deref() == Some(owner.as_str())) {
                let summary = self.summary(record); catalog.active_session = Some(summary.id.clone()); return Ok(summary);
            }
            let mut record = self.new_record(None, Some(&group.name), SessionSource::Native);
            record.bot = Some(owner.clone());
            let directory = self.create_directory(&record.id)?;
            if let Err(error) = hermes_engine::create_conversation(&directory, &History::default()) { let _ = fs::remove_dir(&directory); return Err(error); }
            catalog.active_session = Some(record.id.clone());
            let summary = self.summary(&record);
            catalog.sessions.push(record);
            Ok(summary)
        })
    }
}

#[cfg(test)]
mod group_tests {
    use super::*;
    #[test]
    fn group_needs_two_bots_and_owns_one_hidden_chat() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        let a = library.create_bot("Ada", "", "", "").unwrap();
        assert!(library.create_group("Solo", &[a.name.clone()]).is_err());
        let b = library.create_bot("Bo", "", "", "").unwrap();
        let group = library.create_group("Team", &[a.name.clone(), b.name.clone()]).unwrap();
        let chat = library.group_chat(&group.id).unwrap();
        assert_eq!(library.group_chat(&group.id).unwrap().id, chat.id);
        assert!(library.list_sessions(&SessionQuery::default()).unwrap().is_empty(), "group chats stay out of Sessions");
        library.delete_group(&group.id).unwrap();
        assert!(library.groups().unwrap().is_empty());
    }
}
