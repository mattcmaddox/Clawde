// providers/key_rotating.rs — Transparent key-rotation wrapper for LlmProvider.
//
// Wraps any `Arc<dyn LlmProvider>` with a `clawde_core::KeyRing` and
// automatically rotates to the next available API key when the active key
// is exhausted (quota exceeded, rate limited, auth failure).
//
// Key exhaustion is detected from `ProviderError` variants:
//   - `ProviderError::QuotaExceeded` — the free-tier quota for this key is
//     exhausted. Cooldown defaults to 3600 seconds (1 hour).
//   - `ProviderError::RateLimited` — the key hit a rate limit. Cooldown
//     defaults to 60 seconds.
//   - `ProviderError::AuthFailed` — the key was rejected (revoked/invalid).
//     Cooldown defaults to 0 (won't retry this key until next call).
//
// When ALL keys are exhausted, the last error is returned with `retry_after`
// populated from the earliest cooldown expiry.
//
// Thread safety: `KeyRing` is behind `Arc<Mutex<>>`. Mutex guards are never
// held across await points.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use clawde_core::key_ring::{KeyRing, KeyStatus};
use clawde_core::provider_id::ProviderId;
use futures::{Stream, StreamExt};

use crate::provider::{LlmProvider, ModelInfo};
use crate::provider_error::ProviderError;
use crate::provider_types::{
    ProviderCapabilities, ProviderRequest, ProviderResponse, ProviderStatus, RateLimitObservation,
    StreamEvent, SystemPromptStyle,
};
use crate::time_extract::estimate_cooldown;

// ---------------------------------------------------------------------------
// Error classification
// ---------------------------------------------------------------------------

/// Which `ProviderError` variants should trigger a key rotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExhaustSignal {
    Quota,
    RateLimit,
    Auth,
}

fn classify_exhaust(err: &ProviderError) -> Option<ExhaustSignal> {
    match err {
        ProviderError::QuotaExceeded { .. } => Some(ExhaustSignal::Quota),
        ProviderError::RateLimited { .. } => Some(ExhaustSignal::RateLimit),
        ProviderError::AuthFailed { .. } => Some(ExhaustSignal::Auth),
        // Provider adapaters may fall back to Other when they can't precisely
        // classify the error. Treat HTTP 429 as rate-limit and 401/403 as auth
        // so the key still gets rotated and the full response body (which
        // Other carries) is passed to the cooldown estimator.
        // HTTP 402 (Payment Required / insufficient credits) is intentionally
        // NOT classified — Cline's negative-balance 402 is a known display
        // issue that does not actually block free-model usage, so exhausting
        // the key would unnecessarily remove a working upstream from rotation.
        ProviderError::Other {
            status: Some(429), ..
        } => Some(ExhaustSignal::RateLimit),
        ProviderError::Other {
            status: Some(401 | 403),
            ..
        } => Some(ExhaustSignal::Auth),
        _ => None,
    }
}

/// Extract a message string from a `ProviderError` for cooldown estimation.
fn error_message(err: &ProviderError) -> &str {
    match err {
        ProviderError::QuotaExceeded { message, .. } => message.as_str(),
        ProviderError::RateLimited { .. } => "rate limited",
        ProviderError::AuthFailed { message, .. } => message.as_str(),
        ProviderError::ServerError { message, .. } => message.as_str(),
        ProviderError::Other { message, .. } => message.as_str(),
        _ => "",
    }
}

fn default_cooldown_for_signal(signal: ExhaustSignal) -> u64 {
    match signal {
        ExhaustSignal::Quota => 3600,
        ExhaustSignal::RateLimit => 60,
        ExhaustSignal::Auth => 300, // 5 min — key won't be retried immediately
    }
}

/// When ALL keys are exhausted and the shortest cooldown is at most this
/// many seconds, the loop will wait and retry instead of returning an error.
/// This lets short rate-limit cooldowns recover transparently without the
/// user seeing a failure. Longer cooldowns surface the error immediately.
///
/// When the provider is nested inside a [`FreeProvider`] fallback chain,
/// [`skip_recovery_loop`](Self::set_skip_recovery_loop) disables this path
/// entirely — the FreeProvider already handles retry at a higher level and
/// sleeping here just delays the overall fallback.
const MAX_COOLDOWN_WAIT: u64 = 10;

/// Maximum number of cooldown wait-retry cycles before giving up and
/// returning a `RateLimited` error to the caller. Each cycle re-reads
/// the shortest cooldown from the key ring, so if cooldowns change
/// (e.g. a different key is exhausted with a different duration), the
/// new value is used on the next wait.
const MAX_COOLDOWN_RETRIES: u32 = 3;

const KEY_CAPACITY_TTL_SECS: u64 = 15 * 60;

#[derive(Debug, Clone, Copy)]
struct KeyCapacityObservation {
    tokens_pct_used: Option<f32>,
    requests_pct_used: Option<f32>,
    retry_after_secs: Option<u64>,
    reset_at_unix: Option<u64>,
    observed_at_unix: u64,
}

impl KeyCapacityObservation {
    fn from_rate_limit(observation: RateLimitObservation) -> Option<Self> {
        let valid = |value: Option<f32>| {
            value.and_then(|value| {
                (value.is_finite() && (0.0..=1.0).contains(&value)).then_some(value)
            })
        };
        let tokens_pct_used = valid(observation.tokens_pct_used);
        let requests_pct_used = valid(observation.requests_pct_used);
        (tokens_pct_used.is_some()
            || requests_pct_used.is_some()
            || observation.retry_after_secs.is_some()
            || observation.reset_at_unix.is_some())
        .then_some(Self {
            tokens_pct_used,
            requests_pct_used,
            retry_after_secs: observation.retry_after_secs,
            reset_at_unix: observation.reset_at_unix,
            observed_at_unix: current_unix_secs(),
        })
    }

    fn utilization(self) -> f32 {
        self.tokens_pct_used
            .into_iter()
            .chain(self.requests_pct_used)
            .fold(0.0, f32::max)
    }

    fn is_fresh(self) -> bool {
        let now = current_unix_secs();
        now.saturating_sub(self.observed_at_unix) <= KEY_CAPACITY_TTL_SECS
            && self.reset_at_unix.is_none_or(|reset| reset > now)
            && self
                .retry_after_secs
                .is_none_or(|retry| now < self.observed_at_unix.saturating_add(retry))
    }
}

#[derive(Debug)]
struct KeyCapacityState {
    observations: Vec<Option<KeyCapacityObservation>>,
}

impl KeyCapacityState {
    fn new(key_count: usize) -> Self {
        Self {
            observations: vec![None; key_count],
        }
    }

