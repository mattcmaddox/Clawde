//! Capacity observations for the FreeProvider fallback chain.
//!
//! This state is deliberately separate from credential health and circuit
//! breaker state. A provider can have a valid credential while its request or
//! token budget is nearly exhausted. Observations are therefore used only to
//! demote an upstream in automatic routing; they never invalidate a key and
//! never hard-skip a provider on missing or stale data.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{current_unix_nanos, current_unix_secs, write_private_json_locked};

/// Capacity data is useful for routing for a short period only. Providers may
/// reset limits independently, and headers can be delayed or approximate.
pub(crate) const CAPACITY_OBSERVATION_TTL_SECS: u64 = 15 * 60;

/// One normalized response-header observation for an upstream.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CapacityObservation {
    /// Fraction of the token budget consumed, when reported by the provider.
    pub tokens_pct_used: Option<f32>,
    /// Fraction of the request budget consumed, when reported by the provider.
    pub requests_pct_used: Option<f32>,
    /// Delta from `Retry-After`, when reported by the provider.
    pub retry_after_secs: Option<u64>,
    /// Unix timestamp at which the current rate-limit window resets.
    pub reset_at_unix: Option<u64>,
    /// Unix timestamp at which the observation was received.
    pub observed_at_unix: u64,
}

impl CapacityObservation {
    fn normalized(
        tokens_pct_used: Option<f32>,
        requests_pct_used: Option<f32>,
        retry_after_secs: Option<u64>,
        reset_at_unix: Option<u64>,
    ) -> Option<Self> {
        let normalize = |value: Option<f32>| {
            value.and_then(|value| {
                (value.is_finite() && (0.0..=1.0).contains(&value)).then_some(value)
            })
        };
        let tokens_pct_used = normalize(tokens_pct_used);
        let requests_pct_used = normalize(requests_pct_used);
        let observed_at_unix = current_unix_secs();
        let reset_at_unix = reset_at_unix
            .or_else(|| retry_after_secs.map(|seconds| observed_at_unix.saturating_add(seconds)));
        (tokens_pct_used.is_some()
            || requests_pct_used.is_some()
            || retry_after_secs.is_some()
            || reset_at_unix.is_some())
        .then_some(Self {
            tokens_pct_used,
            requests_pct_used,
            retry_after_secs,
            reset_at_unix,
            observed_at_unix,
        })
    }

    fn utilization(self) -> f32 {
        self.tokens_pct_used
            .into_iter()
            .chain(self.requests_pct_used)
            .fold(0.0, f32::max)
    }

