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
    fn arg_completions(&self, _partial: &str) -> Vec<ArgCompletion> {
        vec![
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
        ]
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
    fn arg_completions(&self, _partial: &str) -> Vec<ArgCompletion> {
        vec![ArgCompletion {
            value: "list".into(),
            description: "List all saved sessions".into(),
            available: true,
        }]
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        match args.trim() {
            "list" => {
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
            }
            "" => {
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
                        output.push_str(
                            "\nUse /session list for all sessions, /resume <id> to switch.",
                        );
                    }

                    CommandResult::Message(output)
                }
            }
            _ => CommandResult::Error(format!(
                "Unknown subcommand: {}\n\nUsage: /session [list]",
                args
            )),
        }
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
