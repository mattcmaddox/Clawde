//! Provider registry assembly + axum route handlers for the gateway.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::Event;
use axum::response::{IntoResponse, Response, Sse};
use axum::routing::{get, post};
use axum::Router;
use clawde_api::provider_types::{ProviderRequest, StreamEvent};
use clawde_api::{LlmProvider, ProviderRegistry};
use serde_json::{json, Value};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::agent::{run_agent_loop, AgentConfig, AgentFailure, AgentOutcome, LoopEvent};
use crate::auth::{validate_bearer, RateLimiter};
use crate::config::EffectiveGatewayConfig;
use crate::context::OverflowCompactor;
use crate::error::{map_agent_failure, map_provider_error, GatewayError};
use crate::responses::{
    new_response_id, outcome_status, parse_responses_request, response_skeleton, responses_object,
    ParsedResponsesRequest, ResponsesItemBuilder,
};
use crate::session::{output_items_to_messages, ResponseSession, SessionStore};
use crate::tool_exec::GatewayToolExecutor;
use crate::translate::{
    agent_outcome_to_response, agent_stream_chunks, parse_chat_completion_request,
    to_openai_response, ParsedRequest, StreamTranslator,
};

/// Shared gateway state.
#[derive(Clone)]
pub struct GatewayState {
    pub config: EffectiveGatewayConfig,
    pub limiter: Arc<RateLimiter>,
    pub registry: Arc<ProviderRegistry>,
    /// Whether shutdown has begun (readiness drain).
    pub draining: Arc<AtomicBool>,
    /// Active SSE stream count.
    pub active_streams: Arc<AtomicUsize>,
    /// Maximum concurrent provider calls accepted by this gateway instance.
    pub in_flight: Arc<Semaphore>,
    /// Cancellation token used to force active streams after the grace period.
    pub force_cancel: tokio_util::sync::CancellationToken,
    /// Ephemeral response sessions for `previous_response_id` continuation (D5).
    pub sessions: Arc<SessionStore>,
}

/// Per-request agent-loop machinery. Present (agent mode) only when the
/// gateway config enables it or the client sent `max_tool_calls` with at
/// least one tool that maps to a built-in (plan §2 rule 2).
struct AgentRuntime {
    executor: Arc<GatewayToolExecutor>,
    config: AgentConfig,
    cancel: CancellationToken,
    compactor: OverflowCompactor,
}

impl AgentRuntime {
    /// Chat completions: agent mode activates only with a client cap AND a
    /// built-in-mapped tool, or an explicit gateway `agentMode` (relay stays
    /// the default, R9). Mixed turns yield everything (relay semantics).
    fn build(
        cfg: &EffectiveGatewayConfig,
        parsed: &ParsedRequest,
        cancel: CancellationToken,
    ) -> Option<Self> {
        let executor = build_executor(cfg, cancel.clone());
        let client_cap = parsed.max_tool_calls.filter(|m| *m > 0);
        let has_builtin = parsed
            .provider_request
            .tools
            .iter()
            .any(|t| executor.is_builtin(&t.name));
        if !(cfg.agent_mode || client_cap.is_some() && has_builtin) {
            return None;
        }
        let max_tool_calls = client_cap.unwrap_or(cfg.max_tool_calls).max(1);
        Some(Self::assemble(
            cfg,
            executor,
            max_tool_calls,
            parsed.parallel_tool_calls,
            None,
            true, // yield_mixed_turns (chat relay semantics)
            &parsed.provider_request.model,
            cancel,
        ))
    }

