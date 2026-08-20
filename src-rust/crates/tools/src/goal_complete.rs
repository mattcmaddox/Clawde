// GoalCompleteTool — marks the active goal as complete.
//
// This is the tool the model calls after passing a self-audit that verifies
// the goal objective has been fully achieved.  Calling it without a thorough
// audit_summary + evidence is considered a violation of the goal contract.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

pub struct GoalCompleteTool;

#[derive(Debug, Deserialize)]
struct GoalCompleteInput {
    /// A concise summary of what was accomplished (the audit).
    audit_summary: String,
    /// Concrete evidence: test output, file diffs, command results, etc.
    evidence: String,
}

#[async_trait]
impl Tool for GoalCompleteTool {
    fn name(&self) -> &str {
        "GoalComplete"
    }

    fn description(&self) -> &str {
        "Mark the active goal as complete. ONLY call this after a genuine completion audit:\n\
         1. Restate the goal as concrete deliverables.\n\
         2. Check each deliverable against real output, test results, or file diffs.\n\
         3. Confirm all deliverables are satisfied.\n\
         Calling this without a real audit is a goal contract violation."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn stateful(&self) -> bool {
        true
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "audit_summary": {
                    "type": "string",
                    "description": "Concise summary of what was accomplished and verified"
                },
                "evidence": {
                    "type": "string",
                    "description": "Concrete evidence of completion: test output, diffs, command results"
                }
            },
            "required": ["audit_summary", "evidence"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: GoalCompleteInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        if params.audit_summary.trim().is_empty() {
            return ToolResult::error(
                "audit_summary cannot be empty. Provide a concise description of what was completed."
                    .to_string(),
            );
        }
        if params.evidence.trim().is_empty() {
            return ToolResult::error(
                "evidence cannot be empty. Provide test output, diffs, or command results."
                    .to_string(),
            );
        }

        let session_id = &ctx.session_id;

        match clawde_core::GoalStore::open_default() {
            None => ToolResult::error("Could not open goal store.".to_string()),
            Some(store) => match store.set_status(session_id, clawde_core::GoalStatus::Complete) {
                Ok(()) => ToolResult::success(format!(
                    "Goal marked complete.\n\nAudit summary: {}\n\nEvidence: {}",
                    params.audit_summary, params.evidence,
                )),
                Err(e) => ToolResult::error(format!(
                    "Failed to mark goal complete: {}. \
                     There may be no active goal for this session.",
                    e
                )),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawde_core::{GoalStatus, GoalStore};

    /// Run a future with `CLAWDE_HOME` pointed at a fresh temp dir so goal
    /// DB reads/writes never touch the real config dir (and never race other
    /// env-mutating tests under parallelism). Serializes on the crate-wide
    /// [`crate::TEST_ENV_LOCK`] so all env-mutating tests in this crate share
    /// one mutex (AGENTS.md parallel-safe tests).
    #[allow(clippy::await_holding_lock)]
    // The guard must span the whole future: it serialises the CLAWDE_HOME
    // mutation against other env-mutating tests (same std::sync::Mutex
    // convention as crate::paths::ENV_LOCK). Test-only, single acquisition.
    async fn with_temp_home<T>(f: impl FnOnce(std::path::PathBuf) -> T) -> T::Output
    where
        T: std::future::Future,
    {
        let _lock = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("CLAWDE_HOME", dir.path());
        let out = f(dir.path().to_path_buf()).await;
        std::env::remove_var("CLAWDE_HOME");
        out
    }

    #[tokio::test]
    async fn invalid_input_errors() {
        let res = GoalCompleteTool
            .execute(
                json!({ "audit_summary": "only summary" }),
                &crate::test_support::allow_all_context(".".into()),
            )
            .await;
        assert!(res.is_error);
        assert!(res.content.contains("Invalid input"), "{}", res.content);
    }

    #[tokio::test]
    async fn empty_audit_summary_errors() {
        let res = GoalCompleteTool
            .execute(
                json!({ "audit_summary": "   ", "evidence": "tests pass" }),
                &crate::test_support::allow_all_context(".".into()),
            )
            .await;
        assert!(res.is_error);
        assert!(
            res.content.contains("audit_summary cannot be empty"),
            "{}",
            res.content
        );
    }

    #[tokio::test]
    async fn empty_evidence_errors() {
        let res = GoalCompleteTool
            .execute(
                json!({ "audit_summary": "done", "evidence": "" }),
                &crate::test_support::allow_all_context(".".into()),
            )
            .await;
        assert!(res.is_error);
        assert!(
            res.content.contains("evidence cannot be empty"),
            "{}",
            res.content
        );
    }

    #[tokio::test]
    async fn marks_active_goal_complete() {
        with_temp_home(|home| async move {
            let store = GoalStore::open_default().expect("goal store");
            store
                .set_goal("eol-test", "finish the feature", None, 0)
                .expect("set goal");

            let ctx = crate::test_support::allow_all_context(home);
            let res = GoalCompleteTool
                .execute(
                    json!({ "audit_summary": "all deliverables met", "evidence": "3 tests pass" }),
                    &ctx,
                )
                .await;
            assert!(!res.is_error, "{}", res.content);
            assert!(
                res.content.contains("Goal marked complete"),
                "{}",
                res.content
            );
            assert!(
                res.content.contains("all deliverables met"),
                "{}",
                res.content
            );

            let goal = store.get_goal("eol-test").expect("goal exists");
            assert_eq!(goal.status, GoalStatus::Complete);
        })
        .await;
    }
}
