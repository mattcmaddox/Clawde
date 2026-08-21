// Goal command: durable long-running autonomous goals (`/goal`).
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::{ArgCompletion, CommandContext, CommandResult, SlashCommand};
use async_trait::async_trait;

pub struct GoalCommand;

// ---- /goal ---------------------------------------------------------------

/// Parse a soft token budget from strings like "250K", "1M", "500000".
fn parse_token_budget(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num_str, multiplier) = if let Some(n) = s.strip_suffix('K').or_else(|| s.strip_suffix('k'))
    {
        (n, 1_000u64)
    } else if let Some(n) = s.strip_suffix('M').or_else(|| s.strip_suffix('m')) {
        (n, 1_000_000u64)
    } else {
        (s, 1u64)
    };
    num_str.trim().parse::<u64>().ok().map(|n| n * multiplier)
}

#[async_trait]
impl SlashCommand for GoalCommand {
    fn name(&self) -> &str {
        "goal"
    }
    fn description(&self) -> &str {
        "Set or manage a durable long-running goal for autonomous work"
    }
    fn arg_completions(&self, partial: &str) -> Vec<ArgCompletion> {
        let mut completions = vec![
            ArgCompletion {
                value: "status".into(),
                description: "Show current goal status".into(),
                available: true,
            },
            ArgCompletion {
                value: "pause".into(),
                description: "Pause the active goal".into(),
                available: true,
            },
            ArgCompletion {
                value: "resume".into(),
                description: "Resume a paused goal".into(),
                available: true,
            },
            ArgCompletion {
                value: "clear".into(),
                description: "Delete the current goal".into(),
                available: true,
            },
            ArgCompletion {
                value: "complete".into(),
                description: "Request a completion audit".into(),
                available: true,
            },
        ];
        // The main argument is a free-form objective (e.g. `/goal migrate the
        // API to Fastify`). Show a dimmed placeholder hint while the field is
        // still empty so the popup says what goes next, mirroring the other
        // free-form hints (/keys, /config set model, /export --output).
        if partial.trim().is_empty() {
            if let Some(hint) = super::free_form_arg_hint(
                "",
                "<objective>",
                "Describe the goal to work toward autonomously",
                false,
            ) {
                completions.push(hint);
            }
        }
        completions
    }
    fn help(&self) -> &str {
        "Usage:\n\
         /goal <objective>              — set a new goal and begin working autonomously\n\
         /goal --tokens 250K <text>     — set a goal with a soft token budget\n\
         /goal                          — show current goal status\n\
         /goal status                   — show current goal status\n\
         /goal pause                    — pause the active goal\n\
         /goal resume                   — resume a paused goal\n\
         /goal clear                    — delete the current goal\n\
         /goal complete                 — request a completion audit\n\n\
         Goals let Clawde work autonomously across turns toward a single\n\
         verifiable objective. Clawde will keep iterating until the goal is\n\
         complete, you pause it, or the 200-turn runaway guard fires.\n\n\
         Examples:\n\
         /goal Migrate the project from Express to Fastify, keeping all routes passing\n\
         /goal --tokens 500K Fix all TypeScript errors in src/ without breaking tests"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        if !clawde_core::goals_enabled() {
            return CommandResult::Message(
                "Goals are disabled. Unset CLAURST_GOALS=0 (or remove it) to re-enable."
                    .to_string(),
            );
        }

        let args = args.trim();
        let session_id = &ctx.session_id;

        // Parse subcommands with no objective
        match args {
            "" | "status" => return goal_status(session_id),
            "pause" => {
                let store = match open_goal_store() {
                    Some(s) => s,
                    None => return CommandResult::Error("Could not open goal store.".to_string()),
                };
                match store.get_goal(session_id) {
                    None => return CommandResult::Message("No active goal.".to_string()),
                    Some(g) if g.status == clawde_core::GoalStatus::Complete => {
                        return CommandResult::Message("Goal is already complete.".to_string());
                    }
                    Some(g) if g.status == clawde_core::GoalStatus::Paused => {
                        return CommandResult::Message(
                            "Goal is already paused. Use /goal resume to continue.".to_string(),
                        );
                    }
                    _ => {}
                }
                if let Err(e) = store.set_status(session_id, clawde_core::GoalStatus::Paused) {
                    return CommandResult::Error(format!("Failed to pause goal: {}", e));
                }
                return CommandResult::Message(
                    "Goal paused. Use /goal resume to continue.".to_string(),
                );
            }
            "resume" => {
                let store = match open_goal_store() {
                    Some(s) => s,
                    None => return CommandResult::Error("Could not open goal store.".to_string()),
                };
                match store.get_goal(session_id) {
                    None => return CommandResult::Message("No goal to resume.".to_string()),
                    Some(g) if g.status == clawde_core::GoalStatus::Active => {
                        return CommandResult::Message("Goal is already active.".to_string());
                    }
                    Some(g) if g.status == clawde_core::GoalStatus::Complete => {
                        return CommandResult::Message(
                            "Goal is complete. Use /goal <objective> to set a new one.".to_string(),
                        );
                    }
                    _ => {}
                }
                // Re-baseline goal-scoped token accounting so tokens spent
                // while the goal was paused are never attributed to it (G7),
                // and clear the no-progress streak: a pause breaks the
                // "consecutive turns" the guard counts.
                if let Err(e) = store.rebaseline_tokens(session_id, ctx.cost_tracker.total_tokens())
                {
                    return CommandResult::Error(format!("Failed to resume goal: {}", e));
                }
                if let Err(e) = store.set_low_progress_streak(session_id, 0) {
                    return CommandResult::Error(format!("Failed to resume goal: {}", e));
                }
                if let Err(e) = store.set_status(session_id, clawde_core::GoalStatus::Active) {
                    return CommandResult::Error(format!("Failed to resume goal: {}", e));
                }
                return CommandResult::Message(
                    "Goal resumed. Clawde will continue on the next message.".to_string(),
                );
            }
            "clear" => {
                let store = match open_goal_store() {
                    Some(s) => s,
                    None => return CommandResult::Error("Could not open goal store.".to_string()),
                };
                store.clear_goal(session_id).unwrap_or_default();
                return CommandResult::Message("Goal cleared.".to_string());
            }
            "complete" => {
                // Inject a completion-audit user message.
                let store = match open_goal_store() {
                    Some(s) => s,
                    None => return CommandResult::Error("Could not open goal store.".to_string()),
                };
                match store.get_active_goal(session_id) {
                    None => {
                        return CommandResult::Message(
                            "No active goal. Set one with /goal <objective>.".to_string(),
                        );
                    }
                    Some(goal) => {
                        let audit_msg = format!(
                            "[User requested goal completion audit]\n\
                             Please review your active goal:\n\
                             <objective>\n{}\n</objective>\n\n\
                             Run through the completion audit:\n\
                             1. Restate the objective as concrete deliverables.\n\
                             2. Check that all deliverables have been achieved.\n\
                             3. Run any tests or validation commands.\n\
                             4. If fully complete, call GoalComplete with audit_summary and evidence.\n\
                             5. If not complete, describe what remains.",
                            goal.objective
                        );
                        return CommandResult::UserMessage(audit_msg);
                    }
                }
            }
            _ => {} // fall through to parse as objective (possibly with --tokens)
        }

        // Parse optional --tokens flag
        let (token_budget, objective) = if args.starts_with("--tokens") {
            // Expected: --tokens <budget> <objective>
            let rest = args.trim_start_matches("--tokens").trim();
            let mut parts = rest.splitn(2, char::is_whitespace);
            let budget_str = parts.next().unwrap_or("");
            let obj = parts.next().unwrap_or("").trim();
            let budget = parse_token_budget(budget_str);
            (budget, obj)
        } else {
            (None, args)
        };

        if objective.is_empty() {
            return CommandResult::Message(
                "Usage: /goal <objective> [--tokens 250K]\n\
                 Or: /goal status|pause|resume|clear|complete"
                    .to_string(),
            );
        }

        let store = match open_goal_store() {
            Some(s) => s,
            None => return CommandResult::Error("Could not open goal store.".to_string()),
        };

        // Seed the goal-scoped accounting baseline with the session's current
        // cumulative usage so pre-goal tokens never count toward the goal's
        // soft budget (G7).
        let session_tokens_at_start = ctx.cost_tracker.total_tokens();
        match store.set_goal(session_id, objective, token_budget, session_tokens_at_start) {
            Err(clawde_core::GoalError::ObjectiveTooLong { len, max }) => CommandResult::Error(
                format!("Objective too long ({} chars). Max {} chars.", len, max),
            ),
            Err(e) => CommandResult::Error(format!("Failed to set goal: {}", e)),
            Ok(goal) => {
                // Return UserMessage so the query loop fires immediately and the
                // model begins working toward the goal without user needing to
                // send another message.
                CommandResult::UserMessage(clawde_core::goal_kickoff_message(&goal))
            }
        }
    }
}

