//! A view onto an agent-launched PTY. The durable Core binding owns the
//! terminal. Dropping this view on gateway shutdown only detaches its reader.
use super::{Exit, Job, Log};
use crate::herdr::{Core, Frame, Terminal, TerminalInput};
use anyhow::Result;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio::{
    io::AsyncWriteExt,
    process::ChildStdin,
    sync::{Mutex as AsyncMutex, watch},
};
use tokio_util::sync::CancellationToken;

pub(super) enum Input {
    Pipe(ChildStdin),
    Pty(Arc<AsyncMutex<TerminalInput>>),
}

impl Input {
    pub async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        match self {
            Self::Pipe(input) => Ok(input.write_all(bytes).await?),
            Self::Pty(input) => input.lock().await.send(bytes).await,
        }
    }
}

impl Job {
    pub(super) async fn attach(core: Core, t: Terminal) -> Result<Self> {
        let status = core.status(&t).await?;
        let pid = status["pid"].as_u64().and_then(|v| u32::try_from(v).ok());
        let log = Arc::new(Mutex::new(Log::default()));
        let (sender, done) = watch::channel(None);
        let stop = CancellationToken::new();
        let mut job = Self {
            id: t.id.clone(),
            pid,
            stdin: None,
            pty: Some((core.clone(), t.clone())),
            stop: stop.clone(),
            done,
            log: log.clone(),
            cursor: 0,
            turn: usize::MAX,
            persist_on_release: false,
        };
        let cols = status["cols"]
            .as_u64()
            .and_then(|v| u16::try_from(v).ok())
            .unwrap_or(80);
        let rows = status["rows"]
            .as_u64()
            .and_then(|v| u16::try_from(v).ok())
            .unwrap_or(24);
        let stream = if status["state"] == "finished" {
            None
        } else {
            match core.attach(&t, cols, rows).await {
                Ok(stream) => Some(stream),
                Err(e) if core.status(&t).await?["state"] == "finished" => {
                    drop(e);
                    None
                }
                Err(e) => return Err(e),
            }
        };
        let Some(stream) = stream else {
            let snapshot = core.snapshot(&t).await?;
            log.lock()
                .expect("PTY log")
                .push(false, snapshot.as_bytes());
            sender.send_replace(Some(exit(&core.status(&t).await?)));
            return Ok(job);
        };
        let (mut output, input) = stream.split();
        let input = Arc::new(AsyncMutex::new(input));
        job.stdin = Some(Input::Pty(input.clone()));
        let (owner, terminal) = (core.clone(), t.clone());
        tokio::spawn(async move {
            let read = async {
                loop {
                    match output.next().await? {
                        Frame::Closed(value) => {
                            // An empty CLOSE acknowledges detach; it does not prove process exit.
                            if value["reason"].is_null() {
                                anyhow::bail!("PTY stream detached");
                            }
                            return Ok::<_, anyhow::Error>(exit(&value));
                        }
                        Frame::Bytes(bytes) => {
                            log.lock().expect("PTY log").push(false, &bytes);
                            input.lock().await.credit(bytes.len()).await?;
                        }
                    }
                }
            };
            tokio::select! {
                _ = stop.cancelled()=>{},
                result = read => {
                    let exit = match result {
                        Ok(exit) => exit,
                        // A lost stream is not an exit. Reported as one, Stop and cleanup would
                        // skip a command that still runs, so the terminal is ended first.
                        Err(error) => {
                            let running = owner.status(&terminal).await.map_or(true, |status| status["state"] != "finished");
                            if running { let _ = owner.kill(&terminal).await; }
                            let code = owner.status(&terminal).await.ok().and_then(|status| exit(&status).code);
                            let stopped = if running { "; the command was stopped" } else { "" };
                            Exit { code, error: Some(format!("PTY stream lost: {error:#}{stopped}")) }
                        }
                    };
                    sender.send_replace(Some(exit));
                }
            }
        });
        Ok(job)
    }
}

