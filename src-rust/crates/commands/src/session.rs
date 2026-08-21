// Session-control commands: `/plan`, `/tasks`, `/session`, `/fork`.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct PlanCommand;
pub struct TasksCommand;
pub struct SessionCommand;
pub struct ForkCommand;

/// Return the path to this session's plan file on disk.
fn plan_file_path(ctx: &CommandContext) -> PathBuf {
    let plans_dir = clawde_core::config::Settings::config_dir().join("plans");
    let safe_name = ctx
        .session_id
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    plans_dir.join(format!("{}.md", safe_name))
}

/// Read the current plan text (if any) from the plan file on disk.
fn load_plan(ctx: &CommandContext) -> Option<String> {
    let path = plan_file_path(ctx);
    if path.exists() {
        std::fs::read_to_string(&path).ok()
    } else {
        None
    }
}

/// Try to open a file in the user's preferred editor.
fn open_in_editor(file: &std::path::Path) -> Result<(), String> {
    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| {
            #[cfg(target_os = "windows")]
            {
                "notepad".to_string()
            }
            #[cfg(target_os = "macos")]
            {
                "open".to_string()
            }
            #[cfg(not(any(target_os = "windows", target_os = "macos")))]
            {
                "nano".to_string()
            }
        });

    std::process::Command::new(&editor)
        .arg(file)
        .spawn()
        .map(|_| ())
        .map_err(|e| {
            format!(
                "Failed to open '{}' with editor '{}': {}",
                file.display(),
                editor,
                e
            )
        })
}

// ---- /plan ---------------------------------------------------------------

#[async_trait]
impl SlashCommand for PlanCommand {
    fn name(&self) -> &str {
        "plan"
    }
    fn description(&self) -> &str {
        "Enter, view, or manage plan mode"
    }
    fn arg_completions(&self, partial: &str) -> Vec<ArgCompletion> {
        let mut completions = vec![
            ArgCompletion {
                value: "open".into(),
                description: "Open the plan file in your $EDITOR".into(),
                available: true,
            },
            ArgCompletion {
                value: "exit".into(),
                description: "Leave plan mode and resume normal execution".into(),
                available: true,
            },
        ];
        // The main argument is a free-form task description (e.g. `/plan add
        // a login page`). Show a dimmed placeholder hint while the field is
        // still empty so the popup says what goes next.
        if partial.trim().is_empty() {
            if let Some(hint) = super::free_form_arg_hint(
                "",
                "<description>",
                "Describe the task to plan, or use open/exit",
                false,
            ) {
                completions.push(hint);
            }
        }
        completions
    }
    fn help(&self) -> &str {
        "Usage: /plan [open|<description>|exit]\n\n\
         Subcommands:\n\
           /plan              — show current plan status and toggles plan mode\n\
           /plan open         — open the plan file in your $EDITOR\n\
           /plan <description> — enter plan mode for the given task\n\
           /plan exit         — leave plan mode and resume normal execution\n\n\
         Plan mode restricts the model to read-only operations. The model must\n\
         create a detailed plan and receive approval before it can act."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let trimmed = args.trim();

        // --- /plan open ---
        if trimmed == "open" {
            let path = plan_file_path(ctx);
            // Ensure parent directory exists.
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }

            // Create a default plan template if the file does not exist yet.
            if !path.exists() {
                let template =
                    "# Plan\n\n## Objective\n\n\n## Steps\n\n1. \n2. \n3. \n\n## Notes\n\n"
                        .to_string();
                if let Err(e) = std::fs::write(&path, &template) {
                    return CommandResult::Error(format!("Failed to create plan file: {}", e));
                }
            }

            return match open_in_editor(&path) {
                Ok(()) => CommandResult::Message(format!(
                    "Opened plan file in your editor: {}",
                    path.display()
                )),
                Err(e) => CommandResult::Error(e),
            };
        }

        // --- /plan exit ---
        if trimmed == "exit" {
            return CommandResult::UserMessage(
                "[Exiting plan mode. Resuming normal execution.]".to_string(),
            );
        }

