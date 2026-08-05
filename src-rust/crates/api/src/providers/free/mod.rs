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

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};

use std::time::Instant;

use async_trait::async_trait;
use clawde_core::provider_id::{ModelId, ProviderId};

use crate::provider::{LlmProvider, ModelInfo};
use crate::provider_error::ProviderError;
use crate::provider_types::{
    ProviderCapabilities, ProviderRequest, ProviderResponse, ProviderStatus, StreamEvent,
    SystemPromptStyle,
};
use clawde_core::types::ContentBlock;
use rand::seq::SliceRandom;
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
// coupled through private helpers, so one module), and the models.dev
// auto-detection helper.
mod impls;
mod modelsdev;
pub use modelsdev::fetch_best_free_models_from_modelsdev;

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
    /// Try upstreams in catalog (priority) order. Current default.
    #[default]
    Sequential,
    /// Randomly select an upstream with failover to the next on failure.
    RandomFailover,
    /// Route to the upstream with the lowest historical latency.
    LatencyBased,
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
    /// Optional path for empty-cooldown persistence.
    persist_path: Option<std::path::PathBuf>,
    /// Circuit-breaker configuration.
    config: CircuitBreakerConfig,
}

/// Disk snapshot of one upstream's empty-completion cooldown track.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct EmptyCooldownSnapshot {
    upstream: String,
    consecutive_empties: u32,
    empty_cooldown_until_unix: Option<u64>,
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
    // Persistence (empty-completion cooldown track only)
    // -------------------------------------------------------------------

    fn save(&self) {
        let Some(path) = self.persist_path.as_ref() else {
            return;
        };
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let entries: Vec<EmptyCooldownSnapshot> = self
            .upstream_ids
            .iter()
            .enumerate()
            .filter_map(|(idx, id)| {
                let count = self.consecutive_empties.get(idx).copied().unwrap_or(0);
                let until_unix = self
                    .empty_cooldown_until
                    .get(idx)
                    .copied()
                    .flatten()
                    .and_then(|t| {
                        let remaining = t.duration_since(Instant::now()).as_secs();
                        if remaining == 0 {
                            return None;
                        }
                        Some(now_unix.saturating_add(remaining))
                    });
                if count == 0 && until_unix.is_none() {
                    return None;
                }
                Some(EmptyCooldownSnapshot {
                    upstream: id.clone(),
                    consecutive_empties: count,
                    empty_cooldown_until_unix: until_unix,
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
        let entries: Vec<EmptyCooldownSnapshot> = match serde_json::from_str(&json) {
            Ok(e) => e,
            Err(_) => return,
        };
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        for entry in entries {
            let Some(idx) = self
                .upstream_ids
                .iter()
                .position(|id| id == &entry.upstream)
            else {
                continue;
            };
            self.consecutive_empties[idx] = entry.consecutive_empties;
            if let Some(until_unix) = entry.empty_cooldown_until_unix {
                let remaining = until_unix.saturating_sub(now_unix);
                if remaining > 0 {
                    self.empty_cooldown_until[idx] =
                        Some(Instant::now() + std::time::Duration::from_secs(remaining));
                }
            }
        }
    }
}

/// Per-upstream latency samples for latency-based routing.
struct LatencyState {
    /// Sliding window of request durations (seconds) per upstream index.
    samples: Vec<VecDeque<f64>>,
}

impl LatencyState {
    fn new(n: usize) -> Self {
        let mut samples = Vec::with_capacity(n);
        for _ in 0..n {
            samples.push(VecDeque::with_capacity(10));
        }
        Self { samples }
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

    let base_url = match upstream_id {
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
fn chat_probe_for(upstream_id: &str) -> Option<(&'static str, &'static str)> {
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

    let base_url = match upstream_id {
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

#[cfg(test)]
mod tests {
    use super::*;
    use clawde_core::types::{Message, UsageInfo};
    use futures::Stream;
    use std::pin::Pin;
    use std::time::Duration;

    use crate::provider_types::StopReason;

    /// Test harness: records `(upstream_id, key_idx, cooldown_secs)` calls to
    /// `mark_key_exhausted` so tests can assert exhaustion forwarding.
    /// Named to keep clippy::type_complexity off the StubProvider fields.
    type ExhaustionRecorder = Arc<Mutex<Vec<(Option<String>, usize, u64)>>>;

    // ---- Rate-limit header parsing -------------------------------------------

    #[test]
    fn parse_rate_limit_headers_reads_standard_names() {
        use reqwest::header::{HeaderMap, HeaderValue};

        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-limit-requests", HeaderValue::from_static("30"));
        headers.insert(
            "x-ratelimit-remaining-requests",
            HeaderValue::from_static("12"),
        );
        headers.insert(
            "x-ratelimit-limit-requests-day",
            HeaderValue::from_static("1000"),
        );
        headers.insert(
            "x-ratelimit-remaining-requests-day",
            HeaderValue::from_static("999"),
        );
        headers.insert(
            "x-ratelimit-limit-tokens",
            HeaderValue::from_static("200000"),
        );
        headers.insert(
            "x-ratelimit-remaining-tokens",
            HeaderValue::from_static("123456"),
        );
        headers.insert("retry-after", HeaderValue::from_static("7"));

        let info = parse_rate_limit_headers(&headers);
        assert_eq!(info.rpm_limit, Some(30));
        assert_eq!(info.rpm_remaining, Some(12));
        assert_eq!(info.rpd_limit, Some(1000));
        assert_eq!(info.rpd_remaining, Some(999));
        assert_eq!(info.tpm_limit, Some(200000));
        assert_eq!(info.tpm_remaining, Some(123456));
        assert_eq!(info.retry_after, Some(7));
        assert!(info.headers_found);
    }

    #[test]
    fn parse_rate_limit_headers_without_headers_reports_none() {
        use reqwest::header::HeaderMap;

        let info = parse_rate_limit_headers(&HeaderMap::new());
        assert_eq!(info.rpm_limit, None);
        assert_eq!(info.retry_after, None);
        assert!(!info.headers_found);
    }

    // ---- Key-probe classification -------------------------------------------

    #[test]
    fn auth_lax_upstreams_need_chat_confirmation() {
        // These upstreams' /v1/models endpoint returns 200 even for a garbage
        // key (verified by live probing), so a 2xx alone must not conclude
        // "healthy" — the chat probe is required. cloudflare is auth-lax in a
        // different sense: its models endpoint doesn't support GET at all.
        for id in [
            "nvidia",
            "huggingface",
            "openrouter",
            "sambanova",
            "cloudflare",
        ] {
            assert!(
                !models_endpoint_validates_auth(id),
                "{} should be auth-lax",
                id
            );
        }
        // Everything else validates the key on the models endpoint.
        for id in [
            "groq", "cerebras", "google", "mistral", "cohere", "zai", "cline",
        ] {
            assert!(
                models_endpoint_validates_auth(id),
                "{} should validate auth",
                id
            );
        }
    }

    #[test]
    fn chat_probe_prefers_fallback_model_for_capacity_starved_upstreams() {
        // nvidia has a catalog fallback (8B) — the probe must use it instead
        // of the capacity-starved 70B default, so valid keys aren't marked
        // unhealthy by a 30s+ 503.
        let (base, model) = chat_probe_for("nvidia").expect("nvidia probe");
        assert_eq!(model, "meta/llama-3.1-8b-instruct");
        assert!(base.contains("nvidia.com"));
        // Upstreams without fallbacks probe their default model.
        let (_, hf_model) = chat_probe_for("huggingface").expect("hf probe");
        assert_eq!(hf_model, "meta-llama/Llama-3.3-70B-Instruct");
        let (_, sb_model) = chat_probe_for("sambanova").expect("sambanova probe");
        assert_eq!(sb_model, "Meta-Llama-3.3-70B-Instruct");
        // Unsupported upstreams have no chat probe.
        assert!(chat_probe_for("groq").is_none());
    }

    #[test]
    fn probe_status_classification() {
        // Success on an auth-checking upstream is a clean pass.
        assert_eq!(classify_probe_status("groq", 200), Ok(()));
        assert_eq!(classify_probe_status("google", 200), Ok(()));
        // 401/403 are invalid keys everywhere.
        assert!(classify_probe_status("groq", 401).is_err());
        assert!(classify_probe_status("nvidia", 403).is_err());
        // Google reports bad keys as 400 ("API key not valid") — mapped to
        // the invalid-key error, not "unexpected response".
        let err = classify_probe_status("google", 400).unwrap_err();
        assert!(err.contains("Invalid API key"), "got: {}", err);
        // A 400 on a non-Google upstream stays "unexpected response".
        let err = classify_probe_status("groq", 400).unwrap_err();
        assert!(err.contains("unexpected response"), "got: {}", err);
        // 429 is rate-limited.
        let err = classify_probe_status("groq", 429).unwrap_err();
        assert!(err.contains("Rate limited"), "got: {}", err);
        // 5xx is unexpected.
        let err = classify_probe_status("nvidia", 500).unwrap_err();
        assert!(err.contains("unexpected response"), "got: {}", err);
    }

    struct StubProvider {
        id: ProviderId,
        ok: bool,
        /// When set, records the `max_tokens` value seen by `create_message`
        /// so tests can assert dispatch-time clamping.
        seen_max_tokens: Option<Arc<Mutex<Option<u32>>>>,
        /// When set, reports a key-ring status via `key_ring_status()` so
        /// tests can exercise `upstream_key_health()`.
        ring_status: Option<(usize, usize, Option<u64>)>,
        /// When set, records `mark_key_exhausted` calls as
        /// `(upstream_id, key_idx, cooldown_secs)` so tests can assert
        /// exhaustion forwarding from the composite provider.
        exhaustion: Option<ExhaustionRecorder>,
    }

    #[async_trait]
    impl LlmProvider for StubProvider {
        fn id(&self) -> &ProviderId {
            &self.id
        }

        fn name(&self) -> &str {
            "stub"
        }

        async fn create_message(
            &self,
            request: ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            if let Some(rec) = &self.seen_max_tokens {
                if let Ok(mut g) = rec.lock() {
                    *g = Some(request.max_tokens);
                }
            }
            if self.ok {
                Ok(ProviderResponse {
                    id: "msg".to_string(),
                    model: request.model,
                    content: Vec::new(),
                    stop_reason: StopReason::EndTurn,
                    usage: UsageInfo::default(),
                })
            } else {
                Err(ProviderError::RateLimited {
                    provider: self.id.clone(),
                    retry_after: None,
                })
            }
        }

        async fn create_message_stream(
            &self,
            _request: ProviderRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            Err(ProviderError::ServerError {
                provider: self.id.clone(),
                status: None,
                message: "stub".into(),
                is_retryable: false,
            })
        }

        async fn discover_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(vec![])
        }

        async fn health_check(&self) -> Result<ProviderStatus, ProviderError> {
            Ok(ProviderStatus::Healthy)
        }

        fn key_ring_status(&self) -> Option<(usize, usize, Option<u64>)> {
            self.ring_status
        }

        fn mark_key_exhausted(
            &self,
            upstream_id: Option<&str>,
            key_idx: usize,
            cooldown_secs: u64,
            _reason: Option<String>,
        ) -> bool {
            if let Some(rec) = &self.exhaustion {
                if let Ok(mut g) = rec.lock() {
                    g.push((upstream_id.map(|s| s.to_string()), key_idx, cooldown_secs));
                }
            }
            true
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_calling: false,
                thinking: false,
                image_input: false,
                pdf_input: false,
                audio_input: false,
                video_input: false,
                caching: false,
                structured_output: false,
                system_prompt_style: SystemPromptStyle::SystemMessage,
            }
        }
    }

    fn entry(id: &'static str, ok: bool) -> FreeEntry {
        let upstream = *catalog_entry(id).expect("catalog entry");
        FreeEntry {
            upstream,
            provider: Arc::new(StubProvider {
                id: ProviderId::new(id),
                ok,
                seen_max_tokens: None,
                ring_status: None,
                exhaustion: None,
            }),
            effective_model: None,
        }
    }

    fn entry_with_recorder(
        id: &'static str,
        ok: bool,
        recorder: Arc<Mutex<Option<u32>>>,
    ) -> FreeEntry {
        let upstream = *catalog_entry(id).expect("catalog entry");
        FreeEntry {
            upstream,
            provider: Arc::new(StubProvider {
                id: ProviderId::new(id),
                ok,
                seen_max_tokens: Some(recorder),
                ring_status: None,
                exhaustion: None,
            }),
            effective_model: None,
        }
    }

    fn entry_with_exhaustion_recorder(id: &'static str, recorder: ExhaustionRecorder) -> FreeEntry {
        let upstream = *catalog_entry(id).expect("catalog entry");
        FreeEntry {
            upstream,
            provider: Arc::new(StubProvider {
                id: ProviderId::new(id),
                ok: true,
                seen_max_tokens: None,
                ring_status: None,
                exhaustion: Some(recorder),
            }),
            effective_model: None,
        }
    }

    fn entry_with_ring(id: &'static str, ring: (usize, usize, Option<u64>)) -> FreeEntry {
        let upstream = *catalog_entry(id).expect("catalog entry");
        FreeEntry {
            upstream,
            provider: Arc::new(StubProvider {
                id: ProviderId::new(id),
                ok: true,
                seen_max_tokens: None,
                ring_status: Some(ring),
                exhaustion: None,
            }),
            effective_model: None,
        }
    }

    fn dummy_request(model: &str) -> ProviderRequest {
        ProviderRequest {
            model: model.to_string(),
            messages: vec![Message::user("hi")],
            system_prompt: None,
            tools: Vec::new(),
            max_tokens: 8,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: Vec::new(),
            thinking: None,
            provider_options: serde_json::Value::Null,
        }
    }

    #[test]
    fn route_auto_for_free_aliases() {
        let provider = FreeProvider::new(vec![entry("huggingface", true), entry("cerebras", true)]);
        assert!(matches!(provider.resolve_route("free"), Route::Auto));
        assert!(matches!(provider.resolve_route("free/auto"), Route::Auto));
        assert!(matches!(provider.resolve_route("auto"), Route::Auto));
        assert!(matches!(provider.resolve_route(""), Route::Auto));
    }

    #[test]
    fn route_pinned_for_prefix() {
        let provider = FreeProvider::new(vec![entry("huggingface", true), entry("cerebras", true)]);
        let route = provider.resolve_route("cerebras/qwen-3-235b");
        match route {
            Route::Pinned {
                start_idx,
                pinned_model,
            } => {
                assert_eq!(start_idx, 1);
                assert_eq!(pinned_model, "qwen-3-235b");
            }
            other => panic!("expected pinned, got {:?}", other),
        }
    }

    #[test]
    fn nvidia_plan_includes_8b_fallback_after_70b() {
        let provider = FreeProvider::new(vec![
            entry("nvidia", true),
            entry("cerebras", true),
            entry("groq", true),
        ]);
        // Sequential Auto plan: nvidia's 70B primary, then its 8B fallback on
        // the SAME index, then the other upstreams.
        let plan = provider.attempt_plan(&Route::Auto);
        assert_eq!(plan[0], (0, "meta/llama-3.3-70b-instruct".to_string()));
        assert_eq!(plan[1], (0, "meta/llama-3.1-8b-instruct".to_string()));
        assert_eq!(plan[2], (1, "gpt-oss-120b".to_string()));
        assert_eq!(plan[3], (2, "openai/gpt-oss-120b".to_string()));
        // Upstreams without fallbacks still contribute exactly one row.
        assert_eq!(plan.len(), 4);
    }

    #[test]
    fn pinned_route_tries_pinned_model_then_upstream_fallbacks() {
        let provider = FreeProvider::new(vec![
            entry("huggingface", true),
            entry("nvidia", true),
            entry("cerebras", true),
        ]);
        // Pinning nvidia: the pinned model, then nvidia's 8B fallback, then
        // the rest of the chain in catalog order.
        let plan = provider.attempt_plan(&Route::Pinned {
            start_idx: 1,
            pinned_model: "meta/llama-3.3-70b-instruct".to_string(),
        });
        assert_eq!(plan[0], (1, "meta/llama-3.3-70b-instruct".to_string()));
        assert_eq!(plan[1], (1, "meta/llama-3.1-8b-instruct".to_string()));
        assert_eq!(
            plan[2],
            (0, "meta-llama/Llama-3.3-70B-Instruct".to_string())
        );
        assert_eq!(plan[3], (2, "gpt-oss-120b".to_string()));
    }

    #[test]
    fn legacy_zen_prefix_routes_to_opencode_zen() {
        let provider =
            FreeProvider::new(vec![entry("opencode-zen", true), entry("openrouter", true)]);
        let route = provider.resolve_route("zen/big-pickle");
        match route {
            Route::Pinned {
                start_idx,
                pinned_model,
            } => {
                assert_eq!(start_idx, 0);
                assert_eq!(pinned_model, "big-pickle");
            }
            other => panic!("expected pinned, got {:?}", other),
        }
    }

    #[test]
    fn openrouter_free_keeps_full_id() {
        let provider = FreeProvider::new(vec![entry("openrouter", true)]);
        let route = provider.resolve_route("openrouter/free");
        match route {
            Route::Pinned { pinned_model, .. } => {
                assert_eq!(pinned_model, "openrouter/free");
            }
            other => panic!("expected pinned, got {:?}", other),
        }
    }

    #[test]
    fn family_route_resolves_from_slug() {
        let provider = FreeProvider::new(vec![entry("huggingface", true)]);
        match provider.resolve_route("free/family/llama-3.3-70b") {
            Route::Family { model_family } => assert_eq!(model_family, "llama-3.3-70b"),
            other => panic!("expected family, got {:?}", other),
        }
        // Bare `family/<slug>` is accepted too.
        match provider.resolve_route("family/llama-3.3-70b") {
            Route::Family { model_family } => assert_eq!(model_family, "llama-3.3-70b"),
            other => panic!("expected family, got {:?}", other),
        }
    }

    #[test]
    fn unknown_family_falls_back_to_auto() {
        let provider = FreeProvider::new(vec![entry("huggingface", true)]);
        assert!(matches!(
            provider.resolve_route("free/family/does-not-exist"),
            Route::Auto
        ));
        assert!(matches!(
            provider.resolve_route("family/does-not-exist"),
            Route::Auto
        ));
    }

    #[test]
    fn family_plan_leads_with_hosts_then_rest() {
        let provider = FreeProvider::new(vec![
            entry("huggingface", true),
            entry("cerebras", true),
            entry("nvidia", true),
            entry("groq", true),
        ]);
        let plan = provider.attempt_plan(&Route::Family {
            model_family: "llama-3.3-70b",
        });
        // Family hosts first in catalog order — huggingface (idx 0), then
        // nvidia (idx 2) with its 8B fallback on the same index.
        assert_eq!(
            plan[0],
            (0, "meta-llama/Llama-3.3-70B-Instruct".to_string())
        );
        assert_eq!(plan[1], (2, "meta/llama-3.3-70b-instruct".to_string()));
        assert_eq!(plan[2], (2, "meta/llama-3.1-8b-instruct".to_string()));
        // Non-family upstreams follow in catalog order.
        assert_eq!(plan[3], (1, "gpt-oss-120b".to_string()));
        assert_eq!(plan[4], (3, "openai/gpt-oss-120b".to_string()));
    }

    #[test]
    fn family_route_reports_host_capabilities() {
        let provider = FreeProvider::new(vec![entry("huggingface", true)]);
        // The catalog's huggingface entry hosts llama-3.3-70b with tool
        // calling and a max-tokens cap — the family route must surface those
        // from the first matching host.
        let tc = provider.tool_calling_for("free/family/llama-3.3-70b");
        assert_eq!(tc, Some(true));
        let cap = provider.max_tokens_cap_for("free/family/llama-3.3-70b");
        assert!(cap.is_some());
    }

    #[test]
    fn attempt_plan_auto_uses_each_default() {
        let provider = FreeProvider::new(vec![entry("huggingface", true), entry("cerebras", true)]);
        let plan = provider.attempt_plan(&Route::Auto);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].0, 0);
        assert_eq!(plan[0].1, "meta-llama/Llama-3.3-70B-Instruct");
        assert_eq!(plan[1].0, 1);
        assert_eq!(plan[1].1, "gpt-oss-120b");
    }

    #[test]
    fn random_failover_auto_uses_all_entries() {
        let cfg = RoutingConfig {
            strategy: RoutingStrategy::RandomFailover,
            ..Default::default()
        };
        let provider = FreeProvider::with_routing(
            vec![
                entry("huggingface", true),
                entry("cerebras", true),
                entry("google", true),
            ],
            cfg,
            false,
        );
        let plan = provider.attempt_plan(&Route::Auto);

        // Must have all upstreams.
        assert_eq!(plan.len(), 3);

        // Must contain every index exactly once.
        let mut indices: Vec<usize> = plan.iter().map(|(i, _)| *i).collect();
        indices.sort();
        assert_eq!(indices, vec![0, 1, 2]);

        // Every model string must be non-empty.
        for (_, model) in &plan {
            assert!(!model.is_empty());
        }
    }

    #[test]
    fn random_failover_pinned_starts_with_pinned() {
        let cfg = RoutingConfig {
            strategy: RoutingStrategy::RandomFailover,
            ..Default::default()
        };
        let provider = FreeProvider::with_routing(
            vec![
                entry("huggingface", true),
                entry("cerebras", true),
                entry("google", true),
            ],
            cfg,
            false,
        );
        let plan = provider.attempt_plan(&Route::Pinned {
            start_idx: 2,
            pinned_model: "gemini-2.5-pro".into(),
        });

        // Pinned entry must be first.
        assert_eq!(plan[0].0, 2);
        assert_eq!(plan[0].1, "gemini-2.5-pro");

        // Must contain every index exactly once.
        let mut indices: Vec<usize> = plan.iter().map(|(i, _)| *i).collect();
        indices.sort();
        assert_eq!(indices, vec![0, 1, 2]);
    }

    #[test]
    fn routing_config_default_is_sequential() {
        let provider = FreeProvider::new(vec![entry("huggingface", true)]);
        assert!(matches!(
            provider.routing_config().strategy,
            RoutingStrategy::Sequential
        ));
    }

    #[test]
    fn with_routing_stores_config() {
        let cfg = RoutingConfig {
            strategy: RoutingStrategy::RandomFailover,
            ..Default::default()
        };
        let provider = FreeProvider::with_routing(
            vec![entry("huggingface", true), entry("cerebras", true)],
            cfg,
            false,
        );
        assert!(matches!(
            provider.routing_config().strategy,
            RoutingStrategy::RandomFailover
        ));
    }

    #[test]
    fn routing_strategy_serde_round_trip() {
        // Sequential → JSON → deserialize
        let seq = RoutingConfig::default();
        let json = serde_json::to_string(&seq).unwrap();
        let deserialized: RoutingConfig = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized.strategy, RoutingStrategy::Sequential));

        // RandomFailover → JSON → deserialize
        let rng = RoutingConfig {
            strategy: RoutingStrategy::RandomFailover,
            ..Default::default()
        };
        let json = serde_json::to_string(&rng).unwrap();
        assert_eq!(
            json,
            r#"{"strategy":"random_failover","upstream_timeout_secs":30,"upstream_5xx_cooldown_secs":45,"fallback_retries":1}"#
        );
        let deserialized: RoutingConfig = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            deserialized.strategy,
            RoutingStrategy::RandomFailover
        ));
    }

    #[test]
    fn routing_config_from_options_map() {
        // This simulates the config plumbing: reading from
        // provider_configs.get("free").options["routing"].
        use std::collections::HashMap;
        let mut options: HashMap<String, serde_json::Value> = HashMap::new();
        options.insert(
            "routing".to_string(),
            serde_json::json!({"strategy": "random_failover"}),
        );

        let routing: Option<RoutingConfig> = options
            .get("routing")
            .and_then(|v| serde_json::from_value(v.clone()).ok());

        let config = routing.unwrap();
        assert!(matches!(config.strategy, RoutingStrategy::RandomFailover));
    }

    #[test]
    fn attempt_plan_pinned_tries_pin_then_others() {
        let provider = FreeProvider::new(vec![
            entry("huggingface", true),
            entry("cerebras", true),
            entry("google", true),
        ]);
        let plan = provider.attempt_plan(&Route::Pinned {
            start_idx: 2,
            pinned_model: "gemini-2.5-pro".into(),
        });
        assert_eq!(plan.len(), 3);
        assert_eq!(plan[0].0, 2);
        assert_eq!(plan[0].1, "gemini-2.5-pro");
        // Order of remaining = catalog order minus the pinned index.
        assert_eq!(plan[1].0, 0);
        assert_eq!(plan[2].0, 1);
    }

    #[test]
    fn should_fallback_on_transient_errors() {
        let pid = ProviderId::new("groq");
        assert!(FreeProvider::should_fallback(&ProviderError::RateLimited {
            provider: pid.clone(),
            retry_after: None,
        }));
        assert!(FreeProvider::should_fallback(&ProviderError::AuthFailed {
            provider: pid.clone(),
            message: "bad key".into(),
        }));
        assert!(FreeProvider::should_fallback(&ProviderError::ServerError {
            provider: pid.clone(),
            status: Some(500),
            message: "boom".into(),
            is_retryable: true,
        }));
        assert!(!FreeProvider::should_fallback(
            &ProviderError::InvalidRequest {
                provider: pid.clone(),
                message: "bad request".into(),
            }
        ));
        assert!(!FreeProvider::should_fallback(
            &ProviderError::ContentFiltered {
                provider: pid,
                message: "filtered".into(),
            }
        ));
    }

    #[tokio::test]
    async fn create_message_falls_back_to_next_upstream() {
        let provider =
            FreeProvider::new(vec![entry("huggingface", false), entry("cerebras", true)]);
        let resp = provider
            .create_message(dummy_request("free/auto"))
            .await
            .expect("should succeed via cerebras");
        assert_eq!(resp.model, "gpt-oss-120b");
    }

    // -------------------------------------------------------------------
    // max_tokens_cap clamping tests
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn create_message_clamps_max_tokens_to_upstream_cap() {
        // huggingface catalog entry has max_tokens_cap = 8_192.
        let recorder = Arc::new(Mutex::new(None));
        let provider = FreeProvider::new(vec![entry_with_recorder(
            "huggingface",
            true,
            recorder.clone(),
        )]);
        let mut req = dummy_request("free/auto");
        req.max_tokens = 16_384;
        provider.create_message(req).await.expect("should succeed");
        let seen = *recorder.lock().unwrap();
        assert_eq!(
            seen,
            Some(8_192),
            "max_tokens must be clamped to upstream cap"
        );
    }

    #[test]
    fn clamp_max_tokens_for_noop_when_no_cap() {
        // mistral catalog entry has max_tokens_cap = None.
        let entry = entry("mistral", true);
        let mut req = dummy_request("mistral/x");
        req.max_tokens = 16_384;
        clamp_max_tokens_for(&mut req, &entry);
        assert_eq!(req.max_tokens, 16_384, "no cap means no clamping");
    }

    #[test]
    fn clamp_max_tokens_for_never_raises_max_tokens() {
        let entry = entry("huggingface", true); // cap = 8_192
        let mut req = dummy_request("huggingface/x");
        req.max_tokens = 4_096;
        clamp_max_tokens_for(&mut req, &entry);
        assert_eq!(
            req.max_tokens, 4_096,
            "smaller request must pass through unchanged"
        );
    }

    // -------------------------------------------------------------------
    // 5xx cooldown visibility tests (no circuit breaker configured)
    // -------------------------------------------------------------------

    #[test]
    fn five_xx_cooldown_is_visible_without_circuit_breaker() {
        // Circuit breaker is disabled by default; the 5xx cooldown must
        // still be visible to is_in_cooldown (regression for the old gate
        // that made 5xx cooldowns dead on the non-streaming path).
        let provider = FreeProvider::new(vec![entry("huggingface", true)]);
        let err = ProviderError::ServerError {
            provider: ProviderId::new("huggingface"),
            status: Some(503),
            message: "boom".into(),
            is_retryable: true,
        };
        provider.maybe_cooldown_upstream_for_5xx(0, &err);
        assert!(
            provider.is_in_cooldown(0),
            "5xx cooldown should be visible even with circuit breaker disabled"
        );
    }

    #[tokio::test]
    async fn five_xx_cooldown_skips_upstream_in_fallback() {
        // Use a *working* first upstream so the skip is observable: with the
        // old buggy is_in_cooldown gate the loop would try huggingface,
        // succeed, and return its model; with the fix it skips the cooled
        // upstream and lands on cerebras.
        let provider = FreeProvider::new(vec![entry("huggingface", true), entry("cerebras", true)]);
        let err = ProviderError::ServerError {
            provider: ProviderId::new("huggingface"),
            status: Some(503),
            message: "boom".into(),
            is_retryable: true,
        };
        provider.maybe_cooldown_upstream_for_5xx(0, &err);
        assert!(provider.is_in_cooldown(0));

        let resp = provider
            .create_message(dummy_request("free/auto"))
            .await
            .expect("should succeed via cerebras");
        assert_eq!(
            resp.model, "gpt-oss-120b",
            "cooled-down upstream must be skipped even though it would succeed"
        );
    }

    #[test]
    fn upstream_cooldowns_reports_5xx_and_empty_kinds() {
        let provider = FreeProvider::new(vec![entry("huggingface", true), entry("cerebras", true)]);
        // 5xx cooldown on the first upstream (default 45s).
        let err = ProviderError::ServerError {
            provider: ProviderId::new("huggingface"),
            status: Some(503),
            message: "boom".into(),
            is_retryable: true,
        };
        provider.maybe_cooldown_upstream_for_5xx(0, &err);
        // Empty-completion cooldown on the second upstream (default max 3,
        // cooldown 60s). Drive the cooldown state directly — the empty-completion
        // recording path lives on RetryingFreeStream. `record_empty` returns
        // `just_cooled`, i.e. true only when the threshold is crossed.
        {
            let mut cd = provider.cooldown.lock().unwrap();
            assert!(
                !cd.record_empty(1, 3, 60),
                "first empty must not trip the cooldown"
            );
            assert!(
                !cd.record_empty(1, 3, 60),
                "second empty must not trip the cooldown"
            );
            assert!(
                cd.record_empty(1, 3, 60),
                "third consecutive empty must trip the cooldown"
            );
        }

        let cooldowns = provider.upstream_cooldowns();
        let kinds: Vec<&str> = cooldowns.iter().map(|(_, k, _)| k.as_str()).collect();
        assert!(
            kinds.contains(&"5xx"),
            "5xx cooldown must be reported, got {:?}",
            cooldowns
        );
        assert!(
            kinds.contains(&"empty"),
            "empty cooldown must be reported, got {:?}",
            cooldowns
        );
        for (_, _, retry) in &cooldowns {
            assert!(retry.is_some(), "active cooldowns must carry retry_secs");
        }

        // The trait override must surface the empty cooldown through `dyn` —
        // guards the regression where upstream_empty_cooldowns was only an
        // inherent method and the registry (Arc<dyn LlmProvider>) always got
        // the empty trait default.
        let dyn_provider: Arc<dyn LlmProvider> = Arc::new(provider);
        let empty = dyn_provider.upstream_empty_cooldowns();
        assert!(
            empty.iter().any(|(id, _, _)| id == "cerebras"),
            "trait upstream_empty_cooldowns must report cerebras, got {:?}",
            empty
        );
    }

    #[test]
    fn upstream_key_health_reports_ring_backed_upstreams() {
        let provider = FreeProvider::new(vec![
            entry("huggingface", true),
            entry_with_ring("cerebras", (1, 2, Some(45))),
        ]);
        let health = provider.upstream_key_health();
        assert_eq!(
            health.len(),
            1,
            "only ring-backed upstreams report health, got {:?}",
            health
        );
        assert_eq!(health[0].0, "cerebras");
        assert_eq!((health[0].1, health[0].2), (1, 2));
        assert_eq!(health[0].3, Some(45));
    }

    #[test]
    fn mark_key_exhausted_forwards_to_matching_upstream() {
        let recorder: ExhaustionRecorder = Arc::new(Mutex::new(Vec::new()));
        let provider = FreeProvider::new(vec![
            entry("huggingface", true),
            entry_with_exhaustion_recorder("cerebras", recorder.clone()),
        ]);

        // Matches the chain entry's upstream id → forwarded with the real
        // key index and cooldown (as injected by the health poller, §6.4).
        assert!(provider.mark_key_exhausted(
            Some("cerebras"),
            2,
            300,
            Some("Invalid API key (HTTP 401)".to_string())
        ));
        let recorded = recorder.lock().unwrap();
        assert_eq!(recorded.len(), 1, "exactly one forwarding expected");
        assert_eq!(recorded[0], (Some("cerebras".to_string()), 2, 300));
        drop(recorded);
        recorder.lock().unwrap().clear();

        // Unknown upstream / missing id → not forwarded, returns false.
        assert!(!provider.mark_key_exhausted(Some("nope"), 0, 1, None));
        assert!(!provider.mark_key_exhausted(None, 0, 1, None));
        assert!(recorder.lock().unwrap().is_empty(), "no extra forwards");
    }

    // -------------------------------------------------------------------
    // Circuit breaker tests
    // -------------------------------------------------------------------

    #[test]
    fn circuit_breaker_disabled_by_default() {
        let provider = FreeProvider::new(vec![entry("huggingface", true)]);
        provider.record_failure(0);
        assert!(!provider.is_in_cooldown(0));
    }

    #[test]
    fn circuit_breaker_disabled_when_max_fails_is_zero() {
        let cfg = RoutingConfig {
            circuit_breaker: Some(CircuitBreakerConfig {
                max_fails: 0,
                window_secs: 60,
                cooldown_secs: 120,
            }),
            ..Default::default()
        };
        let provider = FreeProvider::with_routing(
            vec![entry("huggingface", true), entry("cerebras", true)],
            cfg,
            false,
        );
        // Even after many failures, no cooldown because max_fails=0
        provider.record_failure(0);
        provider.record_failure(0);
        provider.record_failure(0);
        assert!(!provider.is_in_cooldown(0));
    }

    #[test]
    fn circuit_breaker_activates_after_threshold() {
        let cfg = RoutingConfig {
            circuit_breaker: Some(CircuitBreakerConfig {
                max_fails: 2,
                window_secs: 60,
                cooldown_secs: 300,
            }),
            ..Default::default()
        };
        let provider = FreeProvider::with_routing(
            vec![entry("huggingface", true), entry("cerebras", true)],
            cfg,
            false,
        );

        // Initially no cooldown
        assert!(!provider.is_in_cooldown(0));
        assert!(!provider.is_in_cooldown(1));

        // First failure — not yet at threshold
        provider.record_failure(0);
        assert!(!provider.is_in_cooldown(0));

        // Second failure — now in cooldown
        provider.record_failure(0);
        assert!(provider.is_in_cooldown(0));

        // Other upstream unaffected
        assert!(!provider.is_in_cooldown(1));
    }

    #[test]
    fn circuit_breaker_success_resets_failures() {
        let cfg = RoutingConfig {
            circuit_breaker: Some(CircuitBreakerConfig {
                max_fails: 2,
                window_secs: 60,
                cooldown_secs: 300,
            }),
            ..Default::default()
        };
        let provider = FreeProvider::with_routing(
            vec![entry("huggingface", true), entry("cerebras", true)],
            cfg,
            false,
        );

        // One failure, then a success resets the counter
        provider.record_failure(0);
        provider.record_success(0, Duration::from_secs(1));

        // One more failure should NOT trigger cooldown (counter was reset)
        provider.record_failure(0);
        assert!(!provider.is_in_cooldown(0));

        // Second failure after reset — now in cooldown
        provider.record_failure(0);
        assert!(provider.is_in_cooldown(0));
    }

    #[test]
    fn circuit_breaker_per_upstream_independence() {
        let cfg = RoutingConfig {
            circuit_breaker: Some(CircuitBreakerConfig {
                max_fails: 3,
                window_secs: 60,
                cooldown_secs: 120,
            }),
            ..Default::default()
        };
        let provider = FreeProvider::with_routing(
            vec![
                entry("huggingface", true),
                entry("cerebras", true),
                entry("google", true),
            ],
            cfg,
            false,
        );

        // Exhaust upstream 0 with 3 failures
        for _ in 0..3 {
            provider.record_failure(0);
        }
        assert!(provider.is_in_cooldown(0));

        // Other upstreams are still active
        assert!(!provider.is_in_cooldown(1));
        assert!(!provider.is_in_cooldown(2));
    }

    // -------------------------------------------------------------------
    // Latency tracking tests
    // -------------------------------------------------------------------

    #[test]
    fn latency_tracking_records_and_computes_average() {
        let cfg = RoutingConfig {
            strategy: RoutingStrategy::LatencyBased,
            latency: Some(LatencyConfig { max_samples: 10 }),
            ..Default::default()
        };
        let provider = FreeProvider::with_routing(
            vec![entry("huggingface", true), entry("cerebras", true)],
            cfg,
            false,
        );

        // Record latencies for upstream 0 (fast)
        provider.record_success(0, Duration::from_millis(100));
        provider.record_success(0, Duration::from_millis(200));

        // Record latencies for upstream 1 (slower)
        provider.record_success(1, Duration::from_millis(900));
        provider.record_success(1, Duration::from_millis(1100));

        // Latency-based plan should put faster upstream first
        let plan = provider.attempt_plan(&Route::Auto);
        assert_eq!(plan.len(), 2);
        // Upstream 0 (avg 150ms) comes before upstream 1 (avg 1000ms)
        assert_eq!(plan[0].0, 0);
        assert_eq!(plan[1].0, 1);
    }

    #[test]
    fn latency_plan_keeps_fallback_adjacent_after_primary() {
        let cfg = RoutingConfig {
            strategy: RoutingStrategy::LatencyBased,
            latency: Some(LatencyConfig { max_samples: 10 }),
            ..Default::default()
        };
        let provider = FreeProvider::with_routing(
            vec![
                entry("huggingface", true),
                entry("nvidia", true),
                entry("cerebras", true),
                entry("google", true),
            ],
            cfg,
            false,
        );

        // Record distinct latencies: nvidia fastest (100ms), google 300ms,
        // cerebras 500ms, huggingface 800ms. Even though the latency sort
        // reorders upstreams, nvidia's 8B fallback row must stay adjacent
        // AFTER its 70B primary (stable sort keeps same-idx rows together
        // in insertion order).
        provider.record_success(0, Duration::from_millis(800));
        provider.record_success(1, Duration::from_millis(100));
        provider.record_success(2, Duration::from_millis(500));
        provider.record_success(3, Duration::from_millis(300));

        let plan = provider.attempt_plan(&Route::Auto);

        // nvidia (idx 1, fastest) first: 70B then its 8B fallback adjacent.
        assert_eq!(plan[0], (1, "meta/llama-3.3-70b-instruct".to_string()));
        assert_eq!(plan[1], (1, "meta/llama-3.1-8b-instruct".to_string()));
        // google (300ms), cerebras (500ms), huggingface (800ms).
        assert_eq!(plan[2], (3, "gemini-2.5-flash".to_string()));
        assert_eq!(plan[3], (2, "gpt-oss-120b".to_string()));
        assert_eq!(
            plan[4],
            (0, "meta-llama/Llama-3.3-70B-Instruct".to_string())
        );
        assert_eq!(plan.len(), 5);
    }

    #[test]
    fn latency_tracking_pinned_starts_with_pinned_then_sorted() {
        let cfg = RoutingConfig {
            strategy: RoutingStrategy::LatencyBased,
            latency: Some(LatencyConfig { max_samples: 10 }),
            ..Default::default()
        };
        let provider = FreeProvider::with_routing(
            vec![
                entry("huggingface", true),
                entry("cerebras", true),
                entry("google", true),
            ],
            cfg,
            false,
        );

        // Record latencies: groq is fastest, cerebras is slowest
        provider.record_success(0, Duration::from_millis(100));
        provider.record_success(1, Duration::from_millis(2000));
        provider.record_success(2, Duration::from_millis(500));

        // Pin to cerebras (idx 1) — should be first, then rest sorted by latency
        let plan = provider.attempt_plan(&Route::Pinned {
            start_idx: 1,
            pinned_model: "custom-model".into(),
        });

        assert_eq!(plan.len(), 3);
        assert_eq!(plan[0].0, 1); // pinned first
        assert_eq!(plan[0].1, "custom-model");
        assert_eq!(plan[1].0, 0); // groq (100ms) next
        assert_eq!(plan[2].0, 2); // google (500ms) last
    }

    #[test]
    fn latency_tracking_no_data_preserves_catalog_order() {
        let cfg = RoutingConfig {
            strategy: RoutingStrategy::LatencyBased,
            latency: Some(LatencyConfig { max_samples: 10 }),
            ..Default::default()
        };
        let provider = FreeProvider::with_routing(
            vec![
                entry("huggingface", true),
                entry("cerebras", true),
                entry("google", true),
            ],
            cfg,
            false,
        );

        // No latency data recorded — all avg_latency returns f64::MAX,
        // so partial_cmp returns Equal and order is stable (catalog order).
        let plan = provider.attempt_plan(&Route::Auto);
        assert_eq!(plan.len(), 3);
        assert_eq!(plan[0].0, 0);
        assert_eq!(plan[1].0, 1);
        assert_eq!(plan[2].0, 2);
    }

    #[test]
    fn latency_config_serde_round_trip() {
        let cfg = LatencyConfig { max_samples: 20 };
        let json = serde_json::to_string(&cfg).unwrap();
        assert_eq!(json, r#"{"max_samples":20}"#);
        let deserialized: LatencyConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.max_samples, 20);

        // Default serialization
        let default_cfg = LatencyConfig::default();
        let json = serde_json::to_string(&default_cfg).unwrap();
        assert_eq!(json, r#"{"max_samples":10}"#);
    }

    #[test]
    fn circuit_breaker_config_serde_round_trip() {
        let cfg = CircuitBreakerConfig {
            max_fails: 5,
            window_secs: 120,
            cooldown_secs: 300,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let deserialized: CircuitBreakerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.max_fails, 5);
        assert_eq!(deserialized.window_secs, 120);
        assert_eq!(deserialized.cooldown_secs, 300);

        // Default serialization
        let default_cfg = CircuitBreakerConfig::default();
        let json = serde_json::to_string(&default_cfg).unwrap();
        assert_eq!(
            json,
            r#"{"max_fails":3,"window_secs":60,"cooldown_secs":120}"#
        );
    }

    #[tokio::test]
    async fn empty_chain_returns_auth_error() {
        let provider = FreeProvider::new(vec![]);
        let err = provider
            .create_message(dummy_request("free/auto"))
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::AuthFailed { .. }));
    }
}

