// providers/ollama_native.rs — Native `/api/chat` transport for Ollama.
//
// Clawde's Ollama chat traffic runs over Ollama's native chat endpoint,
// not the OpenAI-compatible `/v1` shim. Rationale (verified against
// Ollama 0.33, 2026-09): `/v1/chat/completions` silently ignores nested
// `options.*` — a 3600-token prompt with `options.num_ctx=512` evaluated
// all 3629 tokens — while `/api/chat` honors every option (`num_ctx`,
// `num_predict`, sampling controls, top-level `keep_alive`). This makes
// the options the `/ollama` screen exposes real instead of stored-only.
//
// Everything that is NOT chat shaping is delegated to the existing
// OpenAI-compat provider (`inner`): native model discovery (`/api/tags`
// + `/api/show` with coder-first sort), health (`/api/tags`), error
// mapping, capabilities, and the transient-404 model-pull recovery
// semantics. This module owns only the chat wire format.
//
// Remote-only invariant (spec §Never-local rule): the host comes from
// the same `resolve_ollama_host` path the compat factory uses, which
// rejects loopback unconditionally. A provider whose host does not
// resolve fails closed against an unroutable address.

use crate::provider::{LlmProvider, ModelInfo};
use crate::provider_error::ProviderError;
use crate::provider_types::{
    ProviderCapabilities, ProviderRequest, ProviderResponse, ProviderStatus, StopReason,
    StreamEvent,
};
use crate::providers::openai::OpenAiProvider;
use crate::providers::openai_compat::OpenAiCompatProvider;
use async_stream::stream;
use async_trait::async_trait;
use clawde_core::provider_id::ProviderId;
use clawde_core::types::{ContentBlock, UsageInfo};
use futures::Stream;
use serde_json::{json, Value};
use std::pin::Pin;

pub struct OllamaNativeProvider {
    id: ProviderId,
    native_base: String,
    inner: OpenAiCompatProvider,
    http_client: reqwest::Client,
}