        // --- /plan (no args) — show current plan status ---
        if trimmed.is_empty() {
            match load_plan(ctx) {
                Some(content) => {
                    let preview: String = content.chars().take(500).collect();
                    let ellipsis = if content.len() > 500 {
                        "\n… (truncated)"
                    } else {
                        ""
                    };
                    CommandResult::Message(format!(
                        "Current Plan\n\
                         ────────────\n\
                         Plan file: {}\n\n\
                         {}{}\n\n\
                         Use /plan open to edit the plan.\n\
                         Use /plan <description> to enter plan mode for a new task.\n\
                         Use /plan exit to leave plan mode.",
                        plan_file_path(ctx).display(),
                        preview,
                        ellipsis
                    ))
                }
                None => CommandResult::Message(
                    "No active plan. Use /plan <description> to enter plan mode.\n\n\
                     Plan mode lets the model create a detailed step-by-step plan\n\
                     that must be approved before any writes or commands are executed."
                        .to_string(),
                ),
            }
        } else {
            // --- /plan <description> — enter plan mode ---
            let task_desc = trimmed.to_string();

            // Create an empty plan file to mark that a plan exists.
            let path = plan_file_path(ctx);
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if !path.exists() {
                let template = format!(
                    "# Plan: {}\n\n## Objective\n\n{}\n\n## Steps\n\n1. \n2. \n3. \n\n## Notes\n\n",
                    task_desc, task_desc
                );
                let _ = std::fs::write(&path, &template);
            }

            CommandResult::UserMessage(format!(
                "[Entering plan mode for: {}]\n\n\
                 Please create a detailed step-by-step plan. Do not execute any \
                 commands or write any files until the plan has been reviewed \
                 and approved.\n\
                 \n\
                 Use EnterPlanMode to enter the planning state, then work \
                 through the analysis. When you have a complete plan, present \
                 it for review. Use the plan file at {} to record the plan \
                 if needed.\n\
                 \n\
                 Use /plan open to edit the plan file in your editor, \
                 /plan to view the current plan, and /plan exit when approved.",
                task_desc,
                path.display()
            ))
        }
    }
}

// ---- /tasks --------------------------------------------------------------

#[async_trait]
impl SlashCommand for TasksCommand {
    fn name(&self) -> &str {
        "tasks"
    }
    fn aliases(&self) -> Vec<&str> {
        vec!["bashes"]
    }
    fn description(&self) -> &str {
        "List and manage background tasks"
    }

    async fn execute(&self, _args: &str, _ctx: &mut CommandContext) -> CommandResult {
        CommandResult::UserMessage(
            "Please list all current tasks using the TaskList tool and show their status."
                .to_string(),
        )
    }
}

// ---- /session ------------------------------------------------------------

