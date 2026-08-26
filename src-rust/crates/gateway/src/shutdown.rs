//! Graceful shutdown with SSE drain semantics.
//!
//! Naive `with_graceful_shutdown` never stops while an SSE stream is active
//! (hyper#2787). The gateway tracks active streams explicitly and drains them
//! within a grace window before aborting.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

/// Shutdown coordinator shared with the router.
#[derive(Clone)]
pub struct ShutdownCoordinator {
    /// Set true when shutdown begins; `/healthz` returns 503 after grace.
    pub draining: Arc<AtomicBool>,
    /// Active SSE stream count.
    pub active_streams: Arc<std::sync::atomic::AtomicUsize>,
    /// Cancelled on SIGINT/SIGTERM.
    pub cancel: CancellationToken,
    /// Grace period for draining active streams.
    pub grace_secs: u64,
}

impl ShutdownCoordinator {
    pub fn new(grace_secs: u64) -> Self {
        Self {
            draining: Arc::new(AtomicBool::new(false)),
            active_streams: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            cancel: CancellationToken::new(),
            grace_secs,
        }
    }

    /// Begin graceful shutdown:
    /// 1. set draining (new requests rejected),
    /// 2. wait up to `grace_secs` for active streams to finish,
    /// 3. cancel remaining streams.
    pub async fn begin_shutdown(&self) {
        self.draining.store(true, Ordering::SeqCst);
        let grace = Duration::from_secs(self.grace_secs);
        let start = std::time::Instant::now();
        while start.elapsed() < grace {
            if self.active_streams.load(Ordering::SeqCst) == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // Grace expired: cancel remaining streams.
        self.cancel.cancel();
    }
}

/// Install SIGINT + SIGTERM handlers that trigger graceful shutdown.
pub fn install_signal_handlers(cancel: CancellationToken) {
    let int_cancel = cancel.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("gateway: received SIGINT, shutting down");
        int_cancel.cancel();
    });
    #[cfg(unix)]
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
        sigterm.recv().await;
        tracing::info!("gateway: received SIGTERM, shutting down");
        cancel.cancel();
    });
}
