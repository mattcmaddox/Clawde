//! Guest chat server (spec §6–§9): the "share a URL with friends" surface.
//!
//! Security posture (all learned from the Cline Kanban WebSocket-hijack
//! incident, spec §3b):
//! - Every request validates the `Origin` header when present (cross-origin
//!   browser requests are refused).
//! - Every stateful route requires a **capability-scoped device token** in an
//!   httpOnly + SameSite=Strict cookie — minted only after the shared password
//!   verifies.
//! - Wrong passwords are rate-limited with a per-IP lockout window.
//! - Per-link concurrency cap (default 2 simultaneous chats).
//! - The surface only renders chat + the guest's own session; there are no
//!   routes to boards, projects, settings, or host files (spec §7).

use crate::chat::{ChatEngine, GuestSession};
use crate::guest::{link_active, GuestStore, DEFAULT_MAX_CONCURRENT};
use axum::extract::{ConnectInfo, State};
use axum::http::header::{CONTENT_TYPE, ORIGIN, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub const DEFAULT_GUEST_PORT: u16 = 8789;
pub const GUEST_COOKIE: &str = "katban_guest";
/// Seconds a device token stays valid between visits (rolling).
pub const DEVICE_TTL_SECS: u64 = 90 * 24 * 3600;
/// Max device tokens kept per link; the oldest are dropped beyond this so a
/// long-lived link can't grow links.json without bound.
pub const MAX_DEVICES_PER_LINK: usize = 20;
/// Chat messages a link may send per 60s window (rate limit, not just
/// concurrency — a script with the shared password can't burn the host's
/// free-tier quota in a tight loop).
pub const CHAT_MAX_PER_MINUTE: u32 = 20;
/// Session summaries a link may generate per 60s window (each is a model
/// call).
pub const SUMMARY_MAX_PER_MINUTE: u32 = 2;
/// Idle time after which a guest's in-memory session is evicted (rolling).
pub const SESSION_TTL_SECS: u64 = 24 * 3600;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Per-token ephemeral session state (in memory only — spec §6.1).
#[derive(Default)]
pub(crate) struct LiveSession {
    chat: GuestSession,
    summary: Option<String>,
    /// Unix seconds of last access — idle sessions are evicted (see
    /// `sweep_sessions`) so a long-running server doesn't leak memory.
    pub(crate) last_used: u64,
}

/// One per-link fixed-window rate-limit bucket (in memory only).
#[derive(Default)]
pub(crate) struct RateWindow {
    /// Start of the current 60s window (unix seconds).
    pub(crate) window_start: u64,
    /// Calls counted in this window.
    pub(crate) count: u32,
}

pub struct GuestServer {
    pub(crate) store: Arc<Mutex<GuestStore>>,
    pub(crate) engine: Arc<ChatEngine>,
    pub(crate) sessions: Arc<tokio::sync::Mutex<HashMap<String, LiveSession>>>,
    /// link_id -> in-flight chat count (concurrency cap).
    pub(crate) inflight: Arc<tokio::sync::Mutex<HashMap<String, usize>>>,
    /// link_id -> chat-request rate bucket.
    pub(crate) chat_rate: Arc<tokio::sync::Mutex<HashMap<String, RateWindow>>>,
    /// link_id -> summary rate bucket.
    pub(crate) summary_rate: Arc<tokio::sync::Mutex<HashMap<String, RateWindow>>>,
    pub(crate) store_mtime: Arc<Mutex<Option<std::time::SystemTime>>>,
}

impl GuestServer {
    pub fn new(engine: Arc<ChatEngine>, store: Arc<Mutex<GuestStore>>) -> Self {
        let store_mtime = std::fs::metadata(crate::guest::links_path())
            .and_then(|metadata| metadata.modified())
            .ok();
        GuestServer {
            store,
            engine,
            sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            inflight: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            chat_rate: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            summary_rate: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            store_mtime: Arc::new(Mutex::new(store_mtime)),
        }
    }

    /// Serve until Ctrl-C / SIGTERM. Keeps axum inside this crate so the CLI
    /// never needs a direct axum dependency. `connect_info` makes the real
    /// TCP peer address available so `X-Forwarded-For` is only trusted from
    /// loopback proxies (caddy) — a direct LAN client can't spoof it.
    pub async fn run(&self, addr: SocketAddr) -> anyhow::Result<()> {
        let app = self
            .router()
            .into_make_service_with_connect_info::<SocketAddr>();
        let listener = tokio::net::TcpListener::bind(addr).await?;
        tracing::info!(%addr, "katban guest server listening");
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await?;
        Ok(())
    }

    pub fn router(&self) -> Router {
        let state = self.snapshot_state();
        Router::new()
            .route("/", get(index))
            .route("/auth", post(auth))
            .route("/chat", get(chat_page))
            .route("/api/chat", post(api_chat))
            .route("/api/summary", post(api_summary))
            .with_state(state)
    }

    fn snapshot_state(&self) -> GuestState {
        GuestState {
            store: self.store.clone(),
            engine: self.engine.clone(),
            sessions: self.sessions.clone(),
            inflight: self.inflight.clone(),
            chat_rate: self.chat_rate.clone(),
            summary_rate: self.summary_rate.clone(),
            store_mtime: self.store_mtime.clone(),
        }
    }
}

#[derive(Clone)]
struct GuestState {
    store: Arc<Mutex<GuestStore>>,
    engine: Arc<ChatEngine>,
    sessions: Arc<tokio::sync::Mutex<HashMap<String, LiveSession>>>,
    inflight: Arc<tokio::sync::Mutex<HashMap<String, usize>>>,
    chat_rate: Arc<tokio::sync::Mutex<HashMap<String, RateWindow>>>,
    summary_rate: Arc<tokio::sync::Mutex<HashMap<String, RateWindow>>>,
    /// mtime of links.json the in-memory store was loaded from, so CLI writes
    /// (`link revoke`, `guest unblock`) take effect without a restart.
    store_mtime: Arc<Mutex<Option<std::time::SystemTime>>>,
}

/// Reload the in-memory store from disk when links.json changed (a CLI
/// command like `link revoke` or `guest unblock` wrote it). The file is tiny
/// and this only runs on auth-touching requests. The server's own saves are
/// idempotent re-reads, so this never fights itself.
fn maybe_reload_store(state: &GuestState) {
    let path = crate::guest::links_path();
    let Ok(metadata) = std::fs::metadata(&path) else {
        return;
    };
    let Ok(modified) = metadata.modified() else {
        return;
    };
    let changed = {
        let mtime = state.store_mtime.lock().unwrap_or_else(|e| e.into_inner());
        mtime.is_some_and(|seen| seen != modified)
    };
    if !changed {
        return;
    }
    if let Ok(fresh) = crate::guest::load() {
        *state.store.lock().unwrap_or_else(|e| e.into_inner()) = fresh;
        *state.store_mtime.lock().unwrap_or_else(|e| e.into_inner()) = Some(modified);
    }
}

/// Normalise an origin/host header to a lowercase bare host (no scheme,
/// port, or path). Handles `https://chat.example.org`, `chat.example.org:443`,
/// and bracketed IPv6 `[::1]:8789`.
pub(crate) fn origin_host(value: &str) -> String {
    let without_scheme = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
        .unwrap_or(value);
    let host_with_port = without_scheme.split('/').next().unwrap_or(without_scheme);
    if let Some(rest) = host_with_port.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest).to_lowercase();
    }
    host_with_port
        .split(':')
        .next()
        .unwrap_or(host_with_port)
        .to_lowercase()
}

