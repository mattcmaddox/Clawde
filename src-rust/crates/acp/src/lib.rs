//! Agent Client Protocol (ACP) server for Clawde.
//!
//! ACP is the open protocol pioneered by Zed for standardizing communication
//! between AI coding agents and editors (Zed, Neovim, JetBrains, VS Code, …).
//! Spec: <https://agentclientprotocol.com>
//!
//! This crate turns the local `clawde` binary into a compliant ACP agent
//! over newline-delimited JSON-RPC 2.0 on stdio. Editors launch `clawde acp`
//! as a subprocess and drive it through the protocol's standard methods:
//!
//! | Method                       | Direction  | Notes                                       |
//! |------------------------------|------------|---------------------------------------------|
//! | `initialize`                 | C → A      | Capability negotiation                      |
//! | `authenticate`               | C → A      | No-op (Clawde uses local credentials)      |
//! | `session/new`                | C → A      | Create a session with cwd + MCP roster      |
//! | `session/prompt`             | C → A      | Run a turn; streams `session/update` events |
//! | `session/cancel`             | C → A (no resp) | Cancel an in-flight prompt             |
//! | `session/update`             | A → C (no resp) | Streamed text/tool deltas              |
//! | `session/request_permission` | A → C      | Tool approval dialog                        |
//!
//! Per-session MCP server configs supplied via `session/new` are validated and
//! connected in a session-owned context. stdio, streamable HTTP, and legacy SSE
//! transports are supported; remote transports use SSRF validation, DNS-pinned
//! clients, and redirect checks. Client-supplied HTTP headers are not forwarded;
//! authentication uses Clawde's existing credential flow. Configured global MCP
//! servers from `settings.json` remain available to all sessions subject to the
//! runtime trust and isolation checks.

mod connection;
mod permission;
mod prompt;
mod runtime;
mod server;
mod sessions;

use std::fs;
use std::sync::Arc;

use clawde_core::config::AcpServerConfig;
use tokio::net::{TcpListener, ToSocketAddrs};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

pub use connection::Connection;
pub use runtime::AgentRuntime;
pub use server::AgentServer;

// ---------------------------------------------------------------------------
// TCP / embedded mode
// ---------------------------------------------------------------------------

/// Run the ACP server on the current process' stdin / stdout. Returns when
/// stdin reaches EOF or when the runtime fails to initialize.
pub async fn run_acp_server() -> anyhow::Result<()> {
    // We must NEVER write to stdout outside the protocol — every byte on
    // stdout is parsed by the client as JSON-RPC. Force logs to stderr.
    install_stderr_tracing();

    let working_dir = std::env::current_dir()?;
    info!(cwd = %working_dir.display(), version = env!("CARGO_PKG_VERSION"), "ACP: starting server");

    let runtime = AgentRuntime::build(working_dir).await?;
    let runtime = Arc::new(runtime);
    let connection = Connection::new(tokio::io::stdout());
    let server = AgentServer::new(connection.clone(), runtime);

    let (tx, mut rx) = mpsc::unbounded_channel();
    let reader_fut = connection::run_reader(connection, tokio::io::stdin(), tx);

    // Track in-flight dispatch tasks so they can finish writing their
    // responses before the runtime shuts down. The dispatch future only
    // returns once the reader has dropped `tx` (closing `rx`) AND every
    // spawned handler has resolved.
    let dispatch_fut = async {
        let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        while let Some(msg) = rx.recv().await {
            tasks.push(server.dispatch(msg));
        }
        for handle in tasks {
            let _ = handle.await;
        }
    };

    let (reader_res, _) = tokio::join!(reader_fut, dispatch_fut);
    if let Err(e) = reader_res {
        error!(?e, "ACP: reader loop failed");
    }

    info!("ACP: server shutdown");
    Ok(())
}

fn install_stderr_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let _ = fmt()
        .with_env_filter(
            EnvFilter::try_from_env("CLAURST_ACP_LOG").unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init();
}

// ---------------------------------------------------------------------------
// TCP mode (standalone or embedded)
// ---------------------------------------------------------------------------

