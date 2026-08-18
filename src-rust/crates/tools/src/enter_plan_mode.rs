// EnterPlanMode tool: switch the session into planning (read-only) mode.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::debug;

pub struct EnterPlanModeTool;

#[derive(Debug, Deserialize)]
struct EnterPlanModeInput {
    #[serde(default)]
    reason: Option<String>,
}

#[async_trait]
impl Tool for EnterPlanModeTool {
    fn name(&self) -> &str {
        clawde_core::constants::TOOL_NAME_ENTER_PLAN_MODE
    }

    fn description(&self) -> &str {
        "Enter plan mode. In plan mode, the assistant can only read files and \
         think, but cannot execute commands or write files. Use this to step back \
         and plan a complex change before implementing it."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "reason": {
                    "type": "string",
                    "description": "Why you want to enter plan mode"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        let params: EnterPlanModeInput =
            serde_json::from_value(input).unwrap_or(EnterPlanModeInput { reason: None });

        debug!(reason = ?params.reason, "Entering plan mode");

        let msg = if let Some(reason) = &params.reason {
            format!("Entered plan mode: {}", reason)
        } else {
            "Entered plan mode. Only read-only operations are allowed.".to_string()
        };

        ToolResult::success(msg).with_metadata(json!({
            "type": "enter_plan_mode",
            "reason": params.reason,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn with_reason_includes_reason_and_metadata() {
        let res = EnterPlanModeTool
            .execute(
                json!({ "reason": "tread carefully" }),
                &crate::test_support::allow_all_context(".".into()),
            )
            .await;
        assert!(!res.is_error, "{}", res.content);
        assert_eq!(res.content, "Entered plan mode: tread carefully");
        let meta = res.metadata.unwrap();
        assert_eq!(meta["type"], "enter_plan_mode");
        assert_eq!(meta["reason"], "tread carefully");
    }

    #[tokio::test]
    async fn without_reason_uses_default_message() {
        let res = EnterPlanModeTool
            .execute(
                json!({}),
                &crate::test_support::allow_all_context(".".into()),
            )
            .await;
        assert!(!res.is_error, "{}", res.content);
        assert_eq!(
            res.content,
            "Entered plan mode. Only read-only operations are allowed."
        );
        assert_eq!(res.metadata.unwrap()["reason"], Value::Null);
    }
}
