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
// compatible endpoint" idea, ported into clawde's native provider
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
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};

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
mod capacity;
mod catalog;
mod discovery;

use capacity::CapacityState;

// Internal const shared with the catalog's cloudflare entry and the
// cloudflare chat probe below.
use catalog::local_quota_for;
use catalog::CLOUDFLARE_PROBE_MODEL;

pub use catalog::{
    catalog_entry, store_free_last_route, store_free_model_defaults, store_free_model_lists,
    take_free_last_route, take_free_model_defaults, take_free_model_lists, FreeLastRoute,
    FreeModelListEntry, FreeUpstream, LocalQuota, LocalQuotaWindow, FREE_CATALOG,
};
pub use discovery::{
    discovery_for, fetch_cline_free_model, fetch_cline_free_models,
    fetch_cloudflare_available_free_models, fetch_gemini_free_models, fetch_gemini_models,
    fetch_openai_compat_free_models, fetch_openai_compat_model_list, fetch_opencode_zen_free_model,
    fetch_opencode_zen_free_models, fetch_openrouter_free_model, fetch_openrouter_free_models,
    run_live_discovery, run_live_discovery_models, FreeModelDiscovery,
};

// Further sub-modules: inherent impl + streaming + trait impl (mutually
// coupled through private helpers, so one module), the models.dev
// auto-detection helper, and the Phase 2 task classifier (smart router).
mod impls;
mod modelsdev;
mod task_classifier;
pub use modelsdev::{fetch_best_free_models_from_modelsdev, modelsdev_free_model_ids};
pub use task_classifier::{classify_request, task_preference_ids, TaskType};

/// Select the first configured vision-capable free upstream in catalog order.
///
/// The returned `provider/model` pin is still routed through the composite
/// FreeProvider by the query layer, so transient failures can fall through to
/// another configured vision upstream. This is used by the TUI image mode;
/// the synthetic `free/auto` entry is intentionally absent from the static
/// model registry and cannot be selected via `best_vision_model_for_provider`.
pub fn first_configured_vision_model(auth_store: &clawde_core::AuthStore) -> Option<String> {
    let effective_models = take_free_model_defaults();
    FREE_CATALOG.iter().find_map(|upstream| {
        if !upstream.vision
            || crate::providers::free::first_free_upstream_key(auth_store, upstream.id).is_none()
        {
            return None;
        }
        let model = effective_models
            .iter()
            .find(|(id, _, _)| id == upstream.id)
            .map(|(_, _, model)| model.as_str())
            .unwrap_or(upstream.default_model);
        Some(format!("{}/{}", upstream.id, model))
    })
}

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

const TELEMETRY_HALF_LIFE_SECS: u64 = 7 * 24 * 60 * 60;
const TELEMETRY_MAX_AGE_SECS: u64 = 30 * 24 * 60 * 60;

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
fn current_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn current_unix_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

fn clamp_max_tokens_for(req: &mut ProviderRequest, entry: &FreeEntry) {
    if let Some(cap) = entry.upstream.max_tokens_cap {
        req.max_tokens = req.max_tokens.min(cap);
    }
}

/// Shape an explicit effort override onto the upstream's native thinking
/// parameters.
///
/// The query layer cannot know which upstream will serve a `free` request
/// (the plan depends on cooldown / latency / task routing), so the chain
/// re-assembles per-entry request parameters at dispatch time — mirroring the
/// `build_provider_options` the query layer performs for direct providers.
/// No-op when the request carries no effort override, or when the upstream's
/// model family exposes no thinking control. The per-upstream mapping is the
/// shared [`shape_provider_thinking`] in `effort_shaping`, the same single
/// source of truth the query layer uses; used by every dispatch site
/// (non-streaming fallback, streaming fallback, `RetryingFreeStream`
/// re-dispatch, and the hedge path).
fn shape_thinking_for_upstream(req: &mut ProviderRequest, entry: &FreeEntry) {
    use crate::providers::effort_shaping::shape_provider_thinking;

    // Only re-shape when the request carries an explicit effort override;
    // otherwise the upstream's own default (or the query layer's shaping for
    // direct providers) stands.
    if req.effort_level.is_none() {
        return;
    }
    // Requests assembled without provider options (test-constructed requests)
    // must not silently drop the override — fuse into an object first.
    if !req.provider_options.is_object() {
        req.provider_options = serde_json::json!({});
    }
    let Some(options) = req.provider_options.as_object_mut() else {
        return;
    };
    // max_tokens is already clamped to the entry's cap by every dispatch
    // site before this runs, so Google's thinkingBudget clamp sees the
    // effective output budget.
    shape_provider_thinking(
        options,
        entry.upstream.id,
        &req.model.to_ascii_lowercase(),
        req.effort_level,
        None,
        Some(req.max_tokens),
    );
}

/// Serialize provider-state writes in this process. The per-file lock below
/// extends the same guarantee across separate Clawde processes.
static PERSISTENCE_WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
const PERSISTENCE_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const PERSISTENCE_STALE_LOCK_AGE: std::time::Duration = std::time::Duration::from_secs(30);

/// Write a snapshot only when it is at least as new as the snapshot already
/// on disk. A process may build a snapshot, get descheduled, and then write it
/// after another process has persisted a newer snapshot; timestamp ordering
/// prevents that older snapshot from rolling state back.
fn write_private_json_if_newer(path: &std::path::Path, json: &str) {
    let _guard = PERSISTENCE_WRITE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    write_private_json_locked(path, Some(json), true, true);
}

fn write_private_json_preserving_newer(path: &std::path::Path, json: &str) {
    let _guard = PERSISTENCE_WRITE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    write_private_json_locked(path, Some(json), true, false);
}

fn write_private_json_locked(
    path: &std::path::Path,
    json: Option<&str>,
    preserve_newer: bool,
    merge_telemetry: bool,
) {
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    set_private_dir_permissions(parent);

    let Some(file_lock) = acquire_persistence_file_lock(path) else {
        return;
    };

    let Some(json) = json else {
        let _ = std::fs::remove_file(path);
        drop(file_lock);
        return;
    };

    let merged_json = if preserve_newer && merge_telemetry {
        merge_telemetry_snapshot(path, json)
    } else {
        None
    };
    let json = merged_json.as_deref().unwrap_or(json);
    if preserve_newer && incoming_snapshot_is_older(path, json) {
        drop(file_lock);
        return;
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state");
    let tmp = parent.join(format!(".{file_name}.tmp-{}", uuid::Uuid::new_v4()));
    let Ok(mut file) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
    else {
        drop(file_lock);
        return;
    };
    if file.write_all(json.as_bytes()).is_err() {
        let _ = std::fs::remove_file(&tmp);
        drop(file_lock);
        return;
    }
    set_private_file_permissions(&tmp);
    if replace_file_atomically(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    drop(file_lock);
}

struct PersistenceFileLock {
    path: std::path::PathBuf,
}

impl Drop for PersistenceFileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn acquire_persistence_file_lock(path: &std::path::Path) -> Option<PersistenceFileLock> {
    let file_name = path.file_name()?.to_str()?;
    let lock_path = path.with_file_name(format!(".{file_name}.lock"));
    let deadline = Instant::now() + PERSISTENCE_LOCK_TIMEOUT;

    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(mut lock_file) => {
                let _ = writeln!(lock_file, "pid={}", std::process::id());
                set_private_file_permissions(&lock_path);
                return Some(PersistenceFileLock { path: lock_path });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = std::fs::metadata(&lock_path)
                    .ok()
                    .and_then(|metadata| metadata.modified().ok())
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|age| age >= PERSISTENCE_STALE_LOCK_AGE);
                if stale && persistence_lock_can_be_reclaimed(&lock_path) {
                    let _ = std::fs::remove_file(&lock_path);
                    continue;
                }
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }
}

#[cfg(target_os = "linux")]
fn persistence_lock_can_be_reclaimed(path: &std::path::Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return true;
    };
    let Some(pid) = contents
        .strip_prefix("pid=")
        .and_then(|value| value.lines().next())
        .and_then(|value| value.trim().parse::<u32>().ok())
    else {
        return true;
    };
    !std::path::Path::new("/proc")
        .join(pid.to_string())
        .try_exists()
        .unwrap_or(false)
}

#[cfg(not(target_os = "linux"))]
fn persistence_lock_can_be_reclaimed(_path: &std::path::Path) -> bool {
    // Other platforms lack a portable process-existence probe here; the age
    // threshold remains the conservative fallback.
    true
}

