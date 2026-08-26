//! OpenAI wire <-> Clawde `ProviderRequest` / `ProviderResponse` / `StreamEvent`
//! translation.
//!
//! The gateway accepts OpenAI-shaped chat completion bodies, converts them to
//! `ProviderRequest`, dispatches through the provider registry, and converts
//! the result back to OpenAI wire format (non-streaming JSON or SSE chunks).

use axum::http::StatusCode;
use clawde_api::provider_types::{ProviderRequest, ProviderResponse, StopReason, StreamEvent};
use clawde_core::types::{ContentBlock, Message, MessageContent, Role, ToolDefinition, UsageInfo};
use serde_json::{json, Value};

use crate::error::GatewayError;

// ---------------------------------------------------------------------------
// Request translation: OpenAI body -> ProviderRequest
// ---------------------------------------------------------------------------

/// A parsed chat completion request before dispatch.
#[derive(Debug, Clone)]
pub struct ParsedRequest {
    pub provider_request: ProviderRequest,
    /// Whether the client asked for a stream.
    pub stream: bool,
    /// Whether the client asked for usage in the stream (`stream_options.include_usage`).
    pub include_usage: bool,
    /// `n` — number of choices. v1 rejects `n > 1`.
    pub n: u32,
}

/// Parse an OpenAI chat completion request body into a [`ProviderRequest`].
///
/// Tolerates unknown fields (OpenAI clients send `stream_options`, `user`,
/// `seed`, `logprobs`, …). Rejects only structurally invalid bodies with
/// `400 invalid_request_error`.
pub fn parse_chat_completion_request(body: &Value) -> Result<ParsedRequest, GatewayError> {
    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or_else(|| GatewayError::invalid_request("missing required field 'model'"))?
        .to_string();

    let messages_value = body
        .get("messages")
        .and_then(|v| v.as_array())
        .ok_or_else(|| GatewayError::invalid_request("missing required field 'messages'"))?;

    let messages = parse_messages(messages_value)?;

    let tools = parse_tools(body.get("tools"))?;

    let max_tokens = body
        .get("max_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(4096) as u32;

    let temperature = body.get("temperature").and_then(|v| v.as_f64());
    let top_p = body.get("top_p").and_then(|v| v.as_f64());
    let stop_sequences: Vec<String> = body
        .get("stop")
        .and_then(|v| {
            v.as_str().map(|s| vec![s.to_string()]).or_else(|| {
                v.as_array().map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
            })
        })
        .unwrap_or_default();

    // reasoning_effort -> effort_level
    let effort_level = body
        .get("reasoning_effort")
        .or_else(|| body.get("effort"))
        .and_then(|v| v.as_str())
        .and_then(clawde_core::effort::EffortLevel::from_str);

    // n > 1 unsupported in v1.
    let n = body.get("n").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
    if n > 1 {
        return Err(GatewayError::invalid_request(
            "n > 1 is not supported by the gateway (v1)",
        ));
    }

    let stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let include_usage = body
        .get("stream_options")
        .and_then(|v| v.get("include_usage"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // tool_choice: passed through via provider_options for upstreams that
    // support it (v1: tolerate + forward; upstreams that don't ignore it).
    let provider_options = if let Some(tool_choice) = body.get("tool_choice") {
        json!({ "tool_choice": tool_choice })
    } else {
        json!({})
    };

    Ok(ParsedRequest {
        provider_request: ProviderRequest {
            model,
            messages,
            system_prompt: None,
            tools,
            max_tokens,
            temperature,
            top_p,
            top_k: None,
            stop_sequences,
            thinking: None,
            effort_level,
            provider_options,
            strict_route: false,
        },
        stream,
        include_usage,
        n,
    })
}

/// Parse OpenAI `messages[]` into Clawde `Message`s.
fn parse_messages(messages: &[Value]) -> Result<Vec<Message>, GatewayError> {
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        let role = m
            .get("role")
            .and_then(|v| v.as_str())
            .ok_or_else(|| GatewayError::invalid_request("message missing 'role'"))?;
        let content = m.get("content").cloned().unwrap_or(Value::Null);

        match role {
            "system" | "developer" => {
                // System prompt: extract text and hold it as a user-role message
                // with the system text prefixed. (Claw stores system prompt
                // separately; simpler: prepend a user message with the system
                // text as a marker-free instruction.)
                let text = content_text(&content);
                if !text.is_empty() {
                    out.push(Message {
                        role: Role::User,
                        content: MessageContent::Text(text),
                        uuid: None,
                        cost: None,
                        snapshot_patch: None,
                        turn_meta: None,
                    });
                }
            }
            "user" => {
                let text = content_text(&content);
                out.push(Message {
                    role: Role::User,
                    content: MessageContent::Text(text),
                    uuid: None,
                    cost: None,
                    snapshot_patch: None,
                    turn_meta: None,
                });
            }
            "assistant" => {
                // Assistant messages may carry content and/or tool_calls.
                let text = content_text(&content);
                let mut blocks: Vec<ContentBlock> = Vec::new();
                if !text.is_empty() {
                    blocks.push(ContentBlock::Text { text: text.clone() });
                }
                // tool_calls -> ToolUse blocks
                if let Some(tool_calls) = m.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tool_calls {
                        let id = tc
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let args = tc
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(|v| v.as_str())
                            .and_then(|s| serde_json::from_str(s).ok())
                            .unwrap_or(Value::Null);
                        blocks.push(ContentBlock::ToolUse {
                            id,
                            name,
                            input: args,
                            thought_signature: None,
                        });
                    }
                }
                out.push(Message {
                    role: Role::Assistant,
                    content: if blocks.is_empty() {
                        MessageContent::Text(text)
                    } else {
                        MessageContent::Blocks(blocks)
                    },
                    uuid: None,
                    cost: None,
                    snapshot_patch: None,
                    turn_meta: None,
                });
            }
            "tool" => {
                // Tool result message: role=tool, tool_call_id, content.
                let id = m
                    .get("tool_call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let text = content_text(&content);
                out.push(Message {
                    role: Role::User,
                    content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                        tool_use_id: id,
                        content: clawde_core::types::ToolResultContent::Text(text),
                        is_error: None,
                    }]),
                    uuid: None,
                    cost: None,
                    snapshot_patch: None,
                    turn_meta: None,
                });
            }
            other => {
                return Err(GatewayError::invalid_request(format!(
                    "unsupported message role '{other}'"
                )));
            }
        }
    }
    Ok(out)
}

