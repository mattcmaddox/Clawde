//! Bridge between Clawde's synchronous `PermissionHandler` trait and the
//! asynchronous `session/request_permission` JSON-RPC round-trip used by ACP.
//!
//! The handler first evaluates the session's permission rules. Requests that
//! still need approval are enqueued by `ToolContext::request_permission_inner`
//! onto a shared `PendingPermissionStore` and block on a oneshot. A background
//! task — spawned by `prompt::handle_prompt` — drains the queue, converts each
//! pending request into a `session/request_permission` call to the connected
//! client, and forwards the client's decision back through the oneshot to
//! unblock the tool.

use std::sync::Arc;

use agent_client_protocol_schema as acp;
use clawde_core::permissions::{PermissionDecision, PermissionManager, PermissionRequest};
use clawde_core::PermissionHandler;
use clawde_tools::{PendingPermissionRequest, PendingPermissionStore};
use tracing::{debug, warn};

use crate::connection::Connection;

/// Permission handler that evaluates session rules before deferring to the
/// ACP client for decisions that still require approval.
pub struct AcpPermissionHandler {
    permission_manager: Arc<std::sync::Mutex<PermissionManager>>,
}

impl AcpPermissionHandler {
    pub fn new(permission_manager: Arc<std::sync::Mutex<PermissionManager>>) -> Self {
        Self { permission_manager }
    }
}

impl PermissionHandler for AcpPermissionHandler {
    fn check_permission(&self, request: &PermissionRequest) -> PermissionDecision {
        let Ok(manager) = self.permission_manager.lock() else {
            return PermissionDecision::Deny;
        };
        manager.evaluate_with_capabilities(
            &request.tool_name,
            &request.description,
            request.permission_level,
            request.network_capable,
            request.stateful,
            request.path.as_deref(),
            request.working_dir.as_deref(),
            &request.allowed_roots,
            request.network_isolated,
        )
    }

    fn request_permission(&self, request: &PermissionRequest) -> PermissionDecision {
        self.check_permission(request)
    }
}

/// Build the structured input shown with an ACP permission request.
///
/// Keep the human-readable title/reason for existing clients, but also expose
/// the target and isolation context so a client can render an informed dialog
/// without scraping prose. This contains request metadata only; credentials
/// and environment values are never copied into it.
fn permission_raw_input(request: &PermissionRequest) -> serde_json::Value {
    serde_json::json!({
        "tool": request.tool_name,
        "description": request.description,
        "details": request.details,
        "context_description": request.context_description,
        "read_only": request.is_read_only,
        "path": request.path,
        "working_dir": request.working_dir.as_ref().map(|path| path.display().to_string()),
        "allowed_roots": request
            .allowed_roots
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>(),
        "network_isolated": request.network_isolated,
        "stateful": request.stateful,
    })
}