pub(crate) fn is_loopback_host(host: &str) -> bool {
    host == "localhost" || host == "127.0.0.1" || host == "::1"
}

/// The origin host plus its explicit port, if any (origin `https://x:8443`
/// is a *different* origin than `https://x` — same host, different port).
/// The port matters for same-host-different-port CSRF: a page served from
/// `localhost:9999` must not be able to POST to the guest server on
/// `localhost:8789`.
pub(crate) fn origin_parts(value: &str) -> (String, Option<u16>) {
    let without_scheme = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
        .unwrap_or(value);
    let host_with_port = without_scheme.split('/').next().unwrap_or(without_scheme);
    if let Some(rest) = host_with_port.strip_prefix('[') {
        // bracketed IPv6 `[::1]:8789`
        let (host, port) = rest.split_once(']').unwrap_or((rest, ""));
        let port = port.strip_prefix(':').and_then(|p| p.parse::<u16>().ok());
        return (host.to_lowercase(), port);
    }
    match host_with_port.rsplit_once(':') {
        Some((host, port)) if port.parse::<u16>().is_ok() => {
            (host.to_lowercase(), port.parse::<u16>().ok())
        }
        _ => (host_with_port.to_lowercase(), None),
    }
}

/// Reject cross-origin requests (Cline Kanban lesson, spec §3b). Allowed:
/// - the empty/absent Origin (curl, same-origin GETs),
/// - the origin matching the request's Host header on **both host and port**
///   (a genuine same-origin browser POST — browsers always send `Origin` on
///   POSTs, including same-origin ones; loopback is only allowed when the
///   port matches too, so a page served from another local port can't CSRF),
/// - the configured public subdomain (chat.example.com).
fn check_origin(headers: &HeaderMap, state: &GuestState) -> Result<(), (StatusCode, &'static str)> {
    let Some(origin) = headers.get(ORIGIN) else {
        return Ok(());
    };
    let Ok(origin) = origin.to_str() else {
        return Err((StatusCode::FORBIDDEN, "bad origin header"));
    };
    let (origin, origin_port) = origin_parts(origin);
    if is_loopback_host(&origin) {
        // Loopback origins must still match the Host on host AND port.
        if let Some(host) = headers
            .get(axum::http::header::HOST)
            .and_then(|value| value.to_str().ok())
        {
            let (host, port) = origin_parts(host);
            if host == origin && port == origin_port {
                return Ok(());
            }
        }
        return Err((StatusCode::FORBIDDEN, "cross-origin request refused"));
    }
    if let Some(host) = headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
    {
        let (host, port) = origin_parts(host);
        if host == origin && port == origin_port {
            return Ok(());
        }
    }
    let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
    let public_host = store.public_subdomain.as_deref().map(origin_host);
    if public_host.is_some_and(|public| public == origin) {
        return Ok(());
    }
    Err((StatusCode::FORBIDDEN, "cross-origin request refused"))
}