    fn observe(&mut self, key_idx: usize, observation: RateLimitObservation) {
        if let Some(observation) = KeyCapacityObservation::from_rate_limit(observation) {
            if let Some(slot) = self.observations.get_mut(key_idx) {
                *slot = Some(observation);
            }
        }
    }

    fn rank(&self, key_idx: usize) -> u8 {
        let used = self
            .observations
            .get(key_idx)
            .copied()
            .flatten()
            .filter(|observation| observation.is_fresh())
            .map_or(0.0, KeyCapacityObservation::utilization);
        match used {
            used if used >= 0.95 => 3,
            used if used >= 0.80 => 2,
            used if used >= 0.60 => 1,
            _ => 0,
        }
    }
}

fn current_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// KeyRotatingProvider
// ---------------------------------------------------------------------------

/// Factory function type that builds a provider instance from an API key.
pub type ProviderFactory = Arc<dyn Fn(&str) -> Arc<dyn LlmProvider> + Send + Sync>;

/// Wraps API keys with automatic rotation on exhaustion.
///
/// On each request, the next available key is fetched from the [`KeyRing`],
/// a fresh provider is built for that key, and the request is dispatched.
/// If it fails with an exhaustible error (`QuotaExceeded`, `RateLimited`,
/// `AuthFailed`), the key is marked exhausted, and the next key is tried.
/// Continues until a request succeeds or all keys are exhausted.
pub struct KeyRotatingProvider {
    provider_id: ProviderId,
    provider_name: String,
    ring: Arc<Mutex<KeyRing>>,
    key_capacity: Arc<Mutex<KeyCapacityState>>,
    build_provider: ProviderFactory,
    /// Path to persisted cooldown state file. `None` = no persistence.
    state_path: Option<PathBuf>,
    /// When `true`, the cooldown sleep+retry loop is skipped: on exhaustion
    /// the error is returned immediately instead of sleeping and retrying.
    /// Set by the [`FreeProvider`] chain builder so that individual upstream
    /// providers don't waste time waiting for cooldowns — the FreeProvider
    /// handles fallback at a higher level.
    skip_recovery_loop: bool,
}

impl KeyRotatingProvider {
    /// Create a new key-rotating provider without disk persistence.
    /// All keys start in the active (usable) state.
    ///
    /// For persistence across restarts, use
    /// [`new_with_persistence`](Self::new_with_persistence).
    pub fn new(
        provider_id: impl Into<String>,
        provider_name: impl Into<String>,
        keys: Vec<String>,
        build_provider: impl Fn(&str) -> Arc<dyn LlmProvider> + Send + Sync + 'static,
    ) -> Self {
        let pid = provider_id.into();
        let key_count = keys.len();
        Self {
            provider_id: ProviderId::new(&pid),
            provider_name: provider_name.into(),
            ring: Arc::new(Mutex::new(KeyRing::new(pid, keys))),
            key_capacity: Arc::new(Mutex::new(KeyCapacityState::new(key_count))),
            build_provider: Arc::new(build_provider),
            state_path: None,
            skip_recovery_loop: false,
        }
    }

    /// Create a key-rotating provider with persisted cooldown state.
    ///
    /// Restores previously-saved cooldowns from
    /// `{clawde_home}/key-ring-state/{provider_id}.json` so that a long
    /// cooldown (e.g. 12-hour quota reset) survives an app restart. Saves
    /// updated cooldown state after each key exhaustion.
    pub fn new_with_persistence(
        provider_id: impl Into<String>,
        provider_name: impl Into<String>,
        keys: Vec<String>,
        build_provider: impl Fn(&str) -> Arc<dyn LlmProvider> + Send + Sync + 'static,
    ) -> Self {
        let pid: String = provider_id.into();
        let key_count = keys.len();
        let state_path = KeyRing::default_state_path(&pid);
        let ring = Arc::new(Mutex::new(KeyRing::new(pid.clone(), keys)));
        // Restore persisted cooldown state so that a 12-hour cooldown
        // doesn't reset just because the user restarted the app.
        ring.lock().unwrap().load_from_file(&state_path);

        Self {
            provider_id: ProviderId::new(&pid),
            provider_name: provider_name.into(),
            ring,
            key_capacity: Arc::new(Mutex::new(KeyCapacityState::new(key_count))),
            build_provider: Arc::new(build_provider),
            state_path: Some(state_path),
            skip_recovery_loop: false,
        }
    }

    /// Snapshot of key ring statuses.
    pub fn key_statuses(&self) -> Vec<KeyStatus> {
        self.ring.lock().unwrap().statuses()
    }

    /// Number of active (non-exhausted) keys. Poison-safe: a poisoned lock
    /// reports zero rather than panicking, so status aggregation never crashes
    /// the query path.
    pub fn active_key_count(&self) -> usize {
        self.ring.lock().map(|r| r.active_count()).unwrap_or(0)
    }

    /// Number of exhausted keys. Poison-safe, see [`Self::active_key_count`].
    pub fn exhausted_key_count(&self) -> usize {
        self.ring.lock().map(|r| r.exhausted_count()).unwrap_or(0)
    }

    /// Disable the cooldown recovery loop. When set, exhaustion returns an
    /// error immediately instead of sleeping and retrying. Call this when the
    /// provider is nested inside a higher-level fallback chain (e.g.
    /// [`FreeProvider`]) that handles retry at its own level.
    pub fn set_skip_recovery_loop(&mut self, skip: bool) {
        self.skip_recovery_loop = skip;
    }

    /// Reference to the key ring (for inspection).
    pub fn ring(&self) -> &Arc<Mutex<KeyRing>> {
        &self.ring
    }

    // -----------------------------------------------------------------------
    // Core retry loop
    // -----------------------------------------------------------------------