impl OllamaNativeProvider {
    /// Wrap a configured compat provider. The native base URL is the
    /// compat provider's configured Ollama host (scheme + host + port,
    /// no `/v1` suffix). Panics in tests only; production construction
    /// always goes through [`ollama_native()`].
    pub fn new(inner: OpenAiCompatProvider, native_base: String) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(crate::request_timeout())
            .build()
            .expect("failed to build reqwest client");
        Self {
            id: inner.id().clone(),
            native_base: native_base.trim_end_matches('/').to_string(),
            inner,
            http_client,
        }
    }

    /// Build from settings, mirroring the compat factory's host
    /// resolution and fail-closed behavior. Returns `None` when no valid
    /// remote Ollama host is configured.
    pub fn from_settings() -> Option<Self> {
        ollama_native()
    }

    fn chat_url(&self) -> String {
        format!("{}/api/chat", self.native_base)
    }

    /// Shape the `/api/chat` request body from a normalized request.
    ///
    /// Message conversion reuses the OpenAI wire mapping (same roles,
    /// tool_calls, and `role: tool` result messages — `/api/chat` accepts
    /// that shape) with two native adjustments applied afterwards:
    /// `tool_call_id` → `tool_name`, and `keep_alive` lifted to a
    /// top-level field.
    fn build_body(&self, request: &ProviderRequest, stream: bool) -> Value {
        let mut messages = OpenAiProvider::to_openai_messages_pub(
            &request.messages,
            request.system_prompt.as_ref(),
        );
        // Native `/api/chat` adjustments applied to the OpenAI-shaped
        // messages:
        // 1. Tool results are keyed by tool NAME, not the OpenAI call id.
        // 2. Assistant `tool_calls[].function.arguments` must be a JSON
        //    OBJECT, not OpenAI's string-encoded JSON — the string form
        //    makes Ollama reject the whole request with "can't find
        //    closing '}' symbol" (verified live against 0.33).
        for msg in messages.iter_mut() {
            match msg.get("role").and_then(Value::as_str) {
                Some("tool") => {
                    if let Some(obj) = msg.as_object_mut() {
                        if let Some(id) = obj.remove("tool_call_id") {
                            obj.entry("tool_name".to_string()).or_insert(id);
                        }
                    }
                }
                Some("assistant") => {
                    if let Some(calls) = msg.get_mut("tool_calls").and_then(Value::as_array_mut) {
                        for call in calls {
                            if let Some(args) = call
                                .get_mut("function")
                                .and_then(|f| f.get_mut("arguments"))
                            {
                                if let Some(s) = args.as_str() {
                                    *args = serde_json::from_str(s)
                                        .unwrap_or(Value::Object(Default::default()));
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // Canonical persisted options → native `options` entries. Request
        // fields win over persisted options so effort-derived caps are
        // never silently overridden, except `num_predict` which is an
        // explicit user pin.
        let mut options = serde_json::Map::new();
        if let Some(opts) = request.provider_options.as_object() {
            for (key, value) in opts {
                if key == "keep_alive" {
                    continue; // top-level, applied below
                }
                options.insert(key.clone(), value.clone());
            }
        }
        if let Some(t) = request.temperature {
            options
                .entry("temperature".to_string())
                .or_insert_with(|| json!(t));
        }
        if let Some(p) = request.top_p {
            options
                .entry("top_p".to_string())
                .or_insert_with(|| json!(p));
        }
        if !request.stop_sequences.is_empty() {
            options
                .entry("stop".to_string())
                .or_insert_with(|| json!(request.stop_sequences));
        }
        if request.max_tokens > 0 {
            options
                .entry("num_predict".to_string())
                .or_insert_with(|| json!(request.max_tokens));
        }

        let mut body = json!({
            "model": request.model,
            "messages": messages,
            "stream": stream,
        });
        if !options.is_empty() {
            body["options"] = Value::Object(options);
        }
        if let Some(keep_alive) = request
            .provider_options
            .get("keep_alive")
            .filter(|v| !v.is_null())
        {
            body["keep_alive"] = keep_alive.clone();
        }
        if !request.tools.is_empty() {
            body["tools"] = json!(OpenAiProvider::to_openai_tools_pub(&request.tools));
        }
        body
    }

    async fn dispatch(
        &self,
        request: &ProviderRequest,
    ) -> Result<reqwest::Response, ProviderError> {
        let body = self.build_body(request, false);
        let resp = self
            .http_client
            .post(self.chat_url())
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Other {
                provider: self.id.clone(),
                message: format!("HTTP request failed: {}", e),
                status: None,
                body: None,
            })?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&(status as usize)) {
            let text = resp.text().await.unwrap_or_default();
            return Err(self.inner.map_http_error(status, &text));
        }
        Ok(resp)
    }

    fn parse_response(&self, body: &Value) -> Result<ProviderResponse, ProviderError> {
        let message = body.get("message").cloned().unwrap_or(Value::Null);
        let mut content: Vec<ContentBlock> = Vec::new();
        if let Some(thinking) = message.get("thinking").and_then(Value::as_str) {
            if !thinking.is_empty() {
                content.push(ContentBlock::Thinking {
                    thinking: thinking.to_string(),
                    signature: String::new(),
                });
            }
        }
        if let Some(text) = message.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                content.push(ContentBlock::Text {
                    text: text.to_string(),
                });
            }
        }
        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for (idx, call) in calls.iter().enumerate() {
                let function = call.get("function").cloned().unwrap_or(Value::Null);
                let name = function
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let raw_args = function.get("arguments").cloned().unwrap_or(Value::Null);
                let input = match raw_args {
                    Value::String(s) => {
                        serde_json::from_str(&s).unwrap_or(Value::Object(Default::default()))
                    }
                    other => other,
                };
                content.push(ContentBlock::ToolUse {
                    id: call
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            format!(
                                "ollama_tool_{}",
                                body.get("created_at")
                                    .and_then(Value::as_str)
                                    .unwrap_or("0")
                            ) + &idx.to_string()
                        }),
                    name,
                    input,
                    thought_signature: None,
                });
            }
        }

        let done_reason = body
            .get("done_reason")
            .and_then(Value::as_str)
            .unwrap_or("stop");
        let stop_reason = match done_reason {
            "stop" => StopReason::EndTurn,
            "length" => StopReason::MaxTokens,
            "load" => StopReason::Other("load".to_string()),
            other => StopReason::Other(other.to_string()),
        };

        let usage = body
            .get("prompt_eval_count")
            .and_then(Value::as_u64)
            .map(|prompt| UsageInfo {
                input_tokens: prompt,
                output_tokens: body.get("eval_count").and_then(Value::as_u64).unwrap_or(0),
                ..Default::default()
            })
            .unwrap_or_default();

        Ok(ProviderResponse {
            id: String::new(),
            content,
            stop_reason,
            usage,
            model: body
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            rate_limit: None,
        })
    }
}

