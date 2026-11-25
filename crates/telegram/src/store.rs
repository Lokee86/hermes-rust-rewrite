//! Private, atomic native Telegram settings and per-bot inbox persistence.
use super::telegram::{Identity, Incoming, Receipt, Token};
use fs2::FileExt;
use hermes_engine::secret;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

const MAX_INBOX_BYTES: u64 = 32 * 1024 * 1024;
const MAX_MESSAGES: usize = 10_000;

#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    pub version: u32,
    pub token: Option<Token>,
    pub allowed: BTreeSet<i64>,
    pub bot: Option<Identity>,
    #[serde(default)]
    pub enabled: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub update_id: Option<i64>,
    pub message_id: i64,
    pub user_id: i64,
    pub sender: String,
    pub text: String,
    pub timestamp: i64,
    pub outgoing: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Inbox {
    pub version: u32,
    pub bot: Identity,
    pub cursor: i64,
    #[serde(default)]
    pub revision: u64,
    pub messages: Vec<ChatMessage>,
}
impl Inbox {
    fn empty(bot: Identity) -> Self {
        Self {
            version: 1,
            bot,
            cursor: 0,
            revision: 0,
            messages: vec![],
        }
    }
}

#[derive(Clone)]
pub struct Store {
    root: PathBuf,
}
impl Store {
    pub fn new(library_root: &Path) -> Self {
        Self {
            root: library_root.join("messaging").join("telegram"),
        }
    }
    fn ensure_directory(&self) -> Result<(), String> {
        for path in [self.root.parent().unwrap(), self.root.as_path()] {
            if let Ok(meta) = fs::symlink_metadata(path)
                && (meta.file_type().is_symlink() || !meta.is_dir())
            {
                return Err("Native Telegram storage must be a regular directory".into());
            }
            fs::create_dir_all(path).map_err(|error| error.to_string())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                    .map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }
    fn lock(&self, name: &str) -> Result<File, String> {
        self.ensure_directory()?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join(name))
            .map_err(|error| error.to_string())?;
        file.lock_exclusive().map_err(|error| error.to_string())?;
        Ok(file)
    }
    pub fn load_settings(&self) -> Result<Option<Settings>, String> {
        let path = self.root.join("settings.json");
        if !path.exists() {
            return Ok(None);
        }
        let _lock = self.lock("settings.lock")?;
        let settings = self.read_settings()?;
        if settings.version != 1 {
            return Err("Unsupported native Telegram settings version".into());
        }
        Ok(Some(settings))
    }
    pub fn save_settings(&self, settings: &Settings) -> Result<(), String> {
        let _lock = self.lock("settings.lock")?;
        self.write_settings(settings)
    }
    fn read_saved(&self) -> Result<Value, String> {
        let saved: Value = private_json(&self.root.join("settings.json"), 1024 * 1024)?;
        if !saved.is_object() {
            return Err("Could not read native Telegram storage".into());
        }
        Ok(saved)
    }
    /// The token is encrypted on disk where the system has a credential store.
    fn read_settings(&self) -> Result<Settings, String> {
        let mut saved = self.read_saved()?;
        let token = saved["token"].as_str().map(str::to_owned);
        if let Some(token) = &token {
            saved["token"] = secret::unseal(token).map_err(|error| format!("{error:#}"))?.into();
        }
        let settings: Settings = serde_json::from_value(saved)
            .map_err(|_| "Could not read native Telegram storage".to_owned())?;
        // Protect a token saved by a version that stored it as text.
        if token.is_some_and(|token| !secret::is_sealed(&token)) {
            let _ = self.write_settings(&settings);
        }
        Ok(settings)
    }
    fn write_settings(&self, settings: &Settings) -> Result<(), String> {
        let mut saved = serde_json::to_value(settings).map_err(|error| error.to_string())?;
        if let Some(token) = saved["token"].as_str() {
            saved["token"] = secret::seal(&self.root, token).map_err(|error| format!("{error:#}"))?.into();
        }
        atomic_json(&self.root.join("settings.json"), &saved)
    }
    pub fn clear_token(&self) -> Result<Option<Settings>, String> {
        let _lock = self.lock("settings.lock")?;
        let path = self.root.join("settings.json");
        if !path.exists() {
            return Ok(None);
        }
        // Removing a token must work even when it cannot be opened.
        let mut saved = self.read_saved()?;
        saved["token"] = Value::Null;
        saved["bot"] = Value::Null;
        saved["enabled"] = Value::Bool(false);
        atomic_json(&path, &saved)?;
        serde_json::from_value(saved).map(Some).map_err(|_| "Could not read native Telegram storage".into())
    }
    pub fn set_disabled(&self) -> Result<(), String> {
        let _lock = self.lock("settings.lock")?;
        let path = self.root.join("settings.json");
        if !path.exists() {
            return Ok(());
        }
        let mut saved = self.read_saved()?;
        saved["enabled"] = Value::Bool(false);
        atomic_json(&path, &saved)
    }
    pub fn remember_bot(&self, identity: &Identity, token: &Token) -> Result<(), String> {
        let _lock = self.lock("settings.lock")?;
        let mut settings = self.read_settings()?;
        if settings.token.as_ref() != Some(token) {
            return Err(
                "Telegram setup changed. Reopen Messaging and enable the saved setup.".into(),
            );
        }
        if settings
            .bot
            .as_ref()
            .is_some_and(|saved| saved.id != identity.id)
        {
            return Err(
                "Telegram bot identity changed; review the saved setup before reconnecting".into(),
            );
        }
        settings.bot = Some(identity.clone());
        settings.enabled = true;
        self.write_settings(&settings)
    }
    fn connection_settings(&self, bot: &Identity, token: &Token) -> Result<Settings, String> {
        let settings = self.read_settings()?;
        if settings.version != 1
            || settings.token.as_ref() != Some(token)
            || !settings.enabled
            || settings.bot.as_ref().map(|identity| identity.id) != Some(bot.id)
        {
            return Err(
                "Telegram setup changed. Disable and reopen Messaging before reconnecting.".into(),
            );
        }
        Ok(settings)
    }
    pub fn authorize_reply(
        &self,
        bot: &Identity,
        token: &Token,
        user_id: i64,
    ) -> Result<(), String> {
        let _lock = self.lock("settings.lock")?;
        let settings = self.connection_settings(bot, token)?;
        if !settings.allowed.contains(&user_id) {
            return Err("This Telegram user is no longer on the saved allowlist".into());
        }
        Ok(())
    }
    pub fn claim(&self, bot_id: i64) -> Result<Arc<File>, String> {
        if bot_id <= 0 {
            return Err("Telegram bot identity is invalid".into());
        }
        self.ensure_directory()?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join(format!("bot-{bot_id}.connection.lock")))
            .map_err(|error| error.to_string())?;
        file.try_lock_exclusive().map_err(|_| {
            "This Telegram bot is already connected in another Hermes window".to_owned()
        })?;
        Ok(Arc::new(file))
    }
    fn inbox_path(&self, bot_id: i64) -> Result<PathBuf, String> {
        if bot_id <= 0 {
            return Err("Telegram bot identity is invalid".into());
        }
        Ok(self.root.join(format!("bot-{bot_id}.inbox.json")))
    }
    fn load_inbox_inner(&self, bot: &Identity) -> Result<Inbox, String> {
        let path = self.inbox_path(bot.id)?;
        if !path.exists() {
            return Ok(Inbox::empty(bot.clone()));
        }
        let inbox: Inbox = private_json(&path, MAX_INBOX_BYTES)?;
        if inbox.version != 1 || inbox.bot.id != bot.id {
            return Err("Native Telegram inbox identity or version is invalid".into());
        }
        Ok(inbox)
    }
    pub fn load_inbox(&self, bot: &Identity) -> Result<Arc<Inbox>, String> {
        let _lock = self.lock(&format!("bot-{}.inbox.lock", bot.id))?;
        Ok(Arc::new(self.load_inbox_inner(bot)?))
    }
    pub fn commit_poll(
        &self,
        bot: &Identity,
        token: &Token,
        messages: &[Incoming],
        cursor: i64,
    ) -> Result<(Arc<Inbox>, Vec<Incoming>), String> {
        // Hold settings through the commit so another window cannot save a
        // stricter allowlist while an older batch is being persisted.
        let _settings_lock = self.lock("settings.lock")?;
        let settings = self.connection_settings(bot, token)?;
        let _lock = self.lock(&format!("bot-{}.inbox.lock", bot.id))?;
        let mut inbox = self.load_inbox_inner(bot)?;
        let mut seen: HashSet<_> = inbox
            .messages
            .iter()
            .filter_map(|message| message.update_id)
            .collect();
        let mut received = vec![];
        for message in messages {
            if message.bot_id != bot.id
                || !settings.allowed.contains(&message.user_id)
                || message.chat_id != message.user_id
                || message.update_id < inbox.cursor
                || !seen.insert(message.update_id)
            {
                continue;
            }
            inbox.messages.push(ChatMessage {
                update_id: Some(message.update_id),
                message_id: message.message_id,
                user_id: message.user_id,
                sender: message.sender.clone(),
                text: message.text.clone(),
                timestamp: message.timestamp,
                outgoing: false,
            });
            received.push(message.clone());
        }
        if inbox.messages.len() > MAX_MESSAGES {
            return Err(
                "Telegram inbox reached its native storage limit; existing messages were retained"
                    .into(),
            );
        }
        let next_cursor = inbox.cursor.max(cursor);
        if !received.is_empty() || next_cursor != inbox.cursor {
            inbox.cursor = next_cursor;
            inbox.revision = inbox.revision.saturating_add(1);
            self.save_inbox(&inbox)?;
        }
        Ok((Arc::new(inbox), received))
    }
    pub fn record_sent(
        &self,
        bot: &Identity,
        user_id: i64,
        text: String,
        receipt: Receipt,
    ) -> Result<Arc<Inbox>, String> {
        let _lock = self.lock(&format!("bot-{}.inbox.lock", bot.id))?;
        let mut inbox = self.load_inbox_inner(bot)?;
        if !inbox.messages.iter().any(|message| {
            message.outgoing
                && message.message_id == receipt.message_id
                && message.user_id == user_id
        }) {
            inbox.messages.push(ChatMessage {
                update_id: None,
                message_id: receipt.message_id,
                user_id,
                sender: "You".into(),
                text,
                timestamp: receipt.timestamp,
                outgoing: true,
            });
        }
        if inbox.messages.len() > MAX_MESSAGES {
            return Err("Reply delivered, but the native inbox storage limit was reached".into());
        }
        inbox.revision = inbox.revision.saturating_add(1);
        self.save_inbox(&inbox)?;
        Ok(Arc::new(inbox))
    }
    fn save_inbox(&self, inbox: &Inbox) -> Result<(), String> {
        let bytes = serde_json::to_vec(inbox).map_err(|error| error.to_string())?;
        if bytes.len() as u64 > MAX_INBOX_BYTES {
            return Err("Telegram inbox exceeds the native safe-load limit; existing messages were retained".into());
        }
        atomic_bytes(&self.inbox_path(inbox.bot.id)?, &bytes)
    }
}

fn private_json<T: serde::de::DeserializeOwned>(path: &Path, max: u64) -> Result<T, String> {
    let meta = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > max {
        return Err("Native Telegram storage is not a regular, bounded file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(
                "Native Telegram credentials and inbox files must be private to your user".into(),
            );
        }
    }
    let file = File::open(path).map_err(|error| error.to_string())?;
    serde_json::from_reader(file.take(max + 1))
        .map_err(|_| "Could not read native Telegram storage".to_owned())
}
fn atomic_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    atomic_bytes(
        path,
        &serde_json::to_vec(value).map_err(|error| error.to_string())?,
    )
}
fn atomic_bytes(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or("Native Telegram storage has no parent")?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
    temp.write_all(bytes)
        .and_then(|_| temp.write_all(b"\n"))
        .and_then(|_| temp.as_file().sync_all())
        .map_err(|error| error.to_string())?;
    temp.persist(path)
        .map_err(|_| "Could not replace native Telegram storage".to_owned())?;
    #[cfg(unix)]
    File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(|error| error.to_string())?;
    Ok(())
}