/// The TCP peer address, when the server runs with `connect-info` (the
/// production path via `run`). Tests that call the router directly have no
/// peer, so this is an `Option`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PeerAddr(pub(crate) Option<SocketAddr>);

impl<S> axum::extract::FromRequestParts<S> for PeerAddr
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(addr)| *addr);
        Ok(PeerAddr(peer))
    }
}

/// Resolve the client IP. `X-Forwarded-For` is ONLY trusted when the request
/// arrives from a loopback peer (caddy on the same host). A direct client
/// (LAN mode, `--allow-non-loopback`) is identified by its TCP peer address —
/// never by a header it can set itself, or the lockout ladder is trivially
/// bypassed by rotating `X-Forwarded-For` on every wrong password.
pub(crate) fn client_ip(headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    match peer {
        Some(addr) if !addr.ip().is_loopback() => addr.ip().to_string(),
        // Loopback peer (trusted proxy) or no peer info (tests): use the
        // proxy-set header if present, else fall back to a shared bucket.
        _ => headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next().map(str::trim))
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| "loopback".to_string()),
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut sigterm) = signal(SignalKind::terminate()) {
            sigterm.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
}

fn device_cookie(token: &str, secure: bool) -> HeaderValue {
    let secure_part = if secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{GUEST_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={DEVICE_TTL_SECS}{secure_part}"
    ))
    .expect("cookie header is valid")
}

/// Resolve the device token from the cookie and return the link id it belongs
/// to (or None when missing/unknown/expired).
fn authenticated(state: &GuestState, headers: &HeaderMap) -> Option<(String, String)> {
    maybe_reload_store(state);
    let token = headers
        .get(axum::http::header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookie| {
            cookie.split(';').find_map(|part| {
                let part = part.trim();
                part.strip_prefix(&format!("{GUEST_COOKIE}="))
            })
        })?;
    let mut store = state.store.lock().unwrap_or_else(|e| e.into_inner());
    let now = now_secs();
    let matched = store
        .links
        .iter()
        .find(|link| link_active(link, now) && store.device_valid(&link.id, token))
        .map(|link| link.id.clone());
    let link_id = matched?;
    // Rolling expiry: refresh last_seen in the same lock scope.
    store.touch_device(&link_id, token);
    Some((link_id, token.to_string()))
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

async fn index(State(state): State<GuestState>, headers: HeaderMap) -> Result<Response, Response> {
    check_origin(&headers, &state).map_err(IntoResponse::into_response)?;
    if authenticated(&state, &headers).is_some() {
        return Ok(Redirect::to("/chat").into_response());
    }
    Ok(Html(LOGIN_PAGE).into_response())
}

async fn chat_page(
    State(state): State<GuestState>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    check_origin(&headers, &state).map_err(IntoResponse::into_response)?;
    if authenticated(&state, &headers).is_none() {
        return Ok(Redirect::to("/").into_response());
    }
    Ok(Html(CHAT_PAGE).into_response())
}

#[derive(Deserialize)]
struct AuthForm {
    password: String,
}

/// Accept the shared password as either JSON (`{"password": "..."}`) or
/// classic form-encoded (`password=...`) so the login page, curl, and older
/// clients all work. Content type decides; JSON wins when ambiguous.
impl<S> axum::extract::FromRequest<S> for AuthForm
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        let is_json = req
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("application/json"));
        if is_json {
            Json::<AuthForm>::from_request(req, state)
                .await
                .map(|Json(form)| form)
                .map_err(IntoResponse::into_response)
        } else {
            Form::<AuthForm>::from_request(req, state)
                .await
                .map(|Form(form)| form)
                .map_err(IntoResponse::into_response)
        }
    }
}

