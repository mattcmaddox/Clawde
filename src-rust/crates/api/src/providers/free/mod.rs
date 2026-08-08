// providers/free.rs — Composite "Free" provider.
//
// Stacks multiple upstream free-tier providers behind a single
// `free/auto` synthetic model id. The chain is iterated in priority
// order on every request — if an upstream fails (auth, rate limit,
// server error, request error) *before* any data has been streamed,
// the same request is retried against the next upstream. Mid-stream
// failures are surfaced as-is; we don't replay partial conversations.
//
// Inspired by https://github.com/tashfeenahmed/freellmapi — the same
// "aggregate the free tiers from many providers behind one OpenAI-
// compatible endpoint" idea, ported into claurst's native provider
// trait.
//
// Routing:
//   * `free` / `free/auto` / `auto`  →  try each configured upstream
//     in catalog order, using that upstream's `default_model`.
//   * `<upstream_id>/<rest>`         →  pin that upstream, then
//     fall through to the rest of the chain on transient errors.
//   * anything else                  →  passed through verbatim
//     to the first upstream in the chain.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use std::time::Instant;

use clawde_core::provider_id::ProviderId;

use crate::provider::LlmProvider;
use crate::provider_error::ProviderError;
use crate::provider_types::ProviderRequest;
use serde::{Deserialize, Serialize}; // Sub-modules split out of this file to keep the provider core readable.
                                     // Everything that was previously `pub` in this file is re-exported so
                                     // `crate::providers::free::X` and `providers::free::X` paths continue to
                                     // work unchanged. Re-exports are explicit (not globs) so internal items
                                     // like `CLOUDFLARE_PROBE_MODEL` don't leak into the public API.
mod catalog;
mod discovery;

// Internal const shared with the catalog's cloudflare entry and the
// cloudflare chat probe below.
use catalog::CLOUDFLARE_PROBE_MODEL;

pub use catalog::{
    catalog_entry, store_free_model_defaults, take_free_model_defaults, FreeUpstream, FREE_CATALOG,
};
pub use discovery::{
    discovery_for, fetch_cline_free_model, fetch_cline_free_models, fetch_gemini_models,
    fetch_openai_compat_model_list, fetch_openrouter_free_model, run_live_discovery,
    FreeModelDiscovery,
};

// Further sub-modules: inherent impl + streaming + trait impl (mutually
// coupled through private helpers, so one module), the models.dev
// auto-detection helper, and the Phase 2 task classifier (smart router).
mod impls;
mod modelsdev;
mod task_classifier;
pub use modelsdev::fetch_best_free_models_from_modelsdev;
pub use task_classifier::{classify_request, task_preference_ids, TaskType};

// ---------------------------------------------------------------------------
// FreeProvider
// ---------------------------------------------------------------------------

/// One configured entry in a [`FreeProvider`]'s chain.
#[derive(Clone)]
pub struct FreeEntry {
    pub upstream: FreeUpstream,
    pub provider: Arc<dyn LlmProvider>,
    /// Overrides `upstream.default_model` when set. Populated by
    /// [`fetch_best_free_models_from_modelsdev`] at build time so that
    /// the chain always uses the best currently-free model for each
    /// upstream without needing hardcoded catalog changes.
    pub effective_model: Option<String>,
}

/// Routing strategy for the FreeProvider's fallback chain.
///
/// Controls how the provider selects which upstream to try first and in what
/// order. Plumbed from `settings.json` → `providers.free.options.routing`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingStrategy {
    /// Smart default (audit spec §8.4 "Auto, no user config needed"):
    /// classify each request by task and try the upstreams best suited to it
    /// first, refined by historical latency within the task-preferred group.
    /// Behaves like [`RoutingStrategy::TaskBased`].
    #[default]
    Auto,
    /// Try upstreams in catalog (priority) order.
    Sequential,
    /// Randomly select an upstream with failover to the next on failure.
    RandomFailover,
    /// Route to the upstream with the lowest historical latency.
    LatencyBased,
    /// Route by task (audit spec Phase 2): classify each request and try the
    /// upstreams best suited to the task first, then the rest in catalog
    /// order. See `task_classifier::task_preference_ids` for the defaults.
    TaskBased,
}

/// Circuit breaker configuration for the FreeProvider.
///
/// When an upstream fails `max_fails` times within `window_secs`, it is
/// cooled down for `cooldown_secs` and skipped in the fallback loop.
/// Disabled by default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreakerConfig {
    /// Max failures before the upstream is cooled down (0 = disabled).
    #[serde(default = "default_cb_max_fails")]
    pub max_fails: u32,
    /// Time window in seconds for counting failures.
    #[serde(default = "default_cb_window")]
    pub window_secs: u64,
    /// How long to cool down an upstream (seconds).
    #[serde(default = "default_cb_cooldown")]
    pub cooldown_secs: u64,
}

const fn default_cb_max_fails() -> u32 {
    3
}
const fn default_cb_window() -> u64 {
    60
}
const fn default_cb_cooldown() -> u64 {
    120
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            max_fails: 3,
            window_secs: 60,
            cooldown_secs: 120,
        }
    }
}

/// Latency tracking configuration for latency-based routing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencyConfig {
    /// How many samples to keep in the sliding window (0 = disabled).
    #[serde(default = "default_latency_samples")]
    pub max_samples: usize,
}

const fn default_latency_samples() -> usize {
    10
}

impl Default for LatencyConfig {
    fn default() -> Self {
        Self { max_samples: 10 }
    }
}