/// Extract plain text from an OpenAI message `content` (string or array of
/// `{type:"text",text:...}` parts).
fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| {
                if p.get("type").and_then(|v| v.as_str()) == Some("text") {
                    p.get("text").and_then(|v| v.as_str()).map(String::from)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Parse OpenAI `tools[]` into `ToolDefinition`s.
fn parse_tools(tools: Option<&Value>) -> Result<Vec<ToolDefinition>, GatewayError> {
    let Some(tools) = tools else {
        return Ok(Vec::new());
    };
    let Some(arr) = tools.as_array() else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(arr.len());
    for t in arr {
        let function = t.get("function").unwrap_or(t);
        let name = function
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| GatewayError::invalid_request("tool missing 'function.name'"))?
            .to_string();
        let description = function
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let input_schema = function
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
        out.push(ToolDefinition {
            name,
            description,
            input_schema,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Non-streaming response translation
// ---------------------------------------------------------------------------

/// Convert a `ProviderResponse` to an OpenAI chat completion JSON body.
pub fn to_openai_response(resp: &ProviderResponse) -> Value {
    let (content, tool_calls, reasoning_content) = blocks_to_message(&resp.content);

    let message = json!({
        "role": "assistant",
        "content": content,
    });
    let mut message = message.as_object().cloned().unwrap_or_default();
    if let Some(tc) = tool_calls {
        message.insert("tool_calls".to_string(), tc);
    }
    if let Some(rc) = reasoning_content {
        message.insert("reasoning_content".to_string(), Value::String(rc));
    }

    json!({
        "id": resp.id,
        "object": "chat.completion",
        "created": chrono::Utc::now().timestamp(),
        "model": resp.model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": stop_reason_to_openai(&resp.stop_reason),
        }],
        "usage": usage_to_openai(&resp.usage),
    })
}

/// Convert `ContentBlock`s into OpenAI message fields.
/// Returns `(content, tool_calls, reasoning_content)`.
fn blocks_to_message(blocks: &[ContentBlock]) -> (Option<String>, Option<Value>, Option<String>) {
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut reasoning: Option<String> = None;
    for block in blocks {
        match block {
            ContentBlock::Text { text } => text_parts.push(text.clone()),
            ContentBlock::Thinking { thinking, .. } => {
                reasoning = Some(thinking.clone());
            }
            ContentBlock::ToolUse {
                id, name, input, ..
            } => {
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": if input.is_null() { "{}".to_string() } else { input.to_string() },
                    }
                }));
            }
            _ => {}
        }
    }
    let content = if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join("\n"))
    };
    let tool_calls = if tool_calls.is_empty() {
        None
    } else {
        Some(Value::Array(tool_calls))
    };
    (content, tool_calls, reasoning)
}

