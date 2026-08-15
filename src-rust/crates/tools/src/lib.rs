// clawde-tools: All tool implementations for Claurst.
//
// Each tool maps to a capability the LLM can invoke: running shell commands,
// reading/writing/editing files, searching codebases, fetching web pages, etc.

// type_complexity: the REPL tool holds a boxed session-callback map whose full
// type is unwieldy; a type alias would not meaningfully improve readability.
#![allow(clippy::type_complexity)]

use async_trait::async_trait;
use clawde_core::config::PermissionMode;
use clawde_core::cost::CostTracker;
use clawde_core::permissions::{PermissionDecision, PermissionHandler, PermissionRequest};
use clawde_core::types::ToolDefinition;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

// Sub-modules – each contains a full tool implementation.
pub mod apply_patch;
pub mod ask_user;
pub mod batch_edit;
pub mod brief;
pub mod bundled_skills;
pub mod computer_use;
pub mod config_tool;
pub mod cron;
pub mod detect_project;
pub mod enter_plan_mode;
pub mod enter_spec_mode;
pub mod exit_plan_mode;
pub mod exit_spec_mode;
pub mod file_edit;
pub mod file_read;
pub mod file_write;
pub mod formatter;
pub mod glob_tool;
pub mod goal_complete;
pub mod grep_tool;
pub mod line_endings;
pub mod lsp_tool;
pub mod mcp_auth_tool;
pub mod mcp_resources;
pub mod monitor_tool;
pub mod notebook_edit;
pub mod powershell;
pub mod pty_bash;
pub mod remote_trigger;
pub mod repl_tool;
pub mod run_lints;
pub mod run_tests;
pub mod send_message;
pub mod skill_tool;
pub mod sleep;
pub mod synthetic_output;
pub mod tasks;
pub mod team_tool;
#[cfg(test)]
pub(crate) mod test_support;
pub mod todo_write;
pub mod tool_search;
pub mod upstream_health;
pub mod web_fetch;
pub mod web_search;
pub mod worktree;

// Re-exports for convenience.
pub use apply_patch::ApplyPatchTool;
pub use ask_user::AskUserQuestionTool;
pub use batch_edit::BatchEditTool;
pub use brief::BriefTool;
pub use computer_use::ComputerUseTool;
pub use config_tool::ConfigTool;
pub use cron::{CronCreateTool, CronDeleteTool, CronListTool};
pub use detect_project::{detect_project_info, DetectProjectTool, ProjectInfo, ProjectLanguage};
pub use enter_plan_mode::EnterPlanModeTool;
pub use enter_spec_mode::EnterSpecModeTool;
pub use exit_plan_mode::ExitPlanModeTool;
pub use exit_spec_mode::ExitSpecModeTool;
pub use file_edit::FileEditTool;
pub use file_read::FileReadTool;
pub use file_write::FileWriteTool;
pub use formatter::try_format_file;
pub use glob_tool::GlobTool;
pub use goal_complete::GoalCompleteTool;
pub use grep_tool::GrepTool;
pub use lsp_tool::LspTool;
pub use mcp_auth_tool::McpAuthTool;
pub use mcp_resources::{ListMcpResourcesTool, ReadMcpResourceTool};
pub use monitor_tool::MonitorTool;
pub use notebook_edit::NotebookEditTool;
pub use powershell::PowerShellTool;
pub use pty_bash::PtyBashTool;
pub use remote_trigger::RemoteTriggerTool;
pub use repl_tool::ReplTool;
pub use run_lints::RunLintsTool;
pub use run_tests::RunTestsTool;
pub use send_message::{drain_inbox, peek_inbox, SendMessageTool};
pub use skill_tool::SkillTool;
pub use sleep::SleepTool;
pub use synthetic_output::SyntheticOutputTool;
pub use tasks::{
    Task, TaskCreateTool, TaskGetTool, TaskListTool, TaskOutputTool, TaskStatus, TaskStopTool,
    TaskUpdateTool, TASK_STORE,
};
pub use team_tool::{register_agent_runner, AgentRunFn, TeamCreateTool, TeamDeleteTool};
pub use todo_write::TodoWriteTool;
pub use tool_search::ToolSearchTool;
pub use upstream_health::UpstreamHealthTool;
pub use web_fetch::WebFetchTool;
pub use web_search::WebSearchTool;
pub use worktree::{EnterWorktreeTool, ExitWorktreeTool};

// ---------------------------------------------------------------------------
// AskUser question channel
// ---------------------------------------------------------------------------

/// Event sent through the TUI side-channel when the `AskUserQuestion` tool
/// needs to pause the query loop and collect a response from the user.
pub struct UserQuestionEvent {
    /// The question text to display.
    pub question: String,
    /// Optional predefined choices (for multiple-choice questions).
    pub options: Option<Vec<String>>,
    /// Send the user's answer back through this channel to resume execution.
    pub reply_tx: tokio::sync::oneshot::Sender<String>,
}

// ---------------------------------------------------------------------------
// Core trait & types
// ---------------------------------------------------------------------------

/// Stable machine-readable categories for tool failures.
///
/// The human-readable `ToolResult::content` remains the model-facing payload;
/// this code is for orchestration, public telemetry, and recovery decisions.
/// New tools may continue using `ToolResult::error` and receive the legacy
/// fallback classification until they opt into a more specific category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolErrorCode {
    InvalidInput,
    PermissionDenied,
    NetworkIsolationBlocked,
    NetworkSandboxUnavailable,
    TestFailed,
    LintFailed,
    Timeout,
    ExecutionFailed,
    ToolUnavailable,
    UnknownTool,
    PlanBlocked,
    Cancelled,
}