/// Drain a single pending permission request, route it through the
/// connection as `session/request_permission`, and fire the oneshot with
/// the resulting decision.
pub async fn forward_pending(
    connection: Arc<Connection>,
    session_id: acp::SessionId,
    pending: PendingPermissionRequest,
    permission_manager: Arc<std::sync::Mutex<PermissionManager>>,
) {
    let PendingPermissionRequest {
        tool_use_id,
        request,
        reason,
        decision_tx,
    } = pending;

    let Some(decision_tx) = decision_tx else {
        warn!(
            tool_use_id,
            "ACP permission: pending request had no decision_tx"
        );
        return;
    };

    let title = if reason.is_empty() {
        format!("Approve {}", request.tool_name)
    } else {
        reason.clone()
    };

    let fields = acp::ToolCallUpdateFields::new()
        .kind(Some(infer_tool_kind(&request)))
        .status(Some(acp::ToolCallStatus::Pending))
        .title(Some(title))
        .raw_input(Some(permission_raw_input(&request)));
    let tool_call = acp::ToolCallUpdate::new(acp::ToolCallId::new(tool_use_id.as_str()), fields);

    let options = vec![
        acp::PermissionOption::new(
            acp::PermissionOptionId::new("allow_once"),
            "Allow once",
            acp::PermissionOptionKind::AllowOnce,
        ),
        acp::PermissionOption::new(
            acp::PermissionOptionId::new("allow_always"),
            "Allow always",
            acp::PermissionOptionKind::AllowAlways,
        ),
        acp::PermissionOption::new(
            acp::PermissionOptionId::new("reject_once"),
            "Reject",
            acp::PermissionOptionKind::RejectOnce,
        ),
    ];

    let request_params = acp::RequestPermissionRequest::new(session_id, tool_call, options);

    debug!(tool = %request.tool_name, "ACP permission: requesting from client");
    let result = connection
        .send_request::<_, acp::RequestPermissionResponse>(
            "session/request_permission",
            request_params,
        )
        .await;

    let decision = match result {
        Ok(Ok(response)) => match response.outcome {
            acp::RequestPermissionOutcome::Selected(sel) => match sel.option_id.0.as_ref() {
                "allow_once" => PermissionDecision::Allow,
                "allow_always" => PermissionDecision::AllowPermanently,
                "reject_always" => PermissionDecision::DenyPermanently,
                _ => PermissionDecision::Deny,
            },
            acp::RequestPermissionOutcome::Cancelled => PermissionDecision::Deny,
            _ => PermissionDecision::Deny,
        },
        Ok(Err(err)) => {
            warn!(?err, "ACP permission: client returned error, denying");
            PermissionDecision::Deny
        }
        Err(err) => {
            warn!(?err, "ACP permission: send_request failed, denying");
            PermissionDecision::Deny
        }
    };

    // ACP's `allow_always` must affect only this session's manager and must
    // persist the same rule shape as the local TUI. Without this update the
    // approval unblocks one call but every subsequent call prompts again.
    if decision == PermissionDecision::AllowPermanently {
        if let Ok(mut manager) = permission_manager.lock() {
            let mut settings = clawde_core::config::Settings::load_sync().unwrap_or_default();
            let result = if let Some(path) = request.path.as_deref() {
                manager.add_persistent_allow_path(
                    &request.tool_name,
                    &format!("{}*", path),
                    &mut settings,
                )
            } else {
                manager.add_persistent_allow(&request.tool_name, &mut settings)
            };
            if let Err(error) = result {
                tracing::warn!(
                    tool = %request.tool_name,
                    error = %error,
                    "ACP permission: failed to persist allow-always rule; keeping it session-local"
                );
                manager.add_session_allow(&request.tool_name);
            }
        }
    }

    let _ = decision_tx.send(decision);
}

/// Classify a Clawde tool name into an ACP `ToolKind` for client UI hints.
fn infer_tool_kind(request: &PermissionRequest) -> acp::ToolKind {
    if request.is_read_only {
        return acp::ToolKind::Read;
    }
    match request.tool_name.as_str() {
        "Edit" | "FileEdit" | "Write" | "FileWrite" | "BatchEdit" | "ApplyPatch" => {
            acp::ToolKind::Edit
        }
        "Bash" | "Shell" | "Execute" => acp::ToolKind::Execute,
        "WebFetch" | "WebSearch" => acp::ToolKind::Fetch,
        "Glob" | "Grep" | "GlobTool" => acp::ToolKind::Search,
        "Delete" | "Rm" => acp::ToolKind::Delete,
        "Move" | "Rename" => acp::ToolKind::Move,
        "Think" | "Sequential" => acp::ToolKind::Think,
        _ => acp::ToolKind::Other,
    }
}

