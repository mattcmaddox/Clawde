// Dev-site hosting server (v0): serve a folder over HTTP on loopback with an
// SSE "changed" stream that injected script tags turn into a browser reload.
//
// Security posture for v0: loopback only, path-traversal guarded, no code
// execution, no directory listing. caddy integration, LAN/public exposure, and
// auth come in later slices (see the katban-selfhost spec §10).

use crate::reload;
use anyhow::Context;
use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{header, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

/// Route browsers subscribe to for live-reload change events.
pub const RELOAD_EVENTS_ROUTE: &str = "/__katban/events";

/// Injected into every served HTML page; reloads the tab on any change event.
const RELOAD_SCRIPT: &str =
    r#"<script>new EventSource('/__katban/events').onmessage=()=>location.reload();</script>"#;

#[derive(Clone)]
struct HostState {
    root: PathBuf,
    reload_tx: broadcast::Sender<()>,
    /// Live-reload mode: injects the reload script and disables caching so a
    /// save is visible immediately. Locked/published sites run with this off.
    live_reload: bool,
}

/// Build the site router. When `live` is true, a polling watcher broadcasts
/// change events that the SSE route relays to browsers and served HTML gets
/// the reload script injected. When false (locked/published), pages are
/// served untouched and cacheable.
pub fn build_router(root: PathBuf, live: bool) -> Router {
    let (reload_tx, _) = broadcast::channel::<()>(16);
    if live {
        reload::spawn_watcher(root.clone(), reload_tx.clone());
    }
    let state = HostState {
        root,
        reload_tx,
        live_reload: live,
    };
    Router::new()
        .route(RELOAD_EVENTS_ROUTE, get(reload_events))
        .route("/", get(static_file))
        .route("/{*path}", get(static_file))
        .with_state(state)
}

/// Serve a folder on 127.0.0.1 until Ctrl-C / SIGTERM.
pub async fn run(root: PathBuf, port: u16, reload: bool) -> anyhow::Result<()> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    run_on(root, addr, reload).await
}

