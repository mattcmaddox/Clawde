//! Open Responses (`POST /v1/responses`) wire translation.
//!
//! Request side: parse `input` (message / function_call / function_call_output
//! items), `instructions`, flat `tools[]`, `tool_choice`, `allowed_tools`,
//! `previous_response_id`, `max_tool_calls`, `parallel_tool_calls`,
//! `max_output_tokens`, `stream`, `store`. Response side: output items
//! (`message`, `reasoning`, `function_call`, `function_call_output`) built
//! from the agent loop's `LoopEvent`s, rendered either as a complete JSON
//! `output[]` array or as semantic SSE events (`response.output_item.added`,
//! `response.output_text.delta`, `response.function_call_arguments.delta`,
//! …) with `sequence_number` ordering per the Open Responses spec.
//!
//! Locked decisions applied here: D3 (reasoning = raw `content`), D6
//! (`allowed_tools` hard enforcement -> tool_error observation), D7
//! (`n > 1` rejected), D8 (client `instructions` verbatim).

use clawde_api::provider_types::{ProviderRequest, StopReason, SystemPrompt};
use clawde_core::types::UsageInfo;
use clawde_core::types::{ContentBlock, ToolDefinition};
use serde_json::{json, Value};

use crate::agent::{AgentOutcome, AgentStatus, LoopEvent};
use crate::error::GatewayError;
use crate::session::output_items_to_messages;

// ---------------------------------------------------------------------------
// Request parsing
// ---------------------------------------------------------------------------

/// A parsed Responses request before dispatch.
#[derive(Debug, Clone)]
pub struct ParsedResponsesRequest {
    pub provider_request: ProviderRequest,
    pub stream: bool,
    /// Retention intent (both values are retained in the in-memory cache, D5).
    pub store: bool,
    pub previous_response_id: Option<String>,
    pub max_tool_calls: Option<u32>,
    pub parallel_tool_calls: bool,
    /// Hard enforcement set (D6); `None` = no restriction.
    pub allowed_tools: Option<Vec<String>>,
    /// Raw input items (session storage / continuation rebuild).
    pub input_items: Vec<Value>,
}

/// Parse an OpenAI Responses request body.
///
/// `input` may be a plain string or an array of items (`message`,
/// `function_call`, `function_call_output`, `reasoning`). `tools` use the
/// flat Responses form `{type, name, description, parameters}`; the nested
/// chat-completions form is also tolerated.
pub fn parse_responses_request(body: &Value) -> Result<ParsedResponsesRequest, GatewayError> {
    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or_else(|| GatewayError::invalid_request("missing required field 'model'"))?
        .to_string();

    let input_items = parse_input(body.get("input"))?;
    let messages = output_items_to_messages(&input_items);

    // `instructions` is the canonical system prompt; client text is verbatim
    // (D8). A minimal gateway preamble is injected only when the client sends
    // neither instructions nor system input messages.
    let instructions = body.get("instructions").and_then(|v| v.as_str());
    let has_system_message = input_items.iter().any(|item| {
        item.get("type").and_then(|v| v.as_str()).unwrap_or("") != "function_call_output"
            && matches!(
                item.get("role").and_then(|v| v.as_str()),
                Some("system") | Some("developer")
            )
    });
    let system_prompt = match instructions {
        Some(text) if !text.trim().is_empty() => Some(SystemPrompt::Text(text.to_string())),
        // No client `instructions` and no system input message: inject the
        // minimal gateway preamble (D8).
        _ if !has_system_message => Some(SystemPrompt::Text(GATEWAY_PREAMBLE.to_string())),
        _ => None,
    };

    let tools = parse_responses_tools(body.get("tools"))?;

    let max_tokens = body
        .get("max_output_tokens")
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
    let effort_level = body
        .get("reasoning_effort")
        .and_then(|v| v.as_str())
        .and_then(clawde_core::effort::EffortLevel::from_str);

    // D7: n must be >= 1; v1 additionally rejects n > 1.
    let n = body.get("n").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
    if n == 0 {
        return Err(GatewayError::invalid_request("n must be at least 1"));
    }
    if n > 1 {
        return Err(GatewayError::invalid_request(
            "n > 1 is not supported by the gateway (v1)",
        ));
    }

    // tool_choice passthrough (auto/required/none strings + forced-function
    // objects are forwarded to the provider via provider_options).
    let mut provider_options = json!({});
    if let Some(tool_choice) = body.get("tool_choice") {
        provider_options["tool_choice"] = tool_choice.clone();
    }

    let stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let store = body.get("store").and_then(|v| v.as_bool()).unwrap_or(false);
    let previous_response_id = body
        .get("previous_response_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);
    let max_tool_calls = body
        .get("max_tool_calls")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let parallel_tool_calls = body
        .get("parallel_tool_calls")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let allowed_tools = body
        .get("allowed_tools")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str().map(String::from))
                .collect()
        })
        .filter(|v: &Vec<String>| !v.is_empty());

    Ok(ParsedResponsesRequest {
        provider_request: ProviderRequest {
            model,
            messages,
            system_prompt,
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
        store,
        previous_response_id,
        max_tool_calls,
        parallel_tool_calls,
        allowed_tools,
        input_items,
    })
}

