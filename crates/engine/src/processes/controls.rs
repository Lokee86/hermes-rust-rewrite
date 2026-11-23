//! Read and stop background jobs without waiting for the turn holding the model session.
use super::{Exit, Job, Log, MAX_JOBS};
use crate::herdr::{Core, Terminal};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct Running {
    id: String,
    pid: Option<u32>,
    cwd: PathBuf,
    command: String,
    log: Arc<Mutex<Log>>,
    stop: CancellationToken,
    done: watch::Receiver<Option<Exit>>,
    pty: Option<(Core, Terminal)>,
    turn: usize,
    persist_on_release: bool,
}

impl Running {
    fn row(&self) -> Value {
        let exit = self.done.borrow().clone();
        let (captured, _) = self.log.lock().expect("process log").snapshot(0);
        let mut text = captured["output"].as_str().unwrap_or("").to_owned();
        crate::privacy::redact(&mut text);
        let start = text.char_indices().rev().nth(1999).map_or(0, |(i, _)| i);
        let mut cmd = self.command.clone();
        crate::privacy::redact(&mut cmd);
        json!({"session_id":self.id,"command":cmd,"cwd":self.cwd,"pid":self.pid,
            "status":if exit.is_some() {"exited"} else {"running"},"exit_code":exit.and_then(|e|e.code),
            "output_tail":&text[start..],"session_scoped":true,"persist_on_release":self.persist_on_release})
    }
}

#[derive(Clone, Default)]
pub struct ProcessControls {
    entries: Arc<Mutex<VecDeque<Running>>>,
    pub(super) owner: Option<(Core, String)>,
}

impl ProcessControls {
    pub(super) fn register(&self, job: &Job, cwd: &Path, command: &str) {
        let mut entries = self.entries.lock().expect("process controls");
        entries.retain(|p| p.id != job.id);
        if entries.len() >= MAX_JOBS
            && let Some(i) = entries.iter().position(|p| p.done.borrow().is_some())
        {
            entries.remove(i);
        }
        entries.push_back(Running {
            id: job.id.clone(),
            pid: job.pid,
            cwd: cwd.into(),
            command: command.into(),
            log: job.log.clone(),
            stop: job.stop.clone(),
            done: job.done.clone(),
            pty: job.pty.clone(),
            turn: job.turn,
            persist_on_release: job.persist_on_release,
        });
    }

    pub fn has_running_jobs(&self) -> bool {
        self.entries
            .lock()
            .expect("process controls")
            .iter()
            .any(|p| p.done.borrow().is_none())
    }

    pub async fn list(&self) -> Result<Vec<Value>> {
        let mut rows: Vec<_> = self
            .entries
            .lock()
            .expect("process controls")
            .iter()
            .map(Running::row)
            .collect();
        if let Some((core, owner)) = &self.owner {
            for t in core.agent_terminals(owner)? {
                if rows.iter().any(|r| r["session_id"] == t.id) {
                    continue;
                }
                let status = core.status(&t).await?;
                rows.push(
                    json!({"session_id":t.id,"command":t.shell,"cwd":t.cwd,"pid":status["pid"],
                    "status":if status["state"] == "live" {"running"} else {"exited"},
                    "exit_code":status["exit_code"],"session_scoped":true}),
                );
            }
        }
        Ok(rows)
    }

    pub async fn kill(&self, id: &str) -> Result<Option<Value>> {
        let entry = self
            .entries
            .lock()
            .expect("process controls")
            .iter()
            .find(|p| p.id == id)
            .cloned();
        if let Some(mut p) = entry {
            let already = p.done.borrow().is_some();
            if !already {
                if let Some((core, t)) = &p.pty {
                    core.kill(t).await?;
                } else {
                    p.stop.cancel();
                }
                tokio::time::timeout(Duration::from_secs(10), p.done.wait_for(Option::is_some))
                    .await
                    .context("Process termination pending")??;
            }
            let mut row = p.row();
            row["status"] = json!(if already { "already_exited" } else { "killed" });
            row["output"] = row["output_tail"].clone();
            return Ok(Some(row));
        }
        if let Some((core, owner)) = &self.owner
            && let Some(t) = core.lookup(id)?
            && t.conversation_id.as_deref() == Some(owner.as_str())
        {
            let status = core.status(&t).await?;
            let already = status["state"] == "finished";
            core.kill(&t).await?;
            return Ok(Some(
                json!({"session_id":id,"status":if already {"already_exited"} else {"killed"},"exit_code":status["exit_code"]}),
            ));
        }
        Ok(None)
    }

    /// An abandoned turn releases only the jobs it started. Explicitly
    /// persisted jobs survive; an explicit kill or stop still reaches them.
    pub async fn release_turn(&self, turn: usize) -> Result<usize> {
        let ids: Vec<_> = self.entries.lock().expect("process controls").iter()
            .filter(|entry| entry.turn == turn && !entry.persist_on_release && entry.done.borrow().is_none())
            .map(|entry| entry.id.clone()).collect();
        let mut released = 0;
        for id in ids {
            if self.kill(&id).await?.is_some_and(|result| result["status"] == "killed") { released += 1; }
        }
        Ok(released)
    }

    pub async fn stop(&self) -> Result<usize> {
        let mut killed = 0;
        for row in self.list().await? {
            if row["status"] == "running"
                && let Some(id) = row["session_id"].as_str()
                && let Some(outcome) = self.kill(id).await?
                && outcome["status"] == "killed"
            {
                killed += 1;
            }
        }
        Ok(killed)
    }
}
