//! Katban administrator authentication and runner authorization state.
//!
//! This module belongs to the development product. It is deliberately
//! separate from `catchat::guest`: Katban controls projects, git worktrees,
//! agent execution, and hosted dev sites. Its credential is not a guest-chat
//! password and does not inherit Cat Chat's relaxed password policy.

use crate::caddy::write_atomic;
use anyhow::Context;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub const ADMIN_VERSION: u32 = 2;
pub const RUN_ALL: &str = "all";
pub const ADMIN_COOKIE: &str = "katban_admin";
pub const ADMIN_SESSION_TTL_SECS: u64 = 30 * 24 * 3600;
/// Wrong-password attempts in one admin lockout window.
pub const ADMIN_MAX_FAILED_ATTEMPTS: u32 = 5;
/// Duration of each admin lockout strike.
pub const ADMIN_LOCKOUT_SECS: u64 = 15 * 60;
/// Lockout strikes before the independent Katban admin 24-hour block.
pub const ADMIN_MAX_STRIKES: u32 = 3;
pub const ADMIN_BLOCK_SECS: u64 = 24 * 3600;
pub const ADMIN_MAX_SESSIONS: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AdminFailedAttempt {
    pub count: u32,
    pub locked_until: Option<u64>,
    #[serde(default)]
    pub strikes: u32,
    pub blocked_until: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminSession {
    pub token_hash: String,
    pub created_at: u64,
    pub last_seen_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminLockoutResult {
    None,
    Temporary(u64),
    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminStore {
    pub version: u32,
    #[serde(default)]
    pub password_salt: String,
    #[serde(default)]
    pub password_hash: String,
    #[serde(default)]
    pub sessions: Vec<AdminSession>,
    #[serde(default)]
    pub failed_attempts: HashMap<String, AdminFailedAttempt>,
    #[serde(default)]
    pub public_subdomain: Option<String>,
    #[serde(default)]
    pub board_port: Option<u16>,
    #[serde(default)]
    pub runner_projects: Vec<String>,
}

impl Default for AdminStore {
    fn default() -> Self {
        Self {
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
    serde_json::from_str(&text)
        .with_context(|| format!("corrupt admin store at {}", path.display()))
}

pub fn save(store: &AdminStore) -> anyhow::Result<()> {
    let path = admin_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_atomic(&path, &serde_json::to_string_pretty(store)?)?;
    Ok(())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buffer);
    hex::encode(buffer)
}

fn hash(salt: &str, value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(value.as_bytes());
    hex::encode(hasher.finalize())
}

/// Katban's admin password is an operational credential, not a shareable
/// Cat Chat phrase. Require length and mixed character classes.
pub fn validate_password(password: &str) -> Result<(), String> {
    let password = password.trim();
    if password.len() < 16 {
        return Err("Katban admin password must be at least 16 characters".to_string());
    }
    if password.chars().all(|character| character.is_ascii_digit()) {
        return Err("Katban admin password cannot be all digits".to_string());
    }
    if password
        .chars()
        .all(|character| character.is_ascii_alphanumeric())
    {
        return Err("Katban admin password should include punctuation or spaces".to_string());
    }
    let first = password.chars().next();
    if first.is_some_and(|character| password.chars().all(|value| value == character)) {
        return Err("Katban admin password cannot be a repeated character".to_string());
    }
    Ok(())
}

impl AdminStore {
    pub fn is_configured(&self) -> bool {
        !self.password_hash.is_empty()
    }

    pub fn set_password(&mut self, password: &str) {
        self.password_salt = random_hex(16);
        self.password_hash = hash(&self.password_salt, password.trim());
        self.failed_attempts.clear();
    }

    pub fn verify_password(&self, password: &str) -> bool {
        self.is_configured() && hash(&self.password_salt, password.trim()) == self.password_hash
    }

    pub fn mint_session(&mut self) -> String {
        let token = random_hex(32);
        let now = now_secs();
        self.sessions.push(AdminSession {
            token_hash: hash(ADMIN_COOKIE, &token),
            created_at: now,
            last_seen_at: now,
        });
        if self.sessions.len() > ADMIN_MAX_SESSIONS {
            self.sessions
                .drain(..self.sessions.len() - ADMIN_MAX_SESSIONS);
        }
        token
    }

    pub fn session_valid(&self, token: &str, now: u64) -> bool {
        self.sessions.iter().any(|session| {
            session.token_hash == hash(ADMIN_COOKIE, token)
                && now.saturating_sub(session.last_seen_at) <= ADMIN_SESSION_TTL_SECS
        })
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

    pub fn blocked_until(&self, ip: &str, now: u64) -> Option<u64> {
        self.failed_attempts
            .get(ip)
            .and_then(|attempt| attempt.blocked_until)
            .filter(|until| *until > now)
    }

    pub fn record_failed_attempt(&mut self, ip: &str, now: u64) -> AdminLockoutResult {
        let entry = self.failed_attempts.entry(ip.to_string()).or_default();
        if entry.blocked_until.is_some_and(|until| until > now) {
            return AdminLockoutResult::Blocked;
        }
        if entry.blocked_until.is_some_and(|until| until <= now) {
            *entry = AdminFailedAttempt::default();
        }
        if entry.locked_until.is_some_and(|until| until > now) {
            return AdminLockoutResult::Temporary(entry.locked_until.unwrap_or(now));
        }
        if entry.locked_until.is_some_and(|until| until <= now) {
            entry.locked_until = None;
            entry.count = 0;
        }
        entry.count += 1;
        if entry.count >= ADMIN_MAX_FAILED_ATTEMPTS {
            entry.count = 0;
            entry.strikes += 1;
            if entry.strikes >= ADMIN_MAX_STRIKES {
                entry.blocked_until = Some(now + ADMIN_BLOCK_SECS);
                entry.locked_until = None;
                return AdminLockoutResult::Blocked;
            }
            entry.locked_until = Some(now + ADMIN_LOCKOUT_SECS);
            return AdminLockoutResult::Temporary(now + ADMIN_LOCKOUT_SECS);
        }
        AdminLockoutResult::None
    }

    pub fn reset_failed_attempts(&mut self, ip: &str) {
        self.failed_attempts.remove(ip);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerState {
    pub configured: bool,
    pub mode: String,
    pub scheduled: Vec<String>,
    pub waiting: Vec<String>,
}

pub fn runner_state(store: &AdminStore) -> RunnerState {
    if store.runner_projects.is_empty() {
        return RunnerState {
            configured: false,
            mode: "none".to_string(),
            scheduled: Vec::new(),
            waiting: Vec::new(),
        };
    }
    let is_all = store.runner_projects.len() == 1 && store.runner_projects[0] == RUN_ALL;
    if is_all {
        let registered: HashSet<String> =
            crate::projects::registered_projects().into_iter().collect();
        let boards: HashSet<String> = crate::board::existing_projects().into_iter().collect();
        return RunnerState {
            configured: true,
            mode: "all".to_string(),
            scheduled: sorted(registered.iter().cloned().collect()),
            waiting: sorted(boards.difference(&registered).cloned().collect()),
        };
    }
    RunnerState {
        configured: true,
        mode: "list".to_string(),
        scheduled: sorted(store.runner_projects.clone()),
        waiting: Vec::new(),
    }
}

fn sorted(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_password_policy_is_independent_and_strong() {
        assert!(validate_password("short!").is_err());
        assert!(validate_password("sixteen-character!").is_ok());
        assert!(validate_password("sixteencharacters").is_err());
    }

    #[test]
    fn admin_sessions_expire_and_password_rotation_resets_failures() {
        let mut store = AdminStore::default();
        store.set_password("a-long-admin-password!");
        let token = store.mint_session();
        assert!(store.session_valid(&token, now_secs()));
        assert!(!store.session_valid(&token, now_secs() + ADMIN_SESSION_TTL_SECS + 1));
        let ip = "192.0.2.10";
        assert_eq!(
            store.record_failed_attempt(ip, now_secs()),
            AdminLockoutResult::None
        );
        store.set_password("another-long-admin-password!");
        assert!(store.failed_attempts.is_empty());
    }

    #[test]
    fn admin_lockout_is_independent_and_blocks_after_three_strikes() {
        let mut store = AdminStore::default();
        let ip = "192.0.2.11";
        for strike in 0..ADMIN_MAX_STRIKES {
            let mut result = AdminLockoutResult::None;
            for _ in 0..ADMIN_MAX_FAILED_ATTEMPTS {
                result = store.record_failed_attempt(ip, now_secs());
            }
            if strike + 1 < ADMIN_MAX_STRIKES {
                assert!(matches!(result, AdminLockoutResult::Temporary(_)));
                let entry = store.failed_attempts.get_mut(ip).unwrap();
                entry.locked_until = None;
            } else {
                assert_eq!(result, AdminLockoutResult::Blocked);
            }
        }
        assert!(store.blocked_until(ip, now_secs()).is_some());
        assert_eq!(
            store.record_failed_attempt(ip, now_secs()),
            AdminLockoutResult::Blocked
        );
        store.reset_failed_attempts(ip);
        assert!(store.blocked_until(ip, now_secs()).is_none());
    }
}