impl ToolErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::PermissionDenied => "permission_denied",
            Self::NetworkIsolationBlocked => "network_isolation_blocked",
            Self::NetworkSandboxUnavailable => "network_sandbox_unavailable",
            Self::TestFailed => "test_failed",
            Self::LintFailed => "lint_failed",
            Self::Timeout => "timeout",
            Self::ExecutionFailed => "execution_failed",
            Self::ToolUnavailable => "tool_unavailable",
            Self::UnknownTool => "unknown_tool",
            Self::PlanBlocked => "plan_blocked",
            Self::Cancelled => "cancelled",
        }
    }
}

/// The result of executing a tool.
#[derive(Debug, Clone)]
pub struct ToolResult {
    /// Content to send back to the model as the tool result.
    pub content: String,
    /// Whether this invocation was an error.
    pub is_error: bool,
    /// Optional stable category for an error result.
    pub error_code: Option<ToolErrorCode>,
    /// Optional structured metadata (for the TUI to render diffs, etc.).
    pub metadata: Option<Value>,
}

impl ToolResult {
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            error_code: None,
            metadata: None,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            error_code: None,
            metadata: None,
        }
    }

    pub fn error_with_code(code: ToolErrorCode, content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            error_code: Some(code),
            metadata: None,
        }
    }

    pub fn with_metadata(mut self, meta: Value) -> Self {
        self.metadata = Some(meta);
        self
    }
}

/// Permission level required by a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionLevel {
    /// No permission needed (read-only, purely informational).
    None,
    /// Read-only access to the filesystem or network.
    ReadOnly,
    /// Write access to the filesystem.
    Write,
    /// Arbitrary command execution.
    Execute,
    /// Potentially dangerous (e.g., bypass sandbox).
    Dangerous,
    /// Unconditionally forbidden — the action must never be executed regardless
    /// of permission mode.  Used by the bash tool (`PtyBashTool`) when the
    /// classifier identifies a `Critical`-risk command (e.g. `rm -rf /`,
    /// fork-bomb, `dd if=…`).
    Forbidden,
}

#[derive(Debug)]
pub struct PendingPermissionRequest {
    pub tool_use_id: String,
    pub request: clawde_core::permissions::PermissionRequest,
    pub reason: String,
    pub decision_tx: Option<tokio::sync::oneshot::Sender<PermissionDecision>>,
}

#[derive(Default)]
pub struct PendingPermissionStore {
    pub queue: VecDeque<PendingPermissionRequest>,
    pub waiting: HashMap<String, PendingPermissionRequest>,
}

/// Resolve a permission request that has already been surfaced to a UI.
///
/// The interactive TUI and the bridge/ACP UI share the same pending store.
/// Keeping the oneshot resolution here prevents one UI path from merely
/// dismissing its dialog while leaving the tool task blocked forever.
pub fn resolve_pending_permission(
    store: &Arc<parking_lot::Mutex<PendingPermissionStore>>,
    tool_use_id: &str,
    decision: PermissionDecision,
) -> bool {
    let Some(mut pending) = store.lock().waiting.remove(tool_use_id) else {
        return false;
    };
    if let Some(tx) = pending.decision_tx.take() {
        let _ = tx.send(decision);
    }
    true
}

/// Persistent shell state shared across Bash tool invocations within one session.
///
/// The bash tool (`PtyBashTool`) reads and writes this state on every call so
/// that `cd` and `export` commands persist across separate tool invocations, matching the
/// mental model described in the tool description ("the working directory
/// persists between commands").
#[derive(Debug, Clone, Default)]
pub struct ShellState {
    /// Current working directory as tracked by the shell state.
    /// Starts as the session's `working_dir`; updated after each `cd` command.
    pub cwd: Option<PathBuf>,
    /// Environment variable overrides exported by previous commands.
    pub env_vars: HashMap<String, String>,
}

impl ShellState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Process-global registry of shell states keyed by session_id.
/// This lets us persist cwd/env across Bash invocations without changing
/// the `ToolContext` struct (which is constructed in places we cannot modify).
static SHELL_STATE_REGISTRY: once_cell::sync::Lazy<
    dashmap::DashMap<String, Arc<parking_lot::Mutex<ShellState>>>,
> = once_cell::sync::Lazy::new(dashmap::DashMap::new);

/// Return the persistent `ShellState` for the given session, creating one if needed.
pub fn session_shell_state(session_id: &str) -> Arc<parking_lot::Mutex<ShellState>> {
    SHELL_STATE_REGISTRY
        .entry(session_id.to_string())
        .or_insert_with(|| Arc::new(parking_lot::Mutex::new(ShellState::new())))
        .clone()
}

/// Remove the shell state for a session (e.g. when the session ends).
#[allow(dead_code)]
pub fn clear_session_shell_state(session_id: &str) {
    SHELL_STATE_REGISTRY.remove(session_id);
}

/// Return the `ShadowSnapshot` for `working_dir`, creating it on first call.
/// Returns `None` when git is unavailable or the directory is not in a git repo.
#[allow(dead_code)]
pub fn session_shadow(
    working_dir: &std::path::Path,
) -> Option<Arc<clawde_core::snapshot::ShadowSnapshot>> {
    clawde_core::snapshot::get_or_create(working_dir)
}

