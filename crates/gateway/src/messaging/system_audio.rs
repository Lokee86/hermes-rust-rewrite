//! Produce a private PCM WAV with the installed local speech service.
//! Text is stdin data, never a shell expression or command argument.

use super::{media::MAX_FILE, types::GatewayError};
use std::{io::Read, path::Path, process::Stdio};
use tokio::{io::AsyncWriteExt, process::Command};

pub(super) async fn synthesize(profile: &Path, text: &str) -> Result<Vec<u8>, GatewayError> {
    if text.trim().is_empty() || text.len() > 16 * 1024 {
        return Err(GatewayError::Invalid);
    }
    let folder = tempfile::tempdir_in(profile).map_err(|_| GatewayError::TemporarilyUnavailable)?;
    let output = folder.path().join("reply.wav");
    let (mut cmd, input) = service(text, &output).ok_or(GatewayError::Unsupported)?;
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| GatewayError::TemporarilyUnavailable)?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or(GatewayError::TemporarilyUnavailable)?;
    stdin
        .write_all(&input)
        .await
        .map_err(|_| GatewayError::TemporarilyUnavailable)?;
    stdin
        .shutdown()
        .await
        .map_err(|_| GatewayError::TemporarilyUnavailable)?;
    drop(stdin);
    let status = child
        .wait()
        .await
        .map_err(|_| GatewayError::TemporarilyUnavailable)?;
    if !status.success() {
        return Err(GatewayError::TemporarilyUnavailable);
    }
    super::runtime::blocking(move || {
        let _folder = folder;
        let mut bytes = Vec::new();
        std::fs::File::open(output)
            .map_err(|_| GatewayError::TemporarilyUnavailable)?
            .take((MAX_FILE + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| GatewayError::TemporarilyUnavailable)?;
        if bytes.len() > MAX_FILE {
            return Err(GatewayError::Invalid);
        }
        Ok(bytes)
    })
    .await
}

#[cfg(target_os = "windows")]
fn service(text: &str, output: &Path) -> Option<(Command, Vec<u8>)> {
    let path = std::path::PathBuf::from(std::env::var_os("SystemRoot")?)
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    if !path.is_file() {
        return None;
    }
    const SCRIPT: &str = r#"$ErrorActionPreference='Stop'
[Console]::InputEncoding=[System.Text.UTF8Encoding]::new()
Add-Type -AssemblyName System.Speech
$p=([Console]::In.ReadToEnd() | ConvertFrom-Json)
$s=New-Object System.Speech.Synthesis.SpeechSynthesizer
try {
    $format=[System.Speech.AudioFormat.SpeechAudioFormatInfo]::new(16000,
        [System.Speech.AudioFormat.AudioBitsPerSample]::Sixteen,
        [System.Speech.AudioFormat.AudioChannel]::Mono)
    $s.SetOutputToWaveFile([string]$p.output, $format)
    $s.Speak([string]$p.text)
} finally { $s.Dispose() }"#;
    let mut command = Command::new(path);
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            SCRIPT,
        ])
        .creation_flags(0x0800_0000);
    let input = serde_json::to_vec(&serde_json::json!({"text": text, "output": output})).ok()?;
    Some((command, input))
}

#[cfg(target_os = "linux")]
fn service(text: &str, output: &Path) -> Option<(Command, Vec<u8>)> {
    let binary = [
        "/usr/bin/espeak-ng",
        "/usr/local/bin/espeak-ng",
        "/usr/bin/espeak",
        "/usr/local/bin/espeak",
    ]
    .into_iter()
    .map(std::path::PathBuf::from)
    .find(|path| path.is_file())?;
    let mut command = Command::new(binary);
    command.args(["--stdin", "-w"]).arg(output);
    Some((command, text.as_bytes().to_vec()))
}

#[cfg(target_os = "macos")]
fn service(text: &str, output: &Path) -> Option<(Command, Vec<u8>)> {
    if !Path::new("/usr/bin/say").is_file() {
        return None;
    }
    let mut command = Command::new("/usr/bin/say");
    command
        .args(["--data-format=LEI16@16000", "-f", "-", "-o"])
        .arg(output);
    Some((command, text.as_bytes().to_vec()))
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
fn service(_text: &str, _output: &Path) -> Option<(Command, Vec<u8>)> {
    None
}