#[async_trait]
impl SlashCommand for SessionCommand {
    fn name(&self) -> &str {
        "session"
    }
    fn aliases(&self) -> Vec<&str> {
        vec!["remote"]
    }
    fn description(&self) -> &str {
        "Show or manage conversation sessions"
    }
    fn arg_completions(&self, partial: &str) -> Vec<ArgCompletion> {
        let completions = vec!["list", "delete", "prune"];
        completions
            .into_iter()
            .filter(|c| c.starts_with(partial))
            .map(|c| ArgCompletion {
                value: c.to_string(),
                description: match c {
                    "list" => "List all saved sessions".to_string(),
                    "delete" => "Delete a session by ID".to_string(),
                    "prune" => "Delete sessions older than N days (default: 30)".to_string(),
                    _ => String::new(),
                },
                available: true,
            })
            .collect()
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let trimmed = args.trim();
        if trimmed == "list" {
            let sessions = clawde_core::history::list_sessions().await;
            if sessions.is_empty() {
                CommandResult::Message("No saved sessions found.".to_string())
            } else {
                let mut output = String::from("Recent sessions:\n\n");
                for sess in sessions.iter().take(10) {
                    let updated = sess.updated_at.format("%Y-%m-%d %H:%M").to_string();
                    let id_short = &sess.id[..sess.id.len().min(8)];
                    output.push_str(&format!(
                        "  {} | {} | {} messages | {}\n",
                        id_short,
                        updated,
                        sess.messages.len(),
                        sess.title.as_deref().unwrap_or("(untitled)")
                    ));
                }
                output.push_str("\nUse /resume <id> to resume a session.");
                CommandResult::Message(output)
            }
        } else if trimmed.is_empty() {
            // If a bridge remote URL is active, show it prominently.
            if let Some(ref url) = ctx.remote_session_url {
                let border = "\u{2500}".repeat(url.len().min(60) + 4);
                let display_url = if url.len() > 60 {
                    format!("{}\u{2026}", &url[..60])
                } else {
                    url.clone()
                };
                CommandResult::Message(format!(
                    "Remote session active\n\
                     \u{250C}{border}\u{2510}\n\
                     \u{2502}  {display_url}  \u{2502}\n\
                     \u{2514}{border}\u{2518}\n\n\
                     Open the URL above on any device to connect remotely.\n\
                     Session ID: {}",
                    ctx.session_id,
                ))
            } else {
                // Show current session info + recent sessions list.
                let sessions = clawde_core::history::list_sessions().await;
                let mut output = format!(
                    "Current session\n\
                     \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
                     ID:       {}\n\
                     Title:    {}\n\
                     Messages: {}\n\
                     Model:    {}\n",
                    ctx.session_id,
                    ctx.session_title.as_deref().unwrap_or("(untitled)"),
                    ctx.messages.len(),
                    ctx.config.effective_model()
                );

                if !sessions.is_empty() {
                    output.push_str("\nRecent sessions:\n\n");
                    for sess in sessions.iter().take(5) {
                        let updated = sess.updated_at.format("%Y-%m-%d %H:%M").to_string();
                        let id_short = &sess.id[..sess.id.len().min(8)];
                        let marker = if sess.id == ctx.session_id {
                            " \u{25C0} current"
                        } else {
                            ""
                        };
                        output.push_str(&format!(
                            "  {} | {} | {} messages | {}{}\n",
                            id_short,
                            updated,
                            sess.messages.len(),
                            sess.title.as_deref().unwrap_or("(untitled)"),
                            marker,
                        ));
                    }
                    output
                        .push_str("\nUse /session list for all sessions, /resume <id> to switch.");
                }

                CommandResult::Message(output)
            }
        } else if trimmed == "delete" || trimmed.starts_with("delete ") {
            let session_id = trimmed.strip_prefix("delete").unwrap_or("").trim();
            if session_id.is_empty() {
                return CommandResult::Error(
                    "Usage: /session delete <session-id>\n\n\
                     Delete a session and all its transcripts.\n\
                     Use /session list to find session IDs."
                        .to_string(),
                );
            }
            if session_id == ctx.session_id {
                return CommandResult::Error(
                    "Cannot delete the active session. Use /clear to reset it first.".to_string(),
                );
            }
            match clawde_core::history::delete_session(session_id).await {
                Ok(()) => CommandResult::Message(format!(
                    "Deleted session {}.",
                    &session_id[..session_id.len().min(12)]
                )),
                Err(e) => CommandResult::Error(format!("Failed to delete session: {e}")),
            }
        } else if trimmed.starts_with("prune ") {
            let rest = trimmed.strip_prefix("prune ").unwrap_or("").trim();
            let days: u64 = rest.parse().unwrap_or(30);
            if days == 0 {
                return CommandResult::Error("Days must be at least 1.".to_string());
            }
            let sessions = clawde_core::history::list_sessions().await;
            let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
            let stale: Vec<_> = sessions
                .iter()
                .filter(|s| s.updated_at < cutoff && s.id != ctx.session_id)
                .collect();
            if stale.is_empty() {
                return CommandResult::Message(format!(
                    "No sessions older than {days} day{} found.",
                    if days == 1 { "" } else { "s" }
                ));
            }
            let mut deleted = 0;
            for session in &stale {
                if clawde_core::history::delete_session(&session.id)
                    .await
                    .is_ok()
                {
                    deleted += 1;
                }
            }
            CommandResult::Message(format!(
                "Pruned {deleted} session{} older than {days} day{}.",
                if deleted == 1 { "" } else { "s" },
                if days == 1 { "" } else { "s" }
            ))
        } else {
            CommandResult::Error(format!(
                "Unknown subcommand: {}\n\nUsage: /session [list|delete|prune]",
                args
            ))
        }
    }
}

