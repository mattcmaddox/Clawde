//! Top-level ACP request / notification dispatcher.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol_schema as acp;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::connection::{Connection, Inbound};
use crate::runtime::AgentRuntime;
use crate::sessions::{SessionMcpContext, SessionRegistry, SessionState};

/// The ACP agent: owns the connection, the runtime, and the session registry.
pub struct AgentServer {
    pub connection: Arc<Connection>,
    pub runtime: Arc<AgentRuntime>,
    pub sessions: Arc<SessionRegistry>,
    pub client_capabilities: parking_lot::RwLock<acp::ClientCapabilities>,
}

impl AgentServer {
    pub fn new(connection: Arc<Connection>, runtime: Arc<AgentRuntime>) -> Arc<Self> {
        Arc::new(Self {
            connection,
            runtime,
            sessions: Arc::new(SessionRegistry::new()),
            client_capabilities: parking_lot::RwLock::new(acp::ClientCapabilities::default()),
        })
    }

    /// Dispatch a single inbound message. Spawns the actual handler on a
    /// background task so the reader loop stays responsive while a prompt
    /// is in flight. Returns the join handle so the caller can wait for
    /// in-flight work to finish before shutting down.
    pub fn dispatch(self: &Arc<Self>, msg: Inbound) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            match msg {
                Inbound::Request { id, method, params } => {
                    let response = this.handle_request(&method, params).await;
                    let result = match response {
                        Ok(value) => this.connection.send_response(id, value).await,
                        Err(err) => this.connection.send_error_response(id, err).await,
                    };
                    if let Err(e) = result {
                        warn!(?e, method = %method, "ACP: failed to send response");
                    }
                }
                Inbound::Notification { method, params } => {
                    this.handle_notification(&method, params).await;
                }
            }
        })
    }

    async fn handle_request(
        self: &Arc<Self>,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, acp::Error> {
        debug!(method, "ACP: dispatch request");
        match method {
            "initialize" => {
                let req: acp::InitializeRequest = parse_params(params)?;
                let result = self.on_initialize(req).await?;
                serde_json::to_value(result).map_err(|_| acp::Error::internal_error())
            }
            "authenticate" => {
                let _req: acp::AuthenticateRequest = parse_params(params)?;
                // ACP v1 AuthenticateRequest has no token/credential field, so
                // shared-secret validation must be handled at the transport level.
                serde_json::to_value(acp::AuthenticateResponse::default())
                    .map_err(|_| acp::Error::internal_error())
            }
            "session/new" => {
                let req: acp::NewSessionRequest = parse_params(params)?;
                let result = self.on_new_session(req).await?;
                serde_json::to_value(result).map_err(|_| acp::Error::internal_error())
            }
            "session/load" => {
                // v1: not supported. Capability is advertised as false in
                // initialize so a well-behaved client never calls this.
                Err(acp::Error::method_not_found())
            }
            "session/prompt" => {
                let req: acp::PromptRequest = parse_params(params)?;
                let result = self.on_prompt(req).await?;
                serde_json::to_value(result).map_err(|_| acp::Error::internal_error())
            }
            other => {
                warn!(method = other, "ACP: method not found");
                Err(acp::Error::method_not_found())
            }
        }
    }

    async fn handle_notification(self: &Arc<Self>, method: &str, params: Option<Value>) {
        debug!(method, "ACP: dispatch notification");
        match method {
            "session/cancel" => {
                let parsed: Result<acp::CancelNotification, _> = params
                    .map(serde_json::from_value)
                    .unwrap_or(Err(serde::de::Error::custom("missing params")));
                match parsed {
                    Ok(notif) => {
                        if let Some(session) = self.sessions.get(&notif.session_id) {
                            info!(session_id = %notif.session_id, "ACP: cancelling session");
                            // Cancel only the current turn and atomically
                            // re-arm the session for its next prompt.
                            session.cancel_current_turn();
                        }
                    }
                    Err(e) => warn!(?e, "ACP: malformed session/cancel notification"),
                }
            }
            other => {
                warn!(method = other, "ACP: ignoring unknown notification");
            }
        }
    }

    async fn on_initialize(
        self: &Arc<Self>,
        req: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, acp::Error> {
        info!(
            client_version = ?req.client_info.as_ref().map(|i| (&i.name, &i.version)),
            "ACP: initialize"
        );
        *self.client_capabilities.write() = req.client_capabilities.clone();

        let agent_info = acp::Implementation::new("clawde", env!("CARGO_PKG_VERSION"))
            .title(Some("Clawde".to_string()));

        let mut response = acp::InitializeResponse::new(acp::ProtocolVersion::V1)
            .agent_capabilities(
                acp::AgentCapabilities::new()
                    .load_session(false)
                    .prompt_capabilities(acp::PromptCapabilities::new().image(true))
                    .mcp_capabilities(acp::McpCapabilities::new()),
            );
        response = response.agent_info(Some(agent_info));
        Ok(response)
    }

    async fn on_new_session(
        self: &Arc<Self>,
        req: acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, acp::Error> {
        if let Err(reason) = validate_session_directories(&req.cwd, &req.additional_directories) {
            return Err(
                acp::Error::invalid_params().data(Some(serde_json::json!({ "reason": reason })))
            );
        }
        let mcp_configs = match validate_session_mcp_servers(&req.mcp_servers) {
            Ok(configs) => configs,
            Err(reason) => {
                return Err(acp::Error::invalid_params()
                    .data(Some(serde_json::json!({ "reason": reason }))));
            }
        };
        let mcp_context = if mcp_configs.is_empty() {
            SessionMcpContext::empty()
        } else {
            let manager = clawde_mcp::McpManager::connect_session_stdio(&mcp_configs)
                .await
                .map_err(|error| {
                    acp::Error::invalid_params().data(Some(serde_json::json!({
                        "reason": format!("failed to initialize session MCP servers: {}", error)
                    })))
                })?;
            let manager = Arc::new(manager);
            manager.clone().spawn_notification_poll_loop();
            SessionMcpContext::from_manager(manager)
        };
        let session_id = acp::SessionId::new(format!("acp-{}", uuid::Uuid::new_v4()));
        let state = SessionState::new_with_mcp(
            session_id.clone(),
            req.cwd.clone(),
            req.additional_directories.clone(),
            mcp_context,
        );
        info!(session_id = %session_id, cwd = %req.cwd.display(), "ACP: new session");

        self.sessions.insert(state);
        Ok(acp::NewSessionResponse::new(session_id))
    }

    async fn on_prompt(
        self: &Arc<Self>,
        req: acp::PromptRequest,
    ) -> Result<acp::PromptResponse, acp::Error> {
        let session = match self.sessions.get(&req.session_id) {
            Some(s) => s,
            None => {
                return Err(acp::Error::invalid_params().data(Some(serde_json::json!({
                    "reason": "unknown session",
                    "sessionId": req.session_id,
                }))));
            }
        };
        crate::prompt::handle(self.runtime.clone(), self.connection.clone(), session, req).await
    }
}

fn validate_session_directories(cwd: &Path, additional: &[PathBuf]) -> Result<(), String> {
    if !cwd.is_absolute() {
        return Err("cwd must be absolute".to_string());
    }
    if let Some(path) = additional.iter().find(|path| !path.is_absolute()) {
        return Err(format!(
            "additional_directories must be absolute: {}",
            path.display()
        ));
    }
    Ok(())
}

fn validate_session_mcp_servers(
    servers: &[acp::McpServer],
) -> Result<Vec<clawde_core::config::McpServerConfig>, String> {
    const MAX_SESSION_MCP_SERVERS: usize = 8;
    const MAX_ARGS: usize = 128;
    const MAX_ENV: usize = 64;
    const MAX_VALUE_BYTES: usize = 16 * 1024;

    if servers.len() > MAX_SESSION_MCP_SERVERS {
        return Err(format!(
            "at most {} session MCP servers are allowed",
            MAX_SESSION_MCP_SERVERS
        ));
    }

    let mut names = HashSet::new();
    let mut configs = Vec::with_capacity(servers.len());
    for server in servers {
        let stdio =
            match server {
                acp::McpServer::Stdio(stdio) => stdio,
                _ => return Err(
                    "session MCP currently supports only stdio servers; HTTP/SSE remain disabled"
                        .to_string(),
                ),
            };
        if stdio.name.trim().is_empty() || !names.insert(stdio.name.clone()) {
            return Err(format!(
                "session MCP server names must be non-empty and unique: '{}'",
                stdio.name
            ));
        }
        if !stdio.command.is_absolute() {
            return Err(format!(
                "session MCP stdio command must be an absolute path: {}",
                stdio.command.display()
            ));
        }
        if stdio.args.len() > MAX_ARGS {
            return Err(format!(
                "session MCP server '{}' has too many arguments",
                stdio.name
            ));
        }
        if stdio.env.len() > MAX_ENV {
            return Err(format!(
                "session MCP server '{}' has too many environment variables",
                stdio.name
            ));
        }
        let mut env = std::collections::HashMap::new();
        for variable in &stdio.env {
            if variable.name.is_empty()
                || variable.name.contains('=')
                || variable.name.bytes().any(|byte| byte.is_ascii_control())
                || variable.value.len() > MAX_VALUE_BYTES
                || variable.value.contains("${")
            {
                return Err(format!(
                    "session MCP server '{}' has an invalid or unsafe environment variable",
                    stdio.name
                ));
            }
            if env
                .insert(variable.name.clone(), variable.value.clone())
                .is_some()
            {
                return Err(format!(
                    "session MCP server '{}' contains duplicate environment variable '{}'",
                    stdio.name, variable.name
                ));
            }
        }
        configs.push(clawde_core::config::McpServerConfig {
            name: stdio.name.clone(),
            command: Some(stdio.command.to_string_lossy().into_owned()),
            args: stdio.args.clone(),
            env,
            url: None,
            server_type: "stdio".to_string(),
            origin: Default::default(),
        });
    }
    Ok(configs)
}

#[cfg(test)]
mod tests {
    use super::{validate_session_directories, validate_session_mcp_servers};
    use agent_client_protocol_schema as acp;
    use std::path::Path;

    #[test]
    fn session_directories_require_absolute_paths() {
        assert!(validate_session_directories(Path::new("/workspace"), &[]).is_ok());
        assert!(validate_session_directories(
            Path::new("/workspace"),
            &[Path::new("/shared").to_path_buf()]
        )
        .is_ok());
        assert!(validate_session_directories(Path::new("workspace"), &[]).is_err());
        assert!(validate_session_directories(
            Path::new("/workspace"),
            &[Path::new("shared").to_path_buf()]
        )
        .is_err());
    }

    #[test]
    fn session_mcp_servers_accept_only_safe_stdio_configs() {
        assert!(validate_session_mcp_servers(&[]).unwrap().is_empty());

        let relative = acp::McpServer::Stdio(acp::McpServerStdio::new("local", "server"));
        let error = validate_session_mcp_servers(&[relative]).unwrap_err();
        assert!(error.contains("absolute path"));

        let http = acp::McpServer::Http(acp::McpServerHttp::new(
            "remote",
            "https://example.test/mcp",
        ));
        let error = validate_session_mcp_servers(&[http]).unwrap_err();
        assert!(error.contains("only stdio"));

        let stdio = acp::McpServer::Stdio(acp::McpServerStdio::new("local", "/bin/server"));
        let configs = validate_session_mcp_servers(&[stdio]).unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].server_type, "stdio");
        assert_eq!(configs[0].command.as_deref(), Some("/bin/server"));
    }
}

fn parse_params<T: serde::de::DeserializeOwned>(params: Option<Value>) -> Result<T, acp::Error> {
    let value = params.ok_or_else(acp::Error::invalid_params)?;
    serde_json::from_value(value).map_err(|e| {
        acp::Error::invalid_params().data(Some(
            serde_json::json!({ "deserialize_error": e.to_string() }),
        ))
    })
}