    fn next_available_provider(&self) -> Option<(usize, Arc<dyn LlmProvider>)> {
        let ranks = self
            .key_capacity
            .lock()
            .map(|state| {
                (0..state.observations.len())
                    .map(|idx| state.rank(idx))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut ring = self.ring.lock().ok()?;
        ring.next_available_by(|idx| ranks.get(idx).copied().unwrap_or(0))
            .map(|(idx, key)| (idx, (self.build_provider)(key)))
    }

    /// Get the next available key, build a provider, and call `try_provider`.
    /// On exhaustible errors, marks the key and loops. On non-exhaustible
    /// errors, returns immediately. When all keys are exhausted, returns
    /// the last exhaustible provider error, preserving quota/credits and auth
    /// failures instead of masking them as a synthetic rate limit.
    async fn try_with_rotation<F, Fut, T>(&self, try_provider: F) -> Result<T, ProviderError>
    where
        F: Fn(usize, Arc<dyn LlmProvider>) -> Fut,
        Fut: std::future::Future<Output = Result<T, ProviderError>>,
    {
        let mut retry_count: u32 = 0;
        let mut last_exhaustible_error: Option<ProviderError> = None;

        loop {
            // Get the next available key (lock scope ends before any .await).
            let provider = self.next_available_provider();

            let (active_idx, provider) = match provider {
                Some(selection) => selection,
                None => {
                    // All keys exhausted. Read the current shortest cooldown
                    // fresh from the key ring each cycle (cooldowns can change
                    // between cycles if different keys are exhausted with
                    // different durations by concurrent requests).
                    let (should_wait, retry_secs) = {
                        let ring = self.ring.lock().unwrap();
                        if ring.is_empty() {
                            (false, 60)
                        } else {
                            let s = ring.earliest_retry_secs().unwrap_or(60);
                            (s <= MAX_COOLDOWN_WAIT, s)
                        }
                    };

                    if should_wait && retry_count < MAX_COOLDOWN_RETRIES && !self.skip_recovery_loop
                    {
                        retry_count += 1;
                        tracing::info!(
                            "KeyRotatingProvider: all keys exhausted, \
                             waiting {}s for cooldown (retry {}/{})",
                            retry_secs,
                            retry_count,
                            MAX_COOLDOWN_RETRIES,
                        );
                        tokio::time::sleep(Duration::from_secs(retry_secs)).await;
                        continue;
                    }

                    if let Some(last_error) = last_exhaustible_error.take() {
                        // Do not turn a pool of rejected/creditless keys into
                        // a misleading synthetic rate limit. The caller needs
                        // the actual terminal class to decide whether to
                        // reauthenticate, add credits, or wait for a quota
                        // reset. Genuine RateLimited errors retain the
                        // earliest cooldown hint below.
                        match last_error {
                            ProviderError::RateLimited { .. } => {
                                return Err(ProviderError::RateLimited {
                                    provider: self.provider_id.clone(),
                                    retry_after: Some(retry_secs),
                                });
                            }
                            other => return Err(other),
                        }
                    }
                    return Err(ProviderError::RateLimited {
                        provider: self.provider_id.clone(),
                        retry_after: Some(retry_secs),
                    });
                }
            };

            match try_provider(active_idx, provider).await {
                Ok(response) => return Ok(response),
                Err(err) => {
                    let Some(signal) = classify_exhaust(&err) else {
                        return Err(err);
                    };
                    last_exhaustible_error = Some(err.clone());

                    // Mark the active key as exhausted (brief lock).
                    {
                        let mut ring = self.ring.lock().unwrap();
                        let msg = error_message(&err);
                        let cooldown = default_cooldown_for_signal(signal);

                        // If the provider adapter already parsed a retry_after
                        // value from the HTTP response (e.g. RateLimited carries
                        // it), use that directly. Otherwise fall back to body
                        // text parsing, then the default for the signal type.
                        let retry_from_error = match &err {
                            ProviderError::RateLimited { retry_after, .. } => *retry_after,
                            _ => None,
                        };

                        // Extract raw response body from ProviderError::Other
                        // for better cooldown text matching (the error message
                        // itself is often shorter/less detailed than the full
                        // response body).
                        let body_from_error = match &err {
                            ProviderError::Other { body, .. } => body.as_deref(),
                            _ => None,
                        };

                        let final_cooldown = retry_from_error.unwrap_or_else(|| {
                            let extracted = estimate_cooldown(None, msg, body_from_error);
                            extracted.or_secs(cooldown)
                        });

                        // Mark the exact slot selected before the await. A
                        // concurrent request may have advanced or changed the
                        // ring while this provider call was in flight; picking
                        // the first currently-active slot would exhaust the
                        // wrong credential.
                        ring.mark_exhausted(active_idx, final_cooldown, Some(msg.to_string()));

                        // Persist cooldown state immediately so a 12-hour
                        // cooldown survives an app restart 10 hours in.
                        if let Some(ref persist_path) = self.state_path {
                            ring.save_to_file(persist_path);
                        }

                        tracing::info!(
                            "KeyRotatingProvider: key exhausted ({}s cooldown), \
                             {}/{} active for {}",
                            final_cooldown,
                            ring.active_count(),
                            ring.len(),
                            self.provider_id,
                        );
                    }
                }
            }
        }
    }
}

#[async_trait]
impl LlmProvider for KeyRotatingProvider {
    fn id(&self) -> &ProviderId {
        &self.provider_id
    }

    fn name(&self) -> &str {
        &self.provider_name
    }

    async fn create_message(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        let key_capacity = Arc::clone(&self.key_capacity);
        self.try_with_rotation(|key_idx, provider| {
            let req = request.clone();
            let key_capacity = Arc::clone(&key_capacity);
            async move {
                let mut response = provider.create_message(req).await?;
                if let Some(mut observation) = response.rate_limit {
                    observation.key_idx = Some(key_idx);
                    if let Ok(mut state) = key_capacity.lock() {
                        state.observe(key_idx, observation);
                    }
                    response.rate_limit = Some(observation);
                }
                Ok(response)
            }
        })
        .await
    }

    async fn create_message_stream(
        &self,
        request: ProviderRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        let key_capacity = Arc::clone(&self.key_capacity);
        self.try_with_rotation(|key_idx, provider| {
            let req = request.clone();
            let key_capacity = Arc::clone(&key_capacity);
            async move {
                let stream = provider.create_message_stream(req).await?;
                let stream = stream.map(move |result| {
                    result.map(|event| match event {
                        StreamEvent::RateLimitHeaders {
                            provider_id,
                            tokens_pct_used,
                            requests_pct_used,
                            retry_after_secs,
                            reset_at_unix,
                            ..
                        } => {
                            let observation = RateLimitObservation {
                                key_idx: Some(key_idx),
                                tokens_pct_used: Some(tokens_pct_used),
                                requests_pct_used: Some(requests_pct_used),
                                retry_after_secs,
                                reset_at_unix,
                            };
                            if let Ok(mut state) = key_capacity.lock() {
                                state.observe(key_idx, observation);
                            }
                            StreamEvent::RateLimitHeaders {
                                provider_id,
                                tokens_pct_used,
                                requests_pct_used,
                                retry_after_secs,
                                reset_at_unix,
                                key_idx: Some(key_idx),
                            }
                        }
                        other => other,
                    })
                });
                Ok(Box::pin(stream)
                    as Pin<
                        Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>,
                    >)
            }
        })
        .await
    }

    async fn discover_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let key = {
            let mut ring = self.ring.lock().unwrap();
            ring.next_available().map(|(_, k)| k.to_string())
        };
        match key {
            Some(k) => {
                let provider = (self.build_provider)(&k);
                provider.discover_models().await
            }
            None => Ok(Vec::new()),
        }
    }

    async fn health_check(&self) -> Result<ProviderStatus, ProviderError> {
        // Snapshot each currently-available key once. `next_available()`
        // round-robins and never removes a key on a health-check failure, so
        // looping until it returns None would cycle forever when all keys are
        // reachable but unhealthy.
        let keys = match self.ring.lock() {
            Ok(mut ring) => {
                let active = ring.active_count();
                let mut keys = Vec::with_capacity(active);
                for _ in 0..active {
                    if let Some((_, key)) = ring.next_available() {
                        keys.push(key.to_string());
                    } else {
                        break;
                    }
                }
                keys
            }
            Err(_) => Vec::new(),
        };
        let mut last_status: Result<ProviderStatus, ProviderError> =
            Ok(ProviderStatus::Unavailable {
                reason: "no keys configured".to_string(),
            });
        for key in keys {
            let provider = (self.build_provider)(&key);
            match provider.health_check().await {
                Ok(ProviderStatus::Healthy) => return Ok(ProviderStatus::Healthy),
                Ok(other) => last_status = Ok(other),
                Err(error) => last_status = Err(error),
            }
        }
        last_status
    }

    fn key_ring_status(&self) -> Option<(usize, usize, Option<u64>)> {
        if let Ok(mut ring) = self.ring.lock() {
            ring.prune_expired();
        } else {
            return None;
        }
        let active = self.active_key_count();
        let exhausted = self.exhausted_key_count();
        let retry = self.ring.lock().ok()?.earliest_retry_secs();
        Some((active, active + exhausted, retry))
    }

    fn mark_key_healthy(&self, _upstream_id: Option<&str>, key_idx: usize) -> bool {
        let ok = match self.ring.lock() {
            Ok(mut ring) => ring.mark_healthy(key_idx),
            Err(_) => false,
        };
        // Persist the cleared cooldown so a restart doesn't resurrect stale
        // exhaustion state from a previous session. Without this the health
        // poller clears the key in memory every startup probe, but the disk
        // file still carries the original cooldown and re-applies it on the
        // next launch (the remaining-seconds value is relative to NOW, not
        // to when the original exhaustion happened).
        if ok {
            if let Some(ref persist_path) = self.state_path {
                if let Ok(ring) = self.ring.lock() {
                    ring.save_to_file(persist_path);
                }
            }
        }
        ok
    }

    fn mark_key_exhausted(
        &self,
        _upstream_id: Option<&str>,
        key_idx: usize,
        cooldown_secs: u64,
        reason: Option<String>,
    ) -> bool {
        match self.ring.lock() {
            Ok(mut ring) => {
                let changed = ring.mark_exhausted(key_idx, cooldown_secs, reason);
                if changed {
                    if let Some(ref persist_path) = self.state_path {
                        ring.save_to_file(persist_path);
                    }
                }
                changed
            }
            Err(_) => false,
        }
    }

    fn capabilities(&self) -> ProviderCapabilities {
        let key = {
            let mut ring = self.ring.lock().unwrap();
            ring.next_available().map(|(_, k)| k.to_string())
        };
        match key {
            Some(k) => {
                let provider = (self.build_provider)(&k);
                provider.capabilities()
            }
            None => ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                thinking: false,
                image_input: false,
                pdf_input: false,
                audio_input: false,
                video_input: false,
                caching: false,
                structured_output: false,
                system_prompt_style: SystemPromptStyle::SystemMessage,
            },
        }
    }

