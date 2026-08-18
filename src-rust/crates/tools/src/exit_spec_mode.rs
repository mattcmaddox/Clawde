// ExitSpecMode tool: leave spec-driven development mode and begin
// implementation against the accepted spec (audit spec §10.2).

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::debug;

pub struct ExitSpecModeTool;

#[derive(Debug, Deserialize)]
struct ExitSpecModeInput {
    #[serde(default)]
    summary: Option<String>,
}

#[async_trait]
impl Tool for ExitSpecModeTool {
    fn name(&self) -> &str {
        clawde_core::constants::TOOL_NAME_EXIT_SPEC_MODE
    }

    fn description(&self) -> &str {
        "Exit spec-driven development mode and begin implementing against the \
         accepted specification. Runs the spec's acceptance tests during the \
         implementation, reporting progress such as '2/4 tests passing, fixing...' \
         until all acceptance tests pass."
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
                    "description": "Summary of the accepted spec being implemented"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        let params: ExitSpecModeInput =
            serde_json::from_value(input).unwrap_or(ExitSpecModeInput { summary: None });

        debug!(summary = ?params.summary, "Exiting spec mode");

        let msg = if let Some(summary) = &params.summary {
            format!(
                "Exited spec mode. Implementing against the accepted spec: {summary} \
                 — running its acceptance tests until all pass."
            )
        } else {
            "Exited spec mode. Implementing against the accepted spec, running its \
             acceptance tests until all pass."
                .to_string()
        };

        ToolResult::success(msg).with_metadata(json!({
            "type": "exit_spec_mode",
            "summary": params.summary,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn with_summary_includes_summary_and_metadata() {
        let res = ExitSpecModeTool
            .execute(
                json!({ "summary": "ship the parser" }),
                &crate::test_support::allow_all_context(".".into()),
            )
            .await;
        assert!(!res.is_error, "{}", res.content);
        assert!(res.content.contains("ship the parser"), "{}", res.content);
        let meta = res.metadata.unwrap();
        assert_eq!(meta["type"], "exit_spec_mode");
        assert_eq!(meta["summary"], "ship the parser");
    }

    #[tokio::test]
    async fn without_summary_uses_default_message() {
        let res = ExitSpecModeTool
            .execute(
                json!({}),
                &crate::test_support::allow_all_context(".".into()),
            )
            .await;
        assert!(!res.is_error, "{}", res.content);
        assert!(
            res.content.starts_with("Exited spec mode."),
            "{}",
            res.content
        );
        assert_eq!(res.metadata.unwrap()["summary"], Value::Null);
    }
}
