// health_poller.rs — Startup + periodic health poller (spec §6.4).
//
// Probes each configured free-upstream key via the existing
// `probe_upstream_key()` helper (GET `/v1/models`, 5s timeout; for
// upstreams whose models endpoint doesn't check auth — nvidia,
// huggingface, openrouter, sambanova, cline — a 1-token
// `chat/completions` confirmation probe) and logs the results so dead
// keys are surfaced before the first user request hits them.
//
// Runs once shortly after startup (after a 2s grace so the TUI's first frame
// wins the CPU), then every `health_poll_interval_secs` (default 300s).
// 0 disables the periodic sweep (startup probe still runs). Probes use bounded
// concurrency, preserve ring indexes, and skip providers without keys.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use crate::providers::free::{probe_upstream_key, UpstreamKeyProbe};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Default poll interval (seconds).  0 disables periodic sweeps; the
/// startup probe still runs once.
pub const DEFAULT_HEALTH_POLL_INTERVAL_SECS: u64 = 300;

/// Grace period before the startup sweep begins, so the TUI can render its
/// first frame (and any onboarding/welcome screen) before the background key
/// probes start burning CPU. Short enough that dead keys are still surfaced
/// before the user's first request.
pub const STARTUP_SWEEP_DELAY: Duration = Duration::from_secs(2);

/// Result of probing a single stored key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthProbeResult {
    /// Upstream id (auth-store slot) the key belongs to.
    pub upstream: String,
    /// Index of the key within the upstream's key pool.
    pub key_idx: usize,
    /// `true` when the key is not definitively invalid. A key that hit a
    /// transient failure (5xx / connection / rate limit) is still `ok` — the
    /// upstream accepted the key; it was just busy or unreachable.
    pub ok: bool,
    /// `true` when the probe could not verify the key because of a transient
    /// failure. The key is not proven invalid, but it wasn't confirmed either.
    pub transient: bool,
    /// Validation error message, when `ok` is `false` or `transient` is `true`.
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
    /// `true` when at least one key was probed, every probe was definitive,
    /// and none were unhealthy. Transient results are deliberately excluded:
    /// they are not evidence that a key is healthy.
    pub fn is_all_healthy(&self) -> bool {
        self.checked > 0
            && self.unhealthy == 0
            && self.results.iter().all(|result| !result.transient)
    }

    /// Number of probes that could not produce a definitive verdict.
    pub fn transient_count(&self) -> usize {
        self.results
            .iter()
            .filter(|result| result.transient)
            .count()
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
    // Startup sweep — always runs, but deferred briefly so the TUI's first
    // frame and the onboarding/welcome render win the CPU before the probe
    // storm (15 keys × up to 5s timeouts) starts. The sweep is already fully
    // async (bounded spawn_blocking), so this only shifts its start; it never
    // blocks the main loop.
    tokio::time::sleep(STARTUP_SWEEP_DELAY).await;
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
/// thread so the blocking HTTP clients in `probe_upstream_key` are created
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

    // Fan out by upstream. Keys within one upstream remain sequential so a
    // provider's own rate limits are respected, but there is no unconditional
    // sleep between keys or unrelated providers.
    let mut handles = Vec::new();
    for upstream in crate::providers::free::FREE_CATALOG {
        if filter.is_some_and(|f| f != upstream.id) {
            continue;
        }
        let Some(keys) = resolve_keys(&auth_store, upstream.id) else {
            continue;
        };
        let upstream_id = upstream.id.to_string();
        handles.push(std::thread::spawn(move || {
            let mut outcome = ProbeOutcome::default();
            for (key_idx, key) in keys.iter().enumerate() {
                outcome.checked += 1;
                let verdict = probe_upstream_key(&upstream_id, key);
                let _ = record_probe_verdict(&mut outcome, &upstream_id, key_idx, verdict);
            }
            outcome
        }));
    }

    let mut outcome = ProbeOutcome::default();
    for handle in handles {
        match handle.join() {
            Ok(upstream_outcome) => {
                outcome.unhealthy += upstream_outcome.unhealthy;
                outcome.checked += upstream_outcome.checked;
                outcome.results.extend(upstream_outcome.results);
            }
            Err(_) => warn!("health poll: upstream probe thread panicked"),
        }
    }
    outcome.results.sort_by(|a, b| {
        let upstream_order = |id: &str| {
            crate::providers::free::FREE_CATALOG
                .iter()
                .position(|entry| entry.id == id)
                .unwrap_or(usize::MAX)
        };
        upstream_order(&a.upstream)
            .cmp(&upstream_order(&b.upstream))
            .then(a.key_idx.cmp(&b.key_idx))
    });

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
    const MAX_CONCURRENT_PROBES: usize = 8;
    let semaphore = std::sync::Arc::new(Semaphore::new(MAX_CONCURRENT_PROBES));
    let mut jobs = JoinSet::new();
    let mut outcome = ProbeOutcome::default();

    // Submit every key immediately, with bounded blocking-probe concurrency.
    // This keeps independent providers concurrent without creating an
    // unbounded number of blocking threads or imposing a fixed delay between
    // unrelated keys.
    for (catalog_idx, upstream_id) in targets.iter().enumerate() {
        let Some(keys) = resolve_keys(&auth_store, upstream_id) else {
            continue;
        };
        for (key_idx, key) in keys.iter().enumerate() {
            outcome.checked += 1;
            let permit = match semaphore.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => {
                    let _ = record_probe_verdict(
                        &mut outcome,
                        upstream_id,
                        key_idx,
                        UpstreamKeyProbe::Transient("probe scheduler closed".to_string()),
                    );
                    continue;
                }
            };
            let upstream_id = upstream_id.clone();
            let probe_upstream_id = upstream_id.clone();
            let key = key.clone();
            jobs.spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    probe_upstream_key(&probe_upstream_id, &key)
                })
                .await;
                drop(permit);
                (catalog_idx, key_idx, upstream_id, result)
            });
        }
    }

    while let Some(job) = jobs.join_next().await {
        match job {
            Ok((_catalog_idx, key_idx, upstream_id, Ok(verdict))) => {
                if let Some((cooldown, reason)) =
                    record_probe_verdict(&mut outcome, &upstream_id, key_idx, verdict)
                {
                    if let Some(provider) = free_provider {
                        provider.mark_key_exhausted(
                            Some(&upstream_id),
                            key_idx,
                            cooldown,
                            Some(reason),
                        );
                    }
                } else if let Some(provider) = free_provider {
                    // Only a definitive success clears a prior health-poller
                    // cooldown. A transient result must leave ring state alone.
                    if outcome.results.last().is_some_and(|result| {
                        result.upstream == upstream_id
                            && result.key_idx == key_idx
                            && !result.transient
                    }) {
                        provider.mark_key_healthy(Some(&upstream_id), key_idx);
                    }
                }
            }
            Ok((_catalog_idx, key_idx, upstream_id, Err(join_error))) => {
                let _ = record_probe_verdict(
                    &mut outcome,
                    &upstream_id,
                    key_idx,
                    UpstreamKeyProbe::Transient(format!("probe task panicked: {}", join_error)),
                );
            }
            Err(join_error) => {
                warn!(err = %join_error, "health poll: probe task panicked");
            }
        }
    }

    outcome.results.sort_by(|a, b| {
        let upstream_order = |id: &str| {
            crate::providers::free::FREE_CATALOG
                .iter()
                .position(|entry| entry.id == id)
                .unwrap_or(usize::MAX)
        };
        upstream_order(&a.upstream)
            .cmp(&upstream_order(&b.upstream))
            .then(a.key_idx.cmp(&b.key_idx))
    });
    store_last_sweep(&outcome);
    if let Some(tx) = report_tx {
        let _ = tx.send(outcome.clone());
    }

    if outcome.is_all_healthy() {
        info!(
            healthy = outcome.checked,
            "health_poller: all {} upstream keys healthy", outcome.checked,
        );
    } else if outcome.unhealthy > 0 || outcome.transient_count() > 0 {
        warn!(
            healthy = outcome
                .checked
                .saturating_sub(outcome.unhealthy)
                .saturating_sub(outcome.transient_count()),
            transient = outcome.transient_count(),
            unhealthy = outcome.unhealthy,
            "health_poller: {} unhealthy and {} transient upstream key probe(s)",
            outcome.unhealthy,
            outcome.transient_count(),
        );
    }
}

