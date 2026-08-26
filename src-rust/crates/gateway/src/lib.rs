//! Clawde Gateway — an OpenAI-compatible HTTP API that routes chat completion
//! requests through Clawde's provider registry (FreeProvider fallback, key
//! rotation, cooldowns).
//!
//! Scope guardrail: the gateway proxies **chat completions only**. It does not
//! run the agent loop, execute tools, manage sessions, or expose the TUI.

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
