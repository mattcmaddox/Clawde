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
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

pub const ADMIN_VERSION: u32 = 1;
/// Sentinel `--run all`: when `AdminStore::runner_projects` holds exactly
/// `[RUN_ALL]`, the always-on runner re-resolves the registered project set at
/// serve time and live-joins newly-registered projects (the refresh path),
/// rather than pinning a concrete list. Kept here (not in the CLI) so both the
/// board web server and the CLI interpret the same sentinel.
pub const RUN_ALL: &str = "all";
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
    /// Projects the always-on unit schedules (`board expose --run <NAME,...>`
    /// or `--run all`). Resolved to concrete project names at expose time and
    /// persisted so a later expose keeps rendering the board unit. An empty
    /// list renders no board unit (board not always-on).
    #[serde(default)]
    pub runner_projects: Vec<String>,
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
            runner_projects: Vec::new(),
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

/// The always-on runner's web-facing state: what it is scheduling (or would,
/// for `--run all`) and what it is *waiting to join*. Derived from the
/// persisted `AdminStore::runner_projects` plus the live board/project
/// registries, so the web board can show the admin which projects are being
/// executed and which would join if registered.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerState {
    /// Whether the board is configured to be always-on with a runner at all
    /// (`runner_projects` is non-empty). When false, nothing is scheduled and
    /// the board only serves on demand.
    pub configured: bool,
    /// `all` (schedules every registered project, live-joining new ones) or
    /// `list` (only the pinned projects).
    pub mode: String,
    /// Projects the runner actively schedules right now (resolved from
    /// `runner_projects`): for `all` this is the current registered set; for
    /// `list` it is the pinned names.
    pub scheduled: Vec<String>,
    /// Projects that will be picked up *without a restart* once they are
    /// ready: in `all` mode, every registered project that exists as a board;
    /// in `list` mode, none (a list pins exactly those named).
    pub waiting: Vec<String>,
}

/// Compute `RunnerState` from the persisted admin store.
pub fn runner_state(store: &AdminStore) -> RunnerState {
    let projects = &store.runner_projects;
    if projects.is_empty() {
        return RunnerState {
            configured: false,
            mode: "none".to_string(),
            scheduled: Vec::new(),
            waiting: Vec::new(),
        };
    }
    let is_all = projects.len() == 1 && projects[0] == RUN_ALL;

    // `all` = every registered project (those with a git repo). For display we
    // mark an unregistered board as "waiting to join" only when it can be
    // picked up live (all mode).
    if is_all {
        let registered: HashSet<String> =
            crate::projects::registered_projects().into_iter().collect();
        let boards: HashSet<String> = crate::board::existing_projects().into_iter().collect();
        let scheduled: Vec<String> = registered.iter().cloned().collect();
        let waiting: Vec<String> = boards.difference(&registered).cloned().collect::<Vec<_>>();
        return RunnerState {
            configured: true,
            mode: "all".to_string(),
            scheduled: sorted(scheduled),
            waiting: sorted(waiting),
        };
    }

    RunnerState {
        configured: true,
        mode: "list".to_string(),
        scheduled: sorted(projects.clone()),
        waiting: Vec::new(),
    }
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
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

    #[test]
    fn runner_state_reports_scheduled_and_waiting() {
        // Serialize CLAWDE_HOME mutation on the crate lock (repo rule).
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::env::set_var("CLAWDE_HOME", tmp.path());

        // A board-only project (registered as a repo -> will be scheduled),
        // and a board-only project with no registered repo -> "waiting to
        // join" under --run all.
        std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        crate::projects::set_repo_root("app", repo.path()).unwrap();
        let mut board = crate::board::Board::new();
        board.add_card("task");
        crate::board::save_board(&board, "app").unwrap();
        crate::board::save_board(&board, "docs").unwrap();

        // No runner configured -> configured:false, nothing scheduled.
        let state = runner_state(&AdminStore::default());
        assert!(!state.configured);
        assert!(state.scheduled.is_empty());
        assert!(state.waiting.is_empty());

        // --run all: 'app' is registered+scheduled; 'docs' is a board with no
        // repo -> waiting to join.
        let store = AdminStore {
            runner_projects: vec![RUN_ALL.to_string()],
            ..Default::default()
        };
        let state = runner_state(&store);
        assert!(state.configured);
        assert_eq!(state.mode, "all");
        assert_eq!(state.scheduled, vec!["app"]);
        assert_eq!(state.waiting, vec!["docs"]);

        // Explicit list: only the pinned projects are scheduled; a board not
        // in the list is not running and not reported as live-joinable.
        let store = AdminStore {
            runner_projects: vec!["app".to_string()],
            ..Default::default()
        };
        let state = runner_state(&store);
        assert_eq!(state.mode, "list");
        assert_eq!(state.scheduled, vec!["app"]);
        assert!(state.waiting.is_empty());

        std::env::remove_var("CLAWDE_HOME");
    }
}