/// Empty-completion cooldown configuration (spec §6.3 / §6.7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmptyCooldownConfig {
    #[serde(default = "default_empty_max_consecutive")]
    pub max_consecutive: u32,
    #[serde(default = "default_empty_cooldown_secs")]
    pub cooldown_secs: u64,
}

const fn default_empty_max_consecutive() -> u32 {
    3
}
const fn default_empty_cooldown_secs() -> u64 {
    60
}

impl Default for EmptyCooldownConfig {
    fn default() -> Self {
        Self {
            max_consecutive: default_empty_max_consecutive(),
            cooldown_secs: default_empty_cooldown_secs(),
        }
    }
}

impl EmptyCooldownConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

fn is_default_poll(n: &u64) -> bool {
    *n == 300
}

fn is_true(b: &bool) -> bool {
    *b
}

fn is_upstream_server_error(err: &ProviderError) -> bool {
    match err {
        ProviderError::ServerError {
            status: Some(s), ..
        } if (*s >= 500 && *s <= 599) || *s == 498 => true,
        ProviderError::Other {
            status: Some(s), ..
        } if (*s >= 500 && *s <= 599) || *s == 498 => true,
        _ => false,
    }
}

/// Clamp `req.max_tokens` to the entry's configured cap, when one exists.
/// Single source of truth for the per-upstream token cap — used by every
/// dispatch site (non-streaming fallback, streaming fallback,
/// `RetryingFreeStream` re-dispatch, and the first-byte watchdog probe).
fn clamp_max_tokens_for(req: &mut ProviderRequest, entry: &FreeEntry) {
    if let Some(cap) = entry.upstream.max_tokens_cap {
        req.max_tokens = req.max_tokens.min(cap);
    }
}

/// Routing configuration for a [`FreeProvider`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingConfig {
    #[serde(default)]
    pub strategy: RoutingStrategy,
    /// Per-task upstream preference overrides (audit spec Phase 2 §8.4):
    /// `{ "code_generation": ["groq", "cerebras"], ... }` keyed by
    /// `TaskType::key()`. A task with an override uses it verbatim; tasks
    /// without one fall back to the built-in `task_preference_ids` list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_preferences: Option<HashMap<String, Vec<String>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub circuit_breaker: Option<CircuitBreakerConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency: Option<LatencyConfig>,
    #[serde(default = "default_upstream_timeout")]
    pub upstream_timeout_secs: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_upstreams: Vec<String>,
    #[serde(default, skip_serializing_if = "EmptyCooldownConfig::is_default")]
    pub empty_cooldown: EmptyCooldownConfig,
    #[serde(
        default = "default_first_byte_timeout",
        skip_serializing_if = "is_zero"
    )]
    pub first_byte_timeout_secs: u64,
    #[serde(default = "default_staggered_probe", skip_serializing_if = "is_true")]
    pub staggered_probe: bool,
    #[serde(
        default = "default_upstream_5xx_cooldown",
        skip_serializing_if = "is_zero"
    )]
    pub upstream_5xx_cooldown_secs: u64,
    #[serde(
        default = "default_poll_interval",
        skip_serializing_if = "is_default_poll"
    )]
    pub health_poll_interval_secs: u64,
    #[serde(
        default = "default_fallback_retries",
        skip_serializing_if = "is_zero_u32"
    )]
    pub fallback_retries: u32,
}

const fn default_upstream_timeout() -> u64 {
    30
}

const fn default_first_byte_timeout() -> u64 {
    0
}

const fn default_staggered_probe() -> bool {
    true
}

const fn default_upstream_5xx_cooldown() -> u64 {
    45
}

const fn default_poll_interval() -> u64 {
    300
}

const fn default_fallback_retries() -> u32 {
    1
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            strategy: RoutingStrategy::default(),
            task_preferences: None,
            circuit_breaker: None,
            latency: None,
            upstream_timeout_secs: default_upstream_timeout(),
            disabled_upstreams: Vec::new(),
            empty_cooldown: EmptyCooldownConfig::default(),
            first_byte_timeout_secs: default_first_byte_timeout(),
            staggered_probe: default_staggered_probe(),
            upstream_5xx_cooldown_secs: default_upstream_5xx_cooldown(),
            health_poll_interval_secs: default_poll_interval(),
            fallback_retries: default_fallback_retries(),
        }
    }
}

/// Per-upstream failure history and cooldown for the circuit breaker.
struct CooldownState {
    /// Sliding window of failure timestamps per upstream index.
    failures: Vec<VecDeque<Instant>>,
    /// Circuit-breaker cooldown expiry per upstream index.
    cooldown_until: Vec<Option<Instant>>,
    /// Consecutive empty completions per upstream index.
    consecutive_empties: Vec<u32>,
    /// Empty-completion cooldown expiry per upstream index.
    empty_cooldown_until: Vec<Option<Instant>>,
    /// Upstream id per index, for persistence keyed by provider id.
    upstream_ids: Vec<String>,
    /// Optional path for cooldown persistence (empty-completion + 5xx /
    /// circuit-breaker tracks).
    persist_path: Option<std::path::PathBuf>,
    /// Circuit-breaker configuration.
    config: CircuitBreakerConfig,
}