/// Record a typed probe verdict and return `(cooldown, reason)` only when
/// the key is definitively invalid. This keeps health display, ring mutation,
/// and persistence decisions on the same verdict instead of reparsing error
/// strings in multiple places.
fn record_probe_verdict(
    outcome: &mut ProbeOutcome,
    upstream_id: &str,
    key_idx: usize,
    verdict: UpstreamKeyProbe,
) -> Option<(u64, String)> {
    match verdict {
        UpstreamKeyProbe::Valid => {
            debug!(upstream = upstream_id, key_idx, "health poll: key OK");
            outcome.results.push(HealthProbeResult {
                upstream: upstream_id.to_string(),
                key_idx,
                ok: true,
                transient: false,
                err: None,
            });
            None
        }
        UpstreamKeyProbe::Invalid(reason) => {
            warn!(upstream = upstream_id, key_idx, err = %reason, "health poll: key unhealthy");
            outcome.unhealthy += 1;
            outcome.results.push(HealthProbeResult {
                upstream: upstream_id.to_string(),
                key_idx,
                ok: false,
                transient: false,
                err: Some(reason.clone()),
            });
            Some((300, reason))
        }
        UpstreamKeyProbe::Transient(reason) => {
            warn!(
                upstream = upstream_id,
                key_idx,
                err = %reason,
                "health poll: transient failure (key not proven invalid)"
            );
            outcome.results.push(HealthProbeResult {
                upstream: upstream_id.to_string(),
                key_idx,
                ok: true,
                transient: true,
                err: Some(reason),
            });
            None
        }
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
    use super::{
        build_probe_list, last_sweep_generation, probe_sync_for, record_probe_verdict, ProbeOutcome,
    };
    use crate::providers::free::resolve_free_upstream_keys;
    use crate::providers::free::UpstreamKeyProbe;

    #[test]
    fn typed_probe_results_distinguish_invalid_and_transient() {
        let mut outcome = ProbeOutcome {
            checked: 2,
            ..Default::default()
        };
        let invalid = record_probe_verdict(
            &mut outcome,
            "groq",
            0,
            UpstreamKeyProbe::Invalid("Invalid API key (HTTP 401)".to_string()),
        );
        assert_eq!(invalid.map(|(cooldown, _)| cooldown), Some(300));
        assert_eq!(outcome.unhealthy, 1);
        assert!(!outcome.results[0].ok);

        let transient = record_probe_verdict(
            &mut outcome,
            "nvidia",
            1,
            UpstreamKeyProbe::Transient("Server error (HTTP 503)".to_string()),
        );
        assert_eq!(transient, None);
        assert_eq!(outcome.unhealthy, 1);
        assert!(outcome.results[1].ok);
        assert!(outcome.results[1].transient);
        assert!(!outcome.is_all_healthy());
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