    /// Responses: the loop is the engine (no relay), so every request runs it
    /// with the configured cap default. Mixed turns execute internal calls and
    /// yield only external ones (`yield_mixed_turns: false`).
    fn build_responses(
        cfg: &EffectiveGatewayConfig,
        parsed: &ParsedResponsesRequest,
        cancel: CancellationToken,
    ) -> Self {
        let executor = build_executor(cfg, cancel.clone());
        let max_tool_calls = parsed
            .max_tool_calls
            .filter(|m| *m > 0)
            .unwrap_or(cfg.max_tool_calls)
            .max(1);
        Self::assemble(
            cfg,
            executor,
            max_tool_calls,
            parsed.parallel_tool_calls,
            parsed.allowed_tools.clone(),
            false,
            &parsed.provider_request.model,
            cancel,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn assemble(
        cfg: &EffectiveGatewayConfig,
        executor: GatewayToolExecutor,
        max_tool_calls: u32,
        parallel_tool_calls: bool,
        allowed_tools: Option<Vec<String>>,
        yield_mixed_turns: bool,
        model: &str,
        cancel: CancellationToken,
    ) -> Self {
        let config = AgentConfig {
            max_tool_calls,
            max_turns: max_tool_calls + 1,
            timeout_secs: cfg.request_timeout_secs,
            parallel_tool_calls,
            yield_mixed_turns,
            allowed_tools,
            ..AgentConfig::default()
        };
        let compactor = OverflowCompactor::new(model.to_string(), 4096, cfg.request_timeout_secs);
        Self {
            executor: Arc::new(executor),
            config,
            cancel,
            compactor,
        }
    }
}

/// Build the per-request executor from gateway config.
fn build_executor(cfg: &EffectiveGatewayConfig, cancel: CancellationToken) -> GatewayToolExecutor {
    GatewayToolExecutor::new(
        cfg.permission_mode,
        &cfg.workspace_paths,
        &session_id(),
        &cfg.builtin_tools,
        cancel,
    )
}

/// A per-request session id for shell-state isolation in the tool context.
fn session_id() -> String {
    format!(
        "gw-{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    )
}

/// Build the axum Router.
pub fn build_router(state: GatewayState) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/responses", post(responses))
        .route("/v1/models", get(list_models))
        .route("/v1/models/{id}", get(get_model))
        .route("/healthz", get(healthz))
        .route("/status", get(status))
        .with_state(state)
}

/// Build the provider registry for the gateway.
///
/// Registers the free composite (from the auth store) plus direct providers
/// that have credentials.
pub fn build_registry() -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::new();
    if let Some(free) = clawde_api::registry::runtime_provider_for("free") {
        registry.register(free);
    }
    registry
        .with_openai_if_key_set()
        .with_google_if_key_set()
        .with_azure_if_configured();
    Arc::new(registry)
}

/// Resolve a model string to a provider. Returns the provider and the wire
/// model id to send.
///
/// - `free/auto`, `auto`, `free`, `free/family/<slug>` and `<upstream>/<model>`
///   (free catalog member) -> the free composite (it routes internally).
/// - `<provider>/<model>` for a registered direct provider -> that provider.
/// - bare unknown names -> `model_not_found`.
pub fn resolve_model(
    registry: &ProviderRegistry,
    model: &str,
) -> Result<(Arc<dyn LlmProvider>, String), GatewayError> {
    let is_free_form = model == "free"
        || model == "auto"
        || model == "free/auto"
        || model.starts_with("free/")
        || clawde_api::FREE_CATALOG
            .iter()
            .any(|u| model.starts_with(&format!("{}/", u.id)));
    if is_free_form {
        if let Some(free) = registry.get(&clawde_core::ProviderId::new("free")) {
            return Ok((free.clone(), model.to_string()));
        }
        return Err(GatewayError::service_unavailable(
            "Free provider not configured (no keys found)",
        ));
    }

    if let Some((provider_id, rest)) = model.split_once('/') {
        let pid = clawde_core::ProviderId::from(provider_id);
        if let Some(p) = registry.get(&pid) {
            return Ok((p.clone(), rest.to_string()));
        }
    }

    Err(GatewayError::model_not_found(format!(
        "Unknown model '{model}'"
    )))
}

/// Estimate request tokens from the body for TPM accounting.
/// Handles both chat completions (`messages`) and Responses (`input`).
fn estimate_tokens(body: &Value) -> u64 {
    let mut total = 0usize;
    if let Some(msgs) = body.get("messages").and_then(|v| v.as_array()) {
        for m in msgs {
            if let Some(c) = m.get("content").and_then(|v| v.as_str()) {
                total += c.len() / 4;
            }
            if let Some(tc) = m.get("tool_calls").and_then(|v| v.as_array()) {
                for t in tc {
                    total += t.to_string().len() / 4;
                }
            }
        }
    }
    // Responses `input`: string or item array.
    if let Some(input) = body.get("input") {
        match input {
            Value::String(s) => total += s.len() / 4,
            Value::Array(items) => {
                for item in items {
                    if let Some(text) = item.get("content").and_then(|v| v.as_str()) {
                        total += text.len() / 4;
                    }
                    if let Some(args) = item.get("arguments").and_then(|v| v.as_str()) {
                        total += args.len() / 4;
                    }
                    if let Some(out) = item.get("output").and_then(|v| v.as_str()) {
                        total += out.len() / 4;
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(ins) = body.get("instructions").and_then(|v| v.as_str()) {
        total += ins.len() / 4;
    }
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        total += serde_json::to_string(tools).map(|s| s.len()).unwrap_or(0) / 4;
    }
    if let Some(mt) = body
        .get("max_tokens")
        .or_else(|| body.get("max_output_tokens"))
        .and_then(|v| v.as_u64())
    {
        total += mt as usize;
    }
    total as u64
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Extract the bearer key from headers, or return a 401 response.
fn auth_key(headers: &HeaderMap, allowed: &[String]) -> Result<String, GatewayError> {
    let header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    validate_bearer(header, allowed)
        .ok_or_else(|| GatewayError::unauthorized("Missing or invalid bearer key"))
}

/// `POST /v1/chat/completions` — non-streaming and streaming.
async fn chat_completions(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let key = match auth_key(&headers, &state.config.allowed_keys) {
        Ok(k) => k,
        Err(_) => {
            return GatewayError::unauthorized(
                "Missing or invalid bearer key. Set Authorization: Bearer <key>.",
            )
            .into_response()
        }
    };

    if state.draining.load(Ordering::Relaxed) {
        return GatewayError::service_unavailable("Gateway is shutting down").into_response();
    }

    let body: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return GatewayError::invalid_request(format!("invalid JSON: {e}")).into_response()
        }
    };

    let parsed = match parse_chat_completion_request(&body) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };

    // Rate limit (RPM + TPM estimate).
    let tokens_estimate = estimate_tokens(&body);
    match state.limiter.check(&key, tokens_estimate) {
        crate::auth::RateLimitOutcome::Allowed => {}
        crate::auth::RateLimitOutcome::RpmExhausted(secs) => {
            return GatewayError::rate_limited("Rate limit exceeded (requests/min)", secs)
                .into_response()
        }
        crate::auth::RateLimitOutcome::TpmExhausted(secs) => {
            return GatewayError::rate_limited("Rate limit exceeded (tokens/min)", secs)
                .into_response()
        }
    }

    // Resolve provider.
    let (provider, wire_model) =
        match resolve_model(&state.registry, &parsed.provider_request.model) {
            Ok((p, m)) => (p, m),
            Err(e) => return e.into_response(),
        };
    let mut req = parsed.provider_request.clone();
    req.model = wire_model.clone();

    // Agent-mode machinery (None keeps the request in relay mode).
    let agent = AgentRuntime::build(&state.config, &parsed, CancellationToken::new());

    if parsed.stream {
        handle_stream(state, provider, req, &parsed, key, tokens_estimate, agent).await
    } else if let Some(rt) = agent {
        let permit = match state.in_flight.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => {
                return GatewayError::service_unavailable("Gateway concurrency limit is closed")
                    .into_response()
            }
        };
        let result = run_agent_loop(
            provider.clone(),
            req.clone(),
            rt.executor,
            rt.config,
            rt.cancel,
            None,
            Some(rt.compactor),
        )
        .await;
        drop(permit);
        match result {
            Ok(outcome) => {
                state
                    .limiter
                    .record_usage(&key, tokens_estimate, outcome.usage.total());
                axum::Json(agent_outcome_to_response(&outcome, &wire_model)).into_response()
            }
            Err(failure) => {
                state.limiter.record_usage(&key, tokens_estimate, 0);
                map_agent_failure(&failure).into_response()
            }
        }
    } else {
        let permit = match state.in_flight.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => {
                return GatewayError::service_unavailable("Gateway concurrency limit is closed")
                    .into_response()
            }
        };
        let result = provider_call_with_timeout(
            provider.create_message(req),
            state.config.request_timeout_secs,
        )
        .await;
        drop(permit);
        let resp = match result {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                state.limiter.record_usage(&key, tokens_estimate, 0);
                return map_provider_error(&e).into_response();
            }
            Err(response) => {
                state.limiter.record_usage(&key, tokens_estimate, 0);
                return response;
            }
        };
        state
            .limiter
            .record_usage(&key, tokens_estimate, resp.usage.total());
        axum::Json(to_openai_response(&resp)).into_response()
    }
}