/// Disk snapshot of one upstream's cooldown tracks.
///
/// Carries both the empty-completion track (`consecutive_empties` +
/// `empty_cooldown_until_unix`) and the 5xx / circuit-breaker track
/// (`cooldown_until_unix`). `cooldown_until_unix` is `#[serde(default)]` so
/// files written by older builds (empty-completion only) still parse.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct UpstreamCooldownSnapshot {
    upstream: String,
    consecutive_empties: u32,
    empty_cooldown_until_unix: Option<u64>,
    /// 5xx / circuit-breaker cooldown expiry as a unix timestamp.
    #[serde(default)]
    cooldown_until_unix: Option<u64>,
}

impl CooldownState {
    fn new(n: usize, config: CircuitBreakerConfig) -> Self {
        let mut failures = Vec::with_capacity(n);
        let mut cooldown_until = Vec::with_capacity(n);
        let mut consecutive_empties = Vec::with_capacity(n);
        let mut empty_cooldown_until = Vec::with_capacity(n);
        for _ in 0..n {
            failures.push(VecDeque::new());
            cooldown_until.push(None);
            consecutive_empties.push(0);
            empty_cooldown_until.push(None);
        }
        Self {
            failures,
            cooldown_until,
            consecutive_empties,
            empty_cooldown_until,
            upstream_ids: Vec::new(),
            persist_path: None,
            config,
        }
    }

    fn with_persistence(
        mut self,
        upstream_ids: Vec<String>,
        persist_path: Option<std::path::PathBuf>,
    ) -> Self {
        self.upstream_ids = upstream_ids;
        self.persist_path = persist_path;
        if let Some(path) = self.persist_path.clone() {
            self.load_from_file(&path);
        }
        self
    }
    /// Remove expired cooldowns and old failure timestamps.
    fn prune_expired(&mut self) {
        let now = Instant::now();
        let window = std::time::Duration::from_secs(self.config.window_secs);
        for i in 0..self.failures.len() {
            if let Some(until) = self.cooldown_until[i] {
                if now >= until {
                    self.cooldown_until[i] = None;
                    self.failures[i].clear();
                }
            }
            if let Some(until) = self.empty_cooldown_until[i] {
                if now >= until {
                    self.empty_cooldown_until[i] = None;
                }
            }
            while self.failures[i].front().is_some_and(|t| now - *t > window) {
                self.failures[i].pop_front();
            }
        }
    }

    /// Whether the upstream at `idx` is in circuit-breaker cooldown.
    fn is_in_cooldown(&self, idx: usize) -> bool {
        idx < self.cooldown_until.len() && self.cooldown_until[idx].is_some()
    }

    /// Whether the upstream at `idx` is in empty-completion cooldown.
    fn is_in_empty_cooldown(&self, idx: usize) -> bool {
        idx < self.empty_cooldown_until.len()
            && self.empty_cooldown_until[idx].is_some_and(|t| Instant::now() < t)
    }

    fn empty_cooldown_remaining_secs(&self, idx: usize) -> Option<u64> {
        let until = self.empty_cooldown_until.get(idx).copied().flatten()?;
        let now = Instant::now();
        if now >= until {
            return None;
        }
        Some(until.duration_since(now).as_secs())
    }

    /// Seconds remaining in the 5xx / circuit-breaker cooldown at `idx`,
    /// or `None` when not cooled (or the cooldown has already expired).
    fn cooldown_remaining_secs(&self, idx: usize) -> Option<u64> {
        let until = self.cooldown_until.get(idx).copied().flatten()?;
        let now = Instant::now();
        if now >= until {
            return None;
        }
        Some(until.duration_since(now).as_secs())
    }

    /// Record a failure at `idx`. Returns `true` if the upstream just
    /// crossed the threshold and was put into cooldown.
    fn record_failure(&mut self, idx: usize) -> bool {
        if idx >= self.failures.len() || self.config.max_fails == 0 {
            return false;
        }
        self.failures[idx].push_back(Instant::now());
        if self.failures[idx].len() >= self.config.max_fails as usize {
            self.cooldown_until[idx] =
                Some(Instant::now() + std::time::Duration::from_secs(self.config.cooldown_secs));
            true
        } else {
            false
        }
    }

    fn record_empty(&mut self, idx: usize, max_consecutive: u32, cooldown_secs: u64) -> bool {
        if idx >= self.consecutive_empties.len() || max_consecutive == 0 {
            return false;
        }
        self.consecutive_empties[idx] += 1;
        let just_cooled = if self.consecutive_empties[idx] >= max_consecutive {
            self.consecutive_empties[idx] = 0;
            self.empty_cooldown_until[idx] =
                Some(Instant::now() + std::time::Duration::from_secs(cooldown_secs));
            true
        } else {
            false
        };
        self.save();
        just_cooled
    }

    fn apply_upstream_cooldown(&mut self, idx: usize, cooldown_secs: u64) {
        if cooldown_secs == 0 || idx >= self.cooldown_until.len() {
            return;
        }
        self.cooldown_until[idx] =
            Some(Instant::now() + std::time::Duration::from_secs(cooldown_secs));
        self.save();
    }

    /// Record a success at `idx` — resets the failure counter and the
    /// consecutive-empties counter.
    fn record_success(&mut self, idx: usize) {
        if idx < self.failures.len() {
            self.failures[idx].clear();
        }
        if idx < self.consecutive_empties.len() {
            self.consecutive_empties[idx] = 0;
        }
        self.save();
    }

    fn consecutive_empties(&self, idx: usize) -> u32 {
        self.consecutive_empties.get(idx).copied().unwrap_or(0)
    }