/// Construct the native-transport provider from settings. Fails closed
/// (returns `None`) when no valid remote host is configured — identical
/// policy to the compat `ollama()` factory.
pub fn ollama_native() -> Option<OllamaNativeProvider> {
    let native_host = clawde_core::config::resolve_ollama_host()?;
    let inner = crate::providers::openai_compat_providers::ollama();
    Some(OllamaNativeProvider::new(inner, native_host))
}

#[async_trait]
impl LlmProvider for OllamaNativeProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn create_message(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        let resp = self.dispatch(&request).await?;
        let body: Value = resp.json().await.map_err(|e| ProviderError::Other {
            provider: self.id.clone(),
            message: format!("Failed to parse /api/chat response: {}", e),
            status: None,
            body: None,
        })?;
        self.parse_response(&body)
    }

    async fn create_message_stream(
        &self,
        request: ProviderRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        let body = self.build_body(&request, true);
        let resp = self
            .http_client
            .post(self.chat_url())
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Other {
                provider: self.id.clone(),
                message: format!("HTTP request failed: {}", e),
                status: None,
                body: None,
            })?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&(status as usize)) {
            let text = resp.text().await.unwrap_or_default();
            return Err(self.inner.map_http_error(status, &text));
        }

        let provider_id = self.id.clone();
        let s = stream! {
            let model = request.model.clone();
            yield Ok(StreamEvent::MessageStart {
                id: String::new(),
                model: model.clone(),
                usage: UsageInfo::default(),
            });

            let mut block_index: usize = 0;
            let mut text_block_open = false;
            let mut thinking_block_open = false;
            let mut tool_ids: Vec<String> = Vec::new();
            let mut partial = String::new();
            let mut stop_reason: Option<StopReason> = None;
            let mut final_usage = UsageInfo::default();

            let mut byte_stream = resp.bytes_stream();
            let mut line_buf: Vec<u8> = Vec::new();
            'outer: while let Some(chunk) = futures::StreamExt::next(&mut byte_stream).await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        let classified = if partial.is_empty() {
                            ProviderError::StreamError {
                                provider: provider_id.clone(),
                                message: format!("connection dropped before any content: {e}"),
                                partial_response: None,
                            }
                        } else {
                            ProviderError::StreamError {
                                provider: provider_id.clone(),
                                message: format!("connection dropped mid-stream: {e}"),
                                partial_response: Some(partial.clone()),
                            }
                        };
                        yield Err(classified);
                        break 'outer;
                    }
                };
                line_buf.extend_from_slice(&chunk);
                while let Some(pos) = line_buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = line_buf.drain(..=pos).collect();
                    let line = String::from_utf8_lossy(&line[..line.len() - 1]);
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let msg: Value = match serde_json::from_str(line) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    if let Some(err) = msg.get("error").and_then(Value::as_str) {
                        let classified = if partial.is_empty() {
                            ProviderError::StreamError {
                                provider: provider_id.clone(),
                                message: err.to_string(),
                                partial_response: None,
                            }
                        } else {
                            ProviderError::StreamError {
                                provider: provider_id.clone(),
                                message: err.to_string(),
                                partial_response: Some(partial.clone()),
                            }
                        };
                        yield Err(classified);
                        break 'outer;
                    }

                    let message_obj = msg.get("message").cloned().unwrap_or(Value::Null);

                    if let Some(thinking) = message_obj.get("thinking").and_then(Value::as_str) {
                        if !thinking.is_empty() {
                            if !thinking_block_open {
                                yield Ok(StreamEvent::ContentBlockStart {
                                    index: block_index,
                                    content_block: ContentBlock::Thinking {
                                        thinking: String::new(),
                                        signature: String::new(),
                                    },
                                });
                                thinking_block_open = true;
                            }
                            yield Ok(StreamEvent::ThinkingDelta {
                                index: block_index,
                                thinking: thinking.to_string(),
                            });
                        }
                    }

                    if let Some(text) = message_obj.get("content").and_then(Value::as_str) {
                        if !text.is_empty() {
                            if thinking_block_open {
                                yield Ok(StreamEvent::ContentBlockStop { index: block_index });
                                block_index += 1;
                                thinking_block_open = false;
                            }
                            if !text_block_open {
                                yield Ok(StreamEvent::ContentBlockStart {
                                    index: block_index,
                                    content_block: ContentBlock::Text {
                                        text: String::new(),
                                    },
                                });
                                text_block_open = true;
                            }
                            partial.push_str(text);
                            yield Ok(StreamEvent::TextDelta {
                                index: block_index,
                                text: text.to_string(),
                            });
                        }
                    }

                    if let Some(calls) = message_obj.get("tool_calls").and_then(Value::as_array) {
                        if text_block_open {
                            yield Ok(StreamEvent::ContentBlockStop { index: block_index });
                            block_index += 1;
                            text_block_open = false;
                        }
                        for call in calls {
                            let function = call.get("function").cloned().unwrap_or(Value::Null);
                            let name = function
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            let raw_args = function.get("arguments").cloned().unwrap_or(Value::Null);
                            let args_json = match raw_args {
                                Value::String(s) => s,
                                other => other.to_string(),
                            };
                            let id = call
                                .get("id")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .unwrap_or_else(|| format!("ollama_tool_{}", tool_ids.len()));
                            tool_ids.push(id.clone());
                            yield Ok(StreamEvent::ContentBlockStart {
                                index: block_index,
                                content_block: ContentBlock::ToolUse {
                                    id: id.clone(),
                                    name,
                                    input: Value::Null,
                                    thought_signature: None,
                                },
                            });
                            yield Ok(StreamEvent::InputJsonDelta {
                                index: block_index,
                                partial_json: args_json,
                            });
                            yield Ok(StreamEvent::ContentBlockStop { index: block_index });
                            block_index += 1;
                        }
                    }

                    if msg.get("done").and_then(Value::as_bool).unwrap_or(false) {
                        if thinking_block_open || text_block_open {
                            yield Ok(StreamEvent::ContentBlockStop { index: block_index });
                        }
                        stop_reason = Some(match msg.get("done_reason").and_then(Value::as_str) {
                            Some("stop") | None => StopReason::EndTurn,
                            Some("length") => StopReason::MaxTokens,
                            Some(other) => StopReason::Other(other.to_string()),
                        });
                        final_usage = UsageInfo {
                            input_tokens: msg.get("prompt_eval_count").and_then(Value::as_u64).unwrap_or(0),
                            output_tokens: msg.get("eval_count").and_then(Value::as_u64).unwrap_or(0),
                            ..Default::default()
                        };
                    }
                }
            }

            if let Some(stop_reason) = stop_reason {
                yield Ok(StreamEvent::MessageDelta {
                    stop_reason: Some(stop_reason),
                    usage: Some(final_usage),
                });
                yield Ok(StreamEvent::MessageStop);
            }
        };
        Ok(Box::pin(s))
    }

    async fn discover_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.inner.discover_models().await
    }

    async fn health_check(&self) -> Result<ProviderStatus, ProviderError> {
        self.inner.health_check().await
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities()
    }

    fn map_http_error(&self, status: u16, body: &str) -> ProviderError {
        self.inner.map_http_error(status, body)
    }

    fn max_tokens_cap_for(&self, model: &str) -> Option<u32> {
        self.inner.max_tokens_cap_for(model)
    }

    fn tool_calling_for(&self, model: &str) -> Option<bool> {
        self.inner.tool_calling_for(model)
    }
}
