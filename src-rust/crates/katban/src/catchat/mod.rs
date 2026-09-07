//! Cat Chat: the optional public-facing sandbox chat surface.
//!
//! Cat Chat owns its public pages/server, ephemeral chat engine, guest-link
//! store, and search adapter. It is not a development board, project runner,
//! git worktree manager, or Katban admin surface.

pub mod engine;
pub mod links;
pub mod pages;
pub mod search;
pub mod server;