/// Drop the cached shadow snapshot for `working_dir` (e.g. when a session ends).
#[allow(dead_code)]
pub fn clear_session_shadow(working_dir: &std::path::Path) {
    clawde_core::snapshot::remove(working_dir);
}

/// Write `contents` to `path` atomically: write to a temp file in the same
/// directory, then rename over the destination. A crash or disk-full mid-write
/// can never leave the destination truncated or half-written.
pub(crate) async fn write_atomic(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let tmp = path.with_file_name(format!(".{}.clawde-tmp-{}", file_name, std::process::id()));

    tokio::fs::write(&tmp, contents).await?;
    // Preserve the original file's permissions (e.g. the executable bit on
    // Unix), which a fresh temp file would otherwise reset.
    if let Ok(meta) = tokio::fs::metadata(path).await {
        let _ = tokio::fs::set_permissions(&tmp, meta.permissions()).await;
    }
    match tokio::fs::rename(&tmp, path).await {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            Err(e)
        }
    }
}

/// A cloneable handle for injecting notification messages into the next agent turn.
/// Used by background tasks with `notify_on_complete` to signal completion without polling.
#[derive(Clone)]
pub struct CompletionNotifier(Arc<dyn Fn(String) + Send + Sync>);

impl CompletionNotifier {
    pub fn new(f: impl Fn(String) + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }
    pub fn notify(&self, msg: String) {
        (self.0)(msg);
    }
}

/// Shared context passed to every tool invocation.
#[derive(Clone)]
pub struct ToolContext {
    pub working_dir: PathBuf,
    pub permission_mode: PermissionMode,
    pub permission_handler: Arc<dyn PermissionHandler>,
    pub cost_tracker: Arc<CostTracker>,
    pub session_id: String,
    pub file_history: Arc<parking_lot::Mutex<clawde_core::file_history::FileHistory>>,
    pub current_turn: Arc<AtomicUsize>,
    /// If true, suppress interactive prompts (batch / CI mode).
    pub non_interactive: bool,
    /// Optional MCP manager for ListMcpResources / ReadMcpResource tools.
    pub mcp_manager: Option<Arc<clawde_mcp::McpManager>>,
    /// Configured event hooks (PreToolUse, PostToolUse, etc.).
    pub config: clawde_core::config::Config,
    /// Live provider registry when the query loop has one, so read-only
    /// health tools (e.g. `UpstreamHealthTool`) can report measured upstream
    /// performance to the model. `None` in contexts without a registry (the
    /// tool then reports "no provider health data available").
    pub provider_registry: Option<std::sync::Arc<clawde_api::ProviderRegistry>>,
    /// Managed agent (manager-executor) configuration, if active.
    pub managed_agent_config: Option<clawde_core::config::ManagedAgentConfig>,
    /// Optional notifier for injecting completion messages into the next agent turn.
    /// Set when the query loop has a command queue wired up.
    pub completion_notifier: Option<CompletionNotifier>,
    /// Queue used by interactive mode to surface permission dialogs to the TUI.
    pub pending_permissions: Option<Arc<parking_lot::Mutex<PendingPermissionStore>>>,
    /// Shared permission manager so the interactive loop can record session/persistent approvals.
    pub permission_manager:
        Option<Arc<std::sync::Mutex<clawde_core::permissions::PermissionManager>>>,
    /// Channel for the `AskUserQuestion` tool to send questions to the TUI and
    /// receive the user's typed answer.  `None` in headless / non-interactive mode.
    pub user_question_tx: Option<tokio::sync::mpsc::UnboundedSender<UserQuestionEvent>>,
    /// Cancellation token for the owning query loop (issue #218). The parallel
    /// tool executor selects on this to abandon in-flight tools when the user
    /// cancels, and long-running tools may observe it to bail out early. Defaults
    /// to a fresh disconnected token; `run_query_loop` rebinds it to the loop's
    /// actual token so cancellation propagates into tools and sub-agents.
    pub cancel_token: tokio_util::sync::CancellationToken,
}

impl ToolContext {
    /// Resolve a potentially relative path against the working directory.
    pub fn resolve_path(&self, path: &str) -> PathBuf {
        let p = PathBuf::from(path);
        if p.is_absolute() {
            p
        } else {
            self.working_dir.join(p)
        }
    }

    fn permission_allowed_roots(&self) -> Vec<PathBuf> {
        let mut roots = self.config.workspace_paths.clone();
        roots.extend(self.config.additional_dirs.clone());
        roots
    }

    fn build_permission_request(
        &self,
        tool_name: &str,
        description: &str,
        details: Option<String>,
        is_read_only: bool,
        path: Option<PathBuf>,
    ) -> PermissionRequest {
        PermissionRequest {
            tool_name: tool_name.to_string(),
            description: description.to_string(),
            details,
            is_read_only,
            path: path.map(|p| p.display().to_string()),
            working_dir: Some(self.working_dir.clone()),
            allowed_roots: self.permission_allowed_roots(),
            context_description: None,
        }
    }

