//! On-device speech recognition in the built-in Python (faster-whisper, as in the Python agent).
//! Its packages and the model are downloaded into the app's data folder on first use.
use crate::{mcp::{Mcp, Stdio}, plugins};
use anyhow::{Context, Result, bail, ensure};
use serde_json::json;
use std::{fs, path::Path, process::Command};

const PACKAGE: &str = "faster-whisper==1.2.1";
const PLUGIN: &str = r#"
import os
import wave

_model = None

def _transcribe(args, **_):
    global _model
    if _model is None:
        from faster_whisper import WhisperModel
        _model = WhisperModel("base", device="cpu", compute_type="int8", download_root=os.environ["HERMES_VOICE_MODELS"])
    import numpy
    # The recording is already what the model takes: 16 kHz, mono, 16-bit.
    with wave.open(args["path"]) as recording:
        if (recording.getframerate(), recording.getnchannels(), recording.getsampwidth()) != (16000, 1, 2):
            raise ValueError("Recordings must be 16 kHz mono 16-bit WAV")
        frames = recording.readframes(recording.getnframes())
    audio = numpy.frombuffer(frames, dtype="<i2").astype(numpy.float32) / 32768.0
    segments, _info = _model.transcribe(audio, language=args.get("language") or None, vad_filter=True)
    return {"text": " ".join(segment.text.strip() for segment in segments)}

def register(ctx):
    ctx.register_tool("transcribe", "voice", {"name": "transcribe", "description": "Transcribe a recording",
        "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}, _transcribe)
"#;

/// Whether this build can recognize speech.
pub fn available() -> bool {
    plugins::python_host().is_some()
}

/// Whether the recognizer has been downloaded.
pub fn installed(data: &Path) -> bool {
    data.join("voice/packages/faster_whisper").is_dir()
}

/// The words in a WAV recording.
pub async fn transcribe(data: &Path, recording: &Path, language: Option<&str>) -> Result<String> {
    let host = plugins::python_host().context("This build has no Python to recognize speech with")?;
    let dir = data.join("voice");
    let (packages, plugin, models) = (dir.join("packages"), dir.join("recognizer"), dir.join("models"));
    for folder in [&plugin, &models] {
        fs::create_dir_all(folder)?;
    }
    if !installed(data) {
        // Prepared aside, so an interrupted download is not taken for an installation.
        let (host, staged, target) = (host.clone(), tempfile::tempdir_in(&dir)?, packages.clone());
        tokio::task::spawn_blocking(move || -> Result<()> {
            let output = Command::new(host).arg("install").arg(staged.path()).arg(PACKAGE).output().context("Could not start the Python host")?;
            if !output.status.success() {
                let text = String::from_utf8_lossy(&output.stderr);
                bail!("Could not download speech recognition: {}", text.lines().rev().find(|line| line.contains("ERROR")).unwrap_or("check the internet connection"));
            }
            plugins::remove_dir(&target)?;
            fs::rename(staged.keep(), target)?;
            Ok(())
        }).await??;
    }
    fs::write(plugin.join("__init__.py"), PLUGIN)?;
    let mut server = Mcp::connect_stdio(Stdio {
        command: host.to_string_lossy().into_owned(),
        args: vec!["serve".into(), plugin.to_string_lossy().into_owned(), packages.to_string_lossy().into_owned()],
        env: vec![("HERMES_VOICE_MODELS".into(), models.to_string_lossy().into_owned())],
        cwd: dir,
    }, "voice").await?;
    let language = language.map(|locale| locale.split(['-', '_']).next().unwrap_or_default().to_lowercase());
    let reply = server.call("voice__transcribe", &json!({"path":recording.to_string_lossy(), "language":language})).await;
    server.close().await;
    let reply = reply?;
    let text = reply["content"][0]["text"].as_str().unwrap_or_default();
    ensure!(reply["isError"] != true, "Speech recognition failed: {}", text.chars().take(300).collect::<String>());
    let value: serde_json::Value = serde_json::from_str(text)?;
    Ok(value["text"].as_str().unwrap_or_default().trim().to_owned())
}

#[cfg(test)]
mod tests {
    /// Downloads the recognizer: `cargo test -- --ignored recognizes_speech`. Uses macOS `say`.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "downloads packages and a model"]
    async fn recognizes_speech() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("speech.wav");
        let said = std::process::Command::new("say").args(["-o", wav.to_str().unwrap(), "--data-format=LEI16@16000", "The quick brown fox jumps over the lazy dog"]).status().unwrap();
        assert!(said.success());
        let text = super::transcribe(dir.path(), &wav, Some("en-US")).await.unwrap().to_lowercase();
        assert!(text.contains("quick brown fox"), "{text}");
        assert!(super::installed(dir.path()));
    }
}
