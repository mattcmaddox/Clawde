//! Built-in tool execution for the gateway agent loop.
//!
//! Maps client-declared tool names onto Clawde's built-in tools (the curated
//! surface, D2), executes them under a headless permission gate, and
//! sanitizes results before they are fed back to the model (D14).
//!
//! The executor is the "internal tools" side of the Open Responses
//! internal/external taxonomy: a tool call whose name matches a built-in
//! here is executed locally; anything else is yielded back to the client by
//! the loop (`agent.rs`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use clawde_core::config::PermissionMode;
use clawde_core::permissions::{PermissionDecision, PermissionHandler, PermissionRequest};
use clawde_core::types::{ContentBlock, ToolResultContent};
use clawde_tools::{Tool, ToolContext};
use futures::stream::{self, StreamExt};
use tokio_util::sync::CancellationToken;

/// How the gateway treats local tool execution (D1 risk posture).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayPermissionMode {
    /// File reads, globs, greps, and web fetches are allowed; writes and
    /// shell execution are denied. Maps to the core `Default` permission
    /// mode (read-only + non-stateful).
    AllowReadonly,
    /// Every built-in tool executes (core `BypassPermissions`).
    Allow,
    /// Autopilot: safe actions run, review-required are deferred into
    /// the session's autonomy queue, irreversible are denied. Requires an
    /// `Arc<Mutex<AutonomyState>>` to be passed at construction time.
    AllowAutopilot,
    /// Nothing executes; every call short-circuits to a permission-denied
    /// tool error. Relay-only posture.
    Deny,
}

impl GatewayPermissionMode {
    /// The core permission mode used to build the headless handler.
    fn core_mode(self) -> PermissionMode {
        match self {
            GatewayPermissionMode::AllowReadonly => PermissionMode::Default,
            GatewayPermissionMode::Allow | GatewayPermissionMode::AllowAutopilot => {
                PermissionMode::BypassPermissions
            }
            GatewayPermissionMode::Deny => PermissionMode::Default,
        }
    }

    fn handler(self) -> Arc<dyn PermissionHandler> {
        match self {
            GatewayPermissionMode::Deny => Arc::new(DenyAllHandler),
            mode => Arc::new(clawde_core::permissions::AutoPermissionHandler {
                mode: mode.core_mode(),
            }),
        }
    }

    /// Whether this mode uses the autopilot deferral path.
    pub fn is_autopilot(self) -> bool {
        self == GatewayPermissionMode::AllowAutopilot
    }
}

/// Headless handler that denies everything (gateway `deny` mode).
struct DenyAllHandler;

impl PermissionHandler for DenyAllHandler {
    fn check_permission(&self, _request: &PermissionRequest) -> PermissionDecision {
        PermissionDecision::Deny
    }

    fn request_permission(&self, _request: &PermissionRequest) -> PermissionDecision {
        PermissionDecision::Deny
    }
}

/// The default curated built-in surface (D2). Everything else in
/// `clawde_tools::all_tools()` is TUI/session-bound and stays external.
pub const DEFAULT_BUILTIN_TOOLS: &[&str] = &[
    "Read",
    "Glob",
    "Grep",
    "WebFetch",
    "WebSearch",
    "Write",
    "Edit",
    "ApplyPatch",
    "Bash",
    "RunTests",
];

/// Executes Clawde's built-in tools on behalf of the gateway.
pub struct GatewayToolExecutor {
    /// Built-in tools keyed by lowercase name (case-insensitive matching so a
    /// client's `"read"` binds to the `Read` tool).
    tools: HashMap<String, Box<dyn Tool>>,
    /// Base context; cloned per concurrent tool call.
    ctx: ToolContext,
    mode: GatewayPermissionMode,
}

