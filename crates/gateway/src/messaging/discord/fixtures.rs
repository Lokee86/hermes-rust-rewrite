use super::{DiscordSettings, settings::DiscordFactory};
use crate::{
    RunningGateway,
    messaging::{AdapterConfig, EnrollmentCredential, conformance::Model, runtime::now},
};
use axum::{
    Json, Router,
    body::to_bytes,
    extract::{
        Request, State, WebSocketUpgrade,
        ws::{CloseFrame, Message, WebSocket},
    },
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
};
use reqwest::Url;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::mpsc,
    time::{sleep, timeout},
};

const LIMIT: Duration = Duration::from_secs(15);

#[derive(Clone)]
struct Server {
    gateway: String,
    incoming: mpsc::Sender<Wire>,
    writes: mpsc::Sender<Write>,
    counter: Arc<AtomicU64>,
    messages: Arc<Mutex<BTreeMap<(String, String), Value>>>,
    history: Arc<Mutex<BTreeMap<String, Vec<Value>>>>,
}

struct Wire {
    to_bot: mpsc::Sender<WireEvent>,
    from_bot: mpsc::Receiver<Value>,
}

enum WireEvent {
    Json(Value),
    NoHeartbeatAcks,
    Close(u16),
}

impl Wire {
    async fn data(&self, value: Value) {
        self.to_bot.send(WireEvent::Json(value)).await.unwrap();
    }

    async fn dispatch(&self, sequence: u64, kind: &str, data: Value) {
        self.data(json!({"op":0,"t":kind,"s":sequence,"d":data}))
            .await;
    }

    async fn authentication(&mut self, code: u64) -> Value {
        timeout(LIMIT, async {
            while let Some(event) = self.from_bot.recv().await {
                if event["op"] == code {
                    return event;
                }
            }
            panic!("Discord did not authenticate");
        })
        .await
        .unwrap()
    }
}

enum Write {
    Message {
        channel: String,
        payload: Value,
        id: String,
    },
    Callback(Value),
    Followup(Value),
}

struct Fixtures {
    transport: Server,
    api: Url,
    wires: mpsc::Receiver<Wire>,
    writes: mpsc::Receiver<Write>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Fixtures {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Fixtures {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (incoming, wires) = mpsc::channel(16);
        let (writes_tx, writes) = mpsc::channel(32);
        let transport = Server {
            gateway: format!("ws://{address}/"),
            incoming,
            writes: writes_tx,
            counter: Arc::new(AtomicU64::new(500)),
            messages: Arc::default(),
            history: Arc::default(),
        };
        let endpoint = transport.clone();
        let router = Router::new()
            .route("/", axum::routing::get(upgrade))
            .fallback(rest)
            .with_state(endpoint);
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            transport,
            api: Url::parse(&format!("http://{address}/api/v10/")).unwrap(),
            wires,
            writes,
            task,
        }
    }

    async fn wire(&mut self) -> Wire {
        timeout(LIMIT, self.wires.recv())
            .await
            .expect("Discord socket connection")
            .unwrap()
    }

    async fn write(&mut self) -> Write {
        self.next_write("Discord REST write", |_| true).await
    }

    async fn next_write(&mut self, expected: &str, accepts: impl Fn(&Write) -> bool) -> Write {
        timeout(LIMIT, async {
            while let Some(write) = self.writes.recv().await {
                if accepts(&write) {
                    return write;
                }
            }
            panic!("Discord REST fixture stopped");
        })
        .await
        .unwrap_or_else(|_| panic!("Missing {expected}"))
    }

    async fn reply(&mut self) -> Write {
        self.next_write("a final Discord reply", |write| {
            matches!(write, Write::Message { payload, .. } if payload["content"].as_str().is_some_and(|text| text.starts_with("answer")))
        }).await
    }

    async fn ready(&self, wire: &Wire, id: &str) {
        wire.dispatch(
            1,
            "READY",
            json!({
                "user":{"id":"9","bot":true},"application":{"id":"90"},
                "session_id":id,"resume_gateway_url":self.transport.gateway,
            }),
        )
        .await;
    }

