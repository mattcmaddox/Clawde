//! Katban development product.
//!
//! Katban is the serious, owner-facing feature: project registration, boards,
//! git worktrees, agent execution, verification, review, dev-site hosting,
//! caddy integration, and the authenticated admin board server.
//!
//! Cat Chat is intentionally not part of this product boundary. It is an
//! optional public-facing sandbox and lives under [`catchat`]. Its guest links
//! and simple memorable-password policy are not reused by Katban.

pub mod admin_auth;
pub mod board;
pub mod board_admin;
pub mod board_server;
pub mod caddy;
pub mod catchat;
pub mod commit;
pub mod config;
pub mod container;
pub mod duckdns;
pub mod git;
pub mod host;
pub mod http_security;
pub mod projects;
pub mod reload;
pub mod runner;
pub mod scope;
pub mod status;
pub mod time;
pub mod verify;

#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
