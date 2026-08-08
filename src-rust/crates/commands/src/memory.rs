// `/memory` command (AGENTS.md memory files).
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct MemoryCommand;

// ---- /memory -------------------------------------------------------------

#[async_trait]
impl SlashCommand for MemoryCommand {
    fn name(&self) -> &str {
        "memory"
    }
    fn description(&self) -> &str {
        "View, edit, or clear memory files (AGENTS.md + project memory)"
    }
    fn arg_completions(&self, _partial: &str) -> Vec<ArgCompletion> {
        vec![
            ArgCompletion {
                value: "edit".into(),
                description: "Open project AGENTS.md in editor".into(),
                available: true,
            },
            ArgCompletion {
                value: "clear".into(),
                description: "Clear project AGENTS.md".into(),
                available: true,
            },
            ArgCompletion {
                value: "status".into(),
                description: "Show project memory (auto-memory) status".into(),
                available: true,
            },
            ArgCompletion {
                value: "init".into(),
                description: "Seed project memory templates".into(),
                available: true,
            },
        ]
    }
    fn help(&self) -> &str {
        "Usage: /memory [edit|clear|status|init] [global]\n\n\
         Shows the content of AGENTS.md files that provide project context to Clawde.\n\
         Clawde reads these files automatically at session start.\n\n\
         Subcommands:\n\
           /memory              — show all AGENTS.md files\n\
           /memory edit         — open project AGENTS.md in your editor\n\
           /memory edit global  — open global ~/.clawde/AGENTS.md in your editor\n\
           /memory clear        — clear the project AGENTS.md\n\
           /memory clear global — clear the global ~/.clawde/AGENTS.md\n\
           /memory status       — show project memory dir, index, and session summaries\n\
           /memory init         — seed architecture/conventions/decisions/tasks + MEMORY.md\n\n\
         Locations checked (in priority order):\n\
           1. <project>/.claurst/AGENTS.md\n\
           2. <project>/AGENTS.md\n\
           3. ~/.clawde/AGENTS.md  (global)\n\n\
         Use /init to create a new AGENTS.md from a template."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let project_claude_dir = ctx.working_dir.join(".claurst").join("AGENTS.md");
        let project_root = ctx.working_dir.join("AGENTS.md");
        let global_path = clawde_core::config::Settings::config_dir().join("AGENTS.md");

        let locations = [
            ("project (.claurst/AGENTS.md)", project_claude_dir.clone()),
            ("project (AGENTS.md)", project_root.clone()),
            ("global (~/.clawde/AGENTS.md)", global_path.clone()),
        ];

        let cmd = args.trim();

        // ---- /memory edit [global|project] ------------------------------------
        if cmd == "edit" || cmd.starts_with("edit ") {
            let target_hint = cmd
                .strip_prefix("edit")
                .map(|s| s.trim())
                .unwrap_or("project");
            let target = match target_hint {
                "global" => {
                    // Ensure global dir exists
                    if let Some(parent) = global_path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    global_path.clone()
                }
                _ => {
                    // Best project AGENTS.md
                    if project_root.exists() {
                        project_root.clone()
                    } else if project_claude_dir.exists() {
                        project_claude_dir.clone()
                    } else {
                        project_root.clone() // will be created by editor
                    }
                }
            };
            // Create file if it doesn't exist yet
            if !target.exists() {
                if let Some(parent) = target.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&target, "");
            }
            let editor = std::env::var("VISUAL")
                .or_else(|_| std::env::var("EDITOR"))
                .unwrap_or_else(|_| {
                    if cfg!(target_os = "windows") {
                        "notepad".to_string()
                    } else {
                        "vi".to_string()
                    }
                });
            let editor_hint = if let Ok(visual) = std::env::var("VISUAL") {
                format!("Using $VISUAL=\"{}\".", visual)
            } else if let Ok(ed) = std::env::var("EDITOR") {
                format!("Using $EDITOR=\"{}\".", ed)
            } else {
                "To use a different editor, set the $EDITOR or $VISUAL environment variable."
                    .to_string()
            };
            let spawn_result = std::process::Command::new(&editor).arg(&target).status();
            return match spawn_result {
                Ok(_) => CommandResult::Message(format!(
                    "Opened {} in your editor.\n{}",
                    target.display(),
                    editor_hint
                )),
                Err(e) => CommandResult::Message(format!(
                    "Could not launch '{}': {}. Edit {} manually.\n{}",
                    editor,
                    e,
                    target.display(),
                    editor_hint
                )),
            };
        }