/// Serve a folder on an explicit address (LAN/public exposure) until
/// Ctrl-C / SIGTERM.
pub async fn run_on(root: PathBuf, addr: std::net::SocketAddr, reload: bool) -> anyhow::Result<()> {
    let app = build_router(root.clone(), reload);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(root = %root.display(), %addr, reload, "katban site serving");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Parse a `--host` value into a bind address. Loopback by default; the CLI
/// refuses non-loopback unless `--allow-non-loopback` was passed.
pub fn parse_bind_addr(host: &str, port: u16) -> anyhow::Result<std::net::SocketAddr> {
    let ip: std::net::IpAddr = host
        .parse()
        .with_context(|| format!("--host must be an IP address, got '{host}'"))?;
    Ok(std::net::SocketAddr::new(ip, port))
}

pub fn is_loopback(addr: std::net::SocketAddr) -> bool {
    addr.ip().is_loopback()
}

/// Resolve once on Ctrl-C or SIGTERM — reused by the site, guest, and board
/// servers for graceful shutdown.
pub async fn shutdown_signal() {
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

async fn reload_events(
    State(state): State<HostState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.reload_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|item| match item {
        Ok(()) => Some(Ok::<_, Infallible>(Event::default().data("changed"))),
        Err(_) => None, // lagged behind -> skip
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn static_file(State(state): State<HostState>, path: Option<AxumPath<String>>) -> Response {
    // The `/` route carries no path param; the wildcard route carries one.
    let raw = path.map(|AxumPath(p)| p).unwrap_or_default();
    let Some(rel) = sanitize(&raw) else {
        return (StatusCode::BAD_REQUEST, "invalid path").into_response();
    };
    let Some(target) = resolve(&state.root, &rel) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut response = Response::new(Body::empty());
    if let Some(ctype) = content_type(&target) {
        if let Ok(value) = header::HeaderValue::from_str(ctype) {
            response.headers_mut().insert(header::CONTENT_TYPE, value);
        }
    }
    // Never let a browser MIME-sniff an unknown-extension file into HTML.
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    if is_html(&target) && state.live_reload {
        // Inject the live-reload script and never let browsers cache the page,
        // so a save is visible on the next reload. HTML pages are small, so
        // reading them fully to inject is fine.
        let contents = match tokio::fs::read(&target).await {
            Ok(bytes) => bytes,
            Err(_) => return StatusCode::NOT_FOUND.into_response(),
        };
        let html = String::from_utf8_lossy(&contents);
        *response.body_mut() = Body::from(inject_reload_script(&html));
        if let Ok(value) = header::HeaderValue::from_str("no-cache") {
            response.headers_mut().insert(header::CACHE_CONTROL, value);
        }
    } else {
        // Stream files instead of reading them fully into memory, so a
        // multi-GB video/image in a dev site can't OOM the server.
        let file = match tokio::fs::File::open(&target).await {
            Ok(file) => file,
            Err(_) => return StatusCode::NOT_FOUND.into_response(),
        };
        if let Ok(meta) = file.metadata().await {
            if let Ok(value) = header::HeaderValue::from_str(&meta.len().to_string()) {
                response.headers_mut().insert(header::CONTENT_LENGTH, value);
            }
        }
        *response.body_mut() = Body::from_stream(tokio_util::io::ReaderStream::new(file));
    }
    response
}

/// Reject empty/`..`/percent-encoded segments; never resolve outside the root.
fn sanitize(path: &str) -> Option<PathBuf> {
    if path.is_empty() {
        return Some(PathBuf::from("index.html"));
    }
    let mut out = PathBuf::new();
    for segment in path.split(['/', '\\']) {
        if segment.is_empty() || segment == "." {
            continue;
        }
        // Block traversal and any percent-encoding outright — we serve
        // literal files, there is no legitimate '%' in a relative path here.
        if segment == ".." || segment.contains('%') {
            return None;
        }
        out.push(segment);
    }
    Some(out)
}

/// Canonicalize against the root; a directory resolves to its index.html.
fn resolve(root: &Path, rel: &Path) -> Option<PathBuf> {
    let canon_root = root.canonicalize().ok()?;
    let mut candidate = canon_root.join(rel);
    if candidate.is_dir() {
        candidate = candidate.join("index.html");
    }
    let canon = candidate.canonicalize().ok()?;
    canon.starts_with(&canon_root).then_some(canon)
}

fn is_html(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("html") | Some("htm")
    )
}

fn inject_reload_script(html: &str) -> String {
    match find_case_insensitive(html.as_bytes(), b"</head>") {
        Some(index) => format!("{}{}{}", &html[..index], RELOAD_SCRIPT, &html[index..]),
        None => format!("{html}{RELOAD_SCRIPT}"),
    }
}

fn find_case_insensitive(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

fn content_type(path: &Path) -> Option<&'static str> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => Some("text/html; charset=utf-8"),
        "css" => Some("text/css; charset=utf-8"),
        "js" | "mjs" => Some("text/javascript; charset=utf-8"),
        "json" => Some("application/json"),
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "svg" => Some("image/svg+xml"),
        "webp" => Some("image/webp"),
        "ico" => Some("image/x-icon"),
        "txt" | "md" => Some("text/plain; charset=utf-8"),
        "wasm" => Some("application/wasm"),
        "pdf" => Some("application/pdf"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use std::fs;
    use tower::ServiceExt;

    fn write(root: &Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn html_page() -> &'static str {
        "<!doctype html><html><head><title>demo</title></head><body>hi</body></html>"
    }

    #[tokio::test]
    async fn serves_index_with_injected_reload_script() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "index.html", html_page());
        let app = build_router(tmp.path().to_path_buf(), true);

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains(RELOAD_SCRIPT));
        assert!(text.contains("EventSource"));
        assert!(text.contains("<head>"));
    }

    #[tokio::test]
    async fn locked_site_has_no_injection_and_is_cacheable() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "index.html", html_page());
        let app = build_router(tmp.path().to_path_buf(), false);
        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            !text.contains("EventSource"),
            "locked site must not inject reload"
        );
    }

    #[tokio::test]
    async fn non_html_is_served_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "style.css", "body { color: red; }");
        let app = build_router(tmp.path().to_path_buf(), false);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/style.css")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_LENGTH).unwrap(),
            "20"
        );
        assert_eq!(
            response
                .headers()
                .get(header::X_CONTENT_TYPE_OPTIONS)
                .unwrap(),
            "nosniff"
        );
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        assert_eq!(&body[..], b"body { color: red; }");
    }

    #[tokio::test]
    async fn every_served_file_carries_nosniff() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "index.html", html_page());
        write(tmp.path(), "blob.bin", "\u{0}\u{1}\u{2}");
        let app = build_router(tmp.path().to_path_buf(), false);
        for uri in ["/", "/index.html", "/blob.bin"] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{uri} not served");
            assert_eq!(
                response
                    .headers()
                    .get(header::X_CONTENT_TYPE_OPTIONS)
                    .unwrap(),
                "nosniff",
                "{uri} missing nosniff"
            );
        }
    }

    #[tokio::test]
    async fn missing_file_is_404() {
        let tmp = tempfile::tempdir().unwrap();
        let app = build_router(tmp.path().to_path_buf(), false);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/nope.html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn traversal_is_blocked() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "index.html", html_page());
        // Secret lives outside the served root.
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "top secret").unwrap();
        let secret = outside.path().join("secret.txt").canonicalize().unwrap();

        let app = build_router(tmp.path().to_path_buf(), false);
        let secret_attempt = format!("/..{}", secret.display());
        for attempt in [
            secret_attempt.as_str(),
            "/../etc/passwd",
            "/%2e%2e/etc/passwd",
            "/sub/../../etc/passwd",
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(attempt).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert!(
                response.status() == StatusCode::BAD_REQUEST
                    || response.status() == StatusCode::NOT_FOUND,
                "attempt {attempt} leaked: {}",
                response.status()
            );
        }
    }

    #[tokio::test]
    async fn directory_serves_index_html() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "sub/index.html", "<h1>nested</h1>");
        let app = build_router(tmp.path().to_path_buf(), false);
        let response = app
            .oneshot(Request::builder().uri("/sub").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("<h1>nested</h1>"));
    }

    #[tokio::test]
    async fn reload_events_route_is_sse() {
        let tmp = tempfile::tempdir().unwrap();
        let app = build_router(tmp.path().to_path_buf(), false);
        let response = app
            .oneshot(
                Request::builder()
                    .uri(RELOAD_EVENTS_ROUTE)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let ctype = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(ctype.starts_with("text/event-stream"), "got {ctype}");
    }

    #[test]
    fn injection_inserts_before_head_close() {
        let html = "<html><head><title>t</title></head><body>x</body></html>";
        let out = inject_reload_script(html);
        let head_index = out.find("</head>").unwrap();
        assert!(out[..head_index].contains(RELOAD_SCRIPT));
    }

    #[test]
    fn injection_appends_when_no_head() {
        let out = inject_reload_script("just some text");
        assert!(out.ends_with(RELOAD_SCRIPT));
    }

    #[test]
    fn parse_bind_addr_accepts_loopback_and_any() {
        let loopback = parse_bind_addr("127.0.0.1", 8788).unwrap();
        assert!(is_loopback(loopback));
        let any = parse_bind_addr("0.0.0.0", 8788).unwrap();
        assert!(!is_loopback(any));
        assert!(parse_bind_addr("not-an-ip", 8788).is_err());
    }

    #[test]
    fn sanitize_rejects_traversal_and_encoding() {
        assert!(sanitize("..").is_none());
        assert!(sanitize("../etc/passwd").is_none());
        assert!(sanitize("a/%2e%2e/b").is_none());
        assert!(sanitize("a/b%2fc").is_none());
        assert_eq!(sanitize(""), Some(PathBuf::from("index.html")));
        assert_eq!(sanitize("a/b.html"), Some(PathBuf::from("a/b.html")));
    }
}