/// Normalize `input` (string or item array) into a `Vec<Value>` of items.
fn parse_input(input: Option<&Value>) -> Result<Vec<Value>, GatewayError> {
    let Some(input) = input else {
        return Err(GatewayError::invalid_request(
            "missing required field 'input'",
        ));
    };
    match input {
        Value::String(s) => Ok(vec![json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": s}],
        })]),
        Value::Array(items) => {
            if items.is_empty() {
                return Err(GatewayError::invalid_request("'input' must not be empty"));
            }
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let item_type = item
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("message");
                match item_type {
                    "message" | "function_call" | "function_call_output" | "reasoning" => {
                        out.push(item.clone())
                    }
                    other => {
                        return Err(GatewayError::invalid_request(format!(
                            "unsupported input item type '{other}'"
                        )))
                    }
                }
            }
            Ok(out)
        }
        _ => Err(GatewayError::invalid_request(
            "'input' must be a string or an array of items",
        )),
    }
}

/// Parse Responses `tools[]` (flat function form; nested form tolerated).
fn parse_responses_tools(tools: Option<&Value>) -> Result<Vec<ToolDefinition>, GatewayError> {
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
            .ok_or_else(|| GatewayError::invalid_request("tool missing 'name'"))?
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
// Output items + streaming events
// ---------------------------------------------------------------------------

/// Accumulates `LoopEvent`s into Open Responses output items while emitting
/// the matching semantic SSE events. Used by both the non-stream path (events
/// discarded, items kept) and the stream path (events forwarded as SSE).
pub struct ResponsesItemBuilder {
    /// Completed output items (JSON), in output order.
    pub items: Vec<Value>,
    /// Monotonic `sequence_number` for streaming events.
    seq: u64,
    item_counter: u64,
    /// In-progress message item for the current turn.
    msg: Option<OpenItem>,
    /// In-progress reasoning item for the current turn.
    reasoning: Option<OpenItem>,
}

/// Cap for accumulated reasoning text in the final item (D3 — raw `content`
/// from Thinking blocks, truncated to a budget). The streamed deltas stay
/// raw; only the exposed item text is capped.
const THINKING_TEXT_BUDGET: usize = 32 * 1024;

/// An in-progress streamable item (message or reasoning text).
struct OpenItem {
    item_id: String,
    kind: &'static str, // "message" | "reasoning"
    role: &'static str,
    text: String,
    has_part: bool,
    /// Content was cut at `THINKING_TEXT_BUDGET` (close appends a marker).
    truncated: bool,
    /// The item's slot in `items` (stable even as later items append).
    output_index: usize,
}

impl ResponsesItemBuilder {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            seq: 0,
            item_counter: 0,
            msg: None,
            reasoning: None,
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn next_item_id(&mut self, prefix: &str) -> String {
        self.item_counter += 1;
        format!("{prefix}_{}", self.item_counter)
    }

    /// Push one loop event; returns the SSE events to emit (empty in the
    /// non-stream path, where only the accumulated items matter).
    pub fn push(&mut self, event: &LoopEvent) -> Vec<Value> {
        match event {
            LoopEvent::TurnStart { .. } => {
                // Turn boundary: the loop emits TurnEnd BEFORE the turn's
                // ToolCall/ToolExecuted events, so a message opened by a tool
                // round can only be closed here (or by finalize). Reasoning
                // items are closed by TurnEnd and never span turns.
                let mut out = Vec::new();
                if let Some(reason) = self.reasoning.take() {
                    out.extend(self.close_open_item(reason));
                }
                if let Some(msg) = self.msg.take() {
                    out.extend(self.close_open_item(msg));
                }
                out
            }
            LoopEvent::TextDelta { text } => {
                let mut out = Vec::new();
                if self.msg.is_none() {
                    let (msg, evts) = self.open_message();
                    self.msg = Some(msg);
                    out.extend(evts);
                }
                let (item_id, output_index, has_part) = {
                    let msg = self.msg.as_mut().expect("message opened");
                    (msg.item_id.clone(), msg.output_index, msg.has_part)
                };
                if !has_part {
                    self.msg.as_mut().expect("message opened").has_part = true;
                    out.push(self.event(
                        "response.content_part.added",
                        json!({
                            "item_id": item_id,
                            "output_index": output_index,
                            "content_index": 0,
                            "part": {"type": "output_text", "annotations": [], "text": ""},
                        }),
                    ));
                }
                out.push(self.event(
                    "response.output_text.delta",
                    json!({
                        "item_id": item_id,
                        "output_index": output_index,
                        "content_index": 0,
                        "delta": text,
                    }),
                ));
                self.msg
                    .as_mut()
                    .expect("message opened")
                    .text
                    .push_str(text);
                out
            }
            LoopEvent::ThinkingDelta { thinking } => {
                let mut out = Vec::new();
                if self.reasoning.is_none() {
                    let (reason, evts) = self.open_reasoning();
                    self.reasoning = Some(reason);
                    out.extend(evts);
                }
                let (item_id, output_index, has_part) = {
                    let reason = self.reasoning.as_mut().expect("reasoning opened");
                    (reason.item_id.clone(), reason.output_index, reason.has_part)
                };
                if !has_part {
                    self.reasoning.as_mut().expect("reasoning opened").has_part = true;
                    out.push(self.event(
                        "response.content_part.added",
                        json!({
                            "item_id": item_id,
                            "output_index": output_index,
                            "content_index": 0,
                            "part": {"type": "output_text", "annotations": [], "text": ""},
                        }),
                    ));
                }
                out.push(self.event(
                    "response.output_text.delta",
                    json!({
                        "item_id": item_id,
                        "output_index": output_index,
                        "content_index": 0,
                        "delta": thinking,
                    }),
                ));
                // Bounded accumulation (D3): keep the first `budget` bytes on a
                // char boundary; mark truncated so the close appends a marker.
                let reason = self.reasoning.as_mut().expect("reasoning opened");
                let remaining = THINKING_TEXT_BUDGET.saturating_sub(reason.text.len());
                let take = thinking.floor_char_boundary(remaining);
                reason.text.push_str(&thinking[..take]);
                if take < thinking.len() {
                    reason.truncated = true;
                }
                out
            }
            LoopEvent::ToolCall { id, name, input }
            | LoopEvent::ExternalToolCall { id, name, input } => {
                let mut out = Vec::new();
                // The assistant message item precedes its tool calls in the
                // output (empty content when the turn has no text).
                if self.msg.is_none() {
                    let (msg, evts) = self.open_message();
                    self.msg = Some(msg);
                    out.extend(evts);
                }
                let output_index = self.items.len();
                let call_id = id.clone();
                let item_id = self.next_item_id("fc");
                let arguments = if input.is_null() {
                    "{}".to_string()
                } else {
                    input.to_string()
                };
                out.push(self.event(
                    "response.output_item.added",
                    json!({
                        "output_index": output_index,
                        "item": {
                            "id": item_id,
                            "type": "function_call",
                            "status": "in_progress",
                            "call_id": call_id,
                            "name": name,
                            "arguments": "",
                        },
                    }),
                ));
                out.push(self.event(
                    "response.function_call_arguments.delta",
                    json!({
                        "item_id": item_id,
                        "output_index": output_index,
                        "delta": arguments,
                    }),
                ));
                out.push(self.event(
                    "response.function_call_arguments.done",
                    json!({
                        "item_id": item_id,
                        "output_index": output_index,
                        "arguments": arguments,
                    }),
                ));
                out.push(self.event(
                    "response.output_item.done",
                    json!({
                        "output_index": output_index,
                        "item": {
                            "id": item_id,
                            "type": "function_call",
                            "status": "completed",
                            "call_id": call_id,
                            "name": name,
                            "arguments": arguments,
                        },
                    }),
                ));
                self.items.push(json!({
                    "id": item_id,
                    "type": "function_call",
                    "status": "completed",
                    "call_id": call_id,
                    "name": name,
                    "arguments": arguments,
                }));
                out
            }
            LoopEvent::ToolExecuted {
                id,
                name,
                result,
                is_error,
            } => {
                let output_index = self.items.len();
                let item_id = self.next_item_id("fc_out");
                let output = if *is_error {
                    format!("tool_error: {name}: {result}")
                } else {
                    result.clone()
                };
                let mut out = Vec::new();
                out.push(self.event(
                    "response.output_item.added",
                    json!({
                        "output_index": output_index,
                        "item": {
                            "id": item_id,
                            "type": "function_call_output",
                            "status": "completed",
                            "call_id": id,
                            "output": output,
                        },
                    }),
                ));
                out.push(self.event(
                    "response.output_item.done",
                    json!({
                        "output_index": output_index,
                        "item": {
                            "id": item_id,
                            "type": "function_call_output",
                            "status": "completed",
                            "call_id": id,
                            "output": output,
                        },
                    }),
                ));
                self.items.push(json!({
                    "id": item_id,
                    "type": "function_call_output",
                    "status": "completed",
                    "call_id": id,
                    "output": output,
                }));
                out
            }
            LoopEvent::TurnEnd { .. } => {
                // Close only the reasoning item here: the message of a tool
                // round is still open (its ToolCall events follow TurnEnd in
                // the loop) and closes at the next TurnStart or finalize.
                let mut out = Vec::new();
                if let Some(reason) = self.reasoning.take() {
                    out.extend(self.close_open_item(reason));
                }
                out
            }
            LoopEvent::ContextCompacted { .. } => Vec::new(),
        }
    }

    /// Close any items left open (defensive — the loop may end without a
    /// trailing `TurnStart`, e.g. the final text turn or a cap stop).
    pub fn finalize(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        if let Some(reason) = self.reasoning.take() {
            out.extend(self.close_open_item(reason));
        }
        if let Some(msg) = self.msg.take() {
            out.extend(self.close_open_item(msg));
        }
        out
    }

    /// Open a message item and emit its `output_item.added` event.
    fn open_message(&mut self) -> (OpenItem, Vec<Value>) {
        let output_index = self.items.len();
        self.items.push(json!({
            "id": "",
            "type": "message",
            "status": "in_progress",
            "role": "assistant",
            "content": [],
        }));
        let item_id = self.next_item_id("msg");
        if let Some(slot) = self.items.get_mut(output_index) {
            slot["id"] = json!(item_id.clone());
        }
        let evt = self.event(
            "response.output_item.added",
            json!({
                "output_index": output_index,
                "item": {
                    "id": item_id.clone(),
                    "type": "message",
                    "status": "in_progress",
                    "role": "assistant",
                    "content": [],
                },
            }),
        );
        (
            OpenItem {
                item_id,
                kind: "message",
                role: "assistant",
                text: String::new(),
                has_part: false,
                truncated: false,
                output_index,
            },
            vec![evt],
        )
    }

    /// Open a reasoning item and emit its `output_item.added` event.
    fn open_reasoning(&mut self) -> (OpenItem, Vec<Value>) {
        let output_index = self.items.len();
        self.items.push(json!({
            "id": "",
            "type": "reasoning",
            "status": "in_progress",
            "content": [],
            "summary": [],
            "encrypted_content": null,
        }));
        let item_id = self.next_item_id("rs");
        if let Some(slot) = self.items.get_mut(output_index) {
            slot["id"] = json!(item_id.clone());
        }
        let evt = self.event(
            "response.output_item.added",
            json!({
                "output_index": output_index,
                "item": {
                    "id": item_id.clone(),
                    "type": "reasoning",
                    "status": "in_progress",
                    "content": [],
                    "summary": [],
                    "encrypted_content": null,
                },
            }),
        );
        (
            OpenItem {
                item_id,
                kind: "reasoning",
                role: "assistant",
                text: String::new(),
                has_part: false,
                truncated: false,
                output_index,
            },
            vec![evt],
        )
    }

    /// Emit `output_text.done` + `content_part.done` + `output_item.done` and
    /// write the completed item back into its slot in `items`.
    fn close_open_item(&mut self, item: OpenItem) -> Vec<Value> {
        let display_text = if item.truncated {
            format!("{}…[truncated]", item.text)
        } else {
            item.text
        };
        let mut out = Vec::new();
        if item.has_part {
            out.push(self.event(
                "response.output_text.done",
                json!({
                    "item_id": item.item_id,
                    "output_index": item.output_index,
                    "content_index": 0,
                    "text": display_text,
                }),
            ));
            out.push(self.event(
                "response.content_part.done",
                json!({
                    "item_id": item.item_id,
                    "output_index": item.output_index,
                    "content_index": 0,
                    "part": {"type": "output_text", "annotations": [], "text": display_text},
                }),
            ));
        }
        let completed = json!({
            "id": item.item_id,
            "type": item.kind,
            "status": "completed",
            "role": item.role,
            "content": if item.has_part {
                json!([{"type": "output_text", "annotations": [], "text": display_text}])
            } else {
                json!([])
            },
        });
        out.push(self.event(
            "response.output_item.done",
            json!({
                "output_index": item.output_index,
                "item": completed,
            }),
        ));
        if let Some(slot) = self.items.get_mut(item.output_index) {
            *slot = completed;
        }
        out
    }

    fn event(&mut self, event_type: &str, body: Value) -> Value {
        let seq = self.next_seq();
        let mut body = body;
        if let Value::Object(map) = &mut body {
            map.insert("type".to_string(), json!(event_type));
            map.insert("sequence_number".to_string(), json!(seq));
        }
        body
    }
}

impl Default for ResponsesItemBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Response object assembly
// ---------------------------------------------------------------------------

/// The OpenAI `usage` object for Responses.
pub fn usage_to_responses(usage: &UsageInfo) -> Value {
    json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "total_tokens": usage.total(),
        "input_tokens_details": {"cached_tokens": usage.cache_read_input_tokens},
        "output_tokens_details": {"reasoning_tokens": usage.reasoning_tokens},
    })
}