fn merge_telemetry_snapshot(path: &std::path::Path, incoming: &str) -> Option<String> {
    let existing = std::fs::read_to_string(path).ok()?;
    let mut existing_entries =
        serde_json::from_str::<Vec<UpstreamTelemetrySnapshot>>(&existing).ok()?;
    let incoming_entries = serde_json::from_str::<Vec<UpstreamTelemetrySnapshot>>(incoming).ok()?;

    for incoming_entry in incoming_entries {
        let Some(existing_entry) = existing_entries
            .iter_mut()
            .find(|entry| entry.upstream == incoming_entry.upstream)
        else {
            existing_entries.push(incoming_entry);
            continue;
        };

        let existing_timestamp = existing_entry.saved_at_unix_nanos;
        let incoming_timestamp = incoming_entry.saved_at_unix_nanos;
        existing_entry.successes = existing_entry.successes.max(incoming_entry.successes);
        existing_entry.failures = existing_entry.failures.max(incoming_entry.failures);
        existing_entry.saved_at_unix = existing_entry
            .saved_at_unix
            .max(incoming_entry.saved_at_unix);
        existing_entry.saved_at_unix_nanos = existing_timestamp.max(incoming_timestamp);
        for (task, count) in incoming_entry.task_successes {
            let current = existing_entry.task_successes.entry(task).or_insert(0);
            *current = (*current).max(count);
        }
        for (task, count) in incoming_entry.task_failures {
            let current = existing_entry.task_failures.entry(task).or_insert(0);
            *current = (*current).max(count);
        }
        if incoming_timestamp >= existing_timestamp
            || incoming_entry.samples.len() > existing_entry.samples.len()
        {
            existing_entry.samples = incoming_entry.samples;
        }
        if incoming_timestamp >= existing_timestamp
            || incoming_entry.ttft_samples.len() > existing_entry.ttft_samples.len()
        {
            existing_entry.ttft_samples = incoming_entry.ttft_samples;
        }
        // The incoming snapshot is the fresher write — its failure reason (if
        // any) reflects the most recent dispatch. A None never overwrites a
        // recorded reason; a Some always does.
        if incoming_entry.last_failure_reason.is_some() {
            existing_entry.last_failure_reason = incoming_entry.last_failure_reason;
        }
    }

    serde_json::to_string_pretty(&existing_entries).ok()
}

/// Cap a persisted failure reason so a verbose upstream error (some providers
/// return multi-sentence messages) cannot bloat `telemetry-state/free.json`,
/// which is rewritten on every dispatch. Truncated reasons keep the upstream
/// prefix and error kind — the tail details are rarely diagnostic.
fn cap_failure_reason(reason: String) -> String {
    const MAX_REASON_CHARS: usize = 160;
    if reason.len() <= MAX_REASON_CHARS {
        return reason;
    }
    let mut truncated: String = reason.chars().take(MAX_REASON_CHARS).collect();
    truncated.push('…');
    truncated
}

fn remove_snapshot_if_unchanged(path: &std::path::Path) {
    let observed = std::fs::read_to_string(path).ok();
    let _guard = PERSISTENCE_WRITE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(file_lock) = acquire_persistence_file_lock(path) else {
        return;
    };
    if observed == std::fs::read_to_string(path).ok() {
        let _ = std::fs::remove_file(path);
    }
    drop(file_lock);
}

fn incoming_snapshot_is_older(path: &std::path::Path, incoming: &str) -> bool {
    let Ok(existing) = std::fs::read_to_string(path) else {
        return false;
    };
    let incoming_time = snapshot_timestamp_nanos(incoming);
    let existing_time = snapshot_timestamp_nanos(&existing);
    matches!((incoming_time, existing_time), (Some(incoming), Some(existing)) if incoming < existing)
}

#[derive(Deserialize)]
struct PersistenceTimestamp {
    #[serde(default)]
    saved_at_unix_nanos: u64,
}

fn snapshot_timestamp_nanos(json: &str) -> Option<u64> {
    let entries = serde_json::from_str::<Vec<PersistenceTimestamp>>(json).ok()?;
    entries
        .iter()
        .map(|entry| entry.saved_at_unix_nanos)
        .filter(|timestamp| *timestamp > 0)
        .max()
}

fn replace_file_atomically(
    tmp: &std::path::Path,
    destination: &std::path::Path,
) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        match std::fs::rename(tmp, destination) {
            Ok(()) => return Ok(()),
            Err(first_error) if destination.exists() => {
                let backup = destination.with_file_name(format!(
                    ".{}.bak-{}",
                    destination
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("state"),
                    uuid::Uuid::new_v4()
                ));
                if let Err(backup_error) = std::fs::rename(destination, &backup) {
                    return Err(backup_error);
                }
                return match std::fs::rename(tmp, destination) {
                    Ok(()) => {
                        let _ = std::fs::remove_file(backup);
                        Ok(())
                    }
                    Err(replace_error) => {
                        let _ = std::fs::rename(&backup, destination);
                        Err(replace_error)
                    }
                };
            }
            Err(first_error) => return Err(first_error),
        }
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(tmp, destination)
    }
}

#[cfg(unix)]
fn set_private_file_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = std::fs::metadata(path) {
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o600);
        let _ = std::fs::set_permissions(path, permissions);
    }
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &std::path::Path) {}

#[cfg(unix)]
fn set_private_dir_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = std::fs::metadata(path) {
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o700);
        let _ = std::fs::set_permissions(path, permissions);
    }
}

#[cfg(not(unix))]
fn set_private_dir_permissions(_path: &std::path::Path) {}

// ---------------------------------------------------------------------------
// Free-mode discovery caches (audit fix F2)
//
// Best-free-model auto-detection (models.dev) and live per-upstream discovery
// used to re-run blocking HTTP fetches on every CLI startup — up to 5s for
// models.dev plus 5s per configured discovery-capable upstream, each process.
// These helpers persist the derived results under `{clawde_home}/free-state/`
// so a fresh process starts from disk instead of the network. The in-process
// `AUTO_DETECTED_DEFAULTS` / `LIVE_DISCOVERY_CACHE` caches remain the fast path
// within a process; the disk cache covers the first call of a new process.
// ---------------------------------------------------------------------------

/// How long a persisted discovery result is trusted before a refetch. Both the
/// models.dev catalog and the per-upstream model lists are slow-moving.
// 6h: the per-upstream probes (OpenAI-compat /models, cloudflare account
// search, cline, openrouter, zen, gemini) are cheap single GETs, so a shorter
// TTL keeps new models visible sooner without meaningful cost. The 24h window
// previously made a stale pick stick for a full day.
const DISCOVERY_CACHE_TTL_SECS: u64 = 6 * 60 * 60;
// The models.dev api.json fetch is a much heavier payload than the per-upstream
// probes, so its persisted defaults keep a longer (24h) freshness window.
const MODELSDEV_DEFAULTS_TTL_SECS: u64 = 24 * 60 * 60;

