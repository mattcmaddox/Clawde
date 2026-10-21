//! MCP tool wrappers shared by the CLI and ACP runtimes.

use crate::{PermissionLevel, Tool, ToolContext, ToolErrorCode, ToolResult};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

/// Expose one connected MCP tool as a normal Clawde tool.
pub struct McpToolWrapper {
    binding: clawde_mcp::McpToolBinding,
    manager: Arc<clawde_mcp::McpManager>,
}

#[async_trait]
impl Tool for McpToolWrapper {
    fn name(&self) -> &str {
        &self.binding.public_name
    }

    fn description(&self) -> &str {
        &self.binding.definition.description
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
        self.binding.definition.input_schema.clone()
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let desc = format!("Run MCP tool {}", self.binding.public_name);
        if let Err(error) = ctx.ensure_network_allowed_for_tool(self.name(), true) {
            return ToolResult::error_with_code(
                ToolErrorCode::NetworkIsolationBlocked,
                error.to_string(),
            );
        }
        if let Err(error) = ctx.check_permission_for_tool(self, &desc, false) {
            return ToolResult::error_with_code(ToolErrorCode::PermissionDenied, error.to_string());
        }

        let bare_name = &self.binding.remote_name;
        let args = if input.is_null() { None } else { Some(input) };

        match self.manager.call_tool_binding(&self.binding, args).await {
            Ok(result) => {
                let text = clawde_mcp::mcp_result_to_string(&result);
                let images = mcp_result_images(&result);
                if result.is_error {
                    ToolResult::error_with_code(ToolErrorCode::ExecutionFailed, text)
                } else {
                    // MCP image content (screenshots, rendered charts, page
                    // captures) reaches the model as image blocks; a text
                    // preview of base64 is not something a model can look at.
                    ToolResult::success(text).with_images(images)
                }
            }
            Err(error) => ToolResult::error_with_code(
                ToolErrorCode::ExecutionFailed,
                format!("MCP tool '{}' failed: {}", bare_name, error),
            ),
        }
    }
}

/// Image content blocks from an MCP tool result, ready to attach to the
/// model-facing result.
fn mcp_result_images(result: &clawde_mcp::CallToolResult) -> Vec<clawde_core::types::ImageSource> {
    result
        .content
        .iter()
        .filter_map(|content| match content {
            clawde_mcp::McpContent::Image { data, mime_type } => {
                Some(clawde_core::types::ImageSource {
                    source_type: "base64".to_string(),
                    media_type: Some(mime_type.clone()),
                    data: Some(data.clone()),
                    url: None,
                })
            }
            _ => None,
        })
        .collect()
}

/// Build wrappers for all tools exposed by a connected MCP manager.
pub fn mcp_tool_wrappers(manager: Arc<clawde_mcp::McpManager>) -> Vec<Box<dyn Tool>> {
    manager
        .try_tool_bindings()
        .unwrap_or_else(|error| {
            tracing::error!(error = %error, "MCP tool wrappers disabled because bindings are ambiguous");
            Vec::new()
        })
        .into_iter()
        .map(|binding| {
            Box::new(McpToolWrapper {
                binding,
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
            binding: clawde_mcp::McpToolBinding {
                public_name: "server_tool".to_string(),
                server_name: "server".to_string(),
                remote_name: "tool".to_string(),
                definition: ToolDefinition {
                    name: "server_tool".to_string(),
                    description: "test".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                },
            },
            manager: Arc::new(clawde_mcp::McpManager::new()),
        };
        assert!(wrapper.self_gates());
    }

    /// Image content from an MCP server (a screenshot tool, a rendered chart)
    /// becomes a real image block: the text preview of base64 that the string
    /// form carries is unreadable to the model.
    #[test]
    fn mcp_image_content_becomes_an_image_source() {
        let result = clawde_mcp::CallToolResult {
            content: vec![
                clawde_mcp::McpContent::Text {
                    text: "captured the window".to_string(),
                },
                clawde_mcp::McpContent::Image {
                    data: "AAAA".to_string(),
                    mime_type: "image/png".to_string(),
                },
            ],
            is_error: false,
        };

        let images = super::mcp_result_images(&result);

        assert_eq!(images.len(), 1, "only image content becomes an image");
        assert_eq!(images[0].source_type, "base64");
        assert_eq!(images[0].media_type.as_deref(), Some("image/png"));
        assert_eq!(images[0].data.as_deref(), Some("AAAA"));
    }
}
