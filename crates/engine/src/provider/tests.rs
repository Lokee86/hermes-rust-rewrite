use super::*;
use axum::{
    Json, Router,
    body::{Body, Bytes},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::post,
};
use futures_util::{Stream, stream};
use std::{
    convert::Infallible,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};
use tokio::sync::mpsc;

const OPENAI_TOOLS: &str = include_str!("fixtures/openai_parallel_tools.sse");
const ANTHROPIC_TOOLS: &str = include_str!("fixtures/anthropic_tools.sse");
const ANTHROPIC_TEXT: &str = include_str!("fixtures/anthropic_text.sse");
const OPENAI_TEXT: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

fn config(base_url: impl Into<String>) -> Config {
    Config {
        base_url: base_url.into(),
        model: "fixture-model".into(),
        api_key: String::new(),
    }
}

fn sse(data: &'static str) -> Response {
    // Split inside JSON, SSE delimiters, and UTF-8 code points to exercise the
    // actual network/SSE decoder rather than splitting fixtures on newlines.
    let chunks = data
        .as_bytes()
        .chunks(7)
        .map(|part| Ok::<_, Infallible>(Bytes::copy_from_slice(part)))
        .collect::<Vec<_>>();
    Response::builder()
        .header("content-type", "text/event-stream; charset=utf-8")
        .body(Body::from_stream(stream::iter(chunks)))
        .unwrap()
}

async fn serve(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{address}"), task)
}

fn user_messages() -> Vec<Value> {
    vec![json!({"role":"user","content":"Hello"})]
}

#[tokio::test]
async fn streamed_and_structured_responses_share_selected_reasoning() {
    let router = Router::new().route("/responses", post(|Json(body): Json<Value>| async move {
        assert!(body.get("reasoning_effort").is_none());
        let effort = body["input"][0]["content"][0]["text"].as_str().unwrap();
        if effort == "default" {
            assert!(body.get("reasoning").is_none());
        } else {
            assert_eq!(body["reasoning"], json!({"effort": effort}));
        }
        sse(concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"{\\\"ok\\\":true}\"}\n\n",
            "data: {\"type\":\"response.completed\"}\n\n",
        ))
    }));
    let (base, server) = serve(router).await;
    for (selected, wire) in [(None, "default"), (Some("low"), "low"), (Some("high"), "high"),
        (Some("none"), "none"), (Some("ultra"), "max")]
    {
        let mut provider = Provider::new_with_options(config(&base), crate::models::GenerationOptions {
            reasoning_effort: selected.map(str::to_owned),
            ..Default::default()
        }).unwrap();
        provider.protocol = Protocol::Responses;
        provider.endpoint = format!("{base}/responses").parse().unwrap();
        let mut text = String::new();
        provider.stream_with_tools(vec![json!({"role":"user","content":wire})], &[], |delta| {
            text.push_str(delta);
            Ok(())
        }).await.unwrap();
        assert_eq!(text, "{\"ok\":true}");
        assert_eq!(provider.complete_json("Output JSON.", wire, "fixture_schema", &json!({"type":"object"}))
            .await.unwrap(), json!({"ok":true}));
    }
    server.abort();
}