/// `POST /v1/responses` — the agent-native surface (Open Responses).
///
/// Every request runs the agent loop (no relay); internal tool calls execute
/// server-side and stream as items, external calls yield as `function_call`
/// items. `previous_response_id` hydrates the transcript from the in-memory
/// session store (D5), serialized per session (D11). Sessions are retained for
/// both `store: true` and `store: false` (D5).
async fn responses(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let key = match auth_key(&headers, &state.config.allowed_keys) {
        Ok(k) => k,
        Err(_) => {
            return GatewayError::unauthorized(
                "Missing or invalid bearer key. Set Authorization: Bearer <key>.",
            )
            .into_response()
        }
    };

    if state.draining.load(Ordering::Relaxed) {
        return GatewayError::service_unavailable("Gateway is shutting down").into_response();
    }

    let body: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return GatewayError::invalid_request(format!("invalid JSON: {e}")).into_response()
        }
    };
    let parsed = match parse_responses_request(&body) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };

    let tokens_estimate = estimate_tokens(&body);
    match state.limiter.check(&key, tokens_estimate) {
        crate::auth::RateLimitOutcome::Allowed => {}
        crate::auth::RateLimitOutcome::RpmExhausted(secs) => {
            return GatewayError::rate_limited("Rate limit exceeded (requests/min)", secs)
                .into_response()
        }
        crate::auth::RateLimitOutcome::TpmExhausted(secs) => {
            return GatewayError::rate_limited("Rate limit exceeded (tokens/min)", secs)
                .into_response()
        }
    }

    let (provider, wire_model) =
        match resolve_model(&state.registry, &parsed.provider_request.model) {
            Ok((p, m)) => (p, m),
            Err(e) => return e.into_response(),
        };
    let mut req = parsed.provider_request.clone();
    req.model = wire_model.clone();

    // Session continuation (D5/D11): sample over prev.input + prev.output +
    // new input; serialize concurrent continuations on the same id.
    let mut messages = parsed.provider_request.messages.clone();
    let continuation_guard = if let Some(prev_id) = &parsed.previous_response_id {
        match state.sessions.get(prev_id) {
            Some(prev) => {
                let guard = state.sessions.continuation_lock(prev_id).await;
                let mut transcript = prev.input.clone();
                transcript.extend(output_items_to_messages(&prev.output));
                transcript.extend(messages.clone());
                messages = transcript;
                Some(guard)
            }
            None => return GatewayError::previous_response_not_found(prev_id).into_response(),
        }
    } else {
        None
    };
    let _continuation_guard = continuation_guard;
    req.messages = messages;

    let cancel = CancellationToken::new();
    let rt = AgentRuntime::build_responses(&state.config, &parsed, cancel.clone());
    let response_id = new_response_id();

    if parsed.stream {
        handle_responses_stream(
            state,
            provider,
            req,
            &parsed,
            response_id,
            key,
            tokens_estimate,
            rt,
        )
        .await
    } else {
        let permit = match state.in_flight.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => {
                return GatewayError::service_unavailable("Gateway concurrency limit is closed")
                    .into_response()
            }
        };
        let result = run_responses_loop(provider, req, &rt, cancel, rt.compactor.clone()).await;
        drop(permit);
        match result {
            Ok((outcome, builder)) => {
                let (status, reason) = outcome_status(&outcome);
                state
                    .limiter
                    .record_usage(&key, tokens_estimate, outcome.usage.total());
                state.sessions.put(ResponseSession {
                    id: response_id.clone(),
                    input: parsed.provider_request.messages.clone(),
                    output: builder.items.clone(),
                    created_at: std::time::Instant::now(),
                });
                axum::Json(responses_object(
                    &response_id,
                    &wire_model,
                    builder.items,
                    &outcome.usage,
                    status,
                    reason,
                    None,
                ))
                .into_response()
            }
            Err(failure) => {
                state.limiter.record_usage(&key, tokens_estimate, 0);
                let error = json!({
                    "message": failure.message,
                    "type": "server_error",
                    "param": null,
                    "code": null,
                });
                axum::Json(responses_object(
                    &response_id,
                    &wire_model,
                    builder_items_or_empty(&failure),
                    &clawde_core::types::UsageInfo::default(),
                    "failed",
                    None,
                    Some(error),
                ))
                .into_response()
            }
        }
    }
}