    fn request_permission_inner(
        &self,
        request: PermissionRequest,
    ) -> Result<(), clawde_core::error::ClaudeError> {
        let network_capable = known_network_capability(request.tool_name.as_str());
        // Keep the name-based compatibility path exercised for callers that
        // use the legacy permission helpers; input-aware tools use the explicit
        // capability variant below.
        self.ensure_network_allowed(&request.tool_name)?;
        self.request_permission_inner_with_capability(request, network_capable)
    }

    fn request_permission_inner_with_capability(
        &self,
        request: PermissionRequest,
        network_capable: bool,
    ) -> Result<(), clawde_core::error::ClaudeError> {
        self.ensure_network_allowed_for_tool(&request.tool_name, network_capable)?;
        let interactive_reason = request.details.clone();
        let decision = self.permission_handler.request_permission(&request);
        match decision {
            PermissionDecision::Allow | PermissionDecision::AllowPermanently => Ok(()),
            PermissionDecision::Ask { reason } if self.non_interactive => {
                Err(clawde_core::error::ClaudeError::PermissionDenied(format!(
                    "Permission denied for tool '{}': {}",
                    request.tool_name,
                    interactive_reason.unwrap_or(reason)
                )))
            }
            PermissionDecision::Ask { reason } => {
                let Some(queue) = &self.pending_permissions else {
                    return Err(clawde_core::error::ClaudeError::PermissionDenied(format!(
                        "Permission denied for tool '{}'",
                        request.tool_name
                    )));
                };

                let (tx, rx) = tokio::sync::oneshot::channel();
                queue.lock().queue.push_back(PendingPermissionRequest {
                    tool_use_id: format!(
                        "perm-{}-{}",
                        self.session_id,
                        self.current_turn.fetch_add(1, Ordering::Relaxed)
                    ),
                    request,
                    reason: interactive_reason.unwrap_or(reason),
                    decision_tx: Some(tx),
                });

                let decision = tokio::task::block_in_place(|| rx.blocking_recv());
                match decision {
                    Ok(PermissionDecision::Allow | PermissionDecision::AllowPermanently) => Ok(()),
                    _ => Err(clawde_core::error::ClaudeError::PermissionDenied(
                        "Permission denied by user".to_string(),
                    )),
                }
            }
            _ => Err(clawde_core::error::ClaudeError::PermissionDenied(format!(
                "Permission denied for tool '{}'",
                request.tool_name
            ))),
        }
    }

    /// Reject a network-capable tool when Ollama is in isolated mode.
    ///
    /// This is intentionally separate from the permission handler: bypass mode
    /// can skip ordinary permission decisions, but it must not re-enable online
    /// tools in an isolated session.
    pub fn ensure_network_allowed(
        &self,
        tool_name: &str,
    ) -> Result<(), clawde_core::error::ClaudeError> {
        self.ensure_network_allowed_for_tool(tool_name, known_network_capability(tool_name))
    }

    /// Enforce the isolated-mode boundary using the tool's capability metadata.
    ///
    /// The dispatcher calls this with `Tool::network_capable()`. Self-gating
    /// tools must call it with `true` when they can execute arbitrary code or
    /// otherwise reach the network, so bypass permissions cannot turn an
    /// isolated session back online.
    pub fn ensure_network_allowed_for_tool(
        &self,
        tool_name: &str,
        network_capable: bool,
    ) -> Result<(), clawde_core::error::ClaudeError> {
        if clawde_core::is_ollama_network_blocked() && network_capable {
            return Err(clawde_core::error::ClaudeError::PermissionDenied(format!(
                "Tool '{}' is unavailable in Ollama offline mode: network-capable tools are disabled.",
                tool_name
            )));
        }
        Ok(())
    }

    /// Check permissions for a tool invocation.
    pub fn check_permission(
        &self,
        tool_name: &str,
        description: &str,
        is_read_only: bool,
    ) -> Result<(), clawde_core::error::ClaudeError> {
        let request =
            self.build_permission_request(tool_name, description, None, is_read_only, None);
        self.request_permission_inner(request)
    }

    pub fn check_permission_for_path(
        &self,
        tool_name: &str,
        description: &str,
        path: PathBuf,
        is_read_only: bool,
    ) -> Result<(), clawde_core::error::ClaudeError> {
        let request =
            self.build_permission_request(tool_name, description, None, is_read_only, Some(path));
        self.request_permission_inner(request)
    }

    /// Like `check_permission` but also passes structured `details` text
    /// (e.g. a risk explanation) that the TUI permission dialog can display.
    #[allow(dead_code)]
    pub fn check_permission_with_details(
        &self,
        tool_name: &str,
        description: &str,
        details: &str,
        is_read_only: bool,
    ) -> Result<(), clawde_core::error::ClaudeError> {
        let request = self.build_permission_request(
            tool_name,
            description,
            Some(details.to_string()),
            is_read_only,
            None,
        );
        self.request_permission_inner(request).map_err(|_| {
            clawde_core::error::ClaudeError::PermissionDenied(format!(
                "Permission denied for tool '{}': {}",
                tool_name, details
            ))
        })
    }

    pub fn path_is_within_workspace(&self, path: &std::path::Path) -> bool {
        let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let mut roots =
            vec![std::fs::canonicalize(&self.working_dir)
                .unwrap_or_else(|_| self.working_dir.clone())];
        roots.extend(
            self.permission_allowed_roots()
                .into_iter()
                .map(|root| std::fs::canonicalize(&root).unwrap_or(root)),
        );
        roots.iter().any(|root| resolved.starts_with(root))
    }

