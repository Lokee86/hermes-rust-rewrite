//! The read pump allows the gateway to answer a service call while a plugin callback waits.
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, btree_map::Entry},
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
type Cancel = Box<dyn FnOnce() + Send>;

enum ToolState {
    Queued,
    Running(Cancel),
    Cancelled,
}

pub struct Request {
    pub message: Value,
    pub tool: Option<ToolCall>,
}

pub struct ToolCall {
    wire: Arc<Wire>,
    id: String,
}

impl ToolCall {
    /// Returns whether cancellation was requested while the tool was queued.
    pub fn set_cancel_handler(&self, cancel: Cancel) -> bool {
        let mut tools = self.wire.tools.lock().unwrap();
        let tool = tools.get_mut(&self.id).expect("the request is registered until it is dropped");
        if matches!(tool, ToolState::Cancelled) {
            return true;
        }
        *tool = ToolState::Running(cancel);
        false
    }
}

impl Drop for ToolCall {
    fn drop(&mut self) {
        let tool = self.wire.tools.lock().unwrap().remove(&self.id);
        drop(tool);
    }
}

#[derive(Default)]
struct Pending {
    closed: bool,
    requests: BTreeMap<String, SyncSender<Reply>>,
}

#[derive(Default)]
pub struct Wire {
    service_id: AtomicU64,
    pending: Mutex<Pending>,
    tools: Mutex<BTreeMap<String, ToolState>>,
}

impl Wire {
    /// Requests, and cancellation callbacks to run outside the read pump.
    pub fn read_requests(self: &Arc<Self>) -> (Receiver<Request>, Receiver<Cancel>) {
        let (send, requests) = mpsc::sync_channel(64);
        let (cancel, cancelled) = mpsc::channel();
        let wire = self.clone();
        std::thread::spawn(move || wire.read(send, cancel));
        (requests, cancelled)
    }

    fn read(self: &Arc<Self>, requests: SyncSender<Request>, cancelled: mpsc::Sender<Cancel>) {
        let mut input = std::io::stdin().lock();
        let mut line = Vec::new();
        loop {
            line.clear();
            match (&mut input).take((MAX_MESSAGE + 1) as u64).read_until(b'\n', &mut line) {
                Ok(1..=MAX_MESSAGE) => {}
                _ => break,
            }
            let Ok(message) = serde_json::from_slice::<Value>(&line) else { continue };
            if message["jsonrpc"] != "2.0" {
                continue;
            }
            if message.get("id").is_none() {
                if message["method"] == "notifications/cancelled" {
                    if let Some(cancel) = self.cancel_tool(&message["params"]["requestId"]) {
                        let _ = cancelled.send(cancel);
                    }
                }
                continue;
            }
            if message.get("method").is_none() {
                self.receive_service_reply(message);
            } else {
                // Register before enqueueing so following notifications can cancel queued calls.
                let request = match self.register_request(message) {
                    Ok(request) => request,
                    Err(id) => {
                        if self.reply(&id, Err("The tool request ID is already in use".into())).is_err() {
                            break;
                        }
                        continue;
                    }
                };
                match requests.try_send(request) {
                    Err(TrySendError::Full(request)) => {
                        if self.reply(&request.message["id"], Err("The plugin is busy".into())).is_err() {
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

    fn register_request(self: &Arc<Self>, message: Value) -> Result<Request, Value> {
        let tool = if message["method"] == "tools/call" {
            let id = message["id"].to_string();
            let mut tools = self.tools.lock().unwrap();
            let Entry::Vacant(entry) = tools.entry(id.clone()) else {
                return Err(message["id"].clone());
            };
            entry.insert(ToolState::Queued);
            Some(ToolCall { wire: self.clone(), id })
        } else {
            None
        };
        Ok(Request { message, tool })
    }

    fn cancel_tool(&self, id: &Value) -> Option<Cancel> {
        let mut tools = self.tools.lock().unwrap();
        let tool = tools.get_mut(&id.to_string())?;
        match std::mem::replace(tool, ToolState::Cancelled) {
            ToolState::Running(cancel) => Some(cancel),
            _ => None,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_is_limited_to_live_tool_requests() {
        let wire = Arc::new(Wire::default());
        let id = json!(7);
        assert!(wire.cancel_tool(&id).is_none());
        assert!(wire.tools.lock().unwrap().is_empty());

        let message = json!({"id": id, "method": "tools/call"});
        let queued = wire.register_request(message.clone()).unwrap().tool.unwrap();
        assert!(wire.cancel_tool(&id).is_none());
        assert!(queued.set_cancel_handler(Box::new(|| unreachable!("queued cancellation is immediate"))));
        drop(queued);

        let running = wire.register_request(message.clone()).unwrap().tool.unwrap();
        let (send, received) = mpsc::channel();
        assert!(!running.set_cancel_handler(Box::new(move || send.send(()).unwrap())));
        let cancel = wire.cancel_tool(&id).unwrap();
        assert!(wire.cancel_tool(&id).is_none());
        cancel();
        assert!(received.try_recv().is_ok());
        drop(running);

        let finished = wire.register_request(message).unwrap().tool.unwrap();
        assert!(!finished.set_cancel_handler(Box::new(|| unreachable!("finished tools cannot be cancelled"))));
        drop(finished);
        assert!(wire.cancel_tool(&id).is_none());
        assert!(wire.tools.lock().unwrap().is_empty());
    }
}
