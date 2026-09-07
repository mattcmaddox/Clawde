//! Compatibility name for Katban's administrator credential store.
//!
//! The implementation lives in [`crate::admin_auth`]. Keeping this module as
//! a re-export avoids breaking existing board-server and CLI call sites while
//! making it impossible for Cat Chat's guest password policy to become the
//! board's policy again.

pub use crate::admin_auth::*;
