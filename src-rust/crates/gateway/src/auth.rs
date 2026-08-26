//! Bearer-key authentication and per-key RPM/TPM rate limiting.
//!
//! Two-dimensional token buckets per key (RPM + TPM), hand-rolled per the
//! plan's §4c. Bucket state is allocated lazily on first use per key; unknown
//! keys are rejected before any state allocation.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use subtle::ConstantTimeEq;

/// A token bucket for one dimension (requests or tokens).
#[derive(Debug, Clone)]
struct Bucket {
    capacity: f64,
    tokens: f64,
    /// When tokens were last refilled.
    last_refill: Instant,
    /// Refill rate per second.
    refill_per_sec: f64,
}

impl Bucket {
    fn new(capacity: f64, per_minute: f64) -> Self {
        Self {
            capacity,
            tokens: capacity,
            last_refill: Instant::now(),
            refill_per_sec: per_minute / 60.0,
        }
    }

    /// Try to consume `n` tokens. Refills first based on elapsed time.
    fn try_consume(&mut self, n: f64) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        self.last_refill = now;
        if self.tokens >= n {
            self.tokens -= n;
            true
        } else {
            false
        }
    }

    /// Seconds until the bucket has `n` tokens available again.
    fn seconds_until(&self, n: f64) -> u64 {
        let needed = (n - self.tokens).max(0.0);
        if needed <= 0.0 {
            return 0;
        }
        (needed / self.refill_per_sec).ceil() as u64
    }
}

/// Per-key rate-limit state.
#[derive(Debug)]
struct KeyState {
    rpm: Bucket,
    tpm: Bucket,
}

/// Per-key rate limiter with lazy bucket allocation.
#[derive(Debug, Default)]
pub struct RateLimiter {
    keys: Mutex<HashMap<String, KeyState>>,
    rpm_limit: f64,
    tpm_limit: f64,
}

/// Outcome of a rate-limit check.
#[derive(Debug, Clone, PartialEq)]
pub enum RateLimitOutcome {
    /// Request allowed.
    Allowed,
    /// RPM budget exhausted; retry after this many seconds.
    RpmExhausted(u64),
    /// TPM budget exhausted; retry after this many seconds.
    TpmExhausted(u64),
}

impl RateLimiter {
    pub fn new(rpm: u32, tpm: u32) -> Self {
        Self {
            keys: Mutex::new(HashMap::new()),
            rpm_limit: rpm as f64,
            tpm_limit: tpm as f64,
        }
    }

    /// Check a request against the key's RPM + TPM budgets.
    ///
    /// `tokens_estimate` is the estimated tokens for this request (from
    /// `max_tokens` + input estimate). TPM is enforced on the estimate
    /// up-front; the exact count is added on completion via [`Self::record_usage`].
    pub fn check(&self, key: &str, tokens_estimate: u64) -> RateLimitOutcome {
        let mut keys = self
            .keys
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let state = keys.entry(key.to_string()).or_insert_with(|| KeyState {
            rpm: Bucket::new(self.rpm_limit, self.rpm_limit),
            tpm: Bucket::new(self.tpm_limit, self.tpm_limit),
        });
        if !state.rpm.try_consume(1.0) {
            return RateLimitOutcome::RpmExhausted(state.rpm.seconds_until(1.0));
        }
        if !state.tpm.try_consume(tokens_estimate as f64) {
            // A rejected request must not burn an RPM token.
            state.rpm.tokens = (state.rpm.tokens + 1.0).min(state.rpm.capacity);
            return RateLimitOutcome::TpmExhausted(state.tpm.seconds_until(tokens_estimate as f64));
        }
        RateLimitOutcome::Allowed
    }

    /// Replace the request estimate with the provider's actual usage.
    ///
    /// The estimate was consumed by [`Self::check`]. Refund it first, then
    /// consume the actual count. Keeping both values is important: charging
    /// only the actual count would let a client repeatedly reserve a large
    /// request and receive a free token refund.
    pub fn record_usage(&self, key: &str, estimated_tokens: u64, actual_tokens: u64) {
        let mut keys = self
            .keys
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(state) = keys.get_mut(key) {
            state.tpm.tokens = (state.tpm.tokens + estimated_tokens as f64).min(state.tpm.capacity);
            state.tpm.tokens = (state.tpm.tokens - actual_tokens as f64).max(0.0);
        }
    }

    /// Remaining budget info for headers (0.0..=1.0 fractions).
    pub fn remaining(&self, key: &str) -> (f64, f64) {
        let keys = self
            .keys
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match keys.get(key) {
            Some(state) => (
                (state.rpm.tokens / state.rpm.capacity).clamp(0.0, 1.0),
                (state.tpm.tokens / state.tpm.capacity).clamp(0.0, 1.0),
            ),
            None => (1.0, 1.0),
        }
    }
}

/// Validate a bearer token against the allowed key set in constant time.
pub fn validate_bearer(auth_header: Option<&str>, allowed_keys: &[String]) -> Option<String> {
    let header = auth_header?;
    let token = header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))?;
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    for key in allowed_keys {
        // Constant-time comparison to avoid timing attacks.
        if token.as_bytes().ct_eq(key.as_bytes()).into() {
            return Some(token.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_validates_constant_time() {
        let keys = vec!["secret-key-1".to_string(), "secret-key-2".to_string()];
        assert_eq!(
            validate_bearer(Some("Bearer secret-key-1"), &keys),
            Some("secret-key-1".to_string())
        );
        assert_eq!(
            validate_bearer(Some("bearer secret-key-2"), &keys),
            Some("secret-key-2".to_string())
        );
        assert_eq!(validate_bearer(Some("Bearer wrong"), &keys), None);
        assert_eq!(validate_bearer(None, &keys), None);
        assert_eq!(validate_bearer(Some("secret-key-1"), &keys), None); // no prefix
    }

    #[test]
    fn rate_limiter_allows_under_capacity() {
        let limiter = RateLimiter::new(10, 1000);
        for _ in 0..10 {
            assert_eq!(limiter.check("k", 10), RateLimitOutcome::Allowed);
        }
    }

    #[test]
    fn rate_limiter_rejects_over_rpm() {
        let limiter = RateLimiter::new(2, 1000);
        assert_eq!(limiter.check("k", 10), RateLimitOutcome::Allowed);
        assert_eq!(limiter.check("k", 10), RateLimitOutcome::Allowed);
        match limiter.check("k", 10) {
            RateLimitOutcome::RpmExhausted(secs) => assert!(secs > 0),
            other => panic!("expected RpmExhausted, got {other:?}"),
        }
    }

    #[test]
    fn rate_limiter_exhausts_tpm() {
        let limiter = RateLimiter::new(100, 100);
        assert_eq!(limiter.check("k", 60), RateLimitOutcome::Allowed);
        match limiter.check("k", 60) {
            RateLimitOutcome::TpmExhausted(secs) => assert!(secs > 0),
            other => panic!("expected TpmExhausted, got {other:?}"),
        }
    }

    #[test]
    fn actual_usage_replaces_not_adds_to_the_estimate() {
        let limiter = RateLimiter::new(100, 100);
        assert_eq!(limiter.check("k", 80), RateLimitOutcome::Allowed);
        limiter.record_usage("k", 80, 10);
        let (_, remaining) = limiter.remaining("k");
        assert!((remaining - 0.9).abs() < 0.001);
    }
}
