// `/export` command.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct ExportCommand;

// ---- /export -------------------------------------------------------------

/// Format a single `Message` as a Markdown section.
///
/// User messages render as `## User\n<text>`.
/// Assistant messages render as `## Assistant\n<text>` followed by
/// `### Tool: <name>\n**Input:** …\n**Output:** …` for each tool call pair.
fn export_message_to_markdown(
    msg: &clawde_core::types::Message,
    all_messages: &[clawde_core::types::Message],
    msg_idx: usize,
) -> String {
    use clawde_core::types::{ContentBlock, MessageContent, Role, ToolResultContent};

    let role_label = match msg.role {
        Role::User => "User",
        Role::Assistant => "Assistant",
    };

    let mut out = format!("## {}\n", role_label);

    match &msg.content {
        MessageContent::Text(t) => {
            out.push_str(t);
            out.push('\n');
        }
        MessageContent::Blocks(blocks) => {
            // Collect text first
            let mut text_parts: Vec<&str> = Vec::new();
            let mut tool_uses: Vec<(&str, &str, &serde_json::Value)> = Vec::new(); // (id, name, input)

            for block in blocks {
                match block {
                    ContentBlock::Text { text } => {
                        text_parts.push(text.as_str());
                    }
                    ContentBlock::ToolUse {
                        id, name, input, ..
                    } => {
                        tool_uses.push((id.as_str(), name.as_str(), input));
                    }
                    ContentBlock::Thinking { thinking, .. } => {
                        // Include thinking blocks as a collapsible hint
                        out.push_str("\n<details><summary>Thinking</summary>\n\n");
                        out.push_str(thinking);
                        out.push_str("\n</details>\n\n");
                    }
                    _ => {}
                }
            }

            if !text_parts.is_empty() {
                out.push_str(&text_parts.join(""));
                out.push('\n');
            }

            // For each tool use, look for the matching ToolResult in the NEXT user message
            for (tool_id, tool_name, tool_input) in &tool_uses {
                out.push_str(&format!("\n### Tool: {}\n", tool_name));
                let input_str = serde_json::to_string_pretty(tool_input)
                    .unwrap_or_else(|_| tool_input.to_string());
                out.push_str(&format!("**Input:** `{}`\n", input_str.replace('\n', " ")));

                // Search the next user message for a matching ToolResult
                let mut found_output: Option<String> = None;
                'search: for next_msg in all_messages.iter().skip(msg_idx + 1) {
                    if let MessageContent::Blocks(next_blocks) = &next_msg.content {
                        for nb in next_blocks {
                            if let ContentBlock::ToolResult {
                                tool_use_id,
                                content,
                                is_error,
                            } = nb
                            {
                                if tool_use_id.as_str() == *tool_id {
                                    let text = match content {
                                        ToolResultContent::Text(t) => t.clone(),
                                        ToolResultContent::Blocks(bs) => bs
                                            .iter()
                                            .filter_map(|b| {
                                                if let ContentBlock::Text { text } = b {
                                                    Some(text.as_str())
                                                } else {
                                                    None
                                                }
                                            })
                                            .collect::<Vec<_>>()
                                            .join(""),
                                    };
                                    let label = if is_error.unwrap_or(false) {
                                        "Error"
                                    } else {
                                        "Output"
                                    };
                                    found_output = Some(format!(
                                        "**{}:** `{}`\n",
                                        label,
                                        text.lines().next().unwrap_or(&text).trim()
                                    ));
                                    break 'search;
                                }
                            }
                        }
                    }
                }
                out.push_str(
                    found_output
                        .as_deref()
                        .unwrap_or("**Output:** *(pending)*\n"),
                );
            }
        }
    }

    out
}

/// Build the full markdown export string.
fn build_markdown_export(ctx: &CommandContext) -> String {
    let mut out = String::new();
    out.push_str("# Conversation Export\n\n");
    out.push_str(&format!("- **Session ID:** {}\n", ctx.session_id));
    out.push_str(&format!("- **Model:** {}\n", ctx.config.effective_model()));
    out.push_str(&format!(
        "- **Exported:** {}\n",
        chrono::Utc::now().to_rfc3339()
    ));
    if let Some(ref title) = ctx.session_title {
        out.push_str(&format!("- **Title:** {}\n", title));
    }
    out.push_str(&format!("- **Messages:** {}\n", ctx.messages.len()));
    out.push_str("\n---\n\n");

    let messages = ctx.messages.clone();
    for (i, msg) in messages.iter().enumerate() {
        out.push_str(&export_message_to_markdown(msg, &messages, i));
        out.push_str("\n---\n\n");
    }
    out
}

