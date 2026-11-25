//! Read-only model discovery and supported request options for native providers.
use crate::{Config, accounts, portal, provider::Provider};
use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub context_length: Option<u64>,
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub supported_parameters: Vec<String>,
    #[serde(default)]
    pub free: bool,
}
impl Model {
    pub fn supports(&self, parameter: &str) -> bool {
        self.supported_parameters
            .iter()
            .any(|name| name == parameter)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationOptions {
    pub temperature: Option<f64>,
    pub max_output_tokens: Option<u32>,
    pub reasoning_effort: Option<String>,
    #[serde(default)] pub fast:bool,
    #[serde(default)]
    pub reasoning_object: bool,
    #[serde(default)]
    pub max_completion_tokens: bool,
}
impl GenerationOptions {
    pub fn validate(&self) -> Result<()> {
        if let Some(value) = self.temperature {
            ensure!(
                value.is_finite() && (0.0..=2.0).contains(&value),
                "Temperature must be between 0 and 2"
            );
        }
        if let Some(value) = self.max_output_tokens {
            ensure!(
                (1..=1_048_576).contains(&value),
                "Maximum output tokens must be between 1 and 1,048,576"
            );
        }
        if let Some(value) = &self.reasoning_effort {
            ensure!(
                ["none", "minimal", "low", "medium", "high", "xhigh","max","ultra"].contains(&value.as_str()),
                "Unsupported reasoning effort"
            );
        }
        Ok(())
    }
    pub(crate) fn apply(&self, body: &mut Value, anthropic: bool) -> Result<()> {
        self.validate()?;
        if self.fast && !anthropic{body["service_tier"]=json!("priority");}
        if let Some(value) = self.temperature {
            if anthropic {
                ensure!(
                    value <= 1.0,
                    "Anthropic temperature must be between 0 and 1"
                );
            }
            body["temperature"] = json!(value);
        }
        if let Some(value) = self.max_output_tokens {
            body[if !anthropic && self.max_completion_tokens {
                "max_completion_tokens"
            } else {
                "max_tokens"
            }] = json!(value);
        }
        if let Some(value) = &self.reasoning_effort {
            ensure!(
                !anthropic,
                "Native Anthropic reasoning configuration is not supported by this model control"
            );
            let value=if value=="ultra"{"max"}else{value.as_str()};
            if self.reasoning_object && value=="none"{body["reasoning"]=json!({"enabled":false});}
            else if self.reasoning_object {
                body["reasoning"] = json!({"effort":value});
            } else {
                body["reasoning_effort"] = json!(value);
            }
        }
        Ok(())
    }

    pub(crate) fn apply_responses_reasoning(&self, body: &mut Value) -> Result<()> {
        self.validate()?;
        if let Some(effort) = &self.reasoning_effort {
            let effort = if effort == "ultra" { "max" } else { effort };
            body["reasoning"] = json!({"effort": effort});
        }
        Ok(())
    }
}

/// GET /models only: selecting or inspecting a model never invokes inference.
pub async fn discover(config: Config) -> Result<Vec<Model>> {
    let config = catalog_config(config, accounts::access_token).await?;
    let provider = Provider::new_with_options(config, GenerationOptions::default())?;
    discover_catalog(provider.catalog_requests()?).await
}

async fn catalog_config<F, Fut>(config: Config, token: F) -> Result<Config>
where
    F: FnOnce(&'static str) -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    let mut config = config;
    if config.model.trim().is_empty() {
        config.model = "catalog-discovery".into();
    }
    config.validate()?;
    accounts::resolve_with(portal::catalog_credentials(config).await?, token).await
}

async fn discover_catalog(requests: Vec<reqwest::RequestBuilder>) -> Result<Vec<Model>> {
    for request in requests {
        let models = read_catalog(request).await?;
        if !models.is_empty() {
            return Ok(models);
        }
    }
    anyhow::bail!("Provider returned an empty model catalog")
}

async fn read_catalog(request: reqwest::RequestBuilder) -> Result<Vec<Model>> {
    let mut response = request
        .send()
        .await
        .map_err(|_| anyhow!("Could not reach the provider model catalog"))?;
    ensure!(
        response.status().is_success(),
        "Model discovery returned HTTP {}; check this provider's connection and credentials",
        response.status()
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow!("Model catalog download was interrupted"))?
    {
        ensure!(
            bytes.len() + chunk.len() <= 16 * 1_048_576,
            "Model catalog exceeded the maximum size"
        );
        bytes.extend_from_slice(&chunk);
    }
    let value: Value =
        serde_json::from_slice(&bytes).context("Provider returned an invalid model catalog")?;
    let entries = value
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| value.get("models").and_then(Value::as_array))
        .context("Provider returned no model list")?;
    let mut models = Vec::new();
    for entry in entries.iter().take(20_000) {
        let Some(id) = entry
            .get("id")
            .or_else(|| entry.get("slug"))
            .or_else(|| entry.get("name"))
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty() && id.len() <= 1024)
        else {
            continue;
        };
        if entry.get("visibility").and_then(Value::as_str).is_some_and(|value| {
            value.eq_ignore_ascii_case("hide") || value.eq_ignore_ascii_case("hidden")
        }) {
            continue;
        }
        let mut supported_parameters: Vec<String> = entry
            .get("supported_parameters")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|name| name.len() < 128)
            .map(str::to_owned)
            .collect();
        if entry.get("supported_reasoning_levels").and_then(Value::as_array)
            .is_some_and(|levels| levels.iter().any(|level| level["effort"].is_string()))
            && !supported_parameters.iter().any(|parameter| parameter == "reasoning")
        {
            supported_parameters.push("reasoning".into());
        }
        let zero = |key: &str| {
            entry
                .pointer(&format!("/pricing/{key}"))
                .and_then(|v| {
                    v.as_str()
                        .and_then(|v| v.parse::<f64>().ok())
                        .or_else(|| v.as_f64())
                })
                .is_some_and(|value| value == 0.0)
        };
        models.push(Model {
            id: id.into(),
            name: entry
                .get("display_name")
                .or_else(|| entry.get("name"))
                .and_then(Value::as_str)
                .filter(|name| name.len() < 2048)
                .unwrap_or(id)
                .into(),
            context_length: entry
                .get("context_length")
                .or_else(|| entry.get("max_input_tokens"))
                .or_else(|| entry.get("context_window"))
                .and_then(Value::as_u64),
            max_output_tokens: entry
                .pointer("/top_provider/max_completion_tokens")
                .or_else(|| entry.get("max_output_tokens"))
                .and_then(Value::as_u64),
            supported_parameters,
            free: zero("prompt") && zero("completion"),
        });
    }
    models.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
    models.dedup_by(|a, b| a.id == b.id);
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn managed_catalog_configuration_resolves_account_tokens() {
        for account in accounts::CATALOG.iter().filter(|account| accounts::for_base_url(account.base_url).is_some()) {
            let config = catalog_config(Config {
                base_url: account.base_url.into(), model: String::new(), api_key: String::new(),
            }, |selected| {
                assert_eq!(selected, account.id);
                std::future::ready(Ok("catalog-fixture-token".into()))
            }).await.unwrap();
            let provider = Provider::new_with_options(config, GenerationOptions::default()).unwrap();
            for request in provider.catalog_requests().unwrap() {
                let request = request.build().unwrap();
                assert_eq!(request.headers()["authorization"], "Bearer catalog-fixture-token");
                assert_eq!(request.method(), reqwest::Method::GET);
                assert!(request.body().is_none());
            }
        }
    }

    #[tokio::test]
    async fn catalog_discovery_reads_native_rows_without_running_inference() {
        use axum::{Json, Router, http::HeaderMap, routing::get};

        let app = Router::new().route("/v1/models", get(|headers: HeaderMap| async move {
            assert_eq!(headers["authorization"], "Bearer catalog-fixture-token");
            Json(json!({"models":[
                {"slug":"subscription-fixture-model", "display_name":"Subscription fixture",
                 "context_window":262144, "supported_reasoning_levels":[{"effort":"low"},{"effort":"high"}]},
                {"slug":"hidden-fixture-model", "visibility":"hide"},
                {"id":"compatible-fixture-model", "name":"Compatible fixture", "context_length":8192,
                 "supported_parameters":["temperature"], "pricing":{"prompt":"0","completion":"0"}}
            ]}))
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let catalog = discover(Config {
            base_url: format!("http://{address}/v1/chat/completions"),
            model: String::new(),
            api_key: "catalog-fixture-token".into(),
        }).await.unwrap();
        assert_eq!(catalog.len(), 2);
        let native = catalog.iter().find(|model| model.id == "subscription-fixture-model").unwrap();
        assert_eq!(native.name, "Subscription fixture");
        assert_eq!(native.context_length, Some(262144));
        assert!(native.supports("reasoning"));
        let compatible = catalog.iter().find(|model| model.id == "compatible-fixture-model").unwrap();
        assert!(compatible.supports("temperature"));
        assert!(compatible.free);
        server.abort();
    }

    #[tokio::test]
    async fn catalog_compatibility_fallback_stays_on_the_selected_route() {
        use axum::{Json, Router, extract::Query, http::HeaderMap, routing::get};
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

        let seen = Arc::new(AtomicUsize::new(0));
        let calls = seen.clone();
        let app = Router::new().route("/subscription/models", get(move |headers: HeaderMap, Query(query): Query<std::collections::HashMap<String, String>>| {
            let calls = calls.clone();
            async move {
                assert_eq!(headers["authorization"], "Bearer catalog-fixture-token");
                let index = calls.fetch_add(1, Ordering::SeqCst);
                assert_eq!(query["client_version"], if index == 0 { "99.0.0" } else { "0.0.0" });
                Json(if index == 0 { json!({"models":[]}) } else { json!({"models":[{"slug":"fallback-fixture-model"}]}) })
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let client = reqwest::Client::new();
        let requests = ["99.0.0", "0.0.0"].iter().map(|version| {
            client.get(format!("http://{address}/subscription/models"))
                .query(&[("client_version", version)])
                .bearer_auth("catalog-fixture-token")
        }).collect();
        let catalog = discover_catalog(requests).await.unwrap();
        assert_eq!(catalog[0].id, "fallback-fixture-model");
        assert_eq!(seen.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn catalog_failures_do_not_retry_the_compatibility_request() {
        use axum::{Router, extract::Query, http::StatusCode, routing::get};
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

        let seen = Arc::new(AtomicUsize::new(0));
        let calls = seen.clone();
        let app = Router::new().route("/models", get(move |Query(query): Query<std::collections::HashMap<String, u16>>| {
            let calls = calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                (StatusCode::from_u16(query["status"]).unwrap(), "fixture failure")
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let client = reqwest::Client::new();
        for status in [401, 429, 503] {
            seen.store(0, Ordering::SeqCst);
            let requests = vec![
                client.get(format!("http://{address}/models?status={status}")),
                client.get(format!("http://{address}/models?status=200")),
            ];
            let error = discover_catalog(requests).await.unwrap_err().to_string();
            assert!(error.contains(&format!("HTTP {status}")));
            assert_eq!(seen.load(Ordering::SeqCst), 1);
        }
        // A framing failure also must not turn into another catalog request.
        seen.store(0, Ordering::SeqCst);
        let requests = vec![
            client.get(format!("http://{address}/models?status=200")),
            client.get(format!("http://{address}/models?status=200")),
        ];
        assert!(discover_catalog(requests).await.is_err());
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        server.abort();
    }

    fn body(options: GenerationOptions) -> Value {
        let mut body = json!({"model": "m"});
        options.apply(&mut body, false).unwrap();
        body
    }
    #[test]
    fn picked_effort_reaches_the_request() {
        let high = GenerationOptions { reasoning_effort: Some("high".into()), ..Default::default() };
        assert_eq!(body(high.clone())["reasoning_effort"], "high");
        assert_eq!(body(GenerationOptions { reasoning_object: true, ..high })["reasoning"], json!({"effort": "high"}));
        // Ultra is clamped to the strongest wire level, and Thinking off disables reasoning.
        assert_eq!(body(GenerationOptions { reasoning_effort: Some("ultra".into()), ..Default::default() })["reasoning_effort"], "max");
        assert_eq!(body(GenerationOptions { reasoning_effort: Some("none".into()), reasoning_object: true, ..Default::default() })["reasoning"], json!({"enabled": false}));
        assert!(body(GenerationOptions::default()).get("reasoning_effort").is_none());
    }
}