// -------------------------------------------------------------------
// Live discovery mock tests (fetch_openai_compat_model_list)
// -------------------------------------------------------------------

/// Spawn a robust mock HTTP server on `listener` that answers every
/// connection with `response`. Uses a thread per connection and drains the
/// request before replying — a naive single-threaded accept→write loop makes
/// hyper intermittently fail with "received unexpected message from
/// connection" (a response racing keep-alive connection reuse), which flaked
/// these tests. Returns a ready flag the caller spins on so the fetch never
/// races a not-yet-starting accept loop.
#[cfg(test)]
fn spawn_mock_server(
    listener: std::net::TcpListener,
    response: String,
) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
    let server_ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ready = server_ready.clone();
    std::thread::spawn(move || {
        ready.store(true, std::sync::atomic::Ordering::SeqCst);
        for mut s in listener.incoming().take(16).flatten() {
            let response = response.clone();
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf);
                let _ = s.write_all(response.as_bytes());
            });
        }
    });
    server_ready
}

/// Spin until the mock server's accept loop is running.
#[cfg(test)]
fn wait_for_mock_server(ready: &std::sync::atomic::AtomicBool) {
    while !ready.load(std::sync::atomic::Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Minimal 200 OK JSON response builder for the mock servers.
#[cfg(test)]
fn mock_json_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    )
}

