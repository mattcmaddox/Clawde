// AgentTool: spawn a sub-agent to handle a complex sub-task.
//
// Lives in cc-query (not cc-tools) to avoid a circular dependency:
//   cc-tools would need cc-query, but cc-query already needs cc-tools.
//
// The AgentTool creates a nested query loop with its own context, enabling
// the model to delegate complex work to specialized sub-agents. Each sub-agent:
//   - Runs its own agentic loop
//   - Has access to all tools (except AgentTool itself, preventing infinite recursion)
//   - Returns its final output as the tool result
//
// New capabilities (TS parity):
//   - `isolation: "worktree"` — run the agent in a dedicated git worktree so
//     file edits don't conflict with the parent checkout or sibling agents.
//   - `run_in_background: true` — fire-and-forget; returns agent_id immediately.
//     Use the `monitor` tool to check completion status/output.

use async_trait::async_trait;
use clawde_api::client::ClientConfig;
use clawde_api::{AnthropicClient, ModelRegistry, ProviderRegistry};
use clawde_core::types::Message;
use clawde_tools::{PermissionLevel, Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::{run_query_loop, QueryConfig, QueryOutcome};

// ---------------------------------------------------------------------------
// Worktree isolation helpers
// ---------------------------------------------------------------------------

fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

async fn create_worktree(git_root: &Path, agent_id: &str) -> Option<PathBuf> {
    let worktree_dir = std::env::temp_dir().join(format!("claude-agent-{}", agent_id));
    let output = tokio::process::Command::new("git")
        .args([
            "worktree",
            "add",
            "--detach",
            worktree_dir.to_str().unwrap_or_default(),
            "HEAD",
        ])
        .current_dir(git_root)
        .output()
        .await
        .ok()?;
    if output.status.success() {
        Some(worktree_dir)
    } else {
        warn!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        None
    }
}

async fn remove_worktree(git_root: &Path, worktree_dir: &Path) {
    let _ = tokio::process::Command::new("git")
        .args([
            "worktree",
            "remove",
            "--force",
            worktree_dir.to_str().unwrap_or_default(),
        ])
        .current_dir(git_root)
        .output()
        .await;
}

// ---------------------------------------------------------------------------
// AgentTool
// ---------------------------------------------------------------------------

pub struct AgentTool;

/// Return the exact tool allowlist used by the production semantic verifier.
///
/// Keeping this at the AgentTool boundary lets native diagnostics and the live
/// runner assert the same capability set without constructing a provider or
/// loading credentials.
pub fn semantic_verifier_tool_names() -> Vec<String> {
    crate::continuation::semantic_read_only_tool_names()
}

/// Build the actual production tool set for a semantic verifier.
///
/// This keeps the runtime boundary and native diagnostics on the same path:
/// the verifier receives only the explicit read-only allowlist, and AgentTool
/// itself is excluded so the verifier cannot delegate recursively.
pub fn build_agent_tools(
    allowed: Option<&[String]>,
    exclude_agent_tool: bool,
) -> Vec<Box<dyn Tool>> {
    let network_blocked = clawde_core::is_ollama_network_blocked();
    clawde_tools::all_tools()
        .into_iter()
        .filter(|tool| {
            if exclude_agent_tool && tool.name() == clawde_core::constants::TOOL_NAME_AGENT {
                return false;
            }
            if network_blocked && tool.network_capable() {
                return false;
            }
            allowed.is_none_or(|allowed| allowlisted_tool_name(allowed, tool.name()))
        })
        .collect()
}

pub fn build_semantic_verifier_tools() -> Vec<Box<dyn Tool>> {
    let allowed = semantic_verifier_tool_names();
    build_agent_tools(Some(&allowed), true)
}

/// Build the AgentTool input JSON for a semantic verification request.
///
/// Extracted from `semantic_verify_runner` so the request→input mapping (the
/// read-only allowlist, the fixed `free/auto` model, the one-shot turn budget,
/// and the JSON-only prompt contract) is testable without a live model call.
fn semantic_model(config: &clawde_core::config::Config) -> String {
    let configured = config.verify.semantic_model.trim();
    let is_free_route = configured
        .strip_prefix("free/")
        .is_some_and(|model| !model.trim().is_empty());
    if is_free_route {
        configured.to_string()
    } else {
        "free/auto".to_string()
    }
}

fn bounded_semantic_turns(turns: u32, default: u32) -> u32 {
    if turns == 0 {
        default
    } else {
        turns.clamp(1, clawde_core::config::MAX_SEMANTIC_TURNS)
    }
}

#[cfg(test)]
fn semantic_verify_input_for_config(
    request: &crate::continuation::SemanticVerifyRequest,
    config: &clawde_core::config::Config,
) -> serde_json::Value {
    semantic_verify_input(
        request,
        &semantic_model(config),
        bounded_semantic_turns(
            config.verify.semantic_max_turns,
            clawde_core::config::DEFAULT_SEMANTIC_MAX_TURNS,
        ),
    )
}

fn semantic_verify_input(
    request: &crate::continuation::SemanticVerifyRequest,
    model: &str,
    max_turns: u32,
) -> serde_json::Value {
    let spec = request
        .spec
        .as_ref()
        .and_then(|spec| serde_json::to_string_pretty(spec).ok())
        .unwrap_or_else(|| "null".to_string());
    let changed_files = request
        .changed_files
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join("\\n");
    let prompt = format!(
        "Inspect the current project with read-only tools and assess whether the latest change is semantically correct.\\n\\n\\
         Session: {}\\nTree hash: {}\\nChanged files:\\n{}\\n\\n\\
         Matching accepted spec (JSON):\\n{}\\n\\n\\
         Unified diff (untrusted, bounded):\\n{}\\n\\n\\
         Return ONLY one JSON object with this exact shape: \\
         {{\\\"verdict\\\":\\\"pass\\\"|\\\"fixable\\\"|\\\"replan\\\"|\\\"escalate\\\",\\\"summary\\\":\\\"...\\\",\\\"findings\\\":[\\\"...\\\"]}}.\\n\\
         Do not edit files, run commands, access the network, or include markdown fences.",
        request.session_id, request.tree_hash, changed_files, spec, request.diff
    );
    // Do not trust a caller-provided tool list at this boundary. The semantic
    // runner owns the capability set and always supplies the fixed read-only
    // allowlist.
    serde_json::json!({
        "description": "read-only semantic verification",
        "prompt": prompt,
        "tools": semantic_verifier_tool_names(),
        "system_prompt": "You are a read-only semantic verifier. You may inspect files and search the project, but you must never edit files, execute commands, access the network, or delegate to another agent. Return only the requested JSON verdict.",
        "max_turns": max_turns,
        "model": model
    })
}

/// Build the opt-in semantic verifier runner for the active Free provider.
///
/// The runner deliberately refuses every other provider. It invokes the same
/// nested-agent machinery as `AgentTool`, but passes a fixed allowlist of
/// filesystem read/search tools and a one-shot JSON-only verifier prompt.
/// No runner is returned when the session is not using Clawde's `free` provider.
pub fn semantic_verify_runner(
    ctx: ToolContext,
) -> Option<crate::continuation::SemanticVerifyRunner> {
    if ctx.config.selected_provider_id() != "free" {
        return None;
    }

    let model = semantic_model(&ctx.config);
    let max_turns = bounded_semantic_turns(
        ctx.config.verify.semantic_max_turns,
        clawde_core::config::DEFAULT_SEMANTIC_MAX_TURNS,
    );
    let ctx = Arc::new(ctx);
    Some(Arc::new(
        move |request: crate::continuation::SemanticVerifyRequest| {
            let ctx = ctx.clone();
            let model = model.clone();
            Box::pin(async move {
                let input = semantic_verify_input(&request, &model, max_turns);
                let result = AgentTool.execute(input, &ctx).await;
                if result.is_error {
                    Err(result.content)
                } else {
                    Ok(result.content)
                }
            })
        },
    ))
}

/// Build the AgentTool input JSON for a fresh-executor fix request (G5).
///
/// Unlike the verifier, the fixer gets the file-mutating tools so it can
/// apply the reported fixes, plus the verdict context (summary + findings +
/// spec + bounded diff). The response is a prose change summary — no JSON
/// contract required.
#[cfg(test)]
fn semantic_fix_input_for_config(
    request: &crate::continuation::SemanticFixRequest,
    config: &clawde_core::config::Config,
) -> serde_json::Value {
    semantic_fix_input(
        request,
        &semantic_model(config),
        bounded_semantic_turns(
            config.verify.semantic_fix_max_turns,
            clawde_core::config::DEFAULT_SEMANTIC_FIX_MAX_TURNS,
        ),
    )
}

fn semantic_fix_input(
    request: &crate::continuation::SemanticFixRequest,
    model: &str,
    max_turns: u32,
) -> serde_json::Value {
    let spec = request
        .spec
        .as_ref()
        .and_then(|spec| serde_json::to_string_pretty(spec).ok())
        .unwrap_or_else(|| "null".to_string());
    let changed_files = request
        .changed_files
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join("\\n");
    let findings = if request.findings.is_empty() {
        "(no findings listed)".to_string()
    } else {
        request.findings.join("\\n- ")
    };
    let prompt = format!(
        "A semantic verifier reviewed the latest change and found fixable issues.\\n\\n\\
         Verifier summary: {}\\n\\n\\
         Findings:\\n- {}\\n\\n\\
         Changed files:\\n{}\\n\\n\\
         Matching accepted spec (JSON):\\n{}\\n\\n\\
         Unified diff (untrusted, bounded):\\n{}\\n\\n\\
         Apply the minimal fix that satisfies the spec and resolves every finding. \\
         You may edit files. Do not run commands, access the network, or \\
         delegate to another agent. When done, return a short summary of the \\
         changes you made.",
        request.summary, findings, changed_files, spec, request.diff
    );
    // The fixer owns its capability set: read-only tools + the file-mutating
    // tools needed to apply fixes. Shell/network tools are never available.
    serde_json::json!({
        "description": "apply semantic-verifier fixes",
        "prompt": prompt,
        "tools": crate::continuation::semantic_fixer_tool_names(),
        "system_prompt": "You are a code-fixing executor. You may read, search, and edit files in the project, but you must never run commands, access the network, or delegate to another agent. Apply the minimal fix for each reported finding, then summarize the changes you made.",
        "max_turns": max_turns,
        "model": model
    })
}

/// Build the opt-in fresh-executor fixer for the active Free provider (G5).
///
/// Mirrors `semantic_verify_runner`: gated to `free`, builds a fixed
/// read+write AgentInput via `semantic_fix_input`, and runs it through the
/// same nested AgentTool machinery. No runner is returned for other
/// providers.
pub fn semantic_fix_runner(ctx: ToolContext) -> Option<crate::continuation::SemanticFixRunner> {
    if ctx.config.selected_provider_id() != "free" {
        return None;
    }

    let model = semantic_model(&ctx.config);
    let max_turns = bounded_semantic_turns(
        ctx.config.verify.semantic_fix_max_turns,
        clawde_core::config::DEFAULT_SEMANTIC_FIX_MAX_TURNS,
    );
    let ctx = Arc::new(ctx);
    Some(Arc::new(
        move |request: crate::continuation::SemanticFixRequest| {
            let ctx = ctx.clone();
            let model = model.clone();
            Box::pin(async move {
                let input = semantic_fix_input(&request, &model, max_turns);
                let result = AgentTool.execute(input, &ctx).await;
                if result.is_error {
                    Err(result.content)
                } else {
                    Ok(result.content)
                }
            })
        },
    ))
}

fn build_model_registry() -> ModelRegistry {
    let mut registry = ModelRegistry::new();
    if let Some(cache_dir) = dirs::cache_dir() {
        let cache_path = cache_dir.join("clawde").join("models_dev.json");
        registry.load_cache(&cache_path);
    }
    registry
}

fn resolve_subagent_model(params: &AgentInput, ctx: &ToolContext) -> String {
    let base_model = params
        .model
        .clone()
        .filter(|m| !m.is_empty())
        .or_else(|| {
            ctx.managed_agent_config
                .as_ref()
                .map(|c| c.executor_model.clone())
                .filter(|m| !m.is_empty())
        })
        .unwrap_or_else(|| ctx.config.effective_model().to_string());

    if base_model.contains('/') {
        base_model
    } else {
        let provider_id = ctx.config.selected_provider_id();
        if provider_id != "anthropic" {
            format!("{}/{}", provider_id, base_model)
        } else {
            base_model
        }
    }
}

#[derive(Debug, Deserialize)]
struct AgentInput {
    /// Short description of the agent's task (used for logging).
    description: String,
    /// The complete task prompt to send as the first user message.
    prompt: String,
    /// Optional: which tools to make available (defaults to all minus AgentTool).
    #[serde(default)]
    tools: Option<Vec<String>>,
    /// Optional: system prompt override for the sub-agent.
    #[serde(default)]
    system_prompt: Option<String>,
    /// Optional: max turns for the sub-agent (default 10).
    #[serde(default)]
    max_turns: Option<u32>,
    /// Optional: model override for this sub-agent.
    #[serde(default)]
    model: Option<String>,
    /// Set to "worktree" to run the agent in an isolated git worktree.
    /// Omit (or set to null) for shared working directory.
    #[serde(default)]
    isolation: Option<String>,
    /// If true, start the agent in the background and return agent_id immediately.
    /// Default: false (wait for completion).
    #[serde(default)]
    run_in_background: bool,
}

fn allowlisted_tool_name(allowed: &[String], tool_name: &str) -> bool {
    allowed
        .iter()
        .any(|requested| requested.trim().eq_ignore_ascii_case(tool_name))
}

#[async_trait]
impl Tool for AgentTool {
    fn name(&self) -> &str {
        clawde_core::constants::TOOL_NAME_AGENT
    }

    fn description(&self) -> &str {
        "Launch a new agent to handle complex, multi-step tasks autonomously. \
         The agent runs its own agentic loop with access to tools and returns \
         its final result. Use this to delegate sub-tasks, run parallel \
         workstreams, or handle tasks that require many tool calls."
    }

    fn permission_level(&self) -> PermissionLevel {
        // The agent inherits parent permissions; no extra level required.
        PermissionLevel::None
    }

    fn network_capable(&self) -> bool {
        // A sub-agent can otherwise reconstruct a full tool set and reach
        // providers or network-capable tools outside the parent call.
        true
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "Short description of the agent's task (3-5 words)"
                },
                "prompt": {
                    "type": "string",
                    "description": "The complete task for the agent to perform"
                },
                "tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "List of tool names to make available. Defaults to all tools."
                },
                "system_prompt": {
                    "type": "string",
                    "description": "Optional system prompt override for the sub-agent"
                },
                "max_turns": {
                    "type": "number",
                    "description": "Maximum number of turns for the sub-agent (default 10)"
                },
                "model": {
                    "type": "string",
                    "description": "Optional model to use for this agent"
                },
                "isolation": {
                    "type": "string",
                    "enum": ["worktree"],
                    "description": "Set to \"worktree\" to run the agent in an isolated git worktree. \
                                    Prevents file-edit conflicts when multiple agents run in parallel."
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "If true, the agent starts immediately and this call returns an \
                                    agent_id without waiting for completion. Use the monitor tool \
                                    with action=status/output and task_id=agent_id. Default: false."
                }
            },
            "required": ["description", "prompt"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        // AgentTool can be called directly by semantic runners and other
        // internal paths, so do not rely solely on the outer dispatcher.
        if let Err(e) = ctx.ensure_network_allowed_for_tool(self.name(), true) {
            return ToolResult::error(e.to_string());
        }

        let params: AgentInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        info!(description = %params.description, "Spawning sub-agent");

        let anthropic_key = ctx.config.resolve_anthropic_api_key().unwrap_or_default();
        let anthropic_base = ctx.config.resolve_anthropic_api_base();
        let client = match AnthropicClient::new(ClientConfig {
            api_key: anthropic_key.clone(),
            api_base: anthropic_base,
            ..Default::default()
        }) {
            Ok(c) => Arc::new(c),
            Err(e) => return ToolResult::error(format!("Failed to create client: {}", e)),
        };

        let provider_registry = ProviderRegistry::from_config(
            &ctx.config,
            ClientConfig {
                api_key: anthropic_key,
                api_base: ctx.config.resolve_anthropic_api_base(),
                ..Default::default()
            },
        );
        let model_registry = Arc::new(build_model_registry());

        // Build the tool list for the sub-agent.
        // Always exclude AgentTool itself to prevent unbounded recursion.
        let agent_tools = build_agent_tools(params.tools.as_deref(), true);

        // Resolve model: explicit override > managed config executor model > provider default.
        let model = resolve_subagent_model(&params, ctx);

        let system_prompt = params.system_prompt.unwrap_or_else(|| {
            let mut prompt = "You are a specialized AI agent helping with a specific sub-task. \
             Complete the task thoroughly and return your findings."
                .to_string();

            // Append plugin-contributed agent definitions so the sub-agent
            // is aware of any specialised agents declared by plugins.
            if let Some(registry) = clawde_plugins::global_plugin_registry() {
                let mut agent_defs = String::new();
                for agent_dir in registry.all_agent_paths() {
                    if let Ok(entries) = std::fs::read_dir(&agent_dir) {
                        for entry in entries.flatten() {
                            let p = entry.path();
                            if p.extension().is_some_and(|e| e == "md") {
                                if let Ok(content) = std::fs::read_to_string(&p) {
                                    let name =
                                        p.file_stem().and_then(|s| s.to_str()).unwrap_or("agent");
                                    agent_defs.push_str(&format!(
                                        "\n\n## Agent: {}\n{}",
                                        name,
                                        content.trim()
                                    ));
                                }
                            }
                        }
                    }
                }
                if !agent_defs.is_empty() {
                    prompt.push_str("\n\nThe following specialized agents are available:");
                    prompt.push_str(&agent_defs);
                }
            }

            prompt
        });

        // Resolve max_turns: explicit > managed config executor_max_turns > default.
        let resolved_max_turns = params.max_turns.unwrap_or_else(|| {
            ctx.managed_agent_config
                .as_ref()
                .map(|c| c.executor_max_turns)
                .unwrap_or(10)
        });

        // Resolve isolation: explicit param > managed config executor_isolation.
        let resolved_isolation = params.isolation.clone().or_else(|| {
            if ctx
                .managed_agent_config
                .as_ref()
                .map(|c| c.executor_isolation)
                .unwrap_or(false)
            {
                Some("worktree".to_string())
            } else {
                None
            }
        });

        // -----------------------------------------------------------------------
        // Determine working directory - optionally isolate in a git worktree.
        // -----------------------------------------------------------------------
        let use_isolation = resolved_isolation.as_deref() == Some("worktree");
        let agent_id = uuid::Uuid::new_v4().to_string();

        let (working_dir_str, worktree_path, git_root): (String, Option<PathBuf>, Option<PathBuf>) =
            if use_isolation {
                let git_root = find_git_root(&ctx.working_dir);
                if let Some(ref root) = git_root {
                    if let Some(wt) = create_worktree(root, &agent_id).await {
                        let wd = wt.display().to_string();
                        (wd, Some(wt), git_root)
                    } else {
                        warn!(
                            agent_id = %agent_id,
                            "Worktree creation failed; running agent in shared working directory"
                        );
                        (ctx.working_dir.display().to_string(), None, None)
                    }
                } else {
                    warn!(
                        agent_id = %agent_id,
                        "No git root found; isolation=worktree ignored"
                    );
                    (ctx.working_dir.display().to_string(), None, None)
                }
            } else {
                (ctx.working_dir.display().to_string(), None, None)
            };

        let query_config = QueryConfig {
            model,
            max_tokens: clawde_core::constants::DEFAULT_MAX_TOKENS,
            max_turns: resolved_max_turns,
            system_prompt: Some(system_prompt),
            append_system_prompt: None,
            output_style: ctx.config.effective_output_style(),
            output_style_prompt: ctx.config.resolve_output_style_prompt(),
            working_directory: Some(working_dir_str),
            thinking_budget: None,
            memory_max_tokens: None,
            memory_enabled: None,
            temperature: None,
            tool_result_budget: 50_000,
            effort_level: None,
            command_queue: None,
            skill_index: None,
            max_budget_usd: None,
            fallback_model: None,
            provider_registry: Some(Arc::new(provider_registry)),
            agent_name: None,
            agent_definition: None,
            model_registry: Some(model_registry),
            managed_agents: None,
            // Progressive tool disclosure (issue #233): the sub-agent's system
            // prompt only needs guideline blocks for the tools it actually has.
            enabled_tools: Some(agent_tools.iter().map(|t| t.name().to_string()).collect()),
            // Sub-agents run to their own completion and never drive goal
            // continuation — stop after one turn like every non-goal run.
            continuation: crate::continuation::ContinuationMode::Default,
            semantic_verify_runner: None,
            semantic_fix_runner: None,
        };
        // -----------------------------------------------------------------------
        // Background mode: spawn and return agent_id immediately.
        // -----------------------------------------------------------------------
        if params.run_in_background {
            let mut task = clawde_core::tasks::BackgroundTask::new(format!(
                "subagent: {}",
                params.description
            ));
            task.id = agent_id.clone();
            // Cancellation token shared between the registry and the spawned
            // sub-agent loop: signalling it via TaskRegistry::cancel (e.g. from a
            // monitor cancel) actually stops the loop instead of only relabeling
            // the task (issue #219). Derive it as a CHILD of the parent's token
            // so cancelling the parent query also cancels this sub-agent, while
            // the registry can still cancel this sub-agent independently (#218).
            let cancel = ctx.cancel_token.child_token();
            task.cancel_token = Some(cancel.clone());
            let _ = clawde_core::tasks::global_registry().register(task);

            // Re-create the tool list inside the closure so it is owned and Send.
            let agent_tools_bg = build_agent_tools(None, true);

            let client_bg = client.clone();
            let ctx_bg = ctx.clone();
            let config_bg = query_config.clone();
            let cost_tracker_bg = ctx.cost_tracker.clone();
            let description_bg = params.description.clone();
            let prompt_bg = params.prompt.clone();
            let agent_id_bg = agent_id.clone();

            tokio::spawn(async move {
                let mut messages = vec![Message::user(prompt_bg)];
                let outcome = run_query_loop(
                    client_bg.as_ref(),
                    &mut messages,
                    &agent_tools_bg,
                    &ctx_bg,
                    &config_bg,
                    cost_tracker_bg,
                    None,
                    cancel,
                    None,
                )
                .await;

                // Cleanup worktree if one was created.
                if let (Some(root), Some(wt)) = (git_root, worktree_path) {
                    remove_worktree(&root, &wt).await;
                }

                // Respect a prior external cancellation mark from monitor cancel.
                let cancelled = matches!(
                    clawde_core::tasks::global_registry()
                        .get(&agent_id_bg)
                        .map(|t| t.status),
                    Some(clawde_core::tasks::TaskStatus::Cancelled)
                );

                let result_text = format_outcome(outcome);
                clawde_core::tasks::global_registry().append_output(&agent_id_bg, &result_text);

                if !cancelled {
                    let status = if result_text.starts_with("[Agent error:")
                        || result_text.starts_with("[Agent stopped:")
                    {
                        clawde_core::tasks::TaskStatus::Failed(result_text.clone())
                    } else {
                        clawde_core::tasks::TaskStatus::Completed
                    };
                    clawde_core::tasks::global_registry().update_status(&agent_id_bg, status);
                }

                debug!(
                    agent_id = %agent_id_bg,
                    description = %description_bg,
                    "Background agent completed"
                );
            });

            return ToolResult::success(
                serde_json::json!({
                    "agent_id": agent_id,
                    "status": "running",
                    "message": format!(
                        "Agent '{}' started in background. Use monitor with action=status/output and task_id='{}'.",
                        params.description, agent_id
                    )
                })
                .to_string(),
            );
        }

        // -----------------------------------------------------------------------
        // Synchronous mode: run the sub-agent loop and wait for completion.
        // -----------------------------------------------------------------------
        let mut messages = vec![Message::user(params.prompt)];
        // Derive the sub-agent's token as a CHILD of the parent's so a parent
        // cancel propagates into this sub-agent's own run_query_loop (issue #218).
        let cancel = ctx.cancel_token.child_token();

        let outcome = run_query_loop(
            client.as_ref(),
            &mut messages,
            &agent_tools,
            ctx,
            &query_config,
            ctx.cost_tracker.clone(),
            None, // no event forwarding for sub-agents
            cancel,
            None, // no pending message queue for sub-agents
        )
        .await;

        // Cleanup worktree if one was created.
        if let (Some(root), Some(wt)) = (git_root, worktree_path) {
            remove_worktree(&root, &wt).await;
        }

        match outcome {
            QueryOutcome::EndTurn { message, usage } => {
                let text = message.get_all_text();
                debug!(
                    description = %params.description,
                    output_tokens = usage.output_tokens,
                    "Sub-agent completed"
                );
                ToolResult::success(text)
            }
            QueryOutcome::MaxTokens {
                partial_message, ..
            } => {
                let text = partial_message.get_all_text();
                ToolResult::success(format!("{}\n\n[Note: Agent hit max_tokens limit]", text))
            }
            QueryOutcome::Cancelled => ToolResult::error("Sub-agent was cancelled".to_string()),
            QueryOutcome::Error(e) => ToolResult::error(format!("Sub-agent error: {}", e)),
            QueryOutcome::BudgetExceeded {
                cost_usd,
                limit_usd,
            } => ToolResult::error(format!(
                "Sub-agent stopped: budget ${:.4} exceeded (limit ${:.4})",
                cost_usd, limit_usd
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: convert a QueryOutcome into a result string for background agents
// ---------------------------------------------------------------------------

fn format_outcome(outcome: QueryOutcome) -> String {
    match outcome {
        QueryOutcome::EndTurn { message, .. } => message.get_all_text(),
        QueryOutcome::MaxTokens {
            partial_message, ..
        } => format!(
            "{}\n\n[Note: Agent hit max_tokens limit]",
            partial_message.get_all_text()
        ),
        QueryOutcome::Cancelled => "[Agent was cancelled]".to_string(),
        QueryOutcome::Error(e) => format!("[Agent error: {}]", e),
        QueryOutcome::BudgetExceeded {
            cost_usd,
            limit_usd,
        } => format!(
            "[Agent stopped: budget ${:.4} exceeded (limit ${:.4})]",
            cost_usd, limit_usd
        ),
    }
}

// ---------------------------------------------------------------------------
// Team swarm runner injection
// ---------------------------------------------------------------------------
//
// Called once at process startup (e.g. from main.rs) to inject a real agent
// runner into cc-tools so that TeamCreateTool can spawn sub-agents via
// run_query_loop without creating a circular crate dependency.

/// Register the cc-query-backed agent runner with cc-tools.
///
/// After this call, `TeamCreateTool` will actually invoke `run_query_loop` for
/// each agent instead of returning stub output.
///
/// # Panics
/// Panics if the runner was already registered.
pub fn init_team_swarm_runner() {
    let runner: clawde_tools::AgentRunFn = Arc::new(
        |description: String,
         prompt: String,
         tools: Option<Vec<String>>,
         system: Option<String>,
         max_turns: Option<u32>,
         ctx: Arc<clawde_tools::ToolContext>| {
            // We must return a Pin<Box<dyn Future<...> + Send>>.
            Box::pin(async move {
                let anthropic_key = ctx.config.resolve_anthropic_api_key().unwrap_or_default();
                let anthropic_base = ctx.config.resolve_anthropic_api_base();
                let client =
                    match clawde_api::AnthropicClient::new(clawde_api::client::ClientConfig {
                        api_key: anthropic_key.clone(),
                        api_base: anthropic_base,
                        ..Default::default()
                    }) {
                        Ok(c) => Arc::new(c),
                        Err(e) => {
                            return format!(
                                "[Agent '{}' failed to create client: {}]",
                                description, e
                            )
                        }
                    };

                let provider_registry = ProviderRegistry::from_config(
                    &ctx.config,
                    clawde_api::client::ClientConfig {
                        api_key: anthropic_key,
                        api_base: ctx.config.resolve_anthropic_api_base(),
                        ..Default::default()
                    },
                );
                let model_registry = Arc::new(build_model_registry());

                // Build the tool list, filtering to the allowlist if provided.
                let agent_tools = build_agent_tools(tools.as_deref(), true);

                let model = resolve_subagent_model(
                    &AgentInput {
                        description: description.clone(),
                        prompt: prompt.clone(),
                        tools: tools.clone(),
                        system_prompt: system.clone(),
                        max_turns,
                        model: None,
                        isolation: None,
                        run_in_background: false,
                    },
                    &ctx,
                );

                let system_prompt = system.unwrap_or_else(|| {
                    "You are a specialized AI agent helping with a specific sub-task. \
                     Complete the task thoroughly and return your findings."
                        .to_string()
                });

                let query_config = crate::QueryConfig {
                    model,
                    max_tokens: clawde_core::constants::DEFAULT_MAX_TOKENS,
                    max_turns: max_turns.unwrap_or(10),
                    system_prompt: Some(system_prompt),
                    working_directory: Some(ctx.working_dir.display().to_string()),
                    output_style: ctx.config.effective_output_style(),
                    output_style_prompt: ctx.config.resolve_output_style_prompt(),
                    provider_registry: Some(Arc::new(provider_registry)),
                    model_registry: Some(model_registry),
                    // Progressive tool disclosure (issue #233): only emit
                    // per-tool guidance for tools this team sub-agent has.
                    enabled_tools: Some(agent_tools.iter().map(|t| t.name().to_string()).collect()),
                    ..Default::default()
                };

                // Child of the parent's token so a parent cancel propagates into
                // this team sub-agent as well (issue #218).
                let cancel = ctx.cancel_token.child_token();
                let mut messages = vec![clawde_core::types::Message::user(prompt)];
                let outcome = crate::run_query_loop(
                    client.as_ref(),
                    &mut messages,
                    &agent_tools,
                    &ctx,
                    &query_config,
                    ctx.cost_tracker.clone(),
                    None,
                    cancel,
                    None,
                )
                .await;

                format_outcome(outcome)
            }) as Pin<Box<dyn std::future::Future<Output = String> + Send>>
        },
    );

    clawde_tools::register_agent_runner(runner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::continuation::semantic_fixer_tool_names;
    use crate::continuation::semantic_read_only_tool_names;
    use crate::continuation::{SemanticFixRequest, SemanticVerifyRequest};

    fn test_context(config: clawde_core::config::Config) -> ToolContext {
        ToolContext {
            working_dir: std::env::temp_dir(),
            permission_mode: clawde_core::config::PermissionMode::Default,
            permission_handler: Arc::new(clawde_core::permissions::AutoPermissionHandler {
                mode: clawde_core::config::PermissionMode::BypassPermissions,
            }),
            cost_tracker: clawde_core::cost::CostTracker::new(),
            session_id: "semantic-verify-test".to_string(),
            file_history: Arc::new(parking_lot::Mutex::new(
                clawde_core::file_history::FileHistory::new(),
            )),
            current_turn: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            non_interactive: true,
            mcp_manager: None,
            config,
            provider_registry: None,
            managed_agent_config: None,
            completion_notifier: None,
            pending_permissions: None,
            permission_manager: None,
            user_question_tx: None,
            cancel_token: tokio_util::sync::CancellationToken::new(),
        }
    }

    fn sample_request() -> SemanticVerifyRequest {
        SemanticVerifyRequest {
            session_id: "session-9".to_string(),
            working_dir: std::path::PathBuf::from("/project"),
            changed_files: vec![std::path::PathBuf::from("/project/src/lib.rs")],
            tree_hash: "tree-abc".to_string(),
            diff: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@\n+fn added() {}".to_string(),
            task_id: Some("task-1".to_string()),
            spec: Some(clawde_core::spec::Spec {
                title: "Fixture".to_string(),
                requirements: vec!["sum_pair(1, 2) == 3".to_string()],
                ..Default::default()
            }),
            // Adversarial: a caller-supplied tool list must never be trusted at
            // this boundary. Passing a divergent set pins that invariant.
            read_only_tools: vec!["Write".to_string(), "Bash".to_string()],
        }
    }

    #[test]
    fn allowlist_matches_tool_names_case_insensitively_and_ignores_whitespace() {
        let allowed = vec![" bash ".to_string(), "Read".to_string()];
        assert!(allowlisted_tool_name(&allowed, "Bash"));
        assert!(allowlisted_tool_name(&allowed, "read"));
        assert!(!allowlisted_tool_name(&allowed, "Write"));
    }

    #[test]
    fn semantic_verify_runner_refuses_non_free_providers() {
        let config = clawde_core::config::Config {
            provider: Some("anthropic".to_string()),
            ..Default::default()
        };
        let ctx = test_context(config);
        assert!(
            semantic_verify_runner(ctx).is_none(),
            "runner must refuse non-free providers"
        );
    }

    #[test]
    fn semantic_verify_runner_available_for_default_free_config() {
        // Default config (provider unset) routes to the free composite provider.
        let ctx = test_context(clawde_core::config::Config::default());
        assert!(
            semantic_verify_runner(ctx).is_some(),
            "runner must be available for the free provider"
        );
    }

    #[test]
    fn semantic_config_defaults_are_bounded_and_free() {
        let config = clawde_core::config::VerifyConfig::default();
        assert_eq!(config.semantic_model, "free/auto");
        assert_eq!(config.semantic_max_turns, 3);
        assert_eq!(config.semantic_fix_max_turns, 5);
        assert_eq!(config.semantic_max_attempts, 3);
        assert!(config.semantic_max_turns <= clawde_core::config::MAX_SEMANTIC_TURNS);
        assert!(config.semantic_fix_max_turns <= clawde_core::config::MAX_SEMANTIC_TURNS);
        assert!(config.semantic_max_attempts <= clawde_core::config::MAX_SEMANTIC_ATTEMPTS);
    }

    #[test]
    fn semantic_config_override_round_trips_and_invalid_model_falls_back() {
        let mut config = clawde_core::config::Config::default();
        config.verify.semantic_model = "anthropic/secret-model".to_string();
        config.verify.semantic_max_turns = 99;
        config.verify.semantic_fix_max_turns = 0;
        let serialized = serde_json::to_value(&config.verify).expect("serialize verify config");
        let decoded: clawde_core::config::VerifyConfig =
            serde_json::from_value(serialized).expect("deserialize verify config");
        assert_eq!(decoded.semantic_model, "anthropic/secret-model");
        assert_eq!(decoded.semantic_max_turns, 99);
        assert_eq!(semantic_model(&config), "free/auto");
        assert_eq!(bounded_semantic_turns(decoded.semantic_max_turns, 3), 10);
        assert_eq!(bounded_semantic_turns(decoded.semantic_fix_max_turns, 5), 5);
        assert_eq!(bounded_semantic_turns(decoded.semantic_max_attempts, 3), 3);
    }

    #[test]
    fn semantic_config_values_reach_verifier_and_fixer_inputs() {
        let mut config = clawde_core::config::Config::default();
        config.verify.semantic_model = "free/openai/gpt-oss-120b".to_string();
        config.verify.semantic_max_turns = 7;
        config.verify.semantic_fix_max_turns = 8;
        let verify_input = semantic_verify_input_for_config(&sample_request(), &config);
        let fix_input = semantic_fix_input_for_config(&sample_fix_request(), &config);
        assert_eq!(verify_input["model"], "free/openai/gpt-oss-120b");
        assert_eq!(verify_input["max_turns"], 7);
        assert_eq!(fix_input["model"], "free/openai/gpt-oss-120b");
        assert_eq!(fix_input["max_turns"], 8);
    }

    #[test]
    fn semantic_config_accepts_explicit_free_model_route_and_rejects_empty_suffix() {
        let mut config = clawde_core::config::Config::default();
        config.verify.semantic_model = "free/openai/gpt-oss-120b".to_string();
        assert_eq!(semantic_model(&config), "free/openai/gpt-oss-120b");
        config.verify.semantic_model = "free/".to_string();
        assert_eq!(semantic_model(&config), "free/auto");
    }

    #[test]
    fn semantic_verify_input_carries_read_only_allowlist_and_json_contract() {
        let request = sample_request();
        let input = semantic_verify_input(&request, "free/auto", 3);

        // Fixed routing + budget: the verifier is a one-shot free-model agent.
        assert_eq!(input["model"], serde_json::json!("free/auto"));
        assert_eq!(input["max_turns"], serde_json::json!(3));
        assert_eq!(
            input["description"],
            serde_json::json!("read-only semantic verification")
        );

        // Tools: the exact read-only allowlist, never a caller-supplied set.
        let tools: Vec<String> = input["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|v| v.as_str().expect("tool name").to_string())
            .collect();
        assert_eq!(tools, semantic_read_only_tool_names());
        assert!(!tools.iter().any(|name| name == "Write"));
        assert!(!tools.iter().any(|name| name == "Bash"));

        // The system prompt must lock the agent to read-only, no-network, no-delegate.
        let system = input["system_prompt"].as_str().expect("system prompt");
        assert!(system.contains("never edit files"));
        assert!(system.contains("access the network"));
        assert!(system.contains("delegate to another agent"));

        // The prompt must carry the request context and the strict JSON contract.
        let prompt = input["prompt"].as_str().expect("prompt");
        assert!(prompt.contains("session-9"));
        assert!(prompt.contains("tree-abc"));
        assert!(prompt.contains("src/lib.rs"));
        assert!(prompt.contains("fn added"));
        assert!(prompt.contains("sum_pair(1, 2) == 3"));
        // The JSON shape is escaped (\"verdict\") inside the prompt string.
        assert!(prompt.contains("\\\"verdict\\\""));
        assert!(prompt.contains("\\\"fixable\\\""));
        assert!(prompt.contains("Do not edit files"));
        assert!(prompt.contains("include markdown fences"));
    }

    #[test]
    fn semantic_verify_input_survives_missing_spec() {
        let mut request = sample_request();
        request.spec = None;
        let input = semantic_verify_input(&request, "free/auto", 3);
        assert!(input["prompt"].as_str().unwrap().contains("null"));
    }

    fn sample_fix_request() -> SemanticFixRequest {
        SemanticFixRequest {
            session_id: "session-9".to_string(),
            working_dir: std::path::PathBuf::from("/project"),
            changed_files: vec![std::path::PathBuf::from("/project/src/lib.rs")],
            tree_hash: "tree-abc".to_string(),
            diff: "--- a/src/lib.rs\n+++ b/src/lib.rs\n+fn added() {}".to_string(),
            task_id: Some("task-1".to_string()),
            spec: Some(clawde_core::spec::Spec {
                title: "Fixture".to_string(),
                requirements: vec!["sum_pair(1, 2) == 3".to_string()],
                ..Default::default()
            }),
            summary: "sum_pair returns a constant 0".to_string(),
            findings: vec!["sum_pair returns 0 regardless of inputs".to_string()],
        }
    }

    #[test]
    fn semantic_fix_runner_refuses_non_free_providers() {
        let config = clawde_core::config::Config {
            provider: Some("anthropic".to_string()),
            ..Default::default()
        };
        let ctx = test_context(config);
        assert!(semantic_fix_runner(ctx).is_none());
    }

    #[test]
    fn semantic_fix_runner_available_for_default_free_config() {
        let ctx = test_context(clawde_core::config::Config::default());
        assert!(semantic_fix_runner(ctx).is_some());
    }

    #[test]
    fn semantic_fix_input_carries_verdict_context_and_write_tools() {
        let request = sample_fix_request();
        let input = semantic_fix_input(&request, "free/auto", 5);

        assert_eq!(input["model"], serde_json::json!("free/auto"));
        assert_eq!(input["max_turns"], serde_json::json!(5));
        assert_eq!(
            input["description"],
            serde_json::json!("apply semantic-verifier fixes")
        );

        // The fixer gets the write tools it needs to apply fixes — unlike the
        // read-only verifier.
        let tools: Vec<String> = input["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|v| v.as_str().expect("tool name").to_string())
            .collect();
        assert_eq!(tools, semantic_fixer_tool_names());
        assert!(tools.iter().any(|name| name == "Write"));
        assert!(tools.iter().any(|name| name == "Edit"));
        assert!(tools.iter().any(|name| name == "Read"));
        // No shell/network: the executor repairs files, it does not run commands.
        assert!(!tools.iter().any(|name| name == "Bash"));

        // Verdict context must reach the executor.
        let prompt = input["prompt"].as_str().expect("prompt");
        assert!(prompt.contains("sum_pair returns a constant 0"));
        assert!(prompt.contains("sum_pair returns 0 regardless of inputs"));
        assert!(prompt.contains("src/lib.rs"));
        assert!(prompt.contains("fn added"));
        assert!(prompt.contains("sum_pair(1, 2) == 3"));
        assert!(prompt.contains("Apply the minimal fix"));
        assert!(prompt.contains("Do not run commands"));
    }
}