    pub fn check_permission_with_details_and_path(
        &self,
        tool_name: &str,
        description: &str,
        details: &str,
        path: PathBuf,
        is_read_only: bool,
    ) -> Result<(), clawde_core::error::ClaudeError> {
        self.check_permission_with_details_and_path_for_capability(
            tool_name,
            description,
            details,
            path,
            is_read_only,
            known_network_capability(tool_name),
        )
    }

    /// Permission check variant for a tool whose effective network capability
    /// depends on validated input (for example, an isolated local test runner).
    /// The dispatcher still performs the outer capability check; this method
    /// keeps self-gating tools from re-applying a static, overly broad fallback.
    pub fn check_permission_with_details_and_path_for_capability(
        &self,
        tool_name: &str,
        description: &str,
        details: &str,
        path: PathBuf,
        is_read_only: bool,
        network_capable: bool,
    ) -> Result<(), clawde_core::error::ClaudeError> {
        let request = self.build_permission_request(
            tool_name,
            description,
            Some(details.to_string()),
            is_read_only,
            Some(path),
        );
        self.request_permission_inner_with_capability(request, network_capable)
            .map_err(|_| {
                clawde_core::error::ClaudeError::PermissionDenied(format!(
                    "Permission denied for tool '{}': {}",
                    tool_name, details
                ))
            })
    }

    pub fn current_turn_index(&self) -> usize {
        self.current_turn.load(Ordering::Relaxed)
    }

    pub fn record_file_change(
        &self,
        path: PathBuf,
        before_content: &[u8],
        after_content: &[u8],
        tool_name: &str,
    ) {
        self.file_history.lock().record_modification(
            path,
            before_content,
            after_content,
            self.current_turn_index(),
            tool_name,
        );
    }
}

/// Conservative fallback for direct permission paths that only have a tool
/// name rather than a `Tool` trait object. The dispatcher always uses the
/// capability-aware method above; dynamic MCP wrappers pass `true` explicitly.
fn known_network_capability(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "WebSearch"
            | "WebFetch"
            | "RemoteTrigger"
            | "Bash"
            | "PowerShell"
            | "REPL"
            | "LSP"
            | "ListMcpResources"
            | "ReadMcpResource"
            | "Agent"
            | "TeamCreate"
            | "RunTests"
            | "RunLints"
            | "EnterWorktree"
            | "CronCreate"
    )
}

/// The trait every tool must implement.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Human-readable name (matches the constant in clawde_core::constants).
    fn name(&self) -> &str;

    /// One-line description shown to the LLM.
    fn description(&self) -> &str;

    /// The permission level the tool requires.
    fn permission_level(&self) -> PermissionLevel;

    /// Whether this tool can make an outbound network request.
    ///
    /// Isolated Ollama mode removes these tools from the active tool set and
    /// the query dispatcher rejects them as a second, bypass-resistant
    /// backstop. Keep this capability metadata conservative: a false negative
    /// would expose an online side effect to offline mode.
    fn network_capable(&self) -> bool {
        false
    }

    /// Whether this tool remains available in isolated Ollama mode.
    ///
    /// This is separate from `network_capable()`: a tool may be network-capable
    /// in general but expose a narrowly validated, OS-sandboxed local mode.
    /// Such tools must enforce that mode in `execute()` and fail closed when
    /// the sandbox is unavailable. Arbitrary shell and other network-capable
    /// tools keep the default `false`.
    fn available_in_ollama_isolated_mode(&self) -> bool {
        false
    }

    /// Whether this tool performs its own permission gating inside `execute()`.
    ///
    /// - `false` (default): the central backstop in `execute_tool` is
    ///   responsible for gating this tool. Secure by default — a tool that
    ///   forgets to call `ctx.check_permission*` is still caught by the backstop
    ///   whenever its `permission_level()` is a gated level.
    /// - `true`: the tool already prompts for permission internally (it calls
    ///   `ctx.check_permission*` in `execute()`), so the central gate must NOT
    ///   also prompt — this prevents double-prompting.
    fn self_gates(&self) -> bool {
        false
    }

    /// Whether this tool is "advanced" / rarely used and a candidate for
    /// deferred (on-demand) disclosure rather than being sent in every request.
    ///
    /// Defaults to `false` (always disclosed). This is purely a metadata hint
    /// today — the prompt/tool-definition layer can read it to decide which
    /// tools to omit from the initial request and surface only via `ToolSearch`.
    ///
    // TODO(#233): wire this into the request assembly so `advanced()` tools are
    // omitted from the initial `tools` array and loaded on demand. That requires
    // a mutable *active tool set* tracked across turns inside `run_query_loop`,
    // which is under active refactor on other branches and intentionally left
    // untouched here. Land the tracking + gated re-injection there, then have
    // `all_tools()` callers filter on `advanced()` for the first request.
    fn advanced(&self) -> bool {
        false
    }

    /// JSON Schema describing the tool's input parameters.
    fn input_schema(&self) -> Value;

    /// Execute the tool with the given JSON input.
    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult;

    /// Produce a `ToolDefinition` suitable for sending to the API.
    fn to_definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: self.input_schema(),
        }
    }
}