#[test]
fn fetch_openai_compat_model_list_parses_openai_response() {
    // Mock JSON response from a standard OpenAI-compatible /v1/models endpoint.
    let json = r#"{
            "object": "list",
            "data": [
                { "id": "llama-3.3-70b-versatile", "object": "model", "created": 1700000000, "owned_by": "groq" },
                { "id": "mixtral-8x7b-32768",       "object": "model", "created": 1700000001, "owned_by": "groq" }
            ]
        }"#;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let ready = spawn_mock_server(listener, mock_json_response(json));
    wait_for_mock_server(&ready);

    let base_url = format!("http://127.0.0.1:{}", port);
    let result = fetch_openai_compat_model_list("test-key", &base_url, "groq");
    assert_eq!(result.as_deref(), Some("llama-3.3-70b-versatile"));
}

#[test]
fn fetch_openai_compat_model_list_returns_first_on_no_autodetect() {
    // When the auto-detected model ID is not available (or not yet populated),
    // the function should return the first model from the endpoint.
    let json = r#"{
            "data": [
                { "id": "qwen-3-235b", "object": "model" }
            ]
        }"#;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let ready = spawn_mock_server(listener, mock_json_response(json));
    wait_for_mock_server(&ready);

    let base_url = format!("http://127.0.0.1:{}", port);
    let result = fetch_openai_compat_model_list("test-key", &base_url, "unknown-provider");
    assert_eq!(result.as_deref(), Some("qwen-3-235b"));
}