async fn auth(
    State(state): State<GuestState>,
    headers: HeaderMap,
    peer: PeerAddr,
    form: AuthForm,
) -> Result<Response, Response> {
    check_origin(&headers, &state).map_err(IntoResponse::into_response)?;
    maybe_reload_store(&state);
    let ip = client_ip(&headers, peer.0);
    let now = now_secs();

    // Permanent block (third strike) is checked before anything else.
    let permanently_blocked = {
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        store.is_permanently_blocked(&ip)
    };
    if permanently_blocked {
        return Err((
            StatusCode::FORBIDDEN,
            "this address is permanently blocked from guest chat",
        )
            .into_response());
    }

    let locked_until = {
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        store.locked_until(&ip, now)
    };
    if let Some(until) = locked_until {
        let remaining = until.saturating_sub(now);
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            format!("too many attempts — try again in {remaining}s"),
        )
            .into_response());
    }

    // The guest tier is a single shared link set; authenticate against any
    // active link that matches the password. Unknown passwords are recorded
    // as failed attempts without revealing which links exist. Failed attempts
    // and device tokens are persisted so lockouts survive restarts.
    let token = {
        let mut store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        let found = store
            .links
            .iter()
            .find(|link| link_active(link, now) && store.verify_password(link, &form.password))
            .map(|link| link.id.clone());
        match found {
            Some(link_id) => {
                store.reset_failed_attempts(&ip);
                let token = store.mint_device_token(&link_id, ip.as_str());
                persist_store(&store, &state);
                token
            }
            None => {
                let result = store.record_failed_attempt(&ip);
                persist_store(&store, &state);
                if result == crate::guest::LockoutResult::Permanent {
                    return Err((
                        StatusCode::FORBIDDEN,
                        "too many attempts — this address is now permanently blocked from guest chat",
                    )
                        .into_response());
                }
                None
            }
        }
    };

    let Some(token) = token else {
        // No active link at all vs wrong password: same message, no leak.
        return Err((
            StatusCode::UNAUTHORIZED,
            "wrong password (or no guest links are active)",
        )
            .into_response());
    };
    let mut response = Redirect::to("/chat").into_response();
    let secure = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|proto| proto.split(',').next().unwrap_or("").trim() == "https");
    response
        .headers_mut()
        .insert(SET_COOKIE, device_cookie(&token, secure));
    Ok(response)
}

/// Best-effort persist of the guest store (lockouts + devices). A failed save
/// logs and continues — the in-memory state still enforces the policy for the
/// running server. The mtime is refreshed so the next reload doesn't re-read
/// what we just wrote.
fn persist_store(store: &GuestStore, state: &GuestState) {
    if let Err(error) = crate::guest::save(store) {
        tracing::warn!("failed to persist guest store: {error:#}");
        return;
    }
    if let Ok(metadata) = std::fs::metadata(crate::guest::links_path()) {
        if let Ok(modified) = metadata.modified() {
            *state.store_mtime.lock().unwrap_or_else(|e| e.into_inner()) = Some(modified);
        }
    }
}

#[derive(Deserialize)]
struct ChatRequest {
    message: String,
}

#[derive(Serialize)]
struct ChatReply {
    reply: String,
}

/// True when the per-link fixed-window rate limit has headroom; records the
/// call when it does. `window_start` is per-bucket so a long-lived server
/// doesn't need clock bookkeeping beyond the bucket itself.
async fn rate_limited(
    buckets: &tokio::sync::Mutex<HashMap<String, RateWindow>>,
    link_id: &str,
    max_per_minute: u32,
    now: u64,
) -> bool {
    let mut buckets = buckets.lock().await;
    let bucket = buckets.entry(link_id.to_string()).or_default();
    if now.saturating_sub(bucket.window_start) >= 60 {
        bucket.window_start = now;
        bucket.count = 0;
    }
    if bucket.count >= max_per_minute {
        return true;
    }
    bucket.count += 1;
    false
}

/// Drop sessions idle longer than `SESSION_TTL_SECS` (rolling last-used).
/// Called under the sessions lock on each chat/summary access.
fn sweep_sessions(sessions: &mut HashMap<String, LiveSession>, now: u64) {
    sessions.retain(|_, session| now.saturating_sub(session.last_used) <= SESSION_TTL_SECS);
}

