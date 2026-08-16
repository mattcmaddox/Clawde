//! Per-session state for the ACP server.

use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol_schema as acp;
use clawde_core::types::Message;
use clawde_tools::PendingPermissionStore;
use dashmap::DashMap;
use tokio_util::sync::CancellationToken;

/// One ACP session — a logical conversation with its own cwd, transcript,
/// MCP server roster, and cancellation token.
pub struct SessionState {
    pub session_id: acp::SessionId,
    pub cwd: PathBuf,
    /// Additional absolute roots granted by the ACP client for this session.
    pub additional_directories: Vec<PathBuf>,
    pub messages: parking_lot::Mutex<Vec<Message>>,
    pub cancel_token: CancellationToken,
    pub pending_permissions: Arc<parking_lot::Mutex<PendingPermissionStore>>,
    pub file_history: Arc<parking_lot::Mutex<clawde_core::file_history::FileHistory>>,
    pub current_turn: Arc<std::sync::atomic::AtomicUsize>,
}

impl SessionState {
    pub fn new(
        session_id: acp::SessionId,
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
    ) -> Arc<Self> {
        Arc::new(Self {
            session_id,
            cwd,
            additional_directories,
            messages: parking_lot::Mutex::new(Vec::new()),
            cancel_token: CancellationToken::new(),
            pending_permissions: Arc::new(parking_lot::Mutex::new(
                PendingPermissionStore::default(),
            )),
            file_history: Arc::new(parking_lot::Mutex::new(
                clawde_core::file_history::FileHistory::new(),
            )),
            current_turn: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
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
        self.inner.remove(id).map(|(_, v)| v)
    }
}
