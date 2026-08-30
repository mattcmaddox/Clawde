//! Admin board credential store (spec §6.2, §20.7 — the board write surface).
//!
//! The board web UI is admin-only. Writes (add/advance cards via
//! `board_server`) require a valid admin session; sessions come from a single
//! admin password the operator sets with `clawde katban board password`.
//!
//! Mirrors the guest link store (`guest.rs`) exactly:
//! - the password is never stored — only a salted SHA-256 hash plus the salt,
//! - an admin session is a random 256-bit value stored only as a hash (the
//!   plaintext is handed to the browser once as a cookie),
//! - wrong-password attempts from an IP use the same lockout ladder as the
//!   guest server (5 -> 3 -> 3 -> permanent) via the shared `apply_failed_attempt`.
//!
//! Persisted to `~/.clawde/katban/admin.json`. The same `hash` /
//! `random_hex` / `now_secs` helpers as the guest store are reused so both
//! surfaces stay consistent.

use crate::guest::{apply_failed_attempt, hash, now_secs, random_hex, Device, LockoutResult};
use crate::guest_server::MAX_DEVICES_PER_LINK;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

pub const ADMIN_VERSION: u32 = 1;
/// Cookie that carries the admin session token. Distinct from the guest cookie.
pub const ADMIN_COOKIE: &str = "katban_admin";
/// Session lifetime; sessions are re-minted on each successful login.
pub const ADMIN_SESSION_TTL_SECS: u64 = 30 * 24 * 3600;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminStore {
    pub version: u32,
    #[serde(default)]
    pub password_salt: String,
    #[serde(default)]
    pub password_hash: String,
    /// Admin sessions: token hash + last-seen (a small, capped device list).
    #[serde(default)]
    pub sessions: Vec<Device>,
    /// ip -> failed attempts / lockout (same shape as the guest store).
    #[serde(default)]
    pub failed_attempts: HashMap<String, crate::guest::FailedAttempt>,
    /// Public admin subdomain the board is exposed at through caddy (e.g.
    /// `board.example.com`), set by `board expose`.
    #[serde(default)]
    pub public_subdomain: Option<String>,
    /// Port the board server binds on when the always-on unit runs it (set by
    /// `board expose --port`). Persisted so a later expose regenerates the
    /// caddy block with the same port.
    #[serde(default)]
    pub board_port: Option<u16>,
    /// Project the always-on unit runs (`board expose --run <PROJECT>` — one
    /// unit per project). Persisted so a later expose keeps rendering the
    /// board unit; `None` renders no board unit (board not always-on).
    #[serde(default)]
    pub runner_project: Option<String>,
}

impl Default for AdminStore {
    fn default() -> Self {
        AdminStore {
            version: ADMIN_VERSION,
            password_salt: String::new(),
            password_hash: String::new(),
            sessions: Vec::new(),
            failed_attempts: HashMap::new(),
            public_subdomain: None,
            board_port: None,
            runner_project: None,
        }
    }
}

pub fn admin_path() -> PathBuf {
    crate::config::katban_data_dir().join("admin.json")
}