/// Map a `StopReason` to an OpenAI `finish_reason` string.
fn stop_reason_to_openai(reason: &StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn | StopReason::StopSequence => "stop",
        StopReason::MaxTokens => "length",
        StopReason::ToolUse => "tool_calls",
        StopReason::ContentFiltered => "content_filter",
        StopReason::Other(_) => "stop",
    }
}

/// Map `UsageInfo` to OpenAI `usage`.
pub fn usage_to_openai(usage: &UsageInfo) -> Value {
    json!({
        "prompt_tokens": usage.input_tokens,
        "completion_tokens": usage.output_tokens,
        "total_tokens": usage.total(),
        "reasoning_tokens": usage.reasoning_tokens,
    })
}

// ---------------------------------------------------------------------------
// Streaming translation
// ---------------------------------------------------------------------------

/// Accumulator for translating a `StreamEvent` stream into OpenAI chunks.
///
/// Call [`StreamTranslator::push`] for each event; it returns the OpenAI
/// chunks to emit (usually 0 or 1, sometimes 2 for the terminal usage chunk).
#[derive(Debug, Default)]
pub struct StreamTranslator {
    /// Whether the first chunk (with `delta.role`) has been emitted.
    started: bool,
    /// In-progress tool-call argument fragments, keyed by tool-call index.
    tool_calls: Vec<(usize, String)>,
    /// The message id (from MessageStart).
    id: String,
    /// The model (from MessageStart).
    model: String,
    /// Accumulated usage for the terminal usage chunk.
    usage: Option<UsageInfo>,
}

