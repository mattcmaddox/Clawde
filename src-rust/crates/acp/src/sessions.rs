//! Per-session state for the ACP server.

use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol_schema as acp;
use clawde_core::types::Message;
use clawde_tools::PendingPermissionStore;
use dashmap::DashMap;
use tokio_util::sync::CancellationToken;

/// Session-owned MCP resources. The manager is optional until a session
/// requests MCP servers; keeping this context on the session prevents future
/// ACP sessions from accidentally sharing dynamic tool bindings.
pub struct SessionMcpContext {
    manager: Option<Arc<clawde_mcp::McpManager>>,
    shutdown: CancellationToken,
}

impl SessionMcpContext {
    pub fn empty() -> Arc<Self> {
        Arc::new(Self {
            manager: None,
            shutdown: CancellationToken::new(),
        })
    }

    pub fn from_manager(manager: Arc<clawde_mcp::McpManager>) -> Arc<Self> {
        Arc::new(Self {
            manager: Some(manager),
            shutdown: CancellationToken::new(),
        })
    }

    pub fn manager(&self) -> Option<Arc<clawde_mcp::McpManager>> {
        self.manager.clone()
    }

    pub fn shutdown(&self) {
        if !self.shutdown.is_cancelled() {
            self.shutdown.cancel();
            if let Some(manager) = &self.manager {
                manager.stop_notification_tasks();
            }
        }
    }
}

impl Drop for SessionMcpContext {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// One ACP session — a logical conversation with its own cwd, transcript,
/// MCP server roster, and cancellation token.
pub struct SessionState {
    pub session_id: acp::SessionId,
    pub cwd: PathBuf,
    /// Additional absolute roots granted by the ACP client for this session.
    pub additional_directories: Vec<PathBuf>,
    /// MCP connections and dynamic tool context owned by this session.
    pub mcp: Arc<SessionMcpContext>,
    pub messages: parking_lot::Mutex<Vec<Message>>,
    cancel_token: parking_lot::Mutex<CancellationToken>,
    pub pending_permissions: Arc<parking_lot::Mutex<PendingPermissionStore>>,
    pub file_history: Arc<parking_lot::Mutex<clawde_core::file_history::FileHistory>>,
    pub current_turn: Arc<std::sync::atomic::AtomicUsize>,
    /// Serialize prompts within one session. ACP dispatches requests on
    /// independent tasks, but transcript/tool execution must remain ordered.
    prompt_lock: tokio::sync::Mutex<()>,
}

impl SessionState {
    pub fn new(
        session_id: acp::SessionId,
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
    ) -> Arc<Self> {
        Self::new_with_mcp(
            session_id,
            cwd,
            additional_directories,
            SessionMcpContext::empty(),
        )
    }

    pub fn new_with_mcp(
        session_id: acp::SessionId,
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        mcp: Arc<SessionMcpContext>,
    ) -> Arc<Self> {
        Arc::new(Self {
            session_id,
            cwd,
            additional_directories,
            mcp,
            messages: parking_lot::Mutex::new(Vec::new()),
            cancel_token: parking_lot::Mutex::new(CancellationToken::new()),
            pending_permissions: Arc::new(parking_lot::Mutex::new(
                PendingPermissionStore::default(),
            )),
            file_history: Arc::new(parking_lot::Mutex::new(
                clawde_core::file_history::FileHistory::new(),
            )),
            current_turn: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            prompt_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Serialize prompt handlers for this session while allowing
    /// `session/cancel` notifications to cancel the active turn.
    pub(crate) async fn lock_prompt(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.prompt_lock.lock().await
    }

    /// Snapshot the token for the current prompt turn.
    pub fn current_cancel_token(&self) -> CancellationToken {
        self.cancel_token.lock().clone()
    }

    /// Cancel the in-flight turn and replace its token for the next prompt.
    pub fn shutdown(&self) {
        self.mcp.shutdown();
    }

    pub fn cancel_current_turn(&self) {
        let current = {
            let mut token = self.cancel_token.lock();
            let current = token.clone();
            *token = CancellationToken::new();
            current
        };
        current.cancel();
    }
}

/// Map of active sessions keyed by ACP session id.
#[derive(Default)]
pub struct SessionRegistry {
    inner: DashMap<acp::SessionId, Arc<SessionState>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, state: Arc<SessionState>) {
        self.inner.insert(state.session_id.clone(), state);
    }

    pub fn get(&self, id: &acp::SessionId) -> Option<Arc<SessionState>> {
        self.inner.get(id).map(|r| r.value().clone())
    }

    pub fn remove(&self, id: &acp::SessionId) -> Option<Arc<SessionState>> {
        self.inner.remove(id).map(|(_, state)| {
            state.shutdown();
            state
        })
    }
}

#[cfg(test)]
mod tests {
    use super::SessionState;
    use agent_client_protocol_schema as acp;
    use std::path::PathBuf;

    #[tokio::test]
    async fn prompt_lock_serializes_turns() {
        let session = SessionState::new(
            acp::SessionId::new("test-session"),
            PathBuf::from("/workspace"),
            Vec::new(),
        );
        let first = session.lock_prompt().await;
        assert!(session.prompt_lock.try_lock().is_err());
        drop(first);
        assert!(session.prompt_lock.try_lock().is_ok());
    }

    #[test]
    fn empty_session_mcp_context_has_no_manager() {
        let context = super::SessionMcpContext::empty();
        assert!(context.manager().is_none());
        context.shutdown();
        context.shutdown();
    }

    #[test]
    fn cancelling_a_turn_rearms_the_next_turn_token() {
        let session = SessionState::new(
            acp::SessionId::new("test-session"),
            PathBuf::from("/workspace"),
            Vec::new(),
        );
        let first = session.current_cancel_token();
        assert!(!first.is_cancelled());

        session.cancel_current_turn();

        assert!(first.is_cancelled());
        let next = session.current_cancel_token();
        assert!(!next.is_cancelled());
    }
}