fn free_state_dir() -> std::path::PathBuf {
    clawde_core::config::Settings::config_dir().join("free-state")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ModelsDevDefaultsCache {
    #[serde(default)]
    saved_at_unix: u64,
    #[serde(default)]
    defaults: HashMap<String, String>,
}

/// Load the persisted models.dev auto-detection defaults when fresh.
fn load_modelsdev_defaults_cache() -> Option<HashMap<String, String>> {
    let path = free_state_dir().join("modelsdev-defaults.json");
    let json = std::fs::read_to_string(path).ok()?;
    let cached: ModelsDevDefaultsCache = serde_json::from_str(&json).ok()?;
    let now = current_unix_secs();
    if cached.saved_at_unix == 0
        || now.saturating_sub(cached.saved_at_unix) > MODELSDEV_DEFAULTS_TTL_SECS
    {
        return None;
    }
    (!cached.defaults.is_empty()).then_some(cached.defaults)
}

/// Persist models.dev auto-detection defaults (best-effort, cross-process
/// safe via the shared file-lock / atomic-write machinery).
fn save_modelsdev_defaults_cache(defaults: &HashMap<String, String>) {
    if defaults.is_empty() {
        return;
    }
    let cache = ModelsDevDefaultsCache {
        saved_at_unix: current_unix_secs(),
        defaults: defaults.clone(),
    };
    let json = match serde_json::to_string_pretty(&cache) {
        Ok(j) => j,
        Err(_) => return,
    };
    write_private_json_locked(
        &free_state_dir().join("modelsdev-defaults.json"),
        Some(&json),
        false,
        false,
    );
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LiveDiscoveryCache {
    #[serde(default)]
    saved_at_unix: u64,
    /// upstream id → discovered free model ids (successful discoveries only),
    /// default pick first.
    #[serde(default)]
    models: HashMap<String, Vec<String>>,
}

/// Load a persisted live-discovery result for `upstream_id` when fresh.
/// Returns the full discovered free list (default pick first), or `None`
/// when no fresh entry exists.
fn load_live_discovery_cache(upstream_id: &str) -> Option<Vec<String>> {
    let path = free_state_dir().join("live-discovery.json");
    let json = std::fs::read_to_string(path).ok()?;
    let cached: LiveDiscoveryCache = serde_json::from_str(&json).ok()?;
    let now = current_unix_secs();
    if cached.saved_at_unix == 0
        || now.saturating_sub(cached.saved_at_unix) > DISCOVERY_CACHE_TTL_SECS
    {
        return None;
    }
    cached
        .models
        .get(upstream_id)
        .cloned()
        .filter(|m| !m.is_empty())
}

/// Persist a live-discovery result (full free list). Only successful
/// discoveries are written so a temporarily-down upstream is re-probed on the
/// next process (recovery stays prompt). Merges with the on-disk map so
/// concurrent processes each add their own upstreams without clobbering the
/// others.
fn save_live_discovery_cache(upstream_id: &str, model: Option<Vec<String>>) {
    let Some(model) = model else { return };
    if model.is_empty() {
        return;
    }
    let path = free_state_dir().join("live-discovery.json");
    let mut cache = std::fs::read_to_string(&path)
        .ok()
        .and_then(|json| serde_json::from_str::<LiveDiscoveryCache>(&json).ok())
        .unwrap_or(LiveDiscoveryCache {
            saved_at_unix: current_unix_secs(),
            models: HashMap::new(),
        });
    cache.models.insert(upstream_id.to_string(), model);
    cache.saved_at_unix = current_unix_secs();
    let json = match serde_json::to_string_pretty(&cache) {
        Ok(j) => j,
        Err(_) => return,
    };
    write_private_json_locked(&path, Some(&json), false, false);
}

/// Snapshot of the persisted live-discovery cache: the per-upstream default
/// picks last written (`upstream id → default model id`) plus the
/// `saved_at_unix` timestamp. `None` when no cache file exists yet. Exposed
/// for `clawde models --verbose` and the stats dialog.
pub fn live_discovery_snapshot() -> Option<(std::collections::HashMap<String, String>, u64)> {
    let path = free_state_dir().join("live-discovery.json");
    let json = std::fs::read_to_string(path).ok()?;
    let cached: LiveDiscoveryCache = serde_json::from_str(&json).ok()?;
    let picks: HashMap<String, String> = cached
        .models
        .into_iter()
        .filter_map(|(upstream, mut models)| {
            (!models.is_empty()).then(|| (upstream, models.remove(0)))
        })
        .collect();
    Some((picks, cached.saved_at_unix))
}

/// Force the free chain's live discovery to re-probe on the next build.
///
/// Clears the in-process per-upstream discovery cache and expires the
/// persisted `live-discovery.json` / `modelsdev-defaults.json` caches so the
/// next chain build re-runs every upstream probe instead of serving results
/// inside the 24-hour cache window. The lossless models.dev registry snapshot
/// is intentionally untouched — `clawde models --refresh` covers that layer.
/// Exposed for `clawde --refresh-models`.
///
/// Note: the in-process models.dev defaults (`AUTO_DETECTED_DEFAULTS` OnceLock
/// in modelsdev.rs) are only reset by a fresh process, so deleting
/// `modelsdev-defaults.json` only takes effect for a new process. Callers that
/// have already fetched models.dev defaults in-process should rely on the next
/// process (or call before the first fetch, as the CLI does).
pub fn force_refresh_discovery_caches() {
    discovery::clear_live_discovery_cache();
    for name in ["live-discovery.json", "modelsdev-defaults.json"] {
        let path = free_state_dir().join(name);
        if path.exists() {
            match std::fs::remove_file(&path) {
                Ok(_) => tracing::info!("force-refreshed free discovery cache: {name}"),
                Err(err) => tracing::warn!("failed to remove {}: {err}", path.display()),
            }
        }
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
    0
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

/// Adaptive concurrency controller (based on Netflix gradient-based approach).
/// Tracks latency gradient to dynamically adjust concurrency limits.
/// Infrastructure ready for future integration.
#[allow(dead_code)]
struct AdaptiveConcurrency {
    /// Current concurrency limit per provider
    limits: HashMap<String, u32>,
    /// No-load latency baseline (seconds)
    baseline_latency: HashMap<String, f64>,
    /// Sliding window of recent latencies
    latency_window: HashMap<String, VecDeque<f64>>,
    /// Window size in milliseconds
    window_size_ms: u64,
    /// Gradient threshold below which to reduce concurrency
    gradient_threshold: f64,
}

#[allow(dead_code)]
impl AdaptiveConcurrency {
    fn new(window_size_ms: u64, gradient_threshold: f64) -> Self {
        Self {
            limits: HashMap::new(),
            baseline_latency: HashMap::new(),
            latency_window: HashMap::new(),
            window_size_ms,
            gradient_threshold,
        }
    }

    /// Record a latency observation and update concurrency limit.
    fn record_latency(&mut self, provider_id: &str, latency_secs: f64) {
        // Update sliding window
        let window = self
            .latency_window
            .entry(provider_id.to_string())
            .or_default();
        window.push_back(latency_secs);

        // Keep only recent observations (based on time window)
        // For simplicity, keep last 100 observations
        while window.len() > 100 {
            window.pop_front();
        }

        // Update baseline if not set
        if !self.baseline_latency.contains_key(provider_id) {
            self.baseline_latency
                .insert(provider_id.to_string(), latency_secs);
        }

        // Calculate gradient
        let baseline = self
            .baseline_latency
            .get(provider_id)
            .copied()
            .unwrap_or(100.0);
        let gradient = baseline / latency_secs;

        // Update limit based on Netflix formula
        let current_limit = self.limits.get(provider_id).copied().unwrap_or(10);
        let queue_size = (current_limit as f64).sqrt() as u32;
        let new_limit = ((current_limit as f64 * gradient) as u32 + queue_size).clamp(1, 100); // Cap at 100 concurrent requests

        self.limits.insert(provider_id.to_string(), new_limit);
    }

    /// Check if we can accept a request to this provider.
    fn can_accept_request(&self, _provider_id: &str) -> bool {
        // For now, always allow - actual tracking would need request counting
        true
    }

    /// Get the current concurrency limit for a provider.
    fn get_limit(&self, provider_id: &str) -> u32 {
        self.limits.get(provider_id).copied().unwrap_or(10)
    }
}

/// Memory-efficient stream manager (based on vLLM PagedAttention concepts).
/// Tracks active streams and enforces memory budgets.
/// Infrastructure ready for future integration.
#[allow(dead_code)]
struct StreamManager {
    /// Active streams with memory tracking
    active_streams: HashMap<usize, StreamState>,
    /// Memory budget per stream (estimated tokens)
    memory_budget: u32,
    /// Maximum concurrent streams
    max_concurrent: u32,
    /// Next stream ID
    next_id: usize,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct StreamState {
    /// Tokens received so far
    tokens_received: u64,
    /// Memory used (estimated)
    memory_used: usize,
    /// Whether stream is being cancelled
    cancelling: bool,
    /// Provider index
    provider_idx: usize,
}

#[allow(dead_code)]
impl StreamManager {
    fn new(max_concurrent: u32, memory_budget: u32) -> Self {
        Self {
            active_streams: HashMap::new(),
            memory_budget,
            max_concurrent,
            next_id: 0,
        }
    }

    /// Check if we can start a new stream.
    fn can_start_stream(&self) -> bool {
        self.active_streams.len() < self.max_concurrent as usize
    }

    /// Register a new stream and return its ID.
    fn register_stream(&mut self, provider_idx: usize) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        self.active_streams.insert(
            id,
            StreamState {
                tokens_received: 0,
                memory_used: 0,
                cancelling: false,
                provider_idx,
            },
        );
        id
    }

    /// Update stream token count.
    fn update_stream(&mut self, stream_id: usize, tokens: u64) {
        if let Some(state) = self.active_streams.get_mut(&stream_id) {
            state.tokens_received += tokens;
            state.memory_used += tokens as usize * 4; // Estimate 4 bytes per token
        }
    }

    /// Cancel a stream.
    fn cancel_stream(&mut self, stream_id: usize) {
        if let Some(state) = self.active_streams.get_mut(&stream_id) {
            state.cancelling = true;
        }
    }

    /// Remove a completed stream.
    fn remove_stream(&mut self, stream_id: usize) {
        self.active_streams.remove(&stream_id);
    }

    /// Get total memory usage.
    fn total_memory_used(&self) -> usize {
        self.active_streams.values().map(|s| s.memory_used).sum()
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
    /// Nanosecond timestamp used to reject stale cross-process snapshots.
    #[serde(default)]
    saved_at_unix_nanos: u64,
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

    fn apply_upstream_cooldown(&mut self, idx: usize, base_cooldown_secs: u64) {
        if base_cooldown_secs == 0 || idx >= self.cooldown_until.len() {
            return;
        }
        // Calculate failure count for exponential backoff
        let failure_count = self.failures.get(idx).map_or(0, |f| f.len() as u32);
        // Get max cooldown from profile or use a reasonable default
        let max_cooldown = base_cooldown_secs.saturating_mul(5).min(600);
        // Apply exponential backoff with jitter
        let cooldown =
            calculate_exponential_backoff(base_cooldown_secs, failure_count, max_cooldown);
        self.cooldown_until[idx] = Some(Instant::now() + std::time::Duration::from_secs(cooldown));
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
                    saved_at_unix_nanos: current_unix_nanos(),
                })
            })
            .collect();
        if entries.is_empty() {
            remove_snapshot_if_unchanged(path);
            return;
        }
        let json = match serde_json::to_string_pretty(&entries) {
            Ok(j) => j,
            Err(_) => return,
        };
        write_private_json_preserving_newer(path, &json);
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
    /// Sliding window of time-to-first-token durations (seconds) per upstream
    /// index. TTFT is a better UX proxy than total latency: users feel
    /// responsiveness, not total wall time. Tracked separately so routing
    /// can prefer upstreams that start producing quickly.
    ttft_samples: Vec<VecDeque<f64>>,
    /// Successful dispatches per upstream index.
    successes: Vec<u32>,
    /// Failed dispatches per upstream index.
    failures: Vec<u32>,
    /// Per-upstream per-task dispatch counters (task key → count), for the
    /// routing dialog's per-task model-performance view (spec §8.6): an
    /// upstream can be 100% on verification tasks yet 0% on code generation.
    task_successes: Vec<HashMap<String, u32>>,
    task_failures: Vec<HashMap<String, u32>>,
    /// Last recorded failure reason per upstream (e.g. `[groq] Rate limited`)
    /// so `/keys health` can explain WHY an upstream's success rate is down
    /// without needing a live failing request. `None` before the first
    /// failure. Cleared by a successful dispatch.
    last_failure_reasons: Vec<Option<String>>,
    /// Upstream IDs and optional disk path for cross-process telemetry.
    /// Keeping this metadata with the shared state means direct and streaming
    /// dispatches persist through the same lock without duplicating plumbing.
    upstream_ids: Vec<String>,
    persist_path: Option<std::path::PathBuf>,
}

impl LatencyState {
    fn new(n: usize) -> Self {
        let mut samples = Vec::with_capacity(n);
        let mut ttft_samples = Vec::with_capacity(n);
        let mut successes = Vec::with_capacity(n);
        let mut failures = Vec::with_capacity(n);
        let mut task_successes = Vec::with_capacity(n);
        let mut task_failures = Vec::with_capacity(n);
        let mut last_failure_reasons = Vec::with_capacity(n);
        for _ in 0..n {
            samples.push(VecDeque::with_capacity(10));
            ttft_samples.push(VecDeque::with_capacity(10));
            successes.push(0);
            failures.push(0);
            task_successes.push(HashMap::new());
            task_failures.push(HashMap::new());
            last_failure_reasons.push(None);
        }
        Self {
            samples,
            ttft_samples,
            successes,
            failures,
            task_successes,
            task_failures,
            last_failure_reasons,
            upstream_ids: Vec::new(),
            persist_path: None,
        }
    }

    fn with_persistence(
        mut self,
        upstream_ids: Vec<String>,
        persist_path: Option<std::path::PathBuf>,
        max_samples: usize,
    ) -> Self {
        self.upstream_ids = upstream_ids;
        self.persist_path = persist_path;
        if let Some(path) = self.persist_path.clone() {
            self.load_from_file(&path, max_samples);
        }
        self
    }

    /// Record a latency sample at `idx`.
    fn record(&mut self, idx: usize, duration_secs: f64, max_samples: usize) {
        if idx >= self.samples.len() || max_samples == 0 {
            return;
        }
        let q = &mut self.samples[idx];
        if q.len() >= max_samples {
            q.pop_front();
        }
        q.push_back(duration_secs);
    }

    /// Record a time-to-first-token sample at `idx`. TTFT is tracked
    /// separately from total latency so routing can prefer upstreams that
    /// start producing quickly even if their total time is longer.
    fn record_ttft(&mut self, idx: usize, ttft_secs: f64, max_samples: usize) {
        if idx >= self.ttft_samples.len() || max_samples == 0 {
            return;
        }
        let q = &mut self.ttft_samples[idx];
        if q.len() >= max_samples {
            q.pop_front();
        }
        q.push_back(ttft_secs);
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

    /// Record the reason for the failure at `idx` (e.g. `[groq] Rate
    /// limited`) so `/keys health` can explain a degraded success rate.
    fn record_failure_reason(&mut self, idx: usize, reason: String) {
        if let Some(slot) = self.last_failure_reasons.get_mut(idx) {
            *slot = Some(cap_failure_reason(reason));
        }
    }

    /// Clear the recorded failure reason at `idx` — a later success means the
    /// upstream recovered, and showing a stale failure would mislead.
    fn clear_failure_reason(&mut self, idx: usize) {
        if let Some(slot) = self.last_failure_reasons.get_mut(idx) {
            *slot = None;
        }
    }

    /// Last recorded failure reason per upstream, or `None`.
    fn last_failure_reasons(&self) -> Vec<Option<String>> {
        self.last_failure_reasons.clone()
    }

    /// Record a successful dispatch of `task` at `idx` (per-task view).
    fn record_task_success(&mut self, idx: usize, task: TaskType) {
        if let Some(m) = self.task_successes.get_mut(idx) {
            let e = m.entry(task.key().to_string()).or_insert(0);
            *e = e.saturating_add(1);
        }
    }

    /// Record a failed dispatch of `task` at `idx` (per-task view).
    fn record_task_failure(&mut self, idx: usize, task: TaskType) {
        if let Some(m) = self.task_failures.get_mut(idx) {
            let e = m.entry(task.key().to_string()).or_insert(0);
            *e = e.saturating_add(1);
        }
    }

    /// Per-task dispatch success rate (0.0–1.0) for upstream `idx`, or
    /// `None` when no dispatch of that task has been recorded.
    fn task_success_rate(&self, idx: usize, task: TaskType) -> Option<f64> {
        let successes = self
            .task_successes
            .get(idx)
            .and_then(|m| m.get(task.key()))
            .copied()
            .unwrap_or(0);
        let failures = self
            .task_failures
            .get(idx)
            .and_then(|m| m.get(task.key()))
            .copied()
            .unwrap_or(0);
        let total = successes + failures;
        (total > 0).then(|| successes as f64 / total as f64)
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

    /// Calculate percentile latency for upstream `idx`.
    fn percentile_latency(&self, idx: usize, percentile: f64) -> f64 {
        if idx >= self.samples.len() {
            return f64::MAX;
        }
        let q = &self.samples[idx];
        if q.is_empty() {
            return f64::MAX;
        }
        let mut sorted: Vec<f64> = q.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((sorted.len() as f64) * percentile).min((sorted.len() - 1) as f64) as usize;
        sorted[idx]
    }

    /// Average time-to-first-token for upstream `idx`, or `f64::MAX` if no
    /// TTFT samples. TTFT is a better routing signal than total latency for
    /// user-perceived responsiveness.
    fn avg_ttft(&self, idx: usize) -> f64 {
        if idx >= self.ttft_samples.len() {
            return f64::MAX;
        }
        let q = &self.ttft_samples[idx];
        if q.is_empty() {
            return f64::MAX;
        }
        let sum: f64 = q.iter().sum();
        sum / q.len() as f64
    }

    /// Percentile time-to-first-token for upstream `idx`.
    #[allow(dead_code)]
    fn percentile_ttft(&self, idx: usize, percentile: f64) -> f64 {
        if idx >= self.ttft_samples.len() {
            return f64::MAX;
        }
        let q = &self.ttft_samples[idx];
        if q.is_empty() {
            return f64::MAX;
        }
        let mut sorted: Vec<f64> = q.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((sorted.len() as f64) * percentile).min((sorted.len() - 1) as f64) as usize;
        sorted[idx]
    }

    /// Dispatch success rate (0.0–1.0) for upstream `idx`, or `None` when
    /// no dispatch has been recorded yet.
    fn success_rate(&self, idx: usize) -> Option<f64> {
        let successes = *self.successes.get(idx)?;
        let failures = *self.failures.get(idx)?;
        let total = successes + failures;
        (total > 0).then(|| successes as f64 / total as f64)
    }

    /// Total recorded dispatches (successes + failures) for upstream `idx`.
    /// Used to gate trust in a success rate: a couple of samples are noise.
    fn dispatches(&self, idx: usize) -> u32 {
        let successes = *self.successes.get(idx).unwrap_or(&0);
        let failures = *self.failures.get(idx).unwrap_or(&0);
        successes.saturating_add(failures)
    }

    /// Total dispatches recorded for one task. Unlike the aggregate counter,
    /// this keeps the router from treating unrelated task history as proof
    /// that an upstream is reliable for the current request.
    fn task_dispatches(&self, idx: usize, task: TaskType) -> u32 {
        let successes = self
            .task_successes
            .get(idx)
            .and_then(|m| m.get(task.key()))
            .copied()
            .unwrap_or(0);
        let failures = self
            .task_failures
            .get(idx)
            .and_then(|m| m.get(task.key()))
            .copied()
            .unwrap_or(0);
        successes.saturating_add(failures)
    }

    /// Build a disk snapshot while holding only the in-memory state lock.
    /// The returned JSON is written after the caller releases the mutex.
    fn snapshot(&self) -> Option<(std::path::PathBuf, String)> {
        let path = self.persist_path.as_ref()?.clone();
        let entries: Vec<UpstreamTelemetrySnapshot> = self
            .upstream_ids
            .iter()
            .enumerate()
            .filter_map(|(idx, upstream)| {
                let samples = self.samples.get(idx)?.iter().copied().collect::<Vec<_>>();
                let ttft_samples = self
                    .ttft_samples
                    .get(idx)?
                    .iter()
                    .copied()
                    .collect::<Vec<_>>();
                let successes = self.successes.get(idx).copied().unwrap_or(0);
                let failures = self.failures.get(idx).copied().unwrap_or(0);
                let task_successes = self.task_successes.get(idx).cloned().unwrap_or_default();
                let task_failures = self.task_failures.get(idx).cloned().unwrap_or_default();
                let last_failure_reason = self.last_failure_reasons.get(idx).cloned().flatten();
                if samples.is_empty()
                    && ttft_samples.is_empty()
                    && successes == 0
                    && failures == 0
                    && task_successes.is_empty()
                    && task_failures.is_empty()
                    && last_failure_reason.is_none()
                {
                    return None;
                }
                Some(UpstreamTelemetrySnapshot {
                    upstream: upstream.clone(),
                    samples,
                    ttft_samples,
                    successes,
                    failures,
                    task_successes,
                    task_failures,
                    last_failure_reason,
                    saved_at_unix: current_unix_secs(),
                    saved_at_unix_nanos: current_unix_nanos(),
                })
            })
            .collect();
        let json = serde_json::to_string_pretty(&entries).ok()?;
        Some((path, json))
    }

    /// Write a previously-built telemetry snapshot without holding the
    /// provider's in-memory state mutex across filesystem I/O.
    fn persist_snapshot(snapshot: Option<(std::path::PathBuf, String)>) {
        let Some((path, json)) = snapshot else {
            return;
        };
        if json != "[]" {
            write_private_json_if_newer(&path, &json);
        }
    }

    fn load_from_file(&mut self, path: &std::path::Path, max_samples: usize) {
        let Ok(json) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(entries) = serde_json::from_str::<Vec<UpstreamTelemetrySnapshot>>(&json) else {
            return;
        };
        let now = current_unix_secs();
        let file_saved_at = std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs())
            .unwrap_or(now);
        for mut entry in entries {
            let saved_at = if entry.saved_at_unix == 0 {
                file_saved_at
            } else {
                entry.saved_at_unix
            };
            let age = now.saturating_sub(saved_at);
            if age > TELEMETRY_MAX_AGE_SECS {
                continue;
            }
            if age > 0 {
                let decay = 0.5_f64.powf(age as f64 / TELEMETRY_HALF_LIFE_SECS as f64);
                entry.successes = (entry.successes as f64 * decay).round() as u32;
                entry.failures = (entry.failures as f64 * decay).round() as u32;
                for count in entry.task_successes.values_mut() {
                    *count = (*count as f64 * decay).round() as u32;
                }
                for count in entry.task_failures.values_mut() {
                    *count = (*count as f64 * decay).round() as u32;
                }
                if age > TELEMETRY_HALF_LIFE_SECS {
                    entry.samples.clear();
                    entry.ttft_samples.clear();
                    // The counters that motivated a failure reason have decayed
                    // to noise — drop the reason too so /keys health does not
                    // show a stale failure after the window passes.
                    entry.last_failure_reason = None;
                }
            }
            if entry.successes == 0
                && entry.failures == 0
                && entry.task_successes.values().all(|count| *count == 0)
                && entry.task_failures.values().all(|count| *count == 0)
                && entry.samples.is_empty()
                && entry.ttft_samples.is_empty()
                && entry.last_failure_reason.is_none()
            {
                continue;
            }
            let Some(idx) = self
                .upstream_ids
                .iter()
                .position(|id| id == &entry.upstream)
            else {
                continue;
            };
            let mut samples = VecDeque::from(entry.samples);
            while samples.len() > max_samples {
                samples.pop_front();
            }
            self.samples[idx] = samples;
            let mut ttft_samples = VecDeque::from(entry.ttft_samples);
            while ttft_samples.len() > max_samples {
                ttft_samples.pop_front();
            }
            self.ttft_samples[idx] = ttft_samples;
            self.successes[idx] = entry.successes;
            self.failures[idx] = entry.failures;
            self.task_successes[idx] = entry.task_successes;
            self.task_failures[idx] = entry.task_failures;
            self.last_failure_reasons[idx] = entry.last_failure_reason;
        }
    }
}

/// Disk snapshot of one upstream's latency, aggregate success, and per-task
/// dispatch telemetry. Entries are keyed by upstream ID so catalog ordering
/// changes do not attach history to the wrong provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct UpstreamTelemetrySnapshot {
    #[serde(default)]
    upstream: String,
    #[serde(default)]
    samples: Vec<f64>,
    /// Time-to-first-token samples. `#[serde(default)]` so telemetry files
    /// written by older builds (without TTFT tracking) still parse.
    #[serde(default)]
    ttft_samples: Vec<f64>,
    #[serde(default)]
    successes: u32,
    #[serde(default)]
    failures: u32,
    #[serde(default)]
    task_successes: HashMap<String, u32>,
    #[serde(default)]
    task_failures: HashMap<String, u32>,
    /// Last recorded failure reason (e.g. `groq: [groq] Rate limited`).
    /// `None` for healthy upstreams. `#[serde(default)]` so telemetry files
    /// written by older builds still parse.
    #[serde(default)]
    last_failure_reason: Option<String>,
    /// Unix timestamp of the snapshot. Zero denotes a legacy file; loading
    /// then falls back to the file modification time for aging.
    #[serde(default)]
    saved_at_unix: u64,
    /// Nanosecond timestamp used only to reject stale cross-process writes.
    /// Zero denotes a legacy snapshot without ordering metadata.
    #[serde(default)]
    saved_at_unix_nanos: u64,
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

/// Verdict from a free-upstream key probe.
///
/// `Transient` means the request reached the upstream but did not provide
/// enough evidence to judge the key (for example 429, 5xx, or an empty
/// completion). Callers must not evict a key for that verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamKeyProbe {
    Valid,
    Invalid(String),
    Transient(String),
}

impl UpstreamKeyProbe {
    fn into_result(self) -> Result<(), String> {
        match self {
            Self::Valid => Ok(()),
            Self::Invalid(message) | Self::Transient(message) => Err(message),
        }
    }
}

/// Resolve the key list for a free upstream, handling the OpenCode Zen/Go
/// alias (both slots share the same key).  Used by the health poller and
/// `build_free_provider` in registry.rs.
/// Resolve the rotation keys for a free-catalog upstream, in ring order.
///
/// This is the **single source of truth** for the key list a
/// [`KeyRotatingProvider`] ring is built from, so it must stay **exactly
/// aligned** with [`crate::registry`]'s ring construction: stored `keys`
/// slots are preferred (with environment/OpenCode CLI fallback for Zen when
/// no slot exists), the OpenCode Zen/Go shared slots are collapsed to a single
/// slot (Zen first, Go as fallback), and each key is trimmed with placeholders
/// shorter than 8 chars dropped — the same filtering the registry applies
/// before wrapping a pool in a ring.
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
    let clean = |keys: Option<&[String]>| {
        keys.unwrap_or_default()
            .iter()
            .map(|key| key.trim().to_string())
            .filter(|key| key.len() >= 8)
            .collect::<Vec<_>>()
    };
    // Zen and Go intentionally share one ring: prefer the canonical Zen slot,
    // but fall back to the Go alias when the Zen slot is absent or invalid.
    let filtered = if upstream_id == "opencode-zen" {
        let zen = clean(auth_store.keys_for("opencode-zen"));
        if zen.is_empty() {
            clean(auth_store.keys_for("opencode-go"))
        } else {
            zen
        }
    } else {
        clean(auth_store.keys_for(upstream_id))
    };
    if !filtered.is_empty() {
        return Some(filtered);
    }
    if upstream_id == "opencode-zen" {
        if let Some(key) = std::env::var("OPENCODE_API_KEY")
            .ok()
            .map(|key| key.trim().to_string())
            .filter(|key| key.len() >= 8)
        {
            return Some(vec![key]);
        }
        if let Some(key) = clawde_core::AuthStore::opencode_cli_api_key() {
            return Some(vec![key]);
        }
    }
    None
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
        // This helper is display/migration-oriented, not a dispatch resolver:
        // surface a legacy API credential so `/connect` and diagnostics can
        // show it while the production chain still requires `keys`.
        if let Some(clawde_core::StoredCredential::ApiKey { key }) =
            auth_store.credentials.get(slot)
        {
            let key = key.trim();
            if !key.is_empty() {
                push(key.to_string());
            }
        } else if let Some(key) = auth_store.api_key_for(slot) {
            // Preserve OAuth-backed display credentials such as Copilot while
            // keeping free API credentials out of api_key_for's dispatch path.
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
/// GitHub Copilot's OAuth credential or the provider's environment variable;
/// free API credentials must first be migrated into the canonical `keys` map.
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
    // Free API dispatch is keys-only. Legacy API credentials are intentionally
    // not a fallback here; the free chain builder migrates them explicitly
    // before resolving. GitHub Copilot is the deliberate OAuth exception:
    // its refresh/access token remains in `credentials` and is not an API-key
    // rotation slot. Environment keys remain a supported last resort until
    // successful dispatch auto-imports them into `auth.json.keys`.
    if upstream_id == "github-copilot" {
        return auth_store.api_key_for(upstream_id);
    }
    let key = if matches!(upstream_id, "opencode-zen" | "opencode-go") {
        auth_store
            .keys_for("opencode-zen")
            .and_then(|keys| keys.iter().find(|key| key.trim().len() >= 8))
            .cloned()
            .or_else(|| {
                auth_store
                    .keys_for("opencode-go")
                    .and_then(|keys| keys.iter().find(|key| key.trim().len() >= 8))
                    .cloned()
            })
            .or_else(|| std::env::var("OPENCODE_API_KEY").ok())
            .or_else(clawde_core::AuthStore::opencode_cli_api_key)
    } else {
        auth_store
            .keys_for(upstream_id)
            .and_then(|keys| keys.iter().find(|key| key.trim().len() >= 8))
            .cloned()
            .or_else(|| {
                clawde_core::config::primary_api_key_env_var_for_provider(upstream_id)
                    .and_then(|env| std::env::var(env).ok())
            })
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
                validate_key_via_chat(upstream_id, key, &client)?.headers
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
        "nvidia" | "openrouter" | "sambanova" | "cloudflare" | "poolside"
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
/// an invalid key. 429, 5xx, connection failures, and empty completion bodies
/// are transient: they do not prove the key is invalid or fully healthy.
///
/// The response body is consumed so empty/server-error content can be
/// classified, while headers are retained for [`query_rate_limits`].
///
/// Sends a 1-token `chat/completions` probe to Cloudflare's account-scoped
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
    // These overrides are intended for local mock servers. Release builds
    // ignore them unless an operator explicitly opts in, preventing an
    // inherited environment variable from redirecting API credentials.
    #[cfg(not(debug_assertions))]
    if std::env::var_os("CLAWDE_ALLOW_UNSAFE_FREE_BASE_URL").as_deref()
        != Some(std::ffi::OsStr::new("1"))
    {
        return None;
    }

    let var = format!(
        "CLAWDE_FREE_BASE_URL_{}",
        upstream_id.to_uppercase().replace('-', "_")
    );
    let value = std::env::var(&var)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;

    // The override is intended for local mock servers. Never send a bearer
    // key to a remote endpoint through this development/testing escape hatch.
    let parsed = url::Url::parse(&value).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let host = parsed.host_str()?;
    let is_loopback =
        host == "localhost" || host == "127.0.0.1" || host == "::1" || host.ends_with(".localhost");
    is_loopback.then_some(value)
}

fn chat_probe_for(upstream_id: &str) -> Option<(String, &'static str)> {
    let (base_url, default_model) = match upstream_id {
        "nvidia" => ("https://integrate.api.nvidia.com/v1", "openai/gpt-oss-120b"),
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

#[derive(Debug)]
struct ChatProbeResponse {
    status: u16,
    headers: reqwest::header::HeaderMap,
    body: String,
}

fn validate_key_via_chat(
    upstream_id: &str,
    key: &str,
    client: &reqwest::blocking::Client,
) -> Result<ChatProbeResponse, String> {
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

    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", key))
        .json(&body)
        .send()
        .map_err(|e| format!("Connection failed: {}", e))?;
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = response
        .text()
        .map_err(|e| format!("Failed to read probe response (HTTP {}): {}", status, e))?;
    Ok(ChatProbeResponse {
        status,
        headers,
        body,
    })
}

fn classify_chat_probe(status: u16, body: &str, label: &str) -> UpstreamKeyProbe {
    let lower = body.to_ascii_lowercase();
    if status == 401 || status == 403 {
        return UpstreamKeyProbe::Invalid(format!("Invalid {} (HTTP {})", label, status));
    }
    if status == 429 {
        return UpstreamKeyProbe::Transient("Rate limited — try again later".to_string());
    }
    if lower.contains("empty response content") {
        return UpstreamKeyProbe::Transient(format!(
            "Server error (HTTP {}): empty response content",
            status
        ));
    }
    if status >= 500 {
        let detail = if lower.contains("empty response content") || body.trim().is_empty() {
            "empty response content".to_string()
        } else {
            body.chars().take(160).collect()
        };
        return UpstreamKeyProbe::Transient(format!("Server error (HTTP {}): {}", status, detail));
    }
    if (200..300).contains(&status)
        && (body.trim().is_empty() || lower.contains("empty response content"))
    {
        return UpstreamKeyProbe::Transient(format!(
            "Server error (HTTP {}): empty response content",
            status
        ));
    }
    // A non-auth 4xx can mean the probe model is unavailable while auth has
    // already passed. Keep that key usable and let the request path decide.
    UpstreamKeyProbe::Valid
}

/// Probe an API key and preserve the distinction between invalid credentials
/// and transient upstream failures. This is the health poller's source of
/// truth; `validate_upstream_key` below keeps the older `Result` API for the
/// TUI and other callers.
pub fn probe_upstream_key(upstream_id: &str, key: &str) -> UpstreamKeyProbe {
    if key.trim().len() < 8 {
        return UpstreamKeyProbe::Invalid("Key too short (min 8 characters)".to_string());
    }

    if upstream_id == "cloudflare" {
        let response = match probe_cloudflare_chat(key) {
            Ok(response) => response,
            Err(error) => return UpstreamKeyProbe::Transient(error),
        };
        let status = response.status().as_u16();
        let body = match response.text() {
            Ok(body) => body,
            Err(error) => {
                return UpstreamKeyProbe::Transient(format!(
                    "Failed to read probe response (HTTP {}): {}",
                    status, error
                ));
            }
        };
        if status == 404 {
            return UpstreamKeyProbe::Invalid(
                "Invalid Cloudflare account ID (HTTP 404)".to_string(),
            );
        }
        return classify_chat_probe(status, &body, "API token");
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
        _ => {
            return UpstreamKeyProbe::Transient(format!(
                "No validation endpoint for '{}'",
                upstream_id
            ))
        }
    };
    let base_url = match free_upstream_base_url_override(upstream_id) {
        Some(override_base) => format!("{}/models", override_base.trim_end_matches('/')),
        None => native.to_string(),
    };

    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return UpstreamKeyProbe::Transient(format!("Failed to create HTTP client: {}", error))
        }
    };

    let request = if upstream_id == "google" {
        client.get(base_url).query(&[("key", key)])
    } else {
        client
            .get(base_url)
            .header("Authorization", format!("Bearer {}", key))
    };

    let response = match request.send() {
        Ok(response) => response,
        Err(error) => return UpstreamKeyProbe::Transient(format!("Connection failed: {}", error)),
    };
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return match classify_probe_status(upstream_id, status) {
            Ok(()) => UpstreamKeyProbe::Valid,
            Err(error) if error.contains("Invalid API key") => UpstreamKeyProbe::Invalid(error),
            Err(error) => UpstreamKeyProbe::Transient(error),
        };
    }
    if models_endpoint_validates_auth(upstream_id) {
        return UpstreamKeyProbe::Valid;
    }

    match validate_key_via_chat(upstream_id, key, &client) {
        Ok(response) => classify_chat_probe(response.status, &response.body, "API key"),
        Err(error) => UpstreamKeyProbe::Transient(error),
    }
}

/// Validate an API key for existing callers that only need a pass/fail result.
pub fn validate_upstream_key(upstream_id: &str, key: &str) -> Result<(), String> {
    probe_upstream_key(upstream_id, key).into_result()
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
    /// Provider-specific cooldown profiles (research-based).
    profiles: Arc<ProviderProfiles>,
    latencies: Arc<Mutex<LatencyState>>,
    /// Fresh rate-limit observations used only to demote near-exhausted
    /// upstreams. Kept separate from credential health and cooldown state.
    capacity: Arc<Mutex<CapacityState>>,
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
    /// Strict pin: try ONLY the exact (idx, model) pair. No fallback,
    /// no task-based reordering. Used by `--tool-model` when the user
    /// explicitly specifies a provider/model pair — the intent is
    /// "use THIS model, period".
    Strict { idx: usize, model: String },
}

impl Route {
    /// If this is a `Pinned` route, return `(start_idx, pinned_model)`.
    /// Used by the strict-route path to extract the upstream index.
    fn into_pinned(self) -> Option<(usize, String)> {
        match self {
            Route::Pinned {
                start_idx,
                pinned_model,
            } => Some((start_idx, pinned_model)),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod cache_tests {
    use super::*;
    use crate::test_support::TestHome;
    use clawde_core::AuthStore;

    #[test]
    fn first_configured_vision_model_uses_catalog_order() {
        let _home = TestHome::new();
        let mut auth = AuthStore::default();
        auth.set_keys("google", vec!["google-test-key-12345678".to_string()]);
        auth.set_keys(
            "github-copilot",
            vec!["github-test-key-12345678".to_string()],
        );

        // GitHub Copilot precedes Google in FREE_CATALOG, so it wins when both
        // vision-capable upstreams are configured.
        assert_eq!(
            first_configured_vision_model(&auth).as_deref(),
            Some("github-copilot/gpt-4o-2024-11-20")
        );
    }

    #[test]
    fn first_configured_vision_model_skips_non_vision_upstreams() {
        let _home = TestHome::new();
        let mut auth = AuthStore::default();
        auth.set_keys("groq", vec!["groq-test-key-12345678".to_string()]);
        assert_eq!(first_configured_vision_model(&auth), None);
    }

    /// Verbose upstream error messages are capped before persistence so
    /// `telemetry-state/free.json` cannot grow unbounded across dispatches.
    #[test]
    fn failure_reason_is_capped_before_persist() {
        let _home = TestHome::new();
        let path = clawde_core::config::Settings::config_dir()
            .join("telemetry-state")
            .join("free.json");
        let mut state =
            LatencyState::new(1).with_persistence(vec!["groq".to_string()], Some(path.clone()), 10);
        let long = "groq: [groq] Rate limited: ".to_string() + &"x".repeat(300);
        state.record_failure_reason(0, long);
        LatencyState::persist_snapshot(state.snapshot());

        let disk = std::fs::read_to_string(&path).unwrap();
        let entries: Vec<UpstreamTelemetrySnapshot> = serde_json::from_str(&disk).unwrap();
        let reason = entries[0].last_failure_reason.as_deref().unwrap();
        assert!(reason.ends_with('…'), "reason must be truncated: {reason}");
        assert!(
            reason.chars().count() <= 161,
            "capped reason must stay under the limit: {} chars",
            reason.chars().count()
        );
        // The upstream prefix survives the cap.
        assert!(
            reason.starts_with("groq: [groq] Rate limited"),
            "got: {reason}"
        );
    }

    /// `--refresh-models` semantics: force_refresh_discovery_caches() expires
    /// both persisted free-chain discovery caches (24h TTL) so the next chain
    /// build re-probes every configured upstream.
    #[test]
    fn force_refresh_discovery_caches_expires_persisted_caches() {
        let _home = TestHome::new();
        let state_dir = free_state_dir();
        std::fs::create_dir_all(&state_dir).unwrap();
        // Seed both cache files with future-fresh timestamps.
        std::fs::write(
            state_dir.join("live-discovery.json"),
            r#"{"saved_at_unix": 9999999999, "models": {"cloudflare": ["@cf/qwen/qwen3-30b-a3b-fp8"]}}"#,
        )
        .unwrap();
        std::fs::write(
            state_dir.join("modelsdev-defaults.json"),
            r#"{"saved_at_unix": 9999999999, "defaults": {"groq": "gpt-oss-120b"}}"#,
        )
        .unwrap();
        assert!(state_dir.join("live-discovery.json").exists());
        assert!(state_dir.join("modelsdev-defaults.json").exists());

        force_refresh_discovery_caches();

        assert!(!state_dir.join("live-discovery.json").exists());
        assert!(!state_dir.join("modelsdev-defaults.json").exists());
    }

    /// The per-upstream last failure reason persists to the telemetry file and
    /// survives a restart (a fresh `LatencyState` restores it), so `/keys
    /// health` explains a degraded success rate even after the process exits.
    #[test]
    fn last_failure_reason_persists_and_restores() {
        let _home = TestHome::new();
        let path = clawde_core::config::Settings::config_dir()
            .join("telemetry-state")
            .join("free.json");
        let mut state =
            LatencyState::new(1).with_persistence(vec!["groq".to_string()], Some(path.clone()), 10);
        state.record_failure_reason(0, "groq: [groq] Rate limited".to_string());
        LatencyState::persist_snapshot(state.snapshot());
        assert!(path.exists(), "telemetry file must be written");

        // A fresh state (restart) restores the reason.
        let restored =
            LatencyState::new(1).with_persistence(vec!["groq".to_string()], Some(path), 10);
        assert_eq!(
            restored.last_failure_reasons(),
            vec![Some("groq: [groq] Rate limited".to_string())]
        );
    }

    /// F2 (audit fix): persisted models.dev defaults round-trip through the
    /// `{clawde_home}/free-state/` cache.
    #[test]
    fn modelsdev_defaults_cache_round_trips() {
        let _home = TestHome::new();
        let mut defaults = HashMap::new();
        defaults.insert("groq".to_string(), "llama-3.3-70b-versatile".to_string());
        defaults.insert("cerebras".to_string(), "gpt-oss-120b".to_string());
        save_modelsdev_defaults_cache(&defaults);
        assert_eq!(load_modelsdev_defaults_cache(), Some(defaults));
    }

    /// F2: a cache older than the TTL is ignored so a fresh process re-fetches
    /// instead of trusting stale model lists.
    #[test]
    fn modelsdev_defaults_cache_stale_is_ignored() {
        let _home = TestHome::new();
        let mut defaults = HashMap::new();
        defaults.insert("groq".to_string(), "llama-3.3-70b-versatile".to_string());
        let stale = ModelsDevDefaultsCache {
            saved_at_unix: current_unix_secs().saturating_sub(MODELSDEV_DEFAULTS_TTL_SECS + 1),
            defaults,
        };
        let dir = free_state_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("modelsdev-defaults.json"),
            serde_json::to_string(&stale).unwrap(),
        )
        .unwrap();
        assert_eq!(load_modelsdev_defaults_cache(), None);
    }

    /// F2: live-discovery results persist only on success — a failed probe is
    /// left uncached so a recovering upstream is re-probed on the next process.
    #[test]
    fn live_discovery_cache_persists_only_successes() {
        let _home = TestHome::new();
        assert_eq!(load_live_discovery_cache("groq"), None);
        save_live_discovery_cache("groq", None);
        assert_eq!(
            load_live_discovery_cache("groq"),
            None,
            "failed discovery must not be persisted"
        );
        save_live_discovery_cache(
            "groq",
            Some(vec![
                "llama-3.3-70b-versatile".to_string(),
                "openai/gpt-oss-120b".to_string(),
            ]),
        );
        assert_eq!(
            load_live_discovery_cache("groq"),
            Some(vec![
                "llama-3.3-70b-versatile".to_string(),
                "openai/gpt-oss-120b".to_string(),
            ])
        );
    }
}

/// Provider-specific cooldown profile based on research.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCooldownProfile {
    /// Base cooldown for rate limit errors (seconds)
    pub rate_limit_cooldown_secs: u64,
    /// Cooldown for server errors (seconds)
    pub server_error_cooldown_secs: u64,
    /// Maximum cooldown with exponential backoff (seconds)
    pub max_cooldown_secs: u64,
    /// Whether this provider returns Retry-After headers
    pub respects_retry_after: bool,
    /// Optional notes about this provider's limits
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl Default for ProviderCooldownProfile {
    fn default() -> Self {
        Self {
            rate_limit_cooldown_secs: 120,
            server_error_cooldown_secs: 60,
            max_cooldown_secs: 600,
            respects_retry_after: false,
            notes: None,
        }
    }
}

/// Parallel attempt configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParallelConfig {
    /// Whether to try multiple providers in parallel for timeouts
    pub enabled: bool,
    /// Strategy: "hedged", "sequential", or "parallel"
    #[serde(default = "default_parallel_strategy")]
    pub strategy: String,
    /// Hedging configuration
    #[serde(default)]
    pub hedging: HedgeConfig,
    /// Power of Two Choices selection configuration
    #[serde(default)]
    pub p2c_selection: P2CConfig,
    /// Adaptive concurrency configuration
    #[serde(default)]
    pub adaptive_concurrency: AdaptiveConcurrencyConfig,
    /// Memory budget configuration
    #[serde(default)]
    pub memory_budget: MemoryBudgetConfig,
}

fn default_parallel_strategy() -> String {
    "hedged".to_string()
}

impl Default for ParallelConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            strategy: "hedged".to_string(),
            hedging: HedgeConfig::default(),
            p2c_selection: P2CConfig::default(),
            adaptive_concurrency: AdaptiveConcurrencyConfig::default(),
            memory_budget: MemoryBudgetConfig::default(),
        }
    }
}

