// tool_exec.rs — Shared parallel tool-execution core (refactor-loop-health
// Phase A).
//
// The query loop's two request paths (provider dispatch and explicit
// Anthropic) each used to carry their own Phase 1/2/3 tool executor, so every
// loop-health change was a two-site edit and the sites drifted
// (docs/refactor-loop-health.md §3b). This module is the single shared
// implementation:
//
//   - `prepare_tool_batch` — Phase 1: ToolStart events, write/signature
//     tracking, PreToolUse hooks (config + plugins), malformed-args fallback.
//   - `execute_tool_batch`  — Phase 2/3: parallel dispatch raced against the
//     cancel token, PostToolUse hooks, deterministic check observation, error
//     classification, ToolEnd events, result blocks, repeat-tool reminders.
//   - `TurnToolState`       — per-turn accumulator consumed by the no-progress
//     detector and plan evidence, replacing the scattered loop locals.
//
// Deliberate unifications pinned by the drift audit:
//   - PostToolUse hooks are skipped when the batch was cancelled (they run
//     external commands and would defeat the point of returning promptly).
//   - Hook/plugin blocks carry `PermissionDenied` (B1) so the no-progress
//     detector treats them as recoverable, not unclassified hard-fatal.
//   - ToolResult blocks use the canonical wire form: `is_error` omitted on
//     success.

use serde_json::Value;
use tokio::sync::mpsc;
use tracing::warn;

use clawde_tools::{Tool, ToolContext, ToolErrorCode, ToolResult};

use crate::repeat_guard::RepeatCallDetector;
use crate::runner::tools::{execute_tool_for_task, run_tool_batch};
use crate::{deterministic_check_observation, is_write_tool, QueryEvent};

/// Per-turn tool-health accumulator, replacing the scattered loop locals
/// (refactor-loop-health Phase A / W2). Lives across the turn; `clear_turn`
/// is called when a continuation starts a fresh verification scope.
#[derive(Debug, Default)]
pub(crate) struct TurnToolState {
    /// Signatures of every tool call prepared this logical turn, in order.
    pub(crate) signatures: Vec<String>,
    /// Total error results observed this turn.
    pub(crate) error_count: u32,
    /// Errors whose code is fatal (`!is_recoverable()` or unclassified).
    pub(crate) fatal_error_count: u32,
    /// Fatal errors that are NOT deterministic check failures (RunTests/
    /// RunLints) — the "uncorrectable" class for the stop message.
    pub(crate) hard_fatal_error_count: u32,
    /// Whether a deterministic check tool ran this turn.
    pub(crate) check_run: bool,
    /// Whether a deterministic check tool failed or timed out this turn.
    pub(crate) check_failed: bool,
}

impl TurnToolState {
    /// Classify one tool result into the turn counters. `check_failed`
    /// (RunTests/RunLints failing or timing out) is excluded from the
    /// hard-fatal bucket because test failures are fixable by writing code —
    /// the write would reset the no-progress streak, so the stop message must
    /// not call them "uncorrectable".
    pub(crate) fn observe(&mut self, name: &str, result: &ToolResult) {
        let (check_run, check_failed) = deterministic_check_observation(name, result);
        self.check_run |= check_run;
        self.check_failed |= check_failed;
        if result.is_error {
            self.error_count += 1;
            if result.error_code.is_none_or(|code| !code.is_recoverable()) {
                self.fatal_error_count += 1;
                if !check_failed {
                    self.hard_fatal_error_count += 1;
                }
            }
        }
    }

    /// Reset per-turn accumulation (continuation boundary / new turn).
    pub(crate) fn clear_turn(&mut self) {
        self.signatures.clear();
        self.error_count = 0;
        self.fatal_error_count = 0;
        self.hard_fatal_error_count = 0;
        self.check_run = false;
        self.check_failed = false;
    }
}

/// A tool call prepared for execution. `blocked_result` is `Some` when a
/// PreToolUse hook, a plugin veto, or the malformed-args fallback prevented
/// execution — the result is synthesized without ever running the tool.
pub(crate) struct PreparedTool {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) input: Value,
    pub(crate) blocked_result: Option<ToolResult>,
}