    // -------------------------------------------------------------------
    // Persistence (empty-completion + 5xx/circuit-breaker cooldown tracks)
    // -------------------------------------------------------------------

    fn save(&self) {
        let Some(path) = self.persist_path.as_ref() else {
            return;
        };
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let to_unix = |until: Option<Instant>| -> Option<u64> {
            until.and_then(|t| {
                let remaining = t.duration_since(Instant::now()).as_secs();
                if remaining == 0 {
                    return None;
                }
                Some(now_unix.saturating_add(remaining))
            })
        };
        let entries: Vec<UpstreamCooldownSnapshot> = self
            .upstream_ids
            .iter()
            .enumerate()
            .filter_map(|(idx, id)| {
                let count = self.consecutive_empties.get(idx).copied().unwrap_or(0);
                let empty_until_unix =
                    to_unix(self.empty_cooldown_until.get(idx).copied().flatten());
                let cooldown_until_unix = to_unix(self.cooldown_until.get(idx).copied().flatten());
                if count == 0 && empty_until_unix.is_none() && cooldown_until_unix.is_none() {
                    return None;
                }
                Some(UpstreamCooldownSnapshot {
                    upstream: id.clone(),
                    consecutive_empties: count,
                    empty_cooldown_until_unix: empty_until_unix,
                    cooldown_until_unix,
                })
            })
            .collect();
        if entries.is_empty() {
            let _ = std::fs::remove_file(path);
            return;
        }
        let json = match serde_json::to_string_pretty(&entries) {
            Ok(j) => j,
            Err(_) => return,
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = format!("{}.tmp-{}", path.display(), std::process::id(),);
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }

    fn load_from_file(&mut self, path: &std::path::Path) {
        if !path.exists() {
            return;
        }
        let json = match std::fs::read_to_string(path) {
            Ok(j) => j,
            Err(_) => return,
        };
        let entries: Vec<UpstreamCooldownSnapshot> = match serde_json::from_str(&json) {
            Ok(e) => e,
            Err(_) => return,
        };
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let restore = |until_unix: Option<u64>| -> Option<Instant> {
            until_unix.and_then(|u| {
                let remaining = u.saturating_sub(now_unix);
                (remaining > 0).then(|| Instant::now() + std::time::Duration::from_secs(remaining))
            })
        };
        for entry in entries {
            let Some(idx) = self
                .upstream_ids
                .iter()
                .position(|id| id == &entry.upstream)
            else {
                continue;
            };
            self.consecutive_empties[idx] = entry.consecutive_empties;
            if let Some(until) = restore(entry.empty_cooldown_until_unix) {
                self.empty_cooldown_until[idx] = Some(until);
            }
            if let Some(until) = restore(entry.cooldown_until_unix) {
                self.cooldown_until[idx] = Some(until);
            }
        }
    }
}

/// Per-upstream performance state: latency samples plus dispatch success /
/// failure counters for the routing dialog's model-performance view
/// (spec §8.6).
struct LatencyState {
    /// Sliding window of request durations (seconds) per upstream index.
    samples: Vec<VecDeque<f64>>,
    /// Successful dispatches per upstream index.
    successes: Vec<u32>,
    /// Failed dispatches per upstream index.
    failures: Vec<u32>,
}

impl LatencyState {
    fn new(n: usize) -> Self {
        let mut samples = Vec::with_capacity(n);
        let mut successes = Vec::with_capacity(n);
        let mut failures = Vec::with_capacity(n);
        for _ in 0..n {
            samples.push(VecDeque::with_capacity(10));
            successes.push(0);
            failures.push(0);
        }
        Self {
            samples,
            successes,
            failures,
        }
    }

    /// Record a latency sample at `idx`.
    fn record(&mut self, idx: usize, duration_secs: f64, max_samples: usize) {
        if idx >= self.samples.len() {
            return;
        }
        let q = &mut self.samples[idx];
        if q.len() >= max_samples {
            q.pop_front();
        }
        q.push_back(duration_secs);
    }

    /// Record a successful dispatch at `idx` (success-rate view).
    fn record_success(&mut self, idx: usize) {
        if let Some(s) = self.successes.get_mut(idx) {
            *s = s.saturating_add(1);
        }
    }

    /// Record a failed dispatch at `idx` (success-rate view).
    fn record_failure(&mut self, idx: usize) {
        if let Some(f) = self.failures.get_mut(idx) {
            *f = f.saturating_add(1);
        }
    }

    /// Average latency for upstream `idx`, or `f64::MAX` if no samples.
    fn avg_latency(&self, idx: usize) -> f64 {
        if idx >= self.samples.len() {
            return f64::MAX;
        }
        let q = &self.samples[idx];
        if q.is_empty() {
            return f64::MAX;
        }
        let sum: f64 = q.iter().sum();
        sum / q.len() as f64
    }

