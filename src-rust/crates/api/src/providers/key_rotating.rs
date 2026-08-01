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
use futures::Stream;

use crate::provider::{LlmProvider, ModelInfo};
use crate::provider_error::ProviderError;
use crate::provider_types::{
    ProviderCapabilities, ProviderRequest, ProviderResponse, ProviderStatus, StreamEvent,
    SystemPromptStyle,
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
/// This lets short rate-limit cooldowns (typically 60s) recover transparently
/// without the user seeing a failure. Longer cooldowns (quota, auth) surface
/// the error immediately.
const MAX_COOLDOWN_WAIT: u64 = 60;

/// Maximum number of cooldown wait-retry cycles before giving up and
/// returning a `RateLimited` error to the caller. Each cycle re-reads
/// the shortest cooldown from the key ring, so if cooldowns change
/// (e.g. a different key is exhausted with a different duration), the
/// new value is used on the next wait.
const MAX_COOLDOWN_RETRIES: u32 = 3;

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
    build_provider: ProviderFactory,
    /// Path to persisted cooldown state file. `None` = no persistence.
    state_path: Option<PathBuf>,
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
        Self {
            provider_id: ProviderId::new(&pid),
            provider_name: provider_name.into(),
            ring: Arc::new(Mutex::new(KeyRing::new(pid, keys))),
            build_provider: Arc::new(build_provider),
            state_path: None,
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
        let state_path = KeyRing::default_state_path(&pid);
        let ring = Arc::new(Mutex::new(KeyRing::new(pid.clone(), keys)));
        // Restore persisted cooldown state so that a 12-hour cooldown
        // doesn't reset just because the user restarted the app.
        ring.lock().unwrap().load_from_file(&state_path);

        Self {
            provider_id: ProviderId::new(&pid),
            provider_name: provider_name.into(),
            ring,
            build_provider: Arc::new(build_provider),
            state_path: Some(state_path),
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

    /// Reference to the key ring (for inspection).
    pub fn ring(&self) -> &Arc<Mutex<KeyRing>> {
        &self.ring
    }

    // -----------------------------------------------------------------------
    // Core retry loop
    // -----------------------------------------------------------------------

    /// Get the next available key, build a provider, and call `try_provider`.
    /// On exhaustible errors, marks the key and loops. On non-exhaustible
    /// errors, returns immediately. When all keys are exhausted, returns
    /// `RateLimited` with the earliest retry time.
    async fn try_with_rotation<F, Fut, T>(&self, try_provider: F) -> Result<T, ProviderError>
    where
        F: Fn(Arc<dyn LlmProvider>) -> Fut,
        Fut: std::future::Future<Output = Result<T, ProviderError>>,
    {
        let mut retry_count: u32 = 0;

        loop {
            // Get the next available key (lock scope ends before any .await).
            let provider = {
                let mut ring = self.ring.lock().unwrap();
                ring.next_available()
                    .map(|(_idx, key)| (self.build_provider)(key))
            };

            let provider = match provider {
                Some(p) => p,
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

                    if should_wait && retry_count < MAX_COOLDOWN_RETRIES {
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

                    return Err(ProviderError::RateLimited {
                        provider: self.provider_id.clone(),
                        retry_after: Some(retry_secs),
                    });
                }
            };

            match try_provider(provider).await {
                Ok(response) => return Ok(response),
                Err(err) => {
                    let Some(signal) = classify_exhaust(&err) else {
                        return Err(err);
                    };

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

                        let active_idx = ring.statuses().iter().find(|s| s.active).map(|s| s.index);

                        if let Some(idx) = active_idx {
                            ring.mark_exhausted(idx, final_cooldown, Some(msg.to_string()));

                            // Persist cooldown state immediately so a 12-hour
                            // cooldown survives an app restart 10 hours in.
                            if let Some(ref persist_path) = self.state_path {
                                ring.save_to_file(persist_path);
                            }
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
        self.try_with_rotation(|provider| {
            let req = request.clone();
            async move { provider.create_message(req).await }
        })
        .await
    }

    async fn create_message_stream(
        &self,
        request: ProviderRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        self.try_with_rotation(|provider| {
            let req = request.clone();
            async move { provider.create_message_stream(req).await }
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
        // Try each active key until one reports healthy.
        let mut last_status: Result<ProviderStatus, ProviderError> =
            Ok(ProviderStatus::Unavailable {
                reason: "no keys configured".to_string(),
            });

        loop {
            let key = {
                let mut ring = self.ring.lock().unwrap();
                ring.next_available().map(|(_, k)| k.to_string())
            };
            let Some(k) = key else {
                break;
            };

            let provider = (self.build_provider)(&k);
            match provider.health_check().await {
                Ok(ProviderStatus::Healthy) => return Ok(ProviderStatus::Healthy),
                Ok(other) => last_status = Ok(other),
                Err(e) => last_status = Err(e),
            }
        }

        last_status
    }

    fn key_ring_status(&self) -> Option<(usize, usize, Option<u64>)> {
        // Every key is either active or in cooldown, so the total is the sum of
        // the two public counters (both poison-safe).
        let active = self.active_key_count();
        let total = active + self.exhausted_key_count();
        match self.ring.lock() {
            Ok(ring) => Some((active, total, ring.earliest_retry_secs())),
            Err(_) => None,
        }
    }

    fn mark_key_exhausted(
        &self,
        _upstream_id: Option<&str>,
        key_idx: usize,
        cooldown_secs: u64,
        reason: Option<String>,
    ) -> bool {
        match self.ring.lock() {
            Ok(mut ring) => ring.mark_exhausted(key_idx, cooldown_secs, reason),
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
    use crate::provider_types::StopReason;
    use clawde_core::types::{Message, UsageInfo};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A mock provider that can be configured to fail or succeed.
    struct MockProvider {
        id: ProviderId,
        name: String,
        fail_with: Option<ProviderError>,
        call_count: Arc<AtomicUsize>,
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
            if let Some(ref err) = self.fail_with {
                return Err(err.clone());
            }
            Ok(ProviderResponse {
                id: "mock".into(),
                model: request.model,
                content: vec![],
                stop_reason: StopReason::EndTurn,
                usage: UsageInfo::default(),
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
            if let Some(ref err) = self.fail_with {
                return Err(err.clone());
            }
            let stream = futures::stream::iter(vec![]);
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
            provider_options: serde_json::Value::Null,
        }
    }

    fn build_mock_provider(
        key: &str,
        fail: Option<ProviderError>,
        counters: &Arc<Vec<Arc<AtomicUsize>>>,
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
            ProviderError::RateLimited { retry_after, .. } => {
                assert!(retry_after.is_some(), "should have retry_after");
            }
            other => panic!("expected RateLimited, got {:?}", other),
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
}