/// Spawn a task that watches the shared `PendingPermissionStore` and
/// forwards each enqueued request through the ACP connection. The task
/// exits when `cancel` is fired or the connection drops.
pub fn spawn_drainer(
    connection: Arc<Connection>,
    session_id: acp::SessionId,
    store: Arc<parking_lot::Mutex<PendingPermissionStore>>,
    permission_manager: Arc<std::sync::Mutex<PermissionManager>>,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(50));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = ticker.tick() => {}
            }
            let popped: Vec<PendingPermissionRequest> = {
                let mut guard = store.lock();
                guard.queue.drain(..).collect()
            };
            for pending in popped {
                let conn = connection.clone();
                let sid = session_id.clone();
                let manager = permission_manager.clone();
                tokio::spawn(async move {
                    forward_pending(conn, sid, pending, manager).await;
                });
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{forward_pending, permission_raw_input, AcpPermissionHandler};
    use crate::connection::Connection;
    use agent_client_protocol_schema as acp;
    use clawde_core::permissions::{
        PermissionAction, PermissionDecision, PermissionHandler, PermissionManager,
        PermissionRequest,
    };
    use clawde_tools::PendingPermissionRequest;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader};

    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    struct TestHome {
        previous: Option<OsString>,
        path: PathBuf,
    }

    impl TestHome {
        fn path() -> PathBuf {
            std::env::temp_dir().join(format!(
                "clawde-acp-permission-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ))
        }

        fn new(previous: Option<OsString>, path: PathBuf) -> Self {
            Self { previous, path }
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("CLAWDE_HOME", value),
                None => std::env::remove_var("CLAWDE_HOME"),
            }
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[tokio::test]
    async fn allow_always_round_trip_persists_and_resolves() {
        let _lock = ENV_LOCK.lock().await;
        let path = TestHome::path();
        std::fs::create_dir_all(&path).unwrap();
        let previous = std::env::var_os("CLAWDE_HOME");
        std::env::set_var("CLAWDE_HOME", &path);
        let _home = TestHome::new(previous, path);
        let settings = clawde_core::config::Settings::default();
        let manager = Arc::new(std::sync::Mutex::new(PermissionManager::new(
            clawde_core::config::PermissionMode::Default,
            &settings,
            &[],
            &[],
        )));
        let request = PermissionRequest {
            tool_name: "Bash".to_string(),
            description: "run a command".to_string(),
            details: Some("echo approved".to_string()),
            is_read_only: false,
            path: Some("echo approved".to_string()),
            working_dir: Some(Path::new("/workspace").to_path_buf()),
            allowed_roots: vec![PathBuf::from("/workspace")],
            context_description: None,
            network_isolated: false,
            permission_level: clawde_core::PermissionLevel::Execute,
            network_capable: false,
            stateful: false,
        };
        let (decision_tx, decision_rx) = tokio::sync::oneshot::channel();
        let pending = PendingPermissionRequest {
            tool_use_id: "tool-use-allow-always".to_string(),
            request,
            reason: "approval required".to_string(),
            decision_tx: Some(decision_tx),
        };

        let (server_writer, client_reader) = duplex(16 * 1024);
        let (client_writer, server_reader) = duplex(16 * 1024);
        let connection = Connection::new(server_writer);
        let (inbound_tx, _inbound_rx) = tokio::sync::mpsc::unbounded_channel();
        let reader_task = tokio::spawn(crate::connection::run_reader(
            connection.clone(),
            server_reader,
            inbound_tx,
        ));
        let client_task = tokio::spawn(async move {
            let mut reader = BufReader::new(client_reader);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let request: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(request["method"], "session/request_permission");
            assert_eq!(
                request["params"]["options"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|option| option["optionId"] == "allow_always")
                    .unwrap()["kind"],
                "allow_always"
            );

            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": {
                    "outcome": {
                        "outcome": "selected",
                        "optionId": "allow_always"
                    }
                }
            });
            let mut bytes = serde_json::to_vec(&response).unwrap();
            bytes.push(b'\n');
            let mut writer = client_writer;
            writer.write_all(&bytes).await.unwrap();
        });

        forward_pending(
            connection.clone(),
            acp::SessionId::new("session-allow-always"),
            pending,
            manager.clone(),
        )
        .await;

        assert_eq!(
            decision_rx.await.unwrap(),
            PermissionDecision::AllowPermanently
        );
        {
            let manager_guard = manager.lock().unwrap();
            assert_eq!(
                manager_guard.evaluate_with_capabilities(
                    "Bash",
                    "run a command",
                    clawde_core::PermissionLevel::Execute,
                    false,
                    false,
                    Some("echo approved"),
                    Some(Path::new("/workspace")),
                    &[PathBuf::from("/workspace")],
                    false,
                ),
                PermissionDecision::Allow
            );
        }

        let saved = clawde_core::config::Settings::load_sync().unwrap();
        assert!(saved.permission_rules.iter().any(|rule| {
            rule.tool_name.as_deref() == Some("Bash") && rule.action == PermissionAction::Allow
        }));
        let reloaded_manager = PermissionManager::new(
            clawde_core::config::PermissionMode::Default,
            &saved,
            &[],
            &[],
        );
        assert_eq!(
            reloaded_manager.evaluate_with_capabilities(
                "Bash",
                "run a command",
                clawde_core::PermissionLevel::Execute,
                false,
                false,
                Some("echo approved"),
                Some(Path::new("/workspace")),
                &[PathBuf::from("/workspace")],
                false,
            ),
            PermissionDecision::Allow
        );

        client_task.await.unwrap();
        drop(connection);
        reader_task.await.unwrap().unwrap();
    }

    async fn mock_permission_response(outcome: &str) -> PermissionDecision {
        let settings = clawde_core::config::Settings::default();
        let manager = Arc::new(std::sync::Mutex::new(PermissionManager::new(
            clawde_core::config::PermissionMode::Default,
            &settings,
            &[],
            &[],
        )));
        let request = PermissionRequest {
            tool_name: "Bash".to_string(),
            description: "run a command".to_string(),
            details: Some("echo rejected".to_string()),
            is_read_only: false,
            path: Some("echo rejected".to_string()),
            working_dir: None,
            allowed_roots: Vec::new(),
            context_description: None,
            network_isolated: false,
            permission_level: clawde_core::PermissionLevel::Execute,
            network_capable: false,
            stateful: false,
        };
        let (decision_tx, decision_rx) = tokio::sync::oneshot::channel();
        let pending = PendingPermissionRequest {
            tool_use_id: format!("tool-use-{outcome}"),
            request,
            reason: "approval required".to_string(),
            decision_tx: Some(decision_tx),
        };

        let (server_writer, client_reader) = duplex(16 * 1024);
        let (client_writer, server_reader) = duplex(16 * 1024);
        let connection = Connection::new(server_writer);
        let (inbound_tx, _inbound_rx) = tokio::sync::mpsc::unbounded_channel();
        let reader_task = tokio::spawn(crate::connection::run_reader(
            connection.clone(),
            server_reader,
            inbound_tx,
        ));
        let outcome = outcome.to_string();
        let client_task = tokio::spawn(async move {
            let mut reader = BufReader::new(client_reader);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let request: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(request["method"], "session/request_permission");
            let selected = outcome != "cancelled";
            let response_outcome = if selected {
                serde_json::json!({
                    "outcome": "selected",
                    "optionId": outcome
                })
            } else {
                serde_json::json!({"outcome": "cancelled"})
            };
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": {"outcome": response_outcome}
            });
            let mut bytes = serde_json::to_vec(&response).unwrap();
            bytes.push(b'\n');
            let mut writer = client_writer;
            writer.write_all(&bytes).await.unwrap();
        });

        forward_pending(
            connection.clone(),
            acp::SessionId::new("session-rejection"),
            pending,
            manager.clone(),
        )
        .await;
        let decision = decision_rx.await.unwrap();
        assert!(manager.lock().unwrap().persistent_rules.is_empty());
        client_task.await.unwrap();
        drop(connection);
        reader_task.await.unwrap().unwrap();
        decision
    }

    #[tokio::test]
    async fn reject_once_and_cancelled_round_trips_deny_without_persistence() {
        assert_eq!(
            mock_permission_response("reject_once").await,
            PermissionDecision::Deny
        );
        assert_eq!(
            mock_permission_response("cancelled").await,
            PermissionDecision::Deny
        );
    }

    #[tokio::test]
    async fn malformed_permission_response_denies_without_persisting() {
        let manager = Arc::new(std::sync::Mutex::new(PermissionManager::new(
            clawde_core::config::PermissionMode::Default,
            &clawde_core::config::Settings::default(),
            &[],
            &[],
        )));
        let (decision_tx, decision_rx) = tokio::sync::oneshot::channel();
        let pending = PendingPermissionRequest {
            tool_use_id: "tool-use-malformed".to_string(),
            request: PermissionRequest {
                tool_name: "Bash".to_string(),
                description: "run a command".to_string(),
                details: None,
                is_read_only: false,
                path: Some("echo malformed".to_string()),
                working_dir: None,
                allowed_roots: Vec::new(),
                context_description: None,
                network_isolated: false,
                permission_level: clawde_core::PermissionLevel::Execute,
                network_capable: false,
                stateful: false,
            },
            reason: "approval required".to_string(),
            decision_tx: Some(decision_tx),
        };
        let (server_writer, client_reader) = duplex(16 * 1024);
        let (client_writer, server_reader) = duplex(16 * 1024);
        let connection = Connection::new(server_writer);
        let (inbound_tx, _inbound_rx) = tokio::sync::mpsc::unbounded_channel();
        let reader_task = tokio::spawn(crate::connection::run_reader(
            connection.clone(),
            server_reader,
            inbound_tx,
        ));
        let client_task = tokio::spawn(async move {
            let mut reader = BufReader::new(client_reader);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let request: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": {"outcome": {"outcome": "not-a-valid-outcome"}}
            });
            let mut bytes = serde_json::to_vec(&response).unwrap();
            bytes.push(b'\n');
            let mut writer = client_writer;
            writer.write_all(&bytes).await.unwrap();
        });

        forward_pending(
            connection.clone(),
            acp::SessionId::new("session-malformed"),
            pending,
            manager.clone(),
        )
        .await;

        assert_eq!(decision_rx.await.unwrap(), PermissionDecision::Deny);
        assert!(manager.lock().unwrap().persistent_rules.is_empty());
        client_task.await.unwrap();
        drop(connection);
        reader_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn permission_connection_drop_denies_without_hanging() {
        let manager = Arc::new(std::sync::Mutex::new(PermissionManager::new(
            clawde_core::config::PermissionMode::Default,
            &clawde_core::config::Settings::default(),
            &[],
            &[],
        )));
        let (decision_tx, decision_rx) = tokio::sync::oneshot::channel();
        let pending = PendingPermissionRequest {
            tool_use_id: "tool-use-disconnect".to_string(),
            request: PermissionRequest {
                tool_name: "Bash".to_string(),
                description: "run a command".to_string(),
                details: None,
                is_read_only: false,
                path: Some("echo disconnect".to_string()),
                working_dir: None,
                allowed_roots: Vec::new(),
                context_description: None,
                network_isolated: false,
                permission_level: clawde_core::PermissionLevel::Execute,
                network_capable: false,
                stateful: false,
            },
            reason: "approval required".to_string(),
            decision_tx: Some(decision_tx),
        };
        let (server_writer, client_reader) = duplex(16 * 1024);
        let (client_writer, server_reader) = duplex(16 * 1024);
        let connection = Connection::new(server_writer);
        let (inbound_tx, _inbound_rx) = tokio::sync::mpsc::unbounded_channel();
        let reader_task = tokio::spawn(crate::connection::run_reader(
            connection.clone(),
            server_reader,
            inbound_tx,
        ));
        let client_task = tokio::spawn(async move {
            let mut reader = BufReader::new(client_reader);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            drop(client_writer);
        });

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            forward_pending(
                connection.clone(),
                acp::SessionId::new("session-disconnect"),
                pending,
                manager.clone(),
            ),
        )
        .await
        .expect("permission request must resolve when ACP client disconnects");

        assert_eq!(decision_rx.await.unwrap(), PermissionDecision::Deny);
        assert!(manager.lock().unwrap().persistent_rules.is_empty());
        client_task.await.unwrap();
        drop(connection);
        reader_task.await.unwrap().unwrap();
    }

    #[test]
    fn session_allow_is_reused_without_prompting_again() {
        let settings = clawde_core::config::Settings::default();
        let manager = Arc::new(std::sync::Mutex::new(PermissionManager::new(
            clawde_core::config::PermissionMode::Default,
            &settings,
            &[],
            &[],
        )));
        manager.lock().unwrap().add_session_allow("Bash");
        let handler = AcpPermissionHandler::new(manager);
        let request = PermissionRequest {
            tool_name: "Bash".to_string(),
            description: "run a command".to_string(),
            details: None,
            is_read_only: false,
            path: Some("echo ok".to_string()),
            working_dir: None,
            allowed_roots: Vec::new(),
            context_description: None,
            network_isolated: false,
            permission_level: clawde_core::PermissionLevel::Execute,
            network_capable: false,
            stateful: false,
        };

        assert_eq!(
            handler.check_permission(&request),
            PermissionDecision::Allow
        );
    }

    #[test]
    fn permission_request_exposes_target_and_isolation_context() {
        let input = permission_raw_input(&PermissionRequest {
            tool_name: "Bash".to_string(),
            description: "Run Bash".to_string(),
            details: Some("ls -la".to_string()),
            is_read_only: false,
            path: Some("/workspace/file.txt".to_string()),
            working_dir: Some(PathBuf::from("/workspace")),
            allowed_roots: vec![PathBuf::from("/workspace"), PathBuf::from("/shared")],
            context_description: Some("execute a local command".to_string()),
            network_isolated: true,
            permission_level: clawde_core::PermissionLevel::Execute,
            network_capable: true,
            stateful: false,
        });

        assert_eq!(input["tool"], "Bash");
        assert_eq!(input["details"], "ls -la");
        assert_eq!(input["path"], "/workspace/file.txt");
        assert_eq!(input["network_isolated"], true);
        assert_eq!(input["stateful"], false);
        assert_eq!(
            input["allowed_roots"],
            serde_json::json!(["/workspace", "/shared"])
        );
    }

    #[test]
    fn permission_request_metadata_does_not_add_credentials() {
        let input = permission_raw_input(&PermissionRequest {
            tool_name: "WebFetch".to_string(),
            description: "Fetch URL".to_string(),
            details: None,
            is_read_only: true,
            path: None,
            working_dir: None,
            allowed_roots: Vec::new(),
            context_description: None,
            network_isolated: false,
            permission_level: clawde_core::PermissionLevel::ReadOnly,
            network_capable: true,
            stateful: false,
        });
        assert!(input.get("api_key").is_none());
        assert!(input.get("token").is_none());
        assert!(input.get("authorization").is_none());
    }
}
