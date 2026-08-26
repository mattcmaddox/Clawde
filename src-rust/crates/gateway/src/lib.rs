//! Clawde Gateway — an OpenAI-compatible HTTP API that routes chat completion
//! requests through Clawde's provider registry (FreeProvider fallback, key
//! rotation, cooldowns).
//!
//! Scope guardrail: the gateway proxies **chat completions only**. It does not
//! run the agent loop, execute tools, manage sessions, or expose the TUI.

pub mod auth;
pub mod config;
pub mod error;
pub mod router;
pub mod shutdown;
pub mod translate;

pub use config::EffectiveGatewayConfig;
pub use error::GatewayError;
pub use router::{build_registry, build_router, resolve_model, run_gateway, GatewayState};