/// Hedged request configuration (based on Google's "The Tail at Scale" paper)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HedgeConfig {
    /// Whether hedging is enabled
    pub enabled: bool,
    /// Delay before sending hedge request (milliseconds)
    #[serde(default = "default_hedge_delay")]
    pub delay_ms: u64,
    /// Maximum number of concurrent hedge requests
    #[serde(default = "default_max_hedges")]
    pub max_hedges: u32,
    /// Whether to cancel losing requests on first valid response
    #[serde(default = "default_cancel_on_first_valid")]
    pub cancel_on_first_valid: bool,
    /// Research notes
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

fn default_hedge_delay() -> u64 {
    100
}

fn default_max_hedges() -> u32 {
    1
}

fn default_cancel_on_first_valid() -> bool {
    true
}

impl Default for HedgeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            delay_ms: 100,
            max_hedges: 1,
            cancel_on_first_valid: true,
            notes: None,
        }
    }
}

/// Power of Two Choices selection configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct P2CConfig {
    /// Whether P2C selection is enabled
    pub enabled: bool,
    /// Number of providers to sample (typically 2)
    #[serde(default = "default_p2c_sample")]
    pub sample_count: usize,
    /// Research notes
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

fn default_p2c_sample() -> usize {
    2
}

impl Default for P2CConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_count: 2,
            notes: None,
        }
    }
}

