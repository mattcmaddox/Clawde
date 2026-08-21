// tool_use_tracker.rs — Per-model tool-use success rate tracking (Issue 6).
//
// After each turn, the query loop records whether tools were available and
// whether the model actually emitted `tool_use` blocks. Over time this builds
// per-model success rates that the auto-switch can use to deprioritize models
// that claim tool support but never use tools in practice.
//
// The tracker is cheap: a `Mutex<HashMap>` updated once per turn (no hot-path
// contention). It lives in `QueryConfig` so the query loop and sub-agents share
// the same view.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Unique key identifying a (provider, model) pair.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct ModelKey {
    pub provider: String,
    pub model: String,
}

/// Aggregate stats for one model, using exponential moving average (EMA)
/// so recent turns weight more heavily than old ones. This ensures a model
/// that was unreliable early but has since improved can recover.
#[derive(Debug, Clone)]
pub struct ModelToolStats {
    /// Number of turns where tools were available (sent in the request).
    pub attempts: u32,
    /// Number of turns where the model emitted at least one `tool_use` block.
    pub successes: u32,
    /// EMA-smoothed success rate in `[0.0, 1.0]`. Updated on each turn
    /// with `alpha` weighting the latest observation.
    pub ema_rate: f64,
}

impl Default for ModelToolStats {
    fn default() -> Self {
        Self {
            attempts: 0,
            successes: 0,
            ema_rate: 0.5, // neutral prior until data arrives
        }
    }
}

impl ModelToolStats {
    /// EMA smoothing factor. Higher = more weight on recent turns.
    /// 0.4 means the latest turn contributes 40% to the smoothed rate.
    const EMA_ALPHA: f64 = 0.4;

    /// Update the EMA with a new observation (0.0 or 1.0).
    pub fn update_ema(&mut self, success: bool) {
        let obs = if success { 1.0 } else { 0.0 };
        self.ema_rate = Self::EMA_ALPHA * obs + (1.0 - Self::EMA_ALPHA) * self.ema_rate;
    }

    /// Success rate as a fraction `[0.0, 1.0]`. Returns `None` when there is
    /// not enough data (fewer than `MIN_ATTEMPTS` turns) to form a
    /// trustworthy signal. Uses the EMA-smoothed rate for stability.
    pub fn success_rate(&self, min_attempts: u32) -> Option<f64> {
        if self.attempts < min_attempts {
            return None;
        }
        Some(self.ema_rate)
    }

    /// Whether the model has a poor track record of using tools when they
    /// are available (below `threshold` success rate with enough data).
    pub fn is_tool_use_unreliable(&self, threshold: f64, min_attempts: u32) -> bool {
        self.success_rate(min_attempts)
            .is_some_and(|rate| rate < threshold)
    }
}

/// Thread-safe tool-use tracker shared across the query loop.
#[derive(Clone)]
pub struct ToolUseTracker {
    inner: Arc<Mutex<HashMap<ModelKey, ModelToolStats>>>,
}

/// Minimum number of turns before we form a trustworthy success-rate signal.
/// With fewer attempts the noise is too high to reliably deprioritize a model.
const MIN_ATTEMPTS: u32 = 3;

/// A model whose tool-use success rate falls below this threshold is flagged
/// as unreliable and deprioritized in auto-switch ranking.
const UNRELIABLE_THRESHOLD: f64 = 0.3;

impl Default for ToolUseTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolUseTracker {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Record the outcome of a turn.
    ///
    /// * `tools_were_available` — `true` when tool definitions were included
    ///   in the request (i.e. the model *could* have used tools).
    /// * `tools_were_used` — `true` when the response contained at least one
    ///   `tool_use` content block.
    pub fn record_turn(
        &self,
        provider: &str,
        model: &str,
        tools_were_available: bool,
        tools_were_used: bool,
    ) {
        if !tools_were_available {
            // No tools sent — nothing to record for tool-use tracking.
            return;
        }
        let key = ModelKey {
            provider: provider.to_string(),
            model: model.to_string(),
        };
        let mut guard = self.inner.lock().unwrap();
        let stats = guard.entry(key).or_default();
        stats.attempts += 1;
        if tools_were_used {
            stats.successes += 1;
        }
        stats.update_ema(tools_were_used);
    }

    /// Whether the given model is flagged as tool-use unreliable (low success
    /// rate with enough data). Used by the auto-switch to increase auto-switch
    /// priority for this model.
    pub fn is_unreliable(&self, provider: &str, model: &str) -> bool {
        let key = ModelKey {
            provider: provider.to_string(),
            model: model.to_string(),
        };
        let guard = self.inner.lock().unwrap();
        guard
            .get(&key)
            .is_some_and(|s| s.is_tool_use_unreliable(UNRELIABLE_THRESHOLD, MIN_ATTEMPTS))
    }