fn open_goal_store() -> Option<clawde_core::GoalStore> {
    clawde_core::GoalStore::open_default()
}

fn goal_status(session_id: &str) -> CommandResult {
    let store = match open_goal_store() {
        Some(s) => s,
        None => return CommandResult::Error("Could not open goal store.".to_string()),
    };
    match store.get_goal(session_id) {
        None => {
            CommandResult::Message("No active goal. Set one with:\n  /goal <objective>".to_string())
        }
        Some(g) => {
            let budget_line = g
                .budget_display()
                .map(|b| format!("\nBudget:  {}", b))
                .unwrap_or_default();
            CommandResult::Message(format!(
                "Goal status\n\
                 ───────────\n\
                 Status:  {}\n\
                 Turns:   {}\n\
                 Elapsed: {}{}\n\
                 Objective:\n  {}",
                g.status.as_str(),
                g.turns_used,
                g.elapsed_display(),
                budget_line,
                g.objective,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawde_core::{GoalStatus, GoalStore};
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Run a future with `CLAWDE_HOME` pointed at a fresh temp dir so goal
    /// DB reads/writes never touch the real config dir. Shares the commands
    /// crate's `CLAWDE_HOME_LOCK` with the keys/accounts tests so these tests
    /// serialize with every other env-mutating test under parallelism.
    #[allow(clippy::await_holding_lock)]
    // The guard must span the whole future: it serialises the CLAWDE_HOME
    // mutation against all other env-mutating tests in this crate (same
    // std::sync::Mutex convention as crate::paths::ENV_LOCK). Test-only.
    async fn with_temp_home<T>(f: impl FnOnce(PathBuf) -> T) -> T::Output
    where
        T: std::future::Future,
    {
        let _lock = crate::tests::CLAWDE_HOME_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = std::env::var_os("CLAWDE_HOME");
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("CLAWDE_HOME", tmp.path());
        let out = f(tmp.path().to_path_buf()).await;
        match prev {
            Some(v) => std::env::set_var("CLAWDE_HOME", v),
            None => std::env::remove_var("CLAWDE_HOME"),
        }
        out
    }

    fn make_ctx(working_dir: PathBuf) -> CommandContext {
        CommandContext {
            config: clawde_core::config::Config::default(),
            cost_tracker: clawde_core::cost::CostTracker::new(),
            messages: vec![],
            working_dir,
            session_id: "test-session".to_string(),
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

    fn open_store() -> GoalStore {
        GoalStore::open_default().expect("goal store opens under temp home")
    }

    #[test]
    fn parse_token_budget_handles_k_m_and_plain() {
        assert_eq!(parse_token_budget("250K"), Some(250_000));
        assert_eq!(parse_token_budget("2k"), Some(2_000));
        assert_eq!(parse_token_budget("1M"), Some(1_000_000));
        assert_eq!(parse_token_budget("3m"), Some(3_000_000));
        assert_eq!(parse_token_budget("500000"), Some(500_000));
        assert_eq!(parse_token_budget(" 250K "), Some(250_000));
    }

    #[test]
    fn parse_token_budget_rejects_garbage() {
        assert_eq!(parse_token_budget(""), None);
        assert_eq!(parse_token_budget("   "), None);
        assert_eq!(parse_token_budget("abc"), None);
        assert_eq!(parse_token_budget("10x"), None);
        assert_eq!(parse_token_budget("-5K"), None);
    }

    #[tokio::test]
    async fn status_without_goal_shows_prompt() {
        with_temp_home(|_home| async move {
            let mut ctx = make_ctx(PathBuf::from("."));
            match GoalCommand.execute("status", &mut ctx).await {
                CommandResult::Message(m) => assert!(m.contains("No active goal"), "{}", m),
                other => panic!("expected Message, got {:?}", other),
            }
        })
        .await;
    }

    #[tokio::test]
    async fn set_goal_persists_and_kicks_off() {
        with_temp_home(|_home| async move {
            let mut ctx = make_ctx(PathBuf::from("."));
            match GoalCommand
                .execute("migrate the API to Fastify", &mut ctx)
                .await
            {
                CommandResult::UserMessage(m) => {
                    assert!(m.contains("migrate the API to Fastify"), "{}", m)
                }
                other => panic!("expected UserMessage, got {:?}", other),
            }
            let goal = open_store()
                .get_goal("test-session")
                .expect("goal persisted");
            assert_eq!(goal.status, GoalStatus::Active);
            assert_eq!(goal.objective, "migrate the API to Fastify");
        })
        .await;
    }

    #[tokio::test]
    async fn set_goal_with_tokens_flag_parses_budget() {
        with_temp_home(|_home| async move {
            let mut ctx = make_ctx(PathBuf::from("."));
            GoalCommand
                .execute("--tokens 250K migrate the API to Fastify", &mut ctx)
                .await;
            let goal = open_store()
                .get_goal("test-session")
                .expect("goal persisted");
            assert_eq!(goal.token_budget, Some(250_000));
        })
        .await;
    }

    #[tokio::test]
    async fn status_shows_active_goal() {
        with_temp_home(|_home| async move {
            let mut ctx = make_ctx(PathBuf::from("."));
            GoalCommand
                .execute("migrate the API to Fastify", &mut ctx)
                .await;
            match GoalCommand.execute("status", &mut ctx).await {
                CommandResult::Message(m) => {
                    assert!(m.contains("Goal status"), "{}", m);
                    assert!(m.contains("migrate the API to Fastify"), "{}", m);
                    assert!(m.contains("Status:"), "{}", m);
                }
                other => panic!("expected Message, got {:?}", other),
            }
        })
        .await;
    }

    #[tokio::test]
    async fn pause_then_resume_round_trip() {
        with_temp_home(|_home| async move {
            let mut ctx = make_ctx(PathBuf::from("."));
            GoalCommand
                .execute("migrate the API to Fastify", &mut ctx)
                .await;

            match GoalCommand.execute("pause", &mut ctx).await {
                CommandResult::Message(m) => {
                    assert!(m.contains("Goal paused"), "{}", m);
                }
                other => panic!("expected Message, got {:?}", other),
            }
            assert_eq!(
                open_store().get_goal("test-session").unwrap().status,
                GoalStatus::Paused
            );
            // Pausing twice is idempotent and informative.
            match GoalCommand.execute("pause", &mut ctx).await {
                CommandResult::Message(m) => {
                    assert!(m.contains("already paused"), "{}", m);
                }
                other => panic!("expected Message, got {:?}", other),
            }

            match GoalCommand.execute("resume", &mut ctx).await {
                CommandResult::Message(m) => {
                    assert!(m.contains("Goal resumed"), "{}", m);
                }
                other => panic!("expected Message, got {:?}", other),
            }
            assert_eq!(
                open_store().get_goal("test-session").unwrap().status,
                GoalStatus::Active
            );
        })
        .await;
    }

    #[tokio::test]
    async fn clear_removes_goal() {
        with_temp_home(|_home| async move {
            let mut ctx = make_ctx(PathBuf::from("."));
            GoalCommand
                .execute("migrate the API to Fastify", &mut ctx)
                .await;
            match GoalCommand.execute("clear", &mut ctx).await {
                CommandResult::Message(m) => assert_eq!(m, "Goal cleared."),
                other => panic!("expected Message, got {:?}", other),
            }
            assert!(open_store().get_goal("test-session").is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn complete_without_goal_is_informative() {
        with_temp_home(|_home| async move {
            let mut ctx = make_ctx(PathBuf::from("."));
            match GoalCommand.execute("complete", &mut ctx).await {
                CommandResult::Message(m) => {
                    assert!(m.contains("No active goal"), "{}", m);
                }
                other => panic!("expected Message, got {:?}", other),
            }
        })
        .await;
    }

    #[tokio::test]
    async fn complete_with_goal_injects_audit_message() {
        with_temp_home(|_home| async move {
            let mut ctx = make_ctx(PathBuf::from("."));
            GoalCommand
                .execute("migrate the API to Fastify", &mut ctx)
                .await;
            match GoalCommand.execute("complete", &mut ctx).await {
                CommandResult::UserMessage(m) => {
                    assert!(m.contains("completion audit"), "{}", m);
                    assert!(m.contains("migrate the API to Fastify"), "{}", m);
                }
                other => panic!("expected UserMessage, got {:?}", other),
            }
        })
        .await;
    }

    #[tokio::test]
    async fn empty_objective_shows_usage() {
        with_temp_home(|_home| async move {
            let mut ctx = make_ctx(PathBuf::from("."));
            match GoalCommand.execute("--tokens 250K", &mut ctx).await {
                CommandResult::Message(m) => {
                    assert!(m.contains("Usage: /goal <objective>"), "{}", m);
                }
                other => panic!("expected Message, got {:?}", other),
            }
        })
        .await;
    }

    #[tokio::test]
    async fn objective_too_long_errors() {
        with_temp_home(|_home| async move {
            let mut ctx = make_ctx(PathBuf::from("."));
            let long = "x".repeat(clawde_core::MAX_OBJECTIVE_CHARS + 1);
            match GoalCommand.execute(&long, &mut ctx).await {
                CommandResult::Error(e) => {
                    assert!(e.contains("Objective too long"), "{}", e);
                }
                other => panic!("expected Error, got {:?}", other),
            }
        })
        .await;
    }
}