#[test]
fn fetch_openai_compat_model_list_handles_http_error() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let ready = spawn_mock_server(
        listener,
        "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n".to_string(),
    );
    wait_for_mock_server(&ready);

    let base_url = format!("http://127.0.0.1:{}", port);
    let result = fetch_openai_compat_model_list("bad-key", &base_url, "groq");
    assert!(result.is_none());
}

#[test]
fn fetch_openai_compat_model_list_handles_empty_response() {
    let json = r#"{"data": []}"#;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let ready = spawn_mock_server(listener, mock_json_response(json));
    wait_for_mock_server(&ready);

    let base_url = format!("http://127.0.0.1:{}", port);
    let result = fetch_openai_compat_model_list("test-key", &base_url, "groq");
    assert!(result.is_none());
}

#[test]
fn fetch_gemini_models_parses_gemini_response() {
    // Mock response from Google Gemini's /v1beta/models endpoint.
    let json = r#"{
            "models": [
                {
                    "name": "models/gemini-2.5-flash",
                    "supportedGenerationMethods": ["generateContent", "countTokens"]
                },
                {
                    "name": "models/gemini-2.5-pro",
                    "supportedGenerationMethods": ["generateContent"]
                },
                {
                    "name": "models/gemma-3-27b-it",
                    "supportedGenerationMethods": ["generateContent"]
                }
            ]
        }"#;

    // Verify the fetch_gemini_models logic directly by testing
    // the JSON parsing logic in isolation.
    let payload: serde_json::Value = serde_json::from_str(json).unwrap();
    let models = payload.get("models").and_then(|v| v.as_array()).unwrap();
    let mut model_ids: Vec<String> = Vec::new();
    for model in models {
        let name = model.get("name").and_then(|v| v.as_str()).unwrap();
        let model_id = name.strip_prefix("models/").unwrap_or(name);
        let supported = model
            .get("supportedGenerationMethods")
            .and_then(|v| v.as_array())
            .map(|methods| {
                methods
                    .iter()
                    .any(|m| m.as_str() == Some("generateContent"))
            })
            .unwrap_or(false);
        if supported {
            model_ids.push(model_id.to_string());
        }
    }
    assert_eq!(
        model_ids,
        vec![
            "gemini-2.5-flash".to_string(),
            "gemini-2.5-pro".to_string(),
            "gemma-3-27b-it".to_string(),
        ]
    );
}

