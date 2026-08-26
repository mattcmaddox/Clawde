//! Clawde Gateway — an OpenAI-compatible HTTP API that routes requests
//! through Clawde's provider registry (FreeProvider fallback, key rotation,
//! cooldowns) and can run Clawde's server-side agent loop.
//!
//! Two surfaces: relay chat completions (`POST /v1/chat/completions`, the
//! default) and agent mode — server-side built-in tool execution on the same
//! endpoint plus the agent-native `POST /v1/responses` (Open Responses).
//! Response sessions for `previous_response_id` continuation are ephemeral
//! in-memory only (no disk).

pub mod agent;
pub mod auth;
pub mod config;
pub mod context;
pub mod error;
pub mod responses;
pub mod router;
pub mod session;
pub mod shutdown;
pub mod tool_exec;
pub mod translate;

pub use agent::{run_agent_loop, AgentConfig, AgentFailure, AgentOutcome, AgentStatus, LoopEvent};
pub use context::OverflowCompactor;
pub use responses::{parse_responses_request, responses_object, ResponsesItemBuilder};
pub use session::{output_items_to_messages, ResponseSession, SessionStore};
pub use tool_exec::{GatewayPermissionMode, GatewayToolExecutor};

pub use config::EffectiveGatewayConfig;
pub use error::GatewayError;
pub use router::{build_registry, build_router, resolve_model, run_gateway, GatewayState};
