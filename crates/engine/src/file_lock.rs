//! A program being started shares this process's open files until it runs, locks included. A lock
//! released meanwhile stays held for that moment, so a lock that looks taken is tried again briefly.
use std::{
    fs::{File, TryLockError},
    io,
    time::{Duration, Instant},
};

const WAIT: Duration = Duration::from_millis(500);

pub(crate) fn exclusive(file: &File) -> io::Result<()> {
    briefly(|| file.try_lock())
}

pub(crate) fn shared(file: &File) -> io::Result<()> {
    briefly(|| file.try_lock_shared())
}

fn briefly(mut attempt: impl FnMut() -> Result<(), TryLockError>) -> io::Result<()> {
    let deadline = Instant::now() + WAIT;
    loop {
        match attempt() {
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            result => return result.map_err(io::Error::from),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lock_held_for_a_moment_is_waited_for_and_one_that_stays_held_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let open = || File::options().create(true).truncate(false).read(true).write(true).open(dir.path().join("lock")).unwrap();
        let held = open();
        held.lock().unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(held);
        });
        let mine = open();
        exclusive(&mine).unwrap();
        release.join().unwrap();
        assert_eq!(exclusive(&open()).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert!(shared(&open()).is_err());
        drop(mine);
        shared(&open()).unwrap();
    }
}
