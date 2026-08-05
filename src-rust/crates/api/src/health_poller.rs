// health_poller.rs — Startup + periodic health poller (spec §6.4).
//
// Probes each configured free-upstream key via the existing
// `validate_upstream_key()` helper (GET `/v1/models`, 5s timeout; for
// upstreams whose models endpoint doesn't check auth — nvidia,
// huggingface, openrouter, sambanova, cline — a 1-token
// `chat/completions` confirmation probe) and logs the results so dead
// keys are surfaced before the first user request hits them.
//
// Runs once at startup, then every `health_poll_interval_secs` (default 300s).
// 0 disables the periodic sweep (startup probe still runs).  Probes are
// staggered, respect existing cooldowns, and skip providers without keys.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::providers::free::validate_upstream_key;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Default poll interval (seconds).  0 disables periodic sweeps; the
/// startup probe still runs once.
pub const DEFAULT_HEALTH_POLL_INTERVAL_SECS: u64 = 300;

/// Result of probing a single stored key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthProbeResult {
    /// Upstream id (auth-store slot) the key belongs to.
    pub upstream: String,
    /// Index of the key within the upstream's key pool.
    pub key_idx: usize,
    /// `true` when the key answered the models endpoint successfully.
    pub ok: bool,
    /// Validation error message, when `ok` is `false`.
    pub err: Option<String>,
}

/// Aggregate outcome of one probe sweep over every stored free-upstream key.
/// Shared between the background poller, the `/health` command, and the TUI
/// footer marker.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeOutcome {
    pub checked: usize,
    pub unhealthy: usize,
    pub results: Vec<HealthProbeResult>,
}

impl ProbeOutcome {
    /// `true` when at least one key was probed and none were unhealthy.
    pub fn is_all_healthy(&self) -> bool {
        self.checked > 0 && self.unhealthy == 0
    }
}

// ---------------------------------------------------------------------------
// Last-sweep state (read by the TUI footer marker and /health command)
// ---------------------------------------------------------------------------

static LAST_SWEEP: OnceLock<Mutex<Option<ProbeOutcome>>> = OnceLock::new();
/// Monotonic counter bumped on every store — lets consumers skip cloning the
/// full outcome until it actually changes (avoids per-frame clones in the
/// TUI event loop).
static LAST_SWEEP_GEN: AtomicU64 = AtomicU64::new(0);

fn last_sweep_slot() -> &'static Mutex<Option<ProbeOutcome>> {
    LAST_SWEEP.get_or_init(|| Mutex::new(None))
}

/// Remember the most recent sweep outcome so any consumer (TUI, /health)
/// can read the latest result without waiting for a channel message.
pub fn store_last_sweep(outcome: &ProbeOutcome) {
    if let Ok(mut guard) = last_sweep_slot().lock() {
        *guard = Some(outcome.clone());
        LAST_SWEEP_GEN.fetch_add(1, Ordering::Relaxed);
    }
}

/// Clone of the most recent sweep outcome, or `None` if no sweep has run.
pub fn take_last_sweep() -> Option<ProbeOutcome> {
    last_sweep_slot().lock().ok().and_then(|g| g.clone())
}

/// Monotonic generation counter bumped on every [`store_last_sweep`].
pub fn last_sweep_generation() -> u64 {
    LAST_SWEEP_GEN.load(Ordering::Relaxed)
}

/// Run the health poller async background task.
///
/// * `interval_secs` — 0 disables periodic repeats (startup still runs).
/// * `free_provider` — the running FreeProvider; unhealthy keys are injected
///   into its key rings via `mark_key_exhausted` (spec §6.4).
/// * `report_tx` — optional channel; each sweep's [`ProbeOutcome`] is pushed
///   here so the TUI can surface dead keys the moment a probe finds them.
pub async fn run_health_poller(
    interval_secs: u64,
    free_provider: Option<std::sync::Arc<dyn crate::provider::LlmProvider>>,
    report_tx: Option<mpsc::UnboundedSender<ProbeOutcome>>,
) {
    // Startup sweep — always runs.
    poll_and_log(free_provider.as_deref(), report_tx.as_ref()).await;

    if interval_secs == 0 {
        debug!("health_poller: periodic sweep disabled (interval = 0)");
        return;
    }

    let interval = Duration::from_secs(interval_secs);
    loop {
        tokio::time::sleep(interval).await;
        poll_and_log(free_provider.as_deref(), report_tx.as_ref()).await;
    }
}