    fn configuration(&self, root: &Path) -> AdapterConfig {
        self.configuration_with(
            root,
            json!({
                "grants":{"users":["7"]},
                "operators":["7"],
                "direct":{"rules":{"turn":{"tools":true,"approval":"manual"}}},
                "guilds":{"11":{"channels":{"21":{"threads":{"22":{}}}}}},
                "commands":{"enabled":true,"sync":false},
            }),
        )
    }

    fn configuration_with(&self, root: &Path, settings: Value) -> AdapterConfig {
        let settings: DiscordSettings = serde_json::from_value(settings).unwrap();
        AdapterConfig {
            name: "fixture-discord".into(),
            policy: settings.host_policy(),
            factory: Arc::new(
                DiscordFactory::for_fixture(root.to_owned(), settings, self.api.clone()).unwrap(),
            ),
        }
    }

    async fn activate(&self, gateway: &RunningGateway) {
        let enrollment = gateway
            .begin_adapter_enrollment("fixture-discord", Duration::from_secs(30))
            .await
            .unwrap();
        gateway
            .complete_adapter_enrollment(
                "fixture-discord",
                &enrollment.id,
                EnrollmentCredential::new("fixture-bot-secret".into()).unwrap(),
            )
            .await
            .unwrap();
    }
}

async fn upgrade(State(state): State<Server>, websocket: WebSocketUpgrade) -> Response {
    websocket.on_upgrade(move |socket| run_wire(state, socket))
}