#[test]
fn catalog_requests_use_provider_routes_and_authentication() {
    for (base, expected, anthropic, bearer) in [
        (OPENAI, format!("{OPENAI}/models"), false, true),
        (ANTHROPIC, format!("{ANTHROPIC}/models"), true, false),
        ("http://127.0.0.1:12345/custom/chat/completions", "http://127.0.0.1:12345/custom/models".into(), false, true),
    ] {
        let provider = Provider::new(Config { api_key: "catalog-fixture-token".into(), ..config(base) }).unwrap();
        let mut requests = provider.catalog_requests().unwrap();
        assert_eq!(requests.len(), 1);
        let request = requests.remove(0).build().unwrap();
        assert_eq!(request.method(), reqwest::Method::GET);
        assert_eq!(request.url().as_str(), expected);
        assert!(request.body().is_none());
        assert_eq!(request.headers().get("authorization").map(|value| value.to_str().unwrap()),
            bearer.then_some("Bearer catalog-fixture-token"));
        assert_eq!(request.headers().get("x-api-key").map(|value| value.to_str().unwrap()),
            anthropic.then_some("catalog-fixture-token"));
        assert_eq!(request.headers().get("anthropic-version").map(|value| value.to_str().unwrap()),
            anthropic.then_some("2023-06-01"));
    }
    let subscription = crate::accounts::CATALOG.iter().find(|account| {
        Url::parse(account.base_url).unwrap().host_str() == Some("chatgpt.com")
    }).unwrap();
    let expected_path = format!("{}/models", Url::parse(subscription.base_url).unwrap().path());
    for base in [subscription.base_url.to_owned(), format!("{}/responses", subscription.base_url)] {
        let provider = Provider::new(Config { api_key: "subscription-fixture-token".into(), ..config(base) }).unwrap();
        let requests = provider.catalog_requests().unwrap();
        assert_eq!(requests.len(), 2);
        for (request, version) in requests.into_iter().zip(["99.0.0", "0.0.0"]) {
            let request = request.build().unwrap();
            assert_eq!(request.url().path(), expected_path);
            assert_eq!(request.url().query(), Some(format!("client_version={version}").as_str()));
            assert_eq!(request.headers()["originator"], "hermes-agent");
            assert_eq!(request.headers()["authorization"], "Bearer subscription-fixture-token");
        }
    }
    let gateway = crate::accounts::CATALOG.iter().find(|account| {
        Url::parse(account.base_url).unwrap().path().ends_with("/anthropic")
    }).unwrap();
    let expected_path = format!("{}/v1/models", Url::parse(gateway.base_url).unwrap().path());
    for base in [gateway.base_url.to_owned(), format!("{}/v1/messages", gateway.base_url)] {
        let provider = Provider::new(Config { api_key: "gateway-fixture-token".into(), ..config(base) }).unwrap();
        let request = provider.catalog_requests().unwrap().remove(0).build().unwrap();
        assert_eq!(request.url().path(), expected_path);
        assert_eq!(request.headers()["authorization"], "Bearer gateway-fixture-token");
    }
}

#[tokio::test]
async fn structured_completion_uses_the_native_provider_request() {
    let router = Router::new()
        .route(
            "/v1/messages",
            post(|headers: HeaderMap, Json(body): Json<Value>| async move {
                assert_eq!(headers["x-api-key"], "anthropic-fixture-key");
                assert_eq!(headers["anthropic-version"], "2023-06-01");
                assert_eq!(body["system"], "Output JSON.");
                assert_eq!(body["stream"], false);
                assert_eq!(body["output_config"]["format"]["type"], "json_schema");
                Json(json!({
                    "stop_reason":"end_turn",
                    "content":[{"type":"text","text":"{\"ok\":true}"}],
                }))
            }),
        )
        .route(
            "/responses",
            post(|headers: HeaderMap, Json(body): Json<Value>| async move {
                assert_eq!(headers["authorization"], "Bearer codex-fixture-token");
                assert_eq!(headers["originator"], "hermes-agent");
                assert_eq!(body["instructions"], "Output JSON.");
                assert_eq!(body["store"], false);
                assert_eq!(body["text"]["format"]["name"], "fixture_schema");
                sse(concat!(
                    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"{\\\"ok\\\":true}\"}\n\n",
                    "data: {\"type\":\"response.completed\"}\n\n",
                ))
            }),
        );
    let (base, server) = serve(router).await;
    let anthropic = Provider::new(Config {
        api_key: "anthropic-fixture-key".into(),
        ..config(format!("{base}/v1/messages"))
    })
    .unwrap();
    let mut codex = Provider::new(Config {
        api_key: "codex-fixture-token".into(),
        ..config(&base)
    })
    .unwrap();
    // Keep the Codex wire path under test without making an account request.
    codex.protocol = Protocol::Responses;
    codex.endpoint = format!("{base}/responses").parse().unwrap();
    let schema = json!({
        "type":"object", "properties":{"ok":{"type":"boolean"}},
        "required":["ok"], "additionalProperties":false,
    });
    for provider in [anthropic, codex] {
        assert_eq!(
            provider.complete_json("Output JSON.", "Hello", "fixture_schema", &schema)
                .await.unwrap(),
            json!({"ok":true}),
        );
    }
    server.abort();
}

