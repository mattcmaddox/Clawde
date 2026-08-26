//! Integration tests for `POST /v1/responses` (Phase 2).
//!
//! Covers: server-side built-in tool execution with the item transcript,
//! semantic item-event streaming, external tool yielding, `allowed_tools`
//! enforcement (D6), cap exhaustion -> incomplete, `previous_response_id`
//! continuation (D5/D11), and `n > 1` rejection (D7).

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use clawde_api::provider_types::{
    ProviderCapabilities, ProviderRequest, ProviderResponse, ProviderStatus, StopReason,
    StreamEvent, SystemPromptStyle,
};
use clawde_api::{LlmProvider, ProviderError, ProviderRegistry};
use clawde_core::provider_id::ProviderId;
use clawde_core::types::{ContentBlock, UsageInfo};
use clawde_gateway::auth::RateLimiter;
use clawde_gateway::config::EffectiveGatewayConfig;
use clawde_gateway::router::{build_router, GatewayState};
use clawde_gateway::session::SessionStore;
use futures::Stream;
use serde_json::{json, Value};
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Scripted streaming provider
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ScriptedToolCall {
    id: String,
    name: String,
    input: Value,
}

#[derive(Clone)]
struct ScriptedTurn {
    tool_calls: Vec<ScriptedToolCall>,
    text: String,
}

impl ScriptedTurn {
    fn text(text: &str) -> Self {
        Self {
            tool_calls: Vec::new(),
            text: text.to_string(),
        }
    }

    fn tool_call(name: &str, id: &str, input: Value) -> Self {
        Self {
            tool_calls: vec![ScriptedToolCall {
                id: id.to_string(),
                name: name.to_string(),
                input,
            }],
            text: String::new(),
        }
    }
}

struct ScriptedResponsesProvider {
    id: ProviderId,
    script: Vec<ScriptedTurn>,
    /// Messages received per `create_message_stream` dispatch, for the
    /// continuation test (records transcript growth).
    seen_message_counts: Arc<Mutex<Vec<usize>>>,
    /// Total `create_message_stream` dispatches.
    dispatches: AtomicUsize,
}