/// Run the ACP server in TCP mode, accepting connections on `addr`.
///
/// When `config` contains `tls_cert_path` and `tls_key_path`, the server wraps
/// each connection with TLS (rustls). Without TLS config the server uses raw
/// TCP — suitable for trusted LANs or when combined with an SSH tunnel.
///
/// Each TCP connection gets its own `AgentServer` instance sharing the same
/// `AgentRuntime`. The function runs until the listener errors.
pub async fn run_acp_server_tcp(
    addr: impl ToSocketAddrs + std::fmt::Debug,
    config: Option<&AcpServerConfig>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    install_stderr_tracing();
    let working_dir = std::env::current_dir()?;

    // Warn if TLS is half-configured (only one of cert/key paths set).
    if let Some(cfg) = config {
        if cfg.tls_cert_path.is_some() != cfg.tls_key_path.is_some() {
            warn!(
                "ACP: TLS is half-configured — only one of tlsCertPath/tlsKeyPath is set; falling back to plain TCP"
            );
        }
    }

    // Build optional TLS acceptor from config cert/key paths.
    let tls_acceptor = config
        .and_then(|cfg| {
            let cert_path = cfg.tls_cert_path.as_ref()?;
            let key_path = cfg.tls_key_path.as_ref()?;
            Some(load_tls_acceptor(cert_path, key_path))
        })
        .transpose()?;

    if tls_acceptor.is_some() {
        info!(?addr, cwd = %working_dir.display(), "ACP: starting TCP server (TLS enabled)");
    } else {
        info!(?addr, cwd = %working_dir.display(), "ACP: starting TCP server (plain TCP)");
    }

    // Bind before constructing the runtime so an unsafe address is rejected
    // before credentials, providers, or MCP resources are initialized.
    let listener = TcpListener::bind(&addr).await?;
    let bound_addr = listener.local_addr()?;
    let allow_non_loopback = config.is_some_and(|cfg| cfg.allow_non_loopback);
    validate_acp_bind_address(bound_addr, allow_non_loopback)?;

    let runtime = Arc::new(AgentRuntime::build(working_dir).await?);
    info!(
        ?bound_addr,
        "ACP: TCP server listening (cancel on shutdown)"
    );

    // Track every per-connection task so shutdown can drain them while the
    // runtime is still alive. Dropping the runtime with an in-flight prompt
    // (a reqwest timer poll) triggers a Tokio "context is being shutdown"
    // panic, so we must abort in-flight work before returning.
    let mut conn_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    loop {
        let (stream, peer) = tokio::select! {
            result = listener.accept() => result?,
            _ = cancel.cancelled() => {
                info!("ACP: TCP server shutting down (cancellation requested)");
                // Cooperative drain: each connection task observes the cancel
                // token and aborts its own in-flight dispatch tasks + reader,
                // so no timer is polled after the runtime begins shutting
                // down. Bound the wait, then force-abort stragglers.
                let mut pending: Vec<Option<tokio::task::JoinHandle<()>>> =
                    std::mem::take(&mut conn_tasks)
                        .into_iter()
                        .map(Some)
                        .collect();
                // Await with `Option::take` so the force-abort fallback below
                // still owns the not-yet-awaited handles (draining the vec
                // mid-await would leave the fallback with nothing to abort).
                let deadline = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    async {
                        for slot in &mut pending {
                            // Borrow the handle while awaiting it so a timeout
                            // leaves ownership in `pending` for the force-abort
                            // fallback below.
                            if let Some(handle) = slot.as_mut() {
                                let _ = handle.await;
                            }
                        }
                    },
                );
                if deadline.await.is_err() {
                    for handle in pending.into_iter().flatten() {
                        handle.abort();
                        let _ = handle.await;
                    }
                }
                return Ok(());
            }
        };
        info!(?peer, "ACP: new TCP connection");
        let runtime = runtime.clone();

        let (reader, writer) = if let Some(ref acceptor) = tls_acceptor {
            // Perform TLS handshake before splitting.
            let tls_stream = match acceptor.accept(stream).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(?peer, error = %e, "ACP: TLS handshake failed");
                    continue;
                }
            };
            let (r, w) = tokio::io::split(tls_stream);
            // Box the split halves so the rest of the handler is type-agnostic.
            (
                Box::new(r) as Box<dyn tokio::io::AsyncRead + Unpin + Send>,
                Box::new(w) as Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
            )
        } else {
            let (r, w) = tokio::io::split(stream);
            (
                Box::new(r) as Box<dyn tokio::io::AsyncRead + Unpin + Send>,
                Box::new(w) as Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
            )
        };

        let task_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            let connection = Connection::new(writer);
            let server = AgentServer::new(connection.clone(), runtime);

            let (tx, mut rx) = mpsc::unbounded_channel();
            // Spawn the reader as its own task so this connection task can
            // abort it on shutdown (a plain join would wait for the client to
            // disconnect).
            let reader_task = tokio::spawn(connection::run_reader(connection.clone(), reader, tx));

            let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
            loop {
                tokio::select! {
                    msg = rx.recv() => match msg {
                        Some(msg) => tasks.push(server.dispatch(msg)),
                        // Reader finished: the client disconnected.
                        None => break,
                    },
                    // Server shutdown requested: cancel in-flight work while
                    // the runtime is still alive. The prompt handler is a
                    // separate spawned task (see AgentServer::dispatch), so it
                    // must be aborted explicitly — otherwise it keeps polling
                    // a timer during runtime shutdown and panics. Cancelling
                    // session turns first makes prompts exit cooperatively at
                    // their next await point, which the abort then reaps
                    // immediately.
                    _ = task_cancel.cancelled() => {
                        server.sessions.cancel_all_turns();
                        for handle in &tasks {
                            handle.abort();
                        }
                        reader_task.abort();
                        break;
                    }
                }
            }
            // Drain phase: wait for in-flight dispatch tasks, but cancel
            // session turns first so prompts observe the cancellation
            // cooperatively (JoinHandle::abort cannot interrupt a task
            // running on the blocking pool). After cancellation, await each
            // handle briefly; if a task ignores the cooperative cancel, abort
            // and detach rather than blocking shutdown forever.
            server.sessions.cancel_all_turns();
            for mut handle in tasks {
                let deadline = tokio::time::timeout(std::time::Duration::from_secs(5), &mut handle);
                if deadline.await.is_err() {
                    handle.abort();
                    // Do not await: the task may be on the blocking pool and
                    // uncancellable. Cooperatively cancelled prompts exit on
                    // their own shortly after.
                }
            }
            let reader_res = reader_task.await;
            if let Ok(Err(e)) = reader_res {
                warn!(?peer, error = %e, "ACP: TCP reader error");
            }
            info!(?peer, "ACP: TCP connection closed");
        });
        conn_tasks.push(handle);
    }
}