async fn api_chat(
    State(state): State<GuestState>,
    headers: HeaderMap,
    Json(form): Json<ChatRequest>,
) -> Result<Response, Response> {
    check_origin(&headers, &state).map_err(IntoResponse::into_response)?;
    let (link_id, token) = authenticated(&state, &headers)
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, "not logged in").into_response())?;

    // Per-link rate limit (spec §9): bounds sustained throughput, not just
    // simultaneity, so a script with the shared password can't burn the
    // host's free-tier quota or SearXNG in a tight loop.
    let now = now_secs();
    if rate_limited(&state.chat_rate, &link_id, CHAT_MAX_PER_MINUTE, now).await {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            "too many messages — try again in a moment",
        )
            .into_response());
    }

    // Concurrency cap per link (spec §9).
    let at_cap = {
        let mut inflight = state.inflight.lock().await;
        let count = inflight.entry(link_id.clone()).or_insert(0);
        let link = {
            let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
            store
                .link(&link_id)
                .map(|link| link.max_concurrent)
                .unwrap_or(DEFAULT_MAX_CONCURRENT)
        };
        if *count >= link {
            true
        } else {
            *count += 1;
            false
        }
    };
    if at_cap {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            "all chat slots are busy right now — try again in a moment",
        )
            .into_response());
    }

    let reply = {
        let mut sessions = state.sessions.lock().await;
        sweep_sessions(&mut sessions, now);
        let session = sessions
            .entry(token.clone())
            .or_insert_with(|| LiveSession {
                last_used: now,
                ..Default::default()
            });
        session.last_used = now;
        state.engine.respond(&mut session.chat, &form.message).await
    };

    {
        let mut inflight = state.inflight.lock().await;
        if let Some(count) = inflight.get_mut(&link_id) {
            *count = count.saturating_sub(1);
        }
    }

    match reply {
        Ok(reply) => Ok(Json(ChatReply { reply }).into_response()),
        Err(error) => Err((StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response()),
    }
}

#[derive(Serialize)]
struct SummaryReply {
    summary: String,
}

async fn api_summary(
    State(state): State<GuestState>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    check_origin(&headers, &state).map_err(IntoResponse::into_response)?;
    let (link_id, token) = authenticated(&state, &headers)
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, "not logged in").into_response())?;

    // Each summary is a model call; cap how many a link can generate per
    // minute so the button can't be hammered into a cost loop.
    let now = now_secs();
    if rate_limited(&state.summary_rate, &link_id, SUMMARY_MAX_PER_MINUTE, now).await {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            "summary requests are rate-limited — try again in a moment",
        )
            .into_response());
    }

    let summary = {
        let mut sessions = state.sessions.lock().await;
        sweep_sessions(&mut sessions, now);
        let session = sessions
            .entry(token.clone())
            .or_insert_with(|| LiveSession {
                last_used: now,
                ..Default::default()
            });
        session.last_used = now;
        if let Some(existing) = session.summary.clone() {
            Ok(existing)
        } else {
            let generated = state.engine.summarize(&session.chat).await;
            if let Ok(text) = &generated {
                session.summary = Some(text.clone());
            }
            generated
        }
    };
    match summary {
        Ok(summary) => Ok(Json(SummaryReply { summary }).into_response()),
        Err(error) => Err((StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response()),
    }
}