#[test]
fn endpoint_detection_preserves_explicit_paths_and_supports_ipv6_loopback() {
    for (base, protocol, endpoint) in [
        (
            OPENROUTER,
            Protocol::OpenAi,
            "https://openrouter.ai/api/v1/chat/completions",
        ),
        (
            OPENAI,
            Protocol::OpenAi,
            "https://api.openai.com/v1/chat/completions",
        ),
        (
            "https://api.openai.com",
            Protocol::OpenAi,
            "https://api.openai.com/v1/chat/completions",
        ),
        (
            ANTHROPIC,
            Protocol::Anthropic,
            "https://api.anthropic.com/v1/messages",
        ),
        (
            "https://api.anthropic.com",
            Protocol::Anthropic,
            "https://api.anthropic.com/v1/messages",
        ),
        (
            "https://proxy.example/custom/messages",
            Protocol::Anthropic,
            "https://proxy.example/custom/messages",
        ),
        (
            "http://localhost:11434/v1",
            Protocol::OpenAi,
            "http://localhost:11434/v1/chat/completions",
        ),
        (
            "http://[::1]:8080/v1/chat/completions",
            Protocol::OpenAi,
            "http://[::1]:8080/v1/chat/completions",
        ),
        (
            "http://localhost:1234/v1/chat/completions/",
            Protocol::OpenAi,
            "http://localhost:1234/v1/chat/completions",
        ),
        (
            crate::portal::INFERENCE,
            Protocol::OpenAi,
            "https://inference-api.nousresearch.com/v1/chat/completions",
        ),
    ] {
        let mut config = config(base);
        config.api_key = "unit-test-key".into();
        config.validate().unwrap();
        let actual = config.endpoint().unwrap();
        assert_eq!(actual.0, protocol);
        assert_eq!(actual.1.as_str(), endpoint);
    }
}

#[test]
fn credential_fallback_is_limited_to_exact_official_origins_and_paths() {
    assert_eq!(official_key_env(OPENROUTER), Some("OPENROUTER_API_KEY"));
    assert_eq!(
        official_key_env("https://api.openai.com/v1/"),
        Some("OPENAI_API_KEY")
    );
    assert_eq!(official_key_env(ANTHROPIC), Some("ANTHROPIC_API_KEY"));
    for base in [
        "http://localhost:8000/v1",
        "https://evil.example/v1",
        "https://api.openai.com.evil.example/v1",
        "https://api.openai.com/custom",
        "https://api.openai.com:444/v1",
        "https://user@api.openai.com/v1",
        "https://api.openai.com/v1?api_key=wrong",
        crate::portal::INFERENCE,
    ] {
        assert_eq!(official_key_env(base), None);
    }
}

#[test]
fn config_rejects_remote_http_embedded_credentials_and_invalid_keys() {
    for base in [
        "http://remote.example/v1",
        "https://user:password@proxy.example/v1",
        "https://proxy.example/v1?secret=hidden",
        "https://proxy.example/v1#fragment",
    ] {
        assert!(config(base).validate().is_err());
    }
    let mut invalid = config("http://127.0.0.1:1234/v1");
    invalid.api_key = "test\r\nheader".into();
    assert!(invalid.validate().is_err());
    assert!(
        config(crate::portal::INFERENCE).validate().is_ok(),
        "Portal configuration can be validated before sign-in"
    );
}