/// Immutable execution dependencies bundled so `execute_tool_batch` keeps a
/// readable signature (refactor-loop-health R1).
pub(crate) struct ToolExecCtx<'a> {
    pub(crate) tools: &'a [Box<dyn Tool>],
    pub(crate) tool_ctx: &'a ToolContext,
    pub(crate) active_task_id: Option<&'a str>,
    pub(crate) cancel_token: &'a tokio_util::sync::CancellationToken,
    pub(crate) event_tx: Option<&'a mpsc::UnboundedSender<QueryEvent>>,
}

/// Phase 1: sequential pre-hook pass over the turn's tool calls.
///
/// For each call: emit `ToolStart`, mark writes, record the no-progress
/// signature, run PreToolUse hooks (config then plugins — a veto blocks
/// execution), and fall back to the malformed-args error for calls whose
/// streamed JSON failed to parse (`malformed` is empty for stream handlers
/// that do not detect them, e.g. the Anthropic path — D6). Also advances the
/// goal re-anchor counter so the milestone moves on both request paths.
pub(crate) async fn prepare_tool_batch(
    tool_calls: &[(String, String, Value)],
    tool_ctx: &ToolContext,
    event_tx: Option<&mpsc::UnboundedSender<QueryEvent>>,
    malformed: &std::collections::HashSet<String>,
    state: &mut TurnToolState,
    wrote_files: &mut bool,
    total_tool_calls: &mut u32,
) -> Vec<PreparedTool> {
    let mut prepared = Vec::with_capacity(tool_calls.len());
    for (tool_id, tool_name, tool_input) in tool_calls {
        if let Some(tx) = event_tx {
            let _ = tx.send(QueryEvent::ToolStart {
                tool_name: tool_name.clone(),
                tool_id: tool_id.clone(),
                input_json: tool_input.to_string(),
            });
        }
        *wrote_files |= is_write_tool(tool_name);
        state.signatures.push(format!(
            "{}:{}",
            tool_name,
            serde_json::to_string(tool_input).unwrap_or_default()
        ));

        // PreToolUse hooks (config + plugins). A hook veto takes priority over
        // the malformed-args fallback: a blocked call is never executed.
        let hooks = &tool_ctx.config.hooks;
        let hook_ctx = clawde_core::hooks::HookContext {
            event: "PreToolUse".to_string(),
            tool_name: Some(tool_name.clone()),
            tool_input: Some(tool_input.clone()),
            tool_output: None,
            is_error: None,
            session_id: Some(tool_ctx.session_id.clone()),
            upstream_id: None,
            model: None,
            elapsed_ms: None,
            cost_usd: None,
            fallback_used: None,
            retries: None,
        };
        let pre_outcome = clawde_core::hooks::run_hooks(
            hooks,
            clawde_core::config::HookEvent::PreToolUse,
            &hook_ctx,
            &tool_ctx.working_dir,
        )
        .await;

        let plugin_pre_outcome = clawde_plugins::run_global_pre_tool_hook(tool_name, tool_input);

        // B1: hook/plugin blocks carry `PermissionDenied` so the no-progress
        // detector classifies them as recoverable (real signature, headroom to
        // iterate) instead of an unclassified hard-fatal "uncorrectable" error.
        let blocked_result = if let clawde_core::hooks::HookOutcome::Blocked(reason) = pre_outcome {
            warn!(tool = %tool_name, reason = %reason, "PreToolUse hook blocked execution");
            Some(ToolResult::error_with_code(
                ToolErrorCode::PermissionDenied,
                format!("Blocked by hook: {}", reason),
            ))
        } else if let clawde_plugins::HookOutcome::Deny(reason) = plugin_pre_outcome {
            warn!(tool = %tool_name, reason = %reason, "Plugin PreToolUse hook blocked execution");
            Some(ToolResult::error_with_code(
                ToolErrorCode::PermissionDenied,
                format!("Blocked by plugin hook: {}", reason),
            ))
        } else if malformed.contains(tool_id) {
            Some(ToolResult::error_with_code(
                ToolErrorCode::InvalidInput,
                format!(
                    "Tool call '{}' was not executed: its arguments were malformed or truncated JSON. {}",
                    tool_name,
                    ToolErrorCode::InvalidInput.recovery_hint()
                ),
            ))
        } else {
            None
        };
        prepared.push(PreparedTool {
            id: tool_id.clone(),
            name: tool_name.clone(),
            input: tool_input.clone(),
            blocked_result,
        });
    }
    // Goal re-anchoring counts every prepared call (blocked included), so the
    // milestone advances identically on both request paths.
    *total_tool_calls += prepared.len() as u32;
    prepared
}