    /// Success rate for a model, or `None` if insufficient data.
    pub fn success_rate(&self, provider: &str, model: &str) -> Option<f64> {
        let key = ModelKey {
            provider: provider.to_string(),
            model: model.to_string(),
        };
        let guard = self.inner.lock().unwrap();
        guard.get(&key).and_then(|s| s.success_rate(MIN_ATTEMPTS))
    }

    /// Snapshot of all tracked models (for diagnostics / `/stats`).
    pub fn snapshot(&self) -> HashMap<ModelKey, ModelToolStats> {
        self.inner.lock().unwrap().clone()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_tools_available_does_not_record() {
        let tracker = ToolUseTracker::new();
        tracker.record_turn("free", "tiny-llama", false, false);
        assert!(tracker.success_rate("free", "tiny-llama").is_none());
        let snap = tracker.snapshot();
        assert!(snap.is_empty());
    }

    #[test]
    fn tracks_successes_and_failures() {
        let tracker = ToolUseTracker::new();

        // Turn 1: tools available, model used them
        tracker.record_turn("free", "deepseek", true, true);
        // Turn 2: tools available, model did NOT use them
        tracker.record_turn("free", "deepseek", true, false);
        // Turn 3: tools available, model used them
        tracker.record_turn("free", "deepseek", true, true);

        let stats = tracker
            .snapshot()
            .get(&ModelKey {
                provider: "free".to_string(),
                model: "deepseek".to_string(),
            })
            .cloned()
            .unwrap();
        assert_eq!(stats.attempts, 3);
        assert_eq!(stats.successes, 2);
        // EMA rate should be above 0.3 threshold → not unreliable
        assert!(!tracker.is_unreliable("free", "deepseek"));
        // EMA with alpha=0.4: 0.5 → 0.7 → 0.42 → 0.652
        let rate = tracker.success_rate("free", "deepseek").unwrap();
        assert!(rate > 0.3, "expected EMA rate > 0.3, got {rate}");
    }

    #[test]
    fn low_success_rate_is_unreliable() {
        let tracker = ToolUseTracker::new();

        // 6 turns with all failures — EMA should be well below 0.3
        for _ in 0..6 {
            tracker.record_turn("free", "tiny-llama", true, false);
        }

        assert!(tracker.is_unreliable("free", "tiny-llama"));
        let rate = tracker.success_rate("free", "tiny-llama").unwrap();
        assert!(rate < 0.3, "expected EMA rate < 0.3, got {rate}");
    }

    #[test]
    fn insufficient_data_not_unreliable() {
        let tracker = ToolUseTracker::new();

        // Only 2 attempts (< MIN_ATTEMPTS = 3) — even 0% is not flagged
        tracker.record_turn("free", "new-model", true, false);
        tracker.record_turn("free", "new-model", true, false);

        assert!(!tracker.is_unreliable("free", "new-model"));
        assert!(tracker.success_rate("free", "new-model").is_none());
    }

    #[test]
    fn different_providers_are_independent() {
        let tracker = ToolUseTracker::new();

        // groq uses tools perfectly
        for _ in 0..5 {
            tracker.record_turn("groq", "llama-3", true, true);
        }
        // free never uses tools
        for _ in 0..5 {
            tracker.record_turn("free", "tiny-llama", true, false);
        }

        assert!(!tracker.is_unreliable("groq", "llama-3"));
        assert!(tracker.is_unreliable("free", "tiny-llama"));
    }

    #[test]
    fn clone_shares_state() {
        let tracker = ToolUseTracker::new();
        let tracker2 = tracker.clone();

        tracker.record_turn("openai", "gpt-4o", true, true);
        tracker.record_turn("openai", "gpt-4o", true, true);

        // Clone sees the same data
        assert_eq!(
            tracker.success_rate("openai", "gpt-4o"),
            tracker2.success_rate("openai", "gpt-4o")
        );
    }

    #[test]
    fn ema_recovers_after_sustained_success() {
        let tracker = ToolUseTracker::new();

        // Phase 1: model is unreliable (5 failures)
        for _ in 0..5 {
            tracker.record_turn("groq", "llama-3.1", true, false);
        }
        assert!(tracker.is_unreliable("groq", "llama-3.1"));
        let bad_rate = tracker.success_rate("groq", "llama-3.1").unwrap();
        assert!(
            bad_rate < 0.3,
            "expected bad EMA rate < 0.3, got {bad_rate}"
        );

        // Phase 2: model starts using tools consistently (10 successes)
        for _ in 0..10 {
            tracker.record_turn("groq", "llama-3.1", true, true);
        }
        // EMA should have recovered above 0.3
        assert!(!tracker.is_unreliable("groq", "llama-3.1"));
        let recovered_rate = tracker.success_rate("groq", "llama-3.1").unwrap();
        assert!(
            recovered_rate > 0.3,
            "expected recovered EMA rate > 0.3, got {recovered_rate}"
        );
    }
}
