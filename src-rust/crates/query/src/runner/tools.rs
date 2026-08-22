// Tool execution helpers: argument parsing, permission gating, and the
// single-tool / batch execution paths. Extracted from lib.rs (issue #232).
// Behavior-preserving move.

use crate::*;
use clawde_tools::ToolErrorCode;

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

/// Whether a permission tier must be gated by the central backstop.
pub(crate) fn permission_level_is_gated(level: PermissionLevel) -> bool {
    level.requires_approval()
}

/// Whether a tool must be gated by the central backstop. Stateful coordination
/// tools are gated even when their capability tier is `None`.
fn tool_requires_backstop(tool: &dyn Tool) -> bool {
    tool.stateful() || permission_level_is_gated(tool.permission_level())
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

/// Return whether a built-in file-mutator tool requires a structured spec when
/// spec-driven mode is enabled. Shell and interpreter tools are intentionally
/// outside this first slice because their arbitrary commands cannot be safely
/// classified here without duplicating the shell policy.
fn requires_plan_artifact(name: &str) -> bool {
    matches!(
        name,
        clawde_core::constants::TOOL_NAME_FILE_EDIT
            | clawde_core::constants::TOOL_NAME_FILE_WRITE
            | clawde_core::constants::TOOL_NAME_BATCH_EDIT
            | clawde_core::constants::TOOL_NAME_NOTEBOOK_EDIT
            | clawde_core::constants::TOOL_NAME_APPLY_PATCH
    )
}

/// Return a plan-gate error when spec-driven mode is enabled without a valid
/// structured spec in the current repository. This is intentionally checked
/// at the shared dispatcher rather than inside each file-mutator tool so every
/// covered mutator follows the same policy without duplicating tool semantics.
/// The spec review flow persists approval for the exact generated artifact and
/// active session. A stale, edited, unreviewed, or differently-generated spec
/// therefore cannot authorize this write.
fn plan_gate_error(
    name: &str,
    ctx: &ToolContext,
    active_task_id: Option<&str>,
) -> Option<ToolResult> {
    if !requires_plan_artifact(name) {
        return None;
    }
    // Normal sessions remain unchanged. An accepted implementation turn is
    // enforced even after the UI turns spec_mode off, because its task marker
    // explicitly carries the approval contract into the queued turn.
    if !ctx.config.spec_mode && active_task_id.is_none() {
        return None;
    }

    let project_root = clawde_core::git_utils::project_root(&ctx.working_dir);
    let approved = clawde_core::spec::Spec::approved_in(&project_root, &ctx.session_id);
    let Some((approved_path, approved_spec)) = approved.as_ref() else {
        return Some(ToolResult::error_with_code(
            ToolErrorCode::PlanBlocked,
            format!(
                "Plan approval required before '{}': spec-driven mode is enabled, but no current task-bound spec has been accepted for session '{}' in {}/specs/. Run /spec <task>, then accept it with /spec-review before making file changes.",
                name,
                ctx.session_id,
                project_root.display()
            ),
        ));
    };
    if active_task_id != Some(approved_spec.task_id.as_str()) {
        return Some(ToolResult::error_with_code(
            ToolErrorCode::PlanBlocked,
            format!(
                "Plan approval required before '{}': the accepted spec is bound to task '{}', not the current task. Generate and review a new /spec for this task.",
                name, approved_spec.task_id
            ),
        ));
    }
    // Phase D fail-closed boundary: an approved plan whose replan budget is
    // exhausted is Blocked, and a blocked plan must not authorize further
    // writes. Shell/interpreter tools remain outside this gate, so manual
    // recovery is still possible; continuing implementation requires a new
    // approved spec.
    if let Ok(raw_spec) = std::fs::read_to_string(approved_path) {
        let spec_hash = clawde_core::spec::Spec::content_hash(&raw_spec);
        if let Ok(Some(progress)) = clawde_core::PlanProgress::load_for(
            &project_root,
            &approved_spec.task_id,
            &ctx.session_id,
            &spec_hash,
        ) {
            match progress.status {
                clawde_core::PlanStatus::Blocked => {
                    return Some(ToolResult::error_with_code(
                        ToolErrorCode::PlanBlocked,
                        format!(
                            "Plan approval required before '{}': the approved plan for task '{}' is BLOCKED after exhausting its replan budget. Generate and accept a new /spec before making file changes.",
                            name, approved_spec.task_id
                        ),
                    ));
                }
                clawde_core::PlanStatus::Complete => {
                    return Some(ToolResult::error_with_code(
                        ToolErrorCode::PlanBlocked,
                        format!(
                            "Plan approval required before '{}': the approved plan for task '{}' is COMPLETE. Generate and accept a new /spec before making further file changes.",
                            name, approved_spec.task_id
                        ),
                    ));
                }
                clawde_core::PlanStatus::Active => {}
            }
        }
    }
    None
}

/// Execute a single tool invocation.
///
/// Test-only convenience wrapper (the query loop dispatches through
/// `execute_tool_for_task` with a task classification).
#[cfg(test)]
pub(crate) async fn execute_tool(
    name: &str,
    input: &Value,
    tools: &[Box<dyn Tool>],
    ctx: &ToolContext,
) -> ToolResult {
    execute_tool_for_task(name, input, tools, ctx, None).await
}

pub(crate) async fn execute_tool_for_task(
    name: &str,
    input: &Value,
    tools: &[Box<dyn Tool>],
    ctx: &ToolContext,
    active_task_id: Option<&str>,
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
            let explicitly_denied = ctx
                .config
                .disallowed_tools
                .iter()
                .any(|name| name.eq_ignore_ascii_case(tool.name()));
            let explicitly_allowed = ctx
                .config
                .allowed_tools
                .iter()
                .any(|name| name.eq_ignore_ascii_case(tool.name()));
            if explicitly_denied || (!ctx.config.allowed_tools.is_empty() && !explicitly_allowed) {
                warn!(
                    tool = tool.name(),
                    "Tool blocked by configured tool access rules"
                );
                return ToolResult::error_with_code(
                    ToolErrorCode::PermissionDenied,
                    format!(
                        "Tool '{}' is blocked by the configured tool access rules.",
                        tool.name()
                    ),
                );
            }
            if let Some(blocked) = plan_gate_error(tool.name(), ctx, active_task_id) {
                warn!(tool = tool.name(), "Tool blocked by plan-artifact gate");
                return blocked;
            }

            // Isolated Ollama mode is a hard boundary for outbound tools. Keep
            // this before the permission handler so bypass/allow rules cannot
            // turn an offline session back into an online one.
            if clawde_core::network_isolation_enabled(&ctx.config)
                && unavailable_in_isolated_mode(tool)
            {
                warn!(tool = tool.name(), "Tool blocked by isolated Ollama mode");
                return ToolResult::error_with_code(
                    ToolErrorCode::NetworkIsolationBlocked,
                    format!(
                        "Tool '{}' is unavailable in Ollama offline mode: network-capable tools are disabled.",
                        tool.name()
                    ),
                );
            }

            // Central permission backstop (issue #210): if a tool does not gate
            // itself (`self_gates() == false`) and requires a gated permission
            // level, prompt here BEFORE executing. On denial, return a blocked
            // result WITHOUT running the tool. Tools that already prompt
            // internally opt out via `self_gates() == true` (no double-prompt),
            // and read-only / no-permission tools are skipped. This makes a tool
            // that forgets to gate itself secure by default.
            if !tool.self_gates() && tool_requires_backstop(tool) && !explicitly_allowed {
                let canonical_name = tool.name();
                let description = synthesize_permission_description(canonical_name, input);
                if let Err(e) = ctx.check_permission_for_tool(tool, &description, false) {
                    warn!(
                        tool = canonical_name,
                        requested_tool = requested_name,
                        "Tool blocked by central permission backstop"
                    );
                    return ToolResult::error_with_code(
                        ToolErrorCode::PermissionDenied,
                        e.to_string(),
                    );
                }
            }
            tool.execute(input.clone(), ctx).await
        }
        None => {
            warn!(tool = requested_name, "Unknown or inactive tool requested");
            let hint = tool_name_hint(requested_name, tools);
            let registered_tool = clawde_tools::find_tool(requested_name);
            if clawde_core::network_isolation_enabled(&ctx.config)
                && registered_tool
                    .as_deref()
                    .is_some_and(unavailable_in_isolated_mode)
            {
                return ToolResult::error_with_code(
                    ToolErrorCode::NetworkIsolationBlocked,
                    isolated_network_tool_message(requested_name),
                );
            }
            let registered_elsewhere = registered_tool.is_some()
                || requested_name.eq_ignore_ascii_case(clawde_core::constants::TOOL_NAME_AGENT);
            if registered_elsewhere {
                ToolResult::error_with_code(
                    ToolErrorCode::ToolUnavailable,
                    format!(
                        "Tool '{}' is registered but not active in this session. {} Enable it in the agent/tool settings or use one of the active tools.",
                        requested_name,
                        active_tool_list(tools)
                    ),
                )
            } else if hint.is_empty() {
                ToolResult::error_with_code(
                    ToolErrorCode::UnknownTool,
                    format!(
                        "Unknown tool: {}. This tool is not available in the active tool set. {}",
                        requested_name,
                        active_tool_list(tools)
                    ),
                )
            } else {
                ToolResult::error_with_code(
                    ToolErrorCode::UnknownTool,
                    format!("Unknown tool: {}. {}", requested_name, hint),
                )
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

fn unavailable_in_isolated_mode(tool: &dyn Tool) -> bool {
    tool.network_capable() && !tool.available_in_ollama_isolated_mode()
}

/// Build a short recovery hint for an unknown provider-supplied tool name.
fn isolated_network_tool_message(tool_name: &str) -> String {
    format!(
        "Tool '{}' is unavailable in Ollama offline mode because it is network-capable. Arbitrary shell and network tools remain blocked; use RunTests for validated local tests inside the network-isolated sandbox.",
        tool_name
    )
}

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
                .map(|_| ToolResult::error_with_code(ToolErrorCode::Cancelled, TOOL_CANCELLED_MSG))
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

    struct DenyingPermissionHandler;

    impl clawde_core::permissions::PermissionHandler for DenyingPermissionHandler {
        fn check_permission(
            &self,
            _request: &clawde_core::permissions::PermissionRequest,
        ) -> clawde_core::permissions::PermissionDecision {
            clawde_core::permissions::PermissionDecision::Deny
        }

        fn request_permission(
            &self,
            _request: &clawde_core::permissions::PermissionRequest,
        ) -> clawde_core::permissions::PermissionDecision {
            clawde_core::permissions::PermissionDecision::Deny
        }
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
                mode: clawde_core::config::PermissionMode::BypassPermissions,
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
            effort: None,
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
    async fn plan_gate_blocks_write_without_spec() {
        let mut ctx = test_context();
        ctx.config.spec_mode = true;
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(NamedTool("Edit", PermissionLevel::Write))];

        let result =
            execute_tool_for_task("Edit", &serde_json::json!({}), &tools, &ctx, None).await;

        assert!(result.is_error);
        assert!(result
            .content
            .contains("Plan approval required before 'Edit'"));
        assert!(result.content.contains("/spec <task>"));
        assert!(result.content.contains("/spec-review"));
    }

    #[tokio::test]
    async fn plan_gate_covers_all_concrete_file_mutators() {
        let dir = tempfile::tempdir().unwrap();
        let edit_path = dir.path().join("edit.txt");
        let batch_path = dir.path().join("batch.txt");
        let notebook_path = dir.path().join("notebook.ipynb");
        std::fs::write(&edit_path, "old\n").unwrap();
        std::fs::write(&batch_path, "old\n").unwrap();
        std::fs::write(
            &notebook_path,
            serde_json::json!({
                "cells": [{
                    "cell_type": "code",
                    "id": "c1",
                    "metadata": {},
                    "source": ["old\\n"],
                    "outputs": [],
                    "execution_count": null
                }],
                "metadata": {},
                "nbformat": 4,
                "nbformat_minor": 5
            })
            .to_string(),
        )
        .unwrap();

        let inputs = vec![
            (
                clawde_core::constants::TOOL_NAME_FILE_EDIT,
                serde_json::json!({
                    "file_path": edit_path,
                    "old_string": "old",
                    "new_string": "new"
                }),
            ),
            (
                clawde_core::constants::TOOL_NAME_FILE_WRITE,
                serde_json::json!({"file_path": dir.path().join("write.txt"), "content": "new"}),
            ),
            (
                clawde_core::constants::TOOL_NAME_BATCH_EDIT,
                serde_json::json!({"edits": [{
                    "file_path": batch_path,
                    "old_string": "old",
                    "new_string": "new"
                }]}),
            ),
            (
                clawde_core::constants::TOOL_NAME_NOTEBOOK_EDIT,
                serde_json::json!({
                    "notebook_path": notebook_path,
                    "cell_id": "cell-0",
                    "new_source": "new",
                    "edit_mode": "replace"
                }),
            ),
            (
                clawde_core::constants::TOOL_NAME_APPLY_PATCH,
                serde_json::json!({
                    "patch": r#"--- a/patch.txt
+++ b/patch.txt
@@ -0,0 +1,1 @@
+new
"#
                }),
            ),
        ];
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(clawde_tools::FileEditTool),
            Box::new(clawde_tools::FileWriteTool),
            Box::new(clawde_tools::BatchEditTool),
            Box::new(clawde_tools::NotebookEditTool),
            Box::new(clawde_tools::ApplyPatchTool),
        ];

        let mut no_spec_ctx = test_context();
        no_spec_ctx.working_dir = dir.path().to_path_buf();
        no_spec_ctx.config.spec_mode = true;
        for (name, input) in &inputs {
            let result = execute_tool_for_task(name, input, &tools, &no_spec_ctx, None).await;
            assert!(result.is_error, "{name} must be blocked without a spec");
            assert!(
                result.content.contains("Plan approval required"),
                "{name} returned: {}",
                result.content
            );
        }

        let mut valid_spec_ctx = no_spec_ctx.clone();
        valid_spec_ctx.permission_handler = Arc::new(DenyingPermissionHandler);
        let accepted_spec_path = dir.path().join("specs/concrete-mutators.json");
        clawde_core::spec::Spec {
            task_id: "concrete-mutators-task".to_string(),
            task: "Exercise concrete mutators".to_string(),
            session_id: Some("tool-dispatch-test".to_string()),
            title: "Concrete mutator plan".to_string(),
            ..Default::default()
        }
        .write_to(&accepted_spec_path)
        .unwrap();
        clawde_core::spec::Spec::write_approval_for_session(
            &accepted_spec_path,
            "tool-dispatch-test",
        )
        .unwrap();
        for (name, input) in &inputs {
            let result = execute_tool_for_task(
                name,
                input,
                &tools,
                &valid_spec_ctx,
                Some("concrete-mutators-task"),
            )
            .await;
            assert!(
                result.is_error,
                "{name} must still reach normal permissions"
            );
            assert!(
                result.content.contains("Permission denied"),
                "{name} bypassed normal permissions: {}",
                result.content
            );
        }
    }

    #[tokio::test]
    async fn plan_gate_allows_write_with_valid_spec_and_preserves_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = test_context();
        ctx.working_dir = dir.path().to_path_buf();
        ctx.config.spec_mode = true;
        let spec_path = dir.path().join("specs/test-plan.json");
        clawde_core::spec::Spec {
            task_id: "test-plan-task".to_string(),
            task: "Test plan".to_string(),
            session_id: Some("tool-dispatch-test".to_string()),
            title: "Test plan".to_string(),
            ..Default::default()
        }
        .write_to(&spec_path)
        .unwrap();
        clawde_core::spec::Spec::write_approval_for_session(&spec_path, "tool-dispatch-test")
            .unwrap();
        let seen_tool = Arc::new(parking_lot::Mutex::new(None));
        ctx.permission_handler = Arc::new(RecordingPermissionHandler {
            seen_tool: seen_tool.clone(),
        });
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(NamedTool("Edit", PermissionLevel::Write))];

        let result = execute_tool_for_task(
            "Edit",
            &serde_json::json!({}),
            &tools,
            &ctx,
            Some("test-plan-task"),
        )
        .await;

        assert!(!result.is_error);
        assert_eq!(result.content, "named tool executed");
        assert_eq!(seen_tool.lock().as_deref(), Some("Edit"));
    }

    #[tokio::test]
    async fn plan_gate_blocks_writes_when_approved_plan_is_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = test_context();
        ctx.working_dir = dir.path().to_path_buf();
        ctx.config.spec_mode = true;
        let spec_path = dir.path().join("specs/test-plan.json");
        let spec = clawde_core::spec::Spec {
            task_id: "blocked-plan-task".to_string(),
            task: "Blocked plan".to_string(),
            session_id: Some("tool-dispatch-test".to_string()),
            title: "Blocked plan".to_string(),
            ..Default::default()
        };
        spec.write_to(&spec_path).unwrap();
        clawde_core::spec::Spec::write_approval_for_session(&spec_path, "tool-dispatch-test")
            .unwrap();
        // The approved plan exists but is fail-closed as Blocked (replan
        // budget exhausted) — a valid approval alone must not authorize
        // further structured writes.
        let raw = std::fs::read_to_string(&spec_path).unwrap();
        let mut progress = clawde_core::PlanProgress::initialize_for_spec(
            dir.path(),
            &spec_path,
            &raw,
            &spec,
            "tool-dispatch-test",
        )
        .unwrap();
        progress
            .block_active_step(clawde_core::PlanEvidence {
                kind: "blocked".to_string(),
                summary: "Replan budget exhausted.".to_string(),
                reference: Some("evidence/blocked.txt".to_string()),
            })
            .unwrap();
        progress.save(dir.path()).unwrap();
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(NamedTool("Edit", PermissionLevel::Write))];

        let result = execute_tool_for_task(
            "Edit",
            &serde_json::json!({}),
            &tools,
            &ctx,
            Some("blocked-plan-task"),
        )
        .await;

        assert!(
            result.is_error,
            "blocked plan must block writes: {}",
            result.content
        );
        assert!(result.content.contains("BLOCKED"));
        assert!(result.content.contains("Generate and accept a new /spec"));
    }

    #[tokio::test]
    async fn plan_gate_blocks_writes_when_approved_plan_is_complete() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = test_context();
        ctx.working_dir = dir.path().to_path_buf();
        ctx.config.spec_mode = true;
        let spec_path = dir.path().join("specs/test-plan.json");
        let spec = clawde_core::spec::Spec {
            task_id: "complete-plan-task".to_string(),
            task: "Complete plan".to_string(),
            session_id: Some("tool-dispatch-test".to_string()),
            title: "Complete plan".to_string(),
            ..Default::default()
        };
        spec.write_to(&spec_path).unwrap();
        clawde_core::spec::Spec::write_approval_for_session(&spec_path, "tool-dispatch-test")
            .unwrap();
        let raw = std::fs::read_to_string(&spec_path).unwrap();
        let mut progress = clawde_core::PlanProgress::initialize_for_spec(
            dir.path(),
            &spec_path,
            &raw,
            &spec,
            "tool-dispatch-test",
        )
        .unwrap();
        while progress.active_step_id.is_some() {
            progress
                .record_evidence(clawde_core::PlanEvidence {
                    kind: "complete".to_string(),
                    summary: "Approved step completed deterministically.".to_string(),
                    reference: Some("evidence/complete.txt".to_string()),
                })
                .unwrap();
            progress.complete_active_step().unwrap();
        }
        assert_eq!(progress.status, clawde_core::PlanStatus::Complete);
        progress.save(dir.path()).unwrap();
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(NamedTool("Edit", PermissionLevel::Write))];

        let result = execute_tool_for_task(
            "Edit",
            &serde_json::json!({}),
            &tools,
            &ctx,
            Some("complete-plan-task"),
        )
        .await;

        assert!(
            result.is_error,
            "complete plan must block writes: {}",
            result.content
        );
        assert!(result.content.contains("COMPLETE"));
        assert!(result.content.contains("Generate and accept a new /spec"));
    }

    #[tokio::test]
    async fn new_approved_spec_reopens_writes_after_terminal_plan() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = test_context();
        ctx.working_dir = dir.path().to_path_buf();
        ctx.config.spec_mode = true;
        let spec_path = dir.path().join("specs/test-plan.json");
        let session_id = "tool-dispatch-test";
        let initial_spec = clawde_core::spec::Spec {
            task_id: "complete-plan-task".to_string(),
            task: "Complete plan".to_string(),
            session_id: Some(session_id.to_string()),
            title: "Complete plan".to_string(),
            ..Default::default()
        };
        initial_spec.write_to(&spec_path).unwrap();
        clawde_core::spec::Spec::write_approval_for_session(&spec_path, session_id).unwrap();
        let raw = std::fs::read_to_string(&spec_path).unwrap();
        let mut progress = clawde_core::PlanProgress::initialize_for_spec(
            dir.path(),
            &spec_path,
            &raw,
            &initial_spec,
            session_id,
        )
        .unwrap();
        while progress.active_step_id.is_some() {
            progress
                .record_evidence(clawde_core::PlanEvidence {
                    kind: "complete".to_string(),
                    summary: "Initial approved plan completed.".to_string(),
                    reference: Some("evidence/complete.txt".to_string()),
                })
                .unwrap();
            progress.complete_active_step().unwrap();
        }
        assert_eq!(progress.status, clawde_core::PlanStatus::Complete);
        progress.save(dir.path()).unwrap();

        let replacement_spec = clawde_core::spec::Spec {
            task_id: "replacement-plan-task".to_string(),
            task: "Continue with a newly approved plan".to_string(),
            session_id: Some(session_id.to_string()),
            title: "Replacement plan".to_string(),
            ..Default::default()
        };
        replacement_spec.write_to(&spec_path).unwrap();
        clawde_core::spec::Spec::write_approval_for_session(&spec_path, session_id).unwrap();
        let replacement_raw = std::fs::read_to_string(&spec_path).unwrap();
        let replacement_progress = clawde_core::PlanProgress::load_for(
            dir.path(),
            &replacement_spec.task_id,
            session_id,
            &clawde_core::spec::Spec::content_hash(&replacement_raw),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            replacement_progress.status,
            clawde_core::PlanStatus::Active,
            "a fresh approval must initialize an active replacement plan"
        );
        assert_eq!(
            clawde_core::spec::Spec::approved_in(dir.path(), session_id)
                .unwrap()
                .1
                .task_id,
            replacement_spec.task_id
        );

        let tools: Vec<Box<dyn Tool>> = vec![Box::new(NamedTool("Edit", PermissionLevel::Write))];
        let result = execute_tool_for_task(
            "Edit",
            &serde_json::json!({}),
            &tools,
            &ctx,
            Some(&replacement_spec.task_id),
        )
        .await;
        assert!(
            !result.is_error,
            "a new approved spec must reopen writes: {}",
            result.content
        );
        assert_eq!(result.content, "named tool executed");
    }

    #[tokio::test]
    async fn plan_gate_rejects_unreviewed_and_stale_approval() {
        let dir = tempfile::tempdir().unwrap();
        let spec_path = dir.path().join("specs/task.json");
        let spec = clawde_core::spec::Spec {
            task_id: "task-one".to_string(),
            task: "Task one".to_string(),
            session_id: Some("tool-dispatch-test".to_string()),
            title: "Task one".to_string(),
            ..Default::default()
        };
        spec.write_to(&spec_path).unwrap();
        let mut ctx = test_context();
        ctx.working_dir = dir.path().to_path_buf();
        ctx.config.spec_mode = true;
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(NamedTool("Edit", PermissionLevel::Write))];

        let unreviewed = execute_tool("Edit", &serde_json::json!({}), &tools, &ctx).await;
        assert!(unreviewed.content.contains("Plan approval required"));

        clawde_core::spec::Spec::write_approval_for_session(&spec_path, &ctx.session_id).unwrap();
        let approved = execute_tool_for_task(
            "Edit",
            &serde_json::json!({}),
            &tools,
            &ctx,
            Some("task-one"),
        )
        .await;
        assert!(!approved.is_error);

        std::fs::write(
            &spec_path,
            spec.to_json().replace("Task one", "Task one changed"),
        )
        .unwrap();
        let stale = execute_tool("Edit", &serde_json::json!({}), &tools, &ctx).await;
        assert!(stale.content.contains("Plan approval required"));

        let mut wrong_session = ctx.clone();
        wrong_session.session_id = "different-session".to_string();
        let unrelated = execute_tool("Edit", &serde_json::json!({}), &tools, &wrong_session).await;
        assert!(unrelated.content.contains("Plan approval required"));
    }

    #[tokio::test]
    async fn plan_gate_is_disabled_by_default() {
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(NamedTool("Edit", PermissionLevel::Write))];

        let result = execute_tool("Edit", &serde_json::json!({}), &tools, &test_context()).await;

        assert!(!result.is_error);
        assert_eq!(result.content, "named tool executed");
    }

    #[tokio::test]
    async fn plan_gate_does_not_block_read_tools() {
        let mut ctx = test_context();
        ctx.config.spec_mode = true;
        let tools: Vec<Box<dyn Tool>> =
            vec![Box::new(NamedTool("Read", PermissionLevel::ReadOnly))];

        let result = execute_tool("Read", &serde_json::json!({}), &tools, &ctx).await;

        assert!(!result.is_error);
        assert_eq!(result.content, "named tool executed");
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

    #[test]
    fn isolated_mode_blocks_arbitrary_shell_but_keeps_run_tests() {
        let bash = clawde_tools::find_tool("Bash").expect("Bash is registered");
        let run_tests = clawde_tools::find_tool("RunTests").expect("RunTests is registered");
        let run_lints = clawde_tools::find_tool("RunLints").expect("RunLints is registered");
        assert!(unavailable_in_isolated_mode(bash.as_ref()));
        assert!(!unavailable_in_isolated_mode(run_tests.as_ref()));
        assert!(!unavailable_in_isolated_mode(run_lints.as_ref()));
    }

    #[test]
    fn isolated_network_tool_message_explains_the_safe_alternative() {
        let message = isolated_network_tool_message("Bash");
        assert!(message.contains("Ollama offline mode"));
        assert!(message.contains("network-capable"));
        assert!(message.contains("RunTests"));
    }

    #[tokio::test]
    async fn config_only_isolation_blocks_inactive_network_tool() {
        let mut ctx = test_context();
        ctx.config.provider_configs.insert(
            "ollama".to_string(),
            clawde_core::config::ProviderConfig {
                options: [("mode".to_string(), serde_json::json!("isolated"))]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
        );
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(clawde_tools::ToolSearchTool)];
        let result = execute_tool("Bash", &serde_json::json!({}), &tools, &ctx).await;
        assert!(result.is_error);
        assert_eq!(
            result.error_code,
            Some(ToolErrorCode::NetworkIsolationBlocked)
        );
        assert!(result.content.contains("Ollama offline mode"));
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
