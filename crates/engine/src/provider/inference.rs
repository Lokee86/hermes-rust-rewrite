use super::{Protocol, Provider, Url, Value, anthropic_body, json, responses_body};
use anyhow::{Context, Result, ensure};
use std::time::Duration;

const RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const INFERENCE_TIMEOUT: Duration = Duration::from_secs(60);

impl Provider {
    pub(crate) async fn resolve_for_inference(&self) -> Result<Option<Self>> {
        if !self.config.api_key.trim().is_empty()
            || !(crate::portal::is_portal(&self.config.base_url)
                || crate::accounts::for_base_url(&self.config.base_url).is_some())
        {
            return Ok(None);
        }
        let config = Self::resolve_config(self.config.clone()).await?;
        Ok(Some(Self::new_with_options(config, self.options.clone())?))
    }

    pub(crate) fn validate_structured_options(&self) -> Result<()> {
        self.options
            .apply(&mut json!({}), self.protocol == Protocol::Anthropic)
    }

    pub(crate) async fn complete_json(
        &self,
        system: &str,
        input: &str,
        name: &str,
        schema: &Value,
    ) -> Result<Value> {
        let model = self.config.model.trim();
        let messages = vec![
            json!({"role":"system","content":system}),
            json!({"role":"user","content":input}),
        ];
        let content = match self.protocol {
            Protocol::OpenAi => {
                let mut body = json!({
                    "model":model, "messages":messages,
                    "response_format":{"type":"json_schema","json_schema":{
                        "name":name,"strict":true,"schema":schema
                    }}
                });
                self.options.apply(&mut body, false)?;
                let reply = self
                    .request_json(&self.endpoint, &body, RESPONSE_BYTES)
                    .await?;
                let choice = &reply["choices"][0];
                ensure!(
                    choice["finish_reason"]
                        .as_str()
                        .is_none_or(|reason| reason == "stop"),
                    "Provider did not complete the structured response"
                );
                choice["message"]["content"]
                    .as_str()
                    .context("Missing structured completion content")?
                    .to_owned()
            }
            Protocol::Anthropic => {
                let mut body = anthropic_body(model, messages, &[])?;
                body["stream"] = json!(false);
                body["output_config"] = json!({"format":{"type":"json_schema","schema":schema}});
                self.options.apply(&mut body, true)?;
                let reply = self
                    .request_json(&self.endpoint, &body, RESPONSE_BYTES)
                    .await?;
                ensure!(
                    reply["stop_reason"] == "end_turn",
                    "Provider did not complete the structured response"
                );
                reply["content"]
                    .as_array()
                    .context("Missing structured completion content")?
                    .iter()
                    .filter(|block| block["type"] == "text")
                    .filter_map(|block| block["text"].as_str())
                    .collect::<String>()
            }
            Protocol::Responses => {
                let mut body = responses_body(model, messages, &[]);
                if let Some(effort) = &self.options.reasoning_effort {
                    let effort = if effort == "ultra" { "max" } else { effort };
                    body["reasoning"] = json!({"effort":effort});
                }
                body["text"] = json!({"format":{"type":"json_schema", "name":name,
                    "strict":true, "schema":schema}});
                let request = self
                    .post(&self.endpoint, &body, "text/event-stream")
                    .timeout(INFERENCE_TIMEOUT);
                let mut content = String::new();
                let output = self
                    .read_stream(Self::send_request(request).await?, &mut |delta| {
                        content.push_str(delta);
                        Ok(())
                    })
                    .await?;
                ensure!(
                    output.calls.is_empty(),
                    "Unexpected structured completion tool call"
                );
                content
            }
        };
        serde_json::from_str(&content).context("Invalid structured completion JSON")
    }

    pub(crate) fn embedding_url(&self) -> Result<Url> {
        ensure!(
            self.protocol == Protocol::OpenAi,
            "Select an OpenAI-compatible embedding endpoint"
        );
        let mut url = self.endpoint.clone();
        let prefix = url
            .path()
            .strip_suffix("/chat/completions")
            .context("Invalid embedding endpoint")?;
        url.set_path(&format!("{prefix}/embeddings"));
        Ok(url)
    }

    pub(crate) async fn embed(
        &self,
        endpoint: &Url,
        dimensions: u32,
        inputs: &[String],
        max_response_bytes: usize,
    ) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        ensure!(
            inputs.len() <= 128 && inputs.iter().map(String::len).sum::<usize>() <= 1024 * 1024,
            "Embedding batch exceeds bounds"
        );
        let model = self.config.model.trim();
        let body = json!({
            "model":model, "input":inputs, "dimensions":dimensions, "encoding_format":"float"
        });
        let reply = self
            .request_json(endpoint, &body, max_response_bytes)
            .await?;
        ensure!(
            reply["model"].as_str().is_none_or(|value| value == model),
            "Embedding model differs from the selected route"
        );
        let data = reply["data"].as_array().context("Missing embedding rows")?;
        ensure!(data.len() == inputs.len(), "Unexpected embedding row count");
        let mut rows = vec![None; inputs.len()];
        for item in data {
            let index = item["index"]
                .as_u64()
                .and_then(|i| usize::try_from(i).ok())
                .context("Invalid embedding row index")?;
            let slot = rows
                .get_mut(index)
                .context("Embedding row index out of bounds")?;
            let vector: Vec<f32> = serde_json::from_value(item["embedding"].clone())
                .context("Invalid embedding vector")?;
            ensure!(
                slot.is_none()
                    && vector.len() == dimensions as usize
                    && vector.iter().all(|n| n.is_finite()),
                "Invalid embedding vector or duplicate row"
            );
            *slot = Some(vector);
        }
        rows.into_iter()
            .map(|row| row.context("Missing embedding row"))
            .collect()
    }

    async fn request_json(&self, endpoint: &Url, body: &Value, max: usize) -> Result<Value> {
        let limit = max.min(RESPONSE_BYTES);
        ensure!(limit > 0, "No provider response budget");
        let request = self
            .post(endpoint, body, "application/json")
            .timeout(INFERENCE_TIMEOUT);
        let mut response = Self::send_request(request).await?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                chunk.len() <= limit - bytes.len(),
                "Provider response exceeded its budget"
            );
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).context("Invalid provider response JSON")
    }
}