#[test]
fn anthropic_request_translates_parallel_results_denials_and_tool_schemas() {
    let tools = vec![
        json!({"type":"function","function":{"name":"read_file","description":"Read a file","strict":true,"parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}}}),
    ];
    let messages = vec![
        json!({"role":"system","content":"System instructions"}),
        json!({"role":"user","content":"Read the files"}),
        json!({"role":"assistant","content":"I'll read both.","tool_calls":[
            {"id":"call_one","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"one.txt\"}"}},
            {"id":"call_two","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"two.txt\"}"}}
        ]}),
        json!({"role":"tool","tool_call_id":"call_one","content":"file contents"}),
        json!({"role":"tool","tool_call_id":"call_two","content":"{\"error\":\"User denied this tool call\"}"}),
        json!({"role":"user","content":"Use what you have"}),
    ];
    let body = anthropic_body("claude-fixture", messages, &tools).unwrap();
    assert_eq!(body["system"], "System instructions");
    assert_eq!(body["max_tokens"], 8192);
    assert_eq!(body["messages"].as_array().unwrap().len(), 3);
    assert_eq!(
        body["messages"][1]["content"][1],
        json!({"type":"tool_use","id":"call_one","name":"read_file","input":{"path":"one.txt"}})
    );
    let results = &body["messages"][2]["content"];
    assert_eq!(results[0]["tool_use_id"], "call_one");
    assert_eq!(results[0]["is_error"], false);
    assert_eq!(results[1]["tool_use_id"], "call_two");
    assert_eq!(results[1]["is_error"], true);
    assert_eq!(
        results[2],
        json!({"type":"text","text":"Use what you have"})
    );
    assert_eq!(
        body["tools"][0]["input_schema"],
        tools[0]["function"]["parameters"]
    );
    assert!(body["tools"][0].get("function").is_none());
}

#[tokio::test]
async fn openai_stream_assembles_parallel_tool_deltas_over_real_http() {
    let (captured_tx, mut captured_rx) = mpsc::channel(1);
    let router = Router::new().route(
        "/v1/chat/completions",
        post(
            move |headers: HeaderMap, Json(body): Json<Value>| async move {
                captured_tx.send((headers, body)).await.unwrap();
                sse(OPENAI_TOOLS)
            },
        ),
    );
    let (base, server) = serve(router).await;
    let mut config = config(format!("{base}/v1"));
    config.api_key = "unit-test-key".into();
    let provider = Provider::new(config).unwrap();
    let mut text = String::new();
    let custom_tool = json!({"type":"function","function":{"name":"custom","parameters":{"type":"object","properties":{}}}});
    let calls = provider
        .stream_with_tools(
            user_messages(),
            std::slice::from_ref(&custom_tool),
            |delta| {
                text.push_str(delta);
                Ok(())
            },
        )
        .await
        .unwrap();
    assert_eq!(text, "I'll inspect the workspace.");
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0],
        ToolCall {
            id: "call_read".into(),
            name: "read_file".into(),
            arguments: "{\"path\":\"notes/hello.txt\"}".into()
        }
    );
    assert_eq!(calls[1].arguments, "{}");
    let (headers, body) = captured_rx.recv().await.unwrap();
    assert_eq!(headers["authorization"], "Bearer unit-test-key");
    assert_eq!(
        headers["user-agent"],
        concat!("HermesRust/", env!("CARGO_PKG_VERSION"))
    );
    assert!(!headers.contains_key("x-api-key"));
    assert_eq!(body["tools"], json!([custom_tool]));
    server.abort();
}

#[tokio::test]
async fn native_anthropic_stream_assembles_tools_and_sends_native_headers() {
    let (captured_tx, mut captured_rx) = mpsc::channel(1);
    let router = Router::new().route(
        "/v1/messages",
        post(
            move |headers: HeaderMap, Json(body): Json<Value>| async move {
                captured_tx.send((headers, body)).await.unwrap();
                sse(ANTHROPIC_TOOLS)
            },
        ),
    );
    let (base, server) = serve(router).await;
    let mut config = config(format!("{base}/v1/messages"));
    config.api_key = "unit-test-key".into();
    let provider = Provider::new(config).unwrap();
    let mut text = String::new();
    let calls = provider
        .stream(user_messages(), |delta| {
            text.push_str(delta);
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(text, "I'll read the note.");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].id, "toolu_read");
    assert_eq!(calls[0].arguments, "{\"path\":\"notes/hello.txt\"}");
    assert_eq!(calls[1].arguments, "{}");
    let (headers, body) = captured_rx.recv().await.unwrap();
    assert_eq!(headers["x-api-key"], "unit-test-key");
    assert_eq!(headers["anthropic-version"], "2023-06-01");
    assert!(!headers.contains_key("authorization"));
    assert!(body["tools"][0]["input_schema"].is_object());
    server.abort();
}

#[tokio::test]
async fn native_anthropic_text_handles_split_utf8_and_terminal_message_stop() {
    let (base, server) =
        serve(Router::new().route("/v1/messages", post(|| async { sse(ANTHROPIC_TEXT) }))).await;
    let provider = Provider::new(config(format!("{base}/v1/messages"))).unwrap();
    let mut text = String::new();
    let calls = provider
        .stream(user_messages(), |delta| {
            text.push_str(delta);
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(text, "Hello 🌏");
    assert!(calls.is_empty());
    server.abort();
}

#[tokio::test]
async fn local_provider_needs_no_auth_and_empty_tool_registry_is_supported() {
    let (captured_tx, mut captured_rx) = mpsc::channel(1);
    let router = Router::new().route(
        "/v1/chat/completions",
        post(
            move |headers: HeaderMap, Json(body): Json<Value>| async move {
                captured_tx.send((headers, body)).await.unwrap();
                sse(OPENAI_TEXT)
            },
        ),
    );
    let (base, server) = serve(router).await;
    let provider = Provider::new(config(base)).unwrap();
    let calls = provider
        .stream_with_tools(user_messages(), &[], |_| Ok(()))
        .await
        .unwrap();
    assert!(calls.is_empty());
    let (headers, body) = captured_rx.recv().await.unwrap();
    assert!(!headers.contains_key("authorization"));
    assert!(body.get("tools").is_none());
    server.abort();
}

#[test]
fn openai_finish_reasons_never_dispatch_truncated_tools() {
    for reason in ["length", "content_filter", "stop", "function_call", "error"] {
        let mut state = OpenAiStream::default();
        state.accept(&json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_test","function":{"name":"write_file","arguments":"{\"path\":"}}]},"finish_reason":reason}]}).to_string(), &mut |_| Ok(())).unwrap();
        assert!(
            state.finish().is_err(),
            "Unexpected usable result for {reason}"
        );
    }
    assert!(OpenAiStream::default().finish().is_err());
}