fn validate_acp_bind_address(
    addr: std::net::SocketAddr,
    allow_non_loopback: bool,
) -> anyhow::Result<()> {
    let is_loopback = match addr.ip() {
        std::net::IpAddr::V4(ip) => ip.is_loopback(),
        std::net::IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|mapped| mapped.is_loopback())
        }
    };
    if is_loopback || allow_non_loopback {
        return Ok(());
    }

    anyhow::bail!(
        "refusing ACP non-loopback bind at {addr}; ACP has no application-level authentication. Use 127.0.0.1 or an authenticated tunnel, or explicitly opt in with acpServer.allowNonLoopback=true / --allow-non-loopback"
    )
}

/// Start an embedded ACP TCP server in a background tokio task if enabled in config.
/// The server runs on the configured address (default `127.0.0.1:9876`) and accepts
/// ACP JSON-RPC connections from LAN clients. Tokio cancels the task on shutdown.
/// TLS is enabled automatically when `tls_cert_path` and `tls_key_path` are set.
///
/// Returns a `CancellationToken` that can be used to request graceful shutdown:
/// calling `cancel()` on it stops accepting new connections and lets the accept
/// loop return. The caller must still await/drop the spawned task for cleanup.
pub fn start_embedded_acp_server(config: &AcpServerConfig) -> CancellationToken {
    let cancel = CancellationToken::new();
    if !config.enabled {
        return cancel;
    }
    let addr = config.listen.clone();
    let config = config.clone();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        if let Err(e) = run_acp_server_tcp(addr, Some(&config), cancel_clone).await {
            error!(error = %e, "ACP: embedded server failed");
        }
    });
    cancel
}