impl StreamTranslator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push one provider `StreamEvent`; returns OpenAI chunks to emit.
    pub fn push(&mut self, event: &StreamEvent) -> Vec<Value> {
        match event {
            StreamEvent::MessageStart { id, model, usage } => {
                self.id = id.clone();
                self.model = model.clone();
                self.usage = Some(usage.clone());
                // First chunk: role + empty content.
                vec![self.chunk(json!({
                    "choices": [{
                        "index": 0,
                        "delta": {"role": "assistant", "content": null},
                        "finish_reason": null,
                    }]
                }))]
            }
            StreamEvent::ContentBlockStart { content_block, .. } => {
                // Tool-use block start -> announce tool call.
                if let ContentBlock::ToolUse { id, name, .. } = content_block {
                    let idx = self.tool_calls_count();
                    self.tool_calls.push((idx, String::new()));
                    vec![self.chunk(json!({
                        "choices": [{
                            "index": 0,
                            "delta": {
                                "tool_calls": [{
                                    "index": idx,
                                    "id": id,
                                    "type": "function",
                                    "function": {"name": name, "arguments": ""},
                                }]
                            },
                            "finish_reason": null,
                        }]
                    }))]
                } else {
                    vec![]
                }
            }
            StreamEvent::TextDelta { text, .. } => vec![self.chunk(json!({
                "choices": [{
                    "index": 0,
                    "delta": {"content": text},
                    "finish_reason": null,
                }]
            }))],
            StreamEvent::ThinkingDelta { thinking, .. }
            | StreamEvent::ReasoningDelta {
                reasoning: thinking,
                ..
            } => vec![self.chunk(json!({
                "choices": [{
                    "index": 0,
                    "delta": {"reasoning_content": thinking},
                    "finish_reason": null,
                }]
            }))],
            StreamEvent::InputJsonDelta {
                partial_json,
                index,
            } => {
                // Accumulate arguments into the tool call at `index`.
                if let Some((_, args)) = self.tool_calls.iter_mut().find(|(i, _)| *i == *index) {
                    args.push_str(partial_json);
                    vec![self.chunk(json!({
                        "choices": [{
                            "index": 0,
                            "delta": {
                                "tool_calls": [{
                                    "index": index,
                                    "function": {"arguments": partial_json},
                                }]
                            },
                            "finish_reason": null,
                        }]
                    }))]
                } else {
                    vec![]
                }
            }
            StreamEvent::MessageDelta { stop_reason, usage } => {
                if let Some(u) = usage {
                    self.usage = Some(u.clone());
                }
                let reason = stop_reason
                    .as_ref()
                    .map(stop_reason_to_openai)
                    .unwrap_or("stop");
                vec![self.chunk(json!({
                    "choices": [{
                        "index": 0,
                        "delta": {},
                        "finish_reason": reason,
                    }]
                }))]
            }
            StreamEvent::MessageStop => {
                self.started = true;
                let mut chunks = Vec::new();
                // Terminal chunk (finish_reason already sent in MessageDelta;
                // emit an empty one for clients that expect it).
                chunks.push(self.chunk(json!({
                    "choices": [{
                        "index": 0,
                        "delta": {},
                        "finish_reason": "stop",
                    }]
                })));
                // Usage chunk if requested.
                if let Some(usage) = self.usage.clone() {
                    chunks.push(self.usage_chunk(&usage));
                }
                chunks
            }
            StreamEvent::Error { message, .. } => {
                // Emit an error chunk then let the client see the [DONE].
                vec![self.chunk(json!({
                    "choices": [{
                        "index": 0,
                        "delta": {},
                        "finish_reason": null,
                    }],
                    "error": {"message": message},
                }))]
            }
            _ => vec![],
        }
    }

    /// Whether the stream should end (MessageStop seen).
    pub fn is_done(&self) -> bool {
        self.started
    }

    fn tool_calls_count(&self) -> usize {
        self.tool_calls.len()
    }

    /// Build a chunk with the shared id/object/model fields.
    fn chunk(&self, extra: Value) -> Value {
        let mut base = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": chrono::Utc::now().timestamp(),
            "model": self.model,
        });
        if let Value::Object(map) = &mut base {
            if let Value::Object(extra_map) = extra {
                for (k, v) in extra_map {
                    map.insert(k.clone(), v);
                }
            }
        }
        base
    }

    /// Usage-only chunk for `stream_options.include_usage`.
    fn usage_chunk(&self, usage: &UsageInfo) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": chrono::Utc::now().timestamp(),
            "model": self.model,
            "choices": [],
            "usage": usage_to_openai(usage),
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a `400` response for an unparseable body (used by the router).
pub fn invalid_body_error(detail: &str) -> GatewayError {
    GatewayError {
        status: StatusCode::BAD_REQUEST,
        error_type: "invalid_request_error".to_string(),
        message: format!("Invalid request body: {detail}"),
        param: None,
        code: None,
        retry_after_secs: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_body() -> Value {
        json!({
            "model": "free/auto",
            "messages": [
                {"role": "user", "content": "Hello"},
                {"role": "assistant", "content": "Hi there"},
                {"role": "user", "content": "How are you?"}
            ],
            "max_tokens": 100,
            "stream": false,
        })
    }

    #[test]
    fn parses_basic_request() {
        let parsed = parse_chat_completion_request(&request_body()).unwrap();
        assert_eq!(parsed.provider_request.model, "free/auto");
        assert_eq!(parsed.provider_request.messages.len(), 3);
        assert!(!parsed.stream);
        assert_eq!(parsed.provider_request.max_tokens, 100);
    }

    #[test]
    fn parses_tools_and_tool_choice() {
        let mut body = request_body();
        body["tools"] = json!([{
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
            }
        }]);
        body["tool_choice"] = json!({"type": "function", "function": {"name": "get_weather"}});
        let parsed = parse_chat_completion_request(&body).unwrap();
        assert_eq!(parsed.provider_request.tools.len(), 1);
        assert_eq!(parsed.provider_request.tools[0].name, "get_weather");
        assert_eq!(
            parsed.provider_request.provider_options["tool_choice"]["function"]["name"],
            "get_weather"
        );
    }

    #[test]
    fn parses_assistant_tool_calls() {
        let body = json!({
            "model": "free/auto",
            "messages": [
                {"role": "user", "content": "What's the weather?"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "get_weather", "arguments": "{\"city\":\"SF\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "72F"}
            ]
        });
        let parsed = parse_chat_completion_request(&body).unwrap();
        let msgs = &parsed.provider_request.messages;
        assert_eq!(msgs.len(), 3);
        // Assistant message has a ToolUse block.
        if let MessageContent::Blocks(blocks) = &msgs[1].content {
            assert!(matches!(blocks[0], ContentBlock::ToolUse { .. }));
        } else {
            panic!("expected blocks");
        }
        // Tool result message.
        if let MessageContent::Blocks(blocks) = &msgs[2].content {
            assert!(matches!(blocks[0], ContentBlock::ToolResult { .. }));
        } else {
            panic!("expected blocks");
        }
    }

    #[test]
    fn rejects_n_greater_than_one() {
        let mut body = request_body();
        body["n"] = json!(2);
        assert!(parse_chat_completion_request(&body).is_err());
    }

    #[test]
    fn missing_model_rejected() {
        let body = json!({"messages": [{"role": "user", "content": "hi"}]});
        assert!(parse_chat_completion_request(&body).is_err());
    }

    #[test]
    fn response_translation_includes_reasoning() {
        let resp = ProviderResponse {
            id: "msg_1".to_string(),
            content: vec![
                ContentBlock::Thinking {
                    thinking: "hmm".to_string(),
                    signature: String::new(),
                },
                ContentBlock::Text {
                    text: "result".to_string(),
                },
            ],
            stop_reason: StopReason::EndTurn,
            usage: UsageInfo {
                input_tokens: 10,
                output_tokens: 5,
                reasoning_tokens: 3,
                ..Default::default()
            },
            model: "free/auto".to_string(),
            rate_limit: None,
        };
        let out = to_openai_response(&resp);
        assert_eq!(out["choices"][0]["message"]["content"], "result");
        assert_eq!(out["choices"][0]["message"]["reasoning_content"], "hmm");
        assert_eq!(out["usage"]["reasoning_tokens"], 3);
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn stream_accumulates_tool_arguments() {
        let mut t = StreamTranslator::new();
        let chunks: Vec<Value> = t
            .push(&StreamEvent::MessageStart {
                id: "msg_1".to_string(),
                model: "free/auto".to_string(),
                usage: UsageInfo::default(),
            })
            .into_iter()
            .chain(t.push(&StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "get_weather".to_string(),
                    input: Value::Null,
                    thought_signature: None,
                },
            }))
            .chain(t.push(&StreamEvent::InputJsonDelta {
                index: 0,
                partial_json: "{\"city\":".to_string(),
            }))
            .chain(t.push(&StreamEvent::InputJsonDelta {
                index: 0,
                partial_json: "\"SF\"}".to_string(),
            }))
            .chain(t.push(&StreamEvent::MessageStop))
            .collect();
        // First chunk: role.
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        // Tool call announcement.
        assert_eq!(
            chunks[1]["choices"][0]["delta"]["tool_calls"][0]["id"],
            "call_1"
        );
        assert_eq!(
            chunks[1]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
            "get_weather"
        );
        // Argument fragments streamed.
        assert_eq!(
            chunks[2]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{\"city\":"
        );
        assert_eq!(
            chunks[3]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "\"SF\"}"
        );
    }
}