/// Non-stream loop run with event collection for the Responses builder.
async fn run_responses_loop(
    provider: Arc<dyn LlmProvider>,
    req: ProviderRequest,
    rt: &AgentRuntime,
    cancel: CancellationToken,
    compactor: OverflowCompactor,
) -> Result<(AgentOutcome, ResponsesItemBuilder), AgentFailure> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<LoopEvent>();
    let outcome = run_agent_loop(
        provider,
        req,
        rt.executor.clone(),
        rt.config.clone(),
        cancel,
        Some(tx),
        Some(compactor),
    )
    .await;
    let mut builder = ResponsesItemBuilder::new();
    while let Ok(ev) = rx.try_recv() {
        builder.push(&ev);
    }
    builder.finalize();
    outcome.map(|o| (o, builder))
}

/// Partial items for a failed response (D10: fail with the partial transcript).
fn builder_items_or_empty(failure: &AgentFailure) -> Vec<Value> {
    failure
        .partial
        .as_deref()
        .map(|m| vec![json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": m.get_all_text()}]})])
        .unwrap_or_default()
}

/// Streaming handler: relay `create_message_stream` -> SSE chunks, or the
/// agent loop with silent intermediate turns (D1).
async fn handle_stream(
    state: GatewayState,
    provider: Arc<dyn LlmProvider>,
    req: ProviderRequest,
    parsed: &ParsedRequest,
    key: String,
    tokens_estimate: u64,
    agent: Option<AgentRuntime>,
) -> Response {
    let permit = match state.in_flight.clone().acquire_owned().await {
        Ok(permit) => permit,
        Err(_) => {
            return GatewayError::service_unavailable("Gateway concurrency limit is closed")
                .into_response()
        }
    };
    if let Some(rt) = agent {
        return handle_agent_stream(
            state,
            provider,
            req,
            parsed,
            UsageAccount {
                key,
                estimate: tokens_estimate,
            },
            permit,
            rt,
        )
        .await;
    }
    let include_usage = parsed.include_usage;
    let stream = match provider_call_with_timeout(
        provider.create_message_stream(req),
        state.config.request_timeout_secs,
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            drop(permit);
            state.limiter.record_usage(&key, tokens_estimate, 0);
            return map_provider_error(&e).into_response();
        }
        Err(response) => {
            drop(permit);
            state.limiter.record_usage(&key, tokens_estimate, 0);
            return response;
        }
    };

    state.active_streams.fetch_add(1, Ordering::SeqCst);
    let active = state.active_streams.clone();
    let limiter = state.limiter.clone();
    let force_cancel = state.force_cancel.clone();
    let mut translator = StreamTranslator::new(include_usage);
    let sse = async_stream::stream! {
        use futures::StreamExt;
        let _guard = ActiveStreamGuard(active);
        let _permit = permit;
        let mut stream = stream;
        let mut final_usage = None;
        let mut usage_reconciled = false;
        while let Some(event) = tokio::select! {
            event = stream.next() => event,
            _ = force_cancel.cancelled() => None,
        } {
            match event {
                Ok(ev) => {
                    for chunk in translator.push(&ev) {
                        match Event::default().json_data(chunk) {
                            Ok(e) => yield Ok::<_, std::convert::Infallible>(e),
                            Err(_) => {}
                        }
                    }
                    if let StreamEvent::MessageDelta { usage: Some(usage), .. } = &ev {
                        final_usage = Some(usage.clone());
                    }
                    // A mid-stream Error event also terminates the SSE stream
                    // cleanly with [DONE] (OpenAI closes the stream on error).
                    let terminal = matches!(
                        ev,
                        StreamEvent::MessageStop | StreamEvent::Error { .. }
                    );
                    if terminal {
                        if let Some(usage) = final_usage.take() {
                            limiter.record_usage(&key, tokens_estimate, usage.total());
                            usage_reconciled = true;
                        }
                        yield Ok::<_, std::convert::Infallible>(
                            Event::default().data("[DONE]"),
                        );
                        break;
                    }
                }
                Err(e) => {
                    let ge = map_provider_error(&e);
                    if let Ok(ev) = Event::default().json_data(json!({
                        "error": {"message": ge.message}
                    })) {
                        yield Ok::<_, std::convert::Infallible>(ev);
                    }
                    break;
                }
            }
        }
        // A transport may end without a MessageStop. Do not leave the
        // request estimate charged forever, but only reconcile when the
        // provider supplied authoritative usage.
        if !usage_reconciled {
            if let Some(usage) = final_usage {
                limiter.record_usage(&key, tokens_estimate, usage.total());
            } else {
                // Failed, cancelled, or usage-less streams must not consume
                // the request estimate permanently.
                limiter.record_usage(&key, tokens_estimate, 0);
            }
        }
    };

    Sse::new(sse).into_response()
}

/// Per-request usage-accounting handle (key + token estimate for TPM).
struct UsageAccount {
    key: String,
    estimate: u64,
}

/// Agent-mode streaming: run the loop to completion, then render ONLY the
/// final turn (D1 — silent intermediate turns). Internal tool executions
/// never surface as SSE chunks; external (yielded) calls stream exactly as
/// relay mode does (`finish_reason: tool_calls`). A client disconnect drops
/// the generator, which cancels the per-request token so in-flight tool
/// execution is aborted (D16).
async fn handle_agent_stream(
    state: GatewayState,
    provider: Arc<dyn LlmProvider>,
    req: ProviderRequest,
    parsed: &ParsedRequest,
    account: UsageAccount,
    permit: OwnedSemaphorePermit,
    rt: AgentRuntime,
) -> Response {
    let AgentRuntime {
        executor,
        config,
        cancel,
        compactor,
    } = rt;
    let include_usage = parsed.include_usage;
    // The executor is an Arc so the SSE generator owns it ('static + Send).
    let active = state.active_streams.clone();
    let limiter = state.limiter.clone();
    let force_cancel = state.force_cancel.clone();
    let model = req.model.clone();
    let UsageAccount {
        key,
        estimate: tokens_estimate,
    } = account;

    let sse = async_stream::stream! {
        let _guard = ActiveStreamGuard(active);
        let _permit = permit;
        let _cancel_on_drop = CancelOnDrop(cancel.clone());

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<LoopEvent>();
        let mut events: Vec<LoopEvent> = Vec::new();
        let outcome = tokio::select! {
            outcome = run_agent_loop(
                provider,
                req,
                executor,
                config,
                cancel.clone(),
                Some(tx),
                Some(compactor),
            ) => outcome,
            _ = force_cancel.clone().cancelled_owned() => Err(AgentFailure::cancelled()),
        };
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }

        match &outcome {
            Ok(outcome) => {
                limiter.record_usage(&key, tokens_estimate, outcome.usage.total());
                for chunk in agent_stream_chunks(outcome, &events, include_usage, &model) {
                    match Event::default().json_data(chunk) {
                        Ok(e) => yield Ok::<_, std::convert::Infallible>(e),
                        Err(_) => {}
                    }
                }
            }
            Err(failure) => {
                limiter.record_usage(&key, tokens_estimate, 0);
                let ge = map_agent_failure(failure);
                if let Ok(ev) = Event::default().json_data(json!({
                    "error": {"message": ge.message}
                })) {
                    yield Ok::<_, std::convert::Infallible>(ev);
                }
            }
        }
        yield Ok::<_, std::convert::Infallible>(Event::default().data("[DONE]"));
    };

    Sse::new(sse).into_response()
}