    fn tool_calling_for(&self, model: &str) -> Option<bool> {
        let key = {
            let mut ring = self.ring.lock().unwrap();
            ring.next_available().map(|(_, k)| k.to_string())
        };
        key.and_then(|k| {
            let provider = (self.build_provider)(&k);
            provider.tool_calling_for(model)
        })
    }

    fn max_tokens_cap_for(&self, model: &str) -> Option<u32> {
        let key = {
            let mut ring = self.ring.lock().unwrap();
            ring.next_available().map(|(_, k)| k.to_string())
        };
        key.and_then(|k| {
            let provider = (self.build_provider)(&k);
            provider.max_tokens_cap_for(model)
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_types::{RateLimitObservation, StopReason};
    use clawde_core::types::{Message, UsageInfo};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A mock provider that can be configured to fail or succeed.
    struct MockProvider {
        id: ProviderId,
        name: String,
        fail_with: Option<ProviderError>,
        call_count: Arc<AtomicUsize>,
        delay: Duration,
        rate_limit: Option<RateLimitObservation>,
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        fn id(&self) -> &ProviderId {
            &self.id
        }
        fn name(&self) -> &str {
            &self.name
        }

        async fn create_message(
            &self,
            request: ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            if let Some(ref err) = self.fail_with {
                return Err(err.clone());
            }
            Ok(ProviderResponse {
                id: "mock".into(),
                model: request.model,
                content: vec![],
                stop_reason: StopReason::EndTurn,
                usage: UsageInfo::default(),
                rate_limit: self.rate_limit,
            })
        }

        async fn create_message_stream(
            &self,
            _request: ProviderRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            if let Some(ref err) = self.fail_with {
                return Err(err.clone());
            }
            let events = self
                .rate_limit
                .map(|observation| {
                    vec![Ok(StreamEvent::RateLimitHeaders {
                        provider_id: "mock".into(),
                        tokens_pct_used: observation.tokens_pct_used.unwrap_or(0.0),
                        requests_pct_used: observation.requests_pct_used.unwrap_or(0.0),
                        retry_after_secs: observation.retry_after_secs,
                        reset_at_unix: observation.reset_at_unix,
                        key_idx: observation.key_idx,
                    })]
                })
                .unwrap_or_default();
            let stream = futures::stream::iter(events);
            Ok(Box::pin(stream))
        }

        async fn health_check(&self) -> Result<ProviderStatus, ProviderError> {
            if self.fail_with.is_some() {
                Ok(ProviderStatus::Unavailable {
                    reason: "mock fail".into(),
                })
            } else {
                Ok(ProviderStatus::Healthy)
            }
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_calling: true,
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

    fn dummy_request() -> ProviderRequest {
        ProviderRequest {
            model: "test-model".into(),
            messages: vec![Message::user("hello")],
            system_prompt: None,
            tools: vec![],
            max_tokens: 100,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: vec![],
            thinking: None,
            effort_level: None,
            provider_options: serde_json::Value::Null,
        }
    }

    fn build_mock_provider(
        key: &str,
        fail: Option<ProviderError>,
        counters: &Arc<Vec<Arc<AtomicUsize>>>,
    ) -> Arc<dyn LlmProvider> {
        build_mock_provider_with_observation(key, fail, counters, None)
    }

    fn build_mock_provider_with_observation(
        key: &str,
        fail: Option<ProviderError>,
        counters: &Arc<Vec<Arc<AtomicUsize>>>,
        rate_limit: Option<RateLimitObservation>,
    ) -> Arc<dyn LlmProvider> {
        let idx: usize = key
            .chars()
            .last()
            .and_then(|c| c.to_digit(10))
            .map(|d| d as usize)
            .unwrap_or(0);
        let counter = if idx < counters.len() {
            counters[idx].clone()
        } else {
            Arc::new(AtomicUsize::new(0))
        };
        Arc::new(MockProvider {
            id: ProviderId::new("mock"),
            name: format!("mock-{}", key),
            fail_with: fail,
            call_count: counter,
            delay: Duration::ZERO,
            rate_limit,
        })
    }

    #[tokio::test]
    async fn single_key_success() {
        let counters = Arc::new(vec![Arc::new(AtomicUsize::new(0))]);
        let build = {
            let c = counters.clone();
            move |key: &str| build_mock_provider(key, None, &c)
        };

        let provider = KeyRotatingProvider::new("mock", "Mock", vec!["key0".into()], build);

        let result = provider.create_message(dummy_request()).await;
        assert!(result.is_ok());
        assert_eq!(counters[0].load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn key_slot_attribution_is_added_to_completed_and_streaming_metadata() {
        let counters = Arc::new(vec![
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        ]);
        let observation = RateLimitObservation {
            key_idx: None,
            tokens_pct_used: Some(0.91),
            requests_pct_used: None,
            retry_after_secs: Some(7),
            reset_at_unix: None,
        };
        let build = {
            let counters = counters.clone();
            move |key: &str| {
                build_mock_provider_with_observation(key, None, &counters, Some(observation))
            }
        };
        let provider =
            KeyRotatingProvider::new("mock", "Mock", vec!["key0".into(), "key1".into()], build);

        let response = provider
            .create_message(dummy_request())
            .await
            .expect("completed response");
        assert_eq!(response.rate_limit.and_then(|value| value.key_idx), Some(0));

        let mut stream = provider
            .create_message_stream(dummy_request())
            .await
            .expect("stream response");
        let event = stream
            .next()
            .await
            .expect("rate-limit event")
            .expect("successful event");
        assert!(matches!(
            event,
            StreamEvent::RateLimitHeaders {
                key_idx: Some(1),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn key_capacity_demotes_recently_used_key_without_starving_it() {
        let counters = Arc::new(vec![
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        ]);
        let high = RateLimitObservation {
            key_idx: None,
            tokens_pct_used: Some(0.96),
            requests_pct_used: None,
            retry_after_secs: None,
            reset_at_unix: None,
        };
        let low = RateLimitObservation {
            key_idx: None,
            tokens_pct_used: Some(0.10),
            requests_pct_used: None,
            retry_after_secs: None,
            reset_at_unix: None,
        };
        let build = {
            let counters = counters.clone();
            move |key: &str| {
                let observation = if key == "key0" { high } else { low };
                build_mock_provider_with_observation(key, None, &counters, Some(observation))
            }
        };
        let provider =
            KeyRotatingProvider::new("mock", "Mock", vec!["key0".into(), "key1".into()], build);

        // First call records key0 as critically utilized. The next two calls
        // should prefer key1, but key0 remains eligible rather than skipped.
        provider
            .create_message(dummy_request())
            .await
            .expect("first response");
        provider
            .create_message(dummy_request())
            .await
            .expect("second response");
        provider
            .create_message(dummy_request())
            .await
            .expect("third response");

        assert_eq!(counters[0].load(Ordering::SeqCst), 1);
        assert_eq!(counters[1].load(Ordering::SeqCst), 2);
    }

    #[test]
    fn key_capacity_reset_expiry_restores_normal_rank() {
        let mut state = KeyCapacityState::new(1);
        state.observe(
            0,
            RateLimitObservation {
                key_idx: Some(0),
                tokens_pct_used: Some(0.99),
                requests_pct_used: None,
                retry_after_secs: None,
                reset_at_unix: Some(current_unix_secs().saturating_sub(1)),
            },
        );
        assert_eq!(state.rank(0), 0, "expired reset must not demote the key");
    }

    #[tokio::test]
    async fn rotates_on_quota_exceeded() {
        let counters = Arc::new(vec![
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        ]);

        let key1_fail = ProviderError::QuotaExceeded {
            provider: ProviderId::new("mock"),
            message: "quota exceeded".into(),
        };

        let build = {
            let c = counters.clone();
            move |key: &str| {
                let fail = if key == "key0" {
                    Some(key1_fail.clone())
                } else {
                    None
                };
                build_mock_provider(key, fail, &c)
            }
        };

        let provider =
            KeyRotatingProvider::new("mock", "Mock", vec!["key0".into(), "key1".into()], build);

        let result = provider.create_message(dummy_request()).await;
        assert!(result.is_ok(), "should succeed on key1 after key0 fails");
        assert_eq!(counters[0].load(Ordering::SeqCst), 1);
        assert_eq!(counters[1].load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn all_keys_exhausted_returns_error() {
        let counters = Arc::new(vec![
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        ]);

        let fail_err = ProviderError::QuotaExceeded {
            provider: ProviderId::new("mock"),
            message: "out of quota".into(),
        };

        let build = {
            let c = counters.clone();
            move |_key: &str| build_mock_provider("key", Some(fail_err.clone()), &c)
        };

        let provider =
            KeyRotatingProvider::new("mock", "Mock", vec!["key0".into(), "key1".into()], build);

        let result = provider.create_message(dummy_request()).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ProviderError::QuotaExceeded { message, .. } => {
                assert_eq!(message, "out of quota");
            }
            other => panic!("expected QuotaExceeded, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn non_exhaustible_error_passthrough() {
        let counters = Arc::new(vec![Arc::new(AtomicUsize::new(0))]);

        let fail_err = ProviderError::InvalidRequest {
            provider: ProviderId::new("mock"),
            message: "bad request".into(),
        };

        let build = {
            let c = counters.clone();
            move |_key: &str| build_mock_provider("key", Some(fail_err.clone()), &c)
        };

        let provider = KeyRotatingProvider::new("mock", "Mock", vec!["key1".into()], build);

        let result = provider.create_message(dummy_request()).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ProviderError::InvalidRequest { .. }
        ));
    }

    #[tokio::test]
    async fn empty_key_ring_returns_error() {
        let build = |_: &str| -> Arc<dyn LlmProvider> { unreachable!() };

        let provider = KeyRotatingProvider::new("mock", "Mock", vec![] as Vec<String>, build);

        let result = provider.create_message(dummy_request()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn key_statuses_reflect_state() {
        let counters = Arc::new(vec![
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        ]);

        let fail_err = ProviderError::QuotaExceeded {
            provider: ProviderId::new("mock"),
            message: "quota".into(),
        };

        let build = {
            let c = counters.clone();
            move |key: &str| {
                let fail = if key == "key0" {
                    Some(fail_err.clone())
                } else {
                    None
                };
                build_mock_provider(key, fail, &c)
            }
        };

        let provider =
            KeyRotatingProvider::new("mock", "Mock", vec!["key0".into(), "key1".into()], build);

        let _ = provider.create_message(dummy_request()).await;

        let statuses = provider.key_statuses();
        assert_eq!(statuses.len(), 2);
        assert!(!statuses[0].active, "key0 should be exhausted");
        assert_eq!(statuses[0].last_error.as_deref(), Some("quota"));
        assert!(statuses[1].active, "key1 should be active");
    }

    #[tokio::test]
    async fn stream_rotates_on_connection_error() {
        let counters = Arc::new(vec![
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        ]);

        let fail_err = ProviderError::QuotaExceeded {
            provider: ProviderId::new("mock"),
            message: "quota exceeded".into(),
        };

        let build = {
            let c = counters.clone();
            move |key: &str| {
                let fail = if key == "key0" {
                    Some(fail_err.clone())
                } else {
                    None
                };
                build_mock_provider(key, fail, &c)
            }
        };

        let provider =
            KeyRotatingProvider::new("mock", "Mock", vec!["key0".into(), "key1".into()], build);

        let result = provider.create_message_stream(dummy_request()).await;
        assert!(result.is_ok(), "stream should succeed on key1");
        assert_eq!(counters[0].load(Ordering::SeqCst), 1);
        assert_eq!(counters[1].load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn health_check_terminates_when_all_active_keys_are_unhealthy() {
        let counters = Arc::new(vec![
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        ]);
        let fail = ProviderError::Other {
            provider: ProviderId::new("mock"),
            message: "unavailable".into(),
            status: None,
            body: None,
        };
        let build = {
            let counters = counters.clone();
            move |key: &str| build_mock_provider(key, Some(fail.clone()), &counters)
        };
        let provider =
            KeyRotatingProvider::new("mock", "Mock", vec!["key0".into(), "key1".into()], build);
        let result =
            tokio::time::timeout(Duration::from_millis(100), provider.health_check()).await;
        assert!(result.is_ok(), "health check must make one bounded pass");
        assert!(
            result.unwrap().is_ok(),
            "unhealthy status is still a completed check"
        );
    }

    #[tokio::test]
    async fn health_poll_exhaustion_persists_across_restart() {
        let _home = crate::test_support::TestHome::new();
        let counters = Arc::new(vec![Arc::new(AtomicUsize::new(0))]);
        let build = {
            let counters = counters.clone();
            move |key: &str| build_mock_provider(key, None, &counters)
        };
        let provider = KeyRotatingProvider::new_with_persistence(
            "mock-health",
            "Mock",
            vec!["key0".into()],
            build,
        );
        assert!(provider.mark_key_exhausted(
            None,
            0,
            60,
            Some("Invalid API key (HTTP 401)".into())
        ));
        let restored = KeyRotatingProvider::new_with_persistence(
            "mock-health",
            "Mock",
            vec!["key0".into()],
            {
                let counters = counters.clone();
                move |key: &str| build_mock_provider(key, None, &counters)
            },
        );
        let statuses = restored.key_statuses();
        assert!(!statuses[0].active);
        assert_eq!(
            statuses[0].last_error.as_deref(),
            Some("Invalid API key (HTTP 401)")
        );
    }

    #[tokio::test]
    async fn health_check_skips_exhausted_keys() {
        let counters = Arc::new(vec![
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        ]);

        let fail_err = ProviderError::QuotaExceeded {
            provider: ProviderId::new("mock"),
            message: "quota".into(),
        };

        let build = {
            let c = counters.clone();
            move |key: &str| {
                let fail = if key == "key0" {
                    Some(fail_err.clone())
                } else {
                    None
                };
                build_mock_provider(key, fail, &c)
            }
        };

        let provider =
            KeyRotatingProvider::new("mock", "Mock", vec!["key0".into(), "key1".into()], build);

        // Exhaust key0
        let _ = provider.create_message(dummy_request()).await;

        // Health check should succeed on key1
        let status = provider.health_check().await;
        assert!(matches!(status, Ok(ProviderStatus::Healthy)));
    }

    // -----------------------------------------------------------------------
    // Integration tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn other_with_429_classified_as_rate_limit() {
        // ProviderError::Other with HTTP 429 should trigger rotation
        // (treated as rate-limit) and pass the body to cooldown estimator.
        let counters = Arc::new(vec![
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        ]);

        let body_err = "{\"error\":{\"message\":\"rate limit exceeded\",\"retry_after\":120}}";
        let key0_err = ProviderError::Other {
            provider: ProviderId::new("mock"),
            message: "rate limit exceeded".into(),
            status: Some(429),
            body: Some(body_err.to_string()),
        };

        let build = {
            let c = counters.clone();
            move |key: &str| {
                let fail = if key == "key0" {
                    Some(key0_err.clone())
                } else {
                    None
                };
                build_mock_provider(key, fail, &c)
            }
        };

        let provider =
            KeyRotatingProvider::new("mock", "Mock", vec!["key0".into(), "key1".into()], build);

        let result = provider.create_message(dummy_request()).await;
        assert!(
            result.is_ok(),
            "should succeed on key1 after key0 fails with 429"
        );
        assert_eq!(counters[0].load(Ordering::SeqCst), 1);
        assert_eq!(counters[1].load(Ordering::SeqCst), 1);

        let statuses = provider.key_statuses();
        assert!(!statuses[0].active, "key0 should be exhausted");
        assert!(statuses[1].active, "key1 should be active");
    }

    #[tokio::test]
    async fn other_with_401_classified_as_auth() {
        // ProviderError::Other with HTTP 401 should trigger rotation
        // (treated as auth failure).
        let counters = Arc::new(vec![
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        ]);

        let key0_err = ProviderError::Other {
            provider: ProviderId::new("mock"),
            message: "unauthorized".into(),
            status: Some(401),
            body: None,
        };

        let build = {
            let c = counters.clone();
            move |key: &str| {
                let fail = if key == "key0" {
                    Some(key0_err.clone())
                } else {
                    None
                };
                build_mock_provider(key, fail, &c)
            }
        };

        let provider =
            KeyRotatingProvider::new("mock", "Mock", vec!["key0".into(), "key1".into()], build);

        let result = provider.create_message(dummy_request()).await;
        assert!(
            result.is_ok(),
            "should succeed on key1 after key0 fails with 401"
        );

        let statuses = provider.key_statuses();
        assert!(!statuses[0].active, "key0 should be exhausted");
        assert!(statuses[1].active, "key1 should be active");
    }

    #[tokio::test]
    async fn other_without_http_status_not_rotated() {
        // ProviderError::Other without an HTTP status code should NOT
        // trigger rotation — it might be a non-exhaust error.
        let counters = Arc::new(vec![Arc::new(AtomicUsize::new(0))]);

        let key0_err = ProviderError::Other {
            provider: ProviderId::new("mock"),
            message: "unknown error".into(),
            status: None,
            body: None,
        };

        let build = {
            let c = counters.clone();
            move |key: &str| {
                let fail = if key == "key0" {
                    Some(key0_err.clone())
                } else {
                    None
                };
                build_mock_provider(key, fail, &c)
            }
        };

        let provider = KeyRotatingProvider::new("mock", "Mock", vec!["key0".into()], build);

        // Should fail without rotating (single key, but Other without status
        // is not an exhaust signal).
        let result = provider.create_message(dummy_request()).await;
        assert!(result.is_err(), "Other without status should not rotate");
        // Key should still be active (not marked exhausted).
        assert_eq!(counters[0].load(Ordering::SeqCst), 1);
        let statuses = provider.key_statuses();
        assert!(statuses[0].active, "key should still be active");
    }

    fn build_delayed_mock_provider(
        key: &str,
        fail: Option<ProviderError>,
        counters: &Arc<Vec<Arc<AtomicUsize>>>,
        delay: Duration,
    ) -> Arc<dyn LlmProvider> {
        let idx = key
            .chars()
            .last()
            .and_then(|c| c.to_digit(10))
            .map(|d| d as usize)
            .unwrap_or(0);
        let counter = counters
            .get(idx)
            .cloned()
            .unwrap_or_else(|| Arc::new(AtomicUsize::new(0)));
        Arc::new(MockProvider {
            id: ProviderId::new("mock"),
            name: format!("mock-{}", key),
            fail_with: fail,
            call_count: counter,
            delay,
            rate_limit: None,
        })
    }

    #[tokio::test]
    async fn concurrent_failures_mark_the_selected_key() {
        // key0 is selected first but fails slowly; key1 is selected second
        // and fails immediately. The error reason must remain attached to the
        // slot that actually served it, not whichever slot is first active
        // when the error arrives.
        let counters = Arc::new(vec![
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        ]);
        let build = {
            let counters = counters.clone();
            move |key: &str| {
                let (error, delay) = if key == "key0" {
                    (
                        ProviderError::QuotaExceeded {
                            provider: ProviderId::new("mock"),
                            message: "key0 quota".into(),
                        },
                        Duration::from_millis(40),
                    )
                } else {
                    (
                        ProviderError::AuthFailed {
                            provider: ProviderId::new("mock"),
                            message: "key1 invalid".into(),
                        },
                        Duration::ZERO,
                    )
                };
                build_delayed_mock_provider(key, Some(error), &counters, delay)
            }
        };
        let provider = Arc::new(KeyRotatingProvider::new(
            "mock",
            "Mock",
            vec!["key0".into(), "key1".into()],
            build,
        ));

        let first = {
            let provider = provider.clone();
            tokio::spawn(async move { provider.create_message(dummy_request()).await })
        };
        tokio::time::sleep(Duration::from_millis(5)).await;
        let second = {
            let provider = provider.clone();
            tokio::spawn(async move { provider.create_message(dummy_request()).await })
        };
        let _ = tokio::join!(first, second);

        let statuses = provider.key_statuses();
        assert_eq!(statuses[0].last_error.as_deref(), Some("key0 quota"));
        assert_eq!(statuses[1].last_error.as_deref(), Some("key1 invalid"));
    }

    #[tokio::test]
    async fn concurrent_requests_dont_corrupt_key_ring() {
        // Spawn 10 concurrent requests against a provider with keys where
        // key0 and key1 are exhausted, and key2 succeeds. Verify that all
        // requests succeed and the key ring state is consistent.
        let counters = Arc::new(vec![
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        ]);

        let fail_err = ProviderError::QuotaExceeded {
            provider: ProviderId::new("mock"),
            message: "quota".into(),
        };

        let build = {
            let c = counters.clone();
            move |key: &str| {
                let fail = match key {
                    "key0" | "key1" => Some(fail_err.clone()),
                    _ => None,
                };
                build_mock_provider(key, fail, &c)
            }
        };

        let provider = Arc::new(KeyRotatingProvider::new(
            "mock",
            "Mock",
            vec!["key0".into(), "key1".into(), "key2".into()],
            build,
        ));

        // Spawn 10 concurrent requests.
        let mut handles = Vec::new();
        for _ in 0..10 {
            let p = provider.clone();
            handles.push(tokio::spawn(async move {
                p.create_message(dummy_request()).await
            }));
        }

        // All should succeed (key0 and key1 are exhausted on first use,
        // key2 handles all subsequent requests).
        for handle in handles {
            let result = handle.await.expect("join");
            assert!(result.is_ok(), "concurrent request should succeed");
        }

        // After 10 concurrent requests: key0 called ~1, key1 called ~1,
        // key2 called ~10 (or more if key0/key1 were retried). The total
        // outer calls to create_message should be 10.
        let total: usize = (0..3).map(|i| counters[i].load(Ordering::SeqCst)).sum();
        // Because of retries within try_with_rotation, total inner calls
        // could be > 10. But all should be active/exhausted and no panics.
        assert!(
            total >= 10,
            "should have at least 10 total calls, got {}",
            total
        );

        // key0 and key1 should both be exhausted after 10 concurrent
        // requests through the mutex-guarded ring.
        let statuses = provider.key_statuses();
        assert!(
            !statuses[0].active,
            "key0 should be exhausted after concurrent usage"
        );
        assert!(
            !statuses[1].active,
            "key1 should be exhausted after concurrent usage"
        );
        assert!(statuses[2].active, "key2 should remain active");
    }

    #[tokio::test]
    async fn rotates_through_rate_limit_then_quota_then_auth() {
        // Simulate a provider that fails with different exhaust signals
        // on each key to verify all three are handled.
        let counters = Arc::new(vec![
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        ]);

        let build = {
            let c = counters.clone();
            move |key: &str| {
                let fail = match key {
                    "key0" => Some(ProviderError::RateLimited {
                        provider: ProviderId::new("mock"),
                        retry_after: None,
                    }),
                    "key1" => Some(ProviderError::QuotaExceeded {
                        provider: ProviderId::new("mock"),
                        message: "monthly limit".into(),
                    }),
                    "key2" => Some(ProviderError::AuthFailed {
                        provider: ProviderId::new("mock"),
                        message: "invalid key".into(),
                    }),
                    _ => None,
                };
                build_mock_provider(key, fail, &c)
            }
        };

        let provider = KeyRotatingProvider::new(
            "mock",
            "Mock",
            vec!["key0".into(), "key1".into(), "key2".into(), "key3".into()],
            build,
        );

        let result = provider.create_message(dummy_request()).await;
        assert!(result.is_ok(), "should succeed on key3 after 0/1/2 fail");

        // All four keys were tried exactly once
        assert_eq!(counters[0].load(Ordering::SeqCst), 1);
        assert_eq!(counters[1].load(Ordering::SeqCst), 1);
        assert_eq!(counters[2].load(Ordering::SeqCst), 1);
        assert_eq!(counters[3].load(Ordering::SeqCst), 1);

        // All three error keys are exhausted
        let statuses = provider.key_statuses();
        assert!(!statuses[0].active, "key0 (rate limit) exhausted");
        assert!(!statuses[1].active, "key1 (quota) exhausted");
        assert!(!statuses[2].active, "key2 (auth) exhausted");
        assert!(statuses[3].active, "key3 (success) active");
        assert_eq!(statuses[3].last_error, None);
    }

    #[tokio::test]
    async fn recovers_after_short_cooldown() {
        // Single key: fails once with a 1-second RateLimited cooldown,
        // then succeeds. The sleep+retry loop should wait for the
        // cooldown to expire and re-use the key successfully.
        let counters = Arc::new(vec![Arc::new(AtomicUsize::new(0))]);

        let call_count = Arc::new(AtomicUsize::new(0));

        let build = {
            let c = counters.clone();
            let cc = call_count.clone();
            move |key: &str| {
                let count = cc.fetch_add(1, Ordering::SeqCst);
                let fail = if count == 0 {
                    Some(ProviderError::RateLimited {
                        provider: ProviderId::new("mock"),
                        retry_after: Some(1), // 1s cooldown ≤ MAX_COOLDOWN_WAIT
                    })
                } else {
                    None
                };
                build_mock_provider(key, fail, &c)
            }
        };

        let provider = KeyRotatingProvider::new("mock", "Mock", vec!["key0".into()], build);

        // This will:
        //   1. Try key0 → fails with RateLimited(1s)
        //   2. All exhausted → sleep(1s)
        //   3. Try key0 again → succeeds
        let result = provider.create_message(dummy_request()).await;
        assert!(
            result.is_ok(),
            "request should recover after short cooldown"
        );

        // Exactly 2 calls: first fails, second succeeds
        assert_eq!(call_count.load(Ordering::SeqCst), 2);

        // Key should be active again (cooldown expired)
        let statuses = provider.key_statuses();
        assert!(statuses[0].active, "key should be active after recovery");
    }

    #[tokio::test]
    async fn retry_limit_gives_up_after_max_retries() {
        // Single key that ALWAYS fails with RateLimited(1s). The retry
        // loop should sleep+retry up to MAX_COOLDOWN_RETRIES times,
        // then return the error.
        let counters = Arc::new(vec![Arc::new(AtomicUsize::new(0))]);

        let fail_err = ProviderError::RateLimited {
            provider: ProviderId::new("mock"),
            retry_after: Some(1), // 1s cooldown, triggers wait path
        };

        let build = {
            let c = counters.clone();
            move |_key: &str| build_mock_provider("key", Some(fail_err.clone()), &c)
        };

        let provider = KeyRotatingProvider::new("mock", "Mock", vec!["key0".into()], build);

        let result = provider.create_message(dummy_request()).await;
        assert!(result.is_err(), "should error after exhausting retries");

        match result.unwrap_err() {
            ProviderError::RateLimited { retry_after, .. } => {
                assert!(retry_after.is_some(), "should have retry_after");
            }
            other => panic!("expected RateLimited, got {:?}", other),
        }

        // Expected call pattern:
        //   1. First attempt: key0 fails
        //   2. Retry 1: cooldown expired, key0 fails again
        //   3. Retry 2: cooldown expired, key0 fails again
        //   4. Retry 3: cooldown expired, key0 fails again
        //   Then retry_count=3 >= MAX_COOLDOWN_RETRIES → error returned
        // Total: 1 (initial) + 3 (retries) = 4 calls
        assert_eq!(
            counters[0].load(Ordering::SeqCst),
            1 + MAX_COOLDOWN_RETRIES as usize,
            "should have exactly 1 initial + {} retries",
            MAX_COOLDOWN_RETRIES,
        );

        // Key should be exhausted (all retries consumed)
        let statuses = provider.key_statuses();
        assert!(
            !statuses[0].active,
            "key should be exhausted after retry limit"
        );
    }

    #[tokio::test]
    async fn retry_after_from_rate_limited_is_used_directly() {
        // When a provider returns RateLimited with a retry_after value,
        // it should be used as the cooldown instead of the default 60s.
        let counters = Arc::new(vec![
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        ]);

        let build = {
            let c = counters.clone();
            move |key: &str| {
                let fail = if key == "key0" {
                    Some(ProviderError::RateLimited {
                        provider: ProviderId::new("mock"),
                        retry_after: Some(300), // 5 minutes
                    })
                } else {
                    None
                };
                build_mock_provider(key, fail, &c)
            }
        };

        let provider =
            KeyRotatingProvider::new("mock", "Mock", vec!["key0".into(), "key1".into()], build);

        let result = provider.create_message(dummy_request()).await;
        assert!(result.is_ok(), "should succeed on key1");

        let statuses = provider.key_statuses();
        assert!(!statuses[0].active, "key0 should be exhausted");
        // The cooldown should be ~300s (from retry_after), not ~60s (default)
        if let Some(remaining) = statuses[0].cooldown_remaining_secs {
            assert!(
                remaining >= 290,
                "cooldown should be ~300s, got {}s",
                remaining
            );
        }
    }

    #[tokio::test]
    async fn skip_recovery_loop_returns_immediately_on_exhaustion() {
        // When skip_recovery_loop is set (FreeProvider nesting), the provider
        // must return immediately on exhaustion instead of sleeping/retrying.
        // The FreeProvider handles fallback at a higher level and any sleep
        // here just delays the overall chain.
        let counters = Arc::new(vec![Arc::new(AtomicUsize::new(0))]);

        let fail_err = ProviderError::RateLimited {
            provider: ProviderId::new("mock"),
            retry_after: Some(1), // short cooldown that would normally trigger wait
        };

        let build = {
            let c = counters.clone();
            move |_key: &str| build_mock_provider("key", Some(fail_err.clone()), &c)
        };

        let mut provider = KeyRotatingProvider::new("mock", "Mock", vec!["key0".into()], build);
        provider.set_skip_recovery_loop(true);

        let start = std::time::Instant::now();
        let result = provider.create_message(dummy_request()).await;
        let elapsed = start.elapsed();

        assert!(result.is_err(), "should return error immediately");
        assert!(
            elapsed < std::time::Duration::from_millis(200),
            "skip_recovery_loop should return immediately, took {:?}",
            elapsed
        );
    }
}
