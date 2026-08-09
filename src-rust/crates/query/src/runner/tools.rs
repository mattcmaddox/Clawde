// Tool execution helpers: argument parsing, permission gating, and the
// single-tool / batch execution paths. Extracted from lib.rs (issue #232).
// Behavior-preserving move.

use crate::*;

/// Parse the accumulated JSON arguments of a streamed tool call.
///
/// Providers stream a tool call's arguments as a sequence of partial-JSON
/// deltas which we concatenate into a single buffer. A well-behaved
/// no-argument call yields an empty (or whitespace-only) buffer, which we
/// map to an empty object. Any *non-empty* buffer that fails to parse is
/// returned as an error rather than being silently replaced with `{}` — a
/// truncated stream must never cause a tool (e.g. Edit/Write) to run with
/// empty arguments (issue #215).
pub(crate) fn parse_tool_args(json_str: &str) -> Result<Value, serde_json::Error> {
    let trimmed = json_str.trim();
    if trimmed.is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(trimmed)
}

/// Whether a `PermissionLevel` must be gated by the central backstop.
///
/// Only `None` and `ReadOnly` are exempt; every other level (`Write`,
/// `Execute`, `Dangerous`, `Forbidden`) represents a side-effecting action that
/// the backstop must confirm before it runs.
pub(crate) fn permission_level_is_gated(level: PermissionLevel) -> bool {
    !matches!(level, PermissionLevel::None | PermissionLevel::ReadOnly)
}

/// Synthesize a human-readable permission description for a tool that does not
/// gate itself, surfacing the tool name and a truncated preview of its input so
/// the user can see what is about to run.
pub(crate) fn synthesize_permission_description(name: &str, input: &Value) -> String {
    let rendered = serde_json::to_string(input).unwrap_or_default();
    let preview: String = rendered.chars().take(200).collect();
    if preview.is_empty() || preview == "{}" || preview == "null" {
        format!("Run tool '{}'", name)
    } else {
        format!("Run tool '{}' with input: {}", name, preview)
    }
}

/// Execute a single tool invocation.
pub(crate) async fn execute_tool(
    name: &str,
    input: &Value,
    tools: &[Box<dyn Tool>],
    ctx: &ToolContext,
) -> ToolResult {
    let requested_name = name.trim();
    let tool = find_tool_for_name(requested_name, tools);

    match tool {
        Some(tool) => {
            debug!(
                requested_tool = requested_name,
                resolved_tool = tool.name(),
                "Executing tool"
            );
            // Central permission backstop (issue #210): if a tool does not gate
            // itself (`self_gates() == false`) and requires a gated permission
            // level, prompt here BEFORE executing. On denial, return a blocked
            // result WITHOUT running the tool. Tools that already prompt
            // internally opt out via `self_gates() == true` (no double-prompt),
            // and read-only / no-permission tools are skipped. This makes a tool
            // that forgets to gate itself secure by default.
            if !tool.self_gates() && permission_level_is_gated(tool.permission_level()) {
                let canonical_name = tool.name();
                let description = synthesize_permission_description(canonical_name, input);
                if let Err(e) = ctx.check_permission(canonical_name, &description, false) {
                    warn!(
                        tool = canonical_name,
                        requested_tool = requested_name,
                        "Tool blocked by central permission backstop"
                    );
                    return ToolResult::error(e.to_string());
                }
            }
            tool.execute(input.clone(), ctx).await
        }
        None => {
            warn!(tool = requested_name, "Unknown or inactive tool requested");
            let hint = tool_name_hint(requested_name, tools);
            let registered_elsewhere = clawde_tools::find_tool(requested_name).is_some()
                || requested_name.eq_ignore_ascii_case(clawde_core::constants::TOOL_NAME_AGENT);
            if registered_elsewhere {
                ToolResult::error(format!(
                    "Tool '{}' is registered but not active in this session. {} Enable it in the agent/tool settings or use one of the active tools.",
                    requested_name,
                    active_tool_list(tools)
                ))
            } else if hint.is_empty() {
                ToolResult::error(format!(
                    "Unknown tool: {}. This tool is not available in the active tool set. {}",
                    requested_name,
                    active_tool_list(tools)
                ))
            } else {
                ToolResult::error(format!("Unknown tool: {}. {}", requested_name, hint))
            }
        }
    }
}

/// Resolve a provider-supplied tool name against the registered tools.
/// Exact names win; a unique case-insensitive match is accepted as a recovery
/// path for providers that normalize tool names unexpectedly.
fn find_tool_for_name<'a>(name: &str, tools: &'a [Box<dyn Tool>]) -> Option<&'a dyn Tool> {
    let requested = name.trim();
    if let Some(tool) = tools.iter().find(|tool| tool.name() == requested) {
        return Some(tool.as_ref());
    }

    let mut matches = tools
        .iter()
        .filter(|tool| tool.name().eq_ignore_ascii_case(requested));
    let first = matches.next();
    if first.is_some() && matches.next().is_none() {
        first.map(|tool| tool.as_ref())
    } else {
        None
    }
}

