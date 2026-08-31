//! Admin board web surface (spec §20 — phase 2: thin, loopback, write-capable).
//!
//! A separate axum app on its own port (default 8790) serving a minimal
//! hand-rolled board UI over a small API. Phase 1 was read-only; this adds
//! the write surface (add cards, set status, advance, archive) gated behind
//! admin session auth, mirroring the guest server's hardening:
//! - **Login**: `POST /api/login` accepts the admin password (set with
//!   `clawde katban board password`) as JSON or form-encoded, verifies
//!   against the salted hash in `board_admin::AdminStore`, mints a session
//!   token and sets a HttpOnly+SameSite=Strict cookie.
//! - **Wrong-password lockout**: per-IP, same ladder as the guest server
//!   (5 -> 3 -> 3 -> permanent), via `apply_failed_attempt`.
//! - **Origin check**: write routes reject cross-origin requests (Cline
//!   Kanban lesson, spec §3b), reusing `guest_server`'s helpers.
//! - **Board lock**: every write holds `board::BoardLock` around load -> change
//!   -> save, so a browser edit can never race the CLI / `/katban` / TUI.
//!
//! Reads stay open (a loopback admin board); writes require a session.
//! Reads are lock-free because board saves are atomic (tmp + rename), so a
//! reader always observes a consistent file.

use crate::board::{self, Board, CardStatus, Dependency, DiffSummary};
use crate::board_admin::{AdminStore, ADMIN_COOKIE};
use crate::guest_server::{client_ip, is_loopback_host, origin_parts, PeerAddr};
use anyhow::Context;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

pub const DEFAULT_BOARD_PORT: u16 = 8790;

/// Admin board server. Owns the admin credential store and re-reads it from
/// disk on auth-touching requests (so `clawde katban board password` takes
/// effect without a restart), the same pattern as the guest server's
/// `maybe_reload_store`.
pub struct BoardServer {
    store: Arc<Mutex<AdminStore>>,
    store_mtime: Arc<Mutex<Option<std::time::SystemTime>>>,
    /// Change broadcast: `()` every time any board file changes (web writes
    /// AND CLI//katban//TUI edits, via the same polling watcher `host.rs` uses
    /// for live reload). The frontend's EventSource re-fetches on each event.
    board_tx: tokio::sync::broadcast::Sender<()>,
}

impl BoardServer {
    pub fn new() -> Self {
        let store_mtime = std::fs::metadata(crate::board_admin::admin_path())
            .and_then(|metadata| metadata.modified())
            .ok();
        let (board_tx, _) = tokio::sync::broadcast::channel::<()>(16);
        // Watch the boards directory so live updates fire for edits made by
        // any surface, not just this server's own write routes.
        crate::reload::spawn_watcher(board::board_dir(), board_tx.clone());
        // A corrupt admin.json must never be silently replaced by an empty
        // store: the empty store has no password, so `is_configured()` gates
        // every save and the corrupt file is preserved for repair. Surface the
        // real cause loudly so an admin diagnosing "no password set" sees it.
        let store = match crate::board_admin::load() {
            Ok(store) => store,
            Err(error) => {
                tracing::error!(
                    error = %error,
                    path = %crate::board_admin::admin_path().display(),
                    "admin store is corrupt — login will report no password set until it is fixed or removed"
                );
                crate::board_admin::AdminStore::default()
            }
        };
        BoardServer {
            store: Arc::new(Mutex::new(store)),
            store_mtime: Arc::new(Mutex::new(store_mtime)),
            board_tx,
        }
    }

    pub async fn run(&self, addr: SocketAddr) -> anyhow::Result<()> {
        let app = self
            .router()
            .into_make_service_with_connect_info::<SocketAddr>();
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("bind {addr}"))?;
        tracing::info!(%addr, "katban board serving");
        axum::serve(listener, app)
            .with_graceful_shutdown(crate::host::shutdown_signal())
            .await?;
        Ok(())
    }

    fn router(&self) -> Router {
        let state = BoardState {
            store: self.store.clone(),
            store_mtime: self.store_mtime.clone(),
            board_tx: self.board_tx.clone(),
        };
        Router::new()
            .route("/", get(admin_page))
            .route("/api/projects", get(api_projects))
            .route("/api/runner", get(api_runner))
            .route("/api/board/{project}", get(api_board))
            .route("/api/board/events", get(api_board_events))
            .route("/api/me", get(api_me))
            .route("/api/login", post(api_login))
            .route("/api/boards", post(api_new_board))
            .route("/api/board/{project}/cards", post(api_add_card))
            .route(
                "/api/board/{project}/cards/{id}/status",
                post(api_set_status),
            )
            .route("/api/board/{project}/cards/{id}/advance", post(api_advance))
            .route("/api/board/{project}/cards/{id}/merge", post(api_merge))
            .route("/api/board/{project}/cards/{id}/archive", post(api_archive))
            .route("/api/board/{project}/cards/{id}/comment", post(api_comment))
            .route(
                "/api/board/{project}/cards/{id}/feedback",
                post(api_send_feedback),
            )
            .route("/api/board/{project}/link", post(api_link))
            .route("/api/board/{project}/unlink", post(api_unlink))
            .route("/api/board/{project}/verify", post(api_toggle_verify))
            .route(
                "/api/board/{project}/auto-review",
                post(api_toggle_auto_review),
            )
            .with_state(state)
    }
}

impl Default for BoardServer {
    fn default() -> Self {
        BoardServer::new()
    }
}

/// Reload the admin store from disk when `admin.json` changed (a password
/// rotation by the CLI applies to the running server without a restart).
fn maybe_reload_store(state: &BoardState) {
    let Ok(metadata) = std::fs::metadata(crate::board_admin::admin_path()) else {
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
    if let Ok(fresh) = crate::board_admin::load() {
        *state.store.lock().unwrap_or_else(|e| e.into_inner()) = fresh;
        *state.store_mtime.lock().unwrap_or_else(|e| e.into_inner()) = Some(modified);
    }
}

#[derive(Clone)]
struct BoardState {
    store: Arc<Mutex<AdminStore>>,
    store_mtime: Arc<Mutex<Option<std::time::SystemTime>>>,
    board_tx: tokio::sync::broadcast::Sender<()>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BoardApi {
    project: String,
    cards: Vec<CardApi>,
    dependencies: Vec<Dependency>,
    parallel_cap: usize,
    auto_retry: u32,
    auto_review: bool,
    verify: bool,
    /// ids that can start right now (deps met, not running/review/blocked/done).
    ready: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CardApi {
    id: String,
    prompt: String,
    status: CardStatus,
    ready: bool,
    blocked_reason: Option<String>,
    branch: Option<String>,
    work_dir: Option<String>,
    retries: u32,
    result: Option<String>,
    diff: Option<String>,
    diff_summary: Option<DiffSummary>,
    failure_kind: Option<crate::board::FailureKind>,
    commit: Option<String>,
    reviews: Vec<ReviewCommentApi>,
    followup_feedback: Option<String>,
    created_at: u64,
    updated_at: u64,
}

/// Serialize a review comment for the web board (id, optional diff-line
/// anchor, text).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewCommentApi {
    id: String,
    location: Option<String>,
    text: String,
    created_at: u64,
}

/// Serve the admin board app on `addr` until Ctrl-C / SIGTERM. The write API
/// is gated behind admin session auth; reads stay open on the loopback board.
pub async fn run_on(addr: SocketAddr) -> anyhow::Result<()> {
    BoardServer::new().run(addr).await
}

async fn admin_page() -> Response {
    let response = (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        ADMIN_HTML,
    )
        .into_response();
    no_cache_nosniff(response)
}

/// True when the board is reachable beyond the loopback dev path: either the
/// peer is non-loopback (`--allow-non-loopback`) or the request came through a
/// reverse proxy (caddy sets `X-Forwarded-*`). Reads stay open by design only
/// on direct loopback requests (spec §20.7); once exposed publicly, every
/// route requires the admin session — the admin tier always requires the
/// admin credential (spec §4).
fn exposed_request(peer: Option<SocketAddr>, headers: &HeaderMap) -> bool {
    peer.is_some_and(|p| !p.ip().is_loopback())
        || headers.contains_key("x-forwarded-for")
        || headers.contains_key("x-forwarded-proto")
}

/// Gate a read route: on the loopback dev board reads stay open; once the
/// board is reachable beyond loopback (public subdomain through caddy, or a
/// non-loopback bind), reads require the admin session like every write.
async fn api_projects(
    State(state): State<BoardState>,
    headers: HeaderMap,
    peer: PeerAddr,
) -> Response {
    if exposed_request(peer.0, &headers) && authenticated(&state, &headers).is_none() {
        return json_message(StatusCode::UNAUTHORIZED, "login required");
    }
    no_cache_nosniff((Json(board::existing_projects()),).into_response())
}

/// Always-on runner status: which projects the runner schedules right now and
/// which it is waiting to join (all-mode boards that are registered but not
/// yet being scheduled). Derived from the persisted `AdminStore` via
/// `maybe_reload_store`, so a `board expose --run` change shows up without a
/// board server restart.
async fn api_runner(
    State(state): State<BoardState>,
    headers: HeaderMap,
    peer: PeerAddr,
) -> Response {
    if exposed_request(peer.0, &headers) && authenticated(&state, &headers).is_none() {
        return json_message(StatusCode::UNAUTHORIZED, "login required");
    }
    maybe_reload_store(&state);
    let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
    no_cache_nosniff((Json(crate::board_admin::runner_state(&store)),).into_response())
}

async fn api_board(
    State(state): State<BoardState>,
    Path(project): Path<String>,
    headers: HeaderMap,
    peer: PeerAddr,
) -> Response {
    if exposed_request(peer.0, &headers) && authenticated(&state, &headers).is_none() {
        return json_message(StatusCode::UNAUTHORIZED, "login required");
    }
    match board::load_board(&project) {
        Ok(Some(loaded)) => {
            let api = board_to_api(&project, &loaded);
            no_cache_nosniff((Json(api),).into_response())
        }
        Ok(None) => no_cache_nosniff(
            (
                StatusCode::NOT_FOUND,
                Json::<serde_json::Value>(serde_json::json!({ "error": "no board for project" })),
            )
                .into_response(),
        ),
        Err(error) => no_cache_nosniff(
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json::<serde_json::Value>(serde_json::json!({ "error": error.to_string() })),
            )
                .into_response(),
        ),
    }
}

/// SSE stream of board-change events. The frontend's EventSource keeps a
/// connection open and re-fetches the board on every event. Events fire when
/// the board file changes — from this server's write routes OR the CLI /
/// `/katban` / TUI (the polling watcher in `BoardServer::new` broadcasts on
/// any boards-dir change). Same shape as `host.rs`'s live-reload SSE.
async fn api_board_events(
    State(state): State<BoardState>,
    headers: HeaderMap,
    peer: PeerAddr,
) -> Response {
    // Same read gate as the board data: on loopback the stream is open; once
    // the board is exposed, board-activity timing is admin-only too.
    if exposed_request(peer.0, &headers) && authenticated(&state, &headers).is_none() {
        return json_message(StatusCode::UNAUTHORIZED, "login required");
    }
    let rx = state.board_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|item| match item {
        Ok(()) => Some(Ok::<_, Infallible>(Event::default().data("changed"))),
        Err(_) => None, // lagged behind -> skip
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

// ---------------------------------------------------------------------------
// Admin auth (mirrors guest_server)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct LoginForm {
    password: String,
}

/// Accept the admin password as either JSON (`{"password": "..."}`) or
/// classic form-encoded (`password=...`) so the login page, curl, and older
/// clients all work. Content type decides; JSON wins when ambiguous.
impl<S> axum::extract::FromRequest<S> for LoginForm
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        let is_json = req
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("application/json"));
        if is_json {
            Json::<LoginForm>::from_request(req, state)
                .await
                .map(|Json(form)| form)
                .map_err(IntoResponse::into_response)
        } else {
            Form::<LoginForm>::from_request(req, state)
                .await
                .map(|Form(form)| form)
                .map_err(IntoResponse::into_response)
        }
    }
}