    /// Dispatch success rate (0.0–1.0) for upstream `idx`, or `None` when
    /// no dispatch has been recorded yet.
    fn success_rate(&self, idx: usize) -> Option<f64> {
        let successes = *self.successes.get(idx)?;
        let failures = *self.failures.get(idx)?;
        let total = successes + failures;
        (total > 0).then(|| successes as f64 / total as f64)
    }
}

/// Rate-limit information parsed from provider HTTP response headers.
#[derive(Debug, Default)]
pub struct RateLimitInfo {
    pub rpm_limit: Option<u32>,
    pub rpm_remaining: Option<u32>,
    pub rpd_limit: Option<u32>,
    pub rpd_remaining: Option<u32>,
    pub tpm_limit: Option<u32>,
    pub tpm_remaining: Option<u32>,
    pub retry_after: Option<u64>,
    pub headers_found: bool,
}

/// Resolve the key list for a free upstream, handling the OpenCode Zen/Go
/// alias (both slots share the same key).  Used by the health poller and
/// `build_free_provider` in registry.rs.
/// Resolve the rotation keys for a free-catalog upstream, in ring order.
///
/// This is the **single source of truth** for the key list a
/// [`KeyRotatingProvider`] ring is built from, so it must stay **exactly
/// aligned** with [`crate::registry`]'s ring construction: `keys_for` only
/// (no single credential), the OpenCode Zen/Go shared slots collapsed to a
/// single slot (zen first, go as fallback), and each key trimmed with
/// placeholders shorter than 8 chars dropped — the same filtering the
/// registry applies before wrapping a pool in a ring.
///
/// The health poller probes this exact list by index and forwards `key_idx`
/// into the rings, so a key's position here is its ring slot. Any divergence
/// (a prepended credential, merged slots, a retained short key) would mark
/// the wrong key exhausted.
///
/// For display (Connect Free dialog dots) use
/// [`all_stored_free_upstream_keys`], which merges credentials and slots
/// without ring-alignment constraints.
pub fn resolve_free_upstream_keys(
    auth_store: &clawde_core::AuthStore,
    upstream_id: &str,
) -> Option<Vec<String>> {
    let raw: Vec<String> = if upstream_id == "opencode-zen" {
        auth_store
            .keys_for("opencode-zen")
            .or_else(|| auth_store.keys_for("opencode-go"))
            .map(|k| k.to_vec())
    } else {
        auth_store.keys_for(upstream_id).map(|k| k.to_vec())
    }?;

    let filtered: Vec<String> = raw
        .into_iter()
        .map(|k| k.trim().to_string())
        .filter(|k| k.len() >= 8)
        .collect();
    if filtered.is_empty() {
        None
    } else {
        Some(filtered)
    }
}

/// All stored keys for a free-catalog upstream, including single-key / OAuth
/// credentials (e.g. github-copilot), deduplicated, with OpenCode Zen sharing
/// the OpenCode Go slots.
///
/// Display-oriented: seeds the Connect Free dialog's per-key health dots.
/// Not ring-aligned — the health poller must keep using
/// [`resolve_free_upstream_keys`] so its probe indices match the rings.
pub fn all_stored_free_upstream_keys(
    auth_store: &clawde_core::AuthStore,
    upstream_id: &str,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |key: String| {
        if !out.contains(&key) {
            out.push(key);
        }
    };
    let slots: Vec<&str> = if upstream_id == "opencode-zen" {
        vec!["opencode-zen", "opencode-go"]
    } else {
        vec![upstream_id]
    };
    for slot in slots {
        if let Some(key) = auth_store.api_key_for(slot) {
            push(key);
        }
        if let Some(keys) = auth_store.keys_for(slot) {
            for key in keys {
                push(key.clone());
            }
        }
    }
    out
}

/// First usable key for a free-catalog upstream's single-key chain entry.
///
/// Ring-consistent first: if the multi-key store holds any valid slot after
/// trimming and the 8-char placeholder guard, the first one wins — so a
/// placeholder sitting in slot 0 does not shadow a valid key in slot 1, since
/// the ring resolver would have used it and the poller probes those exact
/// slots. Only when there are no usable rotation keys does it fall back to
/// the stored credential (incl. OAuth, e.g. github-copilot) or the provider's
/// env var.
///
/// OpenCode Zen shares the OpenCode Go slots. `build_free_provider` uses
/// this for the non-rotating single-key path.
pub fn first_free_upstream_key(
    auth_store: &clawde_core::AuthStore,
    upstream_id: &str,
) -> Option<String> {
    if let Some(first) =
        resolve_free_upstream_keys(auth_store, upstream_id).and_then(|keys| keys.first().cloned())
    {
        return Some(first);
    }
    let key = if upstream_id == "opencode-zen" {
        auth_store
            .api_key_for("opencode-zen")
            .or_else(|| auth_store.api_key_for("opencode-go"))
    } else {
        auth_store.api_key_for(upstream_id)
    }?;
    let key = key.trim().to_string();
    (key.len() >= 8).then_some(key)
}

/// Query rate-limit information for a given upstream by making a lightweight
/// GET request to the provider's models endpoint and parsing response headers.
///
/// Uses GET (not HEAD): several upstreams (nvidia, huggingface, cline) reject
/// HEAD with 405. For upstreams whose models endpoint doesn't check auth
/// (nvidia, huggingface, openrouter, sambanova, cline), the key is confirmed
/// with the same minimal `chat/completions` probe as [`validate_upstream_key`]
/// so a dead key is reported as invalid instead of returning empty headers —
/// and the rate-limit headers are read from the **chat response**, since those
/// upstreams expose no rate-limit headers on the models endpoint.
pub fn query_rate_limits(upstream_id: &str, key: &str) -> Result<RateLimitInfo, String> {
    if key.trim().len() < 8 {
        return Err("Key too short (min 8 characters)".to_string());
    }

    // Cloudflare's /ai/v1/models endpoint does not support GET (405), and the
    // account-scoped URL is derived from the composite ACCOUNT_ID:API_TOKEN
    // key — probe the chat endpoint directly and read its headers.
    if upstream_id == "cloudflare" {
        let response = probe_cloudflare_chat(key)?;
        let status = response.status().as_u16();
        if status == 401 || status == 403 {
            return Err(format!("Invalid API token (HTTP {})", status));
        }
        if status == 404 {
            return Err("Invalid Cloudflare account ID (HTTP 404)".to_string());
        }
        // Note: a 429 here is returned as healthy-with-headers, not an error —
        // rate limits are a load signal, not a key-health signal (and the
        // probe already proved the key is valid by reaching this point).
        return Ok(parse_rate_limit_headers(response.headers()));
    }

    let native: &str = match upstream_id {
        "huggingface" => "https://router.huggingface.co/v1/models",
        "cerebras" => "https://api.cerebras.ai/v1/models",
        "nvidia" => "https://integrate.api.nvidia.com/v1/models",
        "google" => "https://generativelanguage.googleapis.com/v1beta/models",
        "groq" => "https://api.groq.com/openai/v1/models",
        "openrouter" => "https://openrouter.ai/api/v1/models",
        "sambanova" => "https://api.sambanova.ai/v1/models",
        "mistral" => "https://api.mistral.ai/v1/models",
        "cohere" => "https://api.cohere.com/v1/models",
        "opencode-zen" => "https://api.opencode.ai/v1/models",
        "zai" => "https://open.bigmodel.cn/api/paas/v4/models",
        "cline" => "https://api.cline.bot/api/v1/ai/cline/recommended-models",
        _ => return Err(format!("No validation endpoint for '{}'", upstream_id)),
    };
    let base_url = match free_upstream_base_url_override(upstream_id) {
        Some(override_base) => format!("{}/models", override_base.trim_end_matches('/')),
        None => native.to_string(),
    };

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    let is_google = upstream_id == "google";
    let request = if is_google {
        client.get(base_url).query(&[("key", key)])
    } else {
        client
            .get(base_url)
            .header("Authorization", format!("Bearer {}", key))
    };

    match request.send() {
        Ok(response) => {
            let status = response.status();
            if !status.is_success() {
                // Reuse the probe classifier so google's 400 ("API key not
                // valid") is reported as invalid, 429 as rate-limited, etc.
                return match classify_probe_status(upstream_id, status.as_u16()) {
                    Ok(()) => Err(format!("HTTP {} — unexpected response", status)),
                    Err(e) => Err(e),
                };
            }

            // Auth-lax upstreams: a models 2xx doesn't prove the key, and the
            // models response carries no rate-limit headers — the
            // chat/completions endpoint is where both auth and limits live.
            // Confirm the key via the chat probe and parse rate-limit headers
            // from THAT response.
            let headers = if models_endpoint_validates_auth(upstream_id) {
                response.headers().clone()
            } else {
                validate_key_via_chat(upstream_id, key, &client)?
                    .headers()
                    .clone()
            };

            Ok(parse_rate_limit_headers(&headers))
        }
        Err(e) => Err(format!("Connection failed: {}", e)),
    }
}

/// Parse rate-limit information from an HTTP response's headers.
///
/// Shared by the models-endpoint and chat-completions probe responses so
/// `/limits` and the health poller surface the same header names.
fn parse_rate_limit_headers(headers: &reqwest::header::HeaderMap) -> RateLimitInfo {
    let parse_u32 = |name: &str| -> Option<u32> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse().ok())
    };

    let parse_retry = || -> Option<u64> {
        headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse().ok())
    };

    RateLimitInfo {
        rpm_limit: parse_u32("x-ratelimit-limit-requests"),
        rpm_remaining: parse_u32("x-ratelimit-remaining-requests"),
        rpd_limit: parse_u32("x-ratelimit-limit-requests-day"),
        rpd_remaining: parse_u32("x-ratelimit-remaining-requests-day"),
        tpm_limit: parse_u32("x-ratelimit-limit-tokens"),
        tpm_remaining: parse_u32("x-ratelimit-remaining-tokens"),
        retry_after: parse_retry(),
        headers_found: headers
            .keys()
            .any(|k| k.as_str().to_lowercase().contains("ratelimit")),
    }
}

