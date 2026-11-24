use super::*;
use crate::{Config, credentials::EndpointStore, settings::ProfileSettings};
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
async fn profile_inference_uses_distinct_saved_chat_and_embedding_endpoints() {
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
    let embedding = endpoints
        .save(
            None,
            "Embedding fixture",
            &Config {
                base_url: format!("{base}/embed/v1"),
                model: "embedding-fixture".into(),
                api_key: "embedding-secret".into(),
            },
        )
        .unwrap();
    let settings = json!({"memory_embedding": {
        "endpoint_id": embedding.id,
        "dimensions": 3
    }});
    ProfileSettings::update(profile.path(), settings.as_object().unwrap()).unwrap();
    let chat = Config {
        base_url: format!("{base}/chat/v1"),
        model: "chat-fixture".into(),
        api_key: "chat-secret".into(),
    };

    let inference = ProfileInference::open(profile.path(), &chat)
        .await
        .unwrap()
        .unwrap();
    let parsed = inference
        .complete_json(
            "Return the requested object.",
            "A synthetic fixture request.",
            "fixture_result",
            &json!({"type":"object","properties":{"accepted":{"type":"boolean"}},"required":["accepted"],"additionalProperties":false}),
        )
        .await
        .unwrap();
    assert_eq!(parsed, json!({"accepted":true}));
    let vectors = inference
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
    assert_eq!(headers["authorization"], "Bearer chat-secret");
    assert_eq!(
        headers["user-agent"],
        concat!("HermesRust/", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(body["model"], "chat-fixture");
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
        inference
            .embed(&["bounded synthetic input".into()], 16)
            .await
            .is_err()
    );
    server.abort();
}

#[tokio::test]
async fn profile_without_embedding_selection_returns_none_without_network_requests() {
    let (request_tx, request_rx) = mpsc::channel();
    let router = Router::new().fallback(move || {
        let request_tx = request_tx.clone();
        async move {
            request_tx.send(()).unwrap();
            Json(json!({"unexpected":true}))
        }
    });
    let (base, server) = serve(router).await;
    let profile = tempfile::tempdir().unwrap();
    let chat = Config {
        base_url: format!("{base}/v1"),
        model: "chat-fixture".into(),
        api_key: "chat-secret".into(),
    };

    assert!(
        ProfileInference::open(profile.path(), &chat)
            .await
            .unwrap()
            .is_none()
    );
    assert!(request_rx.try_recv().is_err());
    server.abort();
}