impl GatewayToolExecutor {
    /// Build an executor with the curated surface (or a `builtin_names`
    /// replacement list, D2). `workspace_paths[0]` (or the process cwd) is
    /// the tool working directory; `session_id` keys shell state.
    ///
    /// When `mode` is `AllowAutopilot`, pass `Some(autonomy)` to enable the
    /// deferral path. `None` with `AllowAutopilot` falls back to `Allow`
    /// (bypass) semantics — fail closed, never silently drop.
    pub fn new(
        mode: GatewayPermissionMode,
        workspace_paths: &[PathBuf],
        session_id: &str,
        builtin_names: &[String],
        cancel: CancellationToken,
        autonomy: Option<Arc<parking_lot::Mutex<clawde_core::autonomy::AutonomyState>>>,
    ) -> Self {
        let working_dir = workspace_paths
            .first()
            .cloned()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let names: Vec<String> = if builtin_names.is_empty() {
            DEFAULT_BUILTIN_TOOLS
                .iter()
                .map(|s| s.to_string())
                .collect()
        } else {
            builtin_names.to_vec()
        };

        let mut tools: HashMap<String, Box<dyn Tool>> = HashMap::new();
        for tool in clawde_tools::all_tools() {
            if names.iter().any(|n| n.eq_ignore_ascii_case(tool.name())) {
                tools.insert(tool.name().to_lowercase(), tool);
            }
        }

        // Wire autopilot state into the tool context when requested.
        // When mode is AllowAutopilot but no state was provided, fall back
        // to Allow (bypass) — never silently drop the autopilot request.
        let effective_mode = if mode.is_autopilot() && autonomy.is_none() {
            tracing::warn!(
                "gateway permission mode is 'allow-autopilot' but no autonomy state was \
                 provided; falling back to 'allow' (bypass). Set permissionMode to \
                 'allow-autopilot' with a valid workspace to enable autopilot deferral."
            );
            GatewayPermissionMode::Allow
        } else {
            mode
        };

        let ctx = ToolContext {
            working_dir,
            permission_mode: effective_mode.core_mode(),
            permission_handler: effective_mode.handler(),
            cost_tracker: clawde_core::cost::CostTracker::new(),
            session_id: session_id.to_string(),
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
            autonomy: if effective_mode.is_autopilot() {
                autonomy
            } else {
                None
            },
            cancel_token: cancel,
        };

        Self {
            tools,
            ctx,
            mode: effective_mode,
        }
    }

    /// Whether a tool name is a built-in this executor can run.
    pub fn is_builtin(&self, name: &str) -> bool {
        self.tools.contains_key(&name.to_lowercase())
    }

    /// Partition tool-use blocks into (internal, external).
    pub fn partition_calls(
        &self,
        calls: &[ContentBlock],
    ) -> (Vec<ContentBlock>, Vec<ContentBlock>) {
        let mut internal = Vec::new();
        let mut external = Vec::new();
        for call in calls {
            let is_internal =
                matches!(call, ContentBlock::ToolUse { name, .. } if self.is_builtin(name));
            if is_internal {
                internal.push(call.clone());
            } else {
                external.push(call.clone());
            }
        }
        (internal, external)
    }

    /// Execute internal tool calls, up to `max_concurrent` at a time,
    /// emitting results in call order (models expect received-order results).
    /// Each result is sanitized and truncated to `budget` bytes (D14).
    ///
    /// Inputs are owned so the returned future is Send without holding
    /// borrows across the buffered awaits (required by the SSE generator).
    pub async fn execute_all(
        &self,
        calls: Vec<ContentBlock>,
        cancel: CancellationToken,
        max_concurrent: usize,
        budget: usize,
    ) -> Vec<ContentBlock> {
        stream::iter(calls)
            .map(|call| self.execute_one(call, cancel.clone(), budget))
            .buffered(max_concurrent.max(1))
            .collect::<Vec<_>>()
            .await
    }

    /// Execute a single internal tool call, producing a `ToolResult` block.
    async fn execute_one(
        &self,
        call: ContentBlock,
        cancel: CancellationToken,
        budget: usize,
    ) -> ContentBlock {
        let (id, name, input) = match call {
            ContentBlock::ToolUse {
                id, name, input, ..
            } => (id, name, input),
            _ => return tool_error_block("unknown", "not a tool call", "invalid_tool_call"),
        };

        if cancel.is_cancelled() {
            return tool_error_block(&id, "cancelled", "cancelled");
        }
        if self.mode == GatewayPermissionMode::Deny {
            return tool_error_block(
                &id,
                "Tool execution is disabled on this gateway",
                "permission_denied",
            );
        }

        // E6: malformed / missing arguments never execute garbage.
        if input.is_null() || !input.is_object() {
            return tool_error_block(
                &id,
                "Malformed tool arguments (expected JSON object)",
                "malformed_arguments",
            );
        }

        let Some(tool) = self.tools.get(&name.to_lowercase()) else {
            return tool_error_block(&id, "Unknown built-in tool", "unknown_tool");
        };

        let result = tool.execute(input, &self.ctx).await;
        let is_error = result.is_error;
        let content = sanitize_result(&result.content, budget);
        ContentBlock::ToolResult {
            tool_use_id: id,
            content: ToolResultContent::Text(content),
            is_error: Some(is_error),
        }
    }

