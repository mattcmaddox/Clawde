// SyntheticOutputTool: Used by coordinator / non-interactive sessions to emit
// structured output that gets captured and displayed as the assistant's final
// response.
//
// Tool name: "StructuredOutput" (matches TypeScript SYNTHETIC_OUTPUT_TOOL_NAME)
//
// Input:  any JSON object (pass-through schema)
// Output: "Structured output provided successfully" (the real effect is that
//         the caller captures `input` as the structured result)
//
// This tool is only surfaced to the model in non-interactive / SDK sessions.
// In the Rust port it simply validates that it received a JSON object and
// echoes back the confirmation string, matching the TS call() behaviour.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct SyntheticOutputTool;

#[async_trait]
impl Tool for SyntheticOutputTool {
    fn name(&self) -> &str {
        "StructuredOutput"
    }

    fn description(&self) -> &str {
        "Return structured output in the requested format. \
         Use this tool to return your final response as structured JSON. \
         You MUST call this tool exactly once at the end of your response \
         to provide the structured output."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn input_schema(&self) -> Value {
        // Accept any object (pass-through) — the schema is intentionally open
        // so that callers can configure it dynamically for their output format.
        json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        // Validate that we at least received a JSON object
        if !input.is_object() {
            return ToolResult::error(
                "StructuredOutput requires a JSON object as input.".to_string(),
            );
        }

        // Return the confirmation string matching the TS implementation.
        // Callers that need the raw structured data should inspect the `input`
        // Value before it is serialised — the tool result here is purely for
        // the model's benefit.
        ToolResult::success("Structured output provided successfully")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn object_input_succeeds() {
        let res = SyntheticOutputTool
            .execute(
                json!({ "result": { "answer": 42 } }),
                &crate::test_support::allow_all_context(".".into()),
            )
            .await;
        assert!(!res.is_error, "{}", res.content);
        assert_eq!(res.content, "Structured output provided successfully");
    }

    #[tokio::test]
    async fn non_object_input_errors() {
        for bad in [json!([1, 2, 3]), json!("string"), json!(42), json!(null)] {
            let res = SyntheticOutputTool
                .execute(bad, &crate::test_support::allow_all_context(".".into()))
                .await;
            assert!(res.is_error, "expected error, got: {}", res.content);
            assert!(
                res.content.contains("requires a JSON object"),
                "{}",
                res.content
            );
        }
    }
}