/// Map a loop outcome to the Responses `status` string + incomplete reason.
pub fn outcome_status(outcome: &AgentOutcome) -> (&'static str, Option<&'static str>) {
    match outcome.status {
        AgentStatus::Completed | AgentStatus::Yielding => ("completed", None),
        AgentStatus::CapExhausted | AgentStatus::NoProgress => {
            ("incomplete", Some("max_tool_calls"))
        }
        AgentStatus::Failed | AgentStatus::Cancelled => ("failed", None),
    }
}

/// Build the full non-stream response object.
pub fn responses_object(
    response_id: &str,
    model: &str,
    output: Vec<Value>,
    usage: &UsageInfo,
    status: &str,
    incomplete_reason: Option<&str>,
    error: Option<Value>,
) -> Value {
    let mut resp = json!({
        "id": response_id,
        "object": "response",
        "created_at": chrono::Utc::now().timestamp(),
        "status": status,
        "model": model,
        "output": output,
        "usage": usage_to_responses(usage),
        "error": error,
    });
    if let Some(reason) = incomplete_reason {
        resp["incomplete_details"] = json!({"reason": reason});
    }
    resp
}

/// Build the minimal response skeleton for `response.created` / `.in_progress`.
pub fn response_skeleton(response_id: &str, model: &str) -> Value {
    json!({
        "id": response_id,
        "object": "response",
        "created_at": chrono::Utc::now().timestamp(),
        "status": "in_progress",
        "model": model,
        "output": [],
        "usage": null,
        "error": null,
    })
}

