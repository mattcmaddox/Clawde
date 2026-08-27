// `/copy` command.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct CopyCommand;

// ---- /copy ---------------------------------------------------------------

#[async_trait]
impl SlashCommand for CopyCommand {
    fn name(&self) -> &str {
        "copy"
    }
    fn description(&self) -> &str {
        "Copy the last assistant response to the clipboard"
    }
    fn help(&self) -> &str {
        "Usage: /copy [n]\n\n\
         Copies the most recent assistant response to the system clipboard.\n\
         Optionally pass a number to copy the Nth most-recent response."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let n: usize = args.trim().parse().unwrap_or(1).max(1);

        // Find the Nth most recent assistant message
        let assistant_msgs: Vec<&clawde_core::types::Message> = ctx
            .messages
            .iter()
            .rev()
            .filter(|m| m.role == clawde_core::types::Role::Assistant)
            .take(n)
            .collect();

        let msg = match assistant_msgs.last() {
            Some(m) => m,
            None => {
                return CommandResult::Message(
                    "No assistant messages found in conversation.".to_string(),
                )
            }
        };

        let text = msg.get_all_text();
        if text.is_empty() {
            return CommandResult::Message("Last assistant message is empty.".to_string());
        }

        // Try system clipboard via arboard
        #[cfg(not(target_os = "linux"))]
        {
            match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text.clone())) {
                Ok(()) => {
                    let preview: String = text.chars().take(80).collect();
                    let ellipsis = if text.len() > 80 { "…" } else { "" };
                    return CommandResult::Message(format!(
                        "Copied {} chars to clipboard.\nPreview: {}{}",
                        text.len(),
                        preview,
                        ellipsis
                    ));
                }
                Err(e) => {
                    tracing::warn!("Clipboard write failed: {}", e);
                    // Fall through to file fallback
                }
            }
        }

        // Fallback: write to a temp file and inform the user
        let tmp_path = std::env::temp_dir().join("claude_copy.md");
        match std::fs::write(&tmp_path, &text) {
            Ok(()) => {
                let preview: String = text.chars().take(80).collect();
                let ellipsis = if text.len() > 80 { "…" } else { "" };
                CommandResult::Message(format!(
                    "Clipboard not available; saved {} chars to {}\nPreview: {}{}",
                    text.len(),
                    tmp_path.display(),
                    preview,
                    ellipsis
                ))
            }
            Err(e) => CommandResult::Error(format!("Failed to copy: {}", e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawde_core::types::Message;

    fn make_ctx(messages: Vec<Message>) -> CommandContext {
        CommandContext {
            config: clawde_core::config::Config::default(),
            cost_tracker: clawde_core::cost::CostTracker::new(),
            messages,
            working_dir: std::path::PathBuf::from("."),
            session_id: "test-session".to_string(),
            session_title: None,
            remote_session_url: None,
            mcp_manager: None,
            mcp_auth_runner: None,
            provider_registry: None,
            test_provider: None,
            effort: None,
            tool_use_tracker: None,
            autonomy: None,
        }
    }

    /// The clipboard fallback writes `claude_copy.md` into the process temp
    /// dir; remove it before/after so no stale file leaks between tests.
    fn cleanup_clipboard_file() {
        let _ = std::fs::remove_file(std::env::temp_dir().join("claude_copy.md"));
    }

    #[tokio::test]
    async fn copy_without_assistant_messages_is_informative() {
        let mut ctx = make_ctx(vec![Message::user("hello")]);
        match CopyCommand.execute("", &mut ctx).await {
            CommandResult::Message(m) => {
                assert!(m.contains("No assistant messages found"), "{}", m);
            }
            other => panic!("expected Message, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn copy_last_assistant_message() {
        cleanup_clipboard_file();
        let mut ctx = make_ctx(vec![
            Message::user("prompt"),
            Message::assistant("hello world"),
        ]);
        match CopyCommand.execute("", &mut ctx).await {
            CommandResult::Message(m) => {
                // Both the clipboard path and the file fallback preview the text.
                assert!(m.contains("hello world"), "{}", m);
                assert!(m.contains("chars"), "{}", m);
            }
            other => panic!("expected Message, got {:?}", other),
        }
        cleanup_clipboard_file();
    }

    #[tokio::test]
    async fn copy_nth_most_recent_message() {
        cleanup_clipboard_file();
        let mut ctx = make_ctx(vec![
            Message::user("prompt"),
            Message::assistant("first response"),
            Message::assistant("second response"),
        ]);
        match CopyCommand.execute("2", &mut ctx).await {
            CommandResult::Message(m) => {
                assert!(m.contains("first response"), "{}", m);
                assert!(!m.contains("second response"), "{}", m);
            }
            other => panic!("expected Message, got {:?}", other),
        }
        cleanup_clipboard_file();
    }

    #[tokio::test]
    async fn copy_empty_assistant_message_is_informative() {
        let mut ctx = make_ctx(vec![Message::assistant("")]);
        match CopyCommand.execute("", &mut ctx).await {
            CommandResult::Message(m) => {
                assert!(m.contains("Last assistant message is empty"), "{}", m);
            }
            other => panic!("expected Message, got {:?}", other),
        }
    }
}
