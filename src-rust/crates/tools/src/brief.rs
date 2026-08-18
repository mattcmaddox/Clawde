// BriefTool: send a formatted message to the user, optionally with file attachments.
//
// This is the model's way of proactively communicating status, completions, or
// findings without being asked. The message is returned as a tool result and
// the TUI renders it prominently.
//
// Status can be:
//   "normal"    – reply to what the user just said
//   "proactive" – unsolicited update (task done, blocker, status ping)

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;
use tracing::debug;

pub struct BriefTool;

#[derive(Debug, Deserialize)]
struct BriefInput {
    message: String,
    #[serde(default)]
    attachments: Vec<String>,
    #[serde(default = "default_status")]
    status: String,
}

fn default_status() -> String {
    "normal".to_string()
}

#[derive(Debug, Serialize)]
struct AttachmentMeta {
    path: String,
    size: u64,
    is_image: bool,
}

#[async_trait]
impl Tool for BriefTool {
    fn name(&self) -> &str {
        "Brief"
    }

    fn description(&self) -> &str {
        "Send a formatted message to the user, optionally with file attachments. \
         Use status=\"proactive\" when surfacing something the user hasn't asked for \
         (task completion, a blocker, an unsolicited update). \
         Use status=\"normal\" when replying to something the user just said."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "The message to send. Supports Markdown."
                },
                "attachments": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional file paths to attach (images, diffs, logs)"
                },
                "status": {
                    "type": "string",
                    "enum": ["normal", "proactive"],
                    "description": "Use 'proactive' for unsolicited updates, 'normal' for direct replies"
                }
            },
            "required": ["message", "status"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: BriefInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        if params.message.trim().is_empty() {
            return ToolResult::error("Message cannot be empty.".to_string());
        }

        // Resolve and validate attachments
        let mut resolved: Vec<AttachmentMeta> = Vec::new();
        let mut errors: Vec<String> = Vec::new();

        for raw_path in &params.attachments {
            let path = ctx.resolve_path(raw_path);
            match resolve_attachment(&path).await {
                Ok(meta) => resolved.push(meta),
                Err(e) => errors.push(format!("{}: {}", raw_path, e)),
            }
        }

        if !errors.is_empty() {
            return ToolResult::error(format!(
                "Failed to resolve attachments:\n{}",
                errors.join("\n")
            ));
        }

        debug!(
            status = %params.status,
            attachments = resolved.len(),
            "Brief message"
        );

        // Build result payload
        let now = chrono::Utc::now().to_rfc3339();

        let mut result = json!({
            "message": params.message,
            "status": params.status,
            "sentAt": now,
        });

        if !resolved.is_empty() {
            result["attachments"] = serde_json::to_value(&resolved).unwrap_or_default();
        }

        ToolResult::success(params.message).with_metadata(result)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn resolve_attachment(path: &Path) -> Result<AttachmentMeta, String> {
    let meta = tokio::fs::metadata(path).await.map_err(|e| e.to_string())?;

    if !meta.is_file() {
        return Err("not a file".to_string());
    }

    let size = meta.len();
    let is_image = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            matches!(
                e.to_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg"
            )
        })
        .unwrap_or(false);

    Ok(AttachmentMeta {
        path: path.display().to_string(),
        size,
        is_image,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn invalid_input_errors() {
        let res = BriefTool
            .execute(
                json!({ "message": 42 }),
                &crate::test_support::allow_all_context(".".into()),
            )
            .await;
        assert!(res.is_error);
        assert!(res.content.contains("Invalid input"), "{}", res.content);
    }

    #[tokio::test]
    async fn empty_message_errors() {
        let res = BriefTool
            .execute(
                json!({ "message": "  " }),
                &crate::test_support::allow_all_context(".".into()),
            )
            .await;
        assert!(res.is_error);
        assert!(
            res.content.contains("Message cannot be empty"),
            "{}",
            res.content
        );
    }

    #[tokio::test]
    async fn missing_attachment_errors() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::test_support::allow_all_context(dir.path().to_path_buf());
        let res = BriefTool
            .execute(
                json!({ "message": "hi", "attachments": ["nope.txt"] }),
                &ctx,
            )
            .await;
        assert!(res.is_error);
        assert!(
            res.content.contains("Failed to resolve attachments"),
            "{}",
            res.content
        );
        assert!(res.content.contains("nope.txt"), "{}", res.content);
    }

    #[tokio::test]
    async fn image_attachment_is_flagged() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shot.png"), b"fake-png-bytes").unwrap();
        let ctx = crate::test_support::allow_all_context(dir.path().to_path_buf());
        let res = BriefTool
            .execute(
                json!({ "message": "here is the screenshot", "attachments": ["shot.png"] }),
                &ctx,
            )
            .await;
        assert!(!res.is_error, "{}", res.content);
        let meta = res.metadata.unwrap();
        assert_eq!(meta["message"], "here is the screenshot");
        assert_eq!(meta["status"], "normal");
        let att = &meta["attachments"][0];
        assert_eq!(att["is_image"], true);
        assert_eq!(
            att["path"],
            dir.path().join("shot.png").display().to_string()
        );
    }

    #[tokio::test]
    async fn text_attachment_and_proactive_status() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("log.txt"), "some log output").unwrap();
        let ctx = crate::test_support::allow_all_context(dir.path().to_path_buf());
        let res = BriefTool
            .execute(
                json!({ "message": "build finished", "status": "proactive", "attachments": ["log.txt"] }),
                &ctx,
            )
            .await;
        assert!(!res.is_error, "{}", res.content);
        assert_eq!(res.content, "build finished");
        let meta = res.metadata.unwrap();
        assert_eq!(meta["status"], "proactive");
        assert!(meta["sentAt"].is_string());
        let att = &meta["attachments"][0];
        assert_eq!(att["is_image"], false);
        assert_eq!(att["size"], 15);
    }
}