/// Build a readable plain-text export without Markdown or JSON syntax.
fn build_plain_text_export(ctx: &CommandContext) -> String {
    use clawde_core::types::Role;

    let mut out = String::new();
    out.push_str(
        ctx.session_title
            .as_deref()
            .unwrap_or("Clawde Conversation Export"),
    );
    out.push('\n');
    out.push_str("Session ID: ");
    out.push_str(&ctx.session_id);
    out.push('\n');
    out.push_str("Model: ");
    out.push_str(ctx.config.effective_model());
    out.push_str("\n\n");

    for (index, msg) in ctx.messages.iter().enumerate() {
        if index > 0 {
            out.push_str("\n-------------------------\n\n");
        }
        out.push_str(match msg.role {
            Role::User => "User:",
            Role::Assistant => "Clawde:",
        });
        out.push('\n');
        out.push_str(&msg.get_all_text());
        out.push('\n');
    }
    out
}

/// Build the full JSON export value.
fn build_json_export(ctx: &CommandContext) -> serde_json::Value {
    serde_json::json!({
        "exported_at": chrono::Utc::now().to_rfc3339(),
        "session_id": ctx.session_id,
        "session_title": ctx.session_title,
        "model": ctx.config.effective_model(),
        "message_count": ctx.messages.len(),
        "messages": ctx.messages.iter().map(|m| {
            serde_json::json!({
                "role": m.role,
                "content": m.content,
                "uuid": m.uuid,
            })
        }).collect::<Vec<_>>(),
    })
}

