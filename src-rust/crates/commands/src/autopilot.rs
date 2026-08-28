//! `/autopilot` — toggle and manage the session's autopilot posture
//! (spec `docs/plans/bypass-autopilot-safety-spec.md` Phase 4C).
//!
//! Bare `/autopilot` toggles autopilot on/off for the current session
//! (deliberately no `on`/`off` subcommand). Subcommands:
//!
//! - `/autopilot status` — posture, pending count, safety note
//! - `/autopilot list` — print pending deferred items inline
//! - `/autopilot reject <id>` — reject a deferred item
//! - `/autopilot approve <id>` — approve a deferred tool call for replay
//!   (Phase 4D); the approval is consumed when the model retries the exact call
//! - `/autopilot answer <id> <text>` — answer a deferred question; the answer
//!   is injected into the next model turn as a user message

use crate::{ArgCompletion, CommandContext, CommandResult, SlashCommand};
use async_trait::async_trait;
use clawde_core::action_risk::{classify_action, ActionRisk};
use clawde_core::autonomy::{AutonomyState, DeferredItem, DeferredPayload, DeferredState};

pub struct AutopilotCommand;
/// Persistent safety note shown in status/toggle output.
const SAFETY_NOTE: &str = "Autopilot runs only actions classified safe. \
Review-required actions are deferred; irreversible actions are denied. \
Nothing deferred executes until you approve it. Approving a deferred tool \
call pre-authorizes exactly that call; the agent must retry it and the \
approval is consumed on first execution.";

/// Shown when the autonomy state is not wired (headless / gateway / ACP).
const UNAVAILABLE_MSG: &str = "Autopilot is unavailable in this session (headless mode). \
It requires an interactive session so the review queue stays visible.";

impl AutopilotCommand {
    /// Resolve the session autonomy state, failing closed when autopilot is
    /// not wired (headless / gateway / ACP sessions).
    fn state(ctx: &CommandContext) -> Option<&parking_lot::Mutex<AutonomyState>> {
        ctx.autonomy.as_deref()
    }

    fn unavailable() -> CommandResult {
        CommandResult::Error(UNAVAILABLE_MSG.to_string())
    }

    fn toggle(ctx: &CommandContext) -> CommandResult {
        let Some(state) = Self::state(ctx) else {
            return Self::unavailable();
        };
        let mut s = state.lock();
        if s.is_active(&ctx.session_id) {
            s.stop_autopilot();
            let pending = s.pending_count();
            CommandResult::Message(format!(
                "Autopilot off. {} pending item{} remain{} visible for review but will not run.",
                pending,
                if pending == 1 { "" } else { "s" },
                if pending == 1 { "s" } else { "" },
            ))
        } else {
            s.start_autopilot(&ctx.session_id);
            let pending = s.pending_count();
            CommandResult::Message(format!(
                "AUTOPILOT ACTIVE — {} pending. {}",
                pending, SAFETY_NOTE
            ))
        }
    }

    fn status(ctx: &CommandContext) -> CommandResult {
        let Some(state) = Self::state(ctx) else {
            return Self::unavailable();
        };
        let s = state.lock();
        let posture = if s.is_active(&ctx.session_id) {
            "ACTIVE"
        } else {
            "OFF"
        };
        let now = s.now();
        let session_pending = s
            .items
            .iter()
            .filter(|item| {
                matches!(item.state, DeferredState::Pending | DeferredState::Stale)
                    && !item.is_expired(now)
                    && item.session_id == ctx.session_id
            })
            .count();
        let blast = &s.blast_radius;
        let blast_line = if blast.has_activity() {
            format!(
                "\nBlast radius: {} files changed, {} risky actions allowed, {} irreversible denied",
                blast.files_changed, blast.risky_actions_allowed, blast.irreversible_denied,
            )
        } else {
            String::new()
        };
        CommandResult::Message(format!(
            "Autopilot: {posture}\nPending for this session: {session_pending}\nQueue: {}/{} items{blast_line}\n{SAFETY_NOTE}",
            s.items.len(),
            s.capacity,
        ))
    }

