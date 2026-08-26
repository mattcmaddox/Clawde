//! Provider registry assembly + axum route handlers for the gateway.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::Event;
use axum::response::{IntoResponse, Response, Sse};
use axum::routing::{get, post};
use axum::Router;
use clawde_api::provider_types::{ProviderRequest, StreamEvent};
use clawde_api::{LlmProvider, ProviderRegistry};
use serde_json::{json, Value};

use crate::auth::{validate_bearer, RateLimiter};
use crate::config::EffectiveGatewayConfig;
use crate::error::{map_provider_error, GatewayError};
use crate::translate::{parse_chat_completion_request, to_openai_response, StreamTranslator};

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
}

/// Build the axum Router.
pub fn build_router(state: GatewayState) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
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
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        total += serde_json::to_string(tools).map(|s| s.len()).unwrap_or(0) / 4;
    }
    if let Some(mt) = body.get("max_tokens").and_then(|v| v.as_u64()) {
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
    req.model = wire_model;

    if parsed.stream {
        handle_stream(state, provider, req).await
    } else {
        let resp = match provider.create_message(req).await {
            Ok(r) => r,
            Err(e) => return map_provider_error(&e).into_response(),
        };
        state.limiter.record_usage(&key, resp.usage.total());
        axum::Json(to_openai_response(&resp)).into_response()
    }
}

/// Streaming handler: `create_message_stream` -> SSE chunks.
async fn handle_stream(
    state: GatewayState,
    provider: Arc<dyn LlmProvider>,
    req: ProviderRequest,
) -> Response {
    let stream = match provider.create_message_stream(req).await {
        Ok(s) => s,
        Err(e) => return map_provider_error(&e).into_response(),
    };

    state.active_streams.fetch_add(1, Ordering::SeqCst);
    let active = state.active_streams.clone();
    let _guard = ActiveStreamGuard(active);

    let mut translator = StreamTranslator::new();
    let sse = async_stream::stream! {
        use futures::StreamExt;
        let mut stream = stream;
        while let Some(event) = stream.next().await {
            match event {
                Ok(ev) => {
                    for chunk in translator.push(&ev) {
                        match Event::default().json_data(chunk) {
                            Ok(e) => yield Ok::<_, std::convert::Infallible>(e),
                            Err(_) => {}
                        }
                    }
                    if matches!(ev, StreamEvent::MessageStop) {
                        yield Ok::<_, std::convert::Infallible>(Event::default().data("[DONE]"));
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
    };

    Sse::new(sse).into_response()
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

    if !config.allow_non_loopback && !config.listen.starts_with("127.0.0.1") {
        anyhow::bail!(
            "Refusing to bind non-loopback address {} without --allow-non-loopback",
            config.listen
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
