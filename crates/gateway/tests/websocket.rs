use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use futures_util::{SinkExt, StreamExt, stream};
use hermes_engine::Config;
use hermes_gateway::{Gateway, GatewayConfig, RunningGateway};
use serde_json::{Value, json};
use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tempfile::TempDir;
use tokio::{
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::timeout,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Client {
    socket: Socket,
    events: Vec<Value>,
    id: u64,
}
impl Client {
    async fn native(gateway: &RunningGateway) -> Self {
        Self::native_at(&gateway.http_url(), &gateway.websocket_url(), gateway.token()).await
    }

    async fn native_at(origin: &str, websocket: &str, token: &str) -> Self {
        let http = reqwest::Client::new();
        let ticket: Value = http
            .post(format!("{origin}/api/auth/ws-ticket"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let mut request = format!("{websocket}?native_dial=1")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "sec-websocket-protocol",
            HeaderValue::from_str(&format!(
                "hermes-gateway-v1, hermes-gateway-ticket.{}",
                ticket["ticket"].as_str().unwrap()
            ))
            .unwrap(),
        );
        let (socket, response) = connect_async(request).await.unwrap();
        assert_eq!(
            response.headers()["sec-websocket-protocol"],
            "hermes-gateway-v1"
        );
        let mut client = Self {
            socket,
            events: vec![],
            id: 0,
        };
        let ready = client.frame("native gateway.ready", 5).await;
        assert_eq!(ready["params"]["type"], "gateway.ready");
        assert_eq!(ready["params"]["payload"]["session_authority"], true);
        client
    }
    async fn legacy(gateway: &RunningGateway) -> Self {
        let (socket, _) = connect_async(format!(
            "{}?token={}",
            gateway.websocket_url(),
            gateway.token()
        ))
        .await
        .unwrap();
        let mut client = Self {
            socket,
            events: vec![],
            id: 0,
        };
        assert_eq!(
            client.frame("legacy gateway.ready", 5).await["params"]["payload"]["session_authority"],
            false
        );
        client
    }
    async fn frame(&mut self, waiting_for: &str, seconds: u64) -> Value {
        loop {
            let frame = timeout(Duration::from_secs(seconds), self.socket.next())
                .await
                .unwrap_or_else(|_| {
                    let observed: Vec<_> = self.events.iter().rev().take(8)
                        .map(|event| (&event["type"], if event["type"] == "error" {
                            &event["payload"]
                        } else {
                            &event["payload"]["status"]
                        })).collect();
                    panic!("gateway timed out waiting for {waiting_for} after RPC {}. Recent events: {observed:?}", self.id)
                })
                .expect("gateway closed")
                .unwrap();
            if let Message::Text(text) = frame {
                return serde_json::from_str(&text).unwrap();
            }
        }
    }
    async fn call(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let id = self.id;
        self.socket
            .send(Message::Text(
                json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        loop {
            let frame = self.frame(method, 5).await;
            if frame["id"] == id {
                return frame;
            }
            self.events.push(frame["params"].clone());
        }
    }
    async fn ok(&mut self, method: &str, params: Value) -> Value {
        let frame = self.call(method, params).await;
        assert!(frame.get("error").is_none(), "{method}: {frame}");
        frame["result"].clone()
    }
    async fn event(&mut self, name: &str) -> Value {
        self.event_with_timeout(name, 5).await
    }
    async fn event_with_timeout(&mut self, name: &str, seconds: u64) -> Value {
        if let Some(index) = self.events.iter().position(|e| e["type"] == name) {
            return self.events.remove(index);
        }
        loop {
            let frame = self.frame(name, seconds).await;
            assert_eq!(frame["method"], "event", "unexpected frame: {frame}");
            let event = frame["params"].clone();
            if event["type"] == name {
                return event;
            }
            self.events.push(event);
        }
    }
}
struct Provider {
    address: SocketAddr,
    memory_requests: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Provider {
    async fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let memory_requests = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&memory_requests);
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/v1/chat/completions", post(model))
                    .route("/v1/embeddings", post(embed_memory_fixture))
                    .with_state(observed),
            )
            .await
            .unwrap();
        });
        Self { address, memory_requests, task }
    }
    fn config(&self) -> Config {
        Config {
            base_url: format!("http://{}/v1", self.address),
            model: "fixture-model".into(),
            api_key: String::new(),
        }
    }
    async fn gateway(&self, root: &TempDir) -> RunningGateway {
        // These tests drive the approval prompt, so they pin the Manual approval mode.
        let mut changes = serde_json::Map::new();
        changes.insert("approval_mode".into(), serde_json::json!("manual"));
        hermes_engine::settings::ProfileSettings::update(root.path(), &changes).unwrap();
        Gateway::start(GatewayConfig::new(root.path(), self.config()))
            .await
            .unwrap()
    }
}
fn event(delta: Value, finish: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        json!({"choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    )
}
async fn model(
    State(memory_requests): State<Arc<AtomicUsize>>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    if let Some(schema) = request["response_format"].get("json_schema") {
        assert_hermes_inference(&headers);
        let route = match schema["name"].as_str().unwrap() {
            "insomnia_authority_disposition_ledger" | "insomnia_memory_wording" => "insomnia",
            "insomnia_memory_ownership" => "metadata",
            "decision_response" => "recall",
            _ => "general",
        };
        assert_memory_route(&headers, &request, route);
        memory_requests.fetch_add(1, Ordering::Relaxed);
        return Json(structured_memory_fixture(&request, schema)).into_response();
    }
    let messages = request["messages"].as_array().unwrap();
    let last = messages.last().unwrap();
    let send = |delta, finish| {
        ([(header::CONTENT_TYPE, "text/event-stream")],
            format!("{}data: [DONE]\n\n", event(delta, Some(finish)))).into_response()
    };
    let tool = |id: &str, name: &str, args: Value| json!({"index":0,"id":id,"function":{
        "name":name,"arguments":args.to_string()
    }});
    if last["role"] == "user" && last["content"] == PROJECT_FACT {
        return send(json!({"content":"Checking the Amethyst Lantern clock.",
            "tool_calls":[tool("archive_fixture_clock", "get_current_time", json!({}))]}), "tool_calls");
    }
    if last["tool_call_id"] == "archive_fixture_clock" {
        return send(json!({"content":"The Amethyst Lantern clock check is complete."}), "stop");
    }
    if last["role"] == "user" && last["content"] == "Inspect the archived clock check." {
        assert!(request["tools"].as_array().unwrap().iter().any(|tool| tool["function"]["name"] == "archive_search"));
        assert!(!request["tools"].as_array().unwrap().iter().any(|tool| tool["function"]["name"] == "echo_read"));
        return send(json!({"tool_calls":[tool("unfiltered_archive", "archive_search", json!({"query":"Amethyst Lantern"}))]}), "tool_calls");
    }
    if last["tool_call_id"] == "unfiltered_archive" {
        let reply: Value = serde_json::from_str(last["content"].as_str().unwrap()).unwrap();
        assert_eq!(reply["error"], "temporal_query_required");
        return send(json!({"tool_calls":[tool("dated_archive", "archive_search", json!({"query":"Amethyst Lantern today"}))]}), "tool_calls");
    }
    if last["tool_call_id"] == "dated_archive" {
        let reply: Value = serde_json::from_str(last["content"].as_str().unwrap()).unwrap();
        let hits = reply["hits"].as_array().expect("expected dated Archive results");
        let hit = hits.iter().find(|hit| {
            hit["messages"].to_string().contains("Checking the Amethyst Lantern clock.")
        }).unwrap_or_else(|| panic!("missing archived commentary in {reply}"));
        assert!(hits.iter().any(|hit| hit["messages"].to_string().contains("The Amethyst Lantern clock check is complete.")), "missing archived final reply in {reply}");
        assert!(request["tools"].as_array().unwrap().iter().any(|tool| tool["function"]["name"] == "echo_read"));
        return send(json!({"tool_calls":[tool("exact_echo", "echo_read", json!({"reference":hit["reference"]}))]}), "tool_calls");
    }
    if last["tool_call_id"] == "exact_echo" {
        let reply: Value = serde_json::from_str(last["content"].as_str().unwrap()).unwrap();
        assert!(reply["echo"].to_string().contains("get_current_time"));
        return send(json!({"content":"Recovered the Archive and Echo."}), "stop");
    }
    if last["content"] == CONTEXT_PROBE {
        // Observe at the chat request. Background capture of this turn starts after it commits.
        let has_tool = |name: &str| request["tools"].as_array().unwrap().iter()
            .any(|tool| tool["function"]["name"] == name);
        let report = json!({
            "memory_requests": memory_requests.load(Ordering::Relaxed),
            "included_fact": messages.iter().any(|message| message.to_string().contains(PROJECT_FACT)),
            "notes_visible": messages[0]["content"].as_str().unwrap().contains(PROFILE_NOTE),
            "notes_tool": has_tool("memory"),
            "query_tool": has_tool("memory_query"),
        });
        return ([(header::CONTENT_TYPE, "text/event-stream")],
                format!("{}data: [DONE]\n\n", event(json!({"content":report.to_string()}), Some("stop")))).into_response();
    }
    if last["role"] == "user" && last["content"] == REQUEST_QUERY {
        assert!(request["tools"].as_array().unwrap().iter().any(|tool| tool["function"]["name"] == "memory_query"));
        assert!(!messages.iter().any(|message| message.to_string().contains(PROJECT_FACT)));
        let call = json!({"index":0, "id":"explicit_memory_query", "function":{
            "name":"memory_query", "arguments":json!({"query":"Amethyst Lantern"}).to_string()
        }});
        let body = event(json!({"tool_calls":[call]}), Some("tool_calls"));
        return ([(header::CONTENT_TYPE, "text/event-stream")],
                format!("{body}data: [DONE]\n\n")).into_response();
    }
    if last["role"] == "tool" && last["tool_call_id"] == "explicit_memory_query" {
        let has_fact = |message: &Value| message.to_string().contains(PROJECT_FACT);
        assert!(!messages[..messages.len() - 1].iter().any(has_fact));
        let reply = if has_fact(last) { "Project memory found." } else { "No project memory." };
        let body = event(json!({"content":reply}), Some("stop"));
        return ([(header::CONTENT_TYPE, "text/event-stream")],
                format!("{body}data: [DONE]\n\n")).into_response();
    }
    if last["content"] == "provider-error" {
        return (
            [(header::CONTENT_TYPE, "text/event-stream")],
            "data: {\"error\":{\"message\":\"fixture rejected\"}}\n\n",
        )
            .into_response();
    }
    let system = messages.first().and_then(|m| m["content"].as_str()).unwrap_or_default();
    if system.contains("You are Ada, a named Hermes agent") && last["role"] == "user" && last["content"].as_str().is_some_and(|content| content.contains("group-job")) {
        let body = event(json!({"tool_calls":[{"index":0,"id":"group_job","function":{"name":"terminal","arguments":json!({"command":"read -r line","background":true}).to_string()}}]}), Some("tool_calls"));
        return ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response();
    }
    if system.contains("You are Ada, a named Hermes agent") && last["content"].as_str().is_some_and(|content| content.contains("hold-group")) {
        let body = stream::once(async move { Ok::<_, Infallible>(Bytes::from(event(json!({"content":"Waiting"}), None))) }).chain(stream::pending());
        return ([(header::CONTENT_TYPE, "text/event-stream")], Body::from_stream(body)).into_response();
    }
    if system.contains("You are Ada, a named Hermes agent") && last["content"].as_str().is_some_and(|content| content.contains("fail-group")) {
        return ([(header::CONTENT_TYPE, "text/event-stream")], "data: {\"error\":{\"message\":\"group fixture failed\"}}\n\n").into_response();
    }
    if system.contains("You are Ada, a named Hermes agent") || system.contains("You are Grace, a named Hermes agent") {
        let reply = if system.contains("You are Ada, a named Hermes agent") { "Ada here" }
            else if messages.iter().any(|message| message["content"].as_str().is_some_and(|content| content.contains("Ada here"))) { "Grace saw Ada" }
            else { "Grace missed Ada" };
        return ([(header::CONTENT_TYPE, "text/event-stream")], event(json!({"content":reply}), Some("stop"))).into_response();
    }
    if last["content"] == "slow" {
        let text = event(json!({"content":"Waiting"}), None);
        let body = stream::once(async move { Ok::<_, Infallible>(Bytes::from(text)) })
            .chain(stream::pending());
        return (
            [(header::CONTENT_TYPE, "text/event-stream")],
            Body::from_stream(body),
        )
            .into_response();
    }
    if last["role"] == "tool" && messages.iter().any(|message| message["role"] == "user" && (message["content"] == "abandon fixture" || message["content"] == "persist fixture")) {
        let body = stream::once(async move { Ok::<_, Infallible>(Bytes::from(event(json!({"content":"Waiting"}), None))) }).chain(stream::pending());
        return ([(header::CONTENT_TYPE, "text/event-stream")], Body::from_stream(body)).into_response();
    }
    let body = if last["role"] == "tool" {
        let content = last["content"].as_str().unwrap();
        let reply = if content.contains("denied") {
            "Tool denied."
        } else {
            "Clock read successfully."
        };
        event(json!({"content":reply}), Some("stop"))
    } else if last["content"] == "persist fixture" {
        event(json!({"tool_calls":[{"index":0,"id":"persistent_fixture","function":{
            "name":"terminal","arguments":json!({"command":"read -r line","background":true,"persist_on_release":true}).to_string()
        }}]}),Some("tool_calls"))
    } else if last["content"] == "run background fixture" || last["content"] == "abandon fixture" {
        event(json!({"tool_calls":[{"index":0,"id":"background_fixture","function":{
            "name":"terminal","arguments":json!({"command":"printf 'background-ready'; read -r line","background":true}).to_string()
        }}]}),Some("tool_calls"))
    } else if last["content"] == "run pipe fixture" {
        assert!(request["tools"].as_array().unwrap().iter().any(|s|s["function"]["name"]=="terminal"));
        event(json!({"tool_calls":[{"index":0,"id":"fixture_terminal_call","function":{
            "name":"terminal","arguments":json!({"command":"printf approved > process-proof.txt; printf pipe-probe"}).to_string()
        }}]}),Some("tool_calls"))
    } else if last["content"]
        .as_str()
        .is_some_and(|s| s.contains("clock"))
    {
        format!(
            "{}{}",
            event(json!({"content":"The clock needs approval."}), None),
            event(
                json!({"tool_calls":[{"index":0,"id":"clock_call","function":{"name":"get_current_time","arguments":"{}"}}]}),
                Some("tool_calls")
            )
        )
    } else {
        format!(
            "{}{}",
            event(json!({"content":"Hello 😀 "}), None),
            event(json!({"content":"world"}), Some("stop"))
        )
    };
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        format!("{body}data: [DONE]\n\n"),
    )
        .into_response()
}

const PROJECT_FACT: &str = "The workspace project is Amethyst Lantern 741.";
const REQUEST_QUERY: &str = "Look up the project name in memory.";
const PROFILE_NOTE: &str = "The notes canary is violet.";
const CONTEXT_PROBE: &str = "How is the Amethyst Lantern project going?";

fn context_report(completed: &Value) -> Value {
    assert_eq!(completed["payload"]["status"], "complete", "{completed}");
    serde_json::from_str(completed["payload"]["text"].as_str().unwrap())
        .unwrap_or_else(|error| panic!("invalid context probe response: {error}: {completed}"))
}

fn assert_hermes_inference(headers: &HeaderMap) {
    let user_agent = headers.get(header::USER_AGENT).and_then(|value| value.to_str().ok());
    assert!(user_agent.is_some_and(|value| value.starts_with("HermesRust/")),
        "expected the Hermes provider transport");
}

fn assert_memory_route(headers: &HeaderMap, request: &Value, route: &str) {
    assert_eq!(request["model"], format!("fixture-{route}"));
    let auth = headers.get(header::AUTHORIZATION).unwrap().to_str().unwrap();
    assert_eq!(auth, format!("Bearer fixture-{route}-key"));
}

async fn embed_memory_fixture(
    State(memory_requests): State<Arc<AtomicUsize>>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Json<Value> {
    assert_hermes_inference(&headers);
    assert_memory_route(&headers, &request, "embedding");
    memory_requests.fetch_add(1, Ordering::Relaxed);
    let inputs = request["input"].as_array().unwrap();
    let data = (0..inputs.len()).map(|index| {
        json!({"index": index, "embedding": [3.0, 4.0]})
    }).collect::<Vec<_>>();
    Json(json!({"model": request["model"], "data": data}))
}

fn structured_memory_fixture(request: &Value, schema: &Value) -> Value {
    let shape = &schema["schema"]["properties"];
    let value = match schema["name"].as_str().unwrap() {
        "insomnia_authority_disposition_ledger" => {
            let episode: Value = serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
            let mut turns = serde_json::Map::new();
            for id in shape["turns"]["required"].as_array().unwrap() {
                let id = id.as_str().unwrap();
                let source = episode["turns"].as_array().unwrap().iter().find(|turn| turn["id"] == id).unwrap();
                let clauses = if source["role"] == "user" && source["content"] == PROJECT_FACT {
                    json!([{"disposition":"retain", "authority_kind":"direct", "category":"fact",
                        "type":"project", "lifecycle":"current", "proposition":PROJECT_FACT,
                        "authority_source_node_id":"", "grounding_source_node_id":"",
                        "reason":"The user gave the project name."}])
                } else {
                    json!([{"disposition":"omit", "authority_kind":"none", "category":"none",
                        "type":"none", "lifecycle":"none", "proposition":"",
                        "authority_source_node_id":"", "grounding_source_node_id":"",
                        "reason":"This turn contains no durable project fact."}])
                };
                turns.insert(id.into(), clauses);
            }
            json!({"turns": turns, "evidence_requests":[]})
        }
        "insomnia_memory_ownership" => {
            let ownership: serde_json::Map<String, Value> = shape["ownership"]["required"]
                .as_array().unwrap().iter()
                .map(|id| (id.as_str().unwrap().into(), json!("project"))).collect();
            json!({"ownership": ownership})
        }
        "insomnia_memory_wording" => {
            let candidates = shape["candidates"]["items"]["properties"]["source_node_id"]["enum"]
                .as_array().unwrap().iter().filter(|id| id.as_str().unwrap().ends_with("-user"))
                .map(|id| json!({
                    "authority_kind":"direct", "category":"fact", "memory_type":"project",
                    "temporal_status":"current", "ownership":"project", "title":"Amethyst Lantern",
                    "content":PROJECT_FACT, "source_node_id":id, "source_quote":PROJECT_FACT,
                    "authority_source":null, "grounding_source":null, "routing_mentions":[]
                })).collect::<Vec<_>>();
            json!({"candidates":candidates})
        }
        "decision_response" => {
            let payload: Value = serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
            let approved = payload["state"]["candidates"].as_array().into_iter().flatten()
                .filter(|candidate| candidate["content"].as_str().is_some_and(|text| text.contains(PROJECT_FACT)))
                .map(|candidate| candidate["memory"].clone()).collect::<Vec<_>>();
            json!({"version":1,"approved":approved})
        }
        "dream_pair_classification" => json!({"relation":"none","direction":"none","evidence":[]}),
        _ => serde_json::Value::Object(serde_json::Map::new()),
    };
    json!({"choices":[{"message":{"content":value.to_string()}}]})
}

#[cfg(feature = "reliquary")]
async fn memory_capture_receipt(
    record_path: &std::path::Path,
    index: usize,
) -> hermes_engine::memory_capture::CaptureReceipt {
    timeout(Duration::from_secs(40), async {
        loop {
            let record: Value = serde_json::from_slice(&std::fs::read(record_path).unwrap()).unwrap();
            let marker = &record["authority"]["receipts"][index]["capture"];
            if marker["settled"] == true {
                break serde_json::from_value(marker["receipt"].clone()).unwrap();
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }).await.unwrap_or_else(|_| {
        let record: Value = serde_json::from_slice(&std::fs::read(record_path).unwrap()).unwrap();
        panic!("{} receipt {index} did not settle: {}", record_path.display(), record["authority"]["receipts"][index]["capture"]);
    })
}

#[cfg(feature = "reliquary")]
async fn approved_memory_query(client: &mut Client, sid: &str) -> String {
    client.ok("prompt.submit", json!({"session_id":sid,"submission_id":"query","text":REQUEST_QUERY})).await;
    let request = client.event("approval.request").await;
    assert_eq!(request["payload"]["tool_name"], "memory_query");
    client.ok("approval.respond", json!({
        "session_id":sid, "prompt_id":request["payload"]["prompt_id"],
        "execution_generation":request["execution_generation"], "choice":"once"
    })).await;
    let completed = client.event_with_timeout("message.complete", 65).await;
    assert_eq!(completed["payload"]["status"], "complete");
    completed["payload"]["text"].as_str().unwrap().to_owned()
}

#[cfg(feature = "reliquary")]
async fn approve_fixture_tool(client: &mut Client, sid: &str, name: &str) {
    let request = client.event("approval.request").await;
    assert_eq!(request["payload"]["tool_name"], name);
    client.ok("approval.respond", json!({
        "session_id":sid, "prompt_id":request["payload"]["prompt_id"],
        "execution_generation":request["execution_generation"], "choice":"once"
    })).await;
}

#[cfg(feature = "reliquary")]
struct PortableGateway {
    process: tokio::process::Child,
    endpoint: Value,
    home: std::path::PathBuf,
}

#[cfg(feature = "reliquary")]
impl PortableGateway {
    const TOKEN: &str = "stock-plugin-test-token-for-local-gateway";

    async fn start(binary: &std::path::Path, home: &std::path::Path, config: &Config) -> Self {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut process = tokio::process::Command::new(binary)
            .args(["gateway", "run", "--port", "0", "--data-dir"]).arg(home)
            .env("HERMES_RUST_BASE_URL", &config.base_url)
            .env("HERMES_RUST_MODEL", &config.model)
            .env("HERMES_RUST_API_KEY", &config.api_key)
            .env("HERMES_RUST_GATEWAY_TOKEN", Self::TOKEN)
            .env_remove("APPIMAGE")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn().unwrap();
        let mut line = String::new();
        timeout(Duration::from_secs(10), BufReader::new(process.stdout.take().unwrap()).read_line(&mut line))
            .await.unwrap().unwrap();
        Self { process, endpoint: serde_json::from_str(&line).unwrap(), home: home.to_owned() }
    }

    async fn client(&self) -> Client {
        Client::native_at(
            self.endpoint["http_url"].as_str().unwrap(),
            self.endpoint["websocket_url"].as_str().unwrap(),
            Self::TOKEN,
        ).await
    }

    async fn private_request(&self, method: &str, params: Value) -> Value {
        use hermes_gateway::{client::{ClientOptions, GatewayClient}, owner};
        let control = owner::discover(&self.home).await.unwrap().unwrap();
        let client = GatewayClient::new(ClientOptions::default());
        control.connect(&client).await.unwrap();
        let result = client.request(method, params).await.unwrap();
        client.close();
        result
    }

    async fn reconcile_memory(&self) -> Value {
        self.private_request("plugins.memory.sync", json!({})).await
    }

    async fn shutdown(mut self) {
        hermes_gateway::owner::discover(&self.home).await.unwrap().unwrap().stop().await.unwrap();
        assert!(self.process.wait().await.unwrap().success());
    }
}

#[cfg(feature = "reliquary")]
#[tokio::test]
async fn stock_reliquary_enrolls_renames_and_recalls_only_on_request() {
    use hermes_engine::{credentials::EndpointStore, settings::ProfileSettings};
    use hermes_library::Library;
    use hermes_reliquary_plugin::{
        EmbeddingNormalization, HostedEmbeddingSettings, HostedModelRoute, ModelSlot,
        save_switchboard_config, switchboard_config,
    };
    use std::{fs, path::Path};

    let provider = Provider::start().await;
    let memory_provider = Provider::start().await;
    let sandbox = TempDir::new().unwrap();
    let install = sandbox.path().join("install");
    fs::create_dir(&install).unwrap();
    let compiled = Path::new(env!("CARGO_BIN_EXE_hermes-gateway"));
    let binary = install.join(compiled.file_name().unwrap());
    fs::copy(compiled, &binary).unwrap();

    let profiles = Library::open(sandbox.path().join("data")).unwrap();
    let profile = profiles.create_profile("work").unwrap();
    let library = Library::open(&profile.path).unwrap();
    let notes = library.root().join("memories");
    fs::create_dir(&notes).unwrap();
    fs::write(notes.join("MEMORY.md"), PROFILE_NOTE).unwrap();
    let workspace = sandbox.path().join("Lantern");
    fs::create_dir(&workspace).unwrap();
    let project = library.add_project(&workspace).unwrap();
    let old = library.create_session(Some(&project.id), Some("Before enrollment")).unwrap();

    let disabled = json!({"memory_provider":"off"});
    ProfileSettings::update(library.root(), disabled.as_object().unwrap()).unwrap();
    let gateway = PortableGateway::start(&binary, library.root(), &provider.config()).await;
    assert_eq!(gateway.reconcile_memory().await["enrolled"], false);
    let store = profiles.root().join("plugin-data").join("reliquary_store");
    let state = store.join("work");
    let select = |provider: &str| {
        let settings = json!({"memory_provider":provider, "approval_mode":"manual"});
        ProfileSettings::update(library.root(), settings.as_object().unwrap()).unwrap();
    };
    // Automatic keeps an unenrolled profile on its Markdown notes and creates nothing.
    select("auto");
    assert_eq!(gateway.reconcile_memory().await["enrolled"], false);
    assert!(!store.exists());
    let mut unenrolled = gateway.client().await;
    let chat = unenrolled.ok("session.create", json!({"title":"Unenrolled"})).await;
    unenrolled.ok("prompt.submit", json!({"session_id":chat["session_id"],"submission_id":"notes","text":CONTEXT_PROBE})).await;
    let report = context_report(&unenrolled.event("message.complete").await);
    assert_eq!(report["notes_visible"], true);
    assert_eq!(report["query_tool"], false);
    drop(unenrolled);
    select("reliquary");
    assert_eq!(gateway.reconcile_memory().await["enrolled"], true);
    assert!(!install.join("reliquary").exists());
    // An existing enrollment stays in use under Automatic.
    select("auto");
    assert_eq!(gateway.reconcile_memory().await["enrolled"], true);
    assert!(state.join("work.phy").is_file());
    assert!(state.join("general.rel").is_file());
    assert!(state.join("projects").join("Lantern.rel").is_file());
    let read_topology = |state: &Path| -> Value {
        serde_json::from_slice(&fs::read(state.join("topology-v1.json")).unwrap()).unwrap()
    };
    let topology = read_topology(&state);
    let rel_id = topology["projects"][&project.id]["rel_id"].clone();
    let general_id = topology["main_rel"].clone();
    let new_workspace = sandbox.path().join("Second project");
    fs::create_dir(&new_workspace).unwrap();
    library.add_project(&new_workspace).unwrap();
    assert_eq!(gateway.reconcile_memory().await["projects_ready"], true);
    assert!(state.join("projects").join("Second project.rel").is_file());

    let mut client = gateway.client().await;
    client.ok("session.resume", json!({"session_id":old.id})).await;
    client.ok("prompt.submit", json!({"session_id":old.id,"submission_id":"preexisting","text":"A historical chat"})).await;
    assert_eq!(client.event("message.complete").await["payload"]["status"], "complete");
    let old_record: Value = serde_json::from_slice(&fs::read(old.directory.join("conversation.json")).unwrap()).unwrap();
    assert!(old_record["authority"]["receipts"][0]["capture"].is_null());

    let source = client.ok("session.create", json!({"project_id":project.id,"title":"Memory source"})).await;
    let source_id = source["session_id"].as_str().unwrap();
    let record_path = library.session(source_id).unwrap().directory.join("conversation.json");
    let submit = json!({"session_id":source_id,"submission_id":"source-fact","text":PROJECT_FACT});
    let admission = client.ok("prompt.submit", submit.clone()).await;
    approve_fixture_tool(&mut client, source_id, "get_current_time").await;
    assert_eq!(client.event("message.complete").await["payload"]["status"], "complete");
    let committed: Value = serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
    assert_eq!(committed["authority"]["receipts"][0]["status"], "terminal");
    assert_eq!(committed["authority"]["receipts"][0]["capture"]["admission_id"], admission["admission_id"]);
    assert_eq!(committed["authority"]["receipts"][0]["capture"]["binding"]["rel_id"], rel_id);

    // Capture works before a model route is saved. Processing resumes after configuration.
    assert_eq!(provider.memory_requests.load(Ordering::Relaxed), 0);
    let vault = EndpointStore::open(&library.root().join("providers")).unwrap();
    let route = |name: &str, embedding| {
        let endpoint = vault
            .save(Some(name), name, &Config {
                model: format!("saved-{name}-default"),
                api_key: format!("fixture-{name}-key"),
                ..memory_provider.config()
            })
            .unwrap();
        Some(HostedModelRoute {
            endpoint: format!("endpoint:{}", endpoint.id),
            model: Some(format!("fixture-{name}")),
            reasoning_effort: None,
            embedding,
        })
    };
    let general = route("general", None);
    save_switchboard_config(
        library.root(),
        &[
            (ModelSlot::General, general.clone()),
            (ModelSlot::EntityResolution, general),
            (ModelSlot::Insomnia, route("insomnia", None)),
            (ModelSlot::InsomniaMetadata, route("metadata", None)),
            (ModelSlot::MemoryRetrievalDecision, route("recall", None)),
            (ModelSlot::Embedding, route("embedding", Some(HostedEmbeddingSettings {
                dimensions: 2,
                normalization: EmbeddingNormalization::L2,
            }))),
        ],
    ).unwrap();
    let accepted = memory_capture_receipt(&record_path, 0).await;
    assert_eq!(provider.memory_requests.load(Ordering::Relaxed), 0);
    assert!(memory_provider.memory_requests.load(Ordering::Relaxed) > 0);

    library.rename_project(&project.id, "Amethyst").unwrap();
    assert_eq!(gateway.reconcile_memory().await["projects_ready"], true);
    assert!(!state.join("projects").join("Lantern.rel").exists());
    assert!(state.join("projects").join("Amethyst.rel").is_file());
    assert_eq!(read_topology(&state)["projects"][&project.id]["rel_id"], rel_id);
    assert!(hermes_gateway::stock_plugins::rename_profile(&profiles, "work", "office").is_err());
    drop(client);
    gateway.shutdown().await;

    assert_eq!(hermes_gateway::stock_plugins::rename_profile(&profiles, "work", "office").unwrap(), None);
    assert!(!state.exists());
    let state = store.join("office");
    assert!(state.join("office.phy").is_file());
    assert!(!state.join("work.phy").exists());
    let relocated = read_topology(&state);
    for field in ["profile_id", "principal_id", "main_rel", "active_phy"] {
        assert_eq!(relocated[field], topology[field], "{field} changed during rename");
    }
    assert_eq!(relocated["projects"][&project.id]["rel_id"], rel_id);
    assert!(state.join("projects").join("Second project.rel").is_file());

    let library = Library::open(profiles.profile_path("office").unwrap()).unwrap();
    assert_eq!(
        switchboard_config(library.root()).unwrap().model(ModelSlot::Insomnia).unwrap().endpoint,
        "endpoint:insomnia",
    );
    let record_path = library.session(source_id).unwrap().directory.join("conversation.json");
    let restarted = PortableGateway::start(&binary, library.root(), &provider.config()).await;
    let mut next = restarted.client().await;
    next.ok("session.resume", json!({"session_id":source_id})).await;
    assert_eq!(next.ok("prompt.submit", submit).await["admission_id"], admission["admission_id"]);
    let record: Value = serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
    assert_eq!(record["authority"]["receipts"].as_array().unwrap().len(), 1);
    assert_eq!(record["authority"]["receipts"][0]["capture"]["receipt"]["import_id"], accepted.import_id);

    let public_archive = next.call("plugins.memory.archive_search", json!({
        "session_id":source_id,"query":"Amethyst Lantern"
    })).await;
    assert_eq!(public_archive["error"]["data"]["reason"], "permission_denied");
    let project_archive = restarted.private_request("plugins.memory.archive_search", json!({
        "session_id":source_id,"query":"Amethyst Lantern"
    })).await;
    assert!(project_archive["hits"].as_array().unwrap().iter().any(|hit| {
        hit["rel"] == "Amethyst" && hit["text"].as_str().unwrap().contains("Amethyst Lantern")
    }), "project Archive results: {project_archive}");

    let investigation = next.ok("session.create", json!({"project_id":project.id,"title":"Archive investigation"})).await;
    let investigation = investigation["session_id"].as_str().unwrap();
    next.ok("prompt.submit", json!({
        "session_id":investigation,"submission_id":"investigate","text":"Inspect the archived clock check."
    })).await;
    for tool in ["archive_search", "archive_search", "echo_read"] {
        approve_fixture_tool(&mut next, investigation, tool).await;
    }
    let completed = next.event_with_timeout("message.complete", 65).await;
    assert_eq!(completed["payload"]["text"], "Recovered the Archive and Echo.");
    let investigation_record = library.session(investigation).unwrap().directory.join("conversation.json");
    memory_capture_receipt(&investigation_record, 0).await;

    let related = next.ok("session.create", json!({"project_id":project.id,"title":"Same project"})).await;
    let related_id = related["session_id"].as_str().unwrap();
    let related_record = library.session(related_id).unwrap().directory.join("conversation.json");
    let read_related = || -> Value { serde_json::from_slice(&fs::read(&related_record).unwrap()).unwrap() };
    let set_provider = |name: &str| {
        let settings = json!({"memory_provider": name});
        ProfileSettings::update(library.root(), settings.as_object().unwrap()).unwrap();
    };
    provider.memory_requests.store(0, Ordering::Relaxed);
    next.ok("prompt.submit", json!({"session_id":related_id,"submission_id":"ordinary","text":CONTEXT_PROBE})).await;
    let completed = next.event("message.complete").await;
    let report = context_report(&completed);
    assert_eq!(report, json!({"memory_requests":0,"included_fact":false,
        "notes_visible":false,"notes_tool":false,"query_tool":true}));
    memory_capture_receipt(&related_record, 0).await;

    assert_eq!(approved_memory_query(&mut next, related_id).await, "Project memory found.");
    let record = read_related();
    assert!(!record.to_string().contains(PROJECT_FACT));
    let entry = record["turns"][1]["entries"].as_array().unwrap().iter()
        .find(|entry| entry["role"] == "tool" && entry["name"] == "memory_query").unwrap();
    assert_eq!(entry["approved"], true);
    let stored: Value = serde_json::from_str(entry["result"].as_str().unwrap()).unwrap();
    assert_eq!(stored.as_object().unwrap().len(), 1);
    assert!(stored["returned"].as_u64().unwrap() > 0);

    set_provider("off");
    next.ok("prompt.submit", json!({"session_id":related_id,"submission_id":"disabled","text":CONTEXT_PROBE})).await;
    let completed = next.event("message.complete").await;
    let report = context_report(&completed);
    for flag in ["query_tool", "notes_tool", "notes_visible", "included_fact"] {
        assert_eq!(report[flag], false, "{flag} while memory is disabled");
    }
    assert!(read_related()["authority"]["receipts"][2]["capture"].is_null());

    set_provider("reliquary");
    next.ok("prompt.submit", json!({"session_id":related_id,"submission_id":"reenabled","text":CONTEXT_PROBE})).await;
    let completed = next.event("message.complete").await;
    let report = context_report(&completed);
    assert_eq!(report["query_tool"], true);
    assert_eq!(report["included_fact"], false);
    memory_capture_receipt(&related_record, 3).await;
    assert!(read_related()["authority"]["receipts"][2]["capture"].is_null());

    let general_chat = next.ok("session.create", json!({"title":"General chat"})).await;
    let general_chat_id = general_chat["session_id"].as_str().unwrap();
    let local_archive = restarted.private_request("plugins.memory.archive_search", json!({
        "session_id":general_chat_id,"query":"Amethyst Lantern"
    })).await;
    assert_eq!(local_archive["hits"], json!([]));
    let related_archive = restarted.private_request("plugins.memory.archive_search", json!({
        "session_id":general_chat_id,"query":"Amethyst Lantern","include_related":true
    })).await;
    assert!(related_archive["hits"].as_array().unwrap().iter().any(|hit| hit["rel"] == "Amethyst"),
        "related Archive results: {related_archive}");
    assert_eq!(approved_memory_query(&mut next, general_chat_id).await, "No project memory.");
    let general_record = library.session(general_chat_id).unwrap().directory.join("conversation.json");
    let record: Value = serde_json::from_slice(&fs::read(general_record).unwrap()).unwrap();
    assert!(!record.to_string().contains(PROJECT_FACT));
    assert_eq!(record["authority"]["receipts"][0]["capture"]["binding"]["rel_id"], general_id);
    drop(next);
    restarted.shutdown().await;
}

#[tokio::test]
async fn disabled_memory_ignores_broken_enrollment_and_profile_notes() {
    use hermes_engine::settings::ProfileSettings;
    use hermes_library::Library;
    use std::fs;

    let provider = Provider::start().await;
    let root = TempDir::new().unwrap();
    let native = root.path().join("reliquary");
    fs::create_dir(&native).unwrap();
    fs::write(native.join("capture-enrollment-v1.json"), b"invalid").unwrap();
    let notes = root.path().join("memories");
    fs::create_dir(&notes).unwrap();
    fs::write(notes.join("MEMORY.md"), PROFILE_NOTE).unwrap();
    let settings = json!({"memory_provider":"off"});
    ProfileSettings::update(root.path(), settings.as_object().unwrap()).unwrap();

    let gateway = provider.gateway(&root).await;
    let mut client = Client::native(&gateway).await;
    let session = client.ok("session.create", json!({"title":"Disabled memory"})).await;
    let id = session["session_id"].as_str().unwrap();
    client.ok("prompt.submit", json!({"session_id":id,"submission_id":"disabled","text":CONTEXT_PROBE})).await;
    let completed = client.event("message.complete").await;
    let report = context_report(&completed);
    assert_eq!(report, json!({"memory_requests":0,"included_fact":false,
        "notes_visible":false,"notes_tool":false,"query_tool":false}));
    let directory = Library::open(root.path()).unwrap().session(id).unwrap().directory;
    let record: Value = serde_json::from_slice(&fs::read(directory.join("conversation.json")).unwrap()).unwrap();
    assert!(record["authority"]["receipts"][0]["capture"].is_null());
    drop(client);
    gateway.shutdown().await.unwrap();
    assert_eq!(provider.memory_requests.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn ticket_auth_is_single_use_and_boundaries_fail_closed() {
    let provider = Provider::start().await;
    let root = TempDir::new().unwrap();
    let gateway = provider.gateway(&root).await;
    let http = reqwest::Client::new();
    let mint = format!("{}/api/auth/ws-ticket", gateway.http_url());
    assert_eq!(
        http.post(&mint).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        http.post(&mint)
            .bearer_auth("incorrect")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        http.post(&mint)
            .bearer_auth(gateway.token())
            .header("Origin", "https://evil.example")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        http.post(&mint)
            .bearer_auth(gateway.token())
            .header("Host", format!("evil.example:{}", gateway.address().port()))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert!(connect_async(gateway.websocket_url()).await.is_err());
    let ticket: Value = http
        .post(&mint)
        .bearer_auth(gateway.token())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let url = format!(
        "{}?ticket={}",
        gateway.websocket_url(),
        ticket["ticket"].as_str().unwrap()
    );
    let (mut ws, _) = connect_async(&url).await.unwrap();
    assert!(
        connect_async(&url).await.is_err(),
        "a redeemed ticket must never work twice"
    );
    ws.close(None).await.unwrap();
    let mut native = Client::native(&gateway).await;
    assert_eq!(
        native.ok("gateway.ping", json!({})).await,
        json!({"ok":true})
    );
    let description = native.ok("runtime.describe", json!({})).await;
    assert_eq!(description["profile_id"], "default");
    assert!(
        description["session_create"]["parameters"]
            .as_array()
            .unwrap()
            .contains(&json!("model"))
    );
    assert_eq!(
        native.call("python.exec", json!({})).await["error"]["code"],
        -32601
    );
    assert_eq!(
        native
            .call("session.list", json!({"profile":"another"}))
            .await["error"]["data"]["reason"],
        "unsupported_profile"
    );
    native.socket.close(None).await.unwrap();
    gateway.shutdown().await.unwrap();
    let mut unsafe_config = GatewayConfig::new(root.path(), provider.config());
    unsafe_config.bind = "0.0.0.0:0".parse().unwrap();
    assert!(Gateway::start(unsafe_config).await.is_err());
}

#[tokio::test]
async fn native_turn_survives_reconnect_approval_and_restart_with_rust_history() {
    let provider = Provider::start().await;
    let root = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let gateway = provider.gateway(&root).await;
    let mut client = Client::native(&gateway).await;
    let create = json!({"request_id":"create-once","source":"gui","title":"Clock test","cwd":workspace.path(),"model":"fixture-model"});
    let session = client.ok("session.create", create.clone()).await;
    let sid = session["session_id"].as_str().unwrap().to_owned();
    assert_eq!(
        client.ok("session.create", create.clone()).await["session_id"],
        sid
    );
    assert_eq!(session["messages"], json!([]));
    assert_eq!(session["execution_generation"], 0);
    let receipt = client
        .ok(
            "prompt.submit",
            json!({"session_id":sid,"submission_id":"send-once","text":"Read the clock"}),
        )
        .await;
    assert_eq!(receipt["ref"]["session_id"], sid);
    assert_eq!(receipt["status"], "started");
    let approval = client.event("approval.request").await;
    let prompt = approval["payload"]["prompt_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let generation = approval["execution_generation"].as_u64().unwrap();
    assert_eq!(approval["payload"]["choices"], json!(["once", "deny"]));
    assert_eq!(
        client.ok("gateway.ping", json!({})).await["ok"],
        true,
        "heartbeats must work while tools wait"
    );
    assert_eq!(client.call("approval.respond",json!({"session_id":sid,"prompt_id":prompt,"execution_generation":0,"choice":"once"})).await["error"]["data"]["reason"],"stale_generation");
    client.socket.close(None).await.unwrap();
    let mut resumed = Client::native(&gateway).await;
    assert_eq!(resumed.call("approval.respond",json!({"session_id":sid,"prompt_id":prompt,"execution_generation":generation,"choice":"once"})).await["error"]["data"]["reason"],"not_attached");
    let snapshot = resumed
        .ok("session.resume", json!({"session_id":sid}))
        .await;
    assert_eq!(snapshot["running"], true);
    assert_eq!(snapshot["prompts"][0]["prompt_id"], prompt);
    assert_eq!(
        resumed
            .ok(
                "prompt.submit",
                json!({"session_id":sid,"submission_id":"send-once","text":"Read the clock"})
            )
            .await["admission_id"],
        receipt["admission_id"],
        "retry must not run a second tool call"
    );
    assert_eq!(resumed.ok("approval.respond",json!({"session_id":sid,"prompt_id":prompt,"execution_generation":generation,"choice":"once"})).await["status"],"resolved");
    let complete = resumed.event("message.complete").await;
    assert_eq!(complete["payload"]["status"], "complete");
    assert_eq!(complete["payload"]["text"], "Clock read successfully.");
    let history = resumed
        .ok("session.history", json!({"session_id":sid}))
        .await;
    assert_eq!(history["count"], 4);
    assert_eq!(history["messages"][2]["name"], "get_current_time");
    let clock: Value =
        serde_json::from_str(history["messages"][2]["content"].as_str().unwrap()).unwrap();
    assert!(clock["utc"].as_str().unwrap().ends_with('Z'));
    let terminal = resumed
        .ok(
            "prompt.receipt",
            json!({"session_id":sid,"admission_id":receipt["admission_id"],"include_result":true}),
        )
        .await;
    assert_eq!(terminal["status"], "terminal");
    assert_eq!(terminal["outcome"], "complete");
    let replay = resumed
        .ok(
            "session.events.since",
            json!({"session_id":sid,"last_seen":0,"replay_epoch":snapshot["replay_epoch"]}),
        )
        .await;
    assert_eq!(replay["truncated"], false);
    assert!(
        replay["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["type"] == "message.complete")
    );
    let mismatch = resumed
        .ok(
            "session.events.since",
            json!({"session_id":sid,"last_seen":100,"replay_epoch":"old-process"}),
        )
        .await;
    assert_eq!(mismatch["snapshot_required"], true);
    let snapshot = resumed
        .ok(
            "session.resume",
            json!({"session_id":sid,"omit_messages":true}),
        )
        .await;
    assert_eq!(snapshot["messages"], json!([]));
    assert_eq!(snapshot["message_count"], 4);
    assert_eq!(snapshot["running"], false);
    let mutation = json!({"session_id":sid,"request_id":"rename-once","operation":"rename","expected_revision":snapshot["revision"],"payload":{"title":"Renamed clock"}});
    let renamed = resumed.ok("session.mutate", mutation.clone()).await;
    assert_eq!(resumed.ok("session.mutate", mutation).await, renamed);
    assert_eq!(resumed.call("session.mutate",json!({"session_id":sid,"request_id":"rename-stale","operation":"rename","expected_revision":0,"payload":{"title":"Wrong"}})).await["error"]["data"]["reason"],"revision_conflict");
    resumed.socket.close(None).await.unwrap();
    gateway.shutdown().await.unwrap();
    // The new listener opens the exact native JSON transcript; no Python process is involved.
    let gateway = provider.gateway(&root).await;
    let mut restarted = Client::native(&gateway).await;
    let restored = restarted
        .ok("session.resume", json!({"session_id":sid}))
        .await;
    assert_eq!(restored["message_count"], 4);
    let retried = restarted.ok("prompt.submit",json!({"session_id":sid,"submission_id":"send-once","text":"Read the clock"})).await;
    assert_eq!(retried["admission_id"],receipt["admission_id"]);
    assert_eq!(retried["status"],"terminal");
    assert_eq!(restarted.call("prompt.submit",json!({"session_id":sid,"submission_id":"send-once","text":"different"})).await["error"]["data"]["reason"],"admission_conflict");
    assert_eq!(restored["info"]["title"], "Renamed clock");
    assert_eq!(
        restarted.ok("session.create", create).await["session_id"],
        sid,
        "creation identity persists across restart"
    );
    assert_eq!(
        restarted
            .ok("session.list", json!({"scope":"stored"}))
            .await["sessions"][0]["id"],
        sid
    );
    restarted.socket.close(None).await.unwrap();
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn legacy_stream_offsets_busy_and_native_interrupt_are_real_transport_operations() {
    let provider = Provider::start().await;
    let root = TempDir::new().unwrap();
    let gateway = provider.gateway(&root).await;
    let mut legacy = Client::legacy(&gateway).await;
    let sid = legacy
        .ok("session.create", json!({"title":"Streaming"}))
        .await["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        legacy
            .ok("prompt.submit", json!({"session_id":sid,"text":"hello"}))
            .await["status"],
        "streaming"
    );
    let delta = legacy.event("message.delta").await;
    assert_eq!(delta["payload"]["offset"], 0);
    let delta = legacy.event("message.delta").await;
    assert_eq!(
        delta["payload"]["offset"], 9,
        "offsets are UTF-16 code units"
    );
    legacy.event("message.complete").await;
    let mut native = Client::native(&gateway).await;
    native.ok("session.resume", json!({"session_id":sid})).await;
    native
        .ok("prompt.submit", json!({"session_id":sid,"text":"slow"}))
        .await;
    native.event("message.delta").await;
    assert_eq!(
        native
            .call("prompt.submit", json!({"session_id":sid,"text":"another"}))
            .await["error"]["data"]["reason"],
        "session_busy"
    );
    let queued = native.ok("prompt.submit",json!({"session_id":sid,"text":"another","queued":true})).await;
    assert_eq!(queued["status"],"queued");
    let cancelled = native.ok("prompt.cancel",json!({"session_id":sid,"admission_id":queued["admission_id"]})).await;
    assert_eq!(cancelled["outcome"],"cancelled");
    assert_eq!(
        native
            .call(
                "session.interrupt",
                json!({"session_id":sid,"execution_generation":1})
            )
            .await["error"]["data"]["reason"],
        "stale_generation"
    );
    let interrupted = native
        .ok(
            "session.interrupt",
            json!({"session_id":sid,"execution_generation":2}),
        )
        .await;
    assert_eq!(interrupted["execution_generation"], 2);
    let complete = native.event("message.complete").await;
    assert_eq!(complete["payload"]["status"], "interrupted");
    assert_eq!(complete["payload"]["text"], "Waiting");
    let snapshot = native.ok("session.resume", json!({"session_id":sid})).await;
    assert_eq!(snapshot["running"], false);
    native.socket.close(None).await.unwrap();
    legacy.socket.close(None).await.unwrap();
    gateway.shutdown().await.unwrap();
    let summaries = hermes_library::Library::open(root.path())
        .unwrap()
        .sessions(None)
        .unwrap();
    let history: Value = serde_json::from_reader(
        std::fs::File::open(summaries[0].directory.join("conversation.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(history["turns"][1]["status"]["state"], "interrupted");
}

#[tokio::test]
async fn denial_provider_errors_and_shutdown_persist_their_real_outcomes() {
    let provider = Provider::start().await;
    let root = TempDir::new().unwrap();
    let gateway = provider.gateway(&root).await;
    let mut client = Client::native(&gateway).await;
    client.socket.send(Message::Text("{".into())).await.unwrap();
    assert_eq!(client.frame("malformed request error", 5).await["error"]["code"], -32700);
    let sid = client.ok("session.create", json!({"title":"Denial"})).await["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    client
        .ok(
            "prompt.submit",
            json!({"session_id":sid,"text":"Read the clock"}),
        )
        .await;
    let approval = client.event("approval.request").await;
    let response = json!({"session_id":sid,"prompt_id":approval["payload"]["prompt_id"],"execution_generation":approval["execution_generation"],"choice":"deny"});
    let mut standing = response.clone();
    standing["choice"] = json!("session");
    assert_eq!(
        client.call("approval.respond", standing).await["error"]["code"],
        -32602
    );
    client.ok("approval.respond", response).await;
    let complete = client.event("message.complete").await;
    assert_eq!(complete["payload"]["text"], "Tool denied.");
    let history = client
        .ok("session.history", json!({"session_id":sid}))
        .await;
    assert_eq!(history["messages"][2]["approved"], false);
    assert!(
        history["messages"][2]["content"]
            .as_str()
            .unwrap()
            .contains("denied")
    );
    client
        .ok(
            "prompt.submit",
            json!({"session_id":sid,"text":"provider-error"}),
        )
        .await;
    let failed = client.event("message.complete").await;
    assert_eq!(failed["payload"]["status"], "error");
    assert!(
        failed["payload"]["error"]
            .as_str()
            .unwrap()
            .contains("fixture rejected")
    );
    client
        .ok("prompt.submit", json!({"session_id":sid,"text":"slow"}))
        .await;
    client.event("message.delta").await;
    gateway.shutdown().await.unwrap();
    let library = hermes_library::Library::open(root.path()).unwrap();
    let history: Value = serde_json::from_reader(
        std::fs::File::open(library.session_dir(&sid).unwrap().join("conversation.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(history["turns"][1]["status"]["state"], "failed");
    assert_eq!(history["turns"][2]["status"]["state"], "interrupted");
}

#[tokio::test]
async fn ported_rust_client_replays_native_completion_after_a_real_disconnect() {
    use hermes_gateway::client::{
        ClientError, ClientMessage, ClientOptions, ConnectionState, GatewayClient,
    };
    let provider = Provider::start().await;
    let root = TempDir::new().unwrap();
    let gateway = provider.gateway(&root).await;
    let client = GatewayClient::new(ClientOptions {
        heartbeat_interval: Duration::ZERO,
        heartbeat_deadline: Duration::ZERO,
        ..ClientOptions::default()
    });
    let mut messages = client.subscribe();
    client
        .connect_native(&gateway.http_url(), gateway.token())
        .await
        .unwrap();
    assert_eq!(client.connection_state(), ConnectionState::Open);
    let sid = client
        .request("session.create", json!({"title":"Ported client"}))
        .await
        .unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    client
        .request(
            "prompt.submit",
            json!({"session_id":sid,"submission_id":"rust-client-turn","text":"Read the clock"}),
        )
        .await
        .unwrap();
    let approval = loop {
        if let ClientMessage::Event(event) = timeout(Duration::from_secs(5), messages.recv())
            .await
            .unwrap()
            .unwrap()
            && event["type"] == "approval.request"
        {
            break event;
        }
    };
    let watermark = client.watermarks()[&sid].as_u64().unwrap();
    assert!(watermark > 0);
    client.close();
    assert!(
        !client.session_replay_barrier(&sid).unwrap().wait().await,
        "disconnected history reads must be discarded"
    );
    // Another attached native viewer completes the actual engine approval while
    // the ported client has no socket. Its old cursor now has a real replay gap.
    let mut other = Client::native(&gateway).await;
    other.ok("session.resume", json!({"session_id":sid})).await;
    other.ok("approval.respond",json!({"session_id":sid,"prompt_id":approval["payload"]["prompt_id"],"execution_generation":approval["execution_generation"],"choice":"once"})).await;
    other.event("message.complete").await;
    client
        .connect_native(&gateway.http_url(), gateway.token())
        .await
        .unwrap();
    if let Some(barrier) = client.session_replay_barrier(&sid) {
        assert!(
            timeout(Duration::from_secs(5), barrier.wait())
                .await
                .unwrap()
        );
    }
    let mut completions = vec![];
    while let Ok(message) = messages.try_recv() {
        if let ClientMessage::Event(event) = message
            && event["type"] == "message.complete"
        {
            completions.push(event);
        }
    }
    assert_eq!(
        completions.len(),
        1,
        "replayed completion must dispatch exactly once"
    );
    assert_eq!(completions[0]["replayed"], true);
    assert_eq!(
        completions[0]["payload"]["text"],
        "Clock read successfully."
    );
    assert!(client.watermarks()[&sid].as_u64().unwrap() > watermark);
    let (ping, history) = tokio::join!(
        client.request("ping", json!({})),
        client.request("session.history", json!({"session_id":sid}))
    );
    assert_eq!(ping.unwrap()["pong"], true);
    assert_eq!(
        history.unwrap()["count"],
        4,
        "RPCs multiplex on the same socket"
    );
    let cancel = hermes_engine::CancellationToken::new();
    cancel.cancel();
    assert!(matches!(
        client
            .request_with_cancel("ping", json!({}), Duration::from_secs(1), Some(&cancel))
            .await,
        Err(ClientError::Cancelled)
    ));
    client.close();
    other.socket.close(None).await.unwrap();
    gateway.shutdown().await.unwrap();
}


#[tokio::test]
async fn local_agent_process_runs_only_after_gateway_approval() {
    let provider = Provider::start().await;
    let root = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let gateway = provider.gateway(&root).await;
    let mut client = Client::native(&gateway).await;
    let sid = client.ok("session.create",json!({"cwd":workspace.path()})).await["session_id"].clone();
    let marker=workspace.path().join("process-proof.txt");
    for choice in ["deny","once"] {
        client.ok("prompt.submit",json!({"session_id":sid,"text":"run pipe fixture"})).await;
        let pending=client.event("approval.request").await;
        assert!(!marker.exists());
        client.ok("approval.respond",json!({"session_id":sid,"prompt_id":pending["payload"]["prompt_id"],
            "execution_generation":pending["execution_generation"],"choice":choice})).await;
        client.event("message.complete").await;
        if choice=="deny" { assert!(!marker.exists()); }
    }
    assert_eq!(std::fs::read(marker).unwrap(),b"approved");
    let history=client.ok("session.history",json!({"session_id":sid})).await;
    assert!(history["messages"].as_array().unwrap().iter().any(|m|m["role"]=="tool" &&
        m["content"].as_str().is_some_and(|s|s.contains("pipe-probe"))));
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn background_process_controls_survive_detach_and_stay_in_the_owning_session() {
    let provider = Provider::start().await;
    let root = TempDir::new().unwrap();
    let gateway = provider.gateway(&root).await;
    let mut first = Client::native(&gateway).await;
    let sid = first.ok("session.create", json!({})).await["session_id"].clone();
    for _ in 0..2 {
        first.ok("prompt.submit", json!({"session_id":sid,"text":"run background fixture"})).await;
        let approval = first.event("approval.request").await;
        first.ok("approval.respond", json!({"session_id":sid,"prompt_id":approval["payload"]["prompt_id"],
            "execution_generation":approval["execution_generation"],"choice":"once"})).await;
        first.event("message.complete").await;
    }
    first.ok("session.close", json!({"session_id":sid})).await;
    drop(first);
    let mut client = Client::native(&gateway).await;
    let other = client.ok("session.create",json!({})).await["session_id"].clone();
    assert!(client.call("process.list",json!({"session_id":sid})).await.get("error").is_some());
    client.ok("session.resume",json!({"session_id":sid})).await;
    let rows = client.ok("process.list",json!({"session_id":sid})).await;
    let jobs = rows["processes"].as_array().unwrap();
    assert_eq!(jobs.len(),2);
    assert!(jobs.iter().all(|p| p["status"] == "running"));
    let process = &jobs[0]["session_id"];
    assert_eq!(client.call("process.kill",json!({"session_id":other,"process_id":process})).await["error"]["code"],4044);
    assert_eq!(client.ok("process.stop",json!({"session_id":other})).await["killed"],0);
    assert_eq!(client.ok("process.kill",json!({"session_id":sid,"process_id":process})).await["status"],"killed");
    assert_eq!(client.ok("process.kill",json!({"session_id":sid,"process_id":process})).await["status"],"already_exited");
    assert_eq!(client.ok("process.stop",json!({"session_id":sid})).await["killed"],1);
    assert_eq!(client.ok("process.stop",json!({"session_id":sid})).await["killed"],0);
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelling_turn_releases_only_processes_started_in_that_turn() {
    let provider = Provider::start().await;
    let root = TempDir::new().unwrap();
    let gateway = provider.gateway(&root).await;
    let mut client = Client::native(&gateway).await;
    let sid = client.ok("session.create", json!({})).await["session_id"].clone();
    client.ok("prompt.submit", json!({"session_id":sid,"text":"run background fixture"})).await;
    let approval = client.event("approval.request").await;
    client.ok("approval.respond", json!({"session_id":sid,"prompt_id":approval["payload"]["prompt_id"],"execution_generation":approval["execution_generation"],"choice":"once"})).await;
    client.event("message.complete").await;
    let before = client.ok("process.list", json!({"session_id":sid})).await;
    let old = before["processes"][0]["session_id"].as_str().unwrap().to_owned();
    let receipt = client.ok("prompt.submit", json!({"session_id":sid,"submission_id":"abandon","text":"abandon fixture"})).await;
    let approval = client.event("approval.request").await;
    client.ok("approval.respond", json!({"session_id":sid,"prompt_id":approval["payload"]["prompt_id"],"execution_generation":approval["execution_generation"],"choice":"once"})).await;
    let rows = timeout(Duration::from_secs(15), async { loop {
        let rows = client.ok("process.list", json!({"session_id":sid})).await;
        if rows["processes"].as_array().unwrap().len() == 2 { break rows; }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }}).await.unwrap();
    let new = rows["processes"].as_array().unwrap().iter().find(|row| row["session_id"] != old).unwrap()["session_id"].as_str().unwrap().to_owned();
    client.ok("prompt.cancel", json!({"session_id":sid,"admission_id":receipt["admission_id"]})).await;
    let complete = client.event("message.complete").await;
    assert_eq!(complete["payload"]["status"], "interrupted");
    let rows = client.ok("process.list", json!({"session_id":sid})).await;
    let rows = rows["processes"].as_array().unwrap();
    assert_eq!(rows.iter().find(|row| row["session_id"] == old).unwrap()["status"], "running");
    assert_eq!(rows.iter().find(|row| row["session_id"] == new).unwrap()["status"], "exited");
    assert_eq!(client.ok("process.stop", json!({"session_id":sid})).await["killed"], 1);
    let receipt = client.ok("prompt.submit", json!({"session_id":sid,"submission_id":"persist","text":"persist fixture"})).await;
    let approval = client.event("approval.request").await;
    client.ok("approval.respond", json!({"session_id":sid,"prompt_id":approval["payload"]["prompt_id"],"execution_generation":approval["execution_generation"],"choice":"once"})).await;
    let id = timeout(Duration::from_secs(15), async { loop {
        let rows = client.ok("process.list", json!({"session_id":sid})).await;
        if let Some(row) = rows["processes"].as_array().unwrap().iter().find(|row| row["status"] == "running" && row["persist_on_release"] == true) { break row["session_id"].as_str().unwrap().to_owned(); }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }}).await.unwrap();
    client.ok("prompt.cancel", json!({"session_id":sid,"admission_id":receipt["admission_id"]})).await;
    assert_eq!(client.event("message.complete").await["payload"]["status"], "interrupted");
    let rows = client.ok("process.list", json!({"session_id":sid})).await;
    assert_eq!(rows["processes"].as_array().unwrap().iter().find(|row| row["session_id"] == id).unwrap()["status"], "running");
    assert_eq!(client.ok("process.stop", json!({"session_id":sid})).await["killed"], 1);
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn group_member_process_remains_controllable_after_viewer_detach() {
    let provider = Provider::start().await;
    let root = TempDir::new().unwrap();
    let gateway = provider.gateway(&root).await;
    let library = hermes_library::Library::open(root.path()).unwrap();
    let ada = library.create_bot("Ada", "", "", "").unwrap();
    let grace = library.create_bot("Grace", "", "", "").unwrap();
    let group = library.create_group("Review", &[ada.name, grace.name]).unwrap();
    let chat = library.group_chat(&group.id).unwrap();
    let mut first = Client::native(&gateway).await;
    first.ok("session.resume", json!({"session_id":chat.id})).await;
    first.ok("prompt.submit", json!({"session_id":chat.id,"submission_id":"group-process","text":"group-job"})).await;
    let approval = first.event("approval.request").await;
    first.ok("approval.respond", json!({"session_id":chat.id,"prompt_id":approval["payload"]["prompt_id"],"execution_generation":approval["execution_generation"],"choice":"once"})).await;
    first.event("message.complete").await;
    first.event("message.complete").await;
    first.ok("session.close", json!({"session_id":chat.id})).await;
    drop(first);
    let mut second = Client::native(&gateway).await;
    second.ok("session.resume", json!({"session_id":chat.id})).await;
    let rows = second.ok("process.list", json!({"session_id":chat.id})).await;
    let jobs = rows["processes"].as_array().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0]["status"], "running");
    assert_eq!(second.ok("process.stop", json!({"session_id":chat.id})).await["killed"], 1);
    gateway.shutdown().await.unwrap();
}

/// Settings › Gateways › Remote: the desktop's remote path (connect_remote,
/// session.create, prompt.submit, approval.respond, message.complete) against
/// a real gateway on this computer.
#[tokio::test]
async fn remote_connect_runs_a_whole_turn_with_approval() {
    use hermes_gateway::client::{ClientMessage, ClientOptions, GatewayClient};
    let provider = Provider::start().await;
    let root = TempDir::new().unwrap();
    let gateway = provider.gateway(&root).await;
    let client = GatewayClient::new(ClientOptions { heartbeat_interval: Duration::ZERO, heartbeat_deadline: Duration::ZERO, ..ClientOptions::default() });
    let mut messages = client.subscribe();
    // A non-HTTPS remote host is refused before any token is sent.
    assert!(GatewayClient::new(ClientOptions::default()).connect_remote("http://example.com", gateway.token()).await.is_err());
    client.connect_remote(&gateway.http_url(), gateway.token()).await.unwrap();
    let sid = client.request("session.create", json!({"source":"gui","title":"Remote"})).await.unwrap()["session_id"].as_str().unwrap().to_owned();
    client.request("prompt.submit", json!({"session_id":sid,"text":"Read the clock"})).await.unwrap();
    let mut streamed = String::new();
    loop {
        let Ok(Ok(ClientMessage::Event(event))) = timeout(Duration::from_secs(10), messages.recv()).await else { continue };
        match event["type"].as_str().unwrap_or_default() {
            "message.delta" => streamed.push_str(event["payload"]["text"].as_str().unwrap_or_default()),
            "approval.request" => { client.request("approval.respond", json!({"session_id":sid,"prompt_id":event["payload"]["prompt_id"],
                "execution_generation":event["execution_generation"],"choice":"once"})).await.unwrap(); }
            "message.complete" => break,
            _ => {}
        }
    }
    assert!(!streamed.is_empty(), "the reply streamed back over the remote connection");
    client.close();
}

#[tokio::test]
async fn signed_webhook_runs_the_agent_into_its_own_chat() {
    let provider = Provider::start().await;
    let root = TempDir::new().unwrap();
    let gateway = provider.gateway(&root).await;
    let library = hermes_library::Library::open(root.path()).unwrap();
    let hook = library.create_webhook(hermes_library::webhooks::Webhook { name: "deploys".into(), prompt: "Read the clock".into(), events: vec!["push".into()], ..Default::default() }).unwrap();
    let body = br#"{"ref":"refs/heads/main"}"#.to_vec();
    let tag = ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, hook.secret.as_bytes()), &body);
    let signature = format!("sha256={}", tag.as_ref().iter().map(|b| format!("{b:02x}")).collect::<String>());
    let http = reqwest::Client::new();
    let url = format!("{}/webhooks/deploys", gateway.http_url());
    let post = |sig: String, event: &'static str| http.post(&url).header("x-hub-signature-256", sig).header("x-github-event", event).header("x-github-delivery", "delivery-1").body(body.clone()).send();
    assert_eq!(post("sha256=00".into(), "push").await.unwrap().status(), 401);
    assert_eq!(post(signature.clone(), "issues").await.unwrap().status(), 200);
    let (first, duplicate) = tokio::join!(post(signature.clone(), "push"), post(signature.clone(), "push"));
    let first = first.unwrap(); let duplicate = duplicate.unwrap();
    assert_eq!(first.status(), 202); assert_eq!(duplicate.status(), 202);
    let first: Value = first.json().await.unwrap(); let duplicate: Value = duplicate.json().await.unwrap();
    assert_eq!(first["receipt"]["admission_id"], duplicate["receipt"]["admission_id"]);
    let unkeyed = || http.post(&url).header("x-hub-signature-256", &signature).header("x-github-event", "push").body(body.clone()).send();
    let (a, b) = tokio::join!(unkeyed(), unkeyed());
    let a = a.unwrap(); let b = b.unwrap();
    assert_eq!(a.status(), 202); assert_eq!(b.status(), 202);
    let a: Value = a.json().await.unwrap(); let b: Value = b.json().await.unwrap();
    assert_ne!(a["delivery_id"], b["delivery_id"]);
    let changed = br#"{\"ref\":\"refs/heads/other\"}"#.to_vec();
    let tag = ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, hook.secret.as_bytes()), &changed);
    let changed_sig = format!("sha256={}", tag.as_ref().iter().map(|b| format!("{b:02x}")).collect::<String>());
    assert_eq!(http.post(&url).header("x-hub-signature-256", changed_sig).header("x-github-event", "push").header("x-github-delivery", "delivery-1").body(changed).send().await.unwrap().status(), 409);
    let hook = library.webhooks().unwrap().remove(0);
    let chat = library.session(hook.session_id.as_deref().unwrap()).unwrap();
    assert_eq!(chat.title, "Webhook · deploys");
    let history: hermes_engine::History = timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(bytes) = std::fs::read(chat.directory.join("conversation.json"))
                && let Ok(history) = serde_json::from_slice::<hermes_engine::History>(&bytes)
                && history.turns.len() == 3 && history.turns.last().is_some_and(|turn| matches!(turn.status, hermes_engine::Status::Complete)) { break history; }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }).await.unwrap();
    assert_eq!(history.turns.len(), 3);
    assert!(history.turns[0].prompt.contains("Read the clock") && history.turns[0].prompt.contains("refs/heads/main"));
    assert!(serde_json::to_string(&history.turns[0]).unwrap().contains("Tool denied."), "unattended webhook tool calls are denied");
    timeout(Duration::from_secs(10), async { loop {
        let hook = library.webhooks().unwrap().remove(0);
        if hook.deliveries.iter().filter(|delivery| delivery.id.is_some()).count() == 3
            && hook.deliveries.iter().filter(|delivery| delivery.id.is_some()).all(|delivery| delivery.status == "ran (complete)") { break; }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }}).await.unwrap();
    gateway.shutdown().await.unwrap();
    // A crash after completion but before the UI projection is repaired on startup.
    library.update_delivery_status("deploys", &hook.secret, "delivery-1", "received").unwrap();
    let gateway = Gateway::start(GatewayConfig::new(root.path(), provider.config())).await.unwrap();
    timeout(Duration::from_secs(10), async { loop {
        let hook = library.webhooks().unwrap().remove(0);
        if hook.deliveries.iter().any(|delivery| delivery.id.as_deref() == Some("delivery-1") && delivery.status == "ran (complete)") { break; }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }}).await.unwrap();
    let response = http.post(format!("{}/webhooks/deploys", gateway.http_url()))
        .header("x-hub-signature-256", &signature).header("x-github-event", "push")
        .header("x-github-delivery", "delivery-1").body(body.clone()).send().await.unwrap();
    assert_eq!(response.status(), 202);
    let retry: Value = response.json().await.unwrap();
    assert_eq!(retry["receipt"]["admission_id"], first["receipt"]["admission_id"]);
    assert_eq!(retry["receipt"]["status"], "terminal");
    let history: hermes_engine::History = serde_json::from_slice(&std::fs::read(chat.directory.join("conversation.json")).unwrap()).unwrap();
    assert_eq!(history.turns.len(), 3);
    library.record_delivery("deploys", hermes_library::webhooks::Delivery { at: "now".into(), event: "push".into(), status: "test".into(), id: None }, Some("missing-session".into())).unwrap();
    let unavailable = http.post(format!("{}/webhooks/deploys", gateway.http_url()))
        .header("x-hub-signature-256", &signature).header("x-github-event", "push")
        .header("x-github-delivery", "delivery-1").body(body.clone()).send().await.unwrap();
    assert_eq!(unavailable.status(), 409, "a lost receipt store must not create a new admission");
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn legacy_flight_hint_does_not_claim_a_matching_historical_row() {
    let provider = Provider::start().await;
    let root = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let library = hermes_library::Library::open(root.path()).unwrap();
    let project = library.add_project(workspace.path()).unwrap();
    let chat = library.create_session(Some(&project.id), Some("Populated legacy chat")).unwrap();
    library.set_archived(&chat.id, true).unwrap();
    let dir = &chat.directory;
    let file = hermes_engine::Attachment::capture_data_url(dir, "note.txt".into(), "data:text/plain;base64,aGVsbG8=").unwrap();
    let history = json!({"version":1,"turns":[{"prompt":"same text","model":"old-model","attachments":[file],
        "entries":[{"role":"assistant","text":"old answer","calls":[]}],"status":{"state":"complete"}}]});
    std::fs::write(dir.join("conversation.json"),serde_json::to_vec(&history).unwrap()).unwrap();
    std::fs::write(dir.join("gateway.json"),br#"{"model":"old-model"}"#).unwrap();
    let queue = json!({"parked":true,"next_id":10,
        "in_flight":{"entry":{"id":9,"text":"same text","attachments":[file]},"turn_index":0},
        "entries":[{"id":10,"text":"later","attachments":[file]}]});
    let path = dir.join("queued-prompts.json");
    std::fs::write(&path,b"{").unwrap();
    let gateway = provider.gateway(&root).await;
    let mut client = Client::native(&gateway).await;
    let params = json!({"session_id":chat.id});
    assert!(client.call("session.resume",params.clone()).await.get("error").is_some());
    let preserved: Value = serde_json::from_slice(&std::fs::read(dir.join("conversation.json")).unwrap()).unwrap();
    assert_eq!(preserved,history);
    std::fs::write(&path,serde_json::to_vec(&queue).unwrap()).unwrap();
    let first = client.ok("session.resume",params.clone()).await;
    let claims = first["pending"].as_array().unwrap().clone();
    assert_eq!(claims.len(),2);
    assert_eq!(claims[0]["status"],"unknown");
    assert_eq!(claims[0]["legacy_turn_index"],0);
    assert!(claims[0]["turn_index"].is_null());
    assert_eq!(claims[1]["status"],"parked");
    assert_eq!(claims[1]["model"],"old-model");
    let rows = client.ok("session.history",params.clone()).await["messages"].clone();
    assert_ne!(rows[0]["turn_id"],claims[0]["admission_id"]);
    assert_eq!(rows[0]["attachments"],json!([file]));
    drop(client);
    gateway.shutdown().await.unwrap();
    let gateway=provider.gateway(&root).await;
    let mut client=Client::native(&gateway).await;
    let recovered = client.ok("session.resume", params.clone()).await;
    assert!(recovered["authority_epoch"].as_u64().unwrap() > first["authority_epoch"].as_u64().unwrap());
    let mut expected = claims.clone();
    expected[1]["authority_epoch"] = recovered["authority_epoch"].clone();
    assert_eq!(recovered["pending"], json!(expected));
    client.ok("prompt.resolve_unknown",json!({"session_id":chat.id,"admission_id":claims[0]["admission_id"],
        "execution_generation":claims[0]["execution_generation"]})).await;
    assert_eq!(client.ok("session.history",params).await["messages"],rows);
    assert_eq!(file.data(dir).unwrap(),b"hello");
    let retained=library.session(&chat.id).unwrap();
    assert_eq!(retained.project_id.as_deref(),Some(project.id.as_str()));
    assert!(retained.archived);
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn crash_checkpoint_holds_follower_until_fenced_resolution_and_preserves_attachments() {
    let provider = Provider::start().await;
    let root = TempDir::new().unwrap();
    let recovery = TempDir::new().unwrap();
    let gateway = provider.gateway(&root).await;
    let mut client = Client::native(&gateway).await;
    let sid = client.ok("session.create",json!({"title":"Crash fixture"})).await["session_id"].as_str().unwrap().to_owned();
    let session = json!({"session_id":sid});
    let head = client.ok("prompt.submit",json!({"session_id":sid,"text":"slow","submission_id":"head"})).await;
    client.event("message.delta").await;
    let attached = client.ok("file.attach", json!({"session_id":sid,"name":"notes.txt","data_url":"data:text/plain;base64,aGVsbG8="})).await;
    let next = json!({"session_id":sid,"submission_id":"follower","text":"Read the note","queued":true,"attachments":[attached["attachment"]]});
    let follower = client.ok("prompt.submit",next.clone()).await;
    assert_eq!(follower["status"],"queued");

    // Copy the actual durable checkpoint while the original owner is still waiting.
    // The second owner sees exactly the pre-crash bytes and a different process lock.
    let source = root.path().join("sessions").join(&sid);
    let copied = recovery.path().join("sessions").join(&sid);
    std::fs::create_dir_all(copied.join("attachments")).unwrap();
    std::fs::copy(root.path().join("library.json"),recovery.path().join("library.json")).unwrap();
    for file in ["gateway.json","conversation.json"] {
        std::fs::copy(source.join(file),copied.join(file)).unwrap();
    }
    let blob = attached["attachment"]["digest"].as_str().unwrap();
    std::fs::copy(source.join("attachments").join(blob),copied.join("attachments").join(blob)).unwrap();
    client.socket.close(None).await.unwrap();
    gateway.shutdown().await.unwrap();

    let gateway = provider.gateway(&recovery).await;
    let mut client = Client::native(&gateway).await;
    let restored = client.ok("session.resume",session.clone()).await;
    assert_eq!(restored["pending"][0]["status"],"unknown");
    assert_eq!(restored["pending"][1]["status"],"queued");
    let mut resolve = json!({"session_id":sid,"admission_id":head["admission_id"],"execution_generation":0});
    assert_eq!(client.call("prompt.resolve_unknown",resolve.clone()).await["error"]["data"]["reason"],"stale_generation");
    resolve["execution_generation"] = head["execution_generation"].clone();
    let settled = client.ok("prompt.resolve_unknown",resolve.clone()).await;
    assert_eq!(settled["outcome"],"interrupted");
    client.event("message.complete").await;
    assert_eq!(client.ok("prompt.resolve_unknown",resolve).await,settled);
    let again = client.ok("prompt.submit",next).await;
    assert_eq!(again["status"],"terminal");
    assert_eq!(again["admission_id"],follower["admission_id"]);
    let history = client.ok("session.resume",session).await;
    assert_eq!(history["native_history"]["turns"].as_array().unwrap().len(),2);
    assert_eq!(history["native_history"]["turns"][1]["attachments"][0],attached["attachment"]);
    assert_eq!(history["native_history"]["turns"][0]["status"]["state"],"interrupted");
    assert!(history["pending"].as_array().unwrap().is_empty());
    client.socket.close(None).await.unwrap();
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn due_routine_runs_and_publishes_with_no_desktop() {
    use hermes_engine::cron::{Draft, RunStatus, Store};
    let provider = Provider::start().await;
    let root = TempDir::new().unwrap();
    let store = Store::open_view(root.path()).unwrap();
    let now = chrono::Utc::now();
    let job = store.create(Draft { name: "Headless".into(), prompt: "Read the clock".into(), schedule: "in 1m".into(), catch_up: Some(true), ..Draft::default() }, now - chrono::Duration::minutes(2)).unwrap();
    let gateway = provider.gateway(&root).await;
    let run = timeout(Duration::from_secs(20), async {
        loop {
            if let Some(run) = store.runs(&job.id).unwrap().into_iter().find(|r| r.status == RunStatus::Completed) { break run; }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }).await.unwrap();
    assert!(run.gateway_session.is_some());
    let index: Value = serde_json::from_slice(&std::fs::read(root.path().join("routine-chats.json")).unwrap()).unwrap();
    let chat_id = index[&job.id].as_str().unwrap();
    let library = hermes_library::Library::open(root.path()).unwrap();
    let chat = library.session(chat_id).unwrap();
    let history: hermes_engine::History = serde_json::from_slice(&std::fs::read(chat.directory.join("conversation.json")).unwrap()).unwrap();
    assert_eq!(history.turns.len(), 1);
    assert_eq!(history.turns[0].prompt, "Read the clock");
    assert!(run.output.contains("Tool denied."), "scheduled approval requests are denied unattended");
    gateway.shutdown().await.unwrap();
    let gateway = provider.gateway(&root).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let history: hermes_engine::History = serde_json::from_slice(&std::fs::read(chat.directory.join("conversation.json")).unwrap()).unwrap();
    assert_eq!(history.turns.len(), 1);
    assert_eq!(store.runs(&job.id).unwrap().len(), 1);
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn manual_routine_retry_reuses_run_and_accepts_approval() {
    use hermes_engine::cron::{Draft, RunStatus, Store};
    let provider = Provider::start().await;
    let root = TempDir::new().unwrap();
    let store = Store::open_view(root.path()).unwrap();
    let job = store.create(Draft { name: "Manual".into(), prompt: "Read the clock".into(), paused: true, ..Draft::default() }, chrono::Utc::now()).unwrap();
    let gateway = provider.gateway(&root).await;
    let mut client = Client::native(&gateway).await;
    let request = json!({"job_id":job.id,"request_id":"trigger-once"});
    let first = client.ok("cron.trigger", request.clone()).await;
    let again = client.ok("cron.trigger", request.clone()).await;
    assert_eq!(first["run_id"], again["run_id"]);
    assert_eq!(first["session_id"], again["session_id"]);
    let sid = first["session_id"].as_str().unwrap();
    let snapshot = client.ok("session.resume", json!({"session_id":sid})).await;
    let approval = if snapshot["prompts"].as_array().unwrap().is_empty() { client.event("approval.request").await["payload"].clone() } else { snapshot["prompts"][0].clone() };
    let prompt = approval["prompt_id"].as_str().unwrap();
    let generation = if snapshot["execution_generation"].as_u64().unwrap_or(0) > 0 { snapshot["execution_generation"].as_u64().unwrap() } else { 1 };
    client.ok("approval.respond", json!({"session_id":sid,"prompt_id":prompt,"execution_generation":generation,"choice":"once"})).await;
    timeout(Duration::from_secs(20), async { loop {
        if store.runs(&job.id).unwrap().iter().any(|run| run.status == RunStatus::Completed) { break; }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }}).await.unwrap();
    assert_eq!(store.runs(&job.id).unwrap().len(), 1);
    client.socket.close(None).await.unwrap();
    gateway.shutdown().await.unwrap();
    let gateway = provider.gateway(&root).await;
    let mut client = Client::native(&gateway).await;
    let retried = client.ok("cron.trigger", request).await;
    assert_eq!(retried["run_id"], first["run_id"]);
    assert_eq!(store.runs(&job.id).unwrap().len(), 1);
    client.socket.close(None).await.unwrap();
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn group_round_runs_each_profile_without_desktop_and_deduplicates_retry() {
    let provider = Provider::start().await;
    let root = TempDir::new().unwrap();
    let library = hermes_library::Library::open(root.path()).unwrap();
    let ada = library.create_bot("Ada", "", "Be terse.", "").unwrap();
    let grace = library.create_bot("Grace", "", "Be kind.", "").unwrap();
    let group = library.create_group("Review", &[ada.name, grace.name]).unwrap();
    let chat = library.group_chat(&group.id).unwrap();
    let gateway = provider.gateway(&root).await;
    let mut client = Client::native(&gateway).await;
    client.ok("session.resume", json!({"session_id":chat.id})).await;
    let request = json!({"session_id":chat.id,"submission_id":"round-once","text":"Review this plan"});
    let receipt = client.ok("prompt.submit", request.clone()).await;
    client.socket.close(None).await.unwrap();
    let history: hermes_engine::History = timeout(Duration::from_secs(20), async { loop {
        if let Ok(bytes) = std::fs::read(chat.directory.join("conversation.json"))
            && let Ok(history) = serde_json::from_slice::<hermes_engine::History>(&bytes)
            && history.turns.len() == 2 && history.turns.last().is_some_and(|turn| matches!(turn.status, hermes_engine::Status::Complete)) { break history; }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }}).await.unwrap();
    let replies: Vec<_> = history.turns.iter().map(|turn| turn.entries.iter().find_map(|entry| if let hermes_engine::Entry::Assistant { text, .. } = entry { Some(text.as_str()) } else { None }).unwrap()).collect();
    assert_eq!(replies, ["Ada here", "Grace saw Ada"]);
    gateway.shutdown().await.unwrap();
    // Reconstruct the durable checkpoint after Ada committed and before Grace
    // was claimed, then restart with every Desktop client closed.
    let path = chat.directory.join("conversation.json");
    let mut checkpoint: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    checkpoint["turns"].as_array_mut().unwrap().pop();
    let child = checkpoint["authority"]["receipts"].as_array_mut().unwrap().iter_mut()
        .find(|receipt| receipt["group"]["first"] == false).unwrap();
    let child_id = child["id"].clone();
    child["status"] = json!("queued"); child["generation"] = json!(0);
    child["outcome"] = Value::Null; child["result"] = Value::Null; child["turn_index"] = Value::Null;
    std::fs::write(&path, serde_json::to_vec(&checkpoint).unwrap()).unwrap();
    let gateway = provider.gateway(&root).await;
    timeout(Duration::from_secs(20), async { loop {
        let record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        if record["turns"].as_array().unwrap().len() == 2 && record["authority"]["receipts"].as_array().unwrap().iter()
            .any(|receipt| receipt["id"] == child_id && receipt["status"] == "terminal") { break; }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }}).await.unwrap();
    let mut client = Client::native(&gateway).await;
    client.ok("session.resume", json!({"session_id":chat.id})).await;
    let retry = client.ok("prompt.submit", request).await;
    assert_eq!(receipt["admission_id"], retry["admission_id"]);
    let saved: hermes_engine::History = serde_json::from_slice(&std::fs::read(chat.directory.join("conversation.json")).unwrap()).unwrap();
    assert_eq!(saved.turns.len(), 2);
    let failed = client.ok("prompt.submit", json!({"session_id":chat.id,"submission_id":"fail-once","text":"fail-group"})).await;
    timeout(Duration::from_secs(20), async { loop {
        let record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let round: Vec<_> = record["authority"]["receipts"].as_array().unwrap().iter()
            .filter(|row| row["group"]["round"] == failed["admission_id"]).collect();
        if round.len() == 2 && round.iter().all(|row| row["status"] == "terminal") {
            assert_eq!(round[1]["outcome"], "cancelled"); break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }}).await.unwrap();
    let active = client.ok("prompt.submit", json!({"session_id":chat.id,"submission_id":"hold","text":"hold-group"})).await;
    let queued = client.ok("prompt.submit", json!({"session_id":chat.id,"submission_id":"queued-round","text":"next","queued":true})).await;
    client.ok("prompt.cancel", json!({"session_id":chat.id,"admission_id":queued["admission_id"]})).await;
    let record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let round: Vec<_> = record["authority"]["receipts"].as_array().unwrap().iter()
        .filter(|row| row["group"]["round"] == queued["admission_id"]).collect();
    assert_eq!(round.len(), 2);
    assert!(round.iter().all(|row| row["status"] == "terminal" && row["outcome"] == "cancelled"));
    client.ok("prompt.cancel", json!({"session_id":chat.id,"admission_id":active["admission_id"]})).await;
    client.socket.close(None).await.unwrap();
    gateway.shutdown().await.unwrap();
}