#[test]
fn invalid_duplicate_or_excessive_tools_are_rejected() {
    let call = ToolCall {
        id: "same".into(),
        name: "read_file".into(),
        arguments: "{}".into(),
    };
    assert!(valid_calls(vec![call.clone(), call]).is_err());
    for arguments in ["[]", "null", "{\"unfinished\":"] {
        assert!(
            valid_calls(vec![ToolCall {
                id: "call".into(),
                name: "read_file".into(),
                arguments: arguments.into()
            }])
            .is_err()
        );
    }
    let mut state = OpenAiStream::default();
    assert!(state.accept(&json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":8,"id":"call","function":{"name":"read_file","arguments":"{}"}}]},"finish_reason":null}]}).to_string(), &mut |_| Ok(())).is_err());
}

#[test]
fn anthropic_disconnect_and_limit_never_dispatch_tools() {
    let mut state = AnthropicStream::default();
    for data in ANTHROPIC_TOOLS
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
    {
        if data.contains("\"type\":\"message_stop\"") {
            break;
        }
        state.accept(data, &mut |_| Ok(())).unwrap();
    }
    assert!(state.finish().is_err());
    let mut limited = AnthropicStream::default();
    for data in ANTHROPIC_TOOLS
        .replace(
            "\"stop_reason\":\"tool_use\"",
            "\"stop_reason\":\"max_tokens\"",
        )
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
    {
        limited.accept(data, &mut |_| Ok(())).unwrap();
    }
    assert!(
        limited
            .finish()
            .unwrap_err()
            .to_string()
            .contains("token limit")
    );
}

#[tokio::test]
async fn http_and_in_stream_errors_redact_configured_credentials() {
    for (path, router) in [
        ("/v1/chat/completions", Router::new().route("/v1/chat/completions", post(|| async { (StatusCode::UNAUTHORIZED, Json(json!({"error":{"message":"Invalid unit-test-secret"}}))) }))),
        ("/v1/chat/completions", Router::new().route("/v1/chat/completions", post(|| async { sse("data: {\"error\":{\"code\":429,\"message\":\"Limited unit-test-secret\"}}\n\n") }))),
        ("/v1/chat/completions", Router::new().route("/v1/chat/completions", post(|| async { sse("event: error\ndata: {\"message\":\"Failed unit-test-secret\"}\n\n") }))),
        ("/v1/messages", Router::new().route("/v1/messages", post(|| async { sse("event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded unit-test-secret\"}}\n\n") }))),
    ] {
        let (base, server) = serve(router).await;
        let mut config = config(format!("{base}{path}"));
        config.api_key = "unit-test-secret".into();
        let error = Provider::new(config).unwrap().stream(user_messages(), |_| Ok(())).await.unwrap_err().to_string();
        assert!(error.contains("[redacted]"));
        assert!(!error.contains("unit-test-secret"));
        server.abort();
    }
}

#[tokio::test]
async fn http_error_supports_top_level_detail_and_non_json_access_hints() {
    let router = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            (
                StatusCode::FORBIDDEN,
                Json(json!({"detail":"Inference scope missing"})),
            )
        }),
    );
    let (base, server) = serve(router).await;
    let error = Provider::new(config(format!("{base}/v1")))
        .unwrap()
        .stream(user_messages(), |_| Ok(()))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("Inference scope missing"));
    server.abort();
    let router = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            (
                StatusCode::FORBIDDEN,
                [("content-type", "text/html")],
                "<html>gateway response</html>",
            )
        }),
    );
    let (base, server) = serve(router).await;
    let error = Provider::new(config(format!("{base}/v1")))
        .unwrap()
        .stream(user_messages(), |_| Ok(()))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("Inference access denied"));
    assert!(error.contains("non-JSON error; content type: text/html"));
    assert!(!error.contains("<html>"));
    server.abort();
}