pub fn load() -> anyhow::Result<AdminStore> {
    let path = admin_path();
    if !path.exists() {
        return Ok(AdminStore::default());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read admin store at {}", path.display()))?;
    let store: AdminStore = serde_json::from_str(&text)
        .with_context(|| format!("corrupt admin store at {}", path.display()))?;
    Ok(store)
}

pub fn save(store: &AdminStore) -> anyhow::Result<()> {
    let path = admin_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::caddy::write_atomic(&path, &serde_json::to_string_pretty(store)?)?;
    Ok(())
}

impl AdminStore {
    pub fn is_configured(&self) -> bool {
        !self.password_hash.is_empty()
    }

    /// Set the admin password: re-salt and re-hash in place. Existing admin
    /// sessions are preserved (they are authenticated separately from the
    /// password). Callers persist afterwards.
    pub fn set_password(&mut self, new_password: &str) {
        self.password_salt = random_hex(16);
        self.password_hash = hash(&self.password_salt, new_password);
    }

    pub fn verify_password(&self, password: &str) -> bool {
        !self.password_hash.is_empty() && hash(&self.password_salt, password) == self.password_hash
    }

    /// Mint a fresh admin session after a successful login. Returns the
    /// plaintext token (set as the cookie); only its hash is stored. The
    /// session list is capped (oldest dropped) so a long-lived store can't
    /// grow without bound.
    pub fn mint_session(&mut self) -> String {
        let token = random_hex(32);
        let now = now_secs();
        self.sessions.push(Device {
            token_hash: hash(ADMIN_COOKIE, &token),
            label: "admin".to_string(),
            created_at: now,
            last_seen_at: now,
        });
        if self.sessions.len() > MAX_DEVICES_PER_LINK {
            self.sessions
                .drain(..self.sessions.len() - MAX_DEVICES_PER_LINK);
        }
        token
    }

    pub fn session_valid(&self, token: &str) -> bool {
        self.sessions
            .iter()
            .any(|session| session.token_hash == hash(ADMIN_COOKIE, token))
    }

    pub fn touch_session(&mut self, token: &str) {
        let token_hash = hash(ADMIN_COOKIE, token);
        if let Some(session) = self
            .sessions
            .iter_mut()
            .find(|s| s.token_hash == token_hash)
        {
            session.last_seen_at = now_secs();
        }
    }

    pub fn locked_until(&self, ip: &str, now: u64) -> Option<u64> {
        self.failed_attempts
            .get(ip)
            .and_then(|attempt| attempt.locked_until)
            .filter(|until| *until > now)
    }

    pub fn is_permanently_blocked(&self, ip: &str) -> bool {
        self.failed_attempts
            .get(ip)
            .is_some_and(|attempt| attempt.permanently_blocked)
    }

    /// Record a wrong admin password from an IP using the shared lockout ladder.
    pub fn record_failed_attempt(&mut self, ip: &str) -> LockoutResult {
        let now = now_secs();
        let entry = self.failed_attempts.entry(ip.to_string()).or_default();
        apply_failed_attempt(entry, now)
    }

    /// Clear an IP's failed attempts (admin escape hatch).
    pub fn reset_failed_attempts(&mut self, ip: &str) {
        self.failed_attempts.remove(ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_roundtrips_and_is_hashed_not_plaintext() {
        let mut store = AdminStore::default();
        assert!(!store.is_configured());
        assert!(!store.verify_password("hunter2"));
        store.set_password("hunter2");
        assert!(store.is_configured());
        assert!(store.verify_password("hunter2"));
        assert!(!store.verify_password("wrong"));
        assert_ne!(store.password_hash, "hunter2");
        assert!(!store.password_salt.is_empty());
    }

    #[test]
    fn rotate_password_invalidates_old_and_keeps_sessions() {
        let mut store = AdminStore::default();
        store.set_password("one");
        let token = store.mint_session();
        store.set_password("two");
        assert!(store.verify_password("two"));
        assert!(!store.verify_password("one"));
        // Sessions survive a password rotation (matches the guest store).
        assert!(store.session_valid(&token));
    }

    #[test]
    fn session_mint_validate_and_cap() {
        let mut store = AdminStore::default();
        store.set_password("pw");
        let token = store.mint_session();
        assert!(store.session_valid(&token));
        assert!(!store.session_valid("bogus"));
        let mut extras = Vec::new();
        for _ in 0..(MAX_DEVICES_PER_LINK + 2) {
            extras.push(store.mint_session());
        }
        assert_eq!(store.sessions.len(), MAX_DEVICES_PER_LINK);
        assert!(!store.session_valid(&token)); // oldest dropped
        assert!(extras.into_iter().last().is_some());
    }

    #[test]
    fn lockout_ladder_matches_guest_policy() {
        let mut store = AdminStore::default();
        store.set_password("pw");
        let ip = "10.0.0.9";
        let now = now_secs();
        assert!(store.locked_until(ip, now).is_none());
        for _ in 0..4 {
            assert_eq!(store.record_failed_attempt(ip), LockoutResult::None);
        }
        // 5th wrong attempt triggers the first 3-minute lockout.
        let locked = store.record_failed_attempt(ip);
        assert!(matches!(locked, LockoutResult::Temporary(_)));
        assert!(store.locked_until(ip, now).is_some());
        store.failed_attempts.clear();
    }
}
