//! Integration tests for chat-completions agent mode (Phase 1).
//!
//! Covers: server-side built-in tool execution (non-stream + stream with
//! silent intermediate turns, D1), external tool yielding, permission-deny
//! short-circuiting, relay-mode default, and the overflow -> compaction ->
//! retry path (D13).

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

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
    /// Emit a provider error instead of a normal turn.
    error: Option<ProviderError>,
}

impl ScriptedTurn {
    fn text(text: &str) -> Self {
        Self {
            tool_calls: Vec::new(),
            text: text.to_string(),
            error: None,
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
            error: None,
        }
    }

    fn error(err: ProviderError) -> Self {
        Self {
            tool_calls: Vec::new(),
            text: String::new(),
            error: Some(err),
        }
    }
}

struct ScriptedAgentProvider {
    id: ProviderId,
    script: Vec<ScriptedTurn>,
    /// Non-streaming response text (used by the compaction summariser and the
    /// relay non-stream path).
    create_message_text: String,
    /// Total `create_message_stream` dispatches.
    dispatches: AtomicUsize,
    /// `create_message` calls (the compaction summariser / relay).
    creates: AtomicUsize,
}

#[async_trait]
impl LlmProvider for ScriptedAgentProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn name(&self) -> &str {
        "scripted-agent-provider"
    }

    async fn create_message(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        Ok(ProviderResponse {
            id: "scripted-summary".to_string(),
            content: vec![ContentBlock::Text {
                text: self.create_message_text.clone(),
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
        let idx = self.dispatches.fetch_add(1, Ordering::SeqCst);
        let turn = self
            .script
            .get(idx)
            .cloned()
            .unwrap_or_else(|| ScriptedTurn::text("script exhausted"));

        if let Some(err) = turn.error {
            return Err(err);
        }

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
        sessions: Arc::new(clawde_gateway::session::SessionStore::new(16, 3600)),
        config,
    }
}

fn default_config() -> EffectiveGatewayConfig {
    EffectiveGatewayConfig {
        allowed_keys: vec!["gateway-test-key".to_string()],
        ..Default::default()
    }
}

fn register(provider: Arc<ScriptedAgentProvider>) -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    registry.register(provider);
    registry
}

/// Request body that declares the built-in `Read` tool and an agent cap.
fn agent_body(extra: Value) -> Value {
    let mut body = json!({
        "model": "scripted/model",
        "messages": [{"role": "user", "content": "read the file and report"}],
        "tools": [{
            "type": "function",
            "function": {
                "name": "Read",
                "description": "Read a file",
                "parameters": {"type": "object", "properties": {"path": {"type": "string"}}},
            }
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
                .uri("/v1/chat/completions")
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
        json!({"path": "/nonexistent/clawde-agent-mode-test-file"}),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn agent_mode_executes_builtin_then_returns_final_message() {
    let provider = Arc::new(ScriptedAgentProvider {
        id: ProviderId::new("scripted"),
        script: vec![
            missing_file_call(),
            ScriptedTurn::text("file missing, but done"),
        ],
        create_message_text: "summary".to_string(),
        dispatches: AtomicUsize::new(0),
        creates: AtomicUsize::new(0),
    });
    let (status, body) = post(
        state_with(default_config(), register(provider.clone())),
        agent_body(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("JSON body");
    // The tool call was executed server-side and never surfaced to the client.
    assert_eq!(
        json["choices"][0]["message"]["content"],
        "file missing, but done"
    );
    assert_eq!(json["choices"][0]["finish_reason"], "stop");
    assert!(json["choices"][0]["message"].get("tool_calls").is_none());
    // Two dispatches: the tool turn and the final text turn.
    assert_eq!(provider.dispatches.load(Ordering::SeqCst), 2);
    // Usage is the aggregate across turns (5+5 input, 7+7 output).
    assert_eq!(json["usage"]["prompt_tokens"], 10);
    assert_eq!(json["usage"]["completion_tokens"], 14);
}

#[tokio::test]
async fn agent_mode_streams_only_final_turn() {
    let provider = Arc::new(ScriptedAgentProvider {
        id: ProviderId::new("scripted"),
        script: vec![
            missing_file_call(),
            ScriptedTurn::text("streamed final answer"),
        ],
        create_message_text: "summary".to_string(),
        dispatches: AtomicUsize::new(0),
        creates: AtomicUsize::new(0),
    });
    let (status, body) = post(
        state_with(default_config(), register(provider.clone())),
        agent_body(json!({"stream": true, "stream_options": {"include_usage": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Silent intermediate turns (D1): no tool_calls deltas anywhere, only the
    // final text turn streams, then a terminal stop + usage chunk + [DONE].
    assert!(body.contains("streamed final answer"), "body: {body}");
    assert!(
        !body.contains("tool_calls"),
        "internal tool calls must not stream: {body}"
    );
    assert!(body.contains("\"finish_reason\":\"stop\""), "body: {body}");
    assert!(
        body.contains("\"usage\""),
        "include_usage chunk expected: {body}"
    );
    assert!(body.trim_end().ends_with("data: [DONE]"), "body: {body}");
    assert_eq!(provider.dispatches.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn agent_mode_yields_external_calls() {
    let provider = Arc::new(ScriptedAgentProvider {
        id: ProviderId::new("scripted"),
        script: vec![ScriptedTurn::tool_call(
            "get_weather",
            "call_1",
            json!({"city": "SF"}),
        )],
        create_message_text: "summary".to_string(),
        dispatches: AtomicUsize::new(0),
        creates: AtomicUsize::new(0),
    });
    let (status, body) = post(
        state_with(default_config(), register(provider.clone())),
        agent_body(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("JSON body");
    let tc = &json["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(tc["function"]["name"], "get_weather");
    assert!(tc["function"]["arguments"].as_str().unwrap().contains("SF"));
    assert_eq!(json["choices"][0]["finish_reason"], "tool_calls");
    // Yielded — the loop stopped after one dispatch (no re-dispatch).
    assert_eq!(provider.dispatches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn external_yield_streams_tool_call_deltas() {
    let provider = Arc::new(ScriptedAgentProvider {
        id: ProviderId::new("scripted"),
        script: vec![ScriptedTurn::tool_call(
            "get_weather",
            "call_1",
            json!({"city": "SF"}),
        )],
        create_message_text: "summary".to_string(),
        dispatches: AtomicUsize::new(0),
        creates: AtomicUsize::new(0),
    });
    let (status, body) = post(
        state_with(default_config(), register(provider.clone())),
        agent_body(json!({"stream": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // External (yielded) calls stream exactly as relay mode does.
    assert!(body.contains("\"tool_calls\""), "body: {body}");
    assert!(body.contains("get_weather"), "body: {body}");
    assert!(
        body.contains("\"finish_reason\":\"tool_calls\""),
        "body: {body}"
    );
    assert!(body.trim_end().ends_with("data: [DONE]"), "body: {body}");
}

#[tokio::test]
async fn permission_deny_short_circuits_internal_tools() {
    let provider = Arc::new(ScriptedAgentProvider {
        id: ProviderId::new("scripted"),
        script: vec![missing_file_call(), ScriptedTurn::text("denied, reported")],
        create_message_text: "summary".to_string(),
        dispatches: AtomicUsize::new(0),
        creates: AtomicUsize::new(0),
    });
    let config = EffectiveGatewayConfig {
        permission_mode: clawde_gateway::GatewayPermissionMode::Deny,
        ..default_config()
    };
    let (status, body) = post(
        state_with(config, register(provider.clone())),
        agent_body(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("JSON body");
    assert_eq!(json["choices"][0]["message"]["content"], "denied, reported");
    // The deny handler turned the tool call into an error observation; the
    // loop continued (2 dispatches).
    assert_eq!(provider.dispatches.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn chat_allowed_tools_denied_becomes_error_observation() {
    let provider = Arc::new(ScriptedAgentProvider {
        id: ProviderId::new("scripted"),
        script: vec![
            missing_file_call(),
            ScriptedTurn::text("read is not allowed"),
        ],
        create_message_text: "summary".to_string(),
        dispatches: AtomicUsize::new(0),
        creates: AtomicUsize::new(0),
    });
    // D6 on chat completions: the client whitelists only `Other`, so the
    // model's `Read` call becomes a tool_error observation and is never
    // executed; the loop continues and the model self-corrects.
    let (status, body) = post(
        state_with(default_config(), register(provider.clone())),
        agent_body(json!({"allowed_tools": ["Other"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let json: Value = serde_json::from_str(&body).expect("JSON body");
    assert_eq!(
        json["choices"][0]["message"]["content"],
        "read is not allowed"
    );
    assert_eq!(json["choices"][0]["finish_reason"], "stop");
    // Two dispatches: the denied-call turn, then the recovery turn. The
    // denied call never surfaced as a yielded tool_call.
    assert_eq!(provider.dispatches.load(Ordering::SeqCst), 2);
    assert!(json["choices"][0]["message"].get("tool_calls").is_none());
}

#[tokio::test]
async fn relay_mode_stays_default_without_agent_knobs() {
    let provider = Arc::new(ScriptedAgentProvider {
        id: ProviderId::new("scripted"),
        script: vec![],
        create_message_text: "relay reply".to_string(),
        dispatches: AtomicUsize::new(0),
        creates: AtomicUsize::new(0),
    });
    // Declares a built-in-mapped tool but NO max_tool_calls and no agentMode:
    // the request stays in relay mode (single non-stream dispatch).
    let mut body = agent_body(json!({}));
    body.as_object_mut().unwrap().remove("max_tool_calls");
    let (status, body) = post(
        state_with(default_config(), register(provider.clone())),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("JSON body");
    assert_eq!(json["choices"][0]["message"]["content"], "relay reply");
    assert_eq!(provider.dispatches.load(Ordering::SeqCst), 0);
    assert_eq!(provider.creates.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn overflow_triggers_compaction_and_retries() {
    let provider = Arc::new(ScriptedAgentProvider {
        id: ProviderId::new("scripted"),
        // Dispatch 0 is the tool turn; dispatch 1 overflows (the transcript is
        // now far above the keep-recent budget because the user message is
        // huge); stage 1 summarises it via create_message; dispatch 2 finishes.
        script: vec![
            missing_file_call(),
            ScriptedTurn::error(ProviderError::ContextOverflow {
                provider: ProviderId::new("scripted"),
                message: "context window exceeded".to_string(),
                max_tokens: Some(4096),
            }),
            ScriptedTurn::text("recovered after compaction"),
        ],
        create_message_text: "<summary>1. Request: read the file and report</summary>".to_string(),
        dispatches: AtomicUsize::new(0),
        creates: AtomicUsize::new(0),
    });
    let mut body = agent_body(json!({}));
    body["messages"][0]["content"] =
        json!(format!("read the file and report {}", "x".repeat(200_000)));
    let (status, body) = post(
        state_with(default_config(), register(provider.clone())),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let json: Value = serde_json::from_str(&body).expect("JSON body");
    assert_eq!(
        json["choices"][0]["message"]["content"],
        "recovered after compaction"
    );
    assert_eq!(json["choices"][0]["finish_reason"], "stop");
    // 3 stream dispatches (tool turn, overflow, final retry) + 1 summariser call.
    assert_eq!(provider.dispatches.load(Ordering::SeqCst), 3);
    assert_eq!(provider.creates.load(Ordering::SeqCst), 1);
}
