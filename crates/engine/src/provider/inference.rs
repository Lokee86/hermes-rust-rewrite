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
                self.options.apply_responses_reasoning(&mut body)?;
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

    pub(crate) async fn complete_json_with_tool(
        &self,
        system: &str,
        input: &str,
        name: &str,
        schema: &Value,
        tool: crate::inference::StructuredTool<'_>,
        run: &mut dyn FnMut(Value) -> Result<Value>,
    ) -> Result<Value> {
        ensure!(
            !tool.name.is_empty() && tool.max_results > 0 && tool.max_input_bytes > 0,
            "Invalid structured tool limits"
        );
        let model = self.config.model.trim();
        let messages = vec![
            json!({"role":"system","content":system}),
            json!({"role":"user","content":input}),
        ];
        let local = json!({"type":"function","function":{
            "name":tool.name,"description":"Measure the exact candidate text with the local tokenizer.","parameters":tool.schema
        }});
        let mut body = match self.protocol {
            Protocol::OpenAi => {
                let mut body = json!({"model":model,"messages":messages,"tools":[local],
                    "parallel_tool_calls":false,"response_format":{"type":"json_schema","json_schema":{
                        "name":name,"strict":true,"schema":schema}}});
                self.options.apply(&mut body, false)?;
                body
            }
            Protocol::Anthropic => {
                let mut body = anthropic_body(model, messages, &[local])?;
                body["stream"] = json!(false);
                body["tool_choice"] = json!({"type":"auto","disable_parallel_tool_use":true});
                body["output_config"] = json!({"format":{"type":"json_schema","schema":schema}});
                self.options.apply(&mut body, true)?;
                body
            }
            Protocol::Responses => {
                let mut body = responses_body(model, messages, &[local]);
                body["parallel_tool_calls"] = json!(false);
                body["include"] = json!(["reasoning.encrypted_content"]);
                body["text"] = json!({"format":{"type":"json_schema","name":name,"strict":true,"schema":schema}});
                self.options.apply_responses_reasoning(&mut body)?;
                body
            }
        };
        let mut ids = std::collections::BTreeSet::new();
        for iteration in 0..=tool.max_results {
            ensure!(
                body.to_string().len() <= tool.max_input_bytes,
                "Structured tool request exceeds its limit"
            );
            let step = match self.protocol {
                Protocol::OpenAi => {
                    let reply = self
                        .request_json(&self.endpoint, &body, RESPONSE_BYTES)
                        .await?;
                    let choice = &reply["choices"][0];
                    let message = &choice["message"];
                    let calls = message["tool_calls"]
                        .as_array()
                        .map_or(&[][..], Vec::as_slice);
                    let text = message["content"].as_str().unwrap_or_default();
                    if calls.is_empty() {
                        ensure!(
                            choice["finish_reason"]
                                .as_str()
                                .is_none_or(|why| why == "stop"),
                            "Provider did not finish structured output"
                        );
                        return serde_json::from_str(text)
                            .context("Invalid terminal tool-job JSON");
                    }
                    ensure!(
                        calls.len() == 1 && text.trim().is_empty(),
                        "Unexpected structured tool calls"
                    );
                    let call = &calls[0];
                    let function = &call["function"];
                    (
                        call["id"].as_str().unwrap_or_default().to_owned(),
                        function["name"].as_str().unwrap_or_default().to_owned(),
                        serde_json::from_str(function["arguments"].as_str().unwrap_or_default())
                            .context("Invalid local tool arguments")?,
                        vec![
                            json!({"role":"assistant","content":message["content"],"tool_calls":calls}),
                        ],
                    )
                }
                Protocol::Anthropic => {
                    let reply = self
                        .request_json(&self.endpoint, &body, RESPONSE_BYTES)
                        .await?;
                    let blocks = reply["content"]
                        .as_array()
                        .context("Missing structured tool response")?;
                    let mut calls = blocks.iter().filter(|block| block["type"] == "tool_use");
                    let Some(call) = calls.next() else {
                        ensure!(
                            reply["stop_reason"] == "end_turn",
                            "Provider did not finish structured output"
                        );
                        let text = blocks
                            .iter()
                            .filter(|block| block["type"] == "text")
                            .filter_map(|block| block["text"].as_str())
                            .collect::<String>();
                        return serde_json::from_str(&text)
                            .context("Invalid terminal tool-job JSON");
                    };
                    ensure!(
                        reply["stop_reason"] == "tool_use" && calls.next().is_none(),
                        "Unexpected structured tool calls"
                    );
                    (
                        call["id"].as_str().unwrap_or_default().to_owned(),
                        call["name"].as_str().unwrap_or_default().to_owned(),
                        call["input"].clone(),
                        vec![json!({"role":"assistant","content":blocks})],
                    )
                }
                Protocol::Responses => {
                    let req = self
                        .post(&self.endpoint, &body, "text/event-stream")
                        .timeout(INFERENCE_TIMEOUT);
                    let mut text = String::new();
                    let output = self
                        .read_stream_with_items(
                            Self::send_request(req).await?,
                            &mut |delta| {
                                text.push_str(delta);
                                Ok(())
                            },
                            true,
                        )
                        .await?;
                    let mut calls = output
                        .response_items
                        .iter()
                        .filter(|item| item["type"] == "function_call");
                    let Some(call) = calls.next() else {
                        ensure!(output.calls.is_empty(), "Missing structured tool call item");
                        return serde_json::from_str(&text)
                            .context("Invalid terminal tool-job JSON");
                    };
                    ensure!(
                        calls.next().is_none() && text.trim().is_empty(),
                        "Unexpected structured tool calls"
                    );
                    let id = call["call_id"].as_str().unwrap_or_default().to_owned();
                    let function = call["name"].as_str().unwrap_or_default().to_owned();
                    let arguments =
                        serde_json::from_str(call["arguments"].as_str().unwrap_or_default())
                            .context("Invalid local tool arguments")?;
                    (id, function, arguments, output.response_items)
                }
            };
            let (id, function, arguments, assistant) = step;
            ensure!(
                iteration < tool.max_results
                    && function == tool.name
                    && !id.is_empty()
                    && id.len() <= 256
                    && ids.insert(id.clone())
                    && arguments.is_object(),
                "Unexpected, repeated, or exhausted local tool call"
            );
            let result = run(arguments)?.to_string();
            let (field, continuation) = match self.protocol {
                Protocol::OpenAi => (
                    "messages",
                    json!({"role":"tool","tool_call_id":id,"content":result}),
                ),
                Protocol::Anthropic => (
                    "messages",
                    json!({"role":"user","content":[{
                    "type":"tool_result","tool_use_id":id,"content":result}]}),
                ),
                Protocol::Responses => (
                    "input",
                    json!({"type":"function_call_output","call_id":id,"output":result}),
                ),
            };
            let turns = body[field]
                .as_array_mut()
                .context("Invalid structured tool transcript")?;
            turns.extend(assistant);
            turns.push(continuation);
        }
        unreachable!("the last structured turn cannot request another tool response")
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
