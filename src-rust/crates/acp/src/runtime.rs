//! Shared agent runtime owned by the ACP server.
//!
//! Built once on startup and reused for every session. Per-session state
//! (cwd, transcript, cancellation token, permission queue) is layered on
//! top via `sessions::SessionState`.

use std::path::PathBuf;
use std::sync::Arc;

use clawde_core::config::{Config, Settings};
use clawde_core::CostTracker;
use clawde_query::QueryConfig;
use clawde_tools::Tool;

/// Snapshot of the global agent runtime — built at server startup, cloned
/// (cheaply, via Arc) into each session.
#[derive(Clone)]
pub struct AgentRuntime {
    pub config: Config,
    pub settings: Settings,
    pub api_client: Arc<clawde_api::AnthropicClient>,
    pub provider_registry: Arc<clawde_api::ProviderRegistry>,
    pub tools: Arc<Vec<Box<dyn Tool>>>,
    pub cost_tracker: Arc<CostTracker>,
    pub query_config: QueryConfig,
    pub mcp_manager: Option<Arc<clawde_mcp::McpManager>>,
    pub working_dir: PathBuf,
}

impl AgentRuntime {
    /// Build a tool vector for one ACP session.
    ///
    /// Ordinary sessions reuse the startup snapshot. A session with its own
    /// MCP manager receives fresh built-in instances plus wrappers bound to
    /// that manager, so dynamic MCP tools cannot leak into another session.
    pub fn tools_for_session(
        &self,
        session_mcp: Option<Arc<clawde_mcp::McpManager>>,
    ) -> Arc<Vec<Box<dyn Tool>>> {
        let Some(manager) = session_mcp else {
            return self.tools.clone();
        };
        let network_blocked = clawde_core::network_isolation_enabled(&self.config);
        let mut tools = builtin_tools_for_config(&self.config);
        let agent_allowed = self.config.allowed_tools.is_empty()
            || self
                .config
                .allowed_tools
                .iter()
                .any(|name| name.eq_ignore_ascii_case(clawde_core::constants::TOOL_NAME_AGENT));
        let agent_denied = self
            .config
            .disallowed_tools
            .iter()
            .any(|name| name.eq_ignore_ascii_case(clawde_core::constants::TOOL_NAME_AGENT));
        if agent_allowed && !agent_denied && !network_blocked {
            tools.push(Box::new(clawde_query::AgentTool::default()));
        }
        if !network_blocked {
            tools.extend(
                clawde_tools::mcp_tool_wrappers(manager)
                    .into_iter()
                    .filter(|tool| {
                        (self.config.allowed_tools.is_empty()
                            || self
                                .config
                                .allowed_tools
                                .iter()
                                .any(|name| name.eq_ignore_ascii_case(tool.name())))
                            && !self
                                .config
                                .disallowed_tools
                                .iter()
                                .any(|name| name.eq_ignore_ascii_case(tool.name()))
                    }),
            );
        }
        Arc::new(tools)
    }

    /// Build the runtime from on-disk settings, env vars, and a working
    /// directory. Mirrors the headless startup path but with ACP-specific
    /// defaults (non-interactive, permission decisions routed back to the
    /// connected client).
    pub async fn build(working_dir: PathBuf) -> anyhow::Result<Self> {
        let settings = Settings::load_sync().unwrap_or_default();
        let mut config = settings.effective_config();
        clawde_core::set_ollama_network_blocked(
            config.resolve_ollama_mode() == clawde_core::OllamaMode::Isolated,
        );
        // Plan mode requires interactive UI — fall back to Default so the
        // ACP permission bridge can route decisions to the client.
        if config.permission_mode == clawde_core::PermissionMode::Plan {
            config.permission_mode = clawde_core::PermissionMode::Default;
        }
        config.project_dir = Some(working_dir.clone());

        let active_provider = config.selected_provider_id().to_string();
        let (api_key, use_bearer_auth) = if active_provider == "anthropic" {
            config
                .resolve_anthropic_auth_async()
                .await
                .unwrap_or_default()
        } else {
            (String::new(), false)
        };

        let client_config = clawde_api::client::ClientConfig {
            api_key: api_key.clone(),
            api_base: config.resolve_anthropic_api_base(),
            use_bearer_auth,
            ..Default::default()
        };
        let api_client = Arc::new(clawde_api::AnthropicClient::new(client_config.clone())?);
        let provider_registry = Arc::new(clawde_api::ProviderRegistry::from_config(
            &config,
            client_config,
        ));

        let cost_tracker = CostTracker::new();

        // Global MCP servers from settings connect upfront so their tools are
        // visible to every session. Per-session MCP servers supplied via
        // `session/new` are connected separately by the session-owned MCP
        // context in the ACP dispatcher.
        let mcp_manager = build_mcp_manager(&config, &settings, &working_dir).await;

        // Build tools: built-ins + AgentTool + trusted configured MCP tools.
        // MCP wrappers are treated as network-capable and are omitted in
        // isolated Ollama mode, matching the CLI registry boundary.
        let network_blocked = clawde_core::network_isolation_enabled(&config);
        let mut tools = builtin_tools_for_config(&config);
        let agent_allowed = config.allowed_tools.is_empty()
            || config
                .allowed_tools
                .iter()
                .any(|name| name.eq_ignore_ascii_case(clawde_core::constants::TOOL_NAME_AGENT));
        let agent_denied = config
            .disallowed_tools
            .iter()
            .any(|name| name.eq_ignore_ascii_case(clawde_core::constants::TOOL_NAME_AGENT));
        if agent_allowed && !agent_denied && !network_blocked {
            tools.push(Box::new(clawde_query::AgentTool::default()));
        }
        if !network_blocked {
            if let Some(manager) = &mcp_manager {
                tools.extend(
                    clawde_tools::mcp_tool_wrappers(manager.clone())
                        .into_iter()
                        .filter(|tool| {
                            (config.allowed_tools.is_empty()
                                || config
                                    .allowed_tools
                                    .iter()
                                    .any(|name| name.eq_ignore_ascii_case(tool.name())))
                                && !config
                                    .disallowed_tools
                                    .iter()
                                    .any(|name| name.eq_ignore_ascii_case(tool.name()))
                        }),
                );
            }
        }
        let tools = Arc::new(tools);

        let mut query_config = QueryConfig::from_config(&config);
        query_config.working_directory = Some(working_dir.display().to_string());
        query_config.provider_registry = Some(provider_registry.clone());

        Ok(Self {
            config,
            settings,
            api_client,
            provider_registry,
            tools,
            cost_tracker,
            query_config,
            mcp_manager,
            working_dir,
        })
    }
}