const LOGIN_PAGE: &str = r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Katban Guest</title>
<style>
  body{font-family:system-ui,sans-serif;background:#0f1115;color:#e6e6e6;display:flex;align-items:center;justify-content:center;min-height:100vh;margin:0}
  .card{background:#1a1e26;border:1px solid #2c3340;border-radius:12px;padding:32px;width:320px}
  h1{font-size:20px;margin:0 0 4px} p{color:#9aa4b2;margin:0 0 20px;font-size:14px}
  input{width:100%;box-sizing:border-box;padding:10px 12px;border-radius:8px;border:1px solid #2c3340;background:#0f1115;color:#e6e6e6;margin-bottom:12px}
  button{width:100%;padding:10px;border:0;border-radius:8px;background:#4c6ef5;color:#fff;font-weight:600;cursor:pointer}
  button:hover{background:#3b5bdb}
</style></head>
<body><div class="card">
<h1>Katban Guest</h1><p>Enter the password your host shared with you.</p>
<form id="f"><input type="password" id="pw" placeholder="password" autofocus autocomplete="current-password">
<button type="submit">Join</button></form>
<p id="err" style="color:#fa5252;display:none"></p>
</div>
<script>
const f=document.getElementById('f');
f.addEventListener('submit',async e=>{e.preventDefault();
  const err=document.getElementById('err');err.style.display='none';
  const res=await fetch('/auth',{method:'POST',headers:{'Content-Type':'application/json'},
    body:JSON.stringify({password:document.getElementById('pw').value})});
  if(res.ok){location.href='/chat'}else{err.textContent=await res.text();err.style.display='block'}});
</script></body></html>"#;

const CHAT_PAGE: &str = r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Katban Guest Chat</title>
<style>
  body{font-family:system-ui,sans-serif;background:#0f1115;color:#e6e6e6;margin:0;height:100vh;display:flex;flex-direction:column}
  header{padding:12px 20px;border-bottom:1px solid #2c3340;display:flex;justify-content:space-between;align-items:center}
  header h1{font-size:16px;margin:0} header button{background:none;border:1px solid #2c3340;color:#9aa4b2;border-radius:6px;padding:6px 10px;cursor:pointer}
  #log{flex:1;overflow-y:auto;padding:20px;display:flex;flex-direction:column;gap:12px}
  .msg{max-width:75%;padding:10px 14px;border-radius:10px;white-space:pre-wrap;line-height:1.45}
  .user{align-self:flex-end;background:#4c6ef5} .bot{align-self:flex-start;background:#1a1e26;border:1px solid #2c3340}
  .note{align-self:center;color:#9aa4b2;font-size:12px}
  form{display:flex;gap:8px;padding:14px 20px;border-top:1px solid #2c3340}
  input{flex:1;padding:10px 12px;border-radius:8px;border:1px solid #2c3340;background:#0f1115;color:#e6e6e6}
  button{padding:10px 18px;border:0;border-radius:8px;background:#4c6ef5;color:#fff;font-weight:600;cursor:pointer}
  #summary{display:none;white-space:pre-wrap;padding:12px;margin:12px 20px;background:#1a1e26;border:1px solid #2c3340;border-radius:8px;font-size:13px}
</style></head>
<body>
<header><h1>Katban Guest</h1><button id="sumBtn">Download session summary</button></header>
<div id="log"><div class="note">Chat with Clawde — web search available, nothing else.</div></div>
<div id="summary"></div>
<form id="f"><input id="msg" placeholder="Ask anything…" autocomplete="off"><button id="send" type="submit">Send</button></form>
<script>
const log=document.getElementById('log'),form=document.getElementById('f'),msg=document.getElementById('msg'),send=document.getElementById('send');
function add(text,cls){const d=document.createElement('div');d.className='msg '+cls;d.textContent=text;log.appendChild(d);log.scrollTop=log.scrollHeight}
form.addEventListener('submit',async e=>{e.preventDefault();
  const text=msg.value.trim();if(!text)return;msg.value='';add(text,'user');
  send.disabled=true;const n=document.createElement('div');n.className='note';n.textContent='thinking…';log.appendChild(n);
  try{
    const res=await fetch('/api/chat',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({message:text})});
    n.remove();
    if(res.ok){const data=await res.json();add(data.reply,'bot')}
    else{add('Sorry — '+await res.text(),'bot')}
  }catch(err){n.remove();add('Network error — try again.','bot')}
  send.disabled=false;msg.focus()});
document.getElementById('sumBtn').addEventListener('click',async()=>{
  const res=await fetch('/api/summary',{method:'POST'});
  if(!res.ok){alert('Summary unavailable right now.');return}
  const data=await res.json();const s=document.getElementById('summary');
  s.textContent=data.summary;s.style.display='block';
  const blob=new Blob([data.summary],{type:'text/markdown'});
  const a=document.createElement('a');a.href=URL.createObjectURL(blob);a.download='katban-session-summary.md';a.click()});
</script></body></html>"#;

#[cfg(test)]
mod tests {
    // The env-pin guard is deliberately held across the request awaits so
    // CLAWDE_HOME cannot change mid-test (the server re-reads links.json from
    // disk on auth-touching requests). The std lock is only ever held inside
    // the test thread — this is a test-only pattern, not production code.
    #![allow(clippy::await_holding_lock)]

    use super::*;
    use crate::chat::{ChatEngine, GuestBackend};
    use crate::guest::save;
    use crate::search::GuestSearch;
    use axum::body::Body;
    use axum::http::header::CONTENT_TYPE;
    use axum::http::Request;
    use clawde_api::provider_types::{ProviderRequest, ProviderResponse, StopReason};
    use clawde_core::types::{ContentBlock, UsageInfo};
    use tempfile::tempdir;
    use tower::ServiceExt;

    fn stub_engine() -> Arc<ChatEngine> {
        struct StubBackend;
        #[async_trait::async_trait]
        impl GuestBackend for StubBackend {
            async fn chat(&self, _request: ProviderRequest) -> Result<ProviderResponse, String> {
                Ok(ProviderResponse {
                    id: "stub".to_string(),
                    content: vec![ContentBlock::Text {
                        text: "hi from stub".to_string(),
                    }],
                    stop_reason: StopReason::EndTurn,
                    usage: UsageInfo::default(),
                    model: "free/auto".to_string(),
                    rate_limit: None,
                })
            }
        }
        struct StubSearch;
        #[async_trait::async_trait]
        impl GuestSearch for StubSearch {
            async fn search(
                &self,
                _query: &str,
            ) -> Result<Vec<crate::search::SearchResult>, String> {
                Ok(Vec::new())
            }
        }
        Arc::new(ChatEngine::new(Arc::new(StubBackend), Arc::new(StubSearch)))
    }

    /// Builds a router + returns the env guard so CLAWDE_HOME stays pinned to
    /// the temp dir for the whole test (the server now re-reads links.json
    /// from disk on auth-touching requests, so the env must not change).
    fn setup() -> (
        Router,
        String,
        String,
        Arc<Mutex<GuestStore>>,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempdir().unwrap();
        std::env::set_var("CLAWDE_HOME", tmp.path());
        let mut store = GuestStore::default();
        let id = store.create_link("friends", "correct-pw", None, 2);
        save(&store).unwrap();
        let store = Arc::new(Mutex::new(store));
        let server = GuestServer::new(stub_engine(), store.clone());
        let router = server.router();
        (router, id, "correct-pw".to_string(), store, guard)
    }

    /// A bare `GuestState` over an empty store, for pure function checks
    /// that only need origin/ip logic (no router round-trip).
    fn bare_state() -> GuestState {
        let store = Arc::new(Mutex::new(GuestStore::default()));
        GuestState {
            store,
            engine: stub_engine(),
            sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            inflight: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            chat_rate: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            summary_rate: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            store_mtime: Arc::new(Mutex::new(None)),
        }
    }

    async fn body_text(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        String::from_utf8_lossy(&bytes).to_string()
    }

    #[tokio::test]
    async fn login_page_serves_without_cookie() {
        let (router, _, _, _, _guard) = setup();
        let response = router
            .clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_text(response).await.contains("Katban Guest"));
    }

    #[tokio::test]
    async fn cross_origin_requests_are_refused() {
        let (router, _, _, _, _guard) = setup();
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(ORIGIN, "https://evil.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn same_origin_browser_post_is_allowed() {
        // Browsers send `Origin` on every POST, including same-origin ones;
        // the origin matches the Host the page was served from.
        let (router, _, correct, _, _guard) = setup();
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/auth")
                    .header(axum::http::header::HOST, "chat.example.com")
                    .header(ORIGIN, "https://chat.example.com")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({ "password": correct }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
    }

    #[tokio::test]
    async fn configured_public_subdomain_is_allowed_as_origin() {
        let (router, _, correct, store, _guard) = setup();
        store.lock().unwrap().public_subdomain = Some("chat.example.com".to_string());
        // No matching Host header; the configured subdomain alone is enough.
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/auth")
                    .header(ORIGIN, "https://chat.example.com")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({ "password": correct }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
    }

    #[tokio::test]
    async fn wrong_password_is_rejected_and_locks_out() {
        let (router, _, correct, _, _guard) = setup();
        let mut statuses = Vec::new();
        for _ in 0..crate::guest::MAX_FAILED_ATTEMPTS {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/auth")
                        .header(CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            serde_json::json!({ "password": "bad" }).to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            statuses.push(response.status());
        }
        // The first four wrong attempts are plain rejections; the fifth arms
        // the lockout, so the *next* request is refused with 429.
        assert!(statuses.iter().all(|s| *s == StatusCode::UNAUTHORIZED));
        // Even the correct password is refused while locked out.
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/auth")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({ "password": correct }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn correct_password_mints_device_cookie_and_chat_works() {
        let (router, _, correct, _, _guard) = setup();
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/auth")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({ "password": correct }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let cookie = response
            .headers()
            .get(SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
        let token = cookie
            .split(';')
            .next()
            .unwrap()
            .strip_prefix("katban_guest=")
            .unwrap()
            .to_string();

        // Chat page now loads with the cookie.
        let chat = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/chat")
                    .header("cookie", format!("katban_guest={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(chat.status(), StatusCode::OK);

        // Chat round-trip.
        let reply = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/chat")
                    .header("cookie", format!("katban_guest={token}"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({ "message": "hello" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(reply.status(), StatusCode::OK);
        let parsed: serde_json::Value = serde_json::from_str(&body_text(reply).await).unwrap();
        assert_eq!(parsed["reply"], "hi from stub");
    }

    #[tokio::test]
    async fn form_encoded_password_login_works() {
        // curl -d 'password=...' sends application/x-www-form-urlencoded;
        // the auth endpoint must accept it alongside JSON.
        let (router, _, correct, _, _guard) = setup();
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/auth")
                    .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!("password={correct}")))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(response.headers().contains_key(SET_COOKIE));
    }

    #[tokio::test]
    async fn unauthenticated_chat_is_rejected() {
        let (router, _, _, _, _guard) = setup();
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/chat")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({ "message": "hello" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn revoked_link_cannot_chat() {
        let (router, id, correct, store, _guard) = setup();
        // Revoke through the same in-memory store the server uses.
        assert!(store.lock().unwrap().revoke_link(&id));

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/auth")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({ "password": correct }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn permanently_blocked_ip_is_refused() {
        let (router, _, correct, store, _guard) = setup();
        // Simulate the third strike having already been served.
        store.lock().unwrap().failed_attempts.insert(
            "loopback".to_string(),
            crate::guest::FailedAttempt {
                count: 0,
                locked_until: None,
                strikes: 3,
                permanently_blocked: true,
            },
        );

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/auth")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({ "password": correct }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn client_ip_ignores_spoofed_xff_from_direct_peers() {
        // A direct (non-loopback) peer IS the client — a spoofed
        // X-Forwarded-For must not move it, or the lockout ladder is dead.
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "1.2.3.4".parse().unwrap());
        let peer: Option<SocketAddr> = "203.0.113.7:45000".parse().ok();
        assert_eq!(client_ip(&headers, peer), "203.0.113.7");

        // Loopback peer (caddy proxy) or no peer info: XFF is trusted.
        let peer: Option<SocketAddr> = "127.0.0.1:45000".parse().ok();
        assert_eq!(client_ip(&headers, peer), "1.2.3.4");
        assert_eq!(client_ip(&headers, None), "1.2.3.4");
    }

    #[test]
    fn client_ip_falls_back_to_loopback_without_proxy_header() {
        let headers = HeaderMap::new();
        assert_eq!(client_ip(&headers, None), "loopback");
    }

    #[test]
    fn origin_check_rejects_same_host_different_port() {
        // A page served from localhost:9999 must not be able to POST to the
        // guest server on localhost:8789 (same host, different port = a
        // different origin).
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::HOST, "localhost:8789".parse().unwrap());
        headers.insert(ORIGIN, "http://localhost:9999".parse().unwrap());
        let state = bare_state();
        assert!(check_origin(&headers, &state).is_err());
    }

    #[tokio::test]
    async fn origin_check_allows_matching_port_on_loopback() {
        let (router, _, correct, _, _guard) = setup();
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/auth")
                    .header(axum::http::header::HOST, "127.0.0.1:8789")
                    .header(ORIGIN, "http://127.0.0.1:8789")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({ "password": correct }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
    }

    #[tokio::test]
    async fn chat_rate_limit_rejects_after_window_budget() {
        let (router, _, correct, _, _guard) = setup();
        // Log in once and reuse the cookie for every message.
        let login = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/auth")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({ "password": correct }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let cookie = login
            .headers()
            .get(SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let mut last = StatusCode::OK;
        for _ in 0..(CHAT_MAX_PER_MINUTE + 1) {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/chat")
                        .header("cookie", &cookie)
                        .header(CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            serde_json::json!({ "message": "hi" }).to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            last = response.status();
        }
        assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn sweep_sessions_drops_idle_entries() {
        let now = now_secs();
        let mut sessions = HashMap::new();
        sessions.insert(
            "fresh".to_string(),
            LiveSession {
                last_used: now,
                ..Default::default()
            },
        );
        sessions.insert(
            "stale".to_string(),
            LiveSession {
                last_used: now - SESSION_TTL_SECS - 1,
                ..Default::default()
            },
        );
        sweep_sessions(&mut sessions, now);
        assert!(sessions.contains_key("fresh"));
        assert!(!sessions.contains_key("stale"));
    }

    #[test]
    fn device_cookie_adds_secure_only_behind_https() {
        assert!(!device_cookie("tok", false)
            .to_str()
            .unwrap()
            .contains("Secure"));
        assert!(device_cookie("tok", true)
            .to_str()
            .unwrap()
            .contains("Secure"));
    }

    #[tokio::test]
    async fn secure_cookie_flag_from_x_forwarded_proto() {
        let (router, _, correct, _, _guard) = setup();
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/auth")
                    .header("x-forwarded-proto", "https")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({ "password": correct }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response
            .headers()
            .get(SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("Secure"));
    }
}