/// Run one synchronous probe sweep, returning per-key results.
///
/// Used by the `/health` slash command. The whole sweep runs on a plain OS
/// thread so the blocking HTTP clients in `validate_upstream_key` are created
/// and dropped outside any tokio runtime context (mirrors
/// `fetch_cline_free_model`).
pub fn probe_sync() -> ProbeOutcome {
    std::thread::spawn(|| probe_sync_body(None))
        .join()
        .unwrap_or_default()
}

/// Run a synchronous probe sweep limited to a single upstream id.
///
/// Used by `/health <upstream>` so one provider can be probed without
/// waiting for the whole catalog (which can take 30-60s on multi-key pools).
/// Unlike [`probe_sync`], the partial outcome is **not** stored as the last
/// sweep — a targeted probe must not clobber the full-sweep data the TUI
/// footer marker and /ctx-viz read.
pub fn probe_sync_for(upstream_id: &str) -> ProbeOutcome {
    let upstream_id = upstream_id.to_string();
    std::thread::spawn(move || probe_sync_body(Some(&upstream_id)))
        .join()
        .unwrap_or_default()
}

fn probe_sync_body(filter: Option<&str>) -> ProbeOutcome {
    let auth_store = clawde_core::AuthStore::load();

    // Fan out: spawn one thread per configured upstream so slow upstreams
    // never block fast ones. Within each upstream thread, keys are still
    // probed sequentially with a small inter-key delay for rate limiting.
    let mut handles = Vec::new();
    for upstream in crate::providers::free::FREE_CATALOG {
        if filter.is_some_and(|f| f != upstream.id) {
            continue;
        }
        let Some(keys) = resolve_keys(&auth_store, upstream.id) else {
            continue;
        };
        let keys: Vec<String> = keys.iter().filter(|k| k.len() >= 8).cloned().collect();
        if keys.is_empty() {
            continue;
        }
        let upstream_id = upstream.id.to_string();
        handles.push(std::thread::spawn(move || {
            let mut results = Vec::new();
            let mut unhealthy = 0usize;
            let checked = keys.len();
            for (key_idx, key) in keys.iter().enumerate() {
                match validate_upstream_key(&upstream_id, key) {
                    Ok(()) => {
                        debug!(upstream = upstream_id, key_idx, "health poll: key OK");
                        results.push(HealthProbeResult {
                            upstream: upstream_id.clone(),
                            key_idx,
                            ok: true,
                            err: None,
                        });
                    }
                    Err(err) => {
                        warn!(
                            upstream = upstream_id,
                            key_idx,
                            err = %err,
                            "health poll: key unhealthy"
                        );
                        unhealthy += 1;
                        results.push(HealthProbeResult {
                            upstream: upstream_id.clone(),
                            key_idx,
                            ok: false,
                            err: Some(err),
                        });
                    }
                }
                // Small gap between keys within the same upstream to avoid
                // triggering provider rate limits. Upstream threads run in
                // parallel, so this doesn't block unrelated providers.
                std::thread::sleep(Duration::from_millis(200));
            }
            ProbeOutcome {
                unhealthy,
                checked,
                results,
            }
        }));
    }

    // Collect results from all upstream threads.
    let mut outcome = ProbeOutcome::default();
    for handle in handles {
        match handle.join() {
            Ok(upstream_outcome) => {
                outcome.checked += upstream_outcome.checked;
                outcome.unhealthy += upstream_outcome.unhealthy;
                outcome.results.extend(upstream_outcome.results);
            }
            Err(_) => {
                // Thread panicked — log and continue.
                warn!("health poll: upstream probe thread panicked");
            }
        }
    }

    // Only full sweeps update the shared last-sweep slot; targeted probes
    // keep the footer marker / /ctx-viz consistent with the full picture.
    if filter.is_none() {
        store_last_sweep(&outcome);
    }
    outcome
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn resolve_keys(auth_store: &clawde_core::AuthStore, upstream_id: &str) -> Option<Vec<String>> {
    crate::providers::free::resolve_free_upstream_keys(auth_store, upstream_id)
}

/// Run one async probe sweep with staggered HTTP checks.
/// When `free_provider` is Some, definitively-dead keys are injected into the
/// running key rings via `mark_key_exhausted`; the resulting [`ProbeOutcome`]
/// is stored and pushed on `report_tx` (when provided).
async fn poll_and_log(
    free_provider: Option<&dyn crate::provider::LlmProvider>,
    report_tx: Option<&mpsc::UnboundedSender<ProbeOutcome>>,
) {
    let auth_store = clawde_core::AuthStore::load();
    let targets = build_probe_list(&auth_store);

    if targets.is_empty() {
        debug!("health_poller: no configured free-upstream keys to probe");
        return;
    }

    info!(count = targets.len(), "health_poller: probing");
    let mut outcome = ProbeOutcome::default();

    for upstream_id in &targets {
        // Re-resolve via the alias-aware helper (matches build_probe_list) so
        // shared slots like opencode-zen/opencode-go are actually probed.
        let Some(keys) = resolve_keys(&auth_store, upstream_id) else {
            continue;
        };

        // Probe EVERY key in the pool — not just key 0 — carrying its real
        // index so exhaustion lands on the right ring slot.
        //
        // No length guard needed here: resolve_free_upstream_keys already
        // trims and drops <8-char placeholders, so every key in this list is
        // exactly what a KeyRotatingProvider ring holds (indices align).
        for (key_idx, key) in keys.iter().enumerate() {
            outcome.checked += 1;
            let upstream_id_owned = upstream_id.clone();
            let upstream_id_for_log = upstream_id_owned.clone();
            let key = key.clone();
            let result = tokio::task::spawn_blocking(move || {
                validate_upstream_key(&upstream_id_owned, &key)
            })
            .await;

            match result {
                Ok(Ok(())) => {
                    debug!(
                        upstream = %upstream_id_for_log,
                        key_idx,
                        "health_poller: key OK"
                    );
                    // Clear any lingering cooldown injected by a previous
                    // definitive failure — the key is demonstrably healthy
                    // right now so the key ring must reflect that.
                    if let Some(provider) = free_provider {
                        provider.mark_key_healthy(Some(&upstream_id_for_log), key_idx);
                    }
                    outcome.results.push(HealthProbeResult {
                        upstream: upstream_id_for_log.clone(),
                        key_idx,
                        ok: true,
                        err: None,
                    });
                }
                Ok(Err(err)) => {
                    warn!(
                        upstream = %upstream_id_for_log,
                        key_idx,
                        err = %err,
                        "health_poller: key unhealthy"
                    );
                    outcome.unhealthy += 1;
                    outcome.results.push(HealthProbeResult {
                        upstream: upstream_id_for_log.clone(),
                        key_idx,
                        ok: false,
                        err: Some(err.clone()),
                    });
                    // Inject exhaustion only for definitive auth failures — a
                    // transient 5xx / rate limit / network blip must not evict
                    // a working key from rotation or flip the TUI health
                    // display red (spec §6.4).
                    if let Some(provider) = free_provider {
                        if let Some(cooldown) = classify_health_error(&err) {
                            provider.mark_key_exhausted(
                                Some(&upstream_id_for_log),
                                key_idx,
                                cooldown,
                                Some(err.clone()),
                            );
                        }
                    }
                }
                Err(join_err) => {
                    warn!(
                        upstream = %upstream_id_for_log,
                        key_idx,
                        err = %join_err,
                        "health_poller: spawn_blocking panicked"
                    );
                    outcome.unhealthy += 1;
                    outcome.results.push(HealthProbeResult {
                        upstream: upstream_id_for_log.clone(),
                        key_idx,
                        ok: false,
                        err: Some(format!("probe task panicked: {}", join_err)),
                    });
                }
            }

            // Small stagger between probes so we don't hammer providers.
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    store_last_sweep(&outcome);
    if let Some(tx) = report_tx {
        let _ = tx.send(outcome.clone());
    }

    if outcome.is_all_healthy() {
        info!(
            healthy = outcome.checked,
            "health_poller: all {} upstream keys healthy", outcome.checked,
        );
    } else if outcome.unhealthy > 0 {
        warn!(
            healthy = outcome.checked.saturating_sub(outcome.unhealthy),
            unhealthy = outcome.unhealthy,
            "health_poller: {}/{} upstream key(s) unhealthy",
            outcome.unhealthy,
            outcome.checked,
        );
    }
}

/// Classify a health poll error into an optional cooldown (seconds).
///
/// Returns `Some(cooldown)` only for **definitive** key failures — auth
/// rejections mean the key itself is dead and should be pulled from
/// rotation (and surfaced as unhealthy in the TUI).
///
/// Returns `None` for transient conditions (5xx, connection errors, rate
/// limits): a momentary provider/network hiccup at probe time must not
/// poison the key ring — the request path already handles those signals
/// with its own short cooldowns and fallback.
fn classify_health_error(err: &str) -> Option<u64> {
    let lower = err.to_lowercase();
    let definitive = lower.contains("401")
        || lower.contains("403")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("invalid api key")
        || lower.contains("invalid key")
        || lower.contains("key is invalid");
    if definitive {
        Some(300) // 5 min for auth failures
    } else {
        None
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::resolve_keys;
    use super::{build_probe_list, classify_health_error, last_sweep_generation, probe_sync_for};
    use crate::providers::free::resolve_free_upstream_keys;

    #[test]
    fn auth_failures_are_definitive() {
        // 401/403 and their textual equivalents mean the key itself is dead.
        assert_eq!(
            classify_health_error("Invalid API key (HTTP 401)"),
            Some(300)
        );
        assert_eq!(
            classify_health_error("Invalid API key (HTTP 403)"),
            Some(300)
        );
        assert_eq!(classify_health_error("401 Unauthorized"), Some(300));
        assert_eq!(classify_health_error("403 Forbidden"), Some(300));
        assert_eq!(
            classify_health_error("Unauthorized: invalid key"),
            Some(300)
        );
        assert_eq!(
            classify_health_error("the provided API key is invalid"),
            Some(300)
        );
        assert_eq!(classify_health_error("Invalid API Key provided"), Some(300));
    }

    #[test]
    fn transient_failures_are_not_definitive() {
        // A momentary provider/network/rate blip must not poison the ring.
        assert_eq!(
            classify_health_error("HTTP 500 — unexpected response"),
            None
        );
        assert_eq!(classify_health_error("HTTP 502 — bad gateway"), None);
        assert_eq!(classify_health_error("Connection failed: timed out"), None);
        assert_eq!(
            classify_health_error("Rate limited — try again later"),
            None
        );
        assert_eq!(classify_health_error("HTTP 429 — too many requests"), None);
        assert_eq!(
            classify_health_error("Key too short (min 8 characters)"),
            None
        );
    }

    #[test]
    fn status_code_substrings_do_not_false_positive() {
        // "502" must not match the old blanket "50" rule; "invalid key"
        // phrases that are NOT auth rejections (e.g. a config error) must
        // not be treated as definitive either.
        assert_eq!(classify_health_error("HTTP 502"), None);
        assert_eq!(classify_health_error("no validation endpoint"), None);
        assert_eq!(classify_health_error("unknown upstream"), None);
    }

    #[test]
    fn probe_sync_for_unknown_filter_is_zero_checked() {
        // An unknown upstream id matches no catalog entry, so the sweep
        // probes nothing: zero checked, zero unhealthy, no results, and no
        // network requests are made. Deterministic regardless of auth-store
        // contents (this crate has no TestHome helper, but the filter
        // excludes every catalog upstream before any key lookup happens, so
        // even a real ~/.clawde store can't influence the outcome).
        let gen_before = last_sweep_generation();
        let outcome = probe_sync_for("bogus-upstream");
        assert_eq!(outcome.checked, 0);
        assert_eq!(outcome.unhealthy, 0);
        assert!(outcome.results.is_empty());
        // Partial probes must not clobber the shared last-sweep slot the
        // TUI footer marker and /ctx-viz read.
        assert_eq!(
            last_sweep_generation(),
            gen_before,
            "targeted probes must not clobber the last-sweep slot"
        );
    }

    /// End-to-end alignment contract: the health poller's probe list (per
    /// upstream, in enumerate order) must EXACTLY equal the keys that
    /// `build_free_provider` feeds into each `KeyRotatingProvider` ring.
    ///
    /// Both sides go through `resolve_free_upstream_keys`, but this test locks
    /// it against regression: if the poller ever switched to a different
    /// resolver (e.g. one that includes credentials or skips the trim/>=8
    /// guard), the `key_idx` forwarded into `mark_key_healthy`/
    /// `mark_key_exhausted` would desync from the ring slots.
    #[test]
    fn poller_probe_list_aligns_with_registry_ring_keys() {
        // Craft a store exercising every resolver rule:
        //   - groq: 3 slots, one <8 placeholder that must be dropped -> ring of 2
        //   - opencode-zen: no own slots, falls back to opencode-go slots
        //   - cline: 1 valid key + 1 placeholder -> single-key chain entry
        let (mut store, _home) = crate::test_support::test_auth_store();
        store.keys.insert(
            "groq".to_string(),
            vec![
                "gsk-valid-key-0000000001".to_string(),
                "   gsk-valid-key-0000000002   ".to_string(), // trimmed, kept
                "short".to_string(),                          // <8, dropped
            ],
        );
        store.keys.insert(
            "opencode-go".to_string(),
            vec!["zen-shared-key-00000000000000".to_string()],
        );
        store.keys.insert(
            "cline".to_string(),
            vec![
                "sk-valid-cline-key-0000000001".to_string(),
                "short".to_string(),
            ],
        );

        // The poller walks FREE_CATALOG and includes every upstream with at
        // least one usable key.
        let targets = build_probe_list(&store);
        assert!(
            targets.contains(&"groq".to_string()),
            "groq must be probed (2 usable keys)"
        );
        assert!(
            targets.contains(&"opencode-zen".to_string()),
            "opencode-zen must be probed via opencode-go slot fallback"
        );
        assert!(
            targets.contains(&"cline".to_string()),
            "cline must be probed (1 usable key)"
        );

        // Per-upstream probe list == registry ring keys, in order.
        let groq_probe = resolve_keys(&store, "groq").expect("groq keys");
        let groq_ring = resolve_free_upstream_keys(&store, "groq").expect("groq ring");
        assert_eq!(
            groq_probe, groq_ring,
            "poller probe list must equal ring keys for groq"
        );
        assert_eq!(
            groq_probe,
            vec![
                "gsk-valid-key-0000000001".to_string(),
                "gsk-valid-key-0000000002".to_string(),
            ],
            "placeholder dropped, whitespace trimmed, order preserved"
        );
        // key_idx 0 and 1 in the probe map to ring slots 0 and 1.
        assert_eq!(groq_probe.len(), 2, "ring holds exactly the probed keys");

        let zen_probe = resolve_keys(&store, "opencode-zen").expect("zen keys");
        let zen_ring = resolve_free_upstream_keys(&store, "opencode-zen").expect("zen ring");
        assert_eq!(
            zen_probe, zen_ring,
            "opencode-zen probe must equal ring built from opencode-go slots"
        );
        assert_eq!(zen_probe, vec!["zen-shared-key-00000000000000".to_string()]);

        // A single usable key still yields exactly one probe entry (single-key
        // chain path uses first_free_upstream_key, but the poller probes the
        // same ring-aligned list).
        let cline_probe = resolve_keys(&store, "cline").expect("cline keys");
        assert_eq!(
            cline_probe,
            vec!["sk-valid-cline-key-0000000001".to_string()]
        );
    }
}
