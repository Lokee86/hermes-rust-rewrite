//! Profiles (app/profiles): each one is its own Hermes — own keys, memory,
//! sessions and settings — living at `<root>/profiles/<name>`. The default
//! profile is the library root itself. Bots are profiles too (Bot Mode), so
//! they are listed, but renamed and deleted from the Bots page.
use crate::Library;
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use std::{fs, path::{Path, PathBuf}, process::Command};

#[derive(Clone, Debug, PartialEq)]
pub struct Profile { pub name: String, pub path: PathBuf, pub is_default: bool, pub is_bot: bool, pub model: String, pub skill_count: usize, pub sessions: usize }

pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().next().is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_') && name != "default"
}

pub fn name_for_home(home: &Path) -> &str {
    home.parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == "profiles"))
        .and_then(|_| home.file_name()?.to_str())
        .unwrap_or("default")
}

fn describe(name: &str, path: &Path, is_default: bool, is_bot: bool) -> Profile {
    let prefs: serde_json::Value = fs::read(path.join("desktop-preferences.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let skill_count = hermes_engine::skill_catalog::status(path).map(|s| s.installed.difference(&s.disabled).count()).unwrap_or(0);
    let sessions = crate::session_count(path);
    Profile { name: name.into(), path: path.into(), is_default, is_bot, model: prefs["model"].as_str().unwrap_or_default().into(), skill_count, sessions }
}

/// A profile's chat database inside its archive.
const EXPORTED_DATABASE: &str = "mercury-export.db";

impl Library {
    fn profiles_dir(&self) -> PathBuf { self.root().join("profiles") }
    pub(crate) fn lock_profiles(&self) -> Result<fs::File> {
        let path = self.root().join("profiles.lock");
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            ensure!(metadata.file_type().is_file(), "Profile lock must be a regular file");
        }
        let lock = fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(path)?;
        lock.lock_exclusive()?;
        Ok(lock)
    }
    pub(crate) fn ensure_profiles_dir(&self) -> Result<PathBuf> {
        let path = self.profiles_dir();
        fs::create_dir_all(&path)?;
        ensure!(fs::symlink_metadata(&path)?.file_type().is_dir(), "Profiles must be stored in a regular directory");
        Ok(path)
    }
    pub fn profiles(&self) -> Result<Vec<Profile>> {
        let bots: Vec<String> = self.bots().unwrap_or_default().into_iter().map(|b| b.name).collect();
        let mut out = vec![describe("default", self.root(), true, false)];
        if let Ok(entries) = fs::read_dir(self.profiles_dir()) {
            let mut named: Vec<_> = entries.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).filter_map(|e| e.file_name().into_string().ok()).filter(|n| valid_name(n)).collect();
            named.sort();
            for name in named { let is_bot = bots.contains(&name); out.push(describe(&name, &self.profiles_dir().join(&name), false, is_bot)); }
        }
        Ok(out)
    }
    pub fn profile_path(&self, name: &str) -> Result<PathBuf> {
        if name == "default" { return Ok(self.root().to_owned()); }
        ensure!(valid_name(name), "Lowercase letters, digits, hyphens and underscores. Must start with a letter or digit.");
        Ok(self.profiles_dir().join(name))
    }
    pub fn create_profile(&self, name: &str) -> Result<Profile> {
        let _lock = self.lock_profiles()?;
        let path = self.profile_path(name)?;
        ensure!(!self.bots()?.iter().any(|bot| bot.name == name), "A bot with that name already exists");
        ensure!(name != "default" && !path.exists(), "A profile with that name already exists");
        self.ensure_profiles_dir()?;
        fs::create_dir(&path).context("A profile with that name already exists")?;
        Ok(describe(name, &path, false, false))
    }
    pub fn rename_profile(&self, from: &str, to: &str) -> Result<()> {
        let _lock = self.lock_profiles()?;
        ensure!(from != "default", "The default profile can't be renamed");
        ensure!(!self.bots()?.iter().any(|b| b.name == from), "Rename bots from the Bots page");
        let (a, b) = (self.profile_path(from)?, self.profile_path(to)?);
        ensure!(a.is_dir(), "No such profile");
        ensure!(!b.exists(), "A profile with that name already exists");
        fs::rename(a, b)?;
        Ok(())
    }
    pub fn profile_path_for_deletion(&self, name: &str) -> Result<PathBuf> {
        ensure!(name != "default", "The default profile can't be deleted");
        ensure!(!self.bots()?.iter().any(|b| b.name == name), "Delete bots from the Bots page");
        let path = self.profile_path(name)?;
        ensure!(path.is_dir(), "No such profile");
        ensure!(
            fs::canonicalize(&path)? == path,
            "The profile directory must be inside the local profile store"
        );
        Ok(path)
    }

    pub fn delete_profile(&self, name: &str) -> Result<()> {
        self.delete_profile_with_cleanup(name, |_| Ok(()))
    }

    /// Run host cleanup while profile directory mutations are fenced.
    pub fn delete_profile_with_cleanup(&self, name: &str, cleanup: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
        let _lock = self.lock_profiles()?;
        let profile = self.profile_path_for_deletion(name)?;
        cleanup(&profile)?;
        hermes_engine::plugins::remove_dir(&profile)
    }
    pub fn profile_soul(&self, name: &str) -> Result<String> { Ok(fs::read_to_string(self.profile_path(name)?.join("SOUL.md")).unwrap_or_default()) }
    pub fn set_profile_soul(&self, name: &str, text: &str) -> Result<()> {
        let path = self.profile_path(name)?;
        fs::create_dir_all(&path)?;
        let file = tempfile::NamedTempFile::new_in(&path)?;
        fs::write(file.path(), text)?;
        file.persist(path.join("SOUL.md"))?;
        Ok(())
    }
    /// A `.tar.gz` of one named profile (its sessions, memory, settings, keys).
    pub fn export_profile(&self, name: &str, dest: &Path) -> Result<()> {
        ensure!(name != "default", "Back up the default profile from Settings › Backup");
        let profile = self.profile_path(name)?;
        ensure!(profile.is_dir(), "No such profile");
        // The database and its companions may change while tar reads them, so a copy goes instead.
        let copy = profile.join(EXPORTED_DATABASE);
        let _ = fs::remove_file(&copy);
        hermes_engine::db::snapshot(&profile, &copy)?;
        // The archive holds the profile's chats and settings: tar fills a private file, which then
        // takes the chosen name.
        let parent = dest.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let archive = tempfile::NamedTempFile::new_in(parent).context("Could not write the profile archive")?.into_temp_path();
        let status = Command::new("tar").arg("--exclude=mercury.db*").arg("-czf").arg(&archive).arg("-C").arg(self.profiles_dir()).arg(name).status();
        let _ = fs::remove_file(&copy);
        let status = status.context("tar is not available")?;
        ensure!(status.success(), "Could not write the profile archive");
        archive.persist(dest).context("Could not write the profile archive")?;
        Ok(())
    }
    /// Import an archive made by `export_profile`; returns the profile's name.
    pub fn import_profile(&self, archive: &Path) -> Result<String> {
        let _lock = self.lock_profiles()?;
        let listing = Command::new("tar").arg("-tzf").arg(archive).output().context("tar is not available")?;
        ensure!(listing.status.success(), "That file is not a profile archive");
        let entries = String::from_utf8_lossy(&listing.stdout).into_owned();
        let mut tops = entries.lines().filter_map(|l| l.trim_start_matches("./").split('/').next()).filter(|t| !t.is_empty()).collect::<Vec<_>>();
        tops.sort(); tops.dedup();
        let [name] = tops.as_slice() else { bail!("A profile archive holds exactly one profile folder") };
        ensure!(valid_name(name), "The archive's profile name is not valid");
        ensure!(!entries.lines().any(|l| l.starts_with('/') || l.split('/').any(|p| p == "..")), "The archive has unsafe paths");
        ensure!(!self.profiles_dir().join(name).exists(), "A profile named {name} already exists");
        ensure!(!self.bots()?.iter().any(|bot| bot.name == *name), "A bot with that name already exists");
        self.ensure_profiles_dir()?;
        // Unpacked beside the profiles first: a link in the archive would let the profile read,
        // chmod or overwrite a file outside it, so nothing appears until the whole tree passes.
        // The staging name is not a valid profile name, so the profile list never shows it.
        let staging = tempfile::Builder::new().prefix(".import-").tempdir_in(self.profiles_dir())?;
        let status = Command::new("tar").arg("-xzf").arg(archive).arg("-C").arg(staging.path()).status()?;
        ensure!(status.success(), "Could not unpack the profile archive");
        let unpacked = staging.path().join(name);
        ensure!(only_folders_and_files(&unpacked)?, "The archive holds links, which a profile archive never does");
        let profile = self.profiles_dir().join(name);
        fs::rename(&unpacked, &profile).context("Could not add the imported profile")?;
        let copy = profile.join(EXPORTED_DATABASE);
        if copy.is_file() {
            let database = profile.join(hermes_engine::db::FILE);
            fs::rename(copy, &database)?;
            // An archive from an earlier version stored the copy readable by everyone.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&database, fs::Permissions::from_mode(0o600))?;
            }
        }
        Ok(name.to_string())
    }
}