/// Adaptive concurrency configuration (based on Netflix gradient-based approach)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdaptiveConcurrencyConfig {
    /// Whether adaptive concurrency is enabled
    pub enabled: bool,
    /// Gradient threshold below which to reduce concurrency
    #[serde(default = "default_gradient_threshold")]
    pub gradient_threshold: f64,
    /// Window size for latency measurement (milliseconds)
    #[serde(default = "default_window_size")]
    pub window_size_ms: u64,
    /// Research notes
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

fn default_gradient_threshold() -> f64 {
    0.8
}

fn default_window_size() -> u64 {
    1000
}

impl Default for AdaptiveConcurrencyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            gradient_threshold: 0.8,
            window_size_ms: 1000,
            notes: None,
        }
    }
}

/// Memory budget configuration (based on vLLM PagedAttention concepts)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryBudgetConfig {
    /// Maximum concurrent streams
    #[serde(default = "default_max_concurrent_streams")]
    pub max_concurrent_streams: u32,
    /// Maximum tokens per stream
    #[serde(default = "default_max_tokens_per_stream")]
    pub max_tokens_per_stream: u32,
    /// Research notes
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

fn default_max_concurrent_streams() -> u32 {
    10
}

fn default_max_tokens_per_stream() -> u32 {
    100_000
}