    fn list(ctx: &CommandContext) -> CommandResult {
        let Some(state) = Self::state(ctx) else {
            return Self::unavailable();
        };
        let s = state.lock();
        let now = s.now();
        let items: Vec<&clawde_core::autonomy::DeferredItem> = s
            .items
            .iter()
            .filter(|item| {
                matches!(
                    item.state,
                    DeferredState::Pending | DeferredState::Stale | DeferredState::Approved
                ) && !item.is_expired(now)
                    && item.session_id == ctx.session_id
            })
            .collect();
        if items.is_empty() {
            return CommandResult::Message(
                "No pending autopilot items for this session.".to_string(),
            );
        }
        let pending = items
            .iter()
            .filter(|item| item.state == DeferredState::Pending)
            .count();
        let stale = items
            .iter()
            .filter(|item| item.state == DeferredState::Stale)
            .count();
        let approved = items.len() - pending - stale;
        let mut lines = vec![format!(
            "Autopilot items ({} pending, {} stale, {} approved)",
            pending, stale, approved
        )];
        lines.push("━━━━━━━━━━━━━━━━".to_string());
        for item in items {
            let age = format_age(item.created_at_unix);
            match &item.payload {
                clawde_core::autonomy::DeferredPayload::ToolCall { tool_name, request } => {
                    let target = request
                        .path
                        .clone()
                        .or_else(|| request.details.clone())
                        .unwrap_or_else(|| request.description.clone());
                    let marker = match item.state {
                        DeferredState::Approved => {
                            "APPROVED — the agent will run it when it retries the exact call"
                        }
                        DeferredState::Stale => {
                            "STALE — restored after restart; re-approve to revalidate"
                        }
                        _ => "",
                    };
                    lines.push(format!(
                        "  {}  {}: {}  · {} · {}{}",
                        item.id,
                        tool_name,
                        target,
                        risk_label(item.risk),
                        age,
                        if marker.is_empty() {
                            String::new()
                        } else {
                            format!("  · {}", marker)
                        }
                    ));
                    if item.state == DeferredState::Approved {
                        lines.push("      awaiting the agent's retry".to_string());
                    } else {
                        lines.push(format!(
                            "      /autopilot approve {} | /autopilot reject {}",
                            item.id, item.id
                        ));
                    }
                }
                clawde_core::autonomy::DeferredPayload::UserQuestion { question, options } => {
                    let choices = options
                        .as_ref()
                        .map(|opts| format!("  [{}]", opts.join(" | ")))
                        .unwrap_or_default();
                    lines.push(format!(
                        "  {}  Question: {}{}  · {}",
                        item.id, question, choices, age
                    ));
                    lines.push(format!("      /autopilot answer {} <text>", item.id));
                }
            }
        }
        CommandResult::Message(lines.join("\n"))
    }

    fn reject(ctx: &CommandContext, id: &str) -> CommandResult {
        let Some(state) = Self::state(ctx) else {
            return Self::unavailable();
        };
        let mut s = state.lock();
        match s.reject_item(&ctx.session_id, id) {
            Ok(()) => {
                CommandResult::Message(format!("Rejected {}. The agent will not run it.", id))
            }
            Err(e) => CommandResult::Error(e),
        }
    }

    fn approve(ctx: &CommandContext, id: &str) -> CommandResult {
        let Some(state) = Self::state(ctx) else {
            return Self::unavailable();
        };
        let mut s = state.lock();
        // Validation closure: the tool must still exist and re-classifying the
        // stored request must not turn irreversible. Runs while holding the
        // lock so the approval decision and the state change are atomic.
        let validate = |item: &DeferredItem| -> Result<(), String> {
            let DeferredPayload::ToolCall { tool_name, request } = &item.payload else {
                return Ok(());
            };
            if clawde_tools::find_tool(tool_name).is_none() {
                return Err(format!(
                    "Tool '{}' no longer exists; the deferred call cannot be replayed.",
                    tool_name
                ));
            }
            let risk = classify_action(
                tool_name,
                &request.description,
                request.permission_level,
                request.path.as_deref(),
                request.network_capable,
                request.stateful,
            );
            if risk == ActionRisk::Irreversible {
                return Err(format!(
                    "{} is now classified irreversible; it cannot be approved for replay.",
                    tool_name
                ));
            }
            Ok(())
        };
        match s.approve_item(&ctx.session_id, id, validate) {
            Ok(()) => {
                let summary = s
                    .items
                    .iter()
                    .find(|item| item.id == id)
                    .and_then(|item| match &item.payload {
                        clawde_core::autonomy::DeferredPayload::ToolCall {
                            tool_name,
                            request,
                            ..
                        } => Some(format!(
                            "{}: {}",
                            tool_name,
                            request
                                .details
                                .as_deref()
                                .or(request.path.as_deref())
                                .unwrap_or(&request.description)
                        )),
                        _ => None,
                    })
                    .unwrap_or_else(|| id.to_string());
                // Injected into the next model turn: the agent retries the
                // exact tool call and the approval is consumed on dispatch.
                CommandResult::UserMessage(format!(
                    "You approved deferred item {} ({}). Retry this exact action now; it has \
                     been pre-approved and will run when you issue it. The approval is \
                     consumed on that one execution.",
                    id, summary
                ))
            }
            Err(e) => CommandResult::Error(e),
        }
    }