/// Whether the tree holds only folders and files with no other names: a symbolic link, or a file
/// hard-linked to one elsewhere, would reach outside the profile.
fn only_folders_and_files(path: &Path) -> Result<bool> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            if !only_folders_and_files(&entry?.path())? {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() > 1 {
            return Ok(false);
        }
    }
    Ok(metadata.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn create_rename_soul_export_import_delete() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        assert_eq!(library.profiles().unwrap().len(), 1, "just the default");
        library.create_profile("work").unwrap();
        assert!(library.create_profile("work").is_err() && library.create_profile("Bad Name").is_err() && library.create_profile("default").is_err());
        library.set_profile_soul("work", "You are focused.").unwrap();
        library.rename_profile("work", "office").unwrap();
        assert_eq!(library.profile_soul("office").unwrap(), "You are focused.");
        let chat = Library::open(library.profile_path("office").unwrap()).unwrap().create_session(None, Some("Exported chat")).unwrap();
        let (_open, _) = hermes_engine::ConversationWriter::open(&chat.directory).unwrap();
        let archive = dir.path().join("office.tar.gz");
        library.export_profile("office", &archive).unwrap();
        drop(_open);
        library.delete_profile("office").unwrap();
        assert_eq!(library.import_profile(&archive).unwrap(), "office");
        let imported = Library::open(library.profile_path("office").unwrap()).unwrap();
        assert_eq!(imported.session(&chat.id).unwrap().title, "Exported chat");
        assert!(!imported.root().join(EXPORTED_DATABASE).exists());
        assert!(library.import_profile(&archive).is_err(), "no overwrite");
        let names: Vec<_> = library.profiles().unwrap().into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["default", "office"]);
        assert!(library.delete_profile("default").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn an_exported_profile_and_its_imported_chats_stay_private() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        library.create_profile("work").unwrap();
        let profile = library.profile_path("work").unwrap();
        Library::open(&profile).unwrap().create_session(None, Some("Private chat")).unwrap();
        let archive = dir.path().join("work.tar.gz");
        library.export_profile("work", &archive).unwrap();
        assert_eq!(mode(&archive), 0o600, "the archive holds the profile's chats");
        assert!(!profile.join(EXPORTED_DATABASE).exists());
        library.delete_profile("work").unwrap();
        library.import_profile(&archive).unwrap();
        assert_eq!(mode(&profile.join(hermes_engine::db::FILE)), 0o600);

        // An archive from an earlier version stored its copy readable by everyone.
        let staging = tempfile::tempdir().unwrap();
        fs::create_dir(staging.path().join("old")).unwrap();
        hermes_engine::db::snapshot(&profile, &staging.path().join("old").join(EXPORTED_DATABASE)).unwrap();
        fs::set_permissions(staging.path().join("old").join(EXPORTED_DATABASE), fs::Permissions::from_mode(0o644)).unwrap();
        let old = dir.path().join("old.tar.gz");
        assert!(Command::new("tar").arg("-czf").arg(&old).arg("-C").arg(staging.path()).arg("old").status().unwrap().success());
        library.import_profile(&old).unwrap();
        assert_eq!(mode(&library.profile_path("old").unwrap().join(hermes_engine::db::FILE)), 0o600);
        assert!(hermes_engine::db::open_existing(&library.profile_path("old").unwrap()).unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn an_archive_with_links_is_refused_and_leaves_outside_files_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(&dir.path().join("home")).unwrap();
        let outside = dir.path().join("private.txt");
        fs::write(&outside, "secret").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o644)).unwrap();
        let pack = |name: &str, link: &dyn Fn(&Path)| {
            let staging = tempfile::tempdir().unwrap();
            let profile = staging.path().join(name);
            fs::create_dir(&profile).unwrap();
            link(&profile);
            let archive = dir.path().join(format!("{name}.tar.gz"));
            assert!(Command::new("tar").arg("-czf").arg(&archive).arg("-C").arg(staging.path()).arg(name).status().unwrap().success());
            archive
        };
        let symlinked = pack("symlinked", &|profile| {
            std::os::unix::fs::symlink(&outside, profile.join(EXPORTED_DATABASE)).unwrap();
            std::os::unix::fs::symlink(&outside, profile.join("SOUL.md")).unwrap();
        });
        let hard_linked = pack("hard-linked", &|profile| {
            fs::write(profile.join("notes.md"), "x").unwrap();
            fs::hard_link(profile.join("notes.md"), profile.join("SOUL.md")).unwrap();
        });
        for (archive, name) in [(symlinked, "symlinked"), (hard_linked, "hard-linked")] {
            assert!(library.import_profile(&archive).is_err(), "{name}");
            assert!(!library.profile_path(name).unwrap().exists(), "{name}");
        }
        assert_eq!(fs::metadata(&outside).unwrap().permissions().mode() & 0o777, 0o644);
        let names: Vec<_> = library.profiles().unwrap().into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["default"]);
        assert_eq!(fs::read_dir(library.root().join("profiles")).unwrap().count(), 0, "no staging left behind");
    }
}
