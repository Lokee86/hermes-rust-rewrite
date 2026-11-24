//! The read pump allows the gateway to answer a service call while a plugin callback waits.
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    time::Duration,
};

const MAX_MESSAGE: usize = 2 * 1024 * 1024;
type Reply = Result<Value, String>;

#[derive(Default)]
struct Pending {
    closed: bool,
    requests: BTreeMap<String, SyncSender<Reply>>,
}

#[derive(Default)]
pub struct Wire {
    service_id: AtomicU64,
    pending: Mutex<Pending>,
}

impl Wire {
    pub fn read_requests(self: &Arc<Self>) -> Receiver<Value> {
        let (send, requests) = mpsc::sync_channel(64);
        let wire = self.clone();
        std::thread::spawn(move || wire.read(send));
        requests
    }

    fn read(&self, requests: SyncSender<Value>) {
        let mut input = std::io::stdin().lock();
        let mut line = Vec::new();
        loop {
            line.clear();
            match (&mut input).take((MAX_MESSAGE + 1) as u64).read_until(b'\n', &mut line) {
                Ok(1..=MAX_MESSAGE) => {}
                _ => break,
            }
            let Ok(message) = serde_json::from_slice::<Value>(&line) else { continue };
            if message["jsonrpc"] != "2.0" || message.get("id").is_none() {
                continue;
            }
            if message.get("method").is_none() {
                self.receive_service_reply(message);
            } else {
                match requests.try_send(message) {
                    Err(TrySendError::Full(message)) => {
                        if self.reply(&message["id"], Err("The plugin is busy".into())).is_err() {
                            break;
                        }
                    }
                    Err(TrySendError::Disconnected(_)) => break,
                    Ok(()) => {}
                }
            }
        }
        let mut pending = self.pending.lock().unwrap();
        pending.closed = true;
        for (_, sender) in std::mem::take(&mut pending.requests) {
            let _ = sender.send(Err("Gateway plugin connection closed".into()));
        }
    }

    fn receive_service_reply(&self, reply: Value) {
        let Some(id) = reply["id"].as_str() else { return };
        let waiter = self.pending.lock().unwrap().requests.remove(id);
        if let Some(waiter) = waiter {
            let result = if let Some(error) = reply.get("error") {
                Err(error["message"].as_str().unwrap_or("Gateway service request failed").to_owned())
            } else {
                reply.get("result").cloned().ok_or_else(|| "Gateway service reply has no result".into())
            };
            let _ = waiter.send(result);
        }
    }

    pub fn service(&self, operation: &str, params: Value) -> Reply {
        let id = format!("host:{}", self.service_id.fetch_add(1, Ordering::Relaxed));
        let (sender, receiver) = mpsc::sync_channel(1);
        {
            let mut pending = self.pending.lock().unwrap();
            if pending.closed {
                return Err("Gateway plugin connection closed".into());
            }
            if pending.requests.len() >= 16 {
                return Err("Too many plugin service requests are in flight".into());
            }
            pending.requests.insert(id.clone(), sender);
        }
        let timeout = params["kwargs"]["timeout"].as_f64()
            .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
            .unwrap_or(30.0).clamp(1.0, 300.0);
        let request = json!({"jsonrpc": "2.0", "id": id, "method": "hermes/service",
            "params": {"operation": operation, "args": params["args"], "kwargs": params["kwargs"]}});
        let result = self.write(&request).map_err(|error| error.to_string()).and_then(|()| {
            receiver.recv_timeout(Duration::from_secs_f64(timeout + 1.0))
                .map_err(|error| format!("Gateway service reply unavailable: {error}"))?
        });
        self.pending.lock().unwrap().requests.remove(&id);
        result
    }

    pub fn reply(&self, id: &Value, result: Reply) -> std::io::Result<()> {
        let message = match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(message) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": message}}),
        };
        match self.write(&message) {
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => self.write(&json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32000, "message": "Plugin response is too large"},
            })),
            result => result,
        }
    }

    fn write(&self, message: &Value) -> std::io::Result<()> {
        let mut bytes = serde_json::to_vec(message)?;
        if bytes.len() >= MAX_MESSAGE {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "Message is too large"));
        }
        bytes.push(b'\n');
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(&bytes)?;
        stdout.flush()
    }
}