/// Agent-mode Responses streaming: semantic events (Open Responses) stream
/// incrementally as the loop runs — items, text deltas, tool calls AND their
/// outputs all surface (unlike chat completions' silent turns, D1). Terminates
/// with `response.completed` / `response.incomplete` / `response.failed` and
/// `[DONE]`. A client disconnect drops the generator, which cancels the
/// per-request token so in-flight tool execution is aborted (D16).
#[allow(clippy::too_many_arguments)]
async fn handle_responses_stream(
    state: GatewayState,
    provider: Arc<dyn LlmProvider>,
    req: ProviderRequest,
    parsed: &ParsedResponsesRequest,
    response_id: String,
    key: String,
    tokens_estimate: u64,
    rt: AgentRuntime,
) -> Response {
    let AgentRuntime {
        executor,
        config,
        cancel,
        compactor,
    } = rt;
    let active = state.active_streams.clone();
    let limiter = state.limiter.clone();
    let force_cancel = state.force_cancel.clone();
    let sessions = state.sessions.clone();
    let model = req.model.clone();
    let input_messages = parsed.provider_request.messages.clone();

    let sse = async_stream::stream! {
        let _guard = ActiveStreamGuard(active);
        let _cancel_on_drop = CancelOnDrop(cancel.clone());

        let skeleton = response_skeleton(&response_id, &model);
        yield Ok::<_, std::convert::Infallible>(Event::default().json_data(json!({
            "type": "response.created",
            "response": skeleton,
        })).unwrap_or_default());
        yield Ok::<_, std::convert::Infallible>(Event::default().json_data(json!({
            "type": "response.in_progress",
            "response": response_skeleton(&response_id, &model),
        })).unwrap_or_default());

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<LoopEvent>();
        let mut builder = ResponsesItemBuilder::new();
        let run = run_agent_loop(
            provider,
            req,
            executor,
            config,
            cancel.clone(),
            Some(tx),
            Some(compactor),
        );
        tokio::pin!(run);
        let outcome = loop {
            tokio::select! {
                biased;
                result = &mut run => break result,
                ev = rx.recv() => {
                    match ev {
                        Some(ev) => {
                            for chunk in builder.push(&ev) {
                                if let Ok(e) = Event::default().json_data(chunk) {
                                    yield Ok::<_, std::convert::Infallible>(e);
                                }
                            }
                        }
                        // All senders dropped but run not yet ready: yield so
                        // the loop future can make progress.
                        None => tokio::task::yield_now().await,
                    }
                }
                _ = force_cancel.clone().cancelled_owned() => {
                    break Err(AgentFailure::cancelled());
                }
            }
        };
        while let Ok(ev) = rx.try_recv() {
            for chunk in builder.push(&ev) {
                if let Ok(e) = Event::default().json_data(chunk) {
                    yield Ok::<_, std::convert::Infallible>(e);
                }
            }
        }
        for chunk in builder.finalize() {
            if let Ok(e) = Event::default().json_data(chunk) {
                yield Ok::<_, std::convert::Infallible>(e);
            }
        }

        match &outcome {
            Ok(outcome) => {
                limiter.record_usage(&key, tokens_estimate, outcome.usage.total());
                let (status, reason) = outcome_status(outcome);
                let obj = responses_object(
                    &response_id,
                    &model,
                    builder.items.clone(),
                    &outcome.usage,
                    status,
                    reason,
                    None,
                );
                sessions.put(ResponseSession {
                    id: response_id.clone(),
                    input: input_messages,
                    output: builder.items.clone(),
                    created_at: std::time::Instant::now(),
                });
                let event_type = if status == "completed" {
                    "response.completed"
                } else {
                    "response.incomplete"
                };
                if let Ok(e) = Event::default().json_data(json!({"type": event_type, "response": obj.clone()})) {
                    yield Ok::<_, std::convert::Infallible>(e);
                }
                // Terminal event with the full response object (Open Responses).
                if let Ok(e) = Event::default().json_data(json!({"type": "response.done", "response": obj})) {
                    yield Ok::<_, std::convert::Infallible>(e);
                }
            }
            Err(failure) => {
                limiter.record_usage(&key, tokens_estimate, 0);
                let error = json!({
                    "message": failure.message,
                    "type": "server_error",
                    "param": null,
                    "code": null,
                });
                let obj = responses_object(
                    &response_id,
                    &model,
                    builder.items.clone(),
                    &clawde_core::types::UsageInfo::default(),
                    "failed",
                    None,
                    Some(error),
                );
                if let Ok(e) = Event::default().json_data(json!({"type": "response.failed", "response": obj.clone()})) {
                    yield Ok::<_, std::convert::Infallible>(e);
                }
                if let Ok(e) = Event::default().json_data(json!({"type": "response.done", "response": obj})) {
                    yield Ok::<_, std::convert::Infallible>(e);
                }
            }
        }
        yield Ok::<_, std::convert::Infallible>(Event::default().data("[DONE]"));
    };

    Sse::new(sse).into_response()
}

