// ExitPlanMode tool: leave planning mode and return to normal execution.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::debug;

pub struct ExitPlanModeTool;

#[derive(Debug, Deserialize)]
struct ExitPlanModeInput {
    #[serde(default)]
    summary: Option<String>,
}

#[async_trait]
impl Tool for ExitPlanModeTool {
    fn name(&self) -> &str {
        clawde_core::constants::TOOL_NAME_EXIT_PLAN_MODE
    }

    fn description(&self) -> &str {
        "Exit plan mode and return to normal execution mode where all tools \
         are available. Optionally provide a summary of the plan."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "Summary of the plan you developed"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        let params: ExitPlanModeInput =
            serde_json::from_value(input).unwrap_or(ExitPlanModeInput { summary: None });

        debug!(summary = ?params.summary, "Exiting plan mode");

        let msg = if let Some(summary) = &params.summary {
            format!("Exited plan mode. Plan summary: {}", summary)
        } else {
            "Exited plan mode. All tools are now available.".to_string()
        };

        ToolResult::success(msg).with_metadata(json!({
            "type": "exit_plan_mode",
            "summary": params.summary,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn with_summary_includes_summary_and_metadata() {
        let res = ExitPlanModeTool
            .execute(
                json!({ "summary": "refactor the parser" }),
                &crate::test_support::allow_all_context(".".into()),
            )
            .await;
        assert!(!res.is_error, "{}", res.content);
        assert_eq!(
            res.content,
            "Exited plan mode. Plan summary: refactor the parser"
        );
        let meta = res.metadata.unwrap();
        assert_eq!(meta["type"], "exit_plan_mode");
        assert_eq!(meta["summary"], "refactor the parser");
    }

    #[tokio::test]
    async fn without_summary_uses_default_message() {
        let res = ExitPlanModeTool
            .execute(
                json!({}),
                &crate::test_support::allow_all_context(".".into()),
            )
            .await;
        assert!(!res.is_error, "{}", res.content);
        assert_eq!(
            res.content,
            "Exited plan mode. All tools are now available."
        );
        assert_eq!(res.metadata.unwrap()["summary"], Value::Null);
    }
}