        // ---- /memory clear [global|project] -----------------------------------
        if cmd == "clear" || cmd.starts_with("clear ") {
            let target_hint = cmd
                .strip_prefix("clear")
                .map(|s| s.trim())
                .unwrap_or("project");
            let (label, target) = match target_hint {
                "global" => ("global (~/.clawde/AGENTS.md)", global_path.clone()),
                _ => {
                    if project_claude_dir.exists() {
                        ("project (.claurst/AGENTS.md)", project_claude_dir.clone())
                    } else {
                        ("project (AGENTS.md)", project_root.clone())
                    }
                }
            };
            if !target.exists() {
                return CommandResult::Message(format!(
                    "No {} memory file found (nothing to clear).",
                    label
                ));
            }
            return match tokio::fs::write(&target, "").await {
                Ok(_) => CommandResult::Message(format!(
                    "Cleared {} memory file at {}.\n\
                     Clawde will no longer see this content at session start.",
                    label,
                    target.display()
                )),
                Err(e) => {
                    CommandResult::Error(format!("Failed to clear {}: {}", target.display(), e))
                }
            };
        }

        // ---- /memory init -------------------------------------------------------
        if cmd == "init" {
            use clawde_core::memdir::{
                auto_memory_path, ensure_memory_dir_exists, MEMORY_ENTRYPOINT,
            };
            const ARCHITECTURE: &str = "# Architecture\n\n\
                Project structure, key abstractions, and the module map.\n\
                Keep this current as the codebase evolves.\n\n\
                ## Overview\n\n\
                ## Key modules\n\n\
                ## Data flow\n";
            const CONVENTIONS: &str = "# Conventions\n\n\
                Code style, naming, test/build commands, and lint rules.\n\n\
                ## Style\n\n\
                ## Commands\n\n\
                ## Testing\n";
            const DECISIONS: &str = "# Decisions\n\n\
                Architectural decision log (ADR style). Append one entry per decision.\n\n\
                ## YYYY-MM-DD — <title>\n\n\
                ### Context\n\n\
                ### Decision\n\n\
                ### Consequences\n";
            const TASKS: &str = "# Tasks\n\n\
                Pending work and its status.\n\n\
                - [ ] \n";
            const MEMORY_INDEX: &str = "# Memory Index\n\n\
                - [Architecture](architecture.md) — project structure and key abstractions\n\
                - [Conventions](conventions.md) — style, build/test/lint commands\n\
                - [Decisions](decisions.md) — architectural decisions log\n\
                - [Tasks](tasks.md) — pending tasks\n";

            let mem_dir = auto_memory_path(&ctx.working_dir);
            ensure_memory_dir_exists(&mem_dir);

            let templates: &[(&str, &str)] = &[
                ("architecture.md", ARCHITECTURE),
                ("conventions.md", CONVENTIONS),
                ("decisions.md", DECISIONS),
                ("tasks.md", TASKS),
            ];
            let mut created: Vec<String> = Vec::new();
            let mut kept: Vec<String> = Vec::new();
            for (name, content) in templates {
                let path = mem_dir.join(name);
                if path.exists() {
                    kept.push((*name).to_string());
                } else if std::fs::write(&path, content).is_ok() {
                    created.push((*name).to_string());
                } else {
                    kept.push(format!("{} (write failed)", name));
                }
            }
            // Seed a starter MEMORY.md index so the system-prompt injection
            // activates immediately — the index is what the model reads.
            let index_path = mem_dir.join(MEMORY_ENTRYPOINT);
            if !index_path.exists() {
                let _ = std::fs::write(&index_path, MEMORY_INDEX);
                created.push(MEMORY_ENTRYPOINT.to_string());
            } else {
                kept.push(MEMORY_ENTRYPOINT.to_string());
            }

            let mut out = String::from("Project memory initialized\n═══════════════════════════\n");
            out.push_str(&format!("Directory: {}\n", mem_dir.display()));
            if created.is_empty() {
                out.push_str("All memory files already exist — nothing was created.\n");
            } else {
                out.push_str(&format!("Created: {}\n", created.join(", ")));
            }
            if !kept.is_empty() {
                out.push_str(&format!("Existing (kept as-is): {}\n", kept.join(", ")));
            }
            out.push_str(
                "\nThese files are injected into the system prompt at session start.\n\
                 Edit them directly, or let the agent maintain them over time.",
            );
            return CommandResult::Message(out);
        }