/// Drop guard that cancels the per-request token when the SSE stream is
/// dropped (client disconnect / shutdown), so in-flight tool execution stops.
struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

async fn provider_call_with_timeout<T>(
    future: impl std::future::Future<Output = T>,
    timeout_secs: u64,
) -> Result<T, Response> {
    if timeout_secs == 0 {
        return Ok(future.await);
    }
    match tokio::time::timeout(Duration::from_secs(timeout_secs), future).await {
        Ok(result) => Ok(result),
        Err(_) => Err(GatewayError::timeout("Upstream request timed out").into_response()),
    }
}

/// `GET /v1/models` — synthetic free-catalog entries + registered providers.
async fn list_models(State(state): State<GatewayState>, headers: HeaderMap) -> Response {
    if auth_key(&headers, &state.config.allowed_keys).is_err() {
        return GatewayError::unauthorized("Missing or invalid bearer key").into_response();
    }

    let mut data: Vec<Value> = Vec::new();
    for upstream in clawde_api::FREE_CATALOG {
        data.push(json!({
            "id": format!("free/{}", upstream.id),
            "object": "model",
            "created": 0,
            "owned_by": upstream.title,
        }));
    }
    data.push(json!({
        "id": "free/auto",
        "object": "model",
        "created": 0,
        "owned_by": "clawde-free",
    }));
    for id in state.registry.provider_ids() {
        if let Some(p) = state.registry.get(id) {
            data.push(json!({
                "id": id.to_string(),
                "object": "model",
                "created": 0,
                "owned_by": p.name(),
            }));
        }
    }
    axum::Json(json!({"object": "list", "data": data})).into_response()
}

