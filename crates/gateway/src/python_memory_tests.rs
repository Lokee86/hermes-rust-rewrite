use super::*;
use axum::{extract::State, response::IntoResponse};
use tempfile::TempDir;
use tokio::time::{Instant, sleep, timeout};

const FIXTURE_ID: &str = "fixture-memory";
const SYNTHETIC_NOTE_KEY: &str = "sk-synthetic-fixture-zcDx17NEs4TqZ11V";

fn install_provider(root: &Path) {
    let dir = root.join("plugins").join(FIXTURE_ID);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("plugin.yaml"),
        "name: fixture-memory\nversion: 0.0.1\n",
    )
    .unwrap();
    fs::write(dir.join("__init__.py"), r#"
from pathlib import Path
import json
from agent.memory_provider import MemoryProvider

class Provider(MemoryProvider):
    name = "Fixture memory"
    def is_available(self): return True
    def initialize(self, session_id, *, hermes_home, **kwargs):
        self.path = Path(hermes_home) / "memory-fixture.jsonl"
        self.sid = session_id
    def get_tool_schemas(self):
        return [{"name":"remember_fixture","description":"Local memory fixture","parameters":{"type":"object"}}]
    def handle_tool_call(self, tool_name, args, **kwargs):
        return '{"answer":"committed fixture knowledge"}'
    def prefetch(self, query, **kwargs):
        return "fixture memory loaded" if self.path.exists() else ""
    def sync_turn(self, user_content, assistant_content, *, session_id, messages, **kwargs):
        assert self.sid == session_id
        with self.path.open("a", encoding="utf-8") as writer:
            writer.write(json.dumps({"session":session_id,"user":user_content,
                "assistant":assistant_content,"messages":messages}) + "\n")

def register(host):
    host.register_memory_provider(Provider())
"#).unwrap();
    fs::write(
        root.join("plugins").join("installed.json"),
        json!([{"name":FIXTURE_ID,"title":"Fixture memory","version":"0.0.1",
            "description":"","sha":"fixture","enabled":true,"servers":["python"],"skills":[]}])
        .to_string(),
    )
    .unwrap();
}

#[derive(Clone, Default)]
struct ModelFixture {
    requests: Arc<StdMutex<Vec<Value>>>,
    waiting: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

async fn reply(
    State(fixture): State<ModelFixture>,
    Json(request): Json<Value>,
) -> impl IntoResponse {
    lock(&fixture.requests).push(request.clone());
    let latest = request["messages"].as_array().unwrap().last().unwrap();
    let prompt = latest["content"].as_str().unwrap_or_default();
    if prompt == "Pause until the provider changes." {
        fixture.waiting.notify_one();
        fixture.release.notified().await;
    }
    let (delta, finish) = if prompt == "Use the memory tool." {
        (
            json!({"tool_calls":[{"index":0,"id":"check-memory","type":"function","function":{
                "name":"remember_fixture","arguments":"{}"
            }}]}),
            "tool_calls",
        )
    } else {
        (json!({"content":"A committed fixture response."}), "stop")
    };
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"index":0,"delta":delta,"finish_reason":finish}]}),
        ),
    )
}

fn peer() -> (Peer, mpsc::Receiver<Value>) {
    let (tx, rx) = mpsc::channel(256);
    (
        Peer {
            id: new_id(),
            tx,
            stop: CancellationToken::new(),
            canonical: true,
            local_native: true,
        },
        rx,
    )
}

async fn call(owner: &Arc<Shared>, peer: &Peer, method: &str, params: Value) -> Value {
    owner
        .dispatch(method, &params, peer)
        .await
        .unwrap_or_else(|error| panic!("{method}: {}", error.message))
}

async fn create(owner: &Arc<Shared>, peer: &Peer) -> String {
    let response = call(owner, peer, "session.create", json!({"source":"gui"})).await;
    response["session_id"]
        .as_str()
        .or_else(|| response["stored_session_id"].as_str())
        .or_else(|| response["session"]["session_id"].as_str())
        .unwrap_or_else(|| panic!("session identity missing: {response}"))
        .to_owned()
}

async fn send_prompt(owner: &Arc<Shared>, peer: &Peer, sid: &str, text: &str) {
    call(
        owner,
        peer,
        "prompt.submit",
        json!({"session_id":sid,"text":text}),
    )
    .await;
}

async fn completed(events: &mut mpsc::Receiver<Value>) {
    timeout(Duration::from_secs(25), async {
        while let Some(frame) = events.recv().await {
            if frame["params"]["type"] == "message.complete" {
                assert_eq!(frame["params"]["payload"]["status"], "complete", "{frame}");
                return;
            }
        }
        panic!("Gateway closed before completing the turn");
    })
    .await
    .expect("model turn did not complete");
}

