//! Settings › About › Updates. Hermes looks for a newer release when asked, or at startup if that
//! was turned on, and downloads it when asked. Nothing is installed by itself.
use super::{Hermes, Message, Session};
use iced::Task;
use serde::{Deserialize, Serialize};
use std::{io::Write, path::PathBuf, time::Duration};

/// By repository ID, which a rename does not change. Download addresses carry the current name.
const LATEST: &str = "https://api.github.com/repositories/1468454330/releases/latest";
const OWNER: &str = "https://github.com/itsflownium/";
pub(super) const RELEASES: &str = "https://github.com/itsflownium/mercury-hermes-rust-rewrite/releases";
const MAX_DOWNLOAD: u64 = 1024 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(super) enum Action {
    Check,
    /// The flag is whether the user asked, so a failed startup check stays quiet.
    Checked(bool, Result<Option<Update>, String>),
    Download,
    Downloaded(Result<PathBuf, String>),
    /// Open what was downloaded and close Hermes so it can be replaced.
    Install,
    AtStartup(bool),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Update {
    pub version: String,
    pub page: String,
    name: String,
    url: String,
    sha256: String,
}

#[derive(Default, Serialize, Deserialize)]
struct Preferences {
    #[serde(default)]
    at_startup: bool,
}

#[derive(Default)]
pub(super) struct State {
    pub at_startup: bool,
    pub busy: bool,
    pub update: Option<Update>,
    pub saved: Option<PathBuf>,
    pub note: Option<String>,
}

fn path() -> Option<PathBuf> {
    Session::default_dir().ok().map(|dir| dir.join("update-settings.json"))
}

impl State {
    pub fn open() -> Self {
        let saved: Preferences = path().and_then(|path| std::fs::read(path).ok()).and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        Self { at_startup: saved.at_startup, ..Self::default() }
    }
}

/// `1.2.3` from `v1.2.3`. A pre-release suffix is not newer than its release.
fn version(text: &str) -> Option<(u64, u64, u64)> {
    let text = text.trim().trim_start_matches('v').split(['-', '+']).next()?;
    let mut parts = text.split('.').map(|part| part.parse::<u64>().ok());
    let found = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(found)
}

/// The file to offer on this system.
fn asset_name() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "Hermes-macOS.dmg",
        ("windows", "x86_64") => "Hermes-windows-AMD64-setup.exe",
        ("linux", "x86_64") => "Hermes-linux-x86_64.tar.gz",
        _ => return None,
    })
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<Asset>,
}
#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    /// `sha256:<hex>`, computed by GitHub on upload.
    digest: Option<String>,
}

/// `https://github.com/itsflownium/<repository>/releases/download/…`
fn is_download(url: &str) -> bool {
    url.strip_prefix(OWNER).and_then(|rest| rest.split_once('/')).is_some_and(|(repository, rest)| {
        !repository.is_empty() && repository.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            && rest.starts_with("releases/download/") && !rest.contains("..")
    })
}

fn newer(release: Release, current: &str, wanted: &str) -> Option<Update> {
    if version(&release.tag_name)? <= version(current)? {
        return None;
    }
    let asset = release.assets.into_iter().find(|asset| asset.name == wanted);
    let (name, url, sha256) = asset.and_then(|asset| {
        let sha256 = asset.digest?.strip_prefix("sha256:")?.to_ascii_lowercase();
        let valid = sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit()) && is_download(&asset.browser_download_url);
        valid.then_some((asset.name, asset.browser_download_url, sha256))
    }).unwrap_or_default();
    Some(Update { version: release.tag_name.trim_start_matches('v').to_owned(), page: release.html_url, name, url, sha256 })
}

fn client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder().user_agent(concat!("HermesRust/", env!("CARGO_PKG_VERSION"))).connect_timeout(Duration::from_secs(15)).read_timeout(Duration::from_secs(60)).build()
}

async fn check() -> Result<Option<Update>, String> {
    let response = client().map_err(|e| e.to_string())?.get(LATEST).header("accept", "application/vnd.github+json").send().await
        .map_err(|_| "Could not reach GitHub to look for updates.".to_owned())?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(format!("GitHub answered {} when asked for updates.", response.status()));
    }
    let release: Release = response.json().await.map_err(|_| "The release information could not be read.".to_owned())?;
    Ok(newer(release, env!("CARGO_PKG_VERSION"), asset_name().unwrap_or_default()))
}

async fn download(update: Update) -> Result<PathBuf, String> {
    let dirs = directories::UserDirs::new().ok_or("No Downloads folder was found.")?;
    let folder = dirs.download_dir().unwrap_or_else(|| dirs.home_dir()).to_path_buf();
    let mut response = client().map_err(|e| e.to_string())?.get(&update.url).send().await.and_then(reqwest::Response::error_for_status)
        .map_err(|_| "The update could not be downloaded.".to_owned())?;
    let mut file = tempfile::NamedTempFile::new_in(&folder).map_err(|e| e.to_string())?;
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    let mut size = 0;
    while let Some(chunk) = response.chunk().await.map_err(|_| "The download was interrupted.".to_owned())? {
        size += chunk.len() as u64;
        if size > MAX_DOWNLOAD {
            return Err("The download is larger than expected.".into());
        }
        hash.update(&chunk);
        file.write_all(&chunk).map_err(|e| e.to_string())?;
    }
    let found: String = hash.finish().as_ref().iter().map(|byte| format!("{byte:02x}")).collect();
    if found != update.sha256 {
        return Err("The download does not match the published checksum, so it was discarded.".into());
    }
    let target = folder.join(format!("{}-{}", update.version, update.name));
    file.persist(&target).map_err(|e| e.error.to_string())?;
    Ok(target)
}