/// `GET /v1/models/{id}` — single model lookup.
async fn get_model(
    State(state): State<GatewayState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if auth_key(&headers, &state.config.allowed_keys).is_err() {
        return GatewayError::unauthorized("Missing or invalid bearer key").into_response();
    }
    let known = id == "free/auto"
        || clawde_api::FREE_CATALOG
            .iter()
            .any(|u| id == format!("free/{}", u.id))
        || state
            .registry
            .get(&clawde_core::ProviderId::from(id.as_str()))
            .is_some();
    if !known {
        return GatewayError::model_not_found(format!("Model '{id}' not found")).into_response();
    }
    axum::Json(json!({
        "id": id,
        "object": "model",
        "created": 0,
        "owned_by": "clawde",
    }))
    .into_response()
}

/// `GET /healthz` — liveness. Returns 503 while draining.
async fn healthz(State(state): State<GatewayState>) -> Response {
    if state.draining.load(Ordering::Relaxed) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({"status": "shutting_down"})),
        )
            .into_response();
    }
    axum::Json(json!({"status": "ok"})).into_response()
}

/// `GET /status` — key-ring/cooldown surface (auth-gated).
async fn status(State(state): State<GatewayState>, headers: HeaderMap) -> Response {
    if auth_key(&headers, &state.config.allowed_keys).is_err() {
        return GatewayError::unauthorized("Missing or invalid bearer key").into_response();
    }
    let summaries = state.registry.key_ring_summaries();
    axum::Json(json!({
        "providers": summaries,
        "active_streams": state.active_streams.load(Ordering::SeqCst),
    }))
    .into_response()
}