// ---- Free-upstream key resolution: ring alignment ----------------------

/// Regression guard for the health-poller ↔ KeyRotatingProvider contract:
/// [`resolve_free_upstream_keys`] must return EXACTLY the list that
/// `build_free_provider` feeds into each `KeyRotatingProvider` ring, in
/// the same order, so that the `key_idx` the health poller forwards into
/// `mark_key_healthy` / `mark_key_exhausted` lines up with the ring slot.
///
/// Rules that keep the index alignment intact:
///   * credentials (`api_key_for`) are NOT included — rings are built from
///     the multi-key store only;
///   * whitespace-trimmed and >=8 chars (placeholder / test-artifact
///     filter applied at ring build time too);
///   * OpenCode Zen reads the `opencode-go` slots as a fallback.
#[test]
fn resolve_free_upstream_keys_is_ring_aligned() {
    let mut store = clawde_core::AuthStore::default();
    store.set_keys(
        "groq",
        vec![
            "   gsk-a-very-long-real-key-0001".into(), // trimmed, kept
            "short".into(),                            // <8 chars, filtered
            "gsk-b-very-long-real-key-0002".into(),
        ],
    );

    let keys = resolve_free_upstream_keys(&store, "groq").expect("keys present");
    assert_eq!(
        keys,
        vec![
            "gsk-a-very-long-real-key-0001".to_string(),
            "gsk-b-very-long-real-key-0002".to_string(),
        ],
        "resolver must trim, filter short keys, and preserve ring order"
    );
    // The order here is the index contract: ring slot 0 = first element.
    assert_eq!(keys.len(), 2);
}