#[async_trait]
impl LlmProvider for ScriptedResponsesProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn name(&self) -> &str {
        "scripted-responses-provider"
    }

    async fn create_message(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        Ok(ProviderResponse {
            id: "scripted-response".to_string(),
            content: vec![ContentBlock::Text {
                text: "summary".to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: UsageInfo {
                input_tokens: 1,
                output_tokens: 1,
                ..UsageInfo::default()
            },
            model: request.model,
            rate_limit: None,
        })
    }

    async fn create_message_stream(
        &self,
        request: ProviderRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        self.seen_message_counts
            .lock()
            .unwrap()
            .push(request.messages.len());
        let idx = self.dispatches.fetch_add(1, Ordering::SeqCst);
        let turn = self
            .script
            .get(idx)
            .cloned()
            .unwrap_or_else(|| ScriptedTurn::text("script exhausted"));

        let mut events: Vec<Result<StreamEvent, ProviderError>> = Vec::new();
        events.push(Ok(StreamEvent::MessageStart {
            id: format!("msg_{idx}"),
            model: request.model,
            usage: UsageInfo {
                input_tokens: 5,
                output_tokens: 0,
                ..UsageInfo::default()
            },
        }));

        let mut index = 0usize;
        for call in &turn.tool_calls {
            events.push(Ok(StreamEvent::ContentBlockStart {
                index,
                content_block: ContentBlock::ToolUse {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    input: Value::Null,
                    thought_signature: None,
                },
            }));
            let args = call.input.to_string();
            if !args.is_empty() {
                events.push(Ok(StreamEvent::InputJsonDelta {
                    index,
                    partial_json: args,
                }));
            }
            events.push(Ok(StreamEvent::ContentBlockStop { index }));
            index += 1;
        }
        if turn.tool_calls.is_empty() && !turn.text.is_empty() {
            events.push(Ok(StreamEvent::ContentBlockStart {
                index,
                content_block: ContentBlock::Text {
                    text: String::new(),
                },
            }));
            events.push(Ok(StreamEvent::TextDelta {
                index,
                text: turn.text.clone(),
            }));
            events.push(Ok(StreamEvent::ContentBlockStop { index }));
        }
        events.push(Ok(StreamEvent::MessageDelta {
            stop_reason: Some(if turn.tool_calls.is_empty() {
                StopReason::EndTurn
            } else {
                StopReason::ToolUse
            }),
            usage: Some(UsageInfo {
                input_tokens: 5,
                output_tokens: 7,
                ..UsageInfo::default()
            }),
        }));
        events.push(Ok(StreamEvent::MessageStop));

        Ok(Box::pin(futures::stream::iter(events)))
    }

    async fn health_check(&self) -> Result<ProviderStatus, ProviderError> {
        Ok(ProviderStatus::Healthy)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            thinking: false,
            image_input: false,
            pdf_input: false,
            audio_input: false,
            video_input: false,
            caching: false,
            structured_output: false,
            system_prompt_style: SystemPromptStyle::SystemMessage,
        }
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn state_with(config: EffectiveGatewayConfig, registry: ProviderRegistry) -> GatewayState {
    GatewayState {
        limiter: Arc::new(RateLimiter::new(100, 100_000)),
        registry: Arc::new(registry),
        draining: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        active_streams: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        in_flight: Arc::new(tokio::sync::Semaphore::new(8)),
        force_cancel: tokio_util::sync::CancellationToken::new(),
        sessions: Arc::new(SessionStore::new(16, 3600)),
        config,
    }
}

fn default_config() -> EffectiveGatewayConfig {
    EffectiveGatewayConfig {
        allowed_keys: vec!["gateway-test-key".to_string()],
        ..Default::default()
    }
}

fn provider(script: Vec<ScriptedTurn>) -> Arc<ScriptedResponsesProvider> {
    Arc::new(ScriptedResponsesProvider {
        id: ProviderId::new("scripted"),
        script,
        seen_message_counts: Arc::new(Mutex::new(Vec::new())),
        dispatches: AtomicUsize::new(0),
    })
}

fn register(provider: Arc<ScriptedResponsesProvider>) -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    registry.register(provider);
    registry
}

/// Request body declaring the built-in `Read` tool and an agent cap.
fn responses_body(extra: Value) -> Value {
    let mut body = json!({
        "model": "scripted/model",
        "input": [{"role": "user", "content": [{"type": "input_text", "text": "read the file and report"}]}],
        "tools": [{
            "type": "function",
            "name": "Read",
            "description": "Read a file",
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}}},
        }],
        "max_tool_calls": 5,
    });
    if let Value::Object(map) = &mut body {
        if let Value::Object(extra_map) = extra {
            for (k, v) in extra_map {
                map.insert(k.clone(), v);
            }
        }
    }
    body
}

async fn post(state: GatewayState, body: Value) -> (StatusCode, String) {
    let response = build_router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("authorization", "Bearer gateway-test-key")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("request builds"),
        )
        .await
        .expect("router responds");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("response body reads");
    (
        status,
        String::from_utf8(bytes.to_vec()).expect("body is UTF-8"),
    )
}

fn missing_file_call() -> ScriptedTurn {
    ScriptedTurn::tool_call(
        "Read",
        "call_1",
        json!({"file_path": "/nonexistent/clawde-responses-test-file"}),
    )
}