    fn answer(ctx: &CommandContext, id: &str, text: &str) -> CommandResult {
        let Some(state) = Self::state(ctx) else {
            return Self::unavailable();
        };
        let mut s = state.lock();
        let answer = text.trim();
        if answer.is_empty() {
            return CommandResult::Error(format!("Usage: /autopilot answer {} <text>", id));
        }
        match s.answer_question(&ctx.session_id, id) {
            Ok(_question) => {
                // Inject the answer into the next model turn so the agent
                // actually sees it and can continue the work that was waiting
                // on the question.
                CommandResult::UserMessage(format!(
                    "The user answered your deferred question {}: {}",
                    id, answer
                ))
            }
            Err(e) => CommandResult::Error(e),
        }
    }
}

fn risk_label(risk: clawde_core::action_risk::ActionRisk) -> &'static str {
    match risk {
        clawde_core::action_risk::ActionRisk::Safe => "safe",
        clawde_core::action_risk::ActionRisk::ReviewRequired => "review-required",
        clawde_core::action_risk::ActionRisk::Irreversible => "irreversible",
    }
}

/// Human "N s / N m / N h ago" from a unix timestamp.
fn format_age(created_at_unix: i64) -> String {
    let now = chrono::Utc::now().timestamp();
    let secs = (now - created_at_unix).max(0);
    if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

#[async_trait]
impl SlashCommand for AutopilotCommand {
    fn name(&self) -> &str {
        "autopilot"
    }

    fn description(&self) -> &str {
        "Toggle autopilot and manage deferred review items"
    }

    fn help(&self) -> &str {
        "Usage:\n\
         \n\
         /autopilot                toggle autopilot on/off for this session\n\
         /autopilot status         show posture and pending count\n\
         /autopilot list           list pending deferred items\n\
         /autopilot reject <id>    reject a deferred item\n\
         /autopilot approve <id>   approve a deferred tool call for replay; the\n\
                                approval is consumed when the agent retries the\n\
                                exact call\n\
         /autopilot answer <id> <text>   answer a deferred question; the answer\n\
                                is injected into the next model turn\n\
         \n\
         Autopilot runs only actions classified safe. Review-required actions\n\
         are deferred with a stable id (AP-001, …) and the agent continues.\n\
         Irreversible actions are always denied. Nothing deferred executes\n\
         until you approve it — approving pre-authorizes exactly that call, and\n\
         the approval is consumed on its first execution."
    }

    fn arg_completions(&self, partial: &str) -> Vec<ArgCompletion> {
        let args: Vec<&str> = partial.split_whitespace().collect();
        // After the subcommand word, offer pending ids (best-effort; the
        // command cannot reach the autonomy state here, so ids are not
        // enumerated — the user can still type them).
        if args.len() >= 2 {
            let sub = args[0];
            if matches!(sub, "reject" | "approve" | "answer") {
                return vec![ArgCompletion {
                    value: "<AP-id>".to_string(),
                    description: "Deferred item id from /autopilot list".to_string(),
                    available: false,
                }];
            }
            return vec![];
        }
        let word = args.first().copied().unwrap_or("");
        ["status", "list", "reject", "approve", "answer"]
            .into_iter()
            .filter(|sub| sub.starts_with(word))
            .map(|sub| ArgCompletion {
                value: sub.to_string(),
                description: match sub {
                    "status" => "Show posture and pending count",
                    "list" => "List pending deferred items",
                    "reject" => "Reject a deferred item",
                    "approve" => "Approve a deferred tool call for replay",
                    _ => "Answer a deferred question",
                }
                .to_string(),
                available: true,
            })
            .collect()
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let args = args.trim();
        let (sub, rest) = match args.split_once(char::is_whitespace) {
            Some((a, b)) => (a, b.trim()),
            None => (args, ""),
        };
        match sub {
            "" => Self::toggle(ctx),
            "toggle" => Self::toggle(ctx),
            "status" => Self::status(ctx),
            "list" | "ls" => Self::list(ctx),
            "reject" => {
                let id = rest.split_whitespace().next().unwrap_or("");
                if id.is_empty() {
                    CommandResult::Error("Usage: /autopilot reject <id>".to_string())
                } else {
                    Self::reject(ctx, id)
                }
            }
            "approve" => {
                let id = rest.split_whitespace().next().unwrap_or("");
                if id.is_empty() {
                    CommandResult::Error("Usage: /autopilot approve <id>".to_string())
                } else {
                    Self::approve(ctx, id)
                }
            }
            "answer" => {
                let mut parts = rest.splitn(2, char::is_whitespace);
                let id = parts.next().unwrap_or("");
                let text = parts.next().unwrap_or("");
                if id.is_empty() {
                    CommandResult::Error("Usage: /autopilot answer <id> <text>".to_string())
                } else {
                    Self::answer(ctx, id, text)
                }
            }
            other => CommandResult::Error(format!(
                "Unknown /autopilot subcommand '{}'. Try /autopilot status, list, reject, answer.",
                other
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawde_core::permissions::{PermissionLevel, PermissionRequest};
    use std::sync::Arc;

    fn ctx_with_state() -> (CommandContext, Arc<parking_lot::Mutex<AutonomyState>>) {
        let mut ctx = CommandContext {
            config: clawde_core::config::Config::default(),
            cost_tracker: clawde_core::cost::CostTracker::new(),
            messages: vec![],
            working_dir: std::path::PathBuf::from("/project"),
            session_id: "s1".to_string(),
            session_title: None,
            remote_session_url: None,
            mcp_manager: None,
            mcp_auth_runner: None,
            provider_registry: None,
            test_provider: None,
            effort: None,
            tool_use_tracker: None,
            autonomy: None,
            transient_prev_config: None,
        };
        let state = Arc::new(parking_lot::Mutex::new(AutonomyState::new("s1")));
        ctx.autonomy = Some(state.clone());
        (ctx, state)
    }

    fn sample_request() -> PermissionRequest {
        PermissionRequest {
            tool_name: "Bash".to_string(),
            description: "run a command".to_string(),
            details: Some("git push".to_string()),
            is_read_only: false,
            path: Some("git push".to_string()),
            working_dir: Some(std::path::PathBuf::from("/project")),
            allowed_roots: vec![std::path::PathBuf::from("/project")],
            context_description: None,
            network_isolated: false,
            permission_level: PermissionLevel::Execute,
            network_capable: false,
            stateful: false,
        }
    }

    fn run(cmd: &AutopilotCommand, args: &str, ctx: &mut CommandContext) -> CommandResult {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(cmd.execute(args, ctx))
    }

    #[test]
    fn bare_toggle_turns_autopilot_on_and_off() {
        let (mut ctx, state) = ctx_with_state();
        let cmd = AutopilotCommand;

        let on = run(&cmd, "", &mut ctx);
        let CommandResult::Message(msg) = on else {
            panic!("expected Message, got {:?}", on);
        };
        assert!(msg.contains("AUTOPILOT ACTIVE"), "{}", msg);
        assert!(state.lock().is_active("s1"));

        let off = run(&cmd, "", &mut ctx);
        let CommandResult::Message(msg) = off else {
            panic!("expected Message, got {:?}", off);
        };
        assert!(msg.contains("Autopilot off"), "{}", msg);
        assert!(!state.lock().is_active("s1"));
    }

    #[test]
    fn status_reports_posture_and_pending() {
        let (mut ctx, state) = ctx_with_state();
        state.lock().start_autopilot("s1");
        let _ = state.lock().enqueue_tool_call(
            "s1",
            "/project",
            "Bash",
            sample_request(),
            clawde_core::action_risk::ActionRisk::ReviewRequired,
            "needs review".to_string(),
        );
        let cmd = AutopilotCommand;
        let CommandResult::Message(msg) = run(&cmd, "status", &mut ctx) else {
            panic!("expected Message");
        };
        assert!(msg.contains("ACTIVE"), "{}", msg);
        assert!(msg.contains("Pending for this session: 1"), "{}", msg);
    }

    #[test]
    fn list_shows_pending_items_with_ids() {
        let (mut ctx, state) = ctx_with_state();
        state.lock().start_autopilot("s1");
        let _ = state.lock().enqueue_tool_call(
            "s1",
            "/project",
            "Bash",
            sample_request(),
            clawde_core::action_risk::ActionRisk::ReviewRequired,
            "needs review".to_string(),
        );
        let _ = state.lock().enqueue_question(
            "s1",
            "/project",
            "Use B or C?".to_string(),
            Some(vec!["B".to_string(), "C".to_string()]),
        );
        let cmd = AutopilotCommand;
        let CommandResult::Message(msg) = run(&cmd, "list", &mut ctx) else {
            panic!("expected Message");
        };
        assert!(msg.contains("AP-001"), "{}", msg);
        assert!(msg.contains("git push"), "{}", msg);
        assert!(msg.contains("AP-002"), "{}", msg);
        assert!(msg.contains("Use B or C?"), "{}", msg);
    }

    #[test]
    fn reject_marks_item_rejected() {
        let (mut ctx, state) = ctx_with_state();
        state.lock().start_autopilot("s1");
        let _ = state.lock().enqueue_tool_call(
            "s1",
            "/project",
            "Bash",
            sample_request(),
            clawde_core::action_risk::ActionRisk::ReviewRequired,
            "needs review".to_string(),
        );
        let cmd = AutopilotCommand;
        let CommandResult::Message(msg) = run(&cmd, "reject AP-001", &mut ctx) else {
            panic!("expected Message");
        };
        assert!(msg.contains("Rejected AP-001"), "{}", msg);
        assert_eq!(state.lock().items[0].state, DeferredState::Rejected);
    }

    #[test]
    fn approve_marks_tool_call_and_injects_retry_prompt() {
        let (mut ctx, state) = ctx_with_state();
        state.lock().start_autopilot("s1");
        let _ = state.lock().enqueue_tool_call(
            "s1",
            "/project",
            "Bash",
            sample_request(),
            clawde_core::action_risk::ActionRisk::ReviewRequired,
            "needs review".to_string(),
        );
        let cmd = AutopilotCommand;
        let res = run(&cmd, "approve AP-001", &mut ctx);
        let CommandResult::UserMessage(msg) = res else {
            panic!("expected UserMessage, got {:?}", res);
        };
        assert!(msg.contains("AP-001"), "{}", msg);
        assert!(msg.contains("Retry this exact action"), "{}", msg);
        assert_eq!(state.lock().items[0].state, DeferredState::Approved);
    }

    #[test]
    fn approve_rejects_questions_and_unknown_ids() {
        let (mut ctx, state) = ctx_with_state();
        state.lock().start_autopilot("s1");
        let _ = state
            .lock()
            .enqueue_question("s1", "/project", "Use B or C?".to_string(), None);
        let cmd = AutopilotCommand;
        let CommandResult::Error(err) = run(&cmd, "approve AP-001", &mut ctx) else {
            panic!("expected Error");
        };
        assert!(err.contains("not a tool call"), "{}", err);
        let CommandResult::Error(err) = run(&cmd, "approve AP-999", &mut ctx) else {
            panic!("expected Error");
        };
        assert!(err.contains("No item"), "{}", err);
    }

    #[test]
    fn answer_injects_user_message_and_completes_question() {
        let (mut ctx, state) = ctx_with_state();
        state.lock().start_autopilot("s1");
        let _ = state
            .lock()
            .enqueue_question("s1", "/project", "Use B or C?".to_string(), None);
        let cmd = AutopilotCommand;
        let res = run(&cmd, "answer AP-001 Use B", &mut ctx);
        let CommandResult::UserMessage(msg) = res else {
            panic!("expected UserMessage, got {:?}", res);
        };
        assert!(msg.contains("AP-001"), "{}", msg);
        assert!(msg.contains("Use B"), "{}", msg);
        assert_eq!(state.lock().items[0].state, DeferredState::Completed);
    }

    #[test]
    fn answer_rejects_tool_call_items() {
        let (mut ctx, state) = ctx_with_state();
        state.lock().start_autopilot("s1");
        let _ = state.lock().enqueue_tool_call(
            "s1",
            "/project",
            "Bash",
            sample_request(),
            clawde_core::action_risk::ActionRisk::ReviewRequired,
            "needs review".to_string(),
        );
        let cmd = AutopilotCommand;
        let CommandResult::Error(err) = run(&cmd, "answer AP-001 yes", &mut ctx) else {
            panic!("expected Error");
        };
        assert!(err.contains("not a question"), "{}", err);
        assert_eq!(state.lock().items[0].state, DeferredState::Pending);
    }

    #[test]
    fn command_unavailable_without_autonomy_handle() {
        let mut ctx = CommandContext {
            config: clawde_core::config::Config::default(),
            cost_tracker: clawde_core::cost::CostTracker::new(),
            messages: vec![],
            working_dir: std::path::PathBuf::from("/project"),
            session_id: "s1".to_string(),
            session_title: None,
            remote_session_url: None,
            mcp_manager: None,
            mcp_auth_runner: None,
            provider_registry: None,
            test_provider: None,
            effort: None,
            tool_use_tracker: None,
            autonomy: None,
            transient_prev_config: None,
        };
        let cmd = AutopilotCommand;
        let CommandResult::Error(err) = run(&cmd, "", &mut ctx) else {
            panic!("expected Error");
        };
        assert!(err.contains("headless"), "{}", err);
    }
}
