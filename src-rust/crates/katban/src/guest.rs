//! Guest links (spec §6/§8): the "share a URL with friends" surface.
//!
//! A guest link is a password-protected entry point to a dedicated guest chat
//! server. The store persists to `~/.clawde/katban/links.json`:
//! - Passwords are never stored — only a salted SHA-256 hash (plus the salt).
//! - Device tokens (the "remember this device" cookie) are random 256-bit
//!   values stored only as hashes; the plaintext is handed to the browser once.
//! - Failed password attempts are tracked per IP with a lockout window.
//! - Links can expire (default 30 days) and be revoked individually.
//!
//! Notes for future hardening (documented, not blocking v1): bcrypt/argon2
//! would raise the cost of a stolen links.json; full encryption at rest is a
//! later slice. The lockout + per-device tokens keep v1 defensible.

use crate::caddy::write_atomic;
use anyhow::Context;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub const STORE_VERSION: u32 = 1;
pub const DEFAULT_EXPIRY_DAYS: u64 = 30;
pub const DEFAULT_MAX_CONCURRENT: usize = 2;
/// Wrong-password attempts allowed on the first strike before the lockout.
pub const MAX_FAILED_ATTEMPTS: u32 = 5;
/// Wrong-password attempts allowed on later strikes before the next lockout.
pub const MAX_FAILED_ATTEMPTS_SUBSEQUENT: u32 = 3;
/// Seconds an IP stays locked out after a strike (3 minutes).
pub const LOCKOUT_SECS: u64 = 180;
/// Lockout strikes before the IP is blocked permanently (5, then 3, then 3).
pub const MAX_STRIKES: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuestLink {
    pub id: String,
    pub name: String,
    pub password_salt: String,
    pub password_hash: String,
    pub created_at: u64,
    /// Unix seconds when the link stops working, or `None` for never.
    pub expires_at: Option<u64>,
    pub revoked: bool,
    /// Concurrent chat slots (guests chatting at once).
    pub max_concurrent: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub token_hash: String,
    pub label: String,
    pub created_at: u64,
    pub last_seen_at: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailedAttempt {
    pub count: u32,
    /// Unix seconds until the IP may try again.
    pub locked_until: Option<u64>,
    /// How many lockout windows this IP has served (5 -> 3 -> 3 ladder).
    #[serde(default)]
    pub strikes: u32,
    /// Set on the third strike: this IP may never try again.
    #[serde(default)]
    pub permanently_blocked: bool,
}

/// What `record_failed_attempt` decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockoutResult {
    /// No lockout yet; the attempt just counts.
    None,
    /// Locked out until this unix time.
    Temporary(u64),
    /// Permanently blocked.
    Permanent,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuestStore {
    pub version: u32,
    #[serde(default)]
    pub links: Vec<GuestLink>,
    /// link_id -> devices
    #[serde(default)]
    pub devices: HashMap<String, Vec<Device>>,
    /// ip -> failed attempts / lockout
    #[serde(default)]
    pub failed_attempts: HashMap<String, FailedAttempt>,
    /// Public subdomain the guest chat is exposed at through caddy (e.g.
    /// `chat.example.com`), set by `guest expose`.
    #[serde(default)]
    pub public_subdomain: Option<String>,
    /// Port the guest chat server binds on when the always-on unit runs it
    /// (set by `guest expose --port`). Persisted so a later `site expose`
    /// regenerates `katban.service` with the same port — otherwise the unit
    /// drifts back to the default while the caddy block still proxies the
    /// custom port.
    #[serde(default)]
    pub guest_port: Option<u16>,
}

pub fn links_path() -> PathBuf {
    crate::config::katban_data_dir().join("links.json")
}

pub fn load() -> anyhow::Result<GuestStore> {
    let path = links_path();
    if !path.exists() {
        return Ok(GuestStore::default());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read guest links at {}", path.display()))?;
    let store: GuestStore = serde_json::from_str(&text)
        .with_context(|| format!("corrupt guest links at {}", path.display()))?;
    Ok(store)
}

pub fn save(store: &GuestStore) -> anyhow::Result<()> {
    let path = links_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_atomic(&path, &serde_json::to_string_pretty(store)?)?;
    Ok(())
}

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(&buf)
}

/// Salted SHA-256 of `value`. Shared by guest links and the admin board
/// credential so passwords are never stored in plaintext on either surface.
pub(crate) fn hash(salt: &str, value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(value.as_bytes());
    hex::encode(hasher.finalize())
}

/// The shared wrong-password lockout ladder (the user's policy: 5 wrong ->
/// lock 3 min, 3 more -> lock 3 min, 3 more -> permanent). Used by both the
/// guest store and the admin board store so both surfaces enforce the same
/// ladder.
pub(crate) fn apply_failed_attempt(entry: &mut FailedAttempt, now: u64) -> LockoutResult {
    if entry.permanently_blocked {
        return LockoutResult::Permanent;
    }
    // Clear the counter if a previous lockout has lapsed.
    if let Some(until) = entry.locked_until {
        if now >= until {
            entry.count = 0;
            entry.locked_until = None;
        }
    }
    entry.count += 1;
    let threshold = if entry.strikes == 0 {
        MAX_FAILED_ATTEMPTS
    } else {
        MAX_FAILED_ATTEMPTS_SUBSEQUENT
    };
    if entry.count >= threshold {
        entry.strikes += 1;
        entry.count = 0;
        if entry.strikes >= MAX_STRIKES {
            entry.permanently_blocked = true;
            entry.locked_until = None;
            return LockoutResult::Permanent;
        }
        entry.locked_until = Some(now + LOCKOUT_SECS);
        return LockoutResult::Temporary(entry.locked_until.unwrap_or(now));
    }
    LockoutResult::None
}

/// Generate a memorable-enough random guest password (no lookalike chars).
pub fn generate_password() -> String {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyzABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let mut rng = rand::thread_rng();
    let mut password = String::with_capacity(12);
    for _ in 0..12 {
        let idx = rng.next_u32() as usize % ALPHABET.len();
        password.push(ALPHABET[idx] as char);
    }
    password
}

/// True when the link is usable right now (not revoked, not expired).
pub fn link_active(link: &GuestLink, now: u64) -> bool {
    !link.revoked && link.expires_at.is_none_or(|expiry| now < expiry)
}

impl GuestStore {
    pub fn link(&self, id: &str) -> Option<&GuestLink> {
        self.links.iter().find(|link| link.id == id)
    }

    pub fn link_mut(&mut self, id: &str) -> Option<&mut GuestLink> {
        self.links.iter_mut().find(|link| link.id == id)
    }

    /// Create a link. `password` is the shared secret the friend types on the
    /// login page; only its hash is stored. Returns the link id.
    pub fn create_link(
        &mut self,
        name: &str,
        password: &str,
        expires_at: Option<u64>,
        max_concurrent: usize,
    ) -> String {
        let id = random_hex(8);
        let salt = random_hex(16);
        let now = now_secs();
        self.links.push(GuestLink {
            id: id.clone(),
            name: name.trim().to_string(),
            password_salt: salt.clone(),
            password_hash: hash(&salt, password),
            created_at: now,
            expires_at,
            revoked: false,
            max_concurrent: if max_concurrent == 0 {
                DEFAULT_MAX_CONCURRENT
            } else {
                max_concurrent
            },
        });
        id
    }

    pub fn verify_password(&self, link: &GuestLink, password: &str) -> bool {
        hash(&link.password_salt, password) == link.password_hash
    }

    /// Rotate a link's password: re-salt and re-hash `new_password` in place.
    /// Returns `false` if the link does not exist. Existing device tokens stay
    /// valid (they are authenticated separately from the shared password).
    pub fn set_password(&mut self, id: &str, new_password: &str) -> bool {
        let Some(link) = self.link_mut(id) else {
            return false;
        };
        link.password_salt = random_hex(16);
        link.password_hash = hash(&link.password_salt, new_password);
        true
    }

    /// Mint a fresh device token for a link after a successful login.
    /// Returns the plaintext token (shown to the browser once as a cookie);
    /// only the hash is stored. The per-link device list is capped (oldest
    /// dropped) so a long-lived link can't grow links.json without bound.
    pub fn mint_device_token(&mut self, link_id: &str, label: &str) -> Option<String> {
        let token = random_hex(32);
        let now = now_secs();
        let devices = self.devices.entry(link_id.to_string()).or_default();
        devices.push(Device {
            token_hash: hash(link_id, &token),
            label: label.to_string(),
            created_at: now,
            last_seen_at: now,
        });
        if devices.len() > crate::guest_server::MAX_DEVICES_PER_LINK {
            // Tokens are pushed in time order, so the front of the list is
            // always the oldest — drop from there to keep the newest cap.
            let excess = devices.len() - crate::guest_server::MAX_DEVICES_PER_LINK;
            devices.drain(..excess);
        }
        Some(token)
    }

    pub fn device_valid(&self, link_id: &str, token: &str) -> bool {
        self.devices
            .get(link_id)
            .is_some_and(|devices| devices.iter().any(|d| d.token_hash == hash(link_id, token)))
    }

    pub fn touch_device(&mut self, link_id: &str, token: &str) {
        let token_hash = hash(link_id, token);
        if let Some(devices) = self.devices.get_mut(link_id) {
            if let Some(device) = devices.iter_mut().find(|d| d.token_hash == token_hash) {
                device.last_seen_at = now_secs();
            }
        }
    }

    pub fn revoke_link(&mut self, id: &str) -> bool {
        let Some(link) = self.link_mut(id) else {
            return false;
        };
        link.revoked = true;
        self.devices.remove(id);
        true
    }

    /// Remove expired, revoked, or missing links and their devices.
    pub fn prune(&mut self, now: u64) {
        let active_ids: Vec<String> = self
            .links
            .iter()
            .filter(|link| link_active(link, now))
            .map(|link| link.id.clone())
            .collect();
        self.links.retain(|link| link_active(link, now));
        self.devices.retain(|id, _| active_ids.contains(id));
    }

    /// Record a wrong password from an IP. Lockout ladder (per the user's
    /// policy): 5 wrong -> lock 3 min, then 3 more -> lock 3 min, then 3 more
    /// -> permanent block.
    pub fn record_failed_attempt(&mut self, ip: &str) -> LockoutResult {
        let now = now_secs();
        let entry = self.failed_attempts.entry(ip.to_string()).or_default();
        apply_failed_attempt(entry, now)
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

    /// Admin escape hatch: clear an IP's failed attempts and any permanent
    /// block. `guest unblock <IP>`.
    pub fn reset_failed_attempts(&mut self, ip: &str) {
        self.failed_attempts.remove(ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn with_home<T>(dir: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("CLAWDE_HOME").ok();
        std::env::set_var("CLAWDE_HOME", dir);
        let result = f();
        match previous {
            Some(value) => std::env::set_var("CLAWDE_HOME", value),
            None => std::env::remove_var("CLAWDE_HOME"),
        }
        result
    }

    #[test]
    fn create_verify_password_and_revoke() {
        let mut store = GuestStore::default();
        let id = store.create_link("friends", "hunter2", None, 3);
        let link = store.link(&id).unwrap();
        assert!(store.verify_password(link, "hunter2"));
        assert!(!store.verify_password(link, "wrong"));
        assert!(link_active(link, now_secs()));
        assert!(store.revoke_link(&id));
        assert!(!link_active(store.link(&id).unwrap(), now_secs()));
        assert!(!store.revoke_link("missing"));
    }

    #[test]
    fn rotate_password_rehashes_and_old_password_fails() {
        let mut store = GuestStore::default();
        let id = store.create_link("friends", "old-pass", None, 2);
        // Device tokens survive a password rotation.
        let token = store.mint_device_token(&id, "phone").unwrap();
        assert!(store.set_password(&id, "new-pass"));
        let link = store.link(&id).unwrap();
        assert!(store.verify_password(link, "new-pass"));
        assert!(!store.verify_password(link, "old-pass"));
        assert!(store.device_valid(&id, &token));
        // Missing link -> false, no crash.
        assert!(!store.set_password("nope", "x"));
    }

    #[test]
    fn expiry_and_prune() {
        let mut store = GuestStore::default();
        let now = now_secs();
        let short = store.create_link("short", "pw", Some(now + 10), 2);
        let never = store.create_link("never", "pw", None, 2);
        let past = store.create_link("past", "pw", Some(now - 1), 2);
        assert!(link_active(store.link(&short).unwrap(), now));
        assert!(link_active(store.link(&never).unwrap(), now));
        assert!(!link_active(store.link(&past).unwrap(), now));
        store.prune(now);
        assert!(store.link(&short).is_some());
        assert!(store.link(&never).is_some());
        assert!(store.link(&past).is_none());
    }

    #[test]
    fn device_list_is_capped_per_link() {
        let mut store = GuestStore::default();
        let id = store.create_link("friends", "pw", None, 2);
        let cap = crate::guest_server::MAX_DEVICES_PER_LINK;
        for _ in 0..cap + 5 {
            store.mint_device_token(&id, "device");
        }
        let devices = store.devices.get(&id).unwrap();
        assert_eq!(devices.len(), cap);
        // The newest token survives the cap.
        let newest = store.mint_device_token(&id, "device");
        let devices = store.devices.get(&id).unwrap();
        assert_eq!(devices.len(), cap);
        assert!(newest.is_some_and(|token| store.device_valid(&id, &token)));
    }

    #[test]
    fn device_tokens_are_hashed_and_revocable() {
        let mut store = GuestStore::default();
        let id = store.create_link("friends", "pw", None, 2);
        let token = store.mint_device_token(&id, "phone").unwrap();
        assert!(store.device_valid(&id, &token));
        assert!(!store.device_valid(&id, "forged"));
        // The plaintext token never appears in the store.
        let serialized = serde_json::to_string(&store).unwrap();
        assert!(!serialized.contains(&token));
        store.revoke_link(&id);
        assert!(!store.device_valid(&id, &token));
    }

    fn lapse(store: &mut GuestStore, ip: &str) {
        // Simulate a lockout expiring so the ladder advances without sleeping.
        if let Some(entry) = store.failed_attempts.get_mut(ip) {
            entry.locked_until = None;
            entry.count = 0;
        }
    }

    #[test]
    fn lockout_ladder_5_then_3_then_3_then_permanent() {
        let mut store = GuestStore::default();
        let ip = "203.0.113.7";

        // Strike 1: 5 wrong attempts -> 3-minute lock.
        let mut result = LockoutResult::None;
        for _ in 0..MAX_FAILED_ATTEMPTS {
            result = store.record_failed_attempt(ip);
        }
        let LockoutResult::Temporary(until) = result else {
            panic!("fifth attempt should lock out, got {result:?}");
        };
        assert_eq!(store.locked_until(ip, now_secs()), Some(until));
        assert_eq!(until, now_secs() + LOCKOUT_SECS);

        // Lock lapses; 3 more wrong attempts -> second 3-minute lock.
        lapse(&mut store, ip);
        for _ in 0..MAX_FAILED_ATTEMPTS_SUBSEQUENT {
            result = store.record_failed_attempt(ip);
        }
        let LockoutResult::Temporary(until2) = result else {
            panic!("third subsequent attempt should lock again, got {result:?}");
        };
        assert_eq!(until2, now_secs() + LOCKOUT_SECS);

        // Lock lapses; 3 more wrong attempts -> permanent block.
        lapse(&mut store, ip);
        for _ in 0..MAX_FAILED_ATTEMPTS_SUBSEQUENT {
            result = store.record_failed_attempt(ip);
        }
        assert_eq!(result, LockoutResult::Permanent);
        assert!(store.is_permanently_blocked(ip));
        assert!(store.locked_until(ip, now_secs()).is_none());

        // Even after the (nonexistent) lock would lapse, it stays permanent.
        assert!(store.is_permanently_blocked(ip));
        assert_eq!(store.record_failed_attempt(ip), LockoutResult::Permanent);

        // Admin escape hatch clears everything.
        store.reset_failed_attempts(ip);
        assert!(!store.is_permanently_blocked(ip));
    }

    #[test]
    fn one_strike_does_not_escalate_the_threshold() {
        let mut store = GuestStore::default();
        let ip = "198.51.100.9";
        for _ in 0..MAX_FAILED_ATTEMPTS {
            store.record_failed_attempt(ip);
        }
        // A single wrong attempt after the lock lapses stays under the 3
        // needed for the second strike.
        lapse(&mut store, ip);
        assert_eq!(store.record_failed_attempt(ip), LockoutResult::None);
        assert_eq!(store.record_failed_attempt(ip), LockoutResult::None);
        assert_eq!(
            store.record_failed_attempt(ip),
            LockoutResult::Temporary(now_secs() + LOCKOUT_SECS)
        );
    }

    #[test]
    fn save_and_load_round_trips() {
        let tmp = tempdir().unwrap();
        with_home(tmp.path(), || {
            let mut store = GuestStore::default();
            let id = store.create_link("friends", "pw", None, 2);
            let token = store.mint_device_token(&id, "phone").unwrap();
            save(&store).unwrap();
            let loaded = load().unwrap();
            assert!(loaded.link(&id).is_some());
            assert!(loaded.device_valid(&id, &token));
        });
    }

    #[test]
    fn passwords_never_stored_in_plaintext() {
        let mut store = GuestStore::default();
        store.create_link("friends", "hunter2-secret", None, 2);
        let serialized = serde_json::to_string(&store).unwrap();
        assert!(!serialized.contains("hunter2-secret"));
        assert!(serialized.contains("passwordHash"));
    }

    #[test]
    fn generated_passwords_are_unique_and_usable() {
        let a = generate_password();
        let b = generate_password();
        assert_ne!(a, b);
        assert_eq!(a.len(), 12);
        // The alphabet excludes lookalikes.
        for ch in a.chars() {
            assert!(!"0O1lI".contains(ch));
        }
    }
}
