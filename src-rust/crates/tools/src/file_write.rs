// FileWrite tool: write/create files.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::debug;

pub struct FileWriteTool;

#[derive(Debug, Deserialize)]
struct FileWriteInput {
    file_path: String,
    content: String,
}

#[async_trait]
impl Tool for FileWriteTool {
    // Gates itself: calls `ctx.check_permission` in `execute()` (#210).
    fn self_gates(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        clawde_core::constants::TOOL_NAME_FILE_WRITE
    }

    fn description(&self) -> &str {
        "Writes a file to the local filesystem. This tool will overwrite the existing \
         file if there is one. Prefer the Edit tool for modifying existing files. \
         Only use this tool to create new files or for complete rewrites."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the file to write"
                },
                "content": {
                    "type": "string",
                    "description": "The content to write to the file"
                }
            },
            "required": ["file_path", "content"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: FileWriteInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let path = ctx.resolve_path(&params.file_path);
        debug!(path = %path.display(), "Writing file");

        // Permission check
        if let Err(e) =
            ctx.check_permission_for_tool(self, &format!("Write {}", path.display()), false)
        {
            return ToolResult::error(e.to_string());
        }

        // Ensure parent directories exist
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    return ToolResult::error(format!(
                        "Failed to create directory {}: {}",
                        parent.display(),
                        e
                    ));
                }
            }
        }

        let existed = path.exists();
        let before_content = if existed {
            match tokio::fs::read(&path).await {
                Ok(bytes) => bytes,
                Err(e) => {
                    return ToolResult::error(format!(
                        "Failed to read existing file {}: {}",
                        path.display(),
                        e
                    ))
                }
            }
        } else {
            Vec::new()
        };
        let is_new = !existed;

        // Write the file
        if let Err(e) = crate::write_atomic(&path, params.content.as_bytes()).await {
            return ToolResult::error(format!("Failed to write file {}: {}", path.display(), e));
        }

        ctx.record_file_change(
            path.clone(),
            &before_content,
            params.content.as_bytes(),
            self.name(),
        );

        // Run any configured formatter for this file type.
        crate::try_format_file(&path.to_string_lossy(), ctx).await;

        let line_count = params.content.lines().count();
        let byte_count = params.content.len();

        let action = if is_new { "Created" } else { "Wrote" };
        ToolResult::success(format!(
            "{} {} ({} lines, {} bytes)",
            action,
            path.display(),
            line_count,
            byte_count
        ))
        .with_metadata(json!({
            "file_path": path.display().to_string(),
            "is_new": is_new,
            "lines": line_count,
            "bytes": byte_count,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::allow_all_context;
    use serde_json::json;

    #[tokio::test]
    async fn write_creates_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.txt");

        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = FileWriteTool
            .execute(
                json!({
                    "file_path": path.to_string_lossy(),
                    "content": "hello world"
                }),
                &ctx,
            )
            .await;

        assert!(!res.is_error, "write failed: {}", res.content);
        assert!(path.exists(), "file should exist");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world");
    }

    #[tokio::test]
    async fn write_overwrites_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing.txt");
        std::fs::write(&path, "original content").unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = FileWriteTool
            .execute(
                json!({
                    "file_path": path.to_string_lossy(),
                    "content": "replaced content"
                }),
                &ctx,
            )
            .await;

        assert!(!res.is_error, "overwrite failed: {}", res.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "replaced content");
    }

    #[tokio::test]
    async fn write_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subdir").join("nested").join("deep.txt");

        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = FileWriteTool
            .execute(
                json!({
                    "file_path": path.to_string_lossy(),
                    "content": "nested file content"
                }),
                &ctx,
            )
            .await;

        assert!(!res.is_error, "write failed: {}", res.content);
        assert!(path.exists(), "nested file should exist");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "nested file content"
        );
    }

    #[tokio::test]
    async fn write_empty_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.txt");

        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = FileWriteTool
            .execute(
                json!({
                    "file_path": path.to_string_lossy(),
                    "content": ""
                }),
                &ctx,
            )
            .await;

        assert!(!res.is_error, "write failed: {}", res.content);
        assert!(path.exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    }

    #[tokio::test]
    async fn write_returns_line_and_byte_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multi_line.txt");
        let content = "line one\nline two\nline three\n";

        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = FileWriteTool
            .execute(
                json!({
                    "file_path": path.to_string_lossy(),
                    "content": content
                }),
                &ctx,
            )
            .await;

        assert!(!res.is_error, "write failed: {}", res.content);
        assert!(
            res.content.contains("3 lines"),
            "expected 3 lines, got: {}",
            res.content
        );
        assert!(
            res.content.contains("bytes"),
            "expected byte count, got: {}",
            res.content
        );
        assert!(
            res.content.contains("Created"),
            "expected 'Created', got: {}",
            res.content
        );
    }

    #[tokio::test]
    async fn write_invalid_input_missing_fields() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = FileWriteTool
            .execute(json!({"file_path": "some/path.txt"}), &ctx)
            .await;

        assert!(res.is_error, "expected error for missing content");
        assert!(res.content.contains("Invalid input"));
    }
}