fn exit(status: &Value) -> Exit {
    Exit {
        code: status["exit_code"]
            .as_i64()
            .and_then(|v| i32::try_from(v).ok()),
        error: None,
    }
}

#[cfg(test)]
#[path = "../../test_support/terminal_host.rs"]
mod fixture_host;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::processes::{Registry, Request};
    use anyhow::Context;
    use serde_json::json;

    #[tokio::test]
    #[ignore = "requires pinned Herdr Core via HERMES_HERDR_CORE"]
    async fn approved_agent_pty_is_reattachable_only_by_its_owner() -> Result<()> {
        let profile = tempfile::tempdir()?;
        let _fixture_host = fixture_host::FixtureHost::for_profile(profile.path());
        let workspace = tempfile::tempdir()?;
        let cwd = workspace.path().join("agent-cwd");
        std::fs::create_dir(&cwd)?;
        let mut owner = Registry::new(workspace.path(), profile.path());
        owner.bind_terminals(profile.path(), "chat-a")?;
        let make = || {
            Request::parse(
                "terminal",
                json!({"command":r#"pwd > cwd-proof.txt; read -r word; printf 'GOT:%s\n' "$word"; read -r next"#,"workdir":cwd,"background":true,"pty":true,"persist_on_release":true}),
            )
        };
        let started = owner.invoke(make()?, 1, "agent-run").await?;
        let id = started["terminal_id"]
            .as_str()
            .context("missing terminal identity")?
            .to_owned();
        let core = Core::new(profile.path())?;
        let binding = core
            .lookup(&id)?
            .context("Desktop cannot resolve terminal")?;
        assert_eq!(binding.conversation_id.as_deref(), Some("chat-a"));
        let pid = core.status(&binding).await?["pid"].clone();
        assert!(pid.is_number());
        assert_eq!(owner.controls().release_turn(1).await?, 0);
        assert_eq!(core.status(&binding).await?["pid"], pid);
        let mut foreign = Registry::new(workspace.path(), workspace.path());
        foreign.bind_terminals(profile.path(), "chat-b")?;
        let poll = || Request::parse("process_manage", json!({"action":"poll","session_id":id}));
        assert!(foreign.invoke(poll()?, 0, "poll").await.is_err());
        drop(owner);
        let mut resumed = Registry::new(workspace.path(), profile.path());
        resumed.bind_terminals(profile.path(), "chat-a")?;
        let replay = resumed.invoke(make()?, 1, "agent-run").await?;
        assert_eq!(replay["terminal_id"], id);
        assert_eq!(core.status(&binding).await?["pid"], pid);
        resumed
            .invoke(
                Request::parse(
                    "process_manage",
                    json!({"action":"submit","session_id":id,"data":"works"}),
                )?,
                1,
                "input",
            )
            .await?;
        let output = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let mut text = String::new();
            loop {
                let v = resumed.invoke(poll()?, 1, "poll").await?;
                text.push_str(v["output"].as_str().unwrap_or_default());
                if text.contains("GOT:works") {
                    return Ok::<_, anyhow::Error>(text);
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await;
        let output = output??;
        assert!(output.contains("GOT:works"), "PTY did not report the submitted input: {output}");
        let cwd_evidence = std::fs::read_to_string(cwd.join("cwd-proof.txt"))?;
        assert!(cwd_evidence.contains("agent-cwd"), "PTY did not start in its requested cwd: {cwd_evidence}");
        resumed
            .invoke(
                Request::parse("process_manage", json!({"action":"close","session_id":id}))?,
                1,
                "eof",
            )
            .await?;
        assert!(resumed
            .invoke(
                Request::parse("process_manage", json!({"action":"submit","session_id":id,"data":"too-late"}))?,
                1,
                "after-eof",
            )
            .await
            .is_err());
        let exited = resumed
            .invoke(
                Request::parse("process_manage", json!({"action":"wait","session_id":id,"timeout":10}))?,
                1,
                "wait-after-eof",
            )
            .await?;
        assert_eq!(exited["status"], "exited", "{exited}");
        core.kill(&binding).await?;
        Ok(())
    }
}
