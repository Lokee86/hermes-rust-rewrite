//! Follow-up messages queued in the desktop. A chat's gateway owns its queue; the gateway also
//! takes over a `queued-prompts.json` an earlier version left.
use hermes_engine::Attachment;
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct QueuedPrompt { pub id: u64, pub text: String, #[serde(default)] pub attachments: Vec<Attachment> }

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct InFlight { pub entry: QueuedPrompt, pub turn_index: usize }

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct PromptQueue {
    pub entries: Vec<QueuedPrompt>,
    pub parked: bool,
    pub in_flight: Option<InFlight>,
    next_id: u64,
}

impl PromptQueue {
    pub fn save(&self, directory: &Path) -> Result<(), String> {
        let path = directory.join("queued-prompts.json");
        if self.entries.is_empty() && self.in_flight.is_none() {
            return match fs::remove_file(path) { Ok(()) => Ok(()), Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()), Err(e) => Err(e.to_string()) };
        }
        let temp = directory.join("queued-prompts.tmp");
        fs::write(&temp, serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        fs::rename(temp, path).map_err(|e| e.to_string())
    }
    pub fn push(&mut self, text: String, attachments: Vec<Attachment>, front: bool) -> Result<u64, String> {
        Attachment::validate_set(&attachments).map_err(|e|e.to_string())?;
        if text.trim().is_empty() && attachments.is_empty() { return Err("Enter a message to queue.".into()); }
        if self.entries.len() >= 50 { return Err("The queue is full. Send or remove a queued message first.".into()); }
        if text.len() > 128 * 1024 { return Err("A queued message can contain at most 128 KiB of text.".into()); }
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let entry = QueuedPrompt { id: self.next_id, text, attachments };
        let id = entry.id;
        if front { self.entries.insert(0, entry); } else { self.entries.push(entry); }
        Ok(id)
    }
}
