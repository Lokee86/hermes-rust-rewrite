//! Webhook subscriptions (app/webhooks, gateway webhook platform): a named
//! endpoint `POST /webhooks/<name>` on the Rust gateway. Each delivery is
//! verified with the subscription's HMAC-SHA256 secret, filtered by event, and
//! then runs the agent with the subscription's prompt (or is only logged).
use crate::{Library, atomic_json};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use fs2::FileExt;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Delivery {
    pub at: String, pub event: String, pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Webhook {
    pub name: String,
    #[serde(default)] pub description: String,
    #[serde(default)] pub prompt: String,
    #[serde(default)] pub events: Vec<String>,
    #[serde(default)] pub skills: Vec<String>,
    /// "log" (record and run the agent into this webhook's chat) — the only target the Rust gateway delivers to.
    #[serde(default = "log")] pub deliver: String,
    /// Record the payload without running the agent.
    #[serde(default)] pub deliver_only: bool,
    #[serde(default = "yes")] pub enabled: bool,
    pub secret: String,
    #[serde(default)] pub created_at: String,
    #[serde(default)] pub session_id: Option<String>,
    #[serde(default)] pub deliveries: Vec<Delivery>,
}
fn log() -> String { "log".into() }
fn yes() -> bool { true }

pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

impl Library {
    fn lock_webhooks(&self) -> Result<std::fs::File> {
        let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(self.root().join(".webhooks.lock"))?;
        lock.lock_exclusive()?;
        Ok(lock)
    }
    fn webhooks_path(&self) -> std::path::PathBuf { self.root().join("webhooks.json") }
    pub fn webhooks(&self) -> Result<Vec<Webhook>> {
        match std::fs::read(self.webhooks_path()) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).context("Invalid webhooks.json")?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }
    fn save_webhooks(&self, list: &[Webhook]) -> Result<()> { atomic_json(&self.webhooks_path(), &list) }
    /// Create a subscription; the returned secret is shown once.
    pub fn create_webhook(&self, mut hook: Webhook) -> Result<Webhook> {
        let _lock = self.lock_webhooks()?;
        hook.name = hook.name.trim().to_lowercase().replace(' ', "-");
        ensure!(valid_name(&hook.name), "Use lowercase letters, numbers, - or _ for the name (up to 64)");
        let mut list = self.webhooks()?;
        ensure!(!list.iter().any(|w| w.name == hook.name), "A webhook with that name already exists");
        let mut secret = [0u8; 24];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut secret).map_err(|_| anyhow::anyhow!("No system randomness"))?;
        hook.secret = secret.iter().map(|b| format!("{b:02x}")).collect();
        hook.created_at = chrono::Utc::now().to_rfc3339();
        hook.enabled = true;
        if hook.deliver.is_empty() { hook.deliver = log(); }
        list.push(hook.clone());
        self.save_webhooks(&list)?;
        Ok(hook)
    }
    pub fn set_webhook_enabled(&self, name: &str, enabled: bool) -> Result<()> {
        let _lock = self.lock_webhooks()?;
        let mut list = self.webhooks()?;
        list.iter_mut().find(|w| w.name == name).context("No such webhook")?.enabled = enabled;
        self.save_webhooks(&list)
    }
    pub fn delete_webhook(&self, name: &str) -> Result<()> {
        let _lock = self.lock_webhooks()?;
        let mut list = self.webhooks()?;
        let before = list.len();
        list.retain(|w| w.name != name);
        ensure!(list.len() < before, "No such webhook");
        self.save_webhooks(&list)
    }
    /// Remember a delivery (newest first, 20 kept) and the chat its runs go to.
    pub fn record_delivery(&self, name: &str, delivery: Delivery, session_id: Option<String>) -> Result<()> {
        self.record_delivery_inner(name, None, delivery, session_id)
    }
    /// Bind a verified delivery only if its subscription has not been replaced or disabled.
    pub fn record_delivery_checked(&self, name: &str, secret: &str, delivery: Delivery, session_id: Option<String>) -> Result<()> {
        self.record_delivery_inner(name, Some(secret), delivery, session_id)
    }
    fn record_delivery_inner(&self, name: &str, expected_secret: Option<&str>, delivery: Delivery, session_id: Option<String>) -> Result<()> {
        let _lock = self.lock_webhooks()?;
        let mut list = self.webhooks()?;
        let hook = list.iter_mut().find(|w| w.name == name).context("No such webhook")?;
        if let Some(secret) = expected_secret { ensure!(hook.enabled && hook.secret == secret, "Webhook changed while processing delivery"); }
        if !delivery.id.as_ref().is_some_and(|id| hook.deliveries.iter().any(|item| item.id.as_ref() == Some(id))) {
            hook.deliveries.insert(0, delivery);
        }
        hook.deliveries.truncate(20);
        if session_id.is_some() { hook.session_id = session_id; }
        self.save_webhooks(&list)
    }
    /// Update a retained delivery without replacing another subscription's log.
    pub fn update_delivery_status(&self, name: &str, secret: &str, id: &str, status: &str) -> Result<()> {
        let _lock = self.lock_webhooks()?;
        let mut list = self.webhooks()?;
        let Some(hook) = list.iter_mut().find(|hook| hook.name == name && hook.secret == secret) else { return Ok(()); };
        let Some(delivery) = hook.deliveries.iter_mut().find(|delivery| delivery.id.as_deref() == Some(id)) else { return Ok(()); };
        if delivery.status != status { delivery.status = status.to_owned(); self.save_webhooks(&list)?; }
        Ok(())
    }
}

