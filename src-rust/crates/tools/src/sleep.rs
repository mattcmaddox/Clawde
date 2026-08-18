// SleepTool: pause execution for a specified duration.
//
// Useful when the model needs to wait between operations (e.g., polling,
// rate limiting, or waiting for external processes). Unlike `Bash(sleep ...)`,
// this does not hold a shell process and can run concurrently with other tools.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use tracing::debug;

pub struct SleepTool;

#[derive(Debug, Deserialize)]
struct SleepInput {
    /// Duration in milliseconds (capped at 300_000 = 5 minutes).
    #[serde(alias = "ms", alias = "duration_ms")]
    ms: u64,
}

#[async_trait]
impl Tool for SleepTool {
    fn name(&self) -> &str {
        "Sleep"
    }

    fn description(&self) -> &str {
        "Wait for a specified duration in milliseconds. \
         Use instead of Bash(sleep ...) — it doesn't hold a shell process \
         and can run concurrently with other tools. \
         The user can interrupt the sleep at any time."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ms": {
                    "type": "number",
                    "description": "Duration to sleep in milliseconds (max 300000 = 5 minutes)"
                }
            },
            "required": ["ms"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        let params: SleepInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        // Cap at 5 minutes
        let duration_ms = params.ms.min(300_000);
        debug!(ms = duration_ms, "Sleeping");

        tokio::time::sleep(Duration::from_millis(duration_ms)).await;

        ToolResult::success(format!("Slept for {}ms.", duration_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_ms_errors() {
        let res = SleepTool
            .execute(
                json!({}),
                &crate::test_support::allow_all_context(".".into()),
            )
            .await;
        assert!(res.is_error);
        assert!(res.content.contains("Invalid input"), "{}", res.content);
    }

    #[tokio::test]
    async fn accepts_ms_and_alias_fields() {
        for input in [json!({ "ms": 0 }), json!({ "duration_ms": 0 })] {
            let res = SleepTool
                .execute(input, &crate::test_support::allow_all_context(".".into()))
                .await;
            assert!(!res.is_error, "{}", res.content);
            assert_eq!(res.content, "Slept for 0ms.");
        }
    }

    #[tokio::test]
    async fn short_sleep_reports_elapsed_ms() {
        let res = SleepTool
            .execute(
                json!({ "ms": 1 }),
                &crate::test_support::allow_all_context(".".into()),
            )
            .await;
        assert!(!res.is_error, "{}", res.content);
        assert_eq!(res.content, "Slept for 1ms.");
    }
}