fn builtin_tools_for_config(config: &Config) -> Vec<Box<dyn Tool>> {
    let network_blocked = clawde_core::network_isolation_enabled(config);
    clawde_tools::all_tools()
        .into_iter()
        .filter(|tool| {
            (!network_blocked
                || !tool.network_capable()
                || tool.available_in_ollama_isolated_mode())
                && (config.allowed_tools.is_empty()
                    || config
                        .allowed_tools
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(tool.name())))
                && !config
                    .disallowed_tools
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(tool.name()))
        })
        .collect()
}

async fn build_mcp_manager(
    config: &Config,
    settings: &Settings,
    working_dir: &std::path::Path,
) -> Option<Arc<clawde_mcp::McpManager>> {
    if config.mcp_servers.is_empty() {
        return None;
    }
    // SECURITY (issue #123): never auto-launch project-defined MCP servers in
    // this non-interactive runtime unless they have been trusted. The ACP
    // runtime loads only global settings today (so all servers are user-origin
    // and pass through), but gating here keeps the invariant if project config
    // is ever merged in.
    let project_root = clawde_core::mcp_trust::project_root_for(working_dir);
    let store = clawde_core::mcp_trust::McpTrustStore::load();
    let decision = clawde_core::mcp_trust::partition_mcp_servers(
        &config.mcp_servers,
        project_root.as_deref(),
        settings.trust_project_mcp_servers,
        &std::collections::HashSet::new(),
        &store,
    );
    if !decision.pending.is_empty() {
        let names: Vec<&str> = decision.pending.iter().map(|s| s.name.as_str()).collect();
        tracing::warn!(
            servers = ?names,
            "Skipping untrusted project-defined MCP server(s) in ACP runtime"
        );
    }
    if decision.allowed.is_empty() {
        return None;
    }
    let mgr = Arc::new(clawde_mcp::McpManager::connect_all(&decision.allowed).await);
    mgr.clone().spawn_notification_poll_loop();
    Some(mgr)
}

#[cfg(test)]
mod tests {
    use super::builtin_tools_for_config;
    use clawde_core::config::Config;

    #[test]
    fn acp_allow_and_deny_lists_filter_builtin_tools() {
        let config = Config {
            allowed_tools: vec!["Read".to_string(), "Grep".to_string(), "Bash".to_string()],
            disallowed_tools: vec!["grep".to_string(), "Bash".to_string()],
            ..Default::default()
        };

        let tools = builtin_tools_for_config(&config);
        assert!(tools.iter().any(|tool| tool.name() == "Read"));
        assert!(!tools.iter().any(|tool| tool.name() == "Grep"));
        assert!(!tools.iter().any(|tool| tool.name() == "Bash"));
    }

    #[test]
    fn acp_empty_allowlist_preserves_default_builtin_exposure() {
        let tools = builtin_tools_for_config(&Config::default());
        assert!(tools.iter().any(|tool| tool.name() == "Read"));
        assert!(tools.iter().any(|tool| tool.name() == "Bash"));
    }
}
