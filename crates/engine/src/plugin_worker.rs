//! Private stdio transport for an embedded Python plugin.
use crate::{Config, mcp::Stdio, plugin_inference};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{collections::HashMap, path::PathBuf, process::Stdio as Pipe, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout},
    sync::{mpsc, oneshot},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

const MAX_FRAME: usize = 2 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_REQUESTS: usize = 32;
const MAX_SERVICES: usize = 4;

#[derive(Clone)]
pub(crate) struct Worker {
    requests: mpsc::Sender<Request>,
    stop: CancellationToken,
}

struct Request {
    method: String,
    params: Value,
    reply: oneshot::Sender<Result<Value>>,
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
        tokio::spawn(async move {
            let _child = child;
            let _ = exchange(input, output, inbox, root, config, &cancelled).await;
            cancelled.cancel();
        });
        Ok(Self { requests, stop })
    }

    pub(crate) fn is_live(&self) -> bool {
        !self.stop.is_cancelled() && !self.requests.is_closed()
    }

    pub(crate) fn stop(&self) {
        self.stop.cancel();
    }

    pub(crate) async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let (reply, response) = oneshot::channel();
        let request = Request { method: method.into(), params, reply };
        match tokio::time::timeout(REQUEST_TIMEOUT, async {
            self.requests.send(request).await.map_err(|_| anyhow::anyhow!("Python host exited"))?;
            response.await.context("Python host exited before replying")?
        }).await {
            Ok(result) => result,
            Err(_) => {
                self.stop();
                bail!("Python host timed out in {method}");
            }
        }
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
    let mut pending = HashMap::new();
    let mut next_id = 0_u64;
    let mut services = JoinSet::new();
    loop {
        tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            request = inbox.recv(), if pending.len() < MAX_REQUESTS => {
                let Some(request) = request else { return Ok(()) };
                next_id += 1;
                let frame = json!({
                    "jsonrpc":"2.0", "id":next_id, "method":request.method, "params":request.params
                });
                pending.insert(next_id, request.reply);
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
                } else if let Some(reply) = message["id"].as_u64().and_then(|id| pending.remove(&id)) {
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
    reader: &mut BufReader<ChildStdout>,
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
            return Ok(Some(serde_json::from_slice(&frame).context("Invalid Python host frame")?));
        }
    }
}
