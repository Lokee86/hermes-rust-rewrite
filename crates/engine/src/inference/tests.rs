use super::*;
use crate::{Config, credentials::EndpointStore};
use axum::{Json, Router, http::HeaderMap, routing::post};
use serde_json::{Value, json};
use std::sync::mpsc;

async fn serve(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{address}"), task)
}

#[tokio::test]
async fn independently_selected_targets_use_exact_saved_credentials_and_model_overrides() {
    let (chat_tx, chat_rx) = mpsc::channel();
    let (embed_tx, embed_rx) = mpsc::channel();
    let router = Router::new()
        .route(
            "/chat/v1/chat/completions",
            post(
                move |headers: HeaderMap, Json(body): Json<Value>| async move {
                    chat_tx.send((headers, body)).unwrap();
                    Json(json!({"choices":[{"message":{"content":"{\"accepted\":true}"}}]}))
                },
            ),
        )
        .route(
            "/embed/v1/embeddings",
            post(
                move |headers: HeaderMap, Json(body): Json<Value>| async move {
                    embed_tx.send((headers, body)).unwrap();
                    Json(json!({"model":"embedding-fixture","data":[
                        {"index":1,"embedding":[4.0,5.0,6.0]},
                        {"index":0,"embedding":[1.0,2.0,3.0]}
                    ]}))
                },
            ),
        );
    let (base, server) = serve(router).await;
    let profile = tempfile::tempdir().unwrap();
    let endpoints = EndpointStore::open(&profile.path().join("providers")).unwrap();
    endpoints
        .save(
            Some("other-at-same-scope"),
            "Different account at same scope",
            &Config {
                base_url: format!("{base}/chat/v1"),
                model: "other-saved-model".into(),
                api_key: "other-secret".into(),
            },
        )
        .unwrap();
    let completion = endpoints
        .save(
            Some("completion"),
            "Completion fixture",
            &Config {
                base_url: format!("{base}/chat/v1"),
                model: "saved-completion-model".into(),
                api_key: "completion-secret".into(),
            },
        )
        .unwrap();
    let embedding = endpoints
        .save(
            Some("embedding"),
            "Embedding fixture",
            &Config {
                base_url: format!("{base}/embed/v1"),
                model: "saved-embedding-model".into(),
                api_key: "embedding-secret".into(),
            },
        )
        .unwrap();
    let chat = Config {
        base_url: format!("{base}/unexpected/v1"),
        model: "chat-fixture".into(),
        api_key: "chat-secret".into(),
    };

    let completion_target = InferenceTarget::parse(&format!("endpoint:{}", completion.id)).unwrap();
    let embedding_target = InferenceTarget::parse(&format!("endpoint:{}", embedding.id)).unwrap();
    let completion = CompletionInference::open(
        profile.path(),
        &chat,
        &completion_target,
        Some("completion-override"),
        Some("low"),
    )
    .await
    .unwrap();
    assert_eq!(completion.route().model, "completion-override");
    assert_eq!(completion.route().provider, format!("{base}/chat/v1"));
    let embedding = EmbeddingInference::open(
        profile.path(),
        &embedding_target,
        Some("embedding-fixture"),
        3,
    )
    .await
    .unwrap();
    assert_eq!(embedding.route().model, "embedding-fixture");
    assert_eq!(embedding.route().provider, format!("{base}/embed/v1"));
    assert_eq!(embedding.dimensions(), 3);
    assert!(chat_rx.try_recv().is_err());
    assert!(embed_rx.try_recv().is_err());

    let other_profile = tempfile::tempdir().unwrap();
    assert!(
        CompletionInference::open(other_profile.path(), &chat, &completion_target, None, None)
            .await
            .is_err()
    );
    let parsed = completion
        .complete_json(
            "Return the requested object.",
            "A synthetic fixture request.",
            "fixture_result",
            &json!({"type":"object","properties":{"accepted":{"type":"boolean"}},"required":["accepted"],"additionalProperties":false}),
        )
        .await
        .unwrap();
    assert_eq!(parsed, json!({"accepted":true}));
    let vectors = embedding
        .embed(
            &[
                "first synthetic input".into(),
                "second synthetic input".into(),
            ],
            4096,
        )
        .await
        .unwrap();
    assert_eq!(vectors, vec![vec![1.0, 2.0, 3.0], vec![4.0, 5.0, 6.0]]);

    let (headers, body) = chat_rx.recv().unwrap();
    assert_eq!(headers["authorization"], "Bearer completion-secret");
    assert_eq!(
        headers["user-agent"],
        concat!("HermesRust/", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(body["model"], "completion-override");
    assert_eq!(body["reasoning_effort"], "low");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(
        body["messages"][0]["content"],
        "Return the requested object."
    );
    assert_eq!(
        body["response_format"]["json_schema"]["name"],
        "fixture_result"
    );

    let (headers, body) = embed_rx.recv().unwrap();
    assert_eq!(headers["authorization"], "Bearer embedding-secret");
    assert_eq!(
        headers["user-agent"],
        concat!("HermesRust/", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(body["model"], "embedding-fixture");
    assert_eq!(
        body["input"],
        json!(["first synthetic input", "second synthetic input"])
    );
    assert_eq!(body["dimensions"], 3);

    assert!(
        embedding
            .embed(&["bounded synthetic input".into()], 16)
            .await
            .is_err()
    );
    server.abort();
}

#[tokio::test]
async fn chat_and_managed_account_targets_use_hermes_defaults_without_auth_at_bind() {
    let (tx, rx) = mpsc::channel();
    let router = Router::new().route(
        "/v1/chat/completions",
        post(
            move |headers: HeaderMap, Json(body): Json<Value>| async move {
                tx.send((headers, body)).unwrap();
                Json(json!({"choices":[{"message":{"content":"{}"}}]}))
            },
        ),
    );
    let (base, server) = serve(router).await;
    let profile = tempfile::tempdir().unwrap();
    let chat = Config {
        base_url: format!("{base}/v1"),
        model: "chat-default".into(),
        api_key: "chat-fixture-key".into(),
    };
    let selected = InferenceTarget::parse("chat").unwrap();
    let completion = CompletionInference::open(profile.path(), &chat, &selected, None, None)
        .await
        .unwrap();
    assert_eq!(completion.route().model, "chat-default");
    assert!(rx.try_recv().is_err());
    completion
        .complete_json("system", "input", "result", &json!({"type":"object"}))
        .await
        .unwrap();
    let (headers, body) = rx.recv().unwrap();
    assert_eq!(headers["authorization"], "Bearer chat-fixture-key");
    assert_eq!(body["model"], "chat-default");
    assert!(body.get("reasoning_effort").is_none());
    assert!(body.get("reasoning").is_none());

    assert!(
        EmbeddingInference::open(profile.path(), &selected, None, 3)
            .await
            .is_err()
    );
    for id in ["openai-codex", "qwen-oauth", "minimax-oauth", "xai-oauth"] {
        let key = format!("account:{id}");
        let account = InferenceTarget::parse(&key).unwrap();
        assert_eq!(account.key(), key);
        let expected = crate::accounts::provider(id).unwrap();
        let completion = CompletionInference::open(profile.path(), &chat, &account, None, None)
            .await
            .unwrap();
        assert_eq!(completion.route().model, expected.default_model);
        assert_eq!(completion.route().provider, expected.base_url);
    }
    let codex = InferenceTarget::parse("account:openai-codex").unwrap();
    assert!(
        EmbeddingInference::open(profile.path(), &codex, Some("not-embeddings"), 3)
            .await
            .is_err()
    );
    assert_eq!(
        InferenceTarget::parse("account:nous").unwrap().key(),
        "account:nous"
    );
    for invalid in [
        "account:anthropic",
        "account:claude-code",
        "account:unknown",
        "endpoint:",
        "chat:",
    ] {
        assert!(InferenceTarget::parse(invalid).is_err());
    }
    server.abort();
}