async fn run_wire(state: Server, mut socket: WebSocket) {
    let (to_bot, mut outbound) = mpsc::channel(16);
    let (inbound, from_bot) = mpsc::channel(16);
    if state
        .incoming
        .send(Wire { to_bot, from_bot })
        .await
        .is_err()
    {
        return;
    }
    let hello = json!({"op":10,"d":{"heartbeat_interval":125}});
    if socket
        .send(Message::Text(hello.to_string().into()))
        .await
        .is_err()
    {
        return;
    }
    let mut ack = true;
    loop {
        tokio::select! {
            operation = outbound.recv() => match operation {
                Some(WireEvent::Json(value)) => {
                    if socket.send(Message::Text(value.to_string().into())).await.is_err() { return }
                }
                Some(WireEvent::NoHeartbeatAcks) => ack = false,
                Some(WireEvent::Close(code)) => {
                    let _ = socket.send(Message::Close(Some(CloseFrame { code, reason: "fixture".into() }))).await;
                    return;
                }
                None => return,
            },
            received = socket.recv() => match received {
                Some(Ok(Message::Text(text))) => {
                    let event: Value = serde_json::from_str(&text).unwrap();
                    if event["op"] == 1 {
                        if ack && socket.send(Message::Text(r#"{"op":11,"d":null}"#.into())).await.is_err() { return }
                    } else if inbound.send(event).await.is_err() { return }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                _ => {},
            }
        }
    }
}

async fn rest(State(state): State<Server>, request: Request) -> Response {
    let path = request
        .uri()
        .path()
        .strip_prefix("/api/v10/")
        .unwrap_or("")
        .to_owned();
    let method = request.method().clone();
    let auth = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok());
    if path.starts_with("interactions/") || path.starts_with("webhooks/") {
        assert!(
            auth.is_none(),
            "Discord bot token must stay out of callback requests"
        );
        let bytes = to_bytes(request.into_body(), 64 * 1024).await.unwrap();
        let payload = serde_json::from_slice::<Value>(&bytes).unwrap();
        let sent = if path.starts_with("interactions/") {
            Write::Callback(payload)
        } else {
            Write::Followup(payload)
        };
        let _ = state.writes.send(sent).await;
        return StatusCode::NO_CONTENT.into_response();
    }
    if auth != Some("Bot fixture-bot-secret") {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let segments = path.split('/').collect::<Vec<_>>();
    let document = match (method, segments.as_slice()) {
        (Method::GET, ["users", "@me"]) => json!({"id":"9","bot":true}),
        (Method::GET, ["applications", "@me"]) => {
            json!({"id":"90","bot":{"id":"9"},"flags":835584})
        }
        (Method::GET, ["gateway", "bot"]) => json!({
            "url":state.gateway,"shards":1,
            "session_start_limit":{"total":1000,"remaining":1000,"reset_after":1000,"max_concurrency":1}
        }),
        (Method::GET, ["channels", "20"]) => json!({"id":"20","type":1}),
        (Method::GET, ["channels", "21"]) => json!({"id":"21","type":0,"guild_id":"11"}),
        (Method::GET, ["channels", "22"]) => {
            json!({"id":"22","type":11,"guild_id":"11","parent_id":"21"})
        }
        (Method::GET, ["channels", "23"]) => json!({"id":"23","type":0,"guild_id":"12"}),
        (Method::GET, ["channels", channel, "messages"]) => {
            json!(
                crate::lock(&state.history)
                    .get(*channel)
                    .cloned()
                    .unwrap_or_default()
            )
        }
        (Method::GET, ["channels", channel, "messages", id]) => {
            let message = crate::lock(&state.messages)
                .get(&(channel.to_string(), id.to_string()))
                .cloned();
            let Some(message) = message else {
                return StatusCode::NOT_FOUND.into_response();
            };
            message
        }
        (Method::POST, ["channels", channel, "messages"]) => {
            let bytes = to_bytes(request.into_body(), 256 * 1024).await.unwrap();
            let payload = serde_json::from_slice::<Value>(&bytes).unwrap();
            let id = state.counter.fetch_add(1, Ordering::SeqCst).to_string();
            let message = json!({"id":id,"channel_id":channel,"author":{"id":"9","bot":true},
                "nonce":payload["nonce"],"content":payload["content"]});
            crate::lock(&state.messages).insert((channel.to_string(), id.clone()), message.clone());
            let _ = state
                .writes
                .send(Write::Message {
                    channel: channel.to_string(),
                    payload,
                    id,
                })
                .await;
            message
        }
        (Method::PATCH, ["channels", channel, "messages", id]) => {
            let bytes = to_bytes(request.into_body(), 256 * 1024).await.unwrap();
            let payload = serde_json::from_slice::<Value>(&bytes).unwrap();
            let message = json!({"id":id,"channel_id":channel,"author":{"id":"9","bot":true},"content":payload["content"]});
            crate::lock(&state.messages)
                .insert((channel.to_string(), id.to_string()), message.clone());
            let _ = state
                .writes
                .send(Write::Message {
                    channel: channel.to_string(),
                    payload,
                    id: id.to_string(),
                })
                .await;
            message
        }
        (Method::DELETE, ["channels", channel, "messages", id]) => {
            crate::lock(&state.messages).remove(&(channel.to_string(), id.to_string()));
            return StatusCode::NO_CONTENT.into_response();
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    Json(document).into_response()
}

fn message(id: &str, channel: &str, content: &str, mentioned: bool) -> Value {
    let mut event = json!({"id":id,"channel_id":channel,"content":content,"author":{"id":"7"},"type":0,
        "mentions":if mentioned { vec![json!({"id":"9","bot":true})] } else { Vec::new() }});
    if channel != "20" {
        event["guild_id"] = json!("11");
        event["member"] = json!({"roles":[]});
    }
    event
}

async fn wait_status(gateway: &RunningGateway, condition: impl Fn(&Value) -> bool) -> Value {
    timeout(LIMIT, async {
        loop {
            let status = gateway.messaging_status().await.unwrap();
            if condition(&status) {
                return status;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("gateway status transition")
}

async fn wait_reply(gateway: &RunningGateway, event: &str) {
    use crate::messaging::store::key;
    wait_status(gateway, |state| {
        let Some(source) = state["input"]
            .as_array()
            .and_then(|inputs| inputs.iter().find(|input| input["id"] == event))
        else {
            return false;
        };
        let connection = source["connection"].as_str().unwrap();
        let admission = key("channel-input", &(connection, event)).unwrap();
        let id = key("reply", &admission).unwrap();
        state["deliveries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|delivery| delivery["id"] == id && delivery["state"] == "sent")
    })
    .await;
}

#[tokio::test]
async fn discord_identifies_resumes_deduplicates_and_routes_threads_through_the_native_gateway() {
    let root = tempfile::tempdir().unwrap();
    let model = Model::new().await;
    let mut fixture = Fixtures::start().await;
    let gateway = model
        .gateway(root.path(), vec![fixture.configuration(root.path())])
        .await;
    fixture.activate(&gateway).await;
    let mut wire = fixture.wire().await;
    assert_eq!(
        wire.authentication(2).await["d"]["token"],
        "fixture-bot-secret"
    );
    assert_eq!(
        gateway.messaging_status().await.unwrap()["connections"][0]["health"],
        "connecting"
    );
    fixture.ready(&wire, "durable-session").await;
    wire.dispatch(2, "MESSAGE_CREATE", message("100", "20", "first", false))
        .await;
    let Write::Message {
        channel, payload, ..
    } = fixture.reply().await
    else {
        panic!("expected direct reply")
    };
    assert_eq!(channel, "20");
    assert_eq!(payload["content"], "answer first");
    assert_eq!(payload["allowed_mentions"]["parse"], json!([]));
    wire.dispatch(
        3,
        "MESSAGE_CREATE",
        message("101", "22", "not addressed", false),
    )
    .await;
    wire.dispatch(
        4,
        "MESSAGE_CREATE",
        message("102", "22", "<@9> threaded", true),
    )
    .await;
    let Write::Message {
        channel, payload, ..
    } = fixture.reply().await
    else {
        panic!("expected thread reply")
    };
    assert_eq!(channel, "22");
    assert_eq!(payload["content"], "answer threaded");
    wait_reply(&gateway, "discord-message-102").await;
    gateway.shutdown().await.unwrap();

    let gateway = model
        .gateway(root.path(), vec![fixture.configuration(root.path())])
        .await;
    let mut wire = fixture.wire().await;
    let resume = wire.authentication(6).await;
    assert_eq!(resume["d"]["session_id"], "durable-session");
    assert_eq!(resume["d"]["seq"], 4);
    wire.dispatch(5, "RESUMED", json!({})).await;
    wire.dispatch(6, "MESSAGE_CREATE", message("100", "20", "first", false))
        .await;
    wire.dispatch(7, "MESSAGE_CREATE", message("103", "20", "next", false))
        .await;
    let Write::Message {
        channel, payload, ..
    } = fixture.reply().await
    else {
        panic!("expected second direct reply")
    };
    assert_eq!(channel, "20");
    assert_eq!(payload["content"], "answer next");
    assert_eq!(model.requests().len(), 3);
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn discord_missing_heartbeat_ack_resumes_then_invalid_session_reports_gap() {
    let root = tempfile::tempdir().unwrap();
    let model = Model::new().await;
    let mut fixture = Fixtures::start().await;
    let gateway = model
        .gateway(root.path(), vec![fixture.configuration(root.path())])
        .await;
    fixture.activate(&gateway).await;
    let mut wire = fixture.wire().await;
    wire.authentication(2).await;
    fixture.ready(&wire, "before-gap").await;
    wait_status(&gateway, |state| {
        state["connections"][0]["health"] == "ready"
    })
    .await;
    wire.to_bot.send(WireEvent::NoHeartbeatAcks).await.unwrap();

    let mut resumed = fixture.wire().await;
    assert_eq!(resumed.authentication(6).await["d"]["seq"], 1);
    resumed.data(json!({"op":9,"d":false})).await;
    let mut fresh = fixture.wire().await;
    fresh.authentication(2).await;
    fixture.ready(&fresh, "after-gap").await;
    wait_status(&gateway, |state| {
        state["connections"][0]["health"] == "ready" && state["connections"][0]["gap"] == true
    })
    .await;
    fresh.to_bot.send(WireEvent::Close(4014)).await.unwrap();
    wait_status(&gateway, |state| {
        state["connections"][0]["health"] == "configuration"
    })
    .await;
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn discord_fresh_sessions_repair_bounded_history_without_admitting_overlap() {
    let root = tempfile::tempdir().unwrap();
    let model = Model::new().await;
    let mut fixture = Fixtures::start().await;
    let settings = json!({
        "grants":{"users":["7"]},
        "guilds":{"11":{"channels":{"21":{}}}},
        "commands":{"sync":false},
        "recovery":{"history_window_secs":120,"max_surfaces":1,"max_messages":10}
    });
    let gateway = model
        .gateway(
            root.path(),
            vec![fixture.configuration_with(root.path(), settings)],
        )
        .await;
    fixture.activate(&gateway).await;
    let mut first = fixture.wire().await;
    first.authentication(2).await;
    fixture.ready(&first, "known-history").await;
    let base = ((now() - 1_420_070_400_000) as u64) << 22;
    let original_id = (base + 1).to_string();
    let original = message(&original_id, "21", "<@9> original", true);
    first.dispatch(2, "MESSAGE_CREATE", original.clone()).await;
    fixture.reply().await;
    wait_reply(&gateway, &format!("discord-message-{original_id}")).await;
    let missing_id = (base + 2).to_string();
    let missing = message(&missing_id, "21", "<@9> recovered", true);
    crate::lock(&fixture.transport.history).insert("21".into(), vec![missing, original]);
    first.data(json!({"op":9,"d":false})).await;
    let mut replacement = fixture.wire().await;
    replacement.authentication(2).await;
    fixture.ready(&replacement, "after-history").await;
    let Write::Message {
        channel, payload, ..
    } = fixture.reply().await
    else {
        panic!("expected reply to recovered Discord history")
    };
    assert_eq!(channel, "21");
    assert_eq!(payload["content"], "answer recovered");
    wait_reply(&gateway, &format!("discord-message-{missing_id}")).await;
    assert_eq!(model.requests().len(), 2);
    assert_eq!(
        gateway.messaging_status().await.unwrap()["connections"][0]["gap"],
        true
    );
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn discord_role_updates_revoke_guild_input_without_blocking_an_independent_dm() {
    let root = tempfile::tempdir().unwrap();
    let model = Model::new().await;
    let mut fixture = Fixtures::start().await;
    let settings = json!({
        "grants":{"users":["7"]},
        "guilds":{"11":{"rules":{"grants":{"roles":["41"]}},"channels":{"21":{"threads":{"22":{}}}}}},
        "commands":{"sync":false}
    });
    let gateway = model
        .gateway(
            root.path(),
            vec![fixture.configuration_with(root.path(), settings)],
        )
        .await;
    fixture.activate(&gateway).await;
    let mut wire = fixture.wire().await;
    wire.authentication(2).await;
    fixture.ready(&wire, "role-revision").await;
    let mut admitted = message("160", "22", "<@9> authorized by guild role", true);
    admitted["member"]["roles"] = json!(["41"]);
    wire.dispatch(2, "MESSAGE_CREATE", admitted).await;
    let Write::Message { channel, .. } = fixture.reply().await else {
        panic!("expected guild reply")
    };
    assert_eq!(channel, "22");
    wait_reply(&gateway, "discord-message-160").await;
    wire.dispatch(
        3,
        "GUILD_MEMBER_UPDATE",
        json!({"guild_id":"11","user":{"id":"7"},"roles":[]}),
    )
    .await;
    wire.dispatch(
        4,
        "MESSAGE_CREATE",
        message("161", "22", "<@9> former role", true),
    )
    .await;
    wait_status(&gateway, |state| {
        state["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|input| input["id"] == "discord-message-161" && input["state"] == "refused")
    })
    .await;
    wire.dispatch(
        5,
        "MESSAGE_CREATE",
        message("162", "20", "independent direct", false),
    )
    .await;
    let Write::Message {
        channel, payload, ..
    } = fixture.reply().await
    else {
        panic!("expected direct reply")
    };
    assert_eq!(channel, "20");
    assert_eq!(payload["content"], "answer independent direct");
    assert_eq!(model.requests().len(), 2);
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn discord_resume_never_skips_a_dispatch_that_failed_to_commit() {
    let root = tempfile::tempdir().unwrap();
    let model = Model::new().await;
    let mut fixture = Fixtures::start().await;
    let gateway = model
        .gateway(root.path(), vec![fixture.configuration(root.path())])
        .await;
    fixture.activate(&gateway).await;
    let mut first = fixture.wire().await;
    first.authentication(2).await;
    fixture.ready(&first, "failed-commit").await;
    wait_status(&gateway, |state| {
        state["connections"][0]["health"] == "ready"
    })
    .await;
    let db = hermes_engine::db::open(root.path()).unwrap();
    db.execute_batch(
        "CREATE TRIGGER reject_test_dispatch BEFORE INSERT ON messaging_ingress
         WHEN NEW.event = 'discord-message-300'
         BEGIN SELECT RAISE(ABORT, 'fixture commit interruption'); END;",
    )
    .unwrap();
    let event = message("300", "20", "retry after a failed commit", false);
    first.dispatch(2, "MESSAGE_CREATE", event.clone()).await;
    let mut resumed = fixture.wire().await;
    assert_eq!(resumed.authentication(6).await["d"]["seq"], 1);
    assert!(model.requests().is_empty());
    db.execute_batch("DROP TRIGGER reject_test_dispatch;")
        .unwrap();
    resumed.dispatch(2, "MESSAGE_CREATE", event).await;
    resumed.dispatch(3, "RESUMED", json!({})).await;
    let Write::Message { payload, .. } = fixture.reply().await else {
        panic!("expected the recovered reply")
    };
    assert_eq!(payload["content"], "answer retry after a failed commit");
    assert_eq!(model.requests().len(), 1);
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn discord_requester_commands_answer_privately_without_creating_a_turn() {
    let root = tempfile::tempdir().unwrap();
    let model = Model::new().await;
    let mut fixture = Fixtures::start().await;
    let settings = json!({
        "grants":{"users":["7"]},
        "guilds":{
            "11":{"operator_roles":["31"],"channels":{"21":{}}},
            "12":{"channels":{"23":{}}}
        },
        "commands":{"sync":false,"guild_roles":{"11":["31"]}},
    });
    let gateway = model
        .gateway(root.path(), vec![fixture.configuration_with(root.path(), settings)])
        .await;
    fixture.activate(&gateway).await;
    let mut wire = fixture.wire().await;
    wire.authentication(2).await;
    fixture.ready(&wire, "requester").await;
    let interaction = |offset: u64, command: &str, channel: &str, guild: &str, roles: &[&str]| {
        let id = (((now() - 1_420_070_400_000) as u64) << 22) + offset;
        json!({
            "id":id.to_string(), "application_id":"90", "type":2,
            "token":format!("callback-{id}"), "channel_id":channel, "guild_id":guild,
            "member":{"roles":roles,"user":{"id":"8"}}, "data":{"name":command},
        })
    };
    let allowed = interaction(1, "status", "21", "11", &["31"]);
    let event = format!("discord-interaction-{}", allowed["id"].as_str().unwrap());
    wire.dispatch(2, "INTERACTION_CREATE", allowed).await;
    let Write::Callback(response) = fixture.write().await else {
        panic!("expected requester callback")
    };
    assert_eq!(response["type"], 4);
    assert_eq!(response["data"]["flags"], 64);
    assert_eq!(
        response["data"]["content"],
        "Mercury is ready. Operator role verified."
    );
    let mut chat = message("300", "21", "stay outside chat", true);
    chat["author"]["id"] = json!("8");
    chat["member"]["roles"] = json!(["31"]);
    wire.dispatch(3, "MESSAGE_CREATE", chat).await;

    for (position, command, channel, guild, roles) in [
        (4, "queue", "21", "11", &["31"][..]),
        (5, "status", "23", "12", &["31"][..]),
        (6, "status", "21", "11", &[][..]),
    ] {
        let denied = interaction(position, command, channel, guild, roles);
        wire.dispatch(position, "INTERACTION_CREATE", denied).await;
        let Write::Callback(response) = fixture.write().await else {
            panic!("expected private refusal")
        };
        assert_eq!(response["type"], 4);
        assert_eq!(response["data"]["flags"], 64);
        assert!(
            !response["data"]["content"]
                .as_str()
                .unwrap()
                .contains("Mercury is ready")
        );
    }

    let status = gateway.messaging_status().await.unwrap();
    let input = status["input"].as_array().unwrap();
    let invocation = input.iter().find(|entry| entry["id"] == event).unwrap();
    assert_eq!(invocation["state"], "done");
    assert!(invocation["route"].is_null());
    assert!(
        input
            .iter()
            .any(|entry| entry["id"] == "discord-message-300" && entry["state"] == "refused")
    );
    assert!(model.requests().is_empty());
    assert_eq!(status["deliveries"], json!([]));
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn discord_approval_is_bound_to_the_exact_bot_message_and_acks_committed_control() {
    let root = tempfile::tempdir().unwrap();
    let model = Model::new().await;
    let mut fixture = Fixtures::start().await;
    let gateway = model
        .gateway(root.path(), vec![fixture.configuration(root.path())])
        .await;
    fixture.activate(&gateway).await;
    let mut wire = fixture.wire().await;
    wire.authentication(2).await;
    fixture.ready(&wire, "approvals").await;
    wire.dispatch(2, "MESSAGE_CREATE", message("200", "20", "approval", false))
        .await;
    let Write::Message { payload, id, .. } = fixture.next_write("an approval prompt", |write| {
        matches!(write, Write::Message { payload, .. } if payload["components"][0]["components"][0]["custom_id"].is_string())
    }).await else {
        panic!("expected an approval prompt")
    };
    let button = payload["components"][0]["components"][0]["custom_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let action = button.split(':').nth(1).unwrap();
    wait_status(&gateway, |state| {
        state["deliveries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|delivery| delivery["id"] == action && delivery["state"] == "sent")
    })
    .await;
    let id_base = ((now() - 1_420_070_400_000) as u64) << 22;
    let click = |id: u64, message: &str| {
        json!({
            "id":id.to_string(),"application_id":"90","token":format!("callback-{id}"),"type":3,
            "channel_id":"20","user":{"id":"7"},"data":{"custom_id":button},
            "message":{"id":message,"author":{"id":"9","bot":true}}
        })
    };
    wire.dispatch(3, "INTERACTION_CREATE", click(id_base + 1, "888"))
        .await;
    let Write::Callback(rejected) = fixture
        .next_write("a private component refusal", |write| {
            matches!(write, Write::Callback(_))
        })
        .await
    else {
        panic!("expected private refusal")
    };
    assert_eq!(rejected["type"], 4);
    assert_eq!(rejected["data"]["flags"], 64);
    let expired = id_base - (5_000_u64 << 22);
    wire.dispatch(4, "INTERACTION_CREATE", click(expired, &id))
        .await;
    wait_status(&gateway, |state| {
        state["input"].as_array().unwrap().iter().any(|input| {
            input["id"] == format!("discord-interaction-{expired}") && input["state"] == "refused"
        })
    })
    .await;
    assert_eq!(model.requests().len(), 1);
    let event = (((now() - 1_420_070_400_000) as u64) << 22) + 2;
    wire.dispatch(5, "INTERACTION_CREATE", click(event, &id))
        .await;
    let mut acknowledged = false;
    let mut applied = false;
    let mut replied = false;
    while !acknowledged || !applied || !replied {
        match fixture.write().await {
            Write::Callback(value) => {
                assert_eq!(value["type"], 5);
                let status = gateway.messaging_status().await.unwrap();
                assert!(
                    status["input"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|input| input["id"] == format!("discord-interaction-{event}"))
                );
                acknowledged = true;
            }
            Write::Followup(value) => {
                assert_eq!(value["content"], "Applied.");
                applied = true;
            }
            Write::Message {
                channel, payload, ..
            } => {
                assert_eq!(channel, "20");
                if payload["content"]
                    .as_str()
                    .is_some_and(|text| text.starts_with("answer"))
                {
                    replied = true;
                }
            }
        }
    }
    assert_eq!(model.requests().len(), 2);
    gateway.shutdown().await.unwrap();
}