#[test]
fn refusals_complete_without_tools_and_unknown_anthropic_events_are_ignored() {
    let mut state = OpenAiStream::default();
    let mut text = String::new();
    state.accept(r#"{"choices":[{"index":0,"delta":{"refusal":"I cannot help with that."},"finish_reason":"stop"}]}"#, &mut |delta| { text.push_str(delta); Ok(()) }).unwrap();
    assert!(state.finish().unwrap().is_empty());
    assert_eq!(text, "I cannot help with that.");
    let mut state = AnthropicStream::default();
    state
        .accept(r#"{"type":"future_event","data":"ignored"}"#, &mut |_| {
            Ok(())
        })
        .unwrap();
    for data in ANTHROPIC_TEXT
        .replace(
            "\"stop_reason\":\"end_turn\"",
            "\"stop_reason\":\"refusal\"",
        )
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
    {
        state.accept(data, &mut |_| Ok(())).unwrap();
    }
    assert!(state.finish().unwrap().is_empty());
}

#[test]
fn malformed_events_and_closed_ui_abort_before_tools_can_execute() {
    let mut state = OpenAiStream::default();
    assert!(state.accept("invalid JSON", &mut |_| Ok(())).is_err());
    assert!(
        state
            .accept(
                r#"{"choices":[{"index":0,"delta":{"content":"text"},"finish_reason":"stop"}]}"#,
                &mut |_| Err(anyhow!("UI closed"))
            )
            .is_err()
    );
    let mut state = AnthropicStream::default();
    assert!(state.accept(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"no start"}}"#, &mut |_| Ok(())).is_err());
}