// ---- /history ------------------------------------------------------------

/// Show the current project's session history and where history is stored.
///
/// Reads the project-scoped JSONL transcripts (`session_storage::list_sessions`)
/// — the same store that backs the welcome screen's "Recent activity" list —
/// and prints each session with its timestamp, title, and id, plus pointers to
/// the on-disk stores and related commands.
pub struct HistoryCommand;

#[async_trait]
impl SlashCommand for HistoryCommand {
    fn name(&self) -> &str {
        "history"
    }
    fn description(&self) -> &str {
        "Show recent sessions for this project and where history lives"
    }
    fn help(&self) -> &str {
        "Usage: /history\n\n\
         Lists the most recent sessions for the current project (the git repo\n\
         root, or the working directory when not in a repo), newest first, with\n\
         their timestamps and titles, plus pointers to the on-disk stores.\n\n\
         See also: /session list (all sessions), /resume <id>, /stats."
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        let project_root = clawde_core::git_utils::get_repo_root(&ctx.working_dir)
            .unwrap_or_else(|| ctx.working_dir.clone());

        let sessions = match clawde_core::session_storage::list_sessions(&project_root).await {
            Ok(s) => s,
            Err(e) => return CommandResult::Error(format!("Failed to read session history: {e}")),
        };

        let sep = "-".repeat(60);
        let mut lines = vec![format!(
            "Project history for {}\n{}",
            project_root.display(),
            sep
        )];

        if sessions.is_empty() {
            lines.push("  No sessions recorded for this project yet.".to_string());
        } else {
            for s in sessions.iter().take(15) {
                let when = clawde_core::format_utils::format_short_absolute_time(s.mtime);
                let label = s
                    .title
                    .clone()
                    .or_else(|| s.ai_title.clone())
                    .or_else(|| s.last_prompt.clone())
                    .map(|t| {
                        t.lines()
                            .find(|l| !l.trim().is_empty())
                            .map(|l| l.trim().to_string())
                            .unwrap_or_else(|| "(untitled)".to_string())
                    })
                    .unwrap_or_else(|| "(untitled)".to_string());
                let short_id: String = s.session_id.chars().take(8).collect();
                lines.push(format!("  [{}] {}  ({})", when, label, short_id));
            }
            if sessions.len() > 15 {
                lines.push(format!(
                    "  ... and {} more (use /session list)",
                    sessions.len() - 15
                ));
            }
        }

        lines.push(String::new());
        lines.push("Where history lives:".to_string());
        lines.push(
            "  - Session store:    ~/.clawde/sessions/<id>.json    (/session, /resume)".to_string(),
        );
        lines.push(
            "  - Project history:  ~/.clawde/projects/<dir>/<id>.jsonl  (recents, /stats)"
                .to_string(),
        );
        lines.push(
            "  - Prompt history:   ~/.clawde/history.jsonl         (up/down in the input box)"
                .to_string(),
        );
        lines.push("  - File checkpoints: /checkpoints, /snapshot, /revert, /undo".to_string());

        CommandResult::Message(lines.join("\n"))
    }
}

// ---- /fork ---------------------------------------------------------------

