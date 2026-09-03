//! Drift metrics for the task-state eval.
//!
//! Goal: make "did `<task_context>` reduce drift?" measurable. Drift here is
//! operational, not semantic — computed from the same `TaskState` projection
//! the loop already maintains, so an eval needs only the finished session's
//! transcript to score a run, with no model call:
//!
//! * `scope_expansions`  — user turns classified as growing scope ("also…")
//!   after the task started. High values mean the model probed for more work
//!   than asked (or the task was underspecified; the A/B design holds the
//!   prompt fixed so the difference is attributable to the treatment).
//! * `repeated_failures_per_target` — the same failing call retried. The
//!   classic focus failure: forgetting what already failed.
//! * `failed_tools` — absolute error count (volume signal).
//! * `files_touched` — blast radius beyond what the task needed.
//!
//! The composite is a weighted sum, deterministic and model-free, so two runs
//! of the same scenario are comparable and the A/B harness can average across
//! repeats. Weights are unit-tested invariants, not tuning knobs — if they
//! need tuning, the metric is measuring the wrong thing.

use crate::task_state::TaskState;

/// Weights for the composite drift score. Pub so the eval runner and tests
/// share one source of truth.
pub struct DriftWeights;

impl DriftWeights {
    pub const SCOPE_EXPANSION: f64 = 3.0;
    pub const REPEATED_FAILURE: f64 = 2.0;
    pub const FAILED_TOOL: f64 = 1.0;
    pub const FILES_TOUCHED: f64 = 0.25;
}

/// Operational drift metrics extracted from a finished run's `TaskState`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DriftMetrics {
    pub scope_expansions: usize,
    pub repeated_failures_per_target: usize,
    pub failed_tools: usize,
    pub files_touched: usize,
    /// Weighted composite; see [`drift_score`].
    pub score: f64,
}

impl DriftMetrics {
    pub fn from_state(state: &TaskState) -> Self {
        Self {
            scope_expansions: state.complexity.scope_expansions,
            repeated_failures_per_target: state.complexity.repeated_failures_per_target,
            failed_tools: state.complexity.failed_tools,
            files_touched: state.complexity.files_touched,
            score: 0.0,
        }
        .with_score()
    }

    fn with_score(mut self) -> Self {
        self.score = drift_score(
            self.scope_expansions,
            self.repeated_failures_per_target,
            self.failed_tools,
            self.files_touched,
        );
        self
    }
}

/// Composite drift score. Deterministic; higher is more drift.
pub fn drift_score(
    scope_expansions: usize,
    repeated_failures: usize,
    failed_tools: usize,
    files_touched: usize,
) -> f64 {
    scope_expansions as f64 * DriftWeights::SCOPE_EXPANSION
        + repeated_failures as f64 * DriftWeights::REPEATED_FAILURE
        + failed_tools as f64 * DriftWeights::FAILED_TOOL
        + files_touched as f64 * DriftWeights::FILES_TOUCHED
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_state::TaskState;
    use clawde_core::types::{ContentBlock, Message, ToolResultContent};

    fn failed_tool_result(id: &str, summary: &str) -> Message {
        Message::user_blocks(vec![ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content: ToolResultContent::Text(summary.to_string()),
            is_error: Some(true),
        }])
    }

    #[test]
    fn clean_run_scores_zero() {
        let state = TaskState::from_messages(&[Message::user("Fix the login bug")]);
        let metrics = DriftMetrics::from_state(&state);
        assert_eq!(metrics.scope_expansions, 0);
        assert_eq!(metrics.failed_tools, 0);
        assert_eq!(metrics.score, 0.0);
    }

    #[test]
    fn scope_expansion_and_failures_weight_the_score() {
        // Two scope-expansion turns + two repeated failures + one failed tool.
        let mut state = TaskState::from_messages(&[Message::user("Build the CLI parser")]);
        state.apply_message(&Message::user("Also add a config file reader"));
        state.apply_message(&Message::user("And then a settings exporter"));
        for id in ["t1", "t2", "t3"] {
            state.apply_message(&failed_tool_result(id, "connection refused"));
        }
        let metrics = DriftMetrics::from_state(&state);
        assert_eq!(metrics.scope_expansions, 2);
        assert_eq!(metrics.repeated_failures_per_target, 3);
        assert_eq!(metrics.failed_tools, 3);
        let expected = 2.0 * DriftWeights::SCOPE_EXPANSION
            + 3.0 * DriftWeights::REPEATED_FAILURE
            + 3.0 * DriftWeights::FAILED_TOOL;
        assert!((metrics.score - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn metrics_serialize_for_the_eval_runner() {
        let mut state = TaskState::from_messages(&[Message::user("Build the CLI parser")]);
        state.apply_message(&Message::user("Also add a config file reader"));
        let json = serde_json::to_string(&DriftMetrics::from_state(&state)).unwrap();
        assert!(json.contains("\"scope_expansions\":1"));
        assert!(json.contains("\"score\""));
    }
}
