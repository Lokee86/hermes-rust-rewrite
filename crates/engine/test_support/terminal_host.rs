//! Stop the Core that belongs to a disposable native-terminal fixture.
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub struct FixtureHost(PathBuf);

impl FixtureHost {
    pub fn for_profile(root: &Path) -> Self {
        Self(root.join("terminal-host"))
    }
}

impl Drop for FixtureHost {
    fn drop(&mut self) {
        let cli = std::env::var_os("HERMES_TEST_HERDR_CLI")
            .or_else(|| std::env::var_os("HERMES_HERDR_CORE"));
        let Some(binary) = cli else {
            return;
        };
        let Ok(identity) = std::fs::read(self.0.join("identity")) else {
            return;
        };
        if !Path::new(&binary).is_absolute()
            || identity.len() != 43
            || !identity
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        {
            return;
        }
        // The CLI addresses one server using its profile identity. It never needs to start one.
        let key = ring::digest::digest(&ring::digest::SHA256, &identity);
        let session = key.as_ref()[..12]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let mut command = Command::new(binary);
        command
            .args(["server", "stop"])
            .env("HC_SESSION", format!("hm-{session}"))
            .env("XDG_STATE_HOME", self.0.join("state"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .current_dir(&self.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        if let Err(error) = command.status() {
            eprintln!("Could not stop fixture terminal host: {error}");
        }
    }
}