impl Default for MemoryBudgetConfig {
    fn default() -> Self {
        Self {
            max_concurrent_streams: 10,
            max_tokens_per_stream: 100_000,
            notes: None,
        }
    }
}

/// All provider cooldown profiles.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderProfiles {
    /// Per-provider cooldown profiles
    pub profiles: HashMap<String, ProviderCooldownProfile>,
    /// Default profile for unknown providers
    pub defaults: ProviderCooldownProfile,
    /// Parallel attempt configuration
    pub parallel: ParallelConfig,
}

impl ProviderProfiles {
    /// Load profiles from the embedded JSON or fallback to defaults.
    pub fn load() -> Self {
        // Try to load from embedded JSON first
        let json_str = include_str!("provider-cooldown-profiles.json");
        serde_json::from_str(json_str).unwrap_or_default()
    }

    /// Get the cooldown profile for a specific provider.
    pub fn profile_for(&self, provider_id: &str) -> &ProviderCooldownProfile {
        self.profiles.get(provider_id).unwrap_or(&self.defaults)
    }
}

/// Calculate exponential backoff with jitter.
///
/// # Arguments
/// * `base_secs` - Base cooldown in seconds
/// * `failure_count` - Number of consecutive failures
/// * `max_secs` - Maximum cooldown in seconds
///
/// # Returns
/// Cooldown duration with exponential backoff and ±20% jitter
fn calculate_exponential_backoff(base_secs: u64, failure_count: u32, max_secs: u64) -> u64 {
    let exponential = base_secs.saturating_mul(2u64.saturating_pow(failure_count.min(5)));
    let capped = exponential.min(max_secs);
    // Add ±20% jitter to prevent thundering herd
    let jitter_range = capped as f64 * 0.2;
    let jitter = (rand::random::<f64>() * jitter_range * 2.0) - jitter_range;
    ((capped as f64 + jitter).max(1.0)) as u64
}