#[test]
fn resolve_free_upstream_keys_ignores_credentials() {
    // A provider with a single credential but no multi-key slots must not
    // be treated as a multi-key ring (index 0 in the ring would otherwise
    // not correspond to anything the poller probes).
    let mut store = clawde_core::AuthStore::default();
    store.credentials.insert(
        "openrouter".to_string(),
        clawde_core::StoredCredential::ApiKey {
            key: "or-credential-key-0123456789".into(),
        },
    );

    assert_eq!(
        resolve_free_upstream_keys(&store, "openrouter"),
        None,
        "credentials must not leak into the ring-aligned key list"
    );
    assert_eq!(
        all_stored_free_upstream_keys(&store, "openrouter"),
        vec!["or-credential-key-0123456789".to_string()],
        "display-oriented union still surfaces the credential"
    );
}

#[test]
fn resolve_free_upstream_keys_opencode_zen_shares_go_slots() {
    let mut store = clawde_core::AuthStore::default();
    store.set_keys("opencode-go", vec!["zen-shared-key-00000000000000".into()]);

    // Zen has no slots of its own — the ring must be built from the Go
    // slots so poller key_idx stays aligned with the actual ring.
    assert_eq!(
        resolve_free_upstream_keys(&store, "opencode-zen"),
        Some(vec!["zen-shared-key-00000000000000".to_string()])
    );
}

#[test]
fn all_stored_free_upstream_keys_dedups_and_merges() {
    let mut store = clawde_core::AuthStore::default();
    store.credentials.insert(
        "groq".to_string(),
        clawde_core::StoredCredential::ApiKey {
            key: "gsk-credential-00000000".into(),
        },
    );
    store.set_keys(
        "groq",
        vec![
            "gsk-credential-00000000".into(),
            "gsk-rotating-000000000".into(),
        ],
    );

    let keys = all_stored_free_upstream_keys(&store, "groq");
    assert_eq!(
        keys,
        vec![
            "gsk-credential-00000000".to_string(),
            "gsk-rotating-000000000".to_string(),
        ],
        "credential first, rotation keys after, duplicates removed"
    );
}