/// Load TLS certificate and key PEM files and build a `TlsAcceptor`.
fn load_tls_acceptor(cert_path: &str, key_path: &str) -> anyhow::Result<tokio_rustls::TlsAcceptor> {
    use rustls::pki_types::PrivateKeyDer;

    let cert_bytes = fs::read(cert_path)
        .map_err(|e| anyhow::anyhow!("failed to read TLS cert '{}': {}", cert_path, e))?;
    let key_bytes = fs::read(key_path)
        .map_err(|e| anyhow::anyhow!("failed to read TLS key '{}': {}", key_path, e))?;

    // Parse PEM-encoded certificate chain.
    let mut cert_reader = std::io::BufReader::new(cert_bytes.as_slice());
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_reader)
            .filter_map(|r| r.ok())
            .collect();

    if certs.is_empty() {
        anyhow::bail!(
            "TLS cert file is empty or contains no certificates: {}",
            cert_path
        );
    }

    // Parse private key: try PKCS8 first, then EC, then RSA.
    // Each format is tried with its own BufReader; we eagerly collect the
    // iterator into a Vec so the borrow on the reader is released before
    // the reader variable goes out of scope.
    let key: PrivateKeyDer<'static> = {
        let mut rdr = std::io::BufReader::new(key_bytes.as_slice());
        let keys: Vec<_> = rustls_pemfile::pkcs8_private_keys(&mut rdr)
            .filter_map(|r| r.ok())
            .collect();
        if let Some(k) = keys.into_iter().next() {
            PrivateKeyDer::Pkcs8(k)
        } else {
            let mut rdr = std::io::BufReader::new(key_bytes.as_slice());
            let keys: Vec<_> = rustls_pemfile::ec_private_keys(&mut rdr)
                .filter_map(|r| r.ok())
                .collect();
            if let Some(k) = keys.into_iter().next() {
                PrivateKeyDer::Sec1(k)
            } else {
                let mut rdr = std::io::BufReader::new(key_bytes.as_slice());
                let keys: Vec<_> = rustls_pemfile::rsa_private_keys(&mut rdr)
                    .filter_map(|r| r.ok())
                    .collect();
                keys.into_iter()
                    .next()
                    .map(PrivateKeyDer::Pkcs1)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                        "TLS key file contains no supported private key (PKCS8, EC, or RSA): {}",
                        key_path
                    )
                    })?
            }
        }
    };

    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("invalid TLS key/cert pair: {}", e))?;

    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(server_config)))
}

#[cfg(test)]
mod bind_tests {
    use super::validate_acp_bind_address;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    #[test]
    fn allows_ipv4_and_ipv6_loopback() {
        assert!(validate_acp_bind_address(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9876),
            false,
        )
        .is_ok());
        assert!(validate_acp_bind_address(
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 9876),
            false,
        )
        .is_ok());
    }

    #[test]
    fn allows_ipv4_mapped_loopback() {
        let mapped = Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x7f00, 1);
        assert!(
            validate_acp_bind_address(SocketAddr::new(IpAddr::V6(mapped), 9876), false,).is_ok()
        );
    }

    #[test]
    fn rejects_non_loopback_by_default() {
        for ip in [
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        ] {
            assert!(validate_acp_bind_address(SocketAddr::new(ip, 9876), false).is_err());
        }
    }

    #[test]
    fn explicit_opt_in_allows_non_loopback() {
        assert!(validate_acp_bind_address(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 9876),
            true,
        )
        .is_ok());
    }

    #[tokio::test]
    async fn tcp_server_rejects_non_loopback_before_runtime_startup() {
        let config = clawde_core::config::AcpServerConfig::default();
        let error = super::run_acp_server_tcp(
            "0.0.0.0:0",
            Some(&config),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect_err("non-loopback ACP bind must fail closed by default");
        assert!(error.to_string().contains("refusing ACP non-loopback bind"));
    }
}