/// Upstreams whose `/v1/models` endpoint does **not** check the API key — it
/// returns 200 even for a garbage key (verified by live probing). For these, a
/// 2xx models response only proves reachability, so the key must be confirmed
/// with a minimal `chat/completions` probe, where auth is enforced.
///
/// opencode-zen is deliberately absent: its chat endpoint also ignores the
/// key, so the models 2xx is the best signal it offers.
///
/// cloudflare is auth-lax in a different sense: its models endpoint does not
/// support GET at all (405), so every probe goes through the chat endpoint
/// (see [`probe_cloudflare_chat`]).
fn models_endpoint_validates_auth(upstream_id: &str) -> bool {
    // Cline's /recommended-models endpoint DOES reject bad keys with 401,
    // so the models response alone proves key validity — no chat probe needed.
    // (Conversely, forcing a chat probe would flag keys as unhealthy during
    // Cline's upstream chat outages even though the key itself is fine.)
    !matches!(
        upstream_id,
        "nvidia" | "huggingface" | "openrouter" | "sambanova" | "cloudflare"
    )
}

/// Classify a models-endpoint HTTP status into a probe verdict.
///
/// Returns `Ok(())` for success, or a human-readable error otherwise.
/// Google reports bad keys as HTTP 400 ("API key not valid") rather than
/// 401/403, so that is mapped to the invalid-key error too.
fn classify_probe_status(upstream_id: &str, status: u16) -> Result<(), String> {
    if (200..300).contains(&status) {
        return Ok(());
    }
    if status == 401 || status == 403 || (upstream_id == "google" && status == 400) {
        return Err(format!("Invalid API key (HTTP {})", status));
    }
    if status == 429 {
        return Err("Rate limited — try again later".to_string());
    }
    Err(format!("HTTP {} — unexpected response", status))
}