/// Phase 2/3: dispatch the prepared batch and post-process the results.
///
/// Blocked tools yield a ready future with their pre-computed result; the
/// rest execute concurrently via `run_tool_batch`, raced against the cancel
/// token (issue #218) so in-flight tools are abandoned promptly on
/// cancellation and a cancelled ToolResult is synthesized for EVERY tool —
/// each tool_use still gets a matching tool_result and the history stays
/// well-formed. Returns `(result_blocks, batch_cancelled)`; the caller owns
/// the cancelled-return / message-push control flow.
pub(crate) async fn execute_tool_batch(
    ctx: &ToolExecCtx<'_>,
    prepared: &[PreparedTool],
    state: &mut TurnToolState,
    repeat_detector: &mut RepeatCallDetector,
) -> (Vec<clawde_core::types::ContentBlock>, bool) {
    let exec_futures: Vec<_> = prepared
        .iter()
        .map(|p| {
            let task_id = ctx.active_task_id;
            if let Some(ref r) = p.blocked_result {
                let r = r.clone();
                futures::future::Either::Left(async move { r })
            } else {
                let name = p.name.clone();
                let input = p.input.clone();
                futures::future::Either::Right(async move {
                    execute_tool_for_task(&name, &input, ctx.tools, ctx.tool_ctx, task_id).await
                })
            }
        })
        .collect();

    let (exec_results, batch_cancelled) = run_tool_batch(exec_futures, ctx.cancel_token).await;

    // Phase 3: post-hooks, event emission, and result block assembly. When the
    // batch was cancelled we skip the awaiting PostToolUse hooks (they run
    // external commands and would defeat the point of returning promptly) but
    // still emit ToolEnd + build every result block so the conversation and
    // TUI stay consistent.
    let mut result_blocks: Vec<clawde_core::types::ContentBlock> =
        Vec::with_capacity(prepared.len());
    for (p, result) in prepared.iter().zip(exec_results) {
        state.observe(&p.name, &result);

        if !batch_cancelled {
            let hooks = &ctx.tool_ctx.config.hooks;
            let post_ctx = clawde_core::hooks::HookContext {
                event: "PostToolUse".to_string(),
                tool_name: Some(p.name.clone()),
                tool_input: Some(p.input.clone()),
                tool_output: Some(result.content.clone()),
                is_error: Some(result.is_error),
                session_id: Some(ctx.tool_ctx.session_id.clone()),
                upstream_id: None,
                model: None,
                elapsed_ms: None,
                cost_usd: None,
                fallback_used: None,
                retries: None,
            };
            clawde_core::hooks::run_hooks(
                hooks,
                clawde_core::config::HookEvent::PostToolUse,
                &post_ctx,
                &ctx.tool_ctx.working_dir,
            )
            .await;

            clawde_plugins::run_global_post_tool_hook(
                &p.name,
                &p.input,
                &result.content,
                result.is_error,
            );
        }

        if let Some(tx) = ctx.event_tx {
            let _ = tx.send(QueryEvent::ToolEnd {
                tool_name: p.name.clone(),
                tool_id: p.id.clone(),
                result: result.content.clone(),
                is_error: result.is_error,
                error_code: result.error_code.map(|code| code.as_str().to_string()),
            });
        }

        result_blocks.push(clawde_core::types::ContentBlock::ToolResult {
            tool_use_id: p.id.clone(),
            content: clawde_core::types::ToolResultContent::Text(result.content),
            // Canonical wire form: omit is_error on success.
            is_error: if result.is_error { Some(true) } else { None },
        });
        // Repeat-tool-reminder: detect consecutive identical tool calls and
        // inject a warning to break loops.
        if let Some(reminder) = repeat_detector.observe(&p.name, &p.input) {
            result_blocks.push(clawde_core::types::ContentBlock::ToolResult {
                tool_use_id: p.id.clone(),
                content: clawde_core::types::ToolResultContent::Text(format!(
                    "[SYSTEM NOTICE: {}]",
                    reminder
                )),
                is_error: Some(false),
            });
        }
    }

    (result_blocks, batch_cancelled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawde_core::types::ContentBlock;
    use std::sync::Arc;

    /// Minimal permission handler that allows everything (non-interactive
    /// headless tests never surface prompts).
    struct AllowHandler;
    impl clawde_core::permissions::PermissionHandler for AllowHandler {
        fn check_permission(
            &self,
            _request: &clawde_core::permissions::PermissionRequest,
        ) -> clawde_core::permissions::PermissionDecision {
            clawde_core::permissions::PermissionDecision::Allow
        }
        fn request_permission(
            &self,
            _request: &clawde_core::permissions::PermissionRequest,
        ) -> clawde_core::permissions::PermissionDecision {
            clawde_core::permissions::PermissionDecision::Allow
        }
    }

    fn test_ctx() -> ToolContext {
        ToolContext {
            working_dir: std::path::PathBuf::from("/workspace"),
            permission_mode: clawde_core::config::PermissionMode::Default,
            permission_handler: Arc::new(AllowHandler),
            cost_tracker: clawde_core::cost::CostTracker::new(),
            session_id: "tool-exec-test".to_string(),
            file_history: Arc::new(parking_lot::Mutex::new(
                clawde_core::file_history::FileHistory::new(),
            )),
            current_turn: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            non_interactive: true,
            mcp_manager: None,
            config: clawde_core::config::Config::default(),
            provider_registry: None,
            managed_agent_config: None,
            effort: None,
            completion_notifier: None,
            pending_permissions: None,
            permission_manager: None,
            user_question_tx: None,
            cancel_token: tokio_util::sync::CancellationToken::new(),
        }
    }

    /// Minimal tool returning a scripted result.
    struct FakeTool {
        name: &'static str,
        result: ToolResult,
    }

    #[async_trait::async_trait]
    impl Tool for FakeTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "fake tool"
        }
        fn permission_level(&self) -> clawde_tools::PermissionLevel {
            clawde_tools::PermissionLevel::ReadOnly
        }
        fn self_gates(&self) -> bool {
            false
        }
        fn stateful(&self) -> bool {
            false
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _input: Value, _ctx: &ToolContext) -> ToolResult {
            self.result.clone()
        }
    }

    fn prepared(id: &str, name: &str, blocked: Option<ToolResult>) -> PreparedTool {
        PreparedTool {
            id: id.to_string(),
            name: name.to_string(),
            input: serde_json::json!({"arg": 1}),
            blocked_result: blocked,
        }
    }

    fn exec_ctx<'a>(
        tools: &'a [Box<dyn Tool>],
        tool_ctx: &'a ToolContext,
        tx: &'a mpsc::UnboundedSender<QueryEvent>,
    ) -> ToolExecCtx<'a> {
        ToolExecCtx {
            tools,
            tool_ctx,
            active_task_id: None,
            cancel_token: &tool_ctx.cancel_token,
            event_tx: Some(tx),
        }
    }

    #[test]
    fn observe_classifies_recoverable_fatal_and_check_failures() {
        // Recoverable (InvalidInput): error + fatal counters stay 0.
        let mut state = TurnToolState::default();
        state.observe(
            "Bash",
            &ToolResult::error_with_code(ToolErrorCode::InvalidInput, "bad args"),
        );
        assert_eq!(state.error_count, 1);
        assert_eq!(state.fatal_error_count, 0, "recoverable must not be fatal");
        assert_eq!(state.hard_fatal_error_count, 0);

        // Unclassified error (None code): conservative fatal + hard fatal.
        state.observe("Bash", &ToolResult::error("spawn failed"));
        assert_eq!(state.error_count, 2);
        assert_eq!(state.fatal_error_count, 1);
        assert_eq!(state.hard_fatal_error_count, 1);

        // Check failure (RunTests + TestFailed): fatal, but NOT hard fatal —
        // the stop message must say "ran checks", not "uncorrectable errors".
        state.observe(
            "RunTests",
            &ToolResult::error_with_code(ToolErrorCode::TestFailed, "tests failed: 2/14"),
        );
        assert_eq!(state.error_count, 3);
        assert_eq!(state.fatal_error_count, 2);
        assert_eq!(
            state.hard_fatal_error_count, 1,
            "check failures are fixable by writing"
        );
        assert!(state.check_run);
        assert!(state.check_failed);

        // A passing check clears the failed flag for the turn.
        let mut state2 = TurnToolState::default();
        state2.observe("RunTests", &ToolResult::success("tests passed: 14/14"));
        assert!(state2.check_run);
        assert!(!state2.check_failed);
    }

    #[tokio::test]
    async fn execute_tool_batch_builds_blocks_and_counts_errors() {
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(FakeTool {
            name: "ok_tool",
            result: ToolResult::success("ran"),
        })];
        let tool_ctx = test_ctx();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let exec_ctx = exec_ctx(&tools, &tool_ctx, &tx);
        let mut state = TurnToolState::default();
        let mut detector = RepeatCallDetector::new();

        let (blocks, cancelled) = execute_tool_batch(
            &exec_ctx,
            &[prepared("t1", "ok_tool", None)],
            &mut state,
            &mut detector,
        )
        .await;

        assert!(!cancelled);
        assert_eq!(blocks.len(), 1);
        let ContentBlock::ToolResult {
            tool_use_id,
            is_error,
            ..
        } = &blocks[0]
        else {
            panic!("expected a ToolResult block");
        };
        assert_eq!(tool_use_id, "t1");
        assert_eq!(
            *is_error, None,
            "success must omit is_error (canonical form)"
        );
        assert_eq!(state.error_count, 0);
        assert!(!state.check_run);
        // ToolEnd event emitted for the executed tool.
        let mut saw_tool_end = false;
        while let Ok(evt) = rx.try_recv() {
            if matches!(evt, QueryEvent::ToolEnd { .. }) {
                saw_tool_end = true;
            }
        }
        assert!(saw_tool_end, "ToolEnd event missing");
    }

    #[tokio::test]
    async fn execute_tool_batch_blocked_tool_never_runs() {
        // A blocked tool (PreToolUse veto) yields its synthesized result
        // without touching the tool set — pass NO tools to prove no execution.
        let tools: Vec<Box<dyn Tool>> = Vec::new();
        let tool_ctx = test_ctx();
        let (tx, _rx) = mpsc::unbounded_channel();
        let exec_ctx = exec_ctx(&tools, &tool_ctx, &tx);
        let mut state = TurnToolState::default();
        let mut detector = RepeatCallDetector::new();

        let (blocks, cancelled) = execute_tool_batch(
            &exec_ctx,
            &[prepared(
                "t1",
                "ok_tool",
                Some(ToolResult::error_with_code(
                    ToolErrorCode::PermissionDenied,
                    "Blocked by hook: nope",
                )),
            )],
            &mut state,
            &mut detector,
        )
        .await;

        assert!(!cancelled);
        assert_eq!(blocks.len(), 1);
        let ContentBlock::ToolResult { is_error, .. } = &blocks[0] else {
            panic!("expected a ToolResult block");
        };
        assert_eq!(*is_error, Some(true), "blocked result must be flagged");
        // B1: PermissionDenied is recoverable — not a hard-fatal "uncorrectable".
        assert_eq!(state.error_count, 1);
        assert_eq!(state.fatal_error_count, 0);
        assert_eq!(state.hard_fatal_error_count, 0);
    }

    #[test]
    fn clear_turn_resets_accumulation() {
        let mut state = TurnToolState::default();
        state.observe(
            "RunTests",
            &ToolResult::error_with_code(ToolErrorCode::TestFailed, "failed"),
        );
        state.signatures.push("RunTests:{}".to_string());
        assert!(state.check_run);
        state.clear_turn();
        let empty = TurnToolState::default();
        assert_eq!(state.signatures, empty.signatures);
        assert_eq!(state.error_count, empty.error_count);
        assert_eq!(state.fatal_error_count, empty.fatal_error_count);
        assert_eq!(state.hard_fatal_error_count, empty.hard_fatal_error_count);
        assert!(!state.check_run);
        assert!(!state.check_failed);
    }
}