struct PendingBody {
    sent: bool,
    dropped: Arc<AtomicBool>,
}
impl Stream for PendingBody {
    type Item = std::result::Result<Bytes, Infallible>;
    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.sent {
            return Poll::Pending;
        }
        self.sent = true;
        Poll::Ready(Some(Ok(Bytes::from_static(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Started\"},\"finish_reason\":null}]}\n\n"))))
    }
}
impl Drop for PendingBody {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn cancelling_stream_drops_http_response_and_server_body() {
    let dropped = Arc::new(AtomicBool::new(false));
    let server_dropped = dropped.clone();
    let router = Router::new().route(
        "/v1/chat/completions",
        post(move || async move {
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(PendingBody {
                    sent: false,
                    dropped: server_dropped,
                }))
                .unwrap()
        }),
    );
    let (base, server) = serve(router).await;
    let provider = Provider::new(config(format!("{base}/v1"))).unwrap();
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    {
        let request = provider.stream(user_messages(), |text| {
            seen_tx.send(text.to_owned()).unwrap();
            Ok(())
        });
        tokio::pin!(request);
        tokio::select! {
            result = &mut request => panic!("Pending stream ended unexpectedly: {result:?}"),
            text = seen_rx.recv() => assert_eq!(text.as_deref(), Some("Started")),
            _ = tokio::time::sleep(Duration::from_secs(3)) => panic!("No initial stream text"),
        }
        // Leaving the scope drops the future exactly as Session cancellation does.
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        while !dropped.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Cancellation did not close the server's response stream");
    server.abort();
}

#[test]
fn permits_empty_usage_trailer_after_finish_but_rejects_late_tokens() {
    let mut state = OpenAiStream::default();
    let mut text = String::new();
    let mut emit = |delta: &str| {text.push_str(delta); Ok(())};
    state.accept(r#"{"choices":[{"index":0,"delta":{"content":"finished"},"finish_reason":"stop"}]}"#, &mut emit).unwrap();
    state.accept(r#"{"choices":[{"index":0,"delta":{},"finish_reason":null}],"usage":{"completion_tokens":1}}"#, &mut emit).unwrap();
    assert!(state.accept(r#"{"choices":[{"index":0,"delta":{"content":"untrusted late text"},"finish_reason":null}]}"#, &mut emit).is_err());
    assert_eq!(text,"finished");
}

#[test]
fn codex_subscription_uses_the_responses_protocol() {
    let config = Config { base_url: "https://chatgpt.com/backend-api/codex".into(), model: "gpt-5.5".into(), api_key: "t".into() };
    let (protocol, url) = config.endpoint().unwrap();
    assert_eq!(protocol, Protocol::Responses);
    assert_eq!(url.as_str(), "https://chatgpt.com/backend-api/codex/responses");
    let minimax = Config { base_url: "https://api.minimax.io/anthropic".into(), model: "MiniMax-M2".into(), api_key: "t".into() };
    let (protocol, url) = minimax.endpoint().unwrap();
    assert_eq!((protocol, url.as_str()), (Protocol::Anthropic, "https://api.minimax.io/anthropic/v1/messages"));
    let body = responses_body("gpt-5.5", vec![
        json!({"role":"system","content":"Be brief."}), json!({"role":"user","content":"hi"}),
        json!({"role":"assistant","content":"","tool_calls":[{"id":"c1","type":"function","function":{"name":"read_file","arguments":"{}"}}]}),
        json!({"role":"tool","tool_call_id":"c1","content":"ok"}),
    ], &[json!({"type":"function","function":{"name":"read_file","description":"Read","parameters":{"type":"object"}}})]);
    assert_eq!(body["instructions"], "Be brief.");
    assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
    assert_eq!(body["input"][1]["type"], "function_call");
    assert_eq!(body["input"][2]["type"], "function_call_output");
    assert_eq!(body["tools"][0]["name"], "read_file");
    assert_eq!(body["store"], false);
    let mut stream = ResponsesStream::default();
    let mut text = String::new();
    let mut emit = |t: &str| { text.push_str(t); Ok(()) };
    assert!(!stream.accept(r#"{"type":"response.output_text.delta","delta":"Hel"}"#, &mut emit).unwrap());
    assert!(!stream.accept(r#"{"type":"response.output_text.delta","delta":"lo"}"#, &mut emit).unwrap());
    assert!(!stream.accept(r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"c2","name":"list_directory","arguments":"{\"path\":\".\"}"}}"#, &mut emit).unwrap());
    assert!(stream.accept(r#"{"type":"response.completed","response":{}}"#, &mut emit).unwrap());
    assert_eq!(text, "Hello");
    let calls = stream.finish().unwrap();
    assert_eq!((calls[0].id.as_str(), calls[0].name.as_str()), ("c2", "list_directory"));
    // The account id from the JWT becomes ChatGPT-Account-ID.
    use base64::Engine;
    let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct_1"}}"#);
    assert_eq!(codex_account_headers(&format!("h.{claims}.s")), vec![("ChatGPT-Account-ID", "acct_1".to_owned())]);
}

#[test]
fn a_stopped_reply_stays_in_the_conversation_with_every_tool_call_answered() {
    let call = |id: &str, arguments: &str| ToolCall { id: id.into(), name: "read_file".into(), arguments: arguments.into() };
    let turn = |status: Status, entries: Vec<Entry>| crate::Turn {
        attachments: vec![], prompt: "look".into(), context: String::new(), model: "m".into(), entries, status,
    };
    let mut history = History::default();
    history.turns.push(turn(Status::Interrupted, vec![
        Entry::Assistant { text: "Reading both.".into(), calls: vec![call("a", "{}"), call("b", "{}")] },
        Entry::Tool { id: "a".into(), name: "read_file".into(), approved: true, result: "contents".into() },
    ]));
    history.turns.push(turn(Status::Failed("secret detail".into()), vec![
        Entry::Assistant { text: "Half a".into(), calls: vec![call("c", "{\"pa")] },
    ]));
    history.turns.push(turn(Status::Interrupted, vec![Entry::Assistant { text: String::new(), calls: vec![] }]));
    history.turns.push(turn(Status::Running, vec![]));
    let sent = messages(&history, std::path::Path::new(".")).unwrap();
    let roles: Vec<_> = sent.iter().map(|message| message["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["system", "user", "assistant", "tool", "tool", "assistant", "user", "assistant", "user", "assistant", "user"]);
    assert_eq!(sent[3]["content"], "contents");
    assert!(sent[4]["content"].as_str().unwrap().contains("stopped before this tool ran"));
    assert!(sent[7]["content"].as_str().unwrap().starts_with("Half a") && sent[7].get("tool_calls").is_none());
    assert!(!serde_json::to_string(&sent).unwrap().contains("secret detail"));
    anthropic_body("claude-fixture", sent, &[]).unwrap();
}

#[test]
fn requests_mark_the_stable_prefix_for_caching() {
    let tools = [json!({"type":"function","function":{"name":"t","parameters":{"type":"object"}}})];
    let conversation = vec![
        json!({"role":"system","content":"System"}),
        json!({"role":"user","content":"one"}),
        json!({"role":"assistant","content":"two"}),
        json!({"role":"user","content":"three"}),
        json!({"role":"assistant","content":"four"}),
        json!({"role":"user","content":"five"}),
    ];
    let mut body = anthropic_body("claude-fixture", conversation.clone(), &tools).unwrap();
    cache_anthropic_prefix(&mut body);
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");
    let marked: Vec<_> = body["messages"].as_array().unwrap().iter().map(|m| m["content"][0].get("cache_control").is_some()).collect();
    assert_eq!(marked, [false, false, true, false, true]);

    let mut routed = json!({"model":"anthropic/claude-fixture","messages":conversation});
    cache_openrouter_prefix(&"https://openrouter.ai/api/v1/chat/completions".parse().unwrap(), &mut routed);
    assert_eq!(routed["messages"][0]["content"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(routed["messages"][5]["content"][0]["text"], "five");
    let plain = json!({"model":"anthropic/claude-fixture","messages":conversation});
    let mut other = plain.clone();
    cache_openrouter_prefix(&"https://example.com/v1/chat/completions".parse().unwrap(), &mut other);
    assert_eq!(other, plain);
}

#[test]
fn responses_calls_meet_the_same_contract_as_other_protocols() {
    let call = r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"c1","name":"read_file","arguments":"{}"}}"#;
    let completed = r#"{"type":"response.completed","response":{}}"#;
    let finish = |events: &[&str]| {
        let mut stream = ResponsesStream::default();
        let mut emit = |_: &str| Ok(());
        for event in events {
            stream.accept(event, &mut emit).unwrap();
        }
        stream.finish()
    };
    assert!(finish(&[call, completed]).is_ok());
    assert!(finish(&[call, call, completed]).is_err(), "duplicate call IDs");
    assert!(finish(&[completed]).is_err(), "no text and no calls");
    let unnamed = r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"c1","name":"","arguments":"{}"}}"#;
    assert!(finish(&[unnamed, completed]).is_err());
    let broken = r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"c1","name":"read_file","arguments":"{"}}"#;
    assert!(finish(&[broken, completed]).is_err());
}

#[test]
fn a_responses_refusal_is_shown_as_the_answer() {
    let mut stream = ResponsesStream::default();
    let mut shown = String::new();
    let mut emit = |text: &str| { shown.push_str(text); Ok(()) };
    for event in [r#"{"type":"response.refusal.delta","delta":"I can't help with that."}"#, r#"{"type":"response.completed","response":{}}"#] {
        stream.accept(event, &mut emit).unwrap();
    }
    assert!(stream.finish().is_ok(), "a refusal is a usable answer");
    assert_eq!(shown, "I can't help with that.");
}

#[tokio::test]
async fn a_managed_sign_in_is_renewed_before_every_model_request() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = seen.clone();
    let router = Router::new().route("/v1/chat/completions", post(move |headers: HeaderMap| {
        let recorded = recorded.clone();
        async move {
            recorded.lock().unwrap().push(headers["authorization"].to_str().unwrap().to_owned());
            sse(OPENAI_TEXT)
        }
    }));
    let (base, server) = serve(router).await;
    let renewals = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = renewals.clone();
    let renew: Renew = Arc::new(move || {
        let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
        Box::pin(async move { Ok(format!("token-{n}")) })
    });
    let provider = Provider::new(config(format!("{base}/v1"))).unwrap().with_renewal(Some(renew));
    for _ in 0..2 {
        provider.stream_with_tools(user_messages(), &[], |_| Ok(())).await.unwrap();
    }
    assert_eq!(*seen.lock().unwrap(), ["Bearer token-1", "Bearer token-2"]);
    server.abort();
}

#[test]
fn work_started_from_a_managed_turn_resolves_its_sign_in_again() {
    let account = crate::accounts::CATALOG.iter().find(|a| crate::accounts::for_base_url(a.base_url).is_some()).unwrap();
    let original = Config { base_url: account.base_url.into(), model: "m".into(), api_key: String::new() };
    let resolved = Config { api_key: "resolved-fixture-token".into(), ..original.clone() };
    let provider = Provider::new(resolved.clone()).unwrap().renewing(&original);
    assert!(provider.turn_config().api_key.is_empty(), "a delegated child renews the sign-in, not the parent's token");
    let explicit = Provider::new(resolved.clone()).unwrap().renewing(&resolved);
    assert_eq!(explicit.turn_config().api_key, "resolved-fixture-token", "an explicit key is passed on");
}
