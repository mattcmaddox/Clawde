// health_poller.rs — Startup + periodic zero-token health poller (spec §6.4).
//
// Probes each configured free-upstream key via the existing
// `validate_upstream_key()` helper (HEAD/GET to `/v1/models`, 5s timeout,
// zero tokens spent) and logs the results so dead keys are surfaced before
// the first user request hits them.
//
// Runs once at startup, then every `health_poll_interval_secs` (default 300s).
// 0 disables the periodic sweep (startup probe still runs).  Probes are
// staggered, respect existing cooldowns, and skip providers without keys.

use std::time::Duration;

use tracing::{debug, info, warn};

use crate::providers::free::validate_upstream_key;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Default poll interval (seconds).  0 disables periodic sweeps; the
/// startup probe still runs once.
pub const DEFAULT_HEALTH_POLL_INTERVAL_SECS: u64 = 300;

/// Run the health poller async background task.
///
/// * `interval_secs` — 0 disables periodic repeats (startup still runs).
/// * `free_provider` — the running FreeProvider; unhealthy keys are injected
///   into its key rings via `mark_key_exhausted` (spec §6.4).
pub async fn run_health_poller(
    interval_secs: u64,
    free_provider: Option<std::sync::Arc<dyn crate::provider::LlmProvider>>,
) {
    // Startup sweep — always runs.
    poll_and_log(free_provider.as_deref()).await;

    if interval_secs == 0 {
        debug!("health_poller: periodic sweep disabled (interval = 0)");
        return;
    }

    let interval = Duration::from_secs(interval_secs);
    loop {
        tokio::time::sleep(interval).await;
        poll_and_log(free_provider.as_deref()).await;
    }
}

/// Run one synchronous sweep (blocking).  Reserved for programmatic use
/// (e.g. a future `/health probe` command).  The blocking HTTP calls are
/// offloaded to a dedicated OS thread so this is safe to call from within
/// a tokio runtime context (mirrors `fetch_cline_free_model`).
#[allow(dead_code)]
pub fn poll_sync() {
    std::thread::spawn(|| {
        poll_sync_body();
    })
    .join()
    .ok();
}

fn poll_sync_body() {
    let auth_store = clawde_core::AuthStore::load();
    let mut healthy = 0usize;
    let mut unhealthy = 0usize;

    for upstream in crate::providers::free::FREE_CATALOG {
        let keys = resolve_keys(&auth_store, upstream.id);
        let Some(key) = keys.and_then(|k| k.first().cloned()) else {
            continue;
        };
        if key.len() < 8 {
            continue;
        }
        match validate_upstream_key(upstream.id, &key) {
            Ok(()) => {
                debug!(upstream = upstream.id, "health poll (sync): key OK");
                healthy += 1;
            }
            Err(err) => {
                warn!(
                    upstream = upstream.id,
                    err = %err,
                    "health poll (sync): key unhealthy"
                );
                unhealthy += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    if unhealthy == 0 && healthy > 0 {
        info!(
            healthy,
            "health poll (sync): all {} upstream keys OK", healthy
        );
    } else if unhealthy > 0 {
        warn!(
            healthy,
            unhealthy,
            "health poll (sync): {}/{} upstream key(s) unhealthy",
            unhealthy,
            healthy + unhealthy
        );
    }
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn resolve_keys(auth_store: &clawde_core::AuthStore, upstream_id: &str) -> Option<Vec<String>> {
    crate::providers::free::resolve_free_upstream_keys(auth_store, upstream_id)
}

/// Run one async probe sweep with staggered HTTP checks.
/// When `free_provider` is Some, unhealthy keys are injected into the
/// running key rings via `mark_key_exhausted`.
async fn poll_and_log(free_provider: Option<&dyn crate::provider::LlmProvider>) {
    let auth_store = clawde_core::AuthStore::load();
    let targets = build_probe_list(&auth_store);

    if targets.is_empty() {
        debug!("health_poller: no configured free-upstream keys to probe");
        return;
    }

    info!(count = targets.len(), "health_poller: probing");
    let mut healthy = 0usize;
    let mut unhealthy = 0usize;

    for upstream_id in &targets {
        let key = auth_store
            .keys_for(upstream_id)
            .and_then(|k| k.first().cloned())
            .unwrap_or_default();

        if key.len() < 8 {
            continue;
        }

        let upstream_id_owned = upstream_id.clone();
        let upstream_id_for_log = upstream_id_owned.clone();
        let result =
            tokio::task::spawn_blocking(move || validate_upstream_key(&upstream_id_owned, &key))
                .await;

        match result {
            Ok(Ok(())) => {
                debug!(upstream = %upstream_id_for_log, "health_poller: key OK");
                healthy += 1;
            }
            Ok(Err(err)) => {
                warn!(
                    upstream = %upstream_id_for_log,
                    err = %err,
                    "health_poller: key unhealthy"
                );
                unhealthy += 1;
                // Inject exhaustion into the running key ring (spec §6.4).
                if let Some(provider) = free_provider {
                    let cooldown = classify_health_error(&err);
                    provider.mark_key_exhausted(
                        Some(&upstream_id_for_log),
                        0,
                        cooldown,
                        Some(err.clone()),
                    );
                }
            }
            Err(join_err) => {
                warn!(
                    upstream = %upstream_id_for_log,
                    err = %join_err,
                    "health_poller: spawn_blocking panicked"
                );
                unhealthy += 1;
            }
        }

        // Small stagger between probes so we don't hammer providers.
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    if unhealthy == 0 && healthy > 0 {
        info!(
            healthy,
            "health_poller: all {} upstream keys healthy", healthy
        );
    } else if unhealthy > 0 {
        warn!(
            healthy,
            unhealthy,
            "health_poller: {}/{} upstream key(s) unhealthy",
            unhealthy,
            healthy + unhealthy,
        );
    }
}

/// Classify a health poll error into a cooldown duration (seconds).
/// Maps known error patterns to sensible cooldowns for the key ring.
fn classify_health_error(err: &str) -> u64 {
    let lower = err.to_lowercase();
    if lower.contains("401") || lower.contains("403") || lower.contains("unauthorized") {
        300 // 5 min for auth failures
    } else if lower.contains("429") || lower.contains("rate") || lower.contains("quota") {
        3600 // 1 hour for rate/quota
    } else if lower.contains("50") || lower.contains("server") {
        120 // 2 min for server errors
    } else {
        300 // default 5 min for unknown errors
    }
}

/// Walk the free catalog and build the list of upstream ids to probe.
fn build_probe_list(auth_store: &clawde_core::AuthStore) -> Vec<String> {
    let mut targets = Vec::new();

    for upstream in crate::providers::free::FREE_CATALOG {
        let Some(keys) = resolve_keys(auth_store, upstream.id) else {
            continue;
        };
        if keys.is_empty() {
            continue;
        }
        targets.push(upstream.id.to_string());
    }

    targets
}
