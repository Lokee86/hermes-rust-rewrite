//! Credentials at rest. Saved API keys and sign-in tokens are encrypted with a key kept in the
//! system's credential store: the macOS Keychain, Windows Credential Manager or the Linux Secret
//! Service. Where there is none (a headless Linux host, say) they stay in files only the user's
//! account can read, as before.
//!
//! Only the app's own data directory uses the credential store. Another directory may be a copy, a
//! test or a different machine's data.
use anyhow::{Context, Result, anyhow, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use fs2::FileExt;
use ring::{
    aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey},
    rand::{SecureRandom, SystemRandom},
};
use std::{path::Path, sync::OnceLock, time::Duration};

const MARK: &str = "hermes-sealed:1:";
const SERVICE: &str = "Hermes Rust Desktop";
const ACCOUNT: &str = "storage-key";
/// The system may ask the user to allow access.
const ASK: Duration = Duration::from_secs(30);

type Key = [u8; 32];

fn own_directory(dir: &Path) -> bool {
    if cfg!(test) || std::env::var_os("HERMES_RUST_DATA_DIR").is_some() {
        return false;
    }
    let Ok(base) = crate::Session::default_dir() else { return false };
    let resolve = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_owned());
    resolve(dir).starts_with(resolve(&base))
}

/// The storage key, created on first use. Asked for once per process.
fn key() -> Option<&'static Key> {
    static KEY: OnceLock<Option<Key>> = OnceLock::new();
    KEY.get_or_init(|| {
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = send.send(load_or_create());
        });
        match receive.recv_timeout(ASK) {
            Ok(Ok(key)) => Some(key),
            Ok(Err(error)) => {
                eprintln!("The system credential store is unavailable: {error:#}");
                None
            }
            Err(_) => {
                eprintln!("The system credential store did not answer");
                None
            }
        }
    }).as_ref()
}

fn load_or_create() -> Result<Key> {
    let base = crate::Session::default_dir()?;
    std::fs::create_dir_all(&base)?;
    // The desktop app and its gateway may both be the first to need the key.
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(base.join("storage-key.lock"))?;
    lock.lock_exclusive()?;
    let entry = keyring::Entry::new(SERVICE, ACCOUNT)?;
    match entry.get_secret() {
        Ok(saved) => saved.try_into().map_err(|_| anyhow!("The saved storage key is not valid")),
        Err(keyring::Error::NoEntry) => {
            let mut key = Key::default();
            SystemRandom::new().fill(&mut key).map_err(|_| anyhow!("The system random source failed"))?;
            entry.set_secret(&key)?;
            ensure!(entry.get_secret()? == key, "The credential store did not keep the storage key");
            Ok(key)
        }
        Err(error) => Err(error.into()),
    }
}

/// Encrypt text that will be stored in `dir`, where that is possible.
pub(crate) fn seal(dir: &Path, text: &str) -> Result<String> {
    if text.is_empty() || !own_directory(dir) {
        return Ok(text.to_owned());
    }
    match key() {
        Some(key) => seal_with(key, text),
        None => Ok(text.to_owned()),
    }
}

/// Whether this value is stored encrypted.
pub(crate) fn is_sealed(text: &str) -> bool {
    text.starts_with(MARK)
}

pub(crate) fn unseal(text: &str) -> Result<String> {
    if !is_sealed(text) {
        return Ok(text.to_owned());
    }
    let key = key().context("The system credential store is locked or unavailable, so the saved credential could not be read")?;
    unseal_with(key, text)
}

fn seal_with(key: &Key, text: &str) -> Result<String> {
    let cipher = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key).map_err(|_| anyhow!("Invalid storage key"))?);
    let mut nonce = [0; NONCE_LEN];
    SystemRandom::new().fill(&mut nonce).map_err(|_| anyhow!("The system random source failed"))?;
    let mut data = text.as_bytes().to_vec();
    cipher.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(MARK), &mut data)
        .map_err(|_| anyhow!("Could not encrypt the credential"))?;
    let mut out = nonce.to_vec();
    out.append(&mut data);
    Ok(format!("{MARK}{}", STANDARD.encode(out)))
}

fn unseal_with(key: &Key, text: &str) -> Result<String> {
    let mut data = STANDARD.decode(text.strip_prefix(MARK).context("Not an encrypted credential")?)
        .context("The saved credential is damaged")?;
    ensure!(data.len() > NONCE_LEN, "The saved credential is damaged");
    let mut sealed = data.split_off(NONCE_LEN);
    let cipher = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key).map_err(|_| anyhow!("Invalid storage key"))?);
    let nonce = Nonce::try_assume_unique_for_key(&data).map_err(|_| anyhow!("The saved credential is damaged"))?;
    let plain = cipher.open_in_place(nonce, Aad::from(MARK), &mut sealed)
        .map_err(|_| anyhow!("The saved credential was encrypted with a different key. Enter it again"))?;
    Ok(String::from_utf8(plain.to_vec())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_text_needs_its_key_and_is_not_readable() {
        let sealed = seal_with(&[7; 32], "sk-fixture-value").unwrap();
        assert!(is_sealed(&sealed) && !sealed.contains("fixture"));
        assert_ne!(sealed, seal_with(&[7; 32], "sk-fixture-value").unwrap());
        assert_eq!(unseal_with(&[7; 32], &sealed).unwrap(), "sk-fixture-value");
        assert!(unseal_with(&[8; 32], &sealed).is_err());
        assert!(unseal_with(&[7; 32], "hermes-sealed:1:AAAA").is_err());
        assert_eq!(unseal("plain from an earlier version").unwrap(), "plain from an earlier version");
    }

    #[test]
    fn other_directories_do_not_use_the_credential_store() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(seal(dir.path(), "value").unwrap(), "value");
    }

    /// Uses the real credential store: `cargo test -- --ignored the_system_store`
    #[test]
    #[ignore = "writes an entry to the system credential store"]
    fn the_system_store_keeps_a_secret() {
        let entry = keyring::Entry::new("Hermes Rust test", "storage-key-test").unwrap();
        entry.set_secret(&[3; 32]).unwrap();
        assert_eq!(entry.get_secret().unwrap(), [3; 32]);
        entry.delete_credential().unwrap();
    }
}