/// Build a short recovery hint for an unknown provider-supplied tool name.
fn active_tool_list(tools: &[Box<dyn Tool>]) -> String {
    let mut names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
    names.sort_unstable();
    names.truncate(12);
    if names.is_empty() {
        "No tools are active for this run.".to_string()
    } else {
        format!(
            "Active tools include: {}{}",
            names.join(", "),
            if tools.len() > names.len() {
                ", …"
            } else {
                "."
            }
        )
    }
}

/// Build a short recovery hint for an unknown provider-supplied tool name.
fn tool_name_hint(name: &str, tools: &[Box<dyn Tool>]) -> String {
    let requested = name.to_lowercase();
    let mut suggestions: Vec<(&str, usize)> = tools
        .iter()
        .filter_map(|tool| {
            let candidate = tool.name();
            let candidate_lower = candidate.to_lowercase();
            let score =
                if candidate_lower.contains(&requested) || requested.contains(&candidate_lower) {
                    100
                } else {
                    candidate_lower
                        .chars()
                        .zip(requested.chars())
                        .take_while(|(left, right)| left == right)
                        .count()
                };
            (score > 0).then_some((candidate, score))
        })
        .collect();
    suggestions.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    suggestions.truncate(3);

    if suggestions.is_empty() {
        String::new()
    } else {
        format!(
            "Available tools with a similar name: {}",
            suggestions
                .iter()
                .map(|(candidate, _)| *candidate)
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// Run a batch of tool-execution futures concurrently, abandoning them promptly
/// if `cancel_token` fires (issue #218).
///
/// Returns exactly one `ToolResult` per input future, in order, plus a bool that
/// is `true` iff the batch was cancelled before every tool finished. On the
/// happy path (no cancellation) this is `join_all` and the results are the real
/// tool outputs. On cancellation the in-flight futures are dropped (abandoned)
/// and every position is filled with a synthetic cancelled `ToolResult` so the
/// caller can still answer every `tool_use` and keep the message history valid.
pub(crate) async fn run_tool_batch<F>(
    exec_futures: Vec<F>,
    cancel_token: &tokio_util::sync::CancellationToken,
) -> (Vec<ToolResult>, bool)
where
    F: std::future::Future<Output = ToolResult>,
{
    let count = exec_futures.len();
    tokio::select! {
        results = futures::future::join_all(exec_futures) => (results, false),
        _ = cancel_token.cancelled() => {
            let cancelled = (0..count)
                .map(|_| ToolResult::error(TOOL_CANCELLED_MSG))
                .collect();
            (cancelled, true)
        }
    }
}

/// Load persisted todos for `session_id` and return a nudge string if any are
/// incomplete (status != "completed"). Returns empty string otherwise.
pub(crate) fn build_todo_nudge(session_id: &str) -> String {
    let todos = clawde_tools::todo_write::load_todos(session_id);
    let incomplete_count = todos
        .iter()
        .filter(|t| t["status"].as_str() != Some("completed"))
        .count();
    if incomplete_count == 0 {
        String::new()
    } else {
        format!(
            "You have {} incomplete task{} in your TodoWrite list. \
             Make sure to complete all tasks before ending your response.",
            incomplete_count,
            if incomplete_count == 1 { "" } else { "s" }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    struct NamedTool(&'static str, PermissionLevel);

    struct RecordingPermissionHandler {
        seen_tool: Arc<parking_lot::Mutex<Option<String>>>,
    }

    impl clawde_core::permissions::PermissionHandler for RecordingPermissionHandler {
        fn check_permission(
            &self,
            request: &clawde_core::permissions::PermissionRequest,
        ) -> clawde_core::permissions::PermissionDecision {
            *self.seen_tool.lock() = Some(request.tool_name.clone());
            clawde_core::permissions::PermissionDecision::Allow
        }

        fn request_permission(
            &self,
            request: &clawde_core::permissions::PermissionRequest,
        ) -> clawde_core::permissions::PermissionDecision {
            self.check_permission(request)
        }
    }

    #[async_trait::async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }

        fn description(&self) -> &str {
            "test tool"
        }

        fn permission_level(&self) -> PermissionLevel {
            self.1
        }

        fn input_schema(&self) -> Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(&self, _input: Value, _ctx: &ToolContext) -> ToolResult {
            ToolResult::success("named tool executed")
        }
    }

    fn test_context() -> ToolContext {
        ToolContext {
            working_dir: std::env::temp_dir(),
            permission_mode: clawde_core::config::PermissionMode::Default,
            permission_handler: Arc::new(clawde_core::permissions::AutoPermissionHandler {
                mode: clawde_core::config::PermissionMode::Default,
            }),
            cost_tracker: clawde_core::cost::CostTracker::new(),
            session_id: "tool-dispatch-test".to_string(),
            file_history: Arc::new(parking_lot::Mutex::new(
                clawde_core::file_history::FileHistory::new(),
            )),
            current_turn: Arc::new(AtomicUsize::new(0)),
            non_interactive: true,
            mcp_manager: None,
            config: clawde_core::config::Config::default(),
            provider_registry: None,
            managed_agent_config: None,
            completion_notifier: None,
            pending_permissions: None,
            permission_manager: None,
            user_question_tx: None,
            cancel_token: tokio_util::sync::CancellationToken::new(),
        }
    }

    #[test]
    fn every_builtin_definition_has_a_dispatch_match() {
        let tools = clawde_tools::all_tools();
        let advertised_names: Vec<String> =
            tools.iter().map(|tool| tool.to_definition().name).collect();

        assert!(advertised_names.iter().any(|name| name == "Bash"));
        for name in advertised_names {
            assert!(
                find_tool_for_name(&name, &tools).is_some(),
                "advertised tool must be executable: {name}"
            );
        }
    }

    #[tokio::test]
    async fn dispatch_accepts_unique_case_insensitive_tool_name() {
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(clawde_tools::ToolSearchTool)];
        let result = execute_tool(
            " toolsearch ",
            &serde_json::json!({"query": ""}),
            &tools,
            &test_context(),
        )
        .await;
        assert!(
            !result.is_error,
            "case-insensitive dispatch failed: {}",
            result.content
        );
        assert!(result.content.contains("Empty query"));
    }

    #[tokio::test]
    async fn inactive_registered_tool_explains_how_to_recover() {
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(clawde_tools::ToolSearchTool)];
        let result = execute_tool("Bash", &serde_json::json!({}), &tools, &test_context()).await;
        assert!(result.is_error);
        assert!(result.content.contains("registered but not active"));
        assert!(result.content.contains("ToolSearch"));
    }

    #[tokio::test]
    async fn unknown_tool_error_suggests_similar_registered_name() {
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(clawde_tools::ToolSearchTool)];
        let result =
            execute_tool("ToolSeach", &serde_json::json!({}), &tools, &test_context()).await;
        assert!(result.is_error);
        assert!(result.content.contains("Unknown tool: ToolSeach."));
        assert!(
            result.content.contains("ToolSearch"),
            "missing recovery hint: {}",
            result.content
        );
    }

    #[tokio::test]
    async fn exact_name_wins_over_case_insensitive_candidates() {
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(NamedTool("tool", PermissionLevel::None)),
            Box::new(NamedTool("Tool", PermissionLevel::None)),
        ];
        let result = execute_tool("Tool", &serde_json::json!({}), &tools, &test_context()).await;
        assert!(!result.is_error);
        assert_eq!(result.content, "named tool executed");
        assert_eq!(
            find_tool_for_name("Tool", &tools).map(Tool::name),
            Some("Tool")
        );
    }

    #[tokio::test]
    async fn ambiguous_case_insensitive_name_is_rejected() {
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(NamedTool("tool", PermissionLevel::None)),
            Box::new(NamedTool("TOOL", PermissionLevel::None)),
        ];
        let result = execute_tool("ToOl", &serde_json::json!({}), &tools, &test_context()).await;
        assert!(result.is_error);
        assert!(result.content.starts_with("Unknown tool: ToOl."));
        assert!(result.content.contains("TOOL, tool"));
    }

    #[tokio::test]
    async fn unknown_name_without_match_keeps_legacy_error_text() {
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(NamedTool("Bash", PermissionLevel::None))];
        let result = execute_tool(
            "CompletelyUnknown",
            &serde_json::json!({}),
            &tools,
            &test_context(),
        )
        .await;
        assert!(result.is_error);
        assert!(result
            .content
            .starts_with("Unknown tool: CompletelyUnknown. This tool is not available"));
        assert!(result.content.contains("Active tools include: Bash."));
    }

    #[tokio::test]
    async fn case_insensitive_alias_uses_canonical_permission_name() {
        let seen_tool = Arc::new(parking_lot::Mutex::new(None));
        let mut ctx = test_context();
        ctx.permission_handler = Arc::new(RecordingPermissionHandler {
            seen_tool: seen_tool.clone(),
        });
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(NamedTool("Write", PermissionLevel::Write))];

        let result = execute_tool("write", &serde_json::json!({"path": "x"}), &tools, &ctx).await;
        assert!(!result.is_error);
        assert_eq!(seen_tool.lock().as_deref(), Some("Write"));
    }
}