/// Return all built-in tools (excluding AgentTool, which lives in cc-query).
pub fn all_tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(PtyBashTool),
        Box::new(FileReadTool),
        Box::new(FileEditTool),
        Box::new(FileWriteTool),
        Box::new(BatchEditTool),
        Box::new(ApplyPatchTool),
        Box::new(GlobTool),
        Box::new(GrepTool),
        Box::new(WebFetchTool),
        Box::new(WebSearchTool),
        Box::new(NotebookEditTool),
        Box::new(TaskCreateTool),
        Box::new(TaskGetTool),
        Box::new(TaskUpdateTool),
        Box::new(TaskListTool),
        Box::new(TaskStopTool),
        Box::new(TaskOutputTool),
        Box::new(TodoWriteTool),
        Box::new(AskUserQuestionTool),
        Box::new(EnterPlanModeTool),
        Box::new(ExitPlanModeTool),
        Box::new(EnterSpecModeTool),
        Box::new(ExitSpecModeTool),
        Box::new(PowerShellTool),
        Box::new(SleepTool),
        Box::new(CronCreateTool),
        Box::new(CronDeleteTool),
        Box::new(CronListTool),
        Box::new(EnterWorktreeTool),
        Box::new(ExitWorktreeTool),
        Box::new(ListMcpResourcesTool),
        Box::new(ReadMcpResourceTool),
        Box::new(ToolSearchTool),
        Box::new(BriefTool),
        Box::new(ConfigTool),
        Box::new(UpstreamHealthTool),
        Box::new(SendMessageTool),
        Box::new(SkillTool),
        Box::new(LspTool),
        Box::new(ReplTool),
        Box::new(DetectProjectTool),
        Box::new(RunTestsTool),
        Box::new(RunLintsTool),
        Box::new(TeamCreateTool),
        Box::new(TeamDeleteTool),
        Box::new(SyntheticOutputTool),
        Box::new(McpAuthTool),
        Box::new(RemoteTriggerTool),
        Box::new(MonitorTool),
        Box::new(GoalCompleteTool),
        // Computer Use is only available when compiled with the feature flag.
        #[cfg(feature = "computer-use")]
        Box::new(computer_use::ComputerUseTool),
    ]
}

