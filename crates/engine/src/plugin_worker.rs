//! Private stdio transport for an embedded Python plugin.
use crate::{Config, mcp::Stdio, plugin_inference};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, process::Stdio as Pipe, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout},
    sync::{mpsc, oneshot, watch},
    task::JoinSet,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

const MAX_FRAME: usize = 2 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// A tool may legitimately work for a while (a download, a render) as long as a caller waits.
const TOOL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const DEADLINE_CHECK: Duration = Duration::from_secs(5);
const MAX_REQUESTS: usize = 32;
const MAX_SERVICES: usize = 4;

#[derive(Clone)]
pub(crate) struct Worker {
    requests: mpsc::Sender<Request>,
    stop: CancellationToken,
    exited: watch::Receiver<Option<std::result::Result<(), String>>>,
}

struct Request {
    method: String,
    params: Value,
    reply: oneshot::Sender<Result<Value>>,
}

struct Pending {
    limit: Duration,
    reply: oneshot::Sender<Result<Value>>,
}

impl Pending {
    /// A callback whose caller is gone gets only the ordinary limit, so a cancelled turn
    /// cannot leave the worker blocked.
    fn expired(&self, running: Duration) -> bool {
        running >= if self.reply.is_closed() { REQUEST_TIMEOUT.min(self.limit) } else { self.limit }
    }
}

impl Worker {
    pub(crate) fn start(spec: Stdio, root: PathBuf, config: Option<Config>) -> Result<Self> {
        let mut child = tokio::process::Command::new(&spec.command)
            .args(&spec.args)
            .envs(spec.env)
            .current_dir(spec.cwd)
            .stdin(Pipe::piped())
            .stdout(Pipe::piped())
            .stderr(Pipe::null())
            .kill_on_drop(true)
            .spawn()
            .context("Could not start the embedded Python host")?;
        let input = child.stdin.take().context("Python host input is unavailable")?;
        let output = child.stdout.take().context("Python host output is unavailable")?;
        let (requests, inbox) = mpsc::channel(MAX_REQUESTS);
        let stop = CancellationToken::new();
        let cancelled = stop.clone();
        let (exit, exited) = watch::channel(None);
        tokio::spawn(async move {
            let _ = exchange(input, output, inbox, root, config, &cancelled).await;
            let exit_result = match child.try_wait() {
                Ok(Some(_)) => Ok(()),
                Ok(None) => match child.kill().await {
                    Ok(()) => child.wait().await.map(|_| ()).context("Could not confirm embedded Python host exit"),
                    Err(error) => match child.try_wait() {
                        Ok(Some(_)) => Ok(()),
                        Ok(None) => Err(error).context("Could not terminate the embedded Python host"),
                        Err(wait_error) => Err(wait_error).context("Could not confirm embedded Python host exit"),
                    },
                },
                Err(error) => Err(error).context("Could not inspect embedded Python host exit"),
            };
            cancelled.cancel();
            let result = exit_result.map_err(|error| format!("{error:#}"));
            let _ = exit.send(Some(result));
        });
        Ok(Self { requests, stop, exited })
    }

    pub(crate) fn is_live(&self) -> bool {
        !self.stop.is_cancelled() && !self.requests.is_closed()
    }

    pub(crate) fn stop(&self) {
        self.stop.cancel();
    }

    pub(crate) async fn stop_and_wait(&self) -> Result<()> {
        self.stop();
        let mut exited = self.exited.clone();
        loop {
            if let Some(result) = exited.borrow().clone() {
                return result.map_err(anyhow::Error::msg);
            }
            exited.changed().await.context("Could not confirm embedded Python host exit")?;
        }
    }

    pub(crate) async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let (reply, response) = oneshot::channel();
        let request = Request { method: method.into(), params, reply };
        // The exchange bounds the running callback and drops every reply when it stops.
        self.requests.send(request).await.map_err(|_| anyhow::anyhow!("Python host exited"))?;
        response.await.context("Python host exited before replying")?
    }
}

async fn exchange(
    mut input: ChildStdin,
    output: ChildStdout,
    mut inbox: mpsc::Receiver<Request>,
    root: PathBuf,
    config: Option<Config>,
    stop: &CancellationToken,
) -> Result<()> {
    let mut output = BufReader::new(output);
    let mut bytes = Vec::new();
    // The host runs requests one at a time in order, so only the oldest one is running. Those
    // behind it are waiting and must not use up their own time.
    let mut pending = BTreeMap::<u64, Pending>::new();
    let mut running: Option<(u64, Instant)> = None;
    let mut next_id = 0_u64;
    let mut services = JoinSet::new();
    let mut check = tokio::time::interval(DEADLINE_CHECK);
    loop {
        let head = pending.keys().next().copied();
        if running.map(|(id, _)| id) != head {
            running = head.map(|id| (id, Instant::now()));
        }
        tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            _ = check.tick(), if running.is_some() => {
                if let Some((id, since)) = running {
                    ensure!(!pending[&id].expired(since.elapsed()), "A Python plugin callback exceeded its deadline");
                }
            }
            request = inbox.recv(), if pending.len() < MAX_REQUESTS => {
                let Some(request) = request else { return Ok(()) };
                next_id += 1;
                let frame = json!({
                    "jsonrpc":"2.0", "id":next_id, "method":request.method, "params":request.params
                });
                let limit = if request.method == "tools/call" { TOOL_TIMEOUT } else { REQUEST_TIMEOUT };
                pending.insert(next_id, Pending { limit, reply: request.reply });
                send(&mut input, frame, stop).await?;
            }
            message = receive(&mut output, &mut bytes) => {
                let Some(message) = message? else { return Ok(()) };
                if let Some(method) = message["method"].as_str() {
                    let Some(id) = message["id"].as_str().map(str::to_owned) else { continue };
                    if method != "hermes/service" || services.len() >= MAX_SERVICES {
                        send(&mut input, service_response(&id, Err(anyhow::anyhow!("Plugin service unavailable"))), stop).await?;
                        continue;
                    }
                    let profile = root.clone();
                    let chat = config.clone();
                    services.spawn(async move {
                        let response = plugin_inference::serve(&profile, chat.as_ref(), &message["params"]).await;
                        service_response(&id, response)
                    });
                } else if let Some(Pending { reply, .. }) = message["id"].as_u64().and_then(|id| pending.remove(&id)) {
                    let result = if let Some(error) = message.get("error") {
                        Err(anyhow::anyhow!("{}", error["message"].as_str().unwrap_or("Plugin request failed")))
                    } else {
                        message.get("result").cloned().context("Missing Python host result")
                    };
                    let _ = reply.send(result);
                }
            }
            result = services.join_next(), if !services.is_empty() => {
                if let Some(Ok(response)) = result {
                    send(&mut input, response, stop).await?;
                }
            }
        }
    }
}