/// Start the downloaded installer. Returns whether Hermes must close for it.
fn open_installer(path: &std::path::Path) -> std::io::Result<bool> {
    use std::process::Command;
    if cfg!(windows) {
        Command::new(path).args(["/SILENT", "/CLOSEAPPLICATIONS"]).spawn()?;
        return Ok(true);
    }
    if cfg!(target_os = "macos") {
        // Shows Hermes beside the Applications folder.
        Command::new("/usr/bin/open").arg(path).spawn()?;
        return Ok(true);
    }
    // An archive has to be unpacked over the old folder; show where it is.
    Command::new("xdg-open").arg(path.parent().unwrap_or(path)).spawn()?;
    Ok(false)
}

pub(super) fn at_startup(app: &mut Hermes) -> Task<Message> {
    if !app.updates.at_startup {
        return Task::none();
    }
    Task::perform(check(), |result| Message::Updates(Action::Checked(false, result)))
}

pub(super) fn update(app: &mut Hermes, action: Action) -> Task<Message> {
    let state = &mut app.updates;
    match action {
        Action::Check if !state.busy => {
            (state.busy, state.note, state.saved) = (true, None, None);
            return Task::perform(check(), |result| Message::Updates(Action::Checked(true, result)));
        }
        Action::Checked(asked, result) => {
            state.busy = false;
            match result {
                Ok(Some(update)) => {
                    let text = format!("Hermes {} is available.", update.version);
                    state.note = Some(text.clone());
                    state.update = Some(update);
                    if !asked { app.note(format!("{text} See Settings › About.")); }
                }
                Ok(None) if asked => state.note = Some("You have the latest release.".into()),
                Err(error) if asked => state.note = Some(error),
                _ => {}
            }
        }
        Action::Download if !state.busy => {
            let Some(found) = state.update.clone() else { return Task::none() };
            if found.url.is_empty() {
                return Task::done(Message::Link(found.page));
            }
            (state.busy, state.note) = (true, Some("Downloading…".into()));
            return Task::perform(download(found), |result| Message::Updates(Action::Downloaded(result)));
        }
        Action::Downloaded(result) => {
            state.busy = false;
            match result {
                Ok(path) => {
                    state.note = Some(format!("Saved and verified: {}.", path.display()));
                    state.saved = Some(path);
                }
                Err(error) => state.note = Some(error),
            }
        }
        Action::Install if !state.busy => {
            let Some(path) = state.saved.clone() else { return Task::none() };
            if app.running || !app.approvals.is_empty() {
                state.note = Some("Wait for the current response, or stop it, before installing.".into());
                return Task::none();
            }
            return match open_installer(&path) {
                Ok(true) => iced::exit(),
                Ok(false) => Task::none(),
                Err(error) => { state.note = Some(format!("Could not open {}: {error}", path.display())); Task::none() }
            };
        }
        Action::AtStartup(on) => {
            state.at_startup = on;
            let saved = path().ok_or("No data folder".to_owned()).and_then(|path| {
                let folder = path.parent().ok_or("No data folder")?;
                std::fs::create_dir_all(folder).map_err(|e| e.to_string())?;
                let file = tempfile::NamedTempFile::new_in(folder).map_err(|e| e.to_string())?;
                serde_json::to_writer(file.as_file(), &Preferences { at_startup: on }).map_err(|e| e.to_string())?;
                file.persist(path).map(|_| ()).map_err(|e| e.error.to_string())
            });
            if let Err(error) = saved { state.note = Some(error); }
        }
        _ => {}
    }
    Task::none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, digest: &str, url: &str) -> Release {
        serde_json::from_value(serde_json::json!({"tag_name":tag,"html_url":"https://github.com/x","assets":[
            {"name":"Hermes-macOS.dmg","browser_download_url":url,"digest":digest}]})).unwrap()
    }

    #[test]
    fn only_newer_verifiable_releases_are_offered() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let url = format!("{RELEASES}/download/v0.2.0/Hermes-macOS.dmg");
        let found = newer(release("v0.2.0", &digest, &url), "0.1.0", "Hermes-macOS.dmg").unwrap();
        assert_eq!((found.version.as_str(), found.sha256.len()), ("0.2.0", 64));
        assert!(newer(release("v0.1.0", &digest, &url), "0.1.0", "Hermes-macOS.dmg").is_none());
        assert!(newer(release("v0.0.9", &digest, &url), "0.1.0", "Hermes-macOS.dmg").is_none());
        assert!(newer(release("nightly", &digest, &url), "0.1.0", "Hermes-macOS.dmg").is_none());
        // Without a checksum, or from another address, only the release page is offered.
        for (digest, url) in [("", url.as_str()), ("sha256:zz", &url), (&digest, "https://example.com/Hermes-macOS.dmg")] {
            assert!(newer(release("v0.2.0", digest, url), "0.1.0", "Hermes-macOS.dmg").unwrap().url.is_empty());
        }
        // A renamed repository keeps working; another owner or path does not.
        assert!(is_download("https://github.com/itsflownium/renamed-again/releases/download/v1.0.0/Hermes-macOS.dmg"));
        for url in ["https://github.com/someone/mercury-hermes-rust-rewrite/releases/download/v1.0.0/x", "https://github.com/itsflownium/x/raw/main/x",
            "https://github.com/itsflownium/../other/x/releases/download/x", "https://github.com/itsflownium/x/releases/download/../../x"] {
            assert!(!is_download(url), "{url}");
        }
        assert!(version("0.10.0") > version("0.9.9"));
        assert_eq!(version("1.2"), None);
    }
}
