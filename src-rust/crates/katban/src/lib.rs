// clawde-katban: the self-hosted web surface for Clawde.
//
// v0 slice (per docs/plans/katban-selfhost-spec.md): dev-site hosting with
// live reload — `clawde katban site serve/add/list`. Board, guest access
// tiers, auth, and caddy automation are later slices of the same spec.
//
// Deliberately small dependencies: axum + tokio only, matching the gateway's
// HTTP plumbing conventions.

pub mod board;
pub mod board_admin;
pub mod board_server;
pub mod caddy;
pub mod chat;
pub mod commit;
pub mod config;
pub mod duckdns;
pub mod git;
pub mod guest;
pub mod guest_server;
pub mod host;
pub mod projects;
pub mod reload;
pub mod runner;
pub mod search;
pub mod status;

/// Serializes `CLAWDE_HOME` mutation across the crate's test modules —
/// parallel test safety per repo rules (see `crates/core/src/paths.rs`).
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