/// Confirm a key with a minimal 1-token `chat/completions` request.
///
/// Used only for upstreams whose models endpoint doesn't check auth. Providers
/// validate the key *before* model validation, so 401/403 unambiguously means
/// an invalid key; any other response (200, or a model-not-found 4xx, 429)
/// means the key was accepted.
///
/// Returns the chat response on success so callers (e.g. [`query_rate_limits`])
/// can read rate-limit headers from the endpoint that actually enforces them.
/// Send a 1-token `chat/completions` probe to Cloudflare's account-scoped
/// OpenAI-compatible endpoint.
///
/// The key is the composite `ACCOUNT_ID:API_TOKEN`; the account ID is used to
/// build the URL path and only the token is sent as the Bearer credential.
/// Returns the raw response so both [`validate_upstream_key`] and
/// [`query_rate_limits`] can classify it and read headers.
fn probe_cloudflare_chat(key: &str) -> Result<reqwest::blocking::Response, String> {
    let (account_id, api_token) = split_cloudflare_key(key)?;
    let url = format!(
        "https://api.cloudflare.com/client/v4/accounts/{}/ai/v1/chat/completions",
        account_id
    );
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;
    let body = serde_json::json!({
        "model": CLOUDFLARE_PROBE_MODEL,
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 1,
    });
    match client
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_token))
        .json(&body)
        .send()
    {
        Ok(response) => Ok(response),
        Err(e) => Err(format!("Connection failed: {}", e)),
    }
}

/// Split a Cloudflare credential into `(account_id, api_token)`.
/// Reuses the same parsing as the provider factory in
/// `openai_compat_providers.rs` so both paths agree on the composite format.
fn split_cloudflare_key(key: &str) -> Result<(&str, &str), String> {
    crate::providers::openai_compat_providers::cloudflare_parts(key).ok_or_else(|| {
        "Cloudflare key must be ACCOUNT_ID:API_TOKEN (account ID, colon, API token)".to_string()
    })
}

/// Pick `(base_url, probe_model)` for the 1-token chat probe used to
/// confirm auth-lax upstream keys.
///
/// Prefers the upstream's first catalog fallback model when one exists —
/// fallbacks are the always-warm small models (e.g. nvidia's 8B). The free
/// tier's 70B workers are frequently capacity-starved ("ResourceExhausted"
/// 503s or 30s+ responses well past the 5s probe timeout), which marks
/// VALID keys unhealthy. Probing the fallback answers in <1s and proves the
/// key just as well. Upstreams without a fallback probe their default model.
/// Dev-only base-URL override for a free upstream.
///
/// Reads `CLAWDE_FREE_BASE_URL_<UPSTREAM_ID>` (upper-cased, `-` -> `_`),
/// e.g. `CLAWDE_FREE_BASE_URL_GROQ=http://127.0.0.1:9876/v1`, and lets a
/// local mock server stand in for a real upstream so the 5xx /
/// empty-completion cooldown paths are deterministically testable live.
///
/// Only OpenAI-compatible upstreams honour the override on the chat
/// dispatch path; `google`, `cohere` and `github-copilot` use native wire
/// formats and keep their real endpoints. The key-validation / probe
/// endpoints honour it where applicable (cloudflare's chat probe is
/// account-scoped and ignores it). Not for production use.
pub fn free_upstream_base_url_override(upstream_id: &str) -> Option<String> {
    let var = format!(
        "CLAWDE_FREE_BASE_URL_{}",
        upstream_id.to_uppercase().replace('-', "_")
    );
    std::env::var(&var)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn chat_probe_for(upstream_id: &str) -> Option<(String, &'static str)> {
    let (base_url, default_model) = match upstream_id {
        "nvidia" => (
            "https://integrate.api.nvidia.com/v1",
            "meta/llama-3.3-70b-instruct",
        ),
        "huggingface" => (
            "https://router.huggingface.co/v1",
            "meta-llama/Llama-3.3-70B-Instruct",
        ),
        "openrouter" => ("https://openrouter.ai/api/v1", "openrouter/free"),
        "sambanova" => ("https://api.sambanova.ai/v1", "Meta-Llama-3.3-70B-Instruct"),
        "cline" => ("https://api.cline.bot/api/v1", "deepseek/deepseek-v4-flash"),
        // Only the 5 auth-lax upstreams reach this probe — every caller gates
        // on `!models_endpoint_validates_auth`. opencode-zen is handled by its
        // models 2xx, so this arm is defensive only.
        _ => return None,
    };
    let model = catalog_entry(upstream_id)
        .and_then(|u| u.fallback_models.first())
        .copied()
        .unwrap_or(default_model);
    let base_url =
        free_upstream_base_url_override(upstream_id).unwrap_or_else(|| base_url.to_string());
    Some((base_url, model))
}

fn validate_key_via_chat(
    upstream_id: &str,
    key: &str,
    client: &reqwest::blocking::Client,
) -> Result<reqwest::blocking::Response, String> {
    let (base_url, model) = match chat_probe_for(upstream_id) {
        Some(v) => v,
        None => return Err(format!("No chat probe for '{}'", upstream_id)),
    };

    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 1,
    });

    match client
        .post(&url)
        .header("Authorization", format!("Bearer {}", key))
        .json(&body)
        .send()
    {
        Ok(response) => {
            let status = response.status().as_u16();
            if status == 401 || status == 403 {
                Err(format!("Invalid API key (HTTP {})", status))
            } else if status >= 500 {
                // Server-side outage — read the body for diagnostic clues
                // so the health probe doesn't treat a real key as healthy
                // (e.g. Cline's "empty response content" upstream failure).
                let body = response.text().unwrap_or_default();
                let detail = if body.contains("empty response content") {
                    "upstream provider returned empty response"
                } else if !body.is_empty() {
                    &body[..body.len().min(120)]
                } else {
                    "—"
                };
                Err(format!("Server error (HTTP {}): {}", status, detail))
            } else {
                Ok(response)
            }
        }
        Err(e) => Err(format!("Connection failed: {}", e)),
    }
}

