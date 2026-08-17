//! MCP tool wrappers shared by the CLI and ACP runtimes.

use crate::{PermissionLevel, Tool, ToolContext, ToolErrorCode, ToolResult};
use async_trait::async_trait;
use clawde_core::types::ToolDefinition;
use serde_json::Value;
use std::sync::Arc;

/// Expose one connected MCP tool as a normal Clawde tool.
pub struct McpToolWrapper {
    tool_def: ToolDefinition,
    server_name: String,
    manager: Arc<clawde_mcp::McpManager>,
}

#[async_trait]
impl Tool for McpToolWrapper {
    fn name(&self) -> &str {
        &self.tool_def.name
    }

    fn description(&self) -> &str {
        &self.tool_def.description
    }

    fn permission_level(&self) -> PermissionLevel {
        // MCP tools may launch external processes or make network requests.
        PermissionLevel::Execute
    }

    fn network_capable(&self) -> bool {
        // MCP servers are arbitrary external integrations; their tool metadata
        // cannot prove that a call is offline-safe.
        true
    }

    fn self_gates(&self) -> bool {
        // execute() performs the network boundary and permission check. Avoid
        // the shared dispatcher prompting once more before it runs.
        true
    }

    fn input_schema(&self) -> Value {
        self.tool_def.input_schema.clone()
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let desc = format!("Run MCP tool {}", self.tool_def.name);
        if let Err(error) = ctx.ensure_network_allowed_for_tool(self.name(), true) {
            return ToolResult::error_with_code(
                ToolErrorCode::NetworkIsolationBlocked,
                error.to_string(),
            );
        }
        if let Err(error) = ctx.check_permission(self.name(), &desc, false) {
            return ToolResult::error_with_code(ToolErrorCode::PermissionDenied, error.to_string());
        }

        let prefix = format!("{}_", self.server_name);
        let bare_name = self
            .tool_def
            .name
            .strip_prefix(&prefix)
            .unwrap_or(&self.tool_def.name);
        let args = if input.is_null() { None } else { Some(input) };

        match self.manager.call_tool(&self.tool_def.name, args).await {
            Ok(result) => {
                let text = clawde_mcp::mcp_result_to_string(&result);
                if result.is_error {
                    ToolResult::error_with_code(ToolErrorCode::ExecutionFailed, text)
                } else {
                    ToolResult::success(text)
                }
            }
            Err(error) => ToolResult::error_with_code(
                ToolErrorCode::ExecutionFailed,
                format!("MCP tool '{}' failed: {}", bare_name, error),
            ),
        }
    }
}

/// Build wrappers for all tools exposed by a connected MCP manager.
pub fn mcp_tool_wrappers(manager: Arc<clawde_mcp::McpManager>) -> Vec<Box<dyn Tool>> {
    manager
        .all_tool_definitions()
        .into_iter()
        .map(|(server_name, tool_def)| {
            Box::new(McpToolWrapper {
                tool_def,
                server_name,
                manager: manager.clone(),
            }) as Box<dyn Tool>
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{mcp_tool_wrappers, McpToolWrapper};
    use crate::{Tool, ToolErrorCode};
    use clawde_core::types::ToolDefinition;
    use std::sync::Arc;

    #[test]
    fn empty_mcp_manager_exposes_no_wrappers() {
        let manager = Arc::new(clawde_mcp::McpManager::new());
        assert!(mcp_tool_wrappers(manager).is_empty());
    }

    #[test]
    fn mcp_wrapper_error_categories_are_stable() {
        assert_eq!(
            ToolErrorCode::NetworkIsolationBlocked.as_str(),
            "network_isolation_blocked"
        );
        assert_eq!(
            ToolErrorCode::PermissionDenied.as_str(),
            "permission_denied"
        );
        assert_eq!(ToolErrorCode::ExecutionFailed.as_str(), "execution_failed");
    }

    #[test]
    fn mcp_wrapper_self_gates_to_avoid_double_prompting() {
        let wrapper = McpToolWrapper {
            tool_def: ToolDefinition {
                name: "server_tool".to_string(),
                description: "test".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
            },
            server_name: "server".to_string(),
            manager: Arc::new(clawde_mcp::McpManager::new()),
        };
        assert!(wrapper.self_gates());
    }
}