/// Find a tool by name. Exact names win; a unique case-insensitive match
/// recovers provider/tool-call casing drift without making ambiguous names
/// executable.
pub fn find_tool(name: &str) -> Option<Box<dyn Tool>> {
    let requested = name.trim();
    let mut tools = all_tools();
    if let Some(index) = tools.iter().position(|tool| tool.name() == requested) {
        return Some(tools.remove(index));
    }
    let mut matches = tools
        .into_iter()
        .filter(|tool| tool.name().eq_ignore_ascii_case(requested));
    let first = matches.next();
    if first.is_some() && matches.next().is_none() {
        first
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_unreferenced_pub_functions_in_workspace() {
        // Dead-code guard: rustc's `dead_code` lint never fires for `pub` items,
        // so a `pub fn` that nothing calls silently rots. The shared
        // implementation in `clawde_core::dead_code_guard` scans the workspace
        // and fails if any `pub fn` / `pub async fn` declared in this crate has
        // no reference anywhere except its own declaration.
        clawde_core::dead_code_guard::assert_no_dead_pub_functions(env!("CARGO_MANIFEST_DIR"));
    }

    struct AskPermissionHandler {
        reason: String,
    }

    impl clawde_core::permissions::PermissionHandler for AskPermissionHandler {
        fn check_permission(
            &self,
            _request: &clawde_core::permissions::PermissionRequest,
        ) -> clawde_core::permissions::PermissionDecision {
            clawde_core::permissions::PermissionDecision::Ask {
                reason: self.reason.clone(),
            }
        }

        fn request_permission(
            &self,
            request: &clawde_core::permissions::PermissionRequest,
        ) -> clawde_core::permissions::PermissionDecision {
            self.check_permission(request)
        }
    }

    fn test_tool_context(
        handler: Arc<dyn clawde_core::permissions::PermissionHandler>,
    ) -> ToolContext {
        use clawde_core::config::Config;

        ToolContext {
            working_dir: PathBuf::from("/workspace"),
            permission_mode: clawde_core::config::PermissionMode::Default,
            permission_handler: handler,
            cost_tracker: clawde_core::cost::CostTracker::new(),
            session_id: "test".to_string(),
            file_history: Arc::new(parking_lot::Mutex::new(
                clawde_core::file_history::FileHistory::new(),
            )),
            current_turn: Arc::new(AtomicUsize::new(0)),
            non_interactive: true,
            mcp_manager: None,
            config: Config::default(),
            provider_registry: None,
            managed_agent_config: None,
            completion_notifier: None,
            pending_permissions: None,
            permission_manager: None,
            user_question_tx: None,
            cancel_token: tokio_util::sync::CancellationToken::new(),
        }
    }

    // ---- Tool registry tests ------------------------------------------------

    #[test]
    fn test_all_tools_non_empty() {
        let tools = all_tools();
        assert!(
            !tools.is_empty(),
            "all_tools() must return at least one tool"
        );
    }

    #[test]
    fn test_all_tools_have_unique_names() {
        let tools = all_tools();
        let mut names = std::collections::HashSet::new();
        for tool in &tools {
            assert!(
                names.insert(tool.name().to_string()),
                "Duplicate tool name: {}",
                tool.name()
            );
        }
    }

    #[test]
    fn isolated_mode_marks_indirect_network_capabilities() {
        let names: std::collections::HashSet<String> = all_tools()
            .into_iter()
            .filter(|tool| tool.network_capable())
            .map(|tool| tool.name().to_string())
            .collect();
        for name in [
            "Bash",
            "PowerShell",
            "REPL",
            "WebSearch",
            "WebFetch",
            "RemoteTrigger",
            "LSP",
            "ListMcpResources",
            "ReadMcpResource",
            "RunTests",
            "RunLints",
            "EnterWorktree",
            "CronCreate",
            "TeamCreate",
        ] {
            assert!(
                names.contains(name),
                "missing network capability for {name}"
            );
        }
    }

    #[test]
    fn isolated_mode_exception_is_narrow_and_explicit() {
        let tools = all_tools();
        let run_tests = tools.iter().find(|tool| tool.name() == "RunTests").unwrap();
        let bash = tools.iter().find(|tool| tool.name() == "Bash").unwrap();
        assert!(run_tests.network_capable());
        assert!(run_tests.available_in_ollama_isolated_mode());
        assert!(bash.network_capable());
        assert!(!bash.available_in_ollama_isolated_mode());
    }

    #[test]
    fn test_all_tools_have_non_empty_descriptions() {
        for tool in all_tools() {
            assert!(
                !tool.description().is_empty(),
                "Tool '{}' has empty description",
                tool.name()
            );
        }
    }

    #[test]
    fn test_all_tools_have_valid_input_schema() {
        for tool in all_tools() {
            let schema = tool.input_schema();
            assert!(
                schema.is_object(),
                "Tool '{}' input_schema must be a JSON object",
                tool.name()
            );
            assert!(
                schema.get("type").is_some() || schema.get("properties").is_some(),
                "Tool '{}' schema missing type or properties",
                tool.name()
            );
        }
    }

    #[test]
    fn test_find_tool_found() {
        let tool = find_tool("Bash");
        assert!(tool.is_some(), "Should find the Bash tool");
        assert_eq!(tool.unwrap().name(), "Bash");
    }

    #[test]
    fn test_find_tool_not_found() {
        assert!(find_tool("NonExistentTool12345").is_none());
    }

    #[test]
    fn test_find_tool_accepts_unique_case_insensitive_alias() {
        assert_eq!(
            find_tool(" bash ").map(|tool| tool.name().to_string()),
            Some("Bash".to_string())
        );
        assert_eq!(
            find_tool("read").map(|tool| tool.name().to_string()),
            Some("Read".to_string())
        );
    }

    #[test]
    fn test_core_tools_present() {
        let expected = [
            "Bash",
            "Read",
            "Edit",
            "Write",
            "Glob",
            "Grep",
            "WebFetch",
            "WebSearch",
            "TodoWrite",
            "Skill",
            "DetectProject",
            "RunTests",
            "RunLints",
        ];
        for name in &expected {
            assert!(
                find_tool(name).is_some(),
                "Expected tool '{}' not found in all_tools()",
                name
            );
        }
    }

    // ---- ToolResult tests ---------------------------------------------------

    #[test]
    fn test_tool_result_success() {
        let r = ToolResult::success("done");
        assert!(!r.is_error);
        assert_eq!(r.content, "done");
        assert!(r.error_code.is_none());
        assert!(r.metadata.is_none());
    }

    #[test]
    fn test_tool_result_error() {
        let r = ToolResult::error("something went wrong");
        assert!(r.is_error);
        assert_eq!(r.content, "something went wrong");
        assert!(r.error_code.is_none());
    }

    #[test]
    fn test_tool_result_error_code_is_stable_and_preserved() {
        let r = ToolResult::error_with_code(ToolErrorCode::NetworkIsolationBlocked, "offline")
            .with_metadata(serde_json::json!({"safe": true}));
        assert_eq!(r.error_code, Some(ToolErrorCode::NetworkIsolationBlocked));
        assert_eq!(
            ToolErrorCode::NetworkIsolationBlocked.as_str(),
            "network_isolation_blocked"
        );
        assert!(r.metadata.is_some());
    }

    #[test]
    fn test_tool_result_with_metadata() {
        let r = ToolResult::success("ok")
            .with_metadata(serde_json::json!({"file": "foo.rs", "lines": 10}));
        assert!(r.metadata.is_some());
        let meta = r.metadata.unwrap();
        assert_eq!(meta["file"], "foo.rs");
    }

    // ---- ToolContext::resolve_path tests ------------------------------------

    #[test]
    fn test_resolve_path_absolute() {
        use clawde_core::permissions::AutoPermissionHandler;

        let handler = Arc::new(AutoPermissionHandler {
            mode: clawde_core::config::PermissionMode::Default,
        });
        let ctx = test_tool_context(handler);

        // Absolute paths pass through unchanged
        let resolved = ctx.resolve_path("/absolute/path/file.rs");
        assert_eq!(resolved, PathBuf::from("/absolute/path/file.rs"));
    }

    #[test]
    fn test_resolve_path_relative() {
        use clawde_core::permissions::AutoPermissionHandler;

        let handler = Arc::new(AutoPermissionHandler {
            mode: clawde_core::config::PermissionMode::Default,
        });
        let ctx = test_tool_context(handler);

        // Relative paths get joined with working_dir
        let resolved = ctx.resolve_path("src/main.rs");
        assert_eq!(resolved, PathBuf::from("/workspace/src/main.rs"));
    }

    #[test]
    fn test_resolve_pending_permission_sends_decision_once() {
        let store = Arc::new(parking_lot::Mutex::new(PendingPermissionStore::default()));
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        store.lock().waiting.insert(
            "perm-1".to_string(),
            PendingPermissionRequest {
                tool_use_id: "perm-1".to_string(),
                request: clawde_core::permissions::PermissionRequest {
                    tool_name: "Bash".to_string(),
                    description: "run test".to_string(),
                    details: None,
                    is_read_only: false,
                    path: Some("python3 -m pytest -q".to_string()),
                    working_dir: None,
                    allowed_roots: vec![],
                    context_description: None,
                },
                reason: "approval required".to_string(),
                decision_tx: Some(tx),
            },
        );

        assert!(resolve_pending_permission(
            &store,
            "perm-1",
            PermissionDecision::Allow,
        ));
        assert!(matches!(rx.try_recv(), Ok(PermissionDecision::Allow)));
        assert!(!resolve_pending_permission(
            &store,
            "perm-1",
            PermissionDecision::Deny,
        ));
    }

    #[test]
    fn test_resolve_pending_permission_missing_id_is_non_destructive() {
        let store = Arc::new(parking_lot::Mutex::new(PendingPermissionStore::default()));
        assert!(!resolve_pending_permission(
            &store,
            "missing",
            PermissionDecision::Deny,
        ));
        assert!(store.lock().waiting.is_empty());
    }

    #[test]
    fn test_request_permission_uses_details_for_non_interactive_errors() {
        let ctx = test_tool_context(Arc::new(AskPermissionHandler {
            reason: "generic reason".to_string(),
        }));
        let request = ctx.build_permission_request(
            "PowerShell",
            "[High risk] set execution policy",
            Some("[High risk] This may modify system-wide security policy.".to_string()),
            false,
            Some(PathBuf::from("Set-ExecutionPolicy RemoteSigned")),
        );

        let error = ctx
            .request_permission_inner(request)
            .unwrap_err()
            .to_string();
        assert!(error.contains("[High risk] This may modify system-wide security policy."));
        assert!(!error.contains("generic reason"));
    }

    #[test]
    fn test_request_permission_falls_back_to_handler_reason_without_details() {
        let ctx = test_tool_context(Arc::new(AskPermissionHandler {
            reason: "generic reason".to_string(),
        }));
        let request = ctx.build_permission_request(
            "Bash",
            "run ls",
            None,
            false,
            Some(PathBuf::from("ls -la")),
        );

        let error = ctx
            .request_permission_inner(request)
            .unwrap_err()
            .to_string();
        assert!(error.contains("generic reason"));
    }

    // ---- PermissionLevel tests ---------------------------------------------

    #[test]
    fn test_permission_level_order() {
        // Just verify the variants exist and are distinct
        assert_ne!(PermissionLevel::None, PermissionLevel::ReadOnly);
        assert_ne!(PermissionLevel::Write, PermissionLevel::Execute);
        assert_ne!(PermissionLevel::Execute, PermissionLevel::Dangerous);
    }

    #[test]
    fn test_bash_tool_permission_level() {
        assert_eq!(PtyBashTool.permission_level(), PermissionLevel::Execute);
    }

    #[test]
    fn test_file_read_permission_level() {
        assert_eq!(FileReadTool.permission_level(), PermissionLevel::ReadOnly);
    }

    #[test]
    fn test_file_edit_permission_level() {
        assert_eq!(FileEditTool.permission_level(), PermissionLevel::Write);
    }

    #[test]
    fn test_file_write_permission_level() {
        assert_eq!(FileWriteTool.permission_level(), PermissionLevel::Write);
    }

    // ---- Tool to_definition tests ------------------------------------------

    #[test]
    fn test_tool_to_definition() {
        let def = PtyBashTool.to_definition();
        assert_eq!(def.name, "Bash");
        assert!(!def.description.is_empty());
        assert!(def.input_schema.is_object());
    }

    // ---- write_atomic tests -------------------------------------------------
    //
    // `write_atomic` is the single atomic-write path that ApplyPatch, BatchEdit,
    // NotebookEdit and the cron store (#226) all route through. These tests pin
    // its contract: it writes the exact bytes and never leaves a temp file
    // behind on success — the guarantee that makes those tools crash-safe.

    /// Count the `.clawde-tmp-*` scratch files left in `dir`.
    fn count_atomic_tmp_files(dir: &std::path::Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".clawde-tmp-"))
            .count()
    }

    #[tokio::test]
    async fn write_atomic_writes_content_and_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");

        // Fresh file.
        write_atomic(&path, b"hello\nworld\n").await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\nworld\n");
        assert_eq!(count_atomic_tmp_files(dir.path()), 0, "no tmp after create");

        // Overwrite an existing file (the crash-truncation scenario #226 fixes).
        write_atomic(&path, b"replaced").await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "replaced");
        assert_eq!(
            count_atomic_tmp_files(dir.path()),
            0,
            "no tmp after overwrite"
        );
    }

    /// The executable bit (and other permissions) must survive an atomic
    /// overwrite, since we rename a fresh temp file over the destination.
    #[cfg(unix)]
    #[tokio::test]
    async fn write_atomic_preserves_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("script.sh");

        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

        write_atomic(&path, b"#!/bin/sh\necho hi\n").await.unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "executable bit preserved");
        assert_eq!(count_atomic_tmp_files(dir.path()), 0);
    }
}