    fn is_fresh(self, now: u64) -> bool {
        now.saturating_sub(self.observed_at_unix) <= CAPACITY_OBSERVATION_TTL_SECS
            && self.reset_at_unix.is_none_or(|reset| reset > now)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct LocalUsage {
    #[serde(default)]
    request_window_started_unix: u64,
    #[serde(default)]
    requests_used: u64,
    #[serde(default)]
    token_window_started_unix: u64,
    #[serde(default)]
    tokens_used: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CapacitySnapshot {
    upstream: String,
    #[serde(default)]
    key_idx: Option<usize>,
    #[serde(default)]
    tokens_pct_used: Option<f32>,
    #[serde(default)]
    requests_pct_used: Option<f32>,
    #[serde(default)]
    retry_after_secs: Option<u64>,
    #[serde(default)]
    reset_at_unix: Option<u64>,
    #[serde(default)]
    observed_at_unix: u64,
    #[serde(default)]
    saved_at_unix_nanos: u64,
    /// Locally estimated usage for providers with explicitly declared limits.
    #[serde(default)]
    local_usage: Option<LocalUsage>,
}

/// In-memory capacity state, keyed by the stable catalog upstream index.
pub(crate) struct CapacityState {
    observations: Vec<Option<CapacityObservation>>,
    key_observations: Vec<HashMap<usize, CapacityObservation>>,
    local_usage: Vec<Option<LocalUsage>>,
    upstream_ids: Vec<String>,
    persist_path: Option<PathBuf>,
}

impl CapacityState {
    pub(crate) fn new(n: usize) -> Self {
        Self {
            observations: vec![None; n],
            key_observations: (0..n).map(|_| HashMap::new()).collect(),
            local_usage: vec![None; n],
            upstream_ids: Vec::new(),
            persist_path: None,
        }
    }

    pub(crate) fn with_persistence(
        mut self,
        upstream_ids: Vec<String>,
        persist_path: Option<PathBuf>,
    ) -> Self {
        self.upstream_ids = upstream_ids;
        self.persist_path = persist_path;
        if let Some(path) = self.persist_path.clone() {
            self.load_from_file(&path);
        }
        self
    }

    /// Record an observation for both the upstream aggregate and, when the
    /// rotating provider supplied an exact slot, that individual key.
    pub(crate) fn observe_for_key(
        &mut self,
        idx: usize,
        key_idx: Option<usize>,
        tokens_pct_used: Option<f32>,
        requests_pct_used: Option<f32>,
        retry_after_secs: Option<u64>,
        reset_at_unix: Option<u64>,
    ) {
        let Some(observation) = CapacityObservation::normalized(
            tokens_pct_used,
            requests_pct_used,
            retry_after_secs,
            reset_at_unix,
        ) else {
            return;
        };
        if let Some(slot) = self.observations.get_mut(idx) {
            *slot = Some(observation);
        }
        if let Some(key_idx) = key_idx {
            if let Some(keys) = self.key_observations.get_mut(idx) {
                keys.insert(key_idx, observation);
            }
        }
        self.save();
    }

    /// Record locally estimated request/token usage for an upstream with an
    /// explicit quota declaration. `requests` is normally 1 at dispatch and
    /// `tokens` is the estimated input plus any known output usage.
    pub(crate) fn record_local_usage(
        &mut self,
        idx: usize,
        quota: Option<super::LocalQuota>,
        requests: u64,
        tokens: u64,
    ) {
        let Some(quota) = quota else {
            return;
        };
        if quota.requests.is_none() && quota.tokens.is_none() {
            return;
        }
        let now = current_unix_secs();
        let Some(slot) = self.local_usage.get_mut(idx) else {
            return;
        };
        let mut usage = slot.unwrap_or(LocalUsage {
            request_window_started_unix: now,
            requests_used: 0,
            token_window_started_unix: now,
            tokens_used: 0,
        });
        if let Some(window) = quota.requests {
            if now.saturating_sub(usage.request_window_started_unix) >= window.window_secs {
                usage.request_window_started_unix = now;
                usage.requests_used = 0;
            }
            usage.requests_used = usage.requests_used.saturating_add(requests);
        }
        if let Some(window) = quota.tokens {
            if now.saturating_sub(usage.token_window_started_unix) >= window.window_secs {
                usage.token_window_started_unix = now;
                usage.tokens_used = 0;
            }
            usage.tokens_used = usage.tokens_used.saturating_add(tokens);
        }
        *slot = Some(usage);
        self.save();
    }

    /// Return a stable demotion rank for automatic routing.
    ///
    /// 0 is normal, 1 means elevated utilization, 2 is near exhaustion, and
    /// 3 is critically near exhaustion. This is intentionally not a hard
    /// eligibility gate: stale/missing headers remain routeable.
    pub(crate) fn rank(&self, idx: usize, quota: Option<super::LocalQuota>) -> u8 {
        let used = self
            .observation(idx)
            .map(CapacityObservation::utilization)
            .or_else(|| self.local_utilization(idx, quota))
            .unwrap_or(0.0);
        match used {
            used if used >= 0.95 => 3,
            used if used >= 0.80 => 2,
            used if used >= 0.60 => 1,
            _ => 0,
        }
    }

    pub(crate) fn observation(&self, idx: usize) -> Option<CapacityObservation> {
        let now = current_unix_secs();
        self.observations
            .get(idx)
            .copied()
            .flatten()
            .filter(|observation| observation.is_fresh(now))
    }

    fn local_utilization(&self, idx: usize, quota: Option<super::LocalQuota>) -> Option<f32> {
        let usage = self.local_usage.get(idx).copied().flatten()?;
        let now = current_unix_secs();
        let mut utilization = None;
        if let Some(window) = quota.and_then(|quota| quota.requests) {
            if now.saturating_sub(usage.request_window_started_unix) < window.window_secs {
                utilization = Some(usage.requests_used as f32 / window.limit as f32);
            }
        }
        if let Some(window) = quota.and_then(|quota| quota.tokens) {
            if now.saturating_sub(usage.token_window_started_unix) < window.window_secs {
                let token_utilization = usage.tokens_used as f32 / window.limit as f32;
                utilization = Some(
                    utilization
                        .map_or(token_utilization, |value: f32| value.max(token_utilization)),
                );
            }
        }
        utilization.filter(|value| value.is_finite())
    }

    fn local_reset_at(&self, idx: usize, quota: Option<super::LocalQuota>) -> Option<u64> {
        let usage = self.local_usage.get(idx).copied().flatten()?;
        let now = current_unix_secs();
        let mut reset_at = None;
        if let Some(window) = quota.and_then(|quota| quota.requests) {
            let reset = usage
                .request_window_started_unix
                .saturating_add(window.window_secs);
            if reset > now {
                reset_at = Some(reset);
            }
        }
        if let Some(window) = quota.and_then(|quota| quota.tokens) {
            let reset = usage
                .token_window_started_unix
                .saturating_add(window.window_secs);
            if reset > now {
                reset_at = Some(reset_at.map_or(reset, |current| current.min(reset)));
            }
        }
        reset_at
    }

    /// Return the effective fresh capacity signal used by routing, with its
    /// provenance. Response headers win over local estimates; expired or
    /// missing data is intentionally omitted rather than presented as zero.
    pub(crate) fn status(
        &self,
        idx: usize,
        quota: Option<super::LocalQuota>,
    ) -> Option<crate::provider::UpstreamCapacityStatus> {
        let upstream_id = self.upstream_ids.get(idx)?.clone();
        if let Some(observation) = self.observation(idx) {
            return Some(crate::provider::UpstreamCapacityStatus {
                upstream_id,
                source: crate::provider::CapacityStatusSource::Headers,
                utilization_pct: (observation.utilization() * 100.0).clamp(0.0, 100.0),
                tokens_pct_used: observation.tokens_pct_used,
                requests_pct_used: observation.requests_pct_used,
                retry_after_secs: observation.retry_after_secs,
                reset_at_unix: observation.reset_at_unix,
            });
        }

        let utilization = self.local_utilization(idx, quota)?;
        Some(crate::provider::UpstreamCapacityStatus {
            upstream_id,
            source: crate::provider::CapacityStatusSource::LocalEstimate,
            utilization_pct: (utilization * 100.0).clamp(0.0, 100.0),
            tokens_pct_used: None,
            requests_pct_used: None,
            retry_after_secs: None,
            reset_at_unix: self.local_reset_at(idx, quota),
        })
    }

    fn save(&self) {
        let Some(path) = self.persist_path.as_ref() else {
            return;
        };
        let mut entries: Vec<CapacitySnapshot> = self
            .upstream_ids
            .iter()
            .enumerate()
            .filter_map(|(idx, upstream)| {
                let observation = self.observations.get(idx).copied().flatten();
                let local_usage = self.local_usage.get(idx).copied().flatten();
                if observation.is_none() && local_usage.is_none() {
                    return None;
                }
                Some(CapacitySnapshot {
                    upstream: upstream.clone(),
                    key_idx: None,
                    tokens_pct_used: observation.and_then(|value| value.tokens_pct_used),
                    requests_pct_used: observation.and_then(|value| value.requests_pct_used),
                    retry_after_secs: observation.and_then(|value| value.retry_after_secs),
                    reset_at_unix: observation.and_then(|value| value.reset_at_unix),

                    observed_at_unix: observation.map(|value| value.observed_at_unix).unwrap_or(0),
                    saved_at_unix_nanos: current_unix_nanos(),
                    local_usage,
                })
            })
            .collect();
        for (idx, upstream) in self.upstream_ids.iter().enumerate() {
            let Some(keys) = self.key_observations.get(idx) else {
                continue;
            };
            entries.extend(keys.iter().map(|(key_idx, observation)| CapacitySnapshot {
                upstream: upstream.clone(),
                key_idx: Some(*key_idx),
                tokens_pct_used: observation.tokens_pct_used,
                requests_pct_used: observation.requests_pct_used,
                retry_after_secs: observation.retry_after_secs,
                reset_at_unix: observation.reset_at_unix,
                observed_at_unix: observation.observed_at_unix,
                saved_at_unix_nanos: current_unix_nanos(),
                local_usage: None,
            }));
        }
        if entries.is_empty() {
            return;
        }
        let Ok(json) = serde_json::to_string_pretty(&entries) else {
            return;
        };
        write_private_json_locked(path, Some(&json), true, false);
    }

    fn load_from_file(&mut self, path: &Path) {
        let Ok(json) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(entries) = serde_json::from_str::<Vec<CapacitySnapshot>>(&json) else {
            return;
        };
        let now = current_unix_secs();
        for entry in entries {
            let Some(idx) = self
                .upstream_ids
                .iter()
                .position(|id| id == &entry.upstream)
            else {
                continue;
            };
            if let Some(local_usage) = entry.local_usage {
                self.local_usage[idx] = Some(local_usage);
            }
            if entry.observed_at_unix > 0
                && now.saturating_sub(entry.observed_at_unix) > CAPACITY_OBSERVATION_TTL_SECS
            {
                continue;
            }
            let observation = CapacityObservation::normalized(
                entry.tokens_pct_used,
                entry.requests_pct_used,
                entry.retry_after_secs,
                entry.reset_at_unix,
            )
            .map(|mut observation| {
                observation.observed_at_unix = entry.observed_at_unix;
                observation
            });
            if let Some(key_idx) = entry.key_idx {
                if let Some(keys) = self.key_observations.get_mut(idx) {
                    if let Some(observation) = observation {
                        keys.insert(key_idx, observation);
                    }
                }
            } else {
                self.observations[idx] = observation;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_rank_demotes_high_utilization_without_hard_skipping() {
        let mut state = CapacityState::new(3);
        assert_eq!(state.rank(0, None), 0);
        state.observe_for_key(0, None, Some(0.61), None, None, None);
        state.observe_for_key(1, None, None, Some(0.81), None, None);
        state.observe_for_key(2, None, Some(0.96), Some(0.20), None, None);
        assert_eq!(state.rank(0, None), 1);
        assert_eq!(state.rank(1, None), 2);
        assert_eq!(state.rank(2, None), 3);
        // Missing data and every rank remain routeable; rank is only an order.
        assert!(state.observation(0).is_some());
        assert_eq!(state.rank(99, None), 0);
    }

    #[test]
    fn stale_observations_stop_affecting_routing() {
        let mut state = CapacityState::new(1);
        state.observations[0] = Some(CapacityObservation {
            tokens_pct_used: Some(0.99),
            requests_pct_used: None,
            retry_after_secs: None,
            reset_at_unix: None,
            observed_at_unix: current_unix_secs().saturating_sub(CAPACITY_OBSERVATION_TTL_SECS + 1),
        });
        assert_eq!(state.rank(0, None), 0);
        assert!(state.observation(0).is_none());
    }

    #[test]
    fn invalid_observations_are_ignored() {
        let mut state = CapacityState::new(1);
        state.observe_for_key(0, None, Some(f32::NAN), Some(2.0), None, None);
        assert_eq!(state.rank(0, None), 0);
        assert!(state.observation(0).is_none());
    }

    #[test]
    fn local_estimates_use_known_limits_and_expire_windows() {
        let mut state = CapacityState::new(1);
        let quota = crate::providers::free::LocalQuota::requests(5, 60);
        state.record_local_usage(0, Some(quota), 3, 0);
        assert_eq!(state.rank(0, Some(quota)), 1);
        state.record_local_usage(0, Some(quota), 2, 0);
        assert_eq!(state.rank(0, Some(quota)), 3);

        state.local_usage[0] = Some(LocalUsage {
            request_window_started_unix: current_unix_secs().saturating_sub(61),
            requests_used: 5,
            token_window_started_unix: current_unix_secs(),
            tokens_used: 0,
        });
        assert_eq!(state.rank(0, Some(quota)), 0);

        let dir = tempfile::tempdir().expect("temporary capacity directory");
        let path = dir.path().join("capacity.json");
        let mut persisted =
            CapacityState::new(1).with_persistence(vec!["groq".to_string()], Some(path.clone()));
        persisted.record_local_usage(0, Some(quota), 3, 0);
        let restored = CapacityState::new(1).with_persistence(vec!["groq".to_string()], Some(path));
        assert_eq!(restored.rank(0, Some(quota)), 1);
    }

    #[test]
    fn unknown_limits_remain_neutral_and_headers_win() {
        let mut state = CapacityState::new(1);
        state.local_usage[0] = Some(LocalUsage {
            request_window_started_unix: current_unix_secs(),
            requests_used: 999,
            token_window_started_unix: current_unix_secs(),
            tokens_used: 0,
        });
        assert_eq!(state.rank(0, None), 0);
        let quota = crate::providers::free::LocalQuota::requests(1_000, 60);
        state.observe_for_key(0, None, Some(0.10), None, None, None);
        assert_eq!(
            state.rank(0, Some(quota)),
            0,
            "fresh server utilization takes precedence over the local estimate"
        );
    }

    #[test]
    fn status_reports_provenance_and_prefers_fresh_headers() {
        let mut state = CapacityState::new(1).with_persistence(vec!["cerebras".to_string()], None);
        let quota = crate::providers::free::LocalQuota::requests(10, 60);
        state.record_local_usage(0, Some(quota), 8, 0);
        let local = state.status(0, Some(quota)).expect("local status");
        assert_eq!(local.upstream_id, "cerebras");
        assert_eq!(
            local.source,
            crate::provider::CapacityStatusSource::LocalEstimate
        );
        assert_eq!(local.utilization_pct, 80.0);
        assert!(local.reset_at_unix.is_some());

        state.observe_for_key(0, None, Some(0.25), Some(0.40), Some(12), None);
        let headers = state.status(0, Some(quota)).expect("header status");
        assert_eq!(
            headers.source,
            crate::provider::CapacityStatusSource::Headers
        );
        assert_eq!(headers.utilization_pct, 40.0);
        assert_eq!(headers.tokens_pct_used, Some(0.25));
        assert_eq!(headers.requests_pct_used, Some(0.40));
        assert_eq!(headers.retry_after_secs, Some(12));
    }

    #[test]
    fn status_omits_missing_and_expired_signals() {
        let mut state = CapacityState::new(1).with_persistence(vec!["groq".to_string()], None);
        assert!(state.status(0, None).is_none());
        state.observations[0] = Some(CapacityObservation {
            tokens_pct_used: Some(0.90),
            requests_pct_used: None,
            retry_after_secs: None,
            reset_at_unix: Some(current_unix_secs().saturating_sub(1)),
            observed_at_unix: current_unix_secs(),
        });
        assert!(state.status(0, None).is_none());
    }

    #[test]
    fn key_observations_are_attributed_and_persisted_by_slot() {
        let dir = tempfile::tempdir().expect("temporary capacity directory");
        let path = dir.path().join("capacity.json");
        let mut state = CapacityState::new(2).with_persistence(
            vec!["groq".to_string(), "nvidia".to_string()],
            Some(path.clone()),
        );
        state.observe_for_key(1, Some(2), Some(0.96), None, Some(7), None);

        assert_eq!(
            state.rank(1, None),
            3,
            "aggregate remains available for routing"
        );
        assert_eq!(
            state.key_observations[1]
                .get(&2)
                .map(|observation| observation.utilization()),
            Some(0.96)
        );
        assert!(!state.key_observations[1].contains_key(&1));

        let restored = CapacityState::new(2)
            .with_persistence(vec!["groq".to_string(), "nvidia".to_string()], Some(path));
        let observation = restored.key_observations[1]
            .get(&2)
            .expect("key-level observation restored");
        assert_eq!(observation.retry_after_secs, Some(7));
        assert_eq!(observation.utilization(), 0.96);
    }

    #[test]
    fn capacity_observations_persist_and_restore_by_upstream_id() {
        let dir = tempfile::tempdir().expect("temporary capacity directory");
        let path = dir.path().join("capacity.json");
        let mut state = CapacityState::new(2).with_persistence(
            vec!["groq".to_string(), "nvidia".to_string()],
            Some(path.clone()),
        );
        state.observe_for_key(1, None, Some(0.96), None, Some(12), None);
        assert!(path.exists());

        let restored = CapacityState::new(2)
            .with_persistence(vec!["groq".to_string(), "nvidia".to_string()], Some(path));
        assert_eq!(restored.rank(0, None), 0);
        assert_eq!(restored.rank(1, None), 3);
        let observation = restored.observation(1).expect("restored observation");
        assert_eq!(observation.retry_after_secs, Some(12));
        assert!(observation.reset_at_unix.is_some());
    }

    #[test]
    fn reset_time_expires_capacity_before_ttl() {
        let mut state = CapacityState::new(1);
        state.observations[0] = Some(CapacityObservation {
            tokens_pct_used: Some(0.99),
            requests_pct_used: None,
            retry_after_secs: None,
            reset_at_unix: Some(current_unix_secs().saturating_sub(1)),
            observed_at_unix: current_unix_secs(),
        });
        assert_eq!(state.rank(0, None), 0);
        assert!(state.observation(0).is_none());
    }
}