#[async_trait]
impl SlashCommand for ForkCommand {
    fn name(&self) -> &str {
        "fork"
    }
    fn description(&self) -> &str {
        "Fork the current session into a new branch"
    }
    fn help(&self) -> &str {
        "Usage: /fork [message_index]\n\n\
         Fork the current session at the specified message index (or at the\n\
         current point if no index is given).  Creates a new session containing\n\
         messages up to the fork point.\n\n\
         Examples:\n\
           /fork        \u{2014} fork at the current end of the conversation\n\
           /fork 5      \u{2014} fork after message 5"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let fork_index: Option<usize> = args.trim().parse().ok();
        let messages = &ctx.messages;
        let fork_at = fork_index.unwrap_or(messages.len()).min(messages.len());
        let forked_messages: Vec<_> = messages[..fork_at].to_vec();

        let mut new_session = clawde_core::history::ConversationSession::new(
            ctx.config.effective_model().to_string(),
        );
        new_session.messages = forked_messages;
        new_session.parent_session_id = Some(ctx.session_id.clone());
        new_session.fork_point_message_index = Some(fork_at);
        new_session.title = Some(format!(
            "Fork of {}",
            ctx.session_title.as_deref().unwrap_or("session")
        ));
        new_session.working_dir = Some(ctx.working_dir.to_string_lossy().to_string());

        let new_id = new_session.id.clone();
        match clawde_core::history::save_session(&new_session).await {
            Ok(()) => CommandResult::Message(format!(
                "Session forked at message {}. New session: {}\nUse /resume {} to switch to it.",
                fork_at, new_id, new_id
            )),
            Err(e) => CommandResult::Error(format!("Failed to save forked session: {}", e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_ctx(session_id: &str) -> CommandContext {
        CommandContext {
            config: clawde_core::config::Config::default(),
            cost_tracker: clawde_core::cost::CostTracker::new(),
            messages: vec![],
            working_dir: PathBuf::from("."),
            session_id: session_id.to_string(),
            session_title: None,
            remote_session_url: None,
            mcp_manager: None,
            mcp_auth_runner: None,
            provider_registry: None,
            test_provider: None,
            effort: None,
            tool_use_tracker: None,
        }
    }

    #[tokio::test]
    async fn session_delete_requires_id() {
        let mut ctx = test_ctx("active-sess");
        let result = SessionCommand.execute("delete ", &mut ctx).await;
        match result {
            CommandResult::Error(msg) => {
                assert!(msg.contains("Usage: /session delete"));
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn session_delete_rejects_active_session() {
        let mut ctx = test_ctx("active-sess");
        let result = SessionCommand.execute("delete active-sess", &mut ctx).await;
        match result {
            CommandResult::Error(msg) => {
                assert!(msg.contains("Cannot delete the active session"));
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn session_delete_removes_session() {
        let mut session = clawde_core::history::ConversationSession::new("test-model".to_string());
        session.title = Some("to-delete".to_string());
        let id = session.id.clone();
        clawde_core::history::save_session(&session).await.unwrap();

        // Verify it exists.
        assert!(clawde_core::history::load_session(&id).await.is_ok());

        // Delete it.
        let mut ctx = test_ctx("other-session");
        let result = SessionCommand
            .execute(&format!("delete {id}"), &mut ctx)
            .await;
        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("Deleted session"));
            }
            other => panic!("expected Message, got {:?}", other),
        }

        // Verify it's gone.
        assert!(clawde_core::history::load_session(&id).await.is_err());
    }

    #[tokio::test]
    async fn session_prune_no_old_sessions() {
        let mut ctx = test_ctx("active-sess");
        let result = SessionCommand.execute("prune 365", &mut ctx).await;
        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("No sessions older than"));
            }
            other => panic!("expected Message, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn session_unknown_subcommand() {
        let mut ctx = test_ctx("active-sess");
        let result = SessionCommand.execute("bogus", &mut ctx).await;
        match result {
            CommandResult::Error(msg) => {
                assert!(msg.contains("Unknown subcommand"));
                assert!(msg.contains("list|delete|prune"));
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }
}