/// Validate an API key for a given upstream by making a lightweight request
/// to the provider's models endpoint. Returns `Ok(())` if the key is valid.
///
/// For upstreams whose models endpoint doesn't check auth (nvidia,
/// huggingface, openrouter, sambanova, cline — it returns 200 even for a
/// garbage key), a 2xx response is confirmed with a minimal 1-token
/// `chat/completions` probe so dead keys are actually caught.
pub fn validate_upstream_key(upstream_id: &str, key: &str) -> Result<(), String> {
    if key.trim().len() < 8 {
        return Err("Key too short (min 8 characters)".to_string());
    }

    // Cloudflare's models endpoint does not support GET, so auth is proven
    // with the chat probe directly (account-scoped URL from the composite key).
    if upstream_id == "cloudflare" {
        let response = probe_cloudflare_chat(key)?;
        let status = response.status().as_u16();
        if status == 401 || status == 403 {
            return Err(format!("Invalid API token (HTTP {})", status));
        }
        if status == 404 {
            return Err("Invalid Cloudflare account ID (HTTP 404)".to_string());
        }
        if status == 429 {
            return Err("Rate limited — try again later".to_string());
        }
        return Ok(());
    }

    let native: &str = match upstream_id {
        "huggingface" => "https://router.huggingface.co/v1/models",
        "cerebras" => "https://api.cerebras.ai/v1/models",
        "nvidia" => "https://integrate.api.nvidia.com/v1/models",
        "google" => "https://generativelanguage.googleapis.com/v1beta/models",
        "groq" => "https://api.groq.com/openai/v1/models",
        "openrouter" => "https://openrouter.ai/api/v1/models",
        "sambanova" => "https://api.sambanova.ai/v1/models",
        "mistral" => "https://api.mistral.ai/v1/models",
        "cohere" => "https://api.cohere.com/v1/models",
        "opencode-zen" => "https://api.opencode.ai/v1/models",
        "zai" => "https://open.bigmodel.cn/api/paas/v4/models",
        "cline" => "https://api.cline.bot/api/v1/ai/cline/recommended-models",
        "github-copilot" => "https://api.githubcopilot.com/models",
        _ => return Err(format!("No validation endpoint for '{}'", upstream_id)),
    };
    let base_url = match free_upstream_base_url_override(upstream_id) {
        Some(override_base) => format!("{}/models", override_base.trim_end_matches('/')),
        None => native.to_string(),
    };

    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => return Err(format!("Failed to create HTTP client: {}", e)),
    };

    let is_google = upstream_id == "google";
    let request = if is_google {
        client.get(base_url).query(&[("key", key)])
    } else {
        client
            .get(base_url)
            .header("Authorization", format!("Bearer {}", key))
    };

    match request.send() {
        Ok(response) => {
            let status = response.status();
            if status.is_success() {
                if models_endpoint_validates_auth(upstream_id) {
                    return Ok(());
                }
                // Auth-lax models endpoint: confirm the key via chat/completions.
                return validate_key_via_chat(upstream_id, key, &client).map(|_| ());
            }
            classify_probe_status(upstream_id, status.as_u16())
        }
        Err(e) => Err(format!("Connection failed: {}", e)),
    }
}

/// Composite provider that stacks free-tier upstreams behind a single
/// `free/auto` model id.
pub struct FreeProvider {
    id: ProviderId,
    chain: Vec<FreeEntry>,
    routing: RoutingConfig,
    /// Circuit-breaker state (per-upstream cooldown).
    cooldown: Arc<Mutex<CooldownState>>,
    /// Latency tracking state (per-upstream sliding window).
    latencies: Arc<Mutex<LatencyState>>,
}

#[derive(Debug)]
enum Route {
    /// Try every entry in order, substituting its `default_model`.
    Auto,
    /// Try the entry at `start_idx` first (with `pinned_model`), then fall
    /// through to the remaining entries in catalog order.
    Pinned {
        start_idx: usize,
        pinned_model: String,
    },
    /// Model-first routing: try every chain entry whose upstream hosts the
    /// given `model_family` (in catalog order, each with its own default
    /// model), then fall through to the remaining entries. This is the
    /// `free/family/<slug>` selection from the model-first picker view —
    /// e.g. `free/family/llama-3.3-70b` round-robins across Hugging Face,
    /// NVIDIA and SambaNova before trying other model families.
    Family { model_family: &'static str },
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