        // ---- /memory status ----------------------------------------------------
        if cmd == "status" {
            use clawde_core::memdir::{
                auto_memory_path, load_memory_index, most_recent_session_summary, scan_memory_dir,
            };
            let mem_dir = auto_memory_path(&ctx.working_dir);
            let mut out =
                String::from("Project Memory (auto-memory)\n══════════════════════════════\n");
            out.push_str(&format!("Directory: {}\n", mem_dir.display()));
            if !mem_dir.is_dir() {
                out.push_str(
                    "No project memory yet. It is created automatically when the agent\n\
                     first writes a memory file (auto-dream consolidation or the memory tools).",
                );
                return CommandResult::Message(out);
            }
            match load_memory_index(&mem_dir) {
                Some(index) => {
                    let index_age = std::fs::metadata(mem_dir.join("MEMORY.md"))
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|duration| clawde_core::memdir::memory_age(duration.as_secs()));
                    out.push_str(&format!(
                        "Index (MEMORY.md): {} lines, {} bytes{}{}\n",
                        index.line_count,
                        index.byte_count,
                        if index.was_line_truncated || index.was_byte_truncated {
                            " (truncated)"
                        } else {
                            ""
                        },
                        index_age
                            .as_deref()
                            .map(|age| format!(", updated {}", age))
                            .unwrap_or_default()
                    ));
                }
                None => out.push_str("Index (MEMORY.md): not present\n"),
            }
            let files = scan_memory_dir(&mem_dir);
            out.push_str(&format!("Memory files: {}\n", files.len()));
            if let Some(newest) = files.first() {
                out.push_str(&format!(
                    "Newest memory file: {} (updated {})\n",
                    newest.filename,
                    clawde_core::memdir::memory_age(newest.modified_secs)
                ));
            }
            let sessions_dir = mem_dir.join("sessions");
            let session_count = std::fs::read_dir(&sessions_dir)
                .map(|entries| {
                    entries
                        .flatten()
                        .filter(|entry| {
                            entry.path().is_file()
                                && entry
                                    .path()
                                    .extension()
                                    .is_some_and(|extension| extension == "md")
                        })
                        .count()
                })
                .unwrap_or(0);
            out.push_str(&format!("Session summaries: {}\n", session_count));
            if let Some(summary) = most_recent_session_summary(&mem_dir) {
                out.push_str(&format!(
                    "Most recent summary: {} chars (injected into system prompt)\n",
                    summary.len()
                ));
            }
            return CommandResult::Message(out);
        }

        // ---- /memory (show all) -----------------------------------------------
        let mut output = String::from("AGENTS.md Memory Files\n══════════════════════\n");
        let mut found_any = false;

        for (label, path) in &locations {
            if path.exists() {
                found_any = true;
                match tokio::fs::read_to_string(path).await {
                    Ok(content) => {
                        let lines: usize = content.lines().count();
                        let chars = content.len();
                        output.push_str(&format!(
                            "\n[{label}]\nPath: {path}\nSize: {lines} lines, {chars} chars\n\
                             ─────────────────────────────────\n\
                             {content}\n",
                            label = label,
                            path = path.display(),
                            lines = lines,
                            chars = chars,
                            content = if content.len() > 2000 {
                                format!(
                                    "{}…\n(truncated — file is {} chars)",
                                    &content[..2000],
                                    chars
                                )
                            } else {
                                content.clone()
                            }
                        ));
                    }
                    Err(e) => output.push_str(&format!(
                        "\n[{label}] — Error reading {}: {}\n",
                        path.display(),
                        e,
                        label = label
                    )),
                }
            }
        }

        if !found_any {
            output.push_str(
                "\nNo AGENTS.md files found.\n\
                 Use /init to create one in the current project.\n\
                 Use /memory edit to create and open a memory file.",
            );
        } else {
            output.push_str(
                "\nSubcommands:\n\
                 /memory edit          — edit project AGENTS.md\n\
                 /memory edit global   — edit global ~/.clawde/AGENTS.md\n\
                 /memory clear         — clear project AGENTS.md\n\
                 /memory clear global  — clear global AGENTS.md",
            );
        }

        CommandResult::Message(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx() -> CommandContext {
        CommandContext {
            config: clawde_core::config::Config::default(),
            cost_tracker: clawde_core::cost::CostTracker::new(),
            messages: vec![],
            working_dir: std::path::PathBuf::from("."),
            session_id: "test-session".to_string(),
            session_title: None,
            remote_session_url: None,
            mcp_manager: None,
            mcp_auth_runner: None,
            provider_registry: None,
            test_provider: None,
        }
    }

    fn message_text(result: CommandResult) -> String {
        match result {
            CommandResult::Message(text) => text,
            other => panic!("expected CommandResult::Message, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn memory_status_shows_project_memory() {
        // Point CLAWDE_HOME at a temp dir (auto_memory_path resolves under it)
        // and seed a project-scoped memory dir for a fake working dir.
        let _home = crate::keys::tests::TestHome::new();
        let project = tempfile::tempdir().unwrap();
        let mem_dir = clawde_core::memdir::auto_memory_path(project.path());
        std::fs::create_dir_all(mem_dir.join("sessions")).unwrap();
        std::fs::write(
            mem_dir.join("MEMORY.md"),
            "- [architecture.md](architecture.md) — index",
        )
        .unwrap();
        std::fs::write(
            mem_dir.join("sessions").join("2026-08-01.md"),
            "## Session\nShipped the routing dialog.",
        )
        .unwrap();

        let mut ctx = test_ctx();
        ctx.working_dir = project.path().to_path_buf();
        let out = message_text(MemoryCommand.execute("status", &mut ctx).await);

        assert!(out.contains("Project Memory (auto-memory)"), "got: {}", out);
        assert!(out.contains("Index (MEMORY.md): 1 lines"), "got: {}", out);
        // The session summary is a `.md` file, so scan_memory_dir counts it.
        assert!(out.contains("Memory files: 1"), "got: {}", out);
        assert!(out.contains("Session summaries: 1"), "got: {}", out);
        assert!(out.contains("Most recent summary: "), "got: {}", out);
        assert!(out.contains("Index (MEMORY.md): 1 lines, "), "got: {}", out);
        assert!(out.contains("updated "), "got: {}", out);
        assert!(
            out.contains("Newest memory file: sessions/2026-08-01.md"),
            "got: {}",
            out
        );
    }

    #[tokio::test]
    async fn memory_init_creates_templates() {
        let _home = crate::keys::tests::TestHome::new();
        let project = tempfile::tempdir().unwrap();

        let mut ctx = test_ctx();
        ctx.working_dir = project.path().to_path_buf();
        let out = message_text(MemoryCommand.execute("init", &mut ctx).await);

        assert!(out.contains("Created"), "got: {}", out);
        let mem_dir = clawde_core::memdir::auto_memory_path(project.path());
        for name in [
            "architecture.md",
            "conventions.md",
            "decisions.md",
            "tasks.md",
            "MEMORY.md",
        ] {
            assert!(mem_dir.join(name).is_file(), "missing {}", name);
        }
    }

    #[tokio::test]
    async fn memory_init_does_not_overwrite_existing() {
        let _home = crate::keys::tests::TestHome::new();
        let project = tempfile::tempdir().unwrap();
        let mem_dir = clawde_core::memdir::auto_memory_path(project.path());
        std::fs::create_dir_all(&mem_dir).unwrap();
        std::fs::write(mem_dir.join("conventions.md"), "# Custom conventions\n").unwrap();

        let mut ctx = test_ctx();
        ctx.working_dir = project.path().to_path_buf();
        let out = message_text(MemoryCommand.execute("init", &mut ctx).await);

        assert!(out.contains("Existing (kept as-is)"), "got: {}", out);
        let content = std::fs::read_to_string(mem_dir.join("conventions.md")).unwrap();
        assert_eq!(content, "# Custom conventions\n");
    }

    #[tokio::test]
    async fn memory_status_empty_dir_reports_nothing_configured() {
        let _home = crate::keys::tests::TestHome::new();
        let project = tempfile::tempdir().unwrap();

        let mut ctx = test_ctx();
        ctx.working_dir = project.path().to_path_buf();
        let out = message_text(MemoryCommand.execute("status", &mut ctx).await);

        assert!(out.contains("No project memory yet"), "got: {}", out);
    }
}