fn output_types(items: &Value) -> Vec<&str> {
    items
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|i| i.get("type").and_then(|t| t.as_str()))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn responses_executes_builtin_then_returns_item_transcript() {
    let prov = provider(vec![
        missing_file_call(),
        ScriptedTurn::text("file missing, but done"),
    ]);
    let (status, body) = post(
        state_with(default_config(), register(prov.clone())),
        responses_body(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let json: Value = serde_json::from_str(&body).expect("JSON body");
    assert_eq!(json["object"], "response");
    assert_eq!(json["status"], "completed");
    assert!(json["id"].as_str().unwrap().starts_with("resp_"));
    assert!(json["error"].is_null());
    // Item transcript: message + function_call + function_call_output + final message.
    let types = output_types(&json["output"]);
    assert_eq!(
        types,
        vec![
            "message",
            "function_call",
            "function_call_output",
            "message"
        ]
    );
    // The executed (internal) tool call is fully represented server-side.
    assert_eq!(json["output"][1]["name"], "Read");
    assert!(
        json["output"][2]["output"]
            .as_str()
            .unwrap()
            .starts_with("tool_error: Read: File not found"),
        "body: {body}"
    );
    // Final assistant message carries the answer text.
    assert_eq!(
        json["output"][3]["content"][0]["text"],
        "file missing, but done"
    );
    // Usage is the aggregate across turns (5+5 input, 7+7 output).
    assert_eq!(json["usage"]["input_tokens"], 10);
    assert_eq!(json["usage"]["output_tokens"], 14);
    assert_eq!(prov.dispatches.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn responses_streams_semantic_item_events() {
    let prov = provider(vec![
        missing_file_call(),
        ScriptedTurn::text("streamed final answer"),
    ]);
    let (status, body) = post(
        state_with(default_config(), register(prov.clone())),
        responses_body(json!({"stream": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    // Lifecycle events bracket the stream.
    assert!(
        body.contains("\"type\":\"response.created\""),
        "body: {body}"
    );
    assert!(
        body.contains("\"type\":\"response.in_progress\""),
        "body: {body}"
    );
    // Built-in tool items stream natively as response items (D1: per-tool
    // progress belongs on /v1/responses).
    assert!(
        body.contains("\"type\":\"response.output_item.added\""),
        "body: {body}"
    );
    assert!(
        body.contains("\"type\":\"response.function_call_arguments.delta\""),
        "body: {body}"
    );
    // Final turn text streams as output_text deltas.
    assert!(body.contains("streamed final answer"), "body: {body}");
    assert!(
        body.contains("\"type\":\"response.output_text.delta\""),
        "body: {body}"
    );
    // Terminal events: completed + done.
    assert!(
        body.contains("\"type\":\"response.completed\""),
        "body: {body}"
    );
    assert!(body.contains("\"type\":\"response.done\""), "body: {body}");
    assert!(body.trim_end().ends_with("data: [DONE]"), "body: {body}");
    assert_eq!(prov.dispatches.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn responses_yields_external_tool_call() {
    let prov = provider(vec![ScriptedTurn::tool_call(
        "get_weather",
        "call_1",
        json!({"city": "SF"}),
    )]);
    let (status, body) = post(
        state_with(default_config(), register(prov.clone())),
        responses_body(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let json: Value = serde_json::from_str(&body).expect("JSON body");
    assert_eq!(json["status"], "completed");
    let types = output_types(&json["output"]);
    assert_eq!(types, vec!["message", "function_call"]);
    // The yielded call is surfaced as a completed function_call item (its
    // arguments are fully emitted) the client can execute and continue.
    assert_eq!(json["output"][1]["name"], "get_weather");
    assert_eq!(json["output"][1]["status"], "completed");
    assert_eq!(json["output"][1]["call_id"], "call_1");
    // Yielded — the loop stopped after one dispatch (no re-dispatch).
    assert_eq!(prov.dispatches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn responses_allowed_tools_denied_becomes_error_observation() {
    let prov = provider(vec![
        missing_file_call(),
        ScriptedTurn::text("read is not allowed"),
    ]);
    let (status, body) = post(
        state_with(default_config(), register(prov.clone())),
        // The client whitelists only `Other`; the model's `Read` call must be
        // rejected with a tool_error observation (D6), not executed.
        responses_body(json!({"allowed_tools": ["Other"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let json: Value = serde_json::from_str(&body).expect("JSON body");
    assert_eq!(json["status"], "completed");
    let types = output_types(&json["output"]);
    assert_eq!(
        types,
        vec![
            "message",
            "function_call",
            "function_call_output",
            "message"
        ]
    );
    // The denied call became an error observation the model can self-correct
    // from; the file was never touched (error prefix).
    assert!(
        json["output"][2]["output"]
            .as_str()
            .unwrap()
            .starts_with("tool_error:"),
        "body: {body}"
    );
    assert_eq!(
        json["output"][3]["content"][0]["text"],
        "read is not allowed"
    );
    // Two dispatches: the denied call turn, then the recovery turn.
    assert_eq!(prov.dispatches.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn responses_cap_exhaustion_marks_incomplete() {
    let prov = provider(vec![
        missing_file_call(),
        missing_file_call(),
        ScriptedTurn::text("never reached"),
    ]);
    let (status, body) = post(
        state_with(default_config(), register(prov.clone())),
        // Cap of 1: the first tool call executes, the second hits the cap and
        // the loop force-stops without executing or yielding it (D9).
        responses_body(json!({"max_tool_calls": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let json: Value = serde_json::from_str(&body).expect("JSON body");
    assert_eq!(json["status"], "incomplete");
    assert_eq!(json["incomplete_details"]["reason"], "max_tool_calls");
    // Only the first call executed; the second was dropped at the cap.
    let types = output_types(&json["output"]);
    assert_eq!(
        types,
        vec!["message", "function_call", "function_call_output"]
    );
    assert_eq!(prov.dispatches.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn responses_continuation_hydrates_previous_turn() {
    let prov = provider(vec![
        missing_file_call(),
        ScriptedTurn::text("first answer"),
        ScriptedTurn::text("second answer"),
    ]);
    let state = state_with(default_config(), register(prov.clone()));

    let first = responses_body(json!({"store": true}));
    let (status, first_body) = post(state.clone(), first).await;
    assert_eq!(status, StatusCode::OK, "body: {first_body}");
    let first_json: Value = serde_json::from_str(&first_body).expect("JSON body");
    let response_id = first_json["id"].as_str().expect("response id").to_string();

    // Continue with `previous_response_id`; the transcript must hydrate as
    // prev.input + prev.output + new input (Open Responses semantics).
    let second = responses_body(json!({
        "previous_response_id": response_id,
        "input": [{"role": "user", "content": [{"type": "input_text", "text": "continue"}]}],
    }));
    let (status, second_body) = post(state.clone(), second).await;
    assert_eq!(status, StatusCode::OK, "body: {second_body}");
    let second_json: Value = serde_json::from_str(&second_body).expect("JSON body");
    assert_eq!(second_json["status"], "completed");
    // The final assistant message is the last output item.
    let types = output_types(&second_json["output"]);
    assert_eq!(types.last().copied(), Some("message"));
    assert_eq!(
        second_json["output"].as_array().unwrap().last().unwrap()["content"][0]["text"],
        "second answer"
    );

    // The second request hydrates prev.input + prev.output + new input:
    // [user] -> turn transcript (assistant tool_use + user tool_result) ->
    // prev.output replay (assistant tool_use, user tool_result, final
    // assistant) -> new [user].
    let counts = prov.seen_message_counts.lock().unwrap().clone();
    assert_eq!(counts.len(), 3, "three dispatches total: {counts:?}");
    assert_eq!(counts[0], 1);
    assert_eq!(counts[1], 3, "turn transcript appended: {counts:?}");
    assert_eq!(
        counts[2],
        counts[1] + 2,
        "prev output + new input: {counts:?}"
    );
}

#[tokio::test]
async fn responses_rejects_n_greater_than_one() {
    let prov = provider(vec![ScriptedTurn::text("nope")]);
    let (status, body) = post(
        state_with(default_config(), register(prov)),
        responses_body(json!({"n": 2})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let json: Value = serde_json::from_str(&body).expect("JSON body");
    assert!(
        json["error"]["message"].as_str().unwrap().contains("n"),
        "body: {body}"
    );
}
