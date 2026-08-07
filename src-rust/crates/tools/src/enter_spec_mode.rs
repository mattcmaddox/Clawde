// EnterSpecMode tool: switch the session into spec-driven development mode
// (audit spec §10). In spec mode the agent generates a structured spec
// (requirements, file plan, data models, acceptance tests, edge cases) for a
// non-trivial task BEFORE writing any code, and the user reviews/approves it.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::debug;

pub struct EnterSpecModeTool;

#[derive(Debug, Deserialize)]
struct EnterSpecModeInput {
    #[serde(default)]
    task: Option<String>,
}

#[async_trait]
impl Tool for EnterSpecModeTool {
    fn name(&self) -> &str {
        clawde_core::constants::TOOL_NAME_ENTER_SPEC_MODE
    }

    fn description(&self) -> &str {
        "Enter spec-driven development mode. For non-trivial tasks, generate a \
         structured specification (requirements, files to touch, data models, \
         acceptance tests, edge cases) and write it to specs/<title>.json BEFORE \
         writing any code. Wait for the user to review and accept the spec before \
         implementing against it."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The task to write a specification for"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        let params: EnterSpecModeInput =
            serde_json::from_value(input).unwrap_or(EnterSpecModeInput { task: None });

        debug!(task = ?params.task, "Entering spec mode");

        let msg = if let Some(task) = &params.task {
            format!(
                "Entered spec mode. Generate a structured specification for: {task} \
                 — requirements, files to touch, data models, acceptance tests, and \
                 edge cases — and write it to specs/<title>.json before implementing."
            )
        } else {
            "Entered spec mode. Generate a structured specification (requirements, \
             file plan, data models, acceptance tests, edge cases) and write it to \
             specs/<title>.json before writing any code."
                .to_string()
        };

        ToolResult::success(msg).with_metadata(json!({
            "type": "enter_spec_mode",
            "task": params.task,
        }))
    }
}