/// Drop guard that decrements the active-stream counter.
struct ActiveStreamGuard(Arc<AtomicUsize>);
impl Drop for ActiveStreamGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Build the full gateway state + app and serve until cancelled.
///
/// This is the entry point used by both `clawde serve` (CLI) and the
/// standalone `clawde-gateway` binary. It wires signal handlers, builds the
/// registry, and serves with graceful shutdown (SSE drain).
pub async fn run_gateway(config: &EffectiveGatewayConfig) -> anyhow::Result<()> {
    use crate::shutdown::{install_signal_handlers, ShutdownCoordinator};

    if !config.allow_non_loopback {
        let is_loopback = config
            .listen
            .parse::<std::net::SocketAddr>()
            .map(|addr| addr.ip().is_loopback())
            .unwrap_or(false);
        if !is_loopback {
            anyhow::bail!(
                "Refusing to bind non-loopback address {} without --allow-non-loopback",
                config.listen
            );
        }
    }
    if config.tls_cert_path.is_some() || config.tls_key_path.is_some() {
        anyhow::bail!(
            "TLS configuration is not supported by the current gateway listener; use a TLS reverse proxy"
        );
    }
    let registry = build_registry();
    let coordinator = ShutdownCoordinator::new(config.shutdown_grace_secs);
    install_signal_handlers(coordinator.cancel.clone());

    let state = GatewayState {
        config: config.clone(),
        limiter: Arc::new(RateLimiter::new(config.rpm, config.tpm)),
        registry,
        draining: coordinator.draining.clone(),
        active_streams: coordinator.active_streams.clone(),
        in_flight: Arc::new(Semaphore::new(config.max_in_flight_per_upstream.max(1))),
        force_cancel: coordinator.force_cancel.clone(),
        sessions: Arc::new(SessionStore::new(
            config.session_capacity,
            config.session_ttl_secs,
        )),
    };

    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(&config.listen).await?;
    tracing::info!("gateway listening on http://{}", config.listen);

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            coordinator.cancel.cancelled().await;
            coordinator.begin_shutdown().await;
        })
        .await?;
    Ok(())
}
