//! File operations shared by the desktop credential stores.
use std::{
    fs::{File, OpenOptions, TryLockError},
    io::Write,
    path::Path,
    time::{Duration, Instant},
};

/// Lock a stable sidecar so replacing or deleting the credential cannot invalidate the lock.
pub(super) fn lock(path: &Path) -> Result<File, String> {
    lock_for(path, Duration::from_secs(2))
}

pub(super) fn lock_for(path: &Path, wait: Duration) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))
        .map_err(|error| error.to_string())?;
    let deadline = Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(format!("Could not lock saved credentials: {error}")),
        }
    }
}

/// NamedTempFile is private by default on Unix. Persist only after the entire write succeeds.
pub(super) fn replace(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = path.parent().ok_or("Missing credential directory.")?;
    let mut temp = tempfile::NamedTempFile::new_in(dir).map_err(|error| error.to_string())?;
    temp.write_all(bytes).map_err(|error| error.to_string())?;
    temp.persist(path).map(|_| ()).map_err(|error| error.to_string())
}

pub(super) fn remove(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}