fn service_response(id: &str, result: Result<Value>) -> Value {
    match result {
        Ok(value) => json!({"jsonrpc":"2.0", "id":id, "result":value}),
        Err(error) => json!({"jsonrpc":"2.0", "id":id, "error":{"code":-32000,"message":error.to_string()}}),
    }
}

async fn send(pipe: &mut ChildStdin, message: Value, stop: &CancellationToken) -> Result<()> {
    let mut bytes = serde_json::to_vec(&message)?;
    ensure!(bytes.len() <= MAX_FRAME, "Python host request exceeds the frame limit");
    bytes.push(b'\n');
    tokio::select! {
        _ = stop.cancelled() => bail!("Python host stopped"),
        result = pipe.write_all(&bytes) => result?,
    }
    Ok(())
}

async fn receive(
    reader: &mut BufReader<impl AsyncRead + Unpin>,
    partial: &mut Vec<u8>,
) -> Result<Option<Value>> {
    loop {
        let slice = reader.fill_buf().await?;
        if slice.is_empty() {
            return Ok(None);
        }
        let newline = slice.iter().position(|b| *b == b'\n');
        let size = newline.map_or(slice.len(), |position| position + 1);
        ensure!(partial.len() + size <= MAX_FRAME + 1, "Python host reply exceeds the frame limit");
        partial.extend_from_slice(&slice[..size]);
        reader.consume(size);
        if newline.is_some() {
            let frame = std::mem::take(partial);
            // A program the plugin runs can inherit the host's stdout. Its output is not a frame.
            if let Ok(message) = serde_json::from_slice::<Value>(&frame)
                && message["jsonrpc"] == "2.0"
            {
                return Ok(Some(message));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs2::FileExt;
    use std::{fs::OpenOptions, thread};

    #[test]
    fn a_slow_tool_outlives_the_ordinary_limit_only_while_someone_waits() {
        let (reply, waiting) = oneshot::channel();
        let tool = Pending { limit: TOOL_TIMEOUT, reply };
        assert!(!tool.expired(REQUEST_TIMEOUT * 2));
        assert!(tool.expired(TOOL_TIMEOUT));
        drop(waiting);
        assert!(!tool.expired(REQUEST_TIMEOUT / 2));
        assert!(tool.expired(REQUEST_TIMEOUT));
    }

    #[tokio::test]
    async fn stray_output_between_frames_is_skipped() {
        let (mut plugin, host) = tokio::io::duplex(256);
        plugin.write_all(b"Cloning into 'x'...\n42\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n").await.unwrap();
        let frame = receive(&mut BufReader::new(host), &mut Vec::new()).await.unwrap().unwrap();
        assert_eq!(frame["id"], 1);
    }

    #[tokio::test]
    async fn stop_and_wait_confirms_the_child_released_its_file_lock() {
        let directory = tempfile::tempdir().unwrap();
        let lock_path = directory.path().join("worker.lock");
        let ready_path = directory.path().join("worker.ready");
        let args = vec![
            "--exact".into(),
            "plugin_worker::tests::child_holds_file_lock_until_killed".into(),
            "--nocapture".into(),
        ];
        let worker = Worker::start(
            Stdio {
                command: std::env::current_exe().unwrap().to_string_lossy().into_owned(),
                args,
                env: vec![
                    ("HERMES_TEST_WORKER_LOCK".into(), lock_path.to_string_lossy().into_owned()),
                    ("HERMES_TEST_WORKER_READY".into(), ready_path.to_string_lossy().into_owned()),
                ],
                cwd: directory.path().to_owned(),
            },
            directory.path().to_owned(),
            None,
        )
        .unwrap();

        tokio::time::timeout(Duration::from_secs(10), async {
            while !ready_path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let probe = OpenOptions::new().read(true).write(true).open(&lock_path).unwrap();
        assert!(probe.try_lock_exclusive().is_err(), "child should still own the lock");

        worker.stop_and_wait().await.unwrap();
        probe.try_lock_exclusive().unwrap();
    }

    #[test]
    fn child_holds_file_lock_until_killed() {
        let (Ok(lock_path), Ok(ready_path)) = (
            std::env::var("HERMES_TEST_WORKER_LOCK"),
            std::env::var("HERMES_TEST_WORKER_READY"),
        ) else {
            return;
        };
        let lock = OpenOptions::new().create(true).truncate(false).read(true).write(true)
            .open(lock_path).unwrap();
        lock.lock_exclusive().unwrap();
        std::fs::write(ready_path, b"ready").unwrap();
        loop {
            thread::sleep(Duration::from_secs(60));
        }
    }
}