/// Generate a fresh response id.
pub fn new_response_id() -> String {
    format!(
        "resp_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    )
}

/// The minimal gateway tool-use preamble injected when the client sends no
/// `instructions` and no system input (D8).
pub const GATEWAY_PREAMBLE: &str = "You are a helpful AI assistant. When a task requires \
it, call the available tools and continue until the task is complete.";

// Keep `StopReason`/`ContentBlock` referenced so the imports stay meaningful
// for future reasoning-delta handling.
#[allow(dead_code)]
fn _unused(_s: StopReason, _b: ContentBlock) {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body() -> Value {
        json!({
            "model": "free/auto",
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "hello"}]}
            ],
            "instructions": "Be concise.",
            "max_output_tokens": 100,
        })
    }

    #[test]
    fn parses_basic_request() {
        let parsed = parse_responses_request(&body()).unwrap();
        assert_eq!(parsed.provider_request.model, "free/auto");
        assert_eq!(parsed.provider_request.messages.len(), 1);
        assert!(parsed.provider_request.messages[0]
            .get_all_text()
            .contains("hello"));
        let sp = parsed.provider_request.system_prompt.expect("instructions");
        match sp {
            SystemPrompt::Text(t) => assert_eq!(t, "Be concise."),
            SystemPrompt::Blocks(_) => panic!("expected text system prompt"),
        }
        assert_eq!(parsed.provider_request.max_tokens, 100);
        assert!(!parsed.stream);
        assert!(!parsed.store);
    }

    #[test]
    fn parses_string_input_as_user_message() {
        let mut b = body();
        b["input"] = json!("plain string input");
        let parsed = parse_responses_request(&b).unwrap();
        assert_eq!(parsed.provider_request.messages.len(), 1);
        assert!(parsed.provider_request.messages[0]
            .get_all_text()
            .contains("plain string input"));
    }

    #[test]
    fn parses_function_call_items_in_input() {
        let mut b = body();
        b["input"] = json!([
            {"type": "function_call", "call_id": "call_1", "name": "Read", "arguments": "{\"path\":\"/x\"}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "file contents"},
            {"role": "user", "content": "now what?"},
        ]);
        let parsed = parse_responses_request(&b).unwrap();
        let msgs = &parsed.provider_request.messages;
        assert_eq!(msgs.len(), 2);
        // Assistant tool_use merged with the tool result, then the user turn.
        use clawde_core::types::MessageContent;
        match &msgs[0].content {
            MessageContent::Blocks(blocks) => {
                assert!(matches!(blocks[0], ContentBlock::ToolUse { .. }));
            }
            _ => panic!("expected tool use block"),
        }
        assert!(msgs[1].get_all_text().contains("now what?"));
    }

    #[test]
    fn parses_flat_tools_and_knobs() {
        let mut b = body();
        b["tools"] = json!([{
            "type": "function",
            "name": "Read",
            "description": "Read a file",
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}}},
        }]);
        b["allowed_tools"] = json!(["Read"]);
        b["max_tool_calls"] = json!(4);
        b["parallel_tool_calls"] = json!(false);
        b["previous_response_id"] = json!("resp_123");
        b["store"] = json!(true);
        let parsed = parse_responses_request(&b).unwrap();
        assert_eq!(parsed.provider_request.tools.len(), 1);
        assert_eq!(parsed.provider_request.tools[0].name, "Read");
        assert_eq!(
            parsed.allowed_tools.as_deref(),
            Some(&["Read".to_string()][..])
        );
        assert_eq!(parsed.max_tool_calls, Some(4));
        assert!(!parsed.parallel_tool_calls);
        assert_eq!(parsed.previous_response_id.as_deref(), Some("resp_123"));
        assert!(parsed.store);
    }

    #[test]
    fn rejects_n_greater_than_one() {
        let mut b = body();
        b["n"] = json!(2);
        assert!(parse_responses_request(&b).is_err());
    }

    #[test]
    fn rejects_n_zero() {
        let mut b = body();
        b["n"] = json!(0);
        assert!(parse_responses_request(&b).is_err());
    }

    #[test]
    fn rejects_unknown_input_item_type() {
        let mut b = body();
        b["input"] = json!([{"type": "bogus_item"}]);
        assert!(parse_responses_request(&b).is_err());
    }

    #[test]
    fn builder_accumulates_message_items() {
        let mut builder = ResponsesItemBuilder::new();
        let mut events = builder.push(&LoopEvent::TurnStart { turn: 1 });
        events.extend(builder.push(&LoopEvent::TextDelta { text: "hel".into() }));
        events.extend(builder.push(&LoopEvent::TextDelta { text: "lo".into() }));
        events.extend(builder.push(&LoopEvent::TurnEnd {
            stop_reason: StopReason::EndTurn,
        }));
        // The final text turn has no trailing TurnStart; finalize closes it.
        events.extend(builder.finalize());
        // Message item added once; two text deltas; terminal closes.
        let types: Vec<&str> = events
            .iter()
            .filter_map(|e| e.get("type").and_then(|t| t.as_str()))
            .collect();
        assert_eq!(types[0], "response.output_item.added");
        assert_eq!(types[1], "response.content_part.added");
        assert_eq!(types[2], "response.output_text.delta");
        assert_eq!(types[3], "response.output_text.delta");
        assert_eq!(types[4], "response.output_text.done");
        assert_eq!(types[5], "response.content_part.done");
        assert_eq!(types[6], "response.output_item.done");
        assert_eq!(builder.items.len(), 1);
        assert_eq!(builder.items[0]["content"][0]["text"], "hello");
        // sequence_number is monotonic.
        let seqs: Vec<u64> = events
            .iter()
            .filter_map(|e| e.get("sequence_number").and_then(|s| s.as_u64()))
            .collect();
        assert!(seqs.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn builder_renders_tool_round() {
        let mut builder = ResponsesItemBuilder::new();
        let mut events = builder.push(&LoopEvent::TurnStart { turn: 1 });
        events.extend(builder.push(&LoopEvent::ToolCall {
            id: "call_1".into(),
            name: "Read".into(),
            input: json!({"path": "/x"}),
        }));
        events.extend(builder.push(&LoopEvent::ToolExecuted {
            id: "call_1".into(),
            name: "Read".into(),
            result: "file contents".into(),
            is_error: false,
        }));
        events.extend(builder.push(&LoopEvent::TurnEnd {
            stop_reason: StopReason::ToolUse,
        }));
        let types: Vec<&str> = events
            .iter()
            .filter_map(|e| e.get("type").and_then(|t| t.as_str()))
            .collect();
        assert!(types.contains(&"response.function_call_arguments.delta"));
        assert!(types.contains(&"response.function_call_arguments.done"));
        assert!(types.contains(&"response.output_item.done"));
        assert_eq!(builder.items.len(), 3); // message + function_call + function_call_output
        assert_eq!(builder.items[1]["type"], "function_call");
        assert_eq!(builder.items[1]["arguments"], "{\"path\":\"/x\"}");
        assert_eq!(builder.items[2]["type"], "function_call_output");
        assert_eq!(builder.items[2]["output"], "file contents");
    }

    #[test]
    fn builder_emits_reasoning_item() {
        let mut builder = ResponsesItemBuilder::new();
        builder.push(&LoopEvent::ThinkingDelta {
            thinking: "think hard".into(),
        });
        builder.push(&LoopEvent::TurnEnd {
            stop_reason: StopReason::EndTurn,
        });
        builder.finalize();
        // Reasoning items close at TurnEnd and do not span turns; a
        // reasoning-only turn yields just the reasoning item.
        assert_eq!(builder.items.len(), 1);
        assert_eq!(builder.items[0]["type"], "reasoning");
        assert_eq!(builder.items[0]["content"][0]["text"], "think hard");
    }

    #[test]
    fn outcome_status_maps_cap_to_incomplete() {
        let outcome = AgentOutcome {
            status: AgentStatus::CapExhausted,
            message: clawde_core::types::Message::assistant(""),
            usage: UsageInfo::default(),
            stop_reason: StopReason::ToolUse,
            turns: 5,
            tool_calls_executed: 5,
            upstream: None,
            pending_external_calls: Vec::new(),
        };
        let (status, reason) = outcome_status(&outcome);
        assert_eq!(status, "incomplete");
        assert_eq!(reason, Some("max_tool_calls"));
    }

    #[test]
    fn parses_tool_choice_passthrough() {
        let mut b = body();
        b["tool_choice"] = json!({"type": "function", "name": "Read"});
        let parsed = parse_responses_request(&b).unwrap();
        assert_eq!(
            parsed.provider_request.provider_options["tool_choice"]["name"],
            "Read"
        );
    }

    #[test]
    fn parses_string_tool_choice() {
        let mut b = body();
        b["tool_choice"] = json!("none");
        let parsed = parse_responses_request(&b).unwrap();
        assert_eq!(
            parsed.provider_request.provider_options["tool_choice"],
            "none"
        );
    }

    #[test]
    fn thinking_text_is_capped_at_budget() {
        let mut builder = ResponsesItemBuilder::new();
        let big = "x".repeat(THINKING_TEXT_BUDGET + 100);
        builder.push(&LoopEvent::ThinkingDelta { thinking: big });
        builder.push(&LoopEvent::TurnEnd {
            stop_reason: StopReason::EndTurn,
        });
        builder.finalize();
        let item = &builder.items[0];
        assert_eq!(item["type"], "reasoning");
        let text = item["content"][0]["text"].as_str().unwrap();
        assert!(text.ends_with("…[truncated]"), "marker expected: {text:?}");
        // Capped to the budget plus the marker.
        assert!(text.len() <= THINKING_TEXT_BUDGET + 16);
        // Under-budget thinking passes through untouched.
        let mut small = ResponsesItemBuilder::new();
        small.push(&LoopEvent::ThinkingDelta {
            thinking: "think hard".into(),
        });
        small.push(&LoopEvent::TurnEnd {
            stop_reason: StopReason::EndTurn,
        });
        small.finalize();
        assert_eq!(small.items[0]["content"][0]["text"], "think hard");
    }

    #[test]
    fn responses_object_shape() {
        let obj = responses_object(
            "resp_1",
            "free/auto",
            vec![json!({"type": "message"})],
            &UsageInfo {
                input_tokens: 3,
                output_tokens: 4,
                reasoning_tokens: 1,
                ..UsageInfo::default()
            },
            "completed",
            None,
            None,
        );
        assert_eq!(obj["object"], "response");
        assert_eq!(obj["status"], "completed");
        assert_eq!(obj["output"][0]["type"], "message");
        assert_eq!(obj["usage"]["total_tokens"], 7);
        assert!(obj.get("incomplete_details").is_none());
    }
}
