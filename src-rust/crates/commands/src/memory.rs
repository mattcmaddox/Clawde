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
    fn aliases(&self) -> Vec<&str> {
        // Themed aliases: `/mnemosyne` is a drop-in for `/memory`;
        // `/lethesyne` (memory conflicts) defaults to the status/conflicts
        // view when invoked without a subcommand (dispatcher translates the
        // empty-args call to `status`).
        vec!["mnemosyne", "lethesyne"]
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
            ArgCompletion {
                value: "undo".into(),
                description: "Reverse the most recent conflict resolution".into(),
                available: true,
            },
        ]
    }
    fn help(&self) -> &str {
        "Usage: /memory [edit|clear|status|init|undo] [global]\n\n\
         Shows the content of AGENTS.md files that provide project context to Clawde.\n\
         Clawde reads these files automatically at session start.\n\n\
         Subcommands:\n\
           /memory              — show all AGENTS.md files\n\
           /memory edit         — open project AGENTS.md in your editor\n\
           /memory edit global  — open global ~/.clawde/AGENTS.md in your editor\n\
           /memory clear        — clear the project AGENTS.md\n\
           /memory clear global — clear the global ~/.clawde/AGENTS.md\n\
           /memory status       — show project memory dir, index, and session summaries\n\
           /memory init         — seed architecture/conventions/decisions/tasks + MEMORY.md\n\
           /memory undo         — reverse the most recent conflict resolution\n\n\
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

            let mut out = String::from("Mnemosyne initialized\n═══════════════════════════\n");
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

        // ---- /memory undo ------------------------------------------------------
        if cmd == "undo" {
            use clawde_core::memdir::{auto_memory_path, undo_last_resolution};
            let mem_dir = auto_memory_path(&ctx.working_dir);
            if !mem_dir.is_dir() {
                return CommandResult::Error(format!(
                    "No project memory directory at {} — nothing to undo",
                    mem_dir.display()
                ));
            }
            return match undo_last_resolution(&mem_dir) {
                Ok(report) => CommandResult::Message(format!(
                    "Undid the most recent resolution ({} vs {} → {}):\n  {}",
                    report.record.claimant,
                    report.record.target,
                    report.record.decision,
                    report.summary
                )),
                Err(e) => CommandResult::Error(e),
            };
        }

        // ---- /memory status ----------------------------------------------------
        if cmd == "status" {
            use clawde_core::memdir::{
                auto_memory_path, load_memory_index, most_recent_session_summary, scan_memory_dir,
                sweep_dangling_memory_refs,
            };
            let mem_dir = auto_memory_path(&ctx.working_dir);
            let mut out = String::from("Mnemosyne (auto-memory)\n══════════════════════════════\n");
            out.push_str(&format!("Directory: {}\n", mem_dir.display()));
            if !mem_dir.is_dir() {
                out.push_str(
                    "No project memory yet. It is created automatically when the agent\n\
                     first writes a memory file (auto-dream consolidation or the memory tools).",
                );
                return CommandResult::Message(out);
            }
            // Hygiene pass: drop conflicts/supersedes/asked/resolved entries
            // pointing at deleted files so the report below reflects the
            // clean state.
            let sweep = sweep_dangling_memory_refs(&mem_dir);
            if !sweep.is_empty() {
                out.push_str(&format!(
                    "Cleaned dangling memory refs: {} conflicts, {} supersedes, {} asked, {} resolved\n",
                    sweep.removed_conflicts.len(),
                    sweep.removed_supersedes.len(),
                    sweep.removed_asked.len(),
                    sweep.removed_resolved.len()
                ));
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
            // Unconfirmed contradictions the user has not adjudicated. These
            // are resolved conversationally (AskUserQuestion) when a session
            // touches the subject — this line is purely informational. The
            // count is the shared pending-conflict *pair* count (same source
            // as the injected block and the TUI indicator), so resolved /
            // dangling / self / superseded-claimant entries never inflate it.
            let pending_pairs = clawde_core::memdir::pending_conflict_pairs(&mem_dir);
            out.push_str(&format!("Lethesyne: {}\n", pending_pairs.len()));
            if !pending_pairs.is_empty() {
                // One claimant may have several pending pairs — list each
                // claimant once (the count above is per-pair).
                let mut claimants: Vec<&str> = pending_pairs
                    .iter()
                    .map(|(claimant, _)| claimant.as_str())
                    .collect();
                claimants.sort_unstable();
                claimants.dedup();
                out.push_str(&format!(
                    "  ({} — resolved in conversation when the topic comes up)\n",
                    claimants.join(", ")
                ));
            }
            // Recent conflict resolutions (audit trail + undo source). The
            // `.resolutions.jsonl` log is written by `resolve_memory_conflict`
            // whenever the user adjudicates a pending conflict.
            let resolutions = clawde_core::memdir::recent_resolutions(&mem_dir, 5);
            if !resolutions.is_empty() {
                out.push_str(&format!(
                    "Resolved conflicts (last {}):\n",
                    resolutions.len()
                ));
                let now_secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                for record in &resolutions {
                    let age_secs = now_secs.saturating_sub(record.ts);
                    let age = if age_secs < 60 {
                        format!("{}s ago", age_secs)
                    } else if age_secs < 3600 {
                        format!("{} min ago", age_secs / 60)
                    } else {
                        format!("{}h ago", age_secs / 3600)
                    };
                    out.push_str(&format!(
                        "  {}: {} vs {} → {}\n",
                        age, record.claimant, record.target, record.decision
                    ));
                }
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

            // ---- AutoDream consolidation state (observability) -------------
            // The state file is written by the auto-dream daemon in
            // clawde-query; surface it here so a user can audit when the last
            // consolidation ran, whether attempts are failing, and how much
            // work is pending before the next trigger.
            let state_path = mem_dir.join(".consolidation_state.json");
            if state_path.exists() {
                if let Ok(raw) = std::fs::read_to_string(&state_path) {
                    if let Ok(state) = serde_json::from_str::<serde_json::Value>(&raw) {
                        let age_of = |key: &str| -> String {
                            state
                                .get(key)
                                .and_then(|v| v.as_u64())
                                .map(|secs| {
                                    let age_secs = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_secs().saturating_sub(secs))
                                        .unwrap_or(0);
                                    if age_secs < 3600 {
                                        format!("{} min ago", age_secs / 60)
                                    } else {
                                        format!("{}h ago", age_secs / 3600)
                                    }
                                })
                                .unwrap_or_else(|| "never".to_string())
                        };
                        let importance_kb = state
                            .get("importance")
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0)
                            / 1000.0;
                        let failures = state
                            .get("consecutive_failures")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let threshold_kb = ctx
                            .config
                            .memory
                            .auto_dream_min_importance_kb
                            .unwrap_or(150.0);
                        out.push_str(&format!(
                            "AutoDream: last consolidation {}, last attempt {}\n",
                            age_of("last_consolidated_at"),
                            age_of("last_attempt_at")
                        ));
                        out.push_str(&format!(
                            "  Pending work: {:.0} KB / {} KB threshold\n",
                            importance_kb, threshold_kb
                        ));
                        out.push_str(&format!(
                            "  Consecutive failed dreams: {} (backoff doubles per failure)\n",
                            failures
                        ));
                    }
                }
            } else {
                out.push_str(
                    "AutoDream: never run (no consolidation state — needs a project \
                     memory dir and accumulated session activity)\n",
                );
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
            effort: None,
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

        assert!(out.contains("Mnemosyne (auto-memory)"), "got: {}", out);
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

    #[tokio::test]
    async fn memory_show_all_lists_priority_locations() {
        let _home = crate::keys::tests::TestHome::new();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("AGENTS.md"), "project rules here").unwrap();

        let mut ctx = test_ctx();
        ctx.working_dir = project.path().to_path_buf();
        let out = message_text(MemoryCommand.execute("", &mut ctx).await);

        assert!(out.contains("[project (AGENTS.md)]"), "got: {}", out);
        assert!(out.contains("project rules here"), "got: {}", out);
        assert!(out.contains("Path:"), "got: {}", out);
        // Global location is empty under the temp home, but the project one
        // shows — and the subcommand footer appears.
        assert!(out.contains("/memory edit"), "got: {}", out);
    }

    #[tokio::test]
    async fn memory_show_all_with_no_files_is_informative() {
        let _home = crate::keys::tests::TestHome::new();
        let project = tempfile::tempdir().unwrap();

        let mut ctx = test_ctx();
        ctx.working_dir = project.path().to_path_buf();
        let out = message_text(MemoryCommand.execute("", &mut ctx).await);

        assert!(out.contains("No AGENTS.md files found."), "got: {}", out);
        assert!(out.contains("Use /init to create one"), "got: {}", out);
    }

    #[tokio::test]
    async fn memory_clear_empties_project_file() {
        let _home = crate::keys::tests::TestHome::new();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("AGENTS.md"), "secret stuff").unwrap();

        let mut ctx = test_ctx();
        ctx.working_dir = project.path().to_path_buf();
        let out = message_text(MemoryCommand.execute("clear", &mut ctx).await);

        assert!(out.contains("Cleared project"), "got: {}", out);
        let content = std::fs::read_to_string(project.path().join("AGENTS.md")).unwrap();
        assert_eq!(content, "");
    }

    #[tokio::test]
    async fn memory_clear_without_file_is_informative() {
        let _home = crate::keys::tests::TestHome::new();
        let project = tempfile::tempdir().unwrap();

        let mut ctx = test_ctx();
        ctx.working_dir = project.path().to_path_buf();
        let out = message_text(MemoryCommand.execute("clear", &mut ctx).await);

        assert!(out.contains("nothing to clear"), "got: {}", out);
    }

    #[tokio::test]
    async fn memory_status_shows_resolution_audit() {
        let _home = crate::keys::tests::TestHome::new();
        let project = tempfile::tempdir().unwrap();
        let mem_dir = clawde_core::memdir::auto_memory_path(project.path());
        std::fs::create_dir_all(&mem_dir).unwrap();
        std::fs::write(
            mem_dir.join("auth-flow-v1.md"),
            "---\ndescription: JWT\n---\n",
        )
        .unwrap();
        std::fs::write(
            mem_dir.join("auth-flow-v2.md"),
            "---\ndescription: OAuth\nconflicts: auth-flow-v1.md\n---\n",
        )
        .unwrap();
        // Resolve through the real core state machine so the audit log is
        // written by the same path the tool uses.
        clawde_core::memdir::resolve_memory_conflict(
            &mem_dir,
            "auth-flow-v2.md",
            "auth-flow-v1.md",
            clawde_core::memdir::ConflictDecision::KeepNew,
        )
        .unwrap();

        let mut ctx = test_ctx();
        ctx.working_dir = project.path().to_path_buf();
        let out = message_text(MemoryCommand.execute("status", &mut ctx).await);
        assert!(out.contains("Resolved conflicts (last 1)"), "got: {}", out);
        assert!(
            out.contains("auth-flow-v2.md vs auth-flow-v1.md → keep_new"),
            "got: {}",
            out
        );
        // The resolved pair is no longer pending.
        assert!(out.contains("Lethesyne: 0"), "got: {}", out);
    }

    #[tokio::test]
    async fn lethesyne_alias_defaults_to_conflicts_view() {
        let _home = crate::keys::tests::TestHome::new();
        let project = tempfile::tempdir().unwrap();
        let mem_dir = clawde_core::memdir::auto_memory_path(project.path());
        std::fs::create_dir_all(&mem_dir).unwrap();
        std::fs::write(mem_dir.join("MEMORY.md"), "# Index\n").unwrap();
        std::fs::write(mem_dir.join("prefs.md"), "---\ndescription: Concise\n---\n").unwrap();
        std::fs::write(
            mem_dir.join("verbose-claim.md"),
            "---\ndescription: Verbose\nconflicts: prefs.md\n---\n",
        )
        .unwrap();

        let mut ctx = test_ctx();
        ctx.working_dir = project.path().to_path_buf();

        // `/lethesyne` with no subcommand defaults to the conflicts/status view.
        let out = match execute_command("/lethesyne", &mut ctx).await {
            Some(CommandResult::Message(text)) => text,
            other => panic!("expected Message, got {:?}", other),
        };
        assert!(out.contains("Mnemosyne (auto-memory)"), "got: {}", out);
        assert!(out.contains("Lethesyne: 1"), "got: {}", out);

        // `/mnemosyne` with an explicit subcommand behaves like `/memory`.
        let out2 = match execute_command("/mnemosyne status", &mut ctx).await {
            Some(CommandResult::Message(text)) => text,
            other => panic!("expected Message, got {:?}", other),
        };
        assert!(out2.contains("Mnemosyne (auto-memory)"), "got: {}", out2);
        assert!(out2.contains("Lethesyne: 1"), "got: {}", out2);

        // `/mnemosyne` with no args matches `/memory` (the AGENTS.md listing).
        std::fs::write(project.path().join("AGENTS.md"), "project rules\n").unwrap();
        let out3 = match execute_command("/mnemosyne", &mut ctx).await {
            Some(CommandResult::Message(text)) => text,
            other => panic!("expected Message, got {:?}", other),
        };
        assert!(out3.contains("AGENTS.md Memory Files"), "got: {}", out3);
    }

    #[tokio::test]
    async fn memory_undo_reverses_last_resolution() {
        let _home = crate::keys::tests::TestHome::new();
        let project = tempfile::tempdir().unwrap();
        let mem_dir = clawde_core::memdir::auto_memory_path(project.path());
        std::fs::create_dir_all(&mem_dir).unwrap();
        std::fs::write(
            mem_dir.join("auth-flow-v1.md"),
            "---\ndescription: JWT\n---\n",
        )
        .unwrap();
        std::fs::write(
            mem_dir.join("auth-flow-v2.md"),
            "---\ndescription: OAuth\nconflicts: auth-flow-v1.md\n---\n",
        )
        .unwrap();
        // Resolve through the real state machine, then reverse it.
        clawde_core::memdir::resolve_memory_conflict(
            &mem_dir,
            "auth-flow-v2.md",
            "auth-flow-v1.md",
            clawde_core::memdir::ConflictDecision::KeepNew,
        )
        .unwrap();

        let mut ctx = test_ctx();
        ctx.working_dir = project.path().to_path_buf();
        let out = message_text(MemoryCommand.execute("undo", &mut ctx).await);
        assert!(
            out.contains("Undid the most recent resolution"),
            "got: {}",
            out
        );
        assert!(out.contains("keep_new"), "got: {}", out);

        // The frontmatter is back to the pending-conflict state.
        let content = std::fs::read_to_string(mem_dir.join("auth-flow-v2.md")).unwrap();
        assert!(
            content.contains("conflicts: auth-flow-v1.md"),
            "got: {}",
            content
        );
        assert!(!content.contains("supersedes:"), "got: {}", content);
        // The log is drained, so a second undo is an error.
        let err = match MemoryCommand.execute("undo", &mut ctx).await {
            CommandResult::Error(e) => e,
            other => panic!("expected Error, got {:?}", other),
        };
        assert!(err.contains("no resolutions to undo"), "got: {}", err);
    }

    #[tokio::test]
    async fn memory_undo_without_log_is_informative() {
        let _home = crate::keys::tests::TestHome::new();
        let project = tempfile::tempdir().unwrap();
        let mem_dir = clawde_core::memdir::auto_memory_path(project.path());
        std::fs::create_dir_all(&mem_dir).unwrap();

        let mut ctx = test_ctx();
        ctx.working_dir = project.path().to_path_buf();
        let err = match MemoryCommand.execute("undo", &mut ctx).await {
            CommandResult::Error(e) => e,
            other => panic!("expected Error, got {:?}", other),
        };
        assert!(err.contains("no resolutions to undo"), "got: {}", err);
    }

    #[tokio::test]
    async fn memory_status_reports_cleaned_dangling_refs() {
        let _home = crate::keys::tests::TestHome::new();
        let project = tempfile::tempdir().unwrap();
        let mem_dir = clawde_core::memdir::auto_memory_path(project.path());
        std::fs::create_dir_all(&mem_dir).unwrap();
        std::fs::write(mem_dir.join("MEMORY.md"), "# Index\n").unwrap();
        std::fs::write(mem_dir.join("alive.md"), "---\ndescription: alive\n---\n").unwrap();
        // One dangling conflict + one dangling supersedes.
        std::fs::write(
            mem_dir.join("claim.md"),
            "---\ndescription: claim\nconflicts: alive.md, deleted.md\nsupersedes: gone.md\n---\nbody",
        )
        .unwrap();

        let mut ctx = test_ctx();
        ctx.working_dir = project.path().to_path_buf();
        let out = message_text(MemoryCommand.execute("status", &mut ctx).await);
        assert!(
            out.contains("Cleaned dangling memory refs: 1 conflicts, 1 supersedes"),
            "got: {}",
            out
        );
        // The live conflict survives the sweep.
        assert!(out.contains("Lethesyne: 1"), "got: {}", out);
        // Second status run reports a clean sweep.
        let out2 = message_text(MemoryCommand.execute("status", &mut ctx).await);
        assert!(!out2.contains("Cleaned dangling"), "got: {}", out2);
    }

    #[tokio::test]
    async fn memory_edit_creates_file_without_spawning_real_editor() {
        let _home = crate::keys::tests::TestHome::new();
        let project = tempfile::tempdir().unwrap();

        // Point EDITOR at a no-op so no interactive editor opens; the test
        // asserts the file side effect, which is platform-independent.
        let prev = std::env::var_os("EDITOR");
        std::env::set_var("EDITOR", if cfg!(windows) { "cmd" } else { "true" });

        let mut ctx = test_ctx();
        ctx.working_dir = project.path().to_path_buf();
        let result = MemoryCommand.execute("edit", &mut ctx).await;

        match prev {
            Some(v) => std::env::set_var("EDITOR", v),
            None => std::env::remove_var("EDITOR"),
        }

        // The file must exist (created empty) regardless of whether the
        // no-op editor launched successfully.
        let target = project.path().join("AGENTS.md");
        assert!(target.is_file(), "edit did not create AGENTS.md");
        let content = std::fs::read_to_string(&target).unwrap();
        assert_eq!(content, "");
        match result {
            CommandResult::Message(m) => {
                assert!(
                    m.contains("Opened") || m.contains("Could not launch"),
                    "got: {}",
                    m
                );
            }
            other => panic!("expected Message, got {:?}", other),
        }
    }
}