fn admin_cookie(token: &str, secure: bool) -> HeaderValue {
    let secure_part = if secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{ADMIN_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{secure_part}",
        crate::board_admin::ADMIN_SESSION_TTL_SECS
    ))
    .expect("cookie header is valid")
}

/// Resolve the admin session from the cookie. Returns the plaintext token on
/// success; refreshes last-seen in the same lock scope.
fn authenticated(state: &BoardState, headers: &HeaderMap) -> Option<String> {
    maybe_reload_store(state);
    let token = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookie| {
            cookie.split(';').find_map(|part| {
                let part = part.trim();
                part.strip_prefix(&format!("{ADMIN_COOKIE}="))
            })
        })?
        .to_string();
    let mut store = state.store.lock().unwrap_or_else(|e| e.into_inner());
    if !store.session_valid(&token) {
        return None;
    }
    store.touch_session(&token);
    Some(token)
}

/// Reject cross-origin write requests (same rule as the guest server, sans
/// the public-subdomain clause — the board is loopback or a configured admin
/// subdomain). A missing Origin is fine (curl/same-origin GETs); when present
/// it must match the Host on both host and port.
fn check_origin(headers: &HeaderMap) -> Result<(), (StatusCode, &'static str)> {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return Ok(());
    };
    let Ok(origin) = origin.to_str() else {
        return Err((StatusCode::FORBIDDEN, "bad origin header"));
    };
    let (origin, origin_port) = origin_parts(origin);
    let Some(host_value) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return Err((StatusCode::FORBIDDEN, "missing Host header"));
    };
    let (host, port) = origin_parts(host_value);
    if host == origin && port == origin_port {
        return Ok(());
    }
    if is_loopback_host(&origin) && is_loopback_host(&host) && origin_port == port {
        return Ok(());
    }
    Err((StatusCode::FORBIDDEN, "cross-origin request refused"))
}

fn json_message(status: StatusCode, message: &str) -> Response {
    no_cache_nosniff(
        (
            status,
            Json::<serde_json::Value>(serde_json::json!({ "error": message })),
        )
            .into_response(),
    )
}

/// Whether the current request carries an admin session. The board's login
/// cookie is HttpOnly, so the frontend can't read it directly — this lets the
/// UI restore the signed-in state across refreshes.
async fn api_me(State(state): State<BoardState>, headers: HeaderMap) -> Response {
    if authenticated(&state, &headers).is_some() {
        no_cache_nosniff(
            Json::<serde_json::Value>(serde_json::json!({ "authed": true })).into_response(),
        )
    } else {
        no_cache_nosniff(
            (
                StatusCode::UNAUTHORIZED,
                Json::<serde_json::Value>(serde_json::json!({ "authed": false })),
            )
                .into_response(),
        )
    }
}

async fn api_login(
    State(state): State<BoardState>,
    headers: HeaderMap,
    peer: PeerAddr,
    form: LoginForm,
) -> Response {
    // Login is a state-changing POST — same cross-origin rule as every write
    // route (and the guest server's `auth`). A page served from another origin
    // must not be able to drive this.
    if let Err((status, message)) = check_origin(&headers) {
        return json_message(status, message);
    }
    maybe_reload_store(&state);
    let ip = client_ip(&headers, peer.0);
    let now = crate::guest::now_secs();

    if state
        .store
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_permanently_blocked(&ip)
    {
        return json_message(StatusCode::FORBIDDEN, "this address is permanently blocked");
    }
    if let Some(until) = state
        .store
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .locked_until(&ip, now)
    {
        let remaining = until.saturating_sub(now);
        return json_message(
            StatusCode::TOO_MANY_REQUESTS,
            &format!("too many attempts — try again in {remaining}s"),
        );
    }

    let token = {
        let mut store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        if !store.is_configured() {
            return json_message(
                StatusCode::SERVICE_UNAVAILABLE,
                "no admin password set — run `clawde katban board password`",
            );
        }
        if store.verify_password(&form.password) {
            store.reset_failed_attempts(&ip);
            let token = store.mint_session();
            let _ = crate::board_admin::save(&store);
            token
        } else {
            let result = store.record_failed_attempt(&ip);
            let _ = crate::board_admin::save(&store);
            match result {
                crate::guest::LockoutResult::Permanent => {
                    return json_message(
                        StatusCode::FORBIDDEN,
                        "too many attempts — permanently blocked",
                    );
                }
                crate::guest::LockoutResult::Temporary(_) => {
                    return json_message(
                        StatusCode::TOO_MANY_REQUESTS,
                        "too many attempts — locked for a few minutes",
                    );
                }
                crate::guest::LockoutResult::None => {
                    return json_message(StatusCode::UNAUTHORIZED, "wrong password");
                }
            }
        }
    };

    // Behind caddy (https admin subdomain), caddy sets x-forwarded-proto;
    // the session cookie must then carry Secure so it is never sent over
    // plaintext. On the raw loopback board the header is absent -> http.
    let secure = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|proto| proto.split(',').next().unwrap_or("").trim() == "https");
    let mut response = no_cache_nosniff(
        Json::<serde_json::Value>(serde_json::json!({ "ok": true })).into_response(),
    );
    response
        .headers_mut()
        .insert(header::SET_COOKIE, admin_cookie(&token, secure));
    response
}

/// Require a valid admin session on a write route.
fn require_auth(state: &BoardState, headers: &HeaderMap) -> Result<String, Box<Response>> {
    check_origin(headers).map_err(|e| Box::new(e.into_response()))?;
    authenticated(state, headers)
        .ok_or_else(|| Box::new(json_message(StatusCode::UNAUTHORIZED, "login required")))
}

// ---------------------------------------------------------------------------
// Board writes (auth-gated, lock-protected)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct NewCardForm {
    prompt: String,
}

#[derive(Deserialize)]
struct StatusForm {
    status: String,
}

#[derive(Deserialize)]
struct NewBoardForm {
    name: String,
}

#[derive(Deserialize)]
struct LinkForm {
    from: String,
    to: String,
}