/// `sha256=<hex>` (GitHub's X-Hub-Signature-256, also accepted bare) over the raw body.
/// Anyone can sign with an empty key, so a subscription without a secret (a hand-edited or
/// restored webhooks.json) accepts nothing.
pub fn verify(secret: &str, body: &[u8], signature: &str) -> bool {
    if secret.is_empty() { return false; }
    let hex = signature.trim().strip_prefix("sha256=").unwrap_or(signature.trim());
    let Some(given) = (0..hex.len()).step_by(2).map(|i| hex.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok())).collect::<Option<Vec<u8>>>() else { return false };
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    ring::hmac::verify(&key, body, &given).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn create_verify_record_delete() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        let hook = library.create_webhook(Webhook { name: "GitHub Push".into(), prompt: "Summarize".into(), events: vec!["push".into()], ..Default::default() }).unwrap();
        assert_eq!(hook.name, "github-push");
        assert_eq!(hook.secret.len(), 48);
        assert!(library.create_webhook(Webhook { name: "github-push".into(), ..Default::default() }).is_err(), "names are unique");
        let body = br#"{"ref":"main"}"#;
        let sig = ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, hook.secret.as_bytes()), body);
        let hex: String = sig.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        assert!(verify(&hook.secret, body, &format!("sha256={hex}")));
        assert!(!verify(&hook.secret, b"tampered", &format!("sha256={hex}")));
        assert!(!verify(&hook.secret, body, "sha256=zz"));
        let unkeyed = ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b""), body);
        let unkeyed: String = unkeyed.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        assert!(!verify("", body, &format!("sha256={unkeyed}")), "an empty secret accepts nothing");
        library.record_delivery("github-push", Delivery { at: "now".into(), event: "push".into(), status: "ran".into(), id: None }, Some("s1".into())).unwrap();
        let saved = &library.webhooks().unwrap()[0];
        assert_eq!((saved.deliveries.len(), saved.session_id.as_deref()), (1, Some("s1")));
        library.set_webhook_enabled("github-push", false).unwrap();
        assert!(!library.webhooks().unwrap()[0].enabled);
        library.delete_webhook("github-push").unwrap();
        assert!(library.webhooks().unwrap().is_empty());
        let replacement = library.create_webhook(Webhook { name: "github-push".into(), ..Default::default() }).unwrap();
        assert_ne!(replacement.secret, hook.secret);
        assert!(library.record_delivery_checked("github-push", &hook.secret, Delivery { at: "later".into(), event: "push".into(), status: "received".into(), id: Some("old".into()) }, Some("old-session".into())).is_err());
        assert!(library.webhooks().unwrap()[0].session_id.is_none());
    }
    #[test]
    fn concurrent_subscription_creates_preserve_all_rows() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8).map(|i| {
            let library = library.clone(); let barrier = barrier.clone();
            std::thread::spawn(move || { barrier.wait(); library.create_webhook(Webhook { name: format!("hook-{i}"), ..Default::default() }).unwrap(); })
        }).collect();
        for thread in threads { thread.join().unwrap(); }
        assert_eq!(library.webhooks().unwrap().len(), 8);
    }

}