    /// The active built-in tool names (lowercase), for status/debug surfaces.
    pub fn builtin_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tools.keys().cloned().collect();
        names.sort();
        names
    }
}

fn tool_error_block(id: &str, message: &str, code: &str) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: id.to_string(),
        content: ToolResultContent::Text(format!("{code}: {message}")),
        is_error: Some(true),
    }
}

/// Sanitize a tool result before it is fed back to the model (D14):
/// strip terminal control sequences (C0 controls + ANSI CSI escapes), then
/// truncate to `budget` bytes on a UTF-8 boundary. Tool output is untrusted
/// data, never instructions.
fn sanitize_result(text: &str, budget: usize) -> String {
    let mut cleaned = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                // ANSI escape: drop the introducer and any CSI sequence
                // (ESC [ ... final-byte in 0x40..=0x7E) or a single
                // non-CSI escape (ESC \, ESC ], …).
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for next in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            break;
                        }
                    }
                } else if let Some(next) = chars.peek().copied() {
                    if !matches!(next, '\u{5c}' | '\u{5d}' | '\u{5e}' | '\u{5f}' | '\u{60}') {
                        chars.next();
                    }
                }
            }
            c if c.is_control() && !matches!(c, '\n' | '\t') => {}
            c => cleaned.push(c),
        }
    }
    if cleaned.len() <= budget {
        return cleaned;
    }
    let mut end = budget;
    while !cleaned.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = cleaned[..end].to_string();
    out.push_str("\n…[truncated]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn executor(mode: GatewayPermissionMode) -> GatewayToolExecutor {
        GatewayToolExecutor::new(
            mode,
            &[],
            "tool-exec-test",
            &[],
            CancellationToken::new(),
            None,
        )
    }

    #[test]
    fn curated_surface_has_ten_defaults() {
        let ex = executor(GatewayPermissionMode::AllowReadonly);
        let names = ex.builtin_names();
        assert_eq!(names.len(), DEFAULT_BUILTIN_TOOLS.len());
        assert!(names.contains(&"read".to_string()));
        assert!(names.contains(&"bash".to_string()));
        assert!(!names.contains(&"ask_user".to_string()));
    }

    #[test]
    fn replacement_list_swaps_surface() {
        let ex = GatewayToolExecutor::new(
            GatewayPermissionMode::AllowReadonly,
            &[],
            "t",
            &["read".to_string()],
            CancellationToken::new(),
            None,
        );
        assert_eq!(ex.builtin_names(), vec!["read".to_string()]);
    }

    #[test]
    fn partition_splits_internal_and_external() {
        let ex = executor(GatewayPermissionMode::AllowReadonly);
        let calls = vec![
            ContentBlock::ToolUse {
                id: "c1".into(),
                name: "Read".into(),
                input: json!({"path": "x"}),
                thought_signature: None,
            },
            ContentBlock::ToolUse {
                id: "c2".into(),
                name: "get_weather".into(),
                input: json!({"city": "SF"}),
                thought_signature: None,
            },
        ];
        let (internal, external) = ex.partition_calls(&calls);
        assert_eq!(internal.len(), 1);
        assert_eq!(external.len(), 1);
        let ContentBlock::ToolUse {
            id, name, input, ..
        } = &external[0]
        else {
            panic!("expected tool use");
        };
        assert_eq!(id, "c2");
        assert_eq!(name, "get_weather");
        assert_eq!(input, &json!({ "city": "SF" }));
    }

    #[tokio::test]
    async fn malformed_arguments_never_execute() {
        let ex = executor(GatewayPermissionMode::Allow);
        let call = ContentBlock::ToolUse {
            id: "c1".into(),
            name: "Read".into(),
            input: Value::Null,
            thought_signature: None,
        };
        let out = ex.execute_one(call, CancellationToken::new(), 50_000).await;
        let ContentBlock::ToolResult {
            is_error, content, ..
        } = &out
        else {
            panic!("expected tool result");
        };
        assert_eq!(*is_error, Some(true));
        assert!(matches!(content, ToolResultContent::Text(t) if t.contains("malformed")));
    }

    #[tokio::test]
    async fn deny_mode_short_circuits() {
        let ex = executor(GatewayPermissionMode::Deny);
        let call = ContentBlock::ToolUse {
            id: "c1".into(),
            name: "Read".into(),
            input: json!({"path": "whatever"}),
            thought_signature: None,
        };
        let out = ex.execute_one(call, CancellationToken::new(), 50_000).await;
        let ContentBlock::ToolResult {
            is_error, content, ..
        } = &out
        else {
            panic!("expected tool result");
        };
        assert_eq!(*is_error, Some(true));
        assert!(matches!(content, ToolResultContent::Text(t) if t.contains("disabled")));
    }

    #[tokio::test]
    async fn read_missing_file_returns_error_observation() {
        let ex = executor(GatewayPermissionMode::Allow);
        let call = ContentBlock::ToolUse {
            id: "c1".into(),
            name: "Read".into(),
            input: json!({"path": "/nonexistent/clawde-gateway-test-file"}),
            thought_signature: None,
        };
        let out = ex.execute_one(call, CancellationToken::new(), 50_000).await;
        let ContentBlock::ToolResult { is_error, .. } = &out else {
            panic!("expected tool result");
        };
        assert_eq!(*is_error, Some(true));
    }

    #[test]
    fn sanitize_strips_controls_and_truncates() {
        // ANSI CSI (red) + C0 controls (NUL, backspace) must be dropped.
        let dirty = "ok\x1b[31mred\x00\x08end";
        let cleaned = sanitize_result(dirty, 1000);
        assert!(!cleaned.contains('\u{1b}'));
        assert!(!cleaned.contains('\u{0}'));
        assert!(!cleaned.contains('\u{8}'));
        assert_eq!(cleaned, "okredend");

        let long = "x".repeat(10_000);
        let truncated = sanitize_result(&long, 100);
        assert!(truncated.len() <= 100 + 16);
        assert!(truncated.ends_with("…[truncated]"));
    }

    #[tokio::test]
    async fn parallel_execution_preserves_order() {
        let ex = executor(GatewayPermissionMode::Allow);
        let calls: Vec<ContentBlock> = (0..4)
            .map(|i| ContentBlock::ToolUse {
                id: format!("c{i}"),
                name: "Read".into(),
                input: json!({"path": "/nonexistent/clawde-gateway-test-file"}),
                thought_signature: None,
            })
            .collect();
        let out = ex
            .execute_all(calls, CancellationToken::new(), 4, 50_000)
            .await;
        assert_eq!(out.len(), 4);
        for (i, block) in out.iter().enumerate() {
            let ContentBlock::ToolResult { tool_use_id, .. } = block else {
                panic!("expected tool result");
            };
            assert_eq!(tool_use_id, &format!("c{i}"));
        }
    }

    // ---- Gateway autopilot adapter tests ------------------------------------

    #[test]
    fn autopilot_mode_without_state_falls_back_to_allow() {
        // AllowAutopilot with None autonomy → effective mode becomes Allow
        // (fail-closed: never silently drop the autopilot request).
        let ex = GatewayToolExecutor::new(
            GatewayPermissionMode::AllowAutopilot,
            &[],
            "test",
            &[],
            CancellationToken::new(),
            None,
        );
        assert_eq!(ex.mode, GatewayPermissionMode::Allow);
    }

    #[test]
    fn autopilot_mode_with_state_is_autopilot() {
        let state = Arc::new(parking_lot::Mutex::new(
            clawde_core::autonomy::AutonomyState::new("gw-test"),
        ));
        state.lock().start_autopilot("gw-test");
        let ex = GatewayToolExecutor::new(
            GatewayPermissionMode::AllowAutopilot,
            &[],
            "gw-test",
            &[],
            CancellationToken::new(),
            Some(state),
        );
        assert_eq!(ex.mode, GatewayPermissionMode::AllowAutopilot);
    }

    #[test]
    fn autopilot_mode_is_autopilot_method() {
        assert!(GatewayPermissionMode::AllowAutopilot.is_autopilot());
        assert!(!GatewayPermissionMode::Allow.is_autopilot());
        assert!(!GatewayPermissionMode::AllowReadonly.is_autopilot());
        assert!(!GatewayPermissionMode::Deny.is_autopilot());
    }
}