#[async_trait]
impl SlashCommand for ExportCommand {
    fn name(&self) -> &str {
        "export"
    }
    fn description(&self) -> &str {
        "Export conversation to Markdown, plain text, or JSON"
    }
    fn arg_completions(&self, partial: &str) -> Vec<ArgCompletion> {
        let mut completions = vec![
            ArgCompletion {
                value: "--format".into(),
                description: "Set output format (markdown, text, or json)".into(),
                available: true,
            },
            ArgCompletion {
                value: "--output".into(),
                description: "Write to file path".into(),
                available: true,
            },
        ];
        // Second-level completions: /export --format <format>
        if partial == "--format" || partial.starts_with("--format ") {
            completions.push(ArgCompletion {
                value: "--format markdown".into(),
                description: "Render as readable Markdown".into(),
                available: true,
            });
            completions.push(ArgCompletion {
                value: "--format json".into(),
                description: "Full structured JSON export".into(),
                available: true,
            });
            completions.push(ArgCompletion {
                value: "--format text".into(),
                description: "Portable plain-text transcript".into(),
                available: true,
            });
        }
        completions
    }
    fn help(&self) -> &str {
        "Usage: /export [--format markdown|text|json] [--output <file>]\n\n\
         Export the current conversation.\n\n\
         Flags:\n\
           --format markdown   Render as readable Markdown (default for .md files)\n\
           --format text       Portable plain-text transcript (default for .txt files)\n\
           --format json       Full structured JSON export (default)\n\
           --output <path>     Write to file; if omitted, prints to the terminal\n\n\
         Examples:\n\
           /export\n\
           /export --format markdown\n\
           /export --format text --output conversation.txt\n\
           /export --format json --output chat.json\n\
           /export --output conversation.md"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        // ── Parse flags ────────────────────────────────────────────────────
        let args = args.trim();
        let mut format: Option<&str> = None; // "markdown" | "text" | "json"
        let mut output_path: Option<String> = None;

        // Simple hand-rolled flag parser (no clap dep in commands crate)
        let tokens: Vec<&str> = args.split_whitespace().collect();
        let mut i = 0;
        while i < tokens.len() {
            match tokens[i] {
                "--format" | "-f" => {
                    if i + 1 < tokens.len() {
                        format = Some(tokens[i + 1]);
                        i += 2;
                    } else {
                        return CommandResult::Error(
                            "--format requires a value: markdown, text, or json".to_string(),
                        );
                    }
                }
                "--output" | "-o" => {
                    if i + 1 < tokens.len() {
                        output_path = Some(tokens[i + 1].to_string());
                        i += 2;
                    } else {
                        return CommandResult::Error("--output requires a file path".to_string());
                    }
                }
                other if !other.starts_with('-') => {
                    // Bare filename as positional arg (legacy compat)
                    if output_path.is_none() {
                        output_path = Some(other.to_string());
                    }
                    i += 1;
                }
                other => {
                    return CommandResult::Error(format!("Unknown flag: {}", other));
                }
            }
        }

        // ── Determine format from output path extension if not explicit ─────
        let resolved_format = match format {
            Some("markdown") | Some("md") => "markdown",
            Some("text") | Some("txt") | Some("plain") | Some("plain-text") => "text",
            Some("json") => "json",
            Some(other) => {
                return CommandResult::Error(format!(
                    "Unknown format '{}'. Use 'markdown', 'text', or 'json'.",
                    other
                ));
            }
            None => {
                // Infer from output file extension
                if let Some(ref path) = output_path {
                    if path.ends_with(".md") || path.ends_with(".markdown") {
                        "markdown"
                    } else if path.ends_with(".txt") {
                        "text"
                    } else {
                        "json"
                    }
                } else {
                    "json"
                }
            }
        };

        // ── Build content ───────────────────────────────────────────────────
        let content: String = match resolved_format {
            "markdown" => build_markdown_export(ctx),
            "text" => build_plain_text_export(ctx),
            _ => {
                let val = build_json_export(ctx);
                match serde_json::to_string_pretty(&val) {
                    Ok(j) => j,
                    Err(e) => return CommandResult::Error(format!("Serialization error: {}", e)),
                }
            }
        };

        // ── Write or return ─────────────────────────────────────────────────
        match output_path {
            Some(ref filename) => {
                // Default extension if the user didn't provide one
                let filename = if !filename.contains('.') {
                    format!(
                        "{}.{}",
                        filename,
                        match resolved_format {
                            "markdown" => "md",
                            "text" => "txt",
                            _ => "json",
                        }
                    )
                } else {
                    filename.to_string()
                };

                let path = if std::path::Path::new(&filename).is_absolute() {
                    std::path::PathBuf::from(&filename)
                } else {
                    ctx.working_dir.join(&filename)
                };

                match tokio::fs::write(&path, &content).await {
                    Ok(()) => CommandResult::Message(format!(
                        "Conversation exported to {} ({} messages, {} format)",
                        path.display(),
                        ctx.messages.len(),
                        resolved_format,
                    )),
                    Err(e) => {
                        CommandResult::Error(format!("Failed to write {}: {}", path.display(), e))
                    }
                }
            }
            None => {
                // Print to terminal
                CommandResult::Message(content)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ctx() -> CommandContext {
        CommandContext {
            config: clawde_core::config::Config::default(),
            cost_tracker: clawde_core::cost::CostTracker::new(),
            messages: vec![
                clawde_core::types::Message::user("hello"),
                clawde_core::types::Message::assistant("world"),
            ],
            working_dir: std::path::PathBuf::from("."),
            session_id: "export-test-session".to_string(),
            session_title: Some("Test session".to_string()),
            remote_session_url: None,
            mcp_manager: None,
            mcp_auth_runner: None,
            provider_registry: None,
            test_provider: None,
            effort: None,
        }
    }

    #[test]
    fn plain_text_export_is_portable() {
        let text = build_plain_text_export(&make_ctx());
        assert!(text.starts_with("Test session\nSession ID: export-test-session\n"));
        assert!(text.contains("User:\nhello"));
        assert!(text.contains("Clawde:\nworld"));
        assert!(!text.contains("**User**"));
        assert!(!text.contains("```"));
    }

    #[tokio::test]
    async fn text_format_can_be_returned_without_writing_a_file() {
        let mut ctx = make_ctx();
        let result = ExportCommand.execute("--format text", &mut ctx).await;
        match result {
            CommandResult::Message(text) => {
                assert!(text.contains("User:\nhello"));
                assert!(text.contains("Clawde:\nworld"));
            }
            other => panic!("expected text export, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn txt_extension_infers_plain_text_and_default_extension_is_txt() {
        let temp = tempfile::tempdir().expect("temp dir");
        let mut ctx = make_ctx();
        ctx.working_dir = temp.path().to_path_buf();
        let result = ExportCommand
            .execute("--output conversation.txt", &mut ctx)
            .await;
        match result {
            CommandResult::Message(message) => {
                assert!(message.contains("text format"));
                assert_eq!(
                    std::fs::read_to_string(temp.path().join("conversation.txt"))
                        .expect("text export file"),
                    build_plain_text_export(&ctx)
                );
            }
            other => panic!("expected successful text export, got {other:?}"),
        }

        let result = ExportCommand
            .execute("--format text --output transcript", &mut ctx)
            .await;
        match result {
            CommandResult::Message(message) => {
                assert!(message.ends_with("transcript.txt (2 messages, text format)"))
            }
            other => panic!("expected successful default extension, got {other:?}"),
        }
        assert!(temp.path().join("transcript.txt").exists());
    }
}