fn handoffs(root: &Path) -> Vec<Value> {
    fs::read_to_string(root.join("memory-fixture.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

async fn wait_for_handoffs(root: &Path, count: usize) -> Vec<Value> {
    let limit = Instant::now() + Duration::from_secs(15);
    loop {
        let present = handoffs(root);
        if present.len() >= count {
            return present;
        }
        assert!(
            Instant::now() < limit,
            "only {} of {count} memory handoffs",
            present.len()
        );
        sleep(Duration::from_millis(30)).await;
    }
}

#[tokio::test]
async fn provider_recalls_committed_context_and_switches_without_crossing_revisions() {
    let home = TempDir::new().unwrap();
    install_provider(home.path());
    ProfileSettings::update(
        home.path(),
        json!({"approval_mode":"off","mcp_tools":false,"memory_provider":"off"})
            .as_object()
            .unwrap(),
    )
    .unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let fixture = ModelFixture::default();
    let config = Config {
        base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
        model: "fixture-model".into(),
        api_key: "local-gateway-fixture-secret".into(),
    };
    let router = Router::new()
        .route("/v1/chat/completions", post(reply))
        .with_state(fixture.clone());
    let model = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let gateway = Gateway::start(GatewayConfig::new(home.path(), config.clone()))
        .await
        .unwrap();
    let (owner, mut events) = peer();
    let (mut viewer, _) = peer();
    viewer.local_native = false;
    let error = gateway
        .shared
        .dispatch("memory.provider.set", &json!({"provider":"off"}), &viewer)
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, 4403);
    let choice = format!("plugin:{FIXTURE_ID}");
    let selected = call(
        &gateway.shared,
        &owner,
        "memory.provider.set",
        json!({"provider":choice}),
    )
    .await;
    assert_eq!(selected["effective"], choice);
    let first = create(&gateway.shared, &owner).await;
    send_prompt(
        &gateway.shared,
        &owner,
        &first,
        &format!("A first fact. {SYNTHETIC_NOTE_KEY}"),
    )
    .await;
    completed(&mut events).await;
    let handoff = wait_for_handoffs(home.path(), 1).await.pop().unwrap();
    assert_eq!(handoff["session"], first);
    assert!(!handoff.to_string().contains(SYNTHETIC_NOTE_KEY));
    let messages = handoff["messages"].as_array().unwrap();
    assert!(messages.len() >= 2);
    assert!(
        messages[0]["message_uid"]
            .as_str()
            .unwrap()
            .starts_with("generation-")
    );
    assert!(messages[0]["timestamp"].as_f64().unwrap() > 0.0);

    send_prompt(
        &gateway.shared,
        &owner,
        &first,
        &format!("This uses {}.", config.api_key),
    )
    .await;
    completed(&mut events).await;

    send_prompt(
        &gateway.shared,
        &owner,
        &first,
        "Pause until the provider changes.",
    )
    .await;
    timeout(Duration::from_secs(10), fixture.waiting.notified())
        .await
        .unwrap();
    call(
        &gateway.shared,
        &owner,
        "memory.provider.set",
        json!({"provider":"off"}),
    )
    .await;
    call(
        &gateway.shared,
        &owner,
        "memory.provider.set",
        json!({"provider":choice}),
    )
    .await;
    fixture.release.notify_one();
    completed(&mut events).await;

    send_prompt(&gateway.shared, &owner, &first, "Use the memory tool.").await;
    completed(&mut events).await;
    let delivered = wait_for_handoffs(home.path(), 2).await;
    assert_eq!(delivered.len(), 2);
    assert_eq!(delivered[1]["user"], "Use the memory tool.");
    assert!(!delivered[1].to_string().contains("A first fact"));
    assert!(!delivered[1].to_string().contains("Pause until"));
    assert!(
        lock(&fixture.requests)
            .iter()
            .any(|body| body.to_string().contains("committed fixture knowledge"))
    );

    call(
        &gateway.shared,
        &owner,
        "catalog.session.set",
        json!({"session_id":first,"archived":true}),
    )
    .await;
    call(
        &gateway.shared,
        &owner,
        "catalog.session.set",
        json!({"session_id":first,"archived":false}),
    )
    .await;
    gateway.shutdown().await.unwrap();

    let gateway = Gateway::start(GatewayConfig::new(home.path(), config))
        .await
        .unwrap();
    let (owner, mut events) = peer();
    call(
        &gateway.shared,
        &owner,
        "session.resume",
        json!({"session_id":first}),
    )
    .await;
    send_prompt(
        &gateway.shared,
        &owner,
        &first,
        "Do not capture a restored source.",
    )
    .await;
    completed(&mut events).await;
    let other = create(&gateway.shared, &owner).await;
    send_prompt(&gateway.shared, &owner, &other, "Use the memory tool.").await;
    completed(&mut events).await;
    let delivered = wait_for_handoffs(home.path(), 3).await;
    assert_eq!(delivered.len(), 3);
    assert_eq!(delivered[2]["session"], other);
    assert!(
        lock(&fixture.requests)
            .iter()
            .any(|body| body.to_string().contains("fixture memory loaded"))
    );
    gateway.shutdown().await.unwrap();
    model.abort();
}