/// Load the board for writing, returning an error response on failure.
fn load_board_for_write(project: &str) -> Result<Board, Box<Response>> {
    match board::load_board(project) {
        Ok(Some(board)) => Ok(board),
        Ok(None) => Err(Box::new(json_message(
            StatusCode::NOT_FOUND,
            "no board for project",
        ))),
        Err(error) => Err(Box::new(json_message(
            StatusCode::INTERNAL_SERVER_ERROR,
            &error.to_string(),
        ))),
    }
}

fn write_board(board: &Board, project: &str) -> Response {
    match board::save_board(board, project) {
        Ok(()) => no_cache_nosniff((Json(board_to_api(project, board)),).into_response()),
        Err(error) => json_message(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

async fn api_add_card(
    State(state): State<BoardState>,
    Path(project): Path<String>,
    headers: HeaderMap,
    Json(body): Json<NewCardForm>,
) -> Response {
    if let Err(response) = require_auth(&state, &headers) {
        return *response;
    }
    let prompt = body.prompt.trim().to_string();
    if prompt.is_empty() {
        return json_message(StatusCode::BAD_REQUEST, "prompt must not be empty");
    }
    let _guard = match board::BoardLock::acquire(&project) {
        Ok(guard) => guard,
        Err(error) => return json_message(StatusCode::CONFLICT, &error.to_string()),
    };
    let mut board = match load_board_for_write(&project) {
        Ok(board) => board,
        Err(response) => return *response,
    };
    board.add_card(&prompt);
    write_board(&board, &project)
}

async fn api_set_status(
    State(state): State<BoardState>,
    Path((project, id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<StatusForm>,
) -> Response {
    if let Err(response) = require_auth(&state, &headers) {
        return *response;
    }
    let Some(status) = CardStatus::parse(&body.status) else {
        return json_message(StatusCode::BAD_REQUEST, "unknown status");
    };
    // Setting a card done without merging is an explicit discard: clean up any
    // pinned branch so it isn't leaked forever. `discard_card` locks + deletes
    // the branch + marks done, so we must not hold `BoardLock` here.
    if status == CardStatus::Done {
        return match crate::commit::discard_card(&project, &id) {
            Ok(()) => fresh_board_response(&project),
            Err(message) => json_message(StatusCode::CONFLICT, &message),
        };
    }
    let _guard = match board::BoardLock::acquire(&project) {
        Ok(guard) => guard,
        Err(error) => return json_message(StatusCode::CONFLICT, &error.to_string()),
    };
    let mut board = match load_board_for_write(&project) {
        Ok(board) => board,
        Err(response) => return *response,
    };
    if !board.set_status(&id, status) {
        return json_message(StatusCode::NOT_FOUND, "no such card");
    }
    write_board(&board, &project)
}

async fn api_advance(
    State(state): State<BoardState>,
    Path((project, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_auth(&state, &headers) {
        return *response;
    }
    let _guard = match board::BoardLock::acquire(&project) {
        Ok(guard) => guard,
        Err(error) => return json_message(StatusCode::CONFLICT, &error.to_string()),
    };
    let mut board = match load_board_for_write(&project) {
        Ok(board) => board,
        Err(response) => return *response,
    };
    let Some(card) = board.card(&id) else {
        return json_message(StatusCode::NOT_FOUND, "no such card");
    };
    let Some(next) = card.status.next() else {
        return json_message(StatusCode::BAD_REQUEST, "card is done — nothing to advance");
    };
    // Advancing a *review* card to Done without merging is an explicit discard:
    // drop the lock and route through `discard_card` so the pinned `katban/<id>`
    // branch is deleted rather than leaked forever. (Merge and archive are the
    // other two review exits and already clean the branch up themselves.)
    if next == CardStatus::Done {
        drop(_guard);
        return match crate::commit::discard_card(&project, &id) {
            Ok(()) => fresh_board_response(&project),
            Err(message) => json_message(StatusCode::CONFLICT, &message),
        };
    }
    board.set_status(&id, next);
    write_board(&board, &project)
}

/// Option B — merge a review card's pinned branch into the project and close
/// the card (dependents unblock via readiness). `commit::merge_card` holds the
/// board lock, runs the merge, and cleans up the branch; on conflict it aborts
/// the merge and reports so the admin resolves manually.
async fn api_merge(
    State(state): State<BoardState>,
    Path((project, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_auth(&state, &headers) {
        return *response;
    }
    match crate::commit::merge_card(&project, &id) {
        Ok(()) => {
            // Reflect the merged/closed card (and any newly-unblocked deps).
            let _guard = match board::BoardLock::acquire(&project) {
                Ok(guard) => guard,
                Err(error) => return json_message(StatusCode::CONFLICT, &error.to_string()),
            };
            match load_board_for_write(&project) {
                Ok(board) => write_board(&board, &project),
                Err(response) => *response,
            }
        }
        Err(error) => json_message(StatusCode::CONFLICT, &error),
    }
}

async fn api_archive(
    State(state): State<BoardState>,
    Path((project, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_auth(&state, &headers) {
        return *response;
    }
    // Discard: archive the card AND delete its pinned branch (if any) so a
    // discarded card leaves no dangling branch behind.
    match crate::commit::discard_card(&project, &id) {
        Ok(()) => {
            let _guard = match board::BoardLock::acquire(&project) {
                Ok(guard) => guard,
                Err(error) => return json_message(StatusCode::CONFLICT, &error.to_string()),
            };
            match load_board_for_write(&project) {
                Ok(board) => write_board(&board, &project),
                Err(response) => *response,
            }
        }
        Err(error) => json_message(StatusCode::CONFLICT, &error),
    }
}

/// Create a brand-new (empty) board for a project name the admin typed in the
/// UI. Previously only the CLI (`board init`) could create boards, so a
/// browser-only admin had no way to stand up a fresh project board. Auth-gated
/// like every write; the project dir encoding keeps the name safe on disk.
async fn api_new_board(
    State(state): State<BoardState>,
    headers: HeaderMap,
    Json(body): Json<NewBoardForm>,
) -> Response {
    if let Err(response) = require_auth(&state, &headers) {
        return *response;
    }
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return json_message(StatusCode::BAD_REQUEST, "board name must not be empty");
    }
    let _guard = match board::BoardLock::acquire(&name) {
        Ok(guard) => guard,
        Err(error) => return json_message(StatusCode::CONFLICT, &error.to_string()),
    };
    if let Ok(Some(_)) = board::load_board(&name) {
        return json_message(
            StatusCode::CONFLICT,
            &format!("a board for '{name}' already exists"),
        );
    }
    let fresh = board::Board::new();
    match board::save_board(&fresh, &name) {
        Ok(()) => no_cache_nosniff((Json(board_to_api(&name, &fresh))).into_response()),
        Err(error) => json_message(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

/// Link `from -> to` (B must finish before A starts), cycle-checked, from the
/// web UI — previously link/unlink were CLI/TUI-only so the board web board
/// could show dependencies but never create or remove them.
async fn api_link(
    State(state): State<BoardState>,
    Path(project): Path<String>,
    headers: HeaderMap,
    Json(body): Json<LinkForm>,
) -> Response {
    if let Err(response) = require_auth(&state, &headers) {
        return *response;
    }
    let _guard = match board::BoardLock::acquire(&project) {
        Ok(guard) => guard,
        Err(error) => return json_message(StatusCode::CONFLICT, &error.to_string()),
    };
    let mut board = match load_board_for_write(&project) {
        Ok(board) => board,
        Err(response) => return *response,
    };
    match board.add_dependency(&body.from, &body.to) {
        Ok(()) => write_board(&board, &project),
        Err(message) => json_message(StatusCode::BAD_REQUEST, &format!("cannot link: {message}")),
    }
}

async fn api_unlink(
    State(state): State<BoardState>,
    Path(project): Path<String>,
    headers: HeaderMap,
    Json(body): Json<LinkForm>,
) -> Response {
    if let Err(response) = require_auth(&state, &headers) {
        return *response;
    }
    let _guard = match board::BoardLock::acquire(&project) {
        Ok(guard) => guard,
        Err(error) => return json_message(StatusCode::CONFLICT, &error.to_string()),
    };
    let mut board = match load_board_for_write(&project) {
        Ok(board) => board,
        Err(response) => return *response,
    };
    if board.remove_dependency(&body.from, &body.to) {
        write_board(&board, &project)
    } else {
        json_message(StatusCode::NOT_FOUND, "no such dependency")
    }
}

#[derive(Default, Deserialize)]
struct CommentForm {
    /// Optional diff-line anchor, e.g. "12" or "14-16" (1-based diff line).
    #[serde(default)]
    location: Option<String>,
    text: String,
}

/// Append a diff-review comment to a card (spec §16a E5). `add_review` is
/// lock-protected, so this handler doesn't re-acquire `BoardLock`; it reloads
/// the saved board for the response (matching the SSE-broadcast pattern).
async fn api_comment(
    State(state): State<BoardState>,
    Path((project, id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<CommentForm>,
) -> Response {
    if let Err(response) = require_auth(&state, &headers) {
        return *response;
    }
    let location = body
        .location
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty());
    let text = body.text.trim().to_string();
    if text.is_empty() {
        return json_message(StatusCode::BAD_REQUEST, "comment text must not be empty");
    }
    match board::add_review(&project, &id, location, &text) {
        Ok(_) => fresh_board_response(&project),
        Err(message) => json_message(
            StatusCode::BAD_REQUEST,
            &format!("cannot comment: {message}"),
        ),
    }
}

/// Send a review card's comments back to the agent as a follow-up run.
/// `send_feedback_to_agent` requeues the card under `BoardLock`, so the
/// response just reloads the saved board (depending cards then unblock in the
/// runner and show as ready here).
async fn api_send_feedback(
    State(state): State<BoardState>,
    Path((project, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_auth(&state, &headers) {
        return *response;
    }
    match board::send_feedback_to_agent(&project, &id) {
        Ok(count) => {
            no_cache_nosniff((Json(serde_json::json!({ "sent": count })),).into_response())
        }
        Err(message) => json_message(
            StatusCode::CONFLICT,
            &format!("cannot send feedback: {message}"),
        ),
    }
}

/// Request body for the board-level toggles: the desired state (`enabled`).
/// Idempotent — setting a flag to its current value is a no-op save.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToggleForm {
    enabled: bool,
}

/// Per-board verification gate toggle (`POST /api/board/{project}/verify`).
/// Auth-gated like every board write. Mirrors `board verify on|off`.
async fn api_toggle_verify(
    State(state): State<BoardState>,
    Path(project): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ToggleForm>,
) -> Response {
    api_set_board_flag(&state, &project, &headers, |board| {
        board.verify = body.enabled
    })
}

/// Per-board auto-review toggle (`POST /api/board/{project}/auto-review`).
/// Auth-gated like every board write. Mirrors `board auto-review on|off`.
async fn api_toggle_auto_review(
    State(state): State<BoardState>,
    Path(project): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ToggleForm>,
) -> Response {
    api_set_board_flag(&state, &project, &headers, |board| {
        board.auto_review = body.enabled;
    })
}

/// Shared body for the two board-flag toggles: auth-gate, then under
/// `BoardLock` load, apply `set`, save.
fn api_set_board_flag(
    state: &BoardState,
    project: &str,
    headers: &HeaderMap,
    set: impl FnOnce(&mut Board),
) -> Response {
    if let Err(response) = require_auth(state, headers) {
        return *response;
    }
    let _guard = match board::BoardLock::acquire(project) {
        Ok(guard) => guard,
        Err(error) => return json_message(StatusCode::CONFLICT, &error.to_string()),
    };
    let mut board = match load_board_for_write(project) {
        Ok(board) => board,
        Err(response) => return *response,
    };
    set(&mut board);
    write_board(&board, project)
}

/// Reload a saved board for a write-response (the lock was already held and
/// released inside the op that saved it), mirroring `load_board_for_write`
/// without re-acquiring `BoardLock`.
fn fresh_board_response(project: &str) -> Response {
    match board::load_board(project) {
        Ok(Some(board)) => write_board(&board, project),
        Ok(None) => json_message(StatusCode::NOT_FOUND, "no board for project"),
        Err(error) => json_message(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

/// Board state is live and never cacheable, and JSON must never be
/// MIME-sniffed into HTML. Same hardening the §19.6 site-host audit applied to
/// the dev-site server (`host.rs`).
fn no_cache_nosniff(mut response: Response) -> Response {
    if let Ok(value) = axum::http::HeaderValue::from_str("no-store") {
        response
            .headers_mut()
            .insert(axum::http::header::CACHE_CONTROL, value);
    }
    response.headers_mut().insert(
        axum::http::header::X_CONTENT_TYPE_OPTIONS,
        axum::http::HeaderValue::from_static("nosniff"),
    );
    response
}

fn board_to_api(project: &str, board: &Board) -> BoardApi {
    let ready: Vec<String> = board
        .cards
        .iter()
        .filter(|card| board.ready_to_run(&card.id))
        .map(|card| card.id.clone())
        .collect();
    let cards = board
        .cards
        .iter()
        .map(|card| CardApi {
            id: card.id.clone(),
            prompt: card.prompt.clone(),
            status: card.status,
            ready: ready.contains(&card.id),
            blocked_reason: board.blocked_reason(&card.id),
            branch: card.branch.clone(),
            work_dir: card.work_dir.clone(),
            retries: card.retries,
            result: card.result.clone(),
            diff: card.diff.clone(),
            diff_summary: card.diff_summary.clone(),
            failure_kind: card.failure_kind,
            commit: card.commit.clone(),
            reviews: card.reviews.iter().map(review_to_api).collect(),
            followup_feedback: card.followup_feedback.clone(),
            created_at: card.created_at,
            updated_at: card.updated_at,
        })
        .collect();
    BoardApi {
        project: project.to_string(),
        cards,
        dependencies: board.dependencies.clone(),
        parallel_cap: board.parallel_cap,
        auto_retry: board.auto_retry,
        auto_review: board.auto_review,
        verify: board.verify,
        ready,
    }
}

fn review_to_api(r: &crate::board::ReviewComment) -> ReviewCommentApi {
    ReviewCommentApi {
        id: r.id.clone(),
        location: r.location.clone(),
        text: r.text.clone(),
        created_at: r.created_at,
    }
}

/// The inline admin board page: a single self-contained HTML/CSS/JS app. It
/// lists projects, renders one column per status, marks ready cards, and
/// shows each card's blocked reason and dependencies. No build step, no
/// external assets, no runtime Node.
const ADMIN_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Katban</title>
<style>
  :root { color-scheme: dark; }
  * { box-sizing: border-box; }
  body { margin: 0; font: 14px/1.45 system-ui, sans-serif; background: #15181d; color: #e7ebf0; }
  header { display:flex; align-items:center; gap:12px; padding:12px 18px; border-bottom:1px solid #2a2f37; background:#1a1e25; }
  header h1 { font-size:16px; margin:0; }
  select, button { background:#242a33; color:#e7ebf0; border:1px solid #3a424e; border-radius:6px; padding:5px 9px; cursor:pointer; }
  main { padding:16px 18px; }
  .board { display:flex; gap:14px; overflow-x:auto; align-items:flex-start; padding-bottom:16px; }
  .col { min-width:230px; width:230px; background:#1c2128; border-radius:10px; border:1px solid #2a2f37; padding:8px; }
  .col h2 { font-size:12px; text-transform:uppercase; letter-spacing:.06em; color:#8b96a5; margin:2px 6px 8px; }
  .card { background:#242a33; border:1px solid #39414d; border-radius:8px; padding:8px 10px; margin-bottom:8px; }
  .card.running { border-left:3px solid #4c8dff; }
  .card.review { border-left:3px solid #c99bff; }
  .card.failed { border-left:3px solid #ff6b6b; }
  .card.done { opacity:.6; }
  .card .prompt { font-weight:600; margin-bottom:4px; }
  .card .meta { font-size:12px; color:#8b96a5; }
  .card .badge { display:inline-block; font-size:11px; padding:1px 6px; border-radius:99px; margin-right:4px; }
  .card .badge.ready { background:#1f6f43; color:#b8e8c9; }
  .card .blocked { color:#ffb36b; font-size:12px; margin-top:4px; white-space:pre-wrap; }
  .card .deps { font-size:12px; color:#8b96a5; margin-top:4px; }
  .muted { color:#8b96a5; }
  .card.blocked { border-left:3px solid #ffb36b; }
  .auth { margin-left:auto; display:flex; gap:8px; align-items:center; }
  .auth input { background:#242a33; color:#e7ebf0; border:1px solid #3a424e; border-radius:6px; padding:5px 9px; }
  .btn.ok { border-color:#2f8f5b; color:#b8e8c9; }
  .toolbar { display:flex; gap:10px; padding:12px 18px; }
  .toolbar input { flex:1; background:#242a33; color:#e7ebf0; border:1px solid #3a424e; border-radius:6px; padding:7px 10px; }
  .card .actions { margin-top:8px; display:flex; gap:6px; align-items:center; }
  .card .actions button, .card .deps button.mini { font-size:12px; padding:3px 8px; }
  .card .deps button.mini { padding:0 5px; margin-left:2px; }
  .card .actions select { font-size:12px; padding:2px 4px; background:#242a33; color:#e7ebf0; border:1px solid #3a424e; border-radius:6px; }
  .card details.diff summary { font-size:12px; color:#8b96a5; cursor:pointer; margin-top:4px; }
  .card details.diff pre { font-size:11px; background:#15181d; border:1px solid #2a2f37; border-radius:6px; padding:6px; overflow:auto; max-height:220px; white-space:pre-wrap; color:#c8d0da; }
  .card .reviews { margin-top:4px; display:flex; flex-direction:column; gap:2px; }
  .card .review { font-size:12px; color:#ffd479; background:#2a2410aa; border-left:2px solid #ffb36b; padding:2px 6px; border-radius:4px; white-space:pre-wrap; }
  .card .rform { margin-top:6px; display:flex; gap:4px; }
  .card .rform input { font-size:12px; background:#15181d; color:#e7ebf0; border:1px solid #3a424e; border-radius:6px; padding:2px 4px; }
  .card .rform .rline { width:72px; flex:0 0 auto; }
  .card .rform .rtext { flex:1; min-width:0; }
  #runner { font-size:12px; color:#8b96a5; }
  #runner .r-ok { color:#b8e8c9; }
  #runner .r-wait { color:#ffb36b; }
  #runner .r-off { color:#ff6b6b; }
</style>
</head>
<body>
<header>
  <h1>Katban</h1>
  <select id="project" title="Board project"></select>
  <button id="refresh">Refresh</button>
  <span class="muted" id="meta"></span>
  <span id="runner" title="Always-on runner state"></span>
  <span class="auth">
    <input id="password" type="password" placeholder="admin password" autocomplete="current-password" hidden>
    <button id="login">Sign in</button>
    <button id="authed" class="ok" hidden disabled>signed in</button>
  </span>
</header>
<main>
  <div id="empty" class="muted" hidden>No boards yet — add cards with <code>/katban board card add</code> or the Alt+G menu.</div>
  <div class="toolbar" id="toolbar" hidden>
    <input id="newprompt" type="text" placeholder="new card prompt" autocomplete="off">
    <button id="addcard">Add card</button>
    <button id="toggleverify" title="Toggle the verification gate: run the project's detected checks in the card's worktree before it reaches review">verify on</button>
    <button id="toggleautoreview" title="Toggle the auto-review pass: a second agent reviews each card's diff">auto-review on</button>
    <span style="flex:1"></span>
    <button id="newboard" title="Create a board for a new project name">New board</button>
  </div>
  <div class="board" id="board"></div>
</main>
<script>
// 'blocked' is a real CardStatus (manual hold) — it must have a column or
// manually-held cards would be invisible on the board.
const COLS = ["backlog","queued","running","blocked","review","failed","done"];
const $ = (s) => document.querySelector(s);

// Always-on runner indicator: which projects the runner schedules right now
// and which it is waiting to join (`--run all` boards with no registered
// git repo). Rendered into the `#runner` header span.
async function loadRunner() {
  let st;
  try { st = await (await fetch("/api/runner")).json(); }
  catch (e) { return; }
  const el = $("#runner");
  if (!st.configured) {
    el.innerHTML = '<span class="r-off" title="The board isn\'t always-on: run `clawde katban board expose --run <NAME,...|all>` to schedule card execution.">runner: not configured</span>';
    return;
  }
  if (st.mode === "all") {
    let parts = 'runner: <span class="r-ok">all (' + st.scheduled.length + ' scheduled)</span>';
    if (st.waiting.length) {
      parts += ' <span class="r-wait" title="Registered as boards but no git repo yet — they join automatically once `clawde katban project set <NAME> <DIR>` runs.">' + st.waiting.length + ' waiting to join</span>';
    }
    el.innerHTML = parts;
  } else {
    el.innerHTML = 'runner: <span class="r-ok">' + esc(st.scheduled.length ? st.scheduled.join(", ") : "no projects") + '</span>';
  }
}

async function loadProjects() {
  const res = await fetch("/api/projects");
  if (res.status === 401) {
    // Exposed board, not signed in: reads are admin-gated. Show the gate
    // instead of "no boards" (which would be misleading).
    $("#board").hidden = true;
    $("#empty").hidden = false;
    $("#empty").innerHTML = "Sign in to view this board.";
    return;
  }
  const projects = await res.json();
  const sel = $("#project");
  const prev = sel.value;
  sel.innerHTML = "";
  for (const p of projects) {
    const o = document.createElement("option");
    o.value = p; o.textContent = p;
    sel.appendChild(o);
  }
  if (!projects.length) { $("#empty").hidden = false; $("#board").hidden = true; return; }
  $("#empty").hidden = true; $("#board").hidden = false;
  // Keep the admin's current selection across Refresh; fall back to the first.
  sel.value = projects.includes(prev) ? prev : projects[0];
  loadBoard(sel.value);
}

async function loadBoard(project) {
  const res = await fetch("/api/board/" + encodeURIComponent(project));
  if (!res.ok) { $("#board").innerHTML = '<span class="blocked">' + (await res.text()) + "</span>"; return; }
  const api = await res.json();
  $("#meta").textContent = api.cards.length + " cards · cap " + api.parallelCap + " · retry " + api.autoRetry + (api.autoReview ? " · auto-review" : "") + " · verify " + (api.verify ? "on" : "off");
  // Keep the board-level toggle buttons truthful (and clickable when signed in).
  gateVerify = api.verify; gateAutoReview = api.autoReview;
  $("#toggleverify").textContent = "verify " + (gateVerify ? "on" : "off");
  $("#toggleautoreview").textContent = "auto-review " + (gateAutoReview ? "on" : "off");
  const byStatus = {};
  COLS.forEach((c) => byStatus[c] = []);
  const waitMap = {};
  api.dependencies.forEach((d) => (waitMap[d.from] ||= []).push(d.to));
  const promptOf = {};
  api.cards.forEach((c) => promptOf[c.id] = c.prompt);

  api.cards.forEach((c) => {
    if (!byStatus[c.status]) byStatus[c.status] = [];
    byStatus[c.status].push(c);
  });

  $("#board").innerHTML = "";
  for (const col of COLS) {
    const colEl = document.createElement("section");
    colEl.className = "col";
    const h = document.createElement("h2");
    h.textContent = col + " (" + (byStatus[col].length) + ")";
    colEl.appendChild(h);
    for (const c of byStatus[col]) {
      const card = document.createElement("div");
      card.className = "card " + c.status;
      let html = '<div class="prompt">' + esc(c.prompt) + "</div>";
      if (c.ready) html += '<span class="badge ready">ready</span>';
      html += '<span class="meta">' + c.id.slice(0,8) + (c.retries ? " · retries " + c.retries : "") +
              (c.commit ? " · " + c.commit.slice(0,8) : "") + "</span>";
      if (c.blockedReason) html += '<div class="blocked">' + esc(c.blockedReason) + "</div>";
      if (c.result) html += '<div class="blocked" title="last result">' + esc(c.result) + "</div>";
      if (c.status === "failed") html += '<div class="meta">failure: ' + esc(c.failureKind || "unknown") + '</div>';
      if (c.diff && !c.commit) html += '<div class="meta">diff only — no mergeable commit</div>';
      const deps = (waitMap[c.id] || []);
      if (deps.length) {
        html += '<div class="deps">waits for: ' +
          deps.map((id) => esc(promptOf[id] || id) +
            (authed && c.status !== "done"
              ? ' <button class="mini" data-unlink="' + c.id + '" data-unlink-target="' + id + '" title="remove dependency">x</button>'
              : "")).join(", ") +
          "</div>";
      }
      if (c.diff_summary) html += '<div class="meta">' + c.diff_summary.filesChanged + ' file(s) · +' + c.diff_summary.additions + ' · -' + c.diff_summary.deletions + '</div>';
      if (c.diff) html += '<details class="diff"><summary>diff (' + (c.diff.length) + ' ch)</summary><pre>' + esc(c.diff) + '</pre></details>';
      if (c.reviews && c.reviews.length) {
        html += '<div class="reviews">' + c.reviews.map((r) =>
          '<div class="review" title="' + (r.createdAt ? "" : "") + '">' +
          esc((r.location ? "[L" + r.location + "] " : "") + r.text) + "</div>").join("") + "</div>";
      }
      if (authed && c.status === "review") {
        html += '<div class="rform">' +
                '<input class="rline" data-rline="' + c.id + '" placeholder="line" title="Optional diff line this comment anchors to (e.g. 12 or 14-16)">' +
                '<input class="rtext" data-rtext="' + c.id + '" placeholder="review comment — send to agent to request a follow-up">' +
                '<button class="mini" data-comment="' + c.id + '">comment</button>' +
                '<button class="mini" data-fb="' + c.id + '" title="Re-run this card's agent with its review comments as feedback">send to agent</button>' +
                "</div>";
      }
      if (authed && c.status !== "done") {
        html += '<div class="actions">' +
                '<button data-link="' + c.id + '" title="Link this card to wait on another (enter its id)">link</button>' +
                '<select data-status="' + c.id + '" title="Set status">' +
                COLS.filter((s) => s !== "done").map((s) =>
                  '<option value="' + s + '"' + (s === c.status ? " selected" : "") + ">" + s + "</option>"
                ).join("") +
                '</select>' +
                (c.status === "review"
                  ? '<button data-mrg="' + c.id + '" style="border-color:#2fbf71;color:#2fbf71" title="Merge the pinned commit into the project & mark done">merge</button>'
                  : '') +
                // Review cards have no generic "advance": moving one to Done is a
                // merge-or-discard decision, and merge/archive above are the only
                // (branch-safe) exits. Advance is the silent-discard footgun.
                (c.status !== "review"
                  ? '<button data-adv="' + c.id + '">advance</button>'
                  : '') +
                '<button data-arc="' + c.id + '">archive</button></div>';
      }
      card.innerHTML = html;
      colEl.appendChild(card);
    }
    if (!byStatus[col].length) colEl.appendChild(Object.assign(document.createElement("div"), {className:"muted", textContent:"—"}));
    $("#board").appendChild(colEl);
  }
}

function esc(s) {
  const d = document.createElement("div");
  d.textContent = s;
  return d.innerHTML;
}

let authed = false;
// Board-level toggle state (from the last loaded board), so the toolbar
// buttons can flip them and stay truthful. `authed` gates the whole toolbar
// (hidden when signed out), so these are only clickable to an admin.
let gateVerify = true, gateAutoReview = true;

// ---- admin session / write controls ----

async function tryLogin() {
  const pw = $("#password").value;
  const res = await fetch("/api/login", {
    method: "POST",
    credentials: "same-origin",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ password: pw })
  });
  if (res.ok) { setAuth(true); $("#password").value = ""; }
  else { alert("login failed"); }
}

function setAuth(ok) {
  authed = ok;
  $("#password").hidden = ok;
  $("#login").hidden = ok;
  $("#authed").hidden = !ok;
  $("#toolbar").hidden = !ok;
  if (ok) { loadProjects(); wireEvents(); }
  else stopEvents();
}

// Flip a board-level toggle (verify | auto-review) via its auth-gated POST
// route, then reload the board so the toolbar labels + meta line update.
async function toggleBoardFlag(route, enabled, label) {
  const res = await fetch("/api/board/" + encodeURIComponent($("#project").value) + "/" + route, {
    method: "POST", credentials: "same-origin",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ enabled })
  });
  if (res.status === 401) { setAuth(false); alert("session expired — sign in again"); return; }
  if (res.ok) loadBoard($("#project").value); else alert("could not toggle " + label);
}

async function addCard() {
  const prompt = $("#newprompt").value.trim();
  if (!prompt) return;
  const res = await fetch("/api/board/" + encodeURIComponent($("#project").value) + "/cards", {
    method: "POST", credentials: "same-origin",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ prompt })
  });
  if (res.status === 401) { setAuth(false); alert("session expired — sign in again"); return; }
  $("#newprompt").value = "";
  if (res.ok) loadBoard($("#project").value); else alert("failed to add card");
}

async function advanceCard(id) {
  const res = await fetch("/api/board/" + encodeURIComponent($("#project").value) + "/cards/" + id + "/advance", {
    method: "POST", credentials: "same-origin"
  });
  if (res.status === 401) { setAuth(false); alert("session expired — sign in again"); return; }
  if (res.ok) loadBoard($("#project").value); else alert("could not advance card");
}

async function archiveCard(id) {
  const res = await fetch("/api/board/" + encodeURIComponent($("#project").value) + "/cards/" + id + "/archive", {
    method: "POST", credentials: "same-origin"
  });
  if (res.status === 401) { setAuth(false); alert("session expired — sign in again"); return; }
  if (res.ok) loadBoard($("#project").value); else alert("could not archive card");
}

async function mergeCard(id) {
  const res = await fetch("/api/board/" + encodeURIComponent($("#project").value) + "/cards/" + id + "/merge", {
    method: "POST", credentials: "same-origin"
  });
  if (res.status === 401) { setAuth(false); alert("session expired — sign in again"); return; }
  const data = await res.json().catch(() => null);
  if (res.ok) loadBoard($("#project").value);
  else alert((data && data.error) || "merge failed — see server log");
}

async function setStatus(id, status) {
  const res = await fetch("/api/board/" + encodeURIComponent($("#project").value) + "/cards/" + id + "/status", {
    method: "POST", credentials: "same-origin",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ status })
  });
  if (res.status === 401) { setAuth(false); alert("session expired — sign in again"); return; }
  if (res.ok) loadBoard($("#project").value); else alert("could not set status");
}

async function linkCard(id, to) {
  const res = await fetch("/api/board/" + encodeURIComponent($("#project").value) + "/link", {
    method: "POST", credentials: "same-origin",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ from: id, to })
  });
  if (res.status === 401) { setAuth(false); alert("session expired — sign in again"); return; }
  if (res.ok) loadBoard($("#project").value); else alert((await res.json()).error || "could not link cards");
}

async function unlinkCard(from, to) {
  const res = await fetch("/api/board/" + encodeURIComponent($("#project").value) + "/unlink", {
    method: "POST", credentials: "same-origin",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ from, to })
  });
  if (res.status === 401) { setAuth(false); alert("session expired — sign in again"); return; }
  if (res.ok) loadBoard($("#project").value); else alert("could not unlink cards");
}

async function postComment(id) {
  const loc = $('[data-rline="' + id + '"]').value.trim();
  const text = $('[data-rtext="' + id + '"]').value.trim();
  if (!text) { alert("comment text is empty"); return; }
  const res = await fetch("/api/board/" + encodeURIComponent($("#project").value) + "/cards/" + id + "/comment", {
    method: "POST", credentials: "same-origin",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ location: loc || null, text })
  });
  if (res.status === 401) { setAuth(false); alert("session expired — sign in again"); return; }
  if (res.ok) loadBoard($("#project").value);
  else alert((await res.json()).error || "could not comment");
}

async function sendFeedback(id) {
  const res = await fetch("/api/board/" + encodeURIComponent($("#project").value) + "/cards/" + id + "/feedback", {
    method: "POST", credentials: "same-origin"
  });
  if (res.status === 401) { setAuth(false); alert("session expired — sign in again"); return; }
  if (res.ok) loadBoard($("#project").value);
  else alert((await res.json()).error || "could not send feedback");
}

async function newBoard() {
  const name = prompt("New board project name:");
  if (!name || !name.trim()) return;
  const res = await fetch("/api/boards", {
    method: "POST", credentials: "same-origin",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name: name.trim() })
  });
  if (res.status === 401) { setAuth(false); alert("session expired — sign in again"); return; }
  if (res.ok) loadProjects(); else alert((await res.json()).error || "could not create board");
}

$("#login").addEventListener("click", tryLogin);
$("#password").addEventListener("keydown", (e) => { if (e.key === "Enter") tryLogin(); });
$("#addcard").addEventListener("click", addCard);
$("#newprompt").addEventListener("keydown", (e) => { if (e.key === "Enter") addCard(); });
$("#toggleverify").addEventListener("click", () => toggleBoardFlag("verify", !gateVerify, "the verify gate"));
$("#toggleautoreview").addEventListener("click", () => toggleBoardFlag("auto-review", !gateAutoReview, "auto-review"));

$("#board").addEventListener("click", (e) => {
  const adv = e.target.closest("[data-adv]");
  const arc = e.target.closest("[data-arc]");
  const mrg = e.target.closest("[data-mrg]");
  const link = e.target.closest("[data-link]");
  const unlink = e.target.closest("[data-unlink]");
  const cmt = e.target.closest("[data-comment]");
  const fb = e.target.closest("[data-fb]");
  if (fb && window.confirm("Re-run this card's agent with its review comments as feedback?")) sendFeedback(fb.dataset.fb);
  if (cmt) postComment(cmt.dataset.comment);
  if (mrg && window.confirm("Merge this card's commit into the project history?")) mergeCard(mrg.dataset.mrg);
  if (adv && window.confirm("Advance this card?")) advanceCard(adv.dataset.adv);
  if (arc && window.confirm("Archive this card (discards its pinned commit/branch)?")) archiveCard(arc.dataset.arc);
  if (link) {
    const to = prompt("Enter the id of the card this one should wait for:");
    if (to && to.trim()) linkCard(link.dataset.link, to.trim());
  }
  if (unlink && window.confirm("Remove this dependency?")) unlinkCard(unlink.dataset.unlink, unlink.dataset.unlinkTarget);
});

$("#newboard").addEventListener("click", newBoard);

$("#board").addEventListener("change", (e) => {
  const sel = e.target.closest("[data-status]");
  if (sel) setStatus(sel.dataset.status, sel.value);
});

$("#project").addEventListener("change", (e) => loadBoard(e.target.value));
$("#refresh").addEventListener("click", () => { loadProjects(); loadRunner(); });

// Live updates: any board change (this server's writes or CLI//katban//TUI
// edits) pushes an SSE event. Re-fetch the project list + current board so a
// board created/removed elsewhere (CLI//katban), or a card change, appears in
// the running UI without a manual Refresh. loadProjects preserves the current
// selection and re-loads its board, so this covers both cases. Created only
// after a successful sign-in: on an exposed board the stream is admin-gated,
// so an unauthenticated EventSource would just error-loop.
let events = null;
function wireEvents() {
  if (events) events.close();
  events = new EventSource("/api/board/events");
  events.onmessage = () => { loadProjects(); loadRunner(); };
}
function stopEvents() {
  if (events) { events.close(); events = null; }
}

// Restore signed-in state from the HttpOnly cookie, so a refresh doesn't
// force a re-login when the session is still valid.
(async () => {
  const me = await fetch("/api/me", { credentials: "same-origin" });
  setAuth(me.ok);
  loadProjects();
  loadRunner();
})();
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    // The env-pin guard is deliberately held across the request awaits so
    // CLAWDE_HOME cannot change mid-test (the server re-reads the board file
    // from disk on each request). The std lock is only ever held inside the
    // test thread — test-only, never production code. Mirrors guest_server.
    #![allow(clippy::await_holding_lock)]

    use super::*;
    use axum::body::to_bytes;
    use axum::http::{Request, StatusCode as HttpStatus};
    use tower::ServiceExt;

    /// Lock `ENV_LOCK` (serializing env mutation across the parallel test
    /// runner), point `CLAWDE_HOME` at a temp dir, optionally seed a board
    /// and set an admin password, then build the router. The env guard rides
    /// in the tuple so it stays held for the test's lifetime, silencing
    /// `await_holding_lock` above.
    fn router_with_home(
        tmp: &std::path::Path,
        seed: Option<&str>,
        admin_password: Option<&str>,
    ) -> (Router, std::sync::MutexGuard<'static, ()>) {
        let guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("CLAWDE_HOME", tmp);
        if let Some(project) = seed {
            let mut board = board::Board::new();
            let base = board.add_card("set up the db");
            let ui = board.add_card("build the ui");
            board.add_dependency(&ui, &base).unwrap();
            board::save_board(&board, project).unwrap();
        }
        if let Some(password) = admin_password {
            let mut admin = AdminStore::default();
            admin.set_password(password);
            crate::board_admin::save(&admin).unwrap();
        }
        (BoardServer::new().router(), guard)
    }

    #[tokio::test]
    async fn serves_admin_page() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _guard) = router_with_home(tmp.path(), None, None);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), HttpStatus::OK);
        // The page must never be cached (board state is live) and must carry
        // nosniff — the same hardening site-host (host.rs) got in the §19.6 audit.
        let headers = response.headers();
        assert_eq!(
            headers
                .get(axum::http::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
        assert_eq!(
            headers
                .get(axum::http::header::X_CONTENT_TYPE_OPTIONS)
                .and_then(|v| v.to_str().ok()),
            Some("nosniff")
        );
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("Katban"));
        assert!(html.contains("/api/board/"));
        // Every real CardStatus must have a column so no card is invisible.
        // The JS codes the column list as ["blocked",...] inside the raw r## string.
        assert!(html.contains("\"blocked\",\"review\""));
    }

    #[tokio::test]
    async fn board_api_carries_no_cache_and_nosniff() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _guard) = router_with_home(tmp.path(), Some("default"), None);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/board/default")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), HttpStatus::OK);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::X_CONTENT_TYPE_OPTIONS)
                .and_then(|v| v.to_str().ok()),
            Some("nosniff")
        );
    }

    #[tokio::test]
    async fn runner_api_reports_configured_and_all_mode() {
        let tmp = tempfile::tempdir().unwrap();
        // Seed an empty admin store BEFORE the server is built so admin.json
        // exists and the server caches its mtime (the live-reload path below
        // compares against it). Pin CLAWDE_HOME, save, then release the lock
        // before router_with_home re-acquires it (std mutex isn't reentrant).
        {
            let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            std::env::set_var("CLAWDE_HOME", tmp.path());
            crate::board_admin::save(&crate::board_admin::AdminStore::default()).unwrap();
        }
        let (app, _guard) = router_with_home(tmp.path(), None, None);

        // Not configured -> configured:false.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/runner")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::OK);
        assert_eq!(
            res.headers()
                .get(axum::http::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
        let body = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["configured"], false);

        // Configure --run all and reach the API again: maybe_reload_store
        // picks up the admin.json change, so the already-running BoardState
        // reports the new runner config without a restart.
        let mut store = crate::board_admin::load().unwrap();
        store.runner_projects = vec![crate::board_admin::RUN_ALL.to_string()];
        crate::board_admin::save(&store).unwrap();
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/runner")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::OK);
        let body = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["configured"], true);
        assert_eq!(json["mode"], "all");
    }

    #[tokio::test]
    async fn board_api_returns_cards_deps_and_readiness() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _guard) = router_with_home(tmp.path(), Some("default"), None);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/board/default")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let api: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(api["project"], "default");
        assert_eq!(api["cards"].as_array().unwrap().len(), 2);
        assert_eq!(api["dependencies"].as_array().unwrap().len(), 1);
        // The db card is ready; the ui card is blocked on it.
        let ready: Vec<&str> = api["ready"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(ready.len(), 1);
        for c in api["cards"].as_array().unwrap() {
            if c["prompt"] == "build the ui" {
                assert!(c["blockedReason"]
                    .as_str()
                    .unwrap()
                    .contains("set up the db"));
            } else {
                assert_eq!(c["ready"], true);
            }
        }
    }

    #[tokio::test]
    async fn board_api_404_on_missing_project() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _guard) = router_with_home(tmp.path(), None, None);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/board/nope")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), HttpStatus::NOT_FOUND);
    }

    #[tokio::test]
    async fn projects_api_lists_existing_boards() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _guard) = router_with_home(tmp.path(), Some("my-repo"), None);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/projects")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let projects: Vec<String> = serde_json::from_slice(&body).unwrap();
        assert_eq!(projects, vec!["my-repo"]);
    }

    #[tokio::test]
    async fn comment_and_feedback_routes_round_trip_under_auth() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _guard) = router_with_home(tmp.path(), Some("default"), Some("hunter2"));
        let cookie = login_cookie(&app, "hunter2").await.unwrap();
        // login_cookie already returns the full `katban_admin=<token>` value.
        let auth = cookie;

        // Move the first card to review (the runner does this on success) so
        // feedback is allowed.
        let id = {
            let _guard = board::BoardLock::acquire("default").unwrap();
            let mut b = board::load_board("default").unwrap().unwrap();
            let card_id = b.cards[0].id.clone();
            b.set_status(&card_id, CardStatus::Review);
            board::save_board(&b, "default").unwrap();
            card_id
        };

        // Unauthenticated writes are refused on the comment route too.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/board/default/cards/{id}/comment"))
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        "{\"text\":\"x\",\"location\":\"12\"}".to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::UNAUTHORIZED);

        // Add a review comment with a diff-line anchor; the response board
        // carries it.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/board/default/cards/{id}/comment"))
                    .header("cookie", &auth)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        "{\"text\":\"add an index on user_id\",\"location\":\"12\"}",
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::OK);
        let body = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        let api: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let card = api["cards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"].as_str() == Some(id.as_str()))
            .unwrap();
        let reviews = card["reviews"].as_array().unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0]["location"], "12");
        assert_eq!(reviews[0]["text"], "add an index on user_id");

        // Send it back to the agent: the card requeues with the feedback
        // composed.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/board/default/cards/{id}/feedback"))
                    .header("cookie", &auth)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::OK);
        let body = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["sent"], 1);

        // The board shows the card requeued with pending feedback.
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/board/default")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        let api: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let card = api["cards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"].as_str() == Some(id.as_str()))
            .unwrap();
        assert_eq!(card["status"], "queued");
        assert!(card["followupFeedback"]
            .as_str()
            .unwrap()
            .contains("add an index on user_id"));
    }

    #[tokio::test]
    async fn board_toggle_routes_round_trip_under_auth() {
        // The web board's verify/auto-review toggle buttons hit auth-gated POST
        // routes that mirror `board verify|auto-review on|off`. The board
        // round-trips through board.json and the API reflects the new state.
        let tmp = tempfile::tempdir().unwrap();
        let (app, _guard) = router_with_home(tmp.path(), Some("default"), Some("hunter2"));
        let cookie = login_cookie(&app, "hunter2").await.unwrap();
        async fn post_flag(app: &Router, uri: &str, cookie: &str, body: &str) -> Response {
            app.clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(uri)
                        .header("cookie", cookie)
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap()
        }

        // Unauthenticated toggle is refused.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/board/default/verify")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from("{\"enabled\":false}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::UNAUTHORIZED);

        // Toggle both gates off, then re-read the board from the API.
        assert_eq!(
            post_flag(
                &app,
                "/api/board/default/verify",
                &cookie,
                "{\"enabled\":false}"
            )
            .await
            .status(),
            HttpStatus::OK
        );
        assert_eq!(
            post_flag(
                &app,
                "/api/board/default/auto-review",
                &cookie,
                "{\"enabled\":false}"
            )
            .await
            .status(),
            HttpStatus::OK
        );

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/board/default")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        let api: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(api["verify"], false);
        assert_eq!(api["autoReview"], false);

        // Toggle verify back on; auto-review stays off.
        assert_eq!(
            post_flag(
                &app,
                "/api/board/default/verify",
                &cookie,
                "{\"enabled\":true}"
            )
            .await
            .status(),
            HttpStatus::OK
        );
        let loaded = board::load_board("default").unwrap().unwrap();
        assert!(loaded.verify);
        assert!(
            !loaded.auto_review,
            "auto-review toggle must be independent"
        );
    }

    // ---- write API + admin auth ----

    async fn login_cookie(app: &Router, password: &str) -> Option<String> {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/login")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(format!(
                        "{{\"password\":\"{password}\"}}"
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::OK);
        res.headers()
            .get(axum::http::header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(';').next().unwrap().to_string())
    }

    #[tokio::test]
    async fn login_without_password_configured_is_503() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _guard) = router_with_home(tmp.path(), Some("default"), None);
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/login")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from("{\"password\":\"x\"}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn login_sets_cookie_for_valid_password() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _guard) = router_with_home(tmp.path(), Some("default"), Some("hunter2"));
        let cookie = login_cookie(&app, "hunter2").await;
        assert!(cookie.is_some());
        assert!(cookie.unwrap().starts_with("katban_admin="));
    }

    #[tokio::test]
    async fn login_cookie_gets_secure_behind_https_proxy() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _guard) = router_with_home(tmp.path(), Some("default"), Some("hunter2"));
        // Behind caddy, x-forwarded-proto=https -> cookie must carry Secure.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/login")
                    .header("content-type", "application/json")
                    .header("x-forwarded-proto", "https")
                    .body(axum::body::Body::from("{\"password\":\"hunter2\"}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::OK);
        let set_cookie = res
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .expect("set-cookie present");
        assert!(
            set_cookie.contains("Secure"),
            "cookie missing Secure: {set_cookie}"
        );
    }

    #[tokio::test]
    async fn wrong_password_is_401() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _guard) = router_with_home(tmp.path(), Some("default"), Some("hunter2"));
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/login")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from("{\"password\":\"nope\"}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn write_requires_auth() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _guard) = router_with_home(tmp.path(), Some("default"), Some("hunter2"));
        // No cookie: unauthenticated.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/board/default/cards")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from("{\"prompt\":\"new\"}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn add_and_advance_card_with_session() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _guard) = router_with_home(tmp.path(), Some("default"), Some("hunter2"));
        let cookie = login_cookie(&app, "hunter2").await.unwrap();

        // Add a card.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/board/default/cards")
                    .header("content-type", "application/json")
                    .header("cookie", &cookie)
                    .body(axum::body::Body::from("{\"prompt\":\"new card\"}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::OK);
        let body = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        let api: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let new_id = api["cards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["prompt"] == "new card")
            .expect("new card present");
        let new_id = new_id["id"].as_str().unwrap();

        // Advance it (backlog -> queued).
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/board/default/cards/{new_id}/advance"))
                    .header("cookie", &cookie)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::OK);
        let body = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        let api: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let advanced = api["cards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == new_id)
            .expect("advanced card present");
        assert_eq!(advanced["status"], "queued");
    }

    #[tokio::test]
    async fn set_status_via_api_moves_card_and_rejects_bad_status() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _guard) = router_with_home(tmp.path(), Some("default"), Some("hunter2"));
        let cookie = login_cookie(&app, "hunter2").await.unwrap();
        // Bad status -> 400.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/board/default/cards/whatever/status")
                    .header("content-type", "application/json")
                    .header("cookie", &cookie)
                    .body(axum::body::Body::from("{\"status\":\"bogus\"}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::BAD_REQUEST);

        // Move the first seeded card to 'blocked' (manual hold is a real status
        // the picker exposes).
        let card_id = {
            let board = board::load_board("default").unwrap().unwrap();
            board.cards[0].id.clone()
        };
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/board/default/cards/{card_id}/status"))
                    .header("content-type", "application/json")
                    .header("cookie", &cookie)
                    .body(axum::body::Body::from("{\"status\":\"blocked\"}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::OK);
        let body = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        let api: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let card = api["cards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == card_id)
            .expect("card present");
        assert_eq!(card["status"], "blocked");
    }

    #[tokio::test]
    async fn merge_requires_auth_and_a_pinned_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _guard) = router_with_home(tmp.path(), Some("default"), Some("hunter2"));
        let cookie = login_cookie(&app, "hunter2").await.unwrap();

        // Unauthenticated merge -> 401.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/board/default/cards/whatever/merge")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::UNAUTHORIZED);

        // A card without a pinned commit can't be merged -> 409 with a reason.
        let card_id = {
            let board = board::load_board("default").unwrap().unwrap();
            board.cards[0].id.clone()
        };
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/board/default/cards/{card_id}/merge"))
                    .header("cookie", &cookie)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::CONFLICT);
        let body = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let err = json["error"].as_str().expect("error message");
        assert!(
            err.contains("review") || err.contains("commit"),
            "err: {err}"
        );
    }

    #[tokio::test]
    async fn cross_origin_write_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _guard) = router_with_home(tmp.path(), Some("default"), Some("hunter2"));
        let cookie = login_cookie(&app, "hunter2").await.unwrap();
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/board/default/cards")
                    .header("content-type", "application/json")
                    .header("cookie", &cookie)
                    .header("origin", "http://evil.example")
                    .header("host", "127.0.0.1:8790")
                    .body(axum::body::Body::from("{\"prompt\":\"x\"}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::FORBIDDEN);
    }

    #[tokio::test]
    async fn board_events_route_is_sse() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _guard) = router_with_home(tmp.path(), None, None);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/board/events")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), HttpStatus::OK);
        let ctype = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(ctype.starts_with("text/event-stream"), "got {ctype}");
    }

    #[tokio::test]
    async fn exposed_reads_require_auth_but_loopback_stays_open() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _guard) = router_with_home(tmp.path(), Some("default"), Some("hunter2"));

        // Direct loopback (no proxy headers): reads stay open by design.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/projects")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::OK);

        // Proxied (caddy in front — public admin subdomain): reads need a
        // session, so a random web visitor sees 401, not the board contents.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/projects")
                    .header("x-forwarded-for", "203.0.113.5")
                    .header("x-forwarded-proto", "https")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::UNAUTHORIZED);

        // Same proxied request with a valid session -> OK.
        let cookie = login_cookie(&app, "hunter2").await.unwrap();
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/projects")
                    .header("x-forwarded-for", "203.0.113.5")
                    .header("x-forwarded-proto", "https")
                    .header("cookie", &cookie)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::OK);

        // Board data + SSE follow the same rule.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/board/default")
                    .header("x-forwarded-for", "203.0.113.5")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::UNAUTHORIZED);
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/board/events")
                    .header("x-forwarded-for", "203.0.113.5")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::UNAUTHORIZED);

        // /api/me stays open everywhere: it only reports authed true/false.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/me")
                    .header("x-forwarded-for", "203.0.113.5")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::UNAUTHORIZED); // correctly reflects no session
    }

    #[tokio::test]
    async fn me_reports_authed_only_with_valid_session() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _guard) = router_with_home(tmp.path(), Some("default"), Some("hunter2"));
        // No cookie -> 401.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/me")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::UNAUTHORIZED);
        // Valid cookie -> 200.
        let cookie = login_cookie(&app, "hunter2").await.unwrap();
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/me")
                    .header("cookie", &cookie)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::OK);
    }

    #[tokio::test]
    async fn login_is_gated_by_origin_like_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _guard) = router_with_home(tmp.path(), Some("default"), Some("hunter2"));
        // Cross-origin login must be refused, even with the correct password.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/login")
                    .header("content-type", "application/json")
                    .header("origin", "http://evil.example")
                    .header("host", "127.0.0.1:8790")
                    .body(axum::body::Body::from("{\"password\":\"hunter2\"}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::FORBIDDEN);

        // Same-origin (loopback, matching host+port) is allowed.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/login")
                    .header("content-type", "application/json")
                    .header("origin", "http://127.0.0.1:8790")
                    .header("host", "127.0.0.1:8790")
                    .body(axum::body::Body::from("{\"password\":\"hunter2\"}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::OK);
    }

    #[tokio::test]
    async fn new_board_creates_board_and_requires_auth() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _guard) = router_with_home(tmp.path(), None, Some("hunter2"));
        // Unauthenticated -> 401.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/boards")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from("{\"name\":\"fresh\"}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::UNAUTHORIZED);

        // Authed -> creates the board; it now appears in /api/projects.
        let cookie = login_cookie(&app, "hunter2").await.unwrap();
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/boards")
                    .header("content-type", "application/json")
                    .header("cookie", &cookie)
                    .body(axum::body::Body::from("{\"name\":\"fresh\"}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::OK);
        // Duplicate -> 409.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/boards")
                    .header("content-type", "application/json")
                    .header("cookie", &cookie)
                    .body(axum::body::Body::from("{\"name\":\"fresh\"}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::CONFLICT);
        let projects = board::existing_projects();
        assert!(projects.contains(&"fresh".to_string()));
    }

    #[tokio::test]
    async fn link_and_unlink_dependencies_via_api() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _guard) = router_with_home(tmp.path(), Some("default"), Some("hunter2"));
        let cookie = login_cookie(&app, "hunter2").await.unwrap();
        let ids: Vec<String> = board::load_board("default")
            .unwrap()
            .unwrap()
            .cards
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids.len(), 2); // seeded: db card + ui card (already linked db->ui)

        // Linking the reverse would create a cycle -> 400 with explanation.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/board/default/link")
                    .header("content-type", "application/json")
                    .header("cookie", &cookie)
                    .body(axum::body::Body::from(format!(
                        "{{\"from\":\"{}\",\"to\":\"{}\"}}",
                        ids[1], ids[0]
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::BAD_REQUEST);

        // Unlink the seeded dependency, then verify readiness flips.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/board/default/unlink")
                    .header("content-type", "application/json")
                    .header("cookie", &cookie)
                    .body(axum::body::Body::from(format!(
                        "{{\"from\":\"{}\",\"to\":\"{}\"}}",
                        ids[1], ids[0]
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::OK);
        let deps = board::load_board("default").unwrap().unwrap();
        assert!(deps.dependencies.is_empty());
        // Unlinking again -> 404.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/board/default/unlink")
                    .header("content-type", "application/json")
                    .header("cookie", &cookie)
                    .body(axum::body::Body::from(format!(
                        "{{\"from\":\"{}\",\"to\":\"{}\"}}",
                        ids[1], ids[0]
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), HttpStatus::NOT_FOUND);
    }
}
