// auth_store.rs — JSON-based credential store at ~/.clawde/auth.json.
//
// Stores API keys and OAuth tokens for providers so users don't have to rely
// solely on environment variables.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Monotonic counter for unique tmp filenames. Two saves racing in the same
/// process must never share a `.auth.json.clawde-tmp-*` path or one rename
/// would steal the other's file (ENOENT race).
static AUTH_TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Short-lived inter-process lock for the compare-and-rename portion of an
/// auth-store save. The hash check alone is not enough: two writers can both
/// observe the same bytes and then race their final renames.
struct AuthFileLock {
    path: PathBuf,
}

impl AuthFileLock {
    fn acquire(auth_path: &Path) -> Option<Self> {
        let path = auth_path.with_file_name("auth.json.lock");
        let parent = path.parent()?;
        let _ = std::fs::create_dir_all(parent);
        for _ in 0..200 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    // The lock contains only an owner PID. Keep it private and
                    // flush it before publishing the guard to other writers.
                    crate::accounts::set_user_only_perms(&path);
                    use std::io::Write;
                    if file
                        .write_all(std::process::id().to_string().as_bytes())
                        .and_then(|_| file.sync_all())
                        .is_err()
                    {
                        let _ = std::fs::remove_file(&path);
                        return None;
                    }
                    return Some(Self { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Fail closed after the bounded wait. Never delete an
                    // old lock by timestamp: a slow live writer must not be
                    // robbed by a second writer. A future explicit recovery
                    // command can remove an abandoned lock after inspection.
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(_) => return None,
            }
        }
        None
    }
}

impl Drop for AuthFileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A stored credential for a provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum StoredCredential {
    #[serde(rename = "api")]
    ApiKey { key: String },
    #[serde(rename = "oauth")]
    OAuthToken {
        access: String,
        refresh: String,
        expires: u64,
    },
}

/// Persistent credential store backed by `~/.clawde/auth.json`.
///
/// Supports both single-key storage (`credentials`) and multi-key storage
/// (`keys`). The two maps are independent — a provider can have a single
/// credential *and* multiple keys, or just one or the other.
///
/// Backward-compatible both ways: old files with only `credentials` load
/// (`keys` defaults to empty), and keys-only files load too (`credentials`
/// defaults to empty). New files omit the `keys` field when it is empty.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AuthStore {
    /// Single-key credentials. `#[serde(default)]` keeps files that only carry
    /// a `keys` map loadable — a missing `credentials` field must never make
    /// the whole store look corrupt (that would hide every stored key).
    #[serde(default)]
    pub credentials: HashMap<String, StoredCredential>,
    /// Multi-key storage: a provider can have multiple API keys. The system
    /// rotates through these automatically when one is exhausted.
    ///
    /// Serialisation: `#[serde(default)]` and `#[serde(skip_serializing_if)]`
    /// ensure that old auth.json files without this field are loaded correctly
    /// and that the field is omitted from saved files when empty.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub keys: HashMap<String, Vec<String>>,
    /// True when [`Self::load`] fell back to an empty store because the file
    /// was corrupt or unreadable. Guards [`Self::save`] from persisting that
    /// fallback state over the real (possibly recoverable) file.
    #[serde(skip)]
    from_fallback: bool,
    /// Human-readable reason the last [`Self::load`] could not read a valid
    /// store (unreadable or corrupt file). `None` when the last load was
    /// clean. Lets callers (no-key hints, `/keys health`) tell the user the
    /// store itself failed instead of claiming no keys are configured.
    #[serde(skip)]
    pub load_error: Option<String>,
    /// True when the on-disk file failed to parse. [`Self::save`] then backs
    /// the original up as `auth.json.corrupt-<timestamp>` before the first
    /// overwrite, so keys still recoverable from the broken file are never
    /// silently destroyed.
    #[serde(skip)]
    file_corrupt: bool,
    /// SHA-256 of the file contents observed by the last successful load.
    /// Saves compare this with the current file before replacing it, so a
    /// stale long-lived TUI instance cannot erase keys written by another
    /// process or session.
    #[serde(skip)]
    loaded_file_hash: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

/// Result of [`AuthStore::salvage_auth_store`]: the recoverable maps plus a
/// human-readable reason for every dropped entry.
struct SalvageResult {
    credentials: HashMap<String, StoredCredential>,
    keys: HashMap<String, Vec<String>>,
    dropped: Vec<String>,
}

impl AuthStore {
    /// Return whether a provider is part of Clawde's composite free catalog.
    /// Kept in core so every credential-writing surface can enforce the same
    /// destination without depending on the API crate. `opencode-go` is the
    /// compatibility alias for the shared OpenCode Zen/Go key slot; it is not
    /// a separate catalog entry.
    pub fn is_free_upstream(provider_id: &str) -> bool {
        matches!(
            provider_id,
            "github-copilot"
                | "cline"
                | "openrouter"
                | "huggingface"
                | "cerebras"
                | "nvidia"
                | "groq"
                | "google"
                | "cloudflare"
                | "mistral"
                | "cohere"
                | "opencode-zen"
                | "opencode-go"
                | "zai"
                | "sambanova"
        )
    }

    /// Normalize a free-provider key pool. Free resolver entries reject
    /// values shorter than eight characters, so the canonical store must use
    /// the same boundary and must never retain placeholder slots.
    fn clean_free_keys(keys: impl IntoIterator<Item = String>) -> Vec<String> {
        keys.into_iter()
            .map(|key| key.trim().to_string())
            .filter(|key| key.len() >= 8)
            .fold(Vec::new(), |mut out, key| {
                if !out.contains(&key) {
                    out.push(key);
                }
                out
            })
    }

    /// Move a legacy single API credential for a free upstream into the
    /// canonical multi-key store. The operation is idempotent and does not
    /// import environment variables or cooldown-state keys.
    ///
    /// Returns true when the in-memory store changed. Callers can persist once
    /// after applying several migrations.
    pub fn migrate_free_credential_to_keys(&mut self, provider_id: &str) -> bool {
        if !Self::is_free_upstream(provider_id) {
            return false;
        }
        let legacy = match self.credentials.get(provider_id).cloned() {
            Some(StoredCredential::ApiKey { key }) => Some(key),
            _ => None,
        };
        let Some(legacy) = legacy else {
            return false;
        };

        let mut keys = Self::clean_free_keys(self.keys.remove(provider_id).unwrap_or_default());
        let legacy = legacy.trim().to_string();
        if legacy.len() >= 8 && !keys.contains(&legacy) {
            keys.insert(0, legacy);
        }
        if keys.is_empty() {
            self.keys.remove(provider_id);
        } else {
            self.keys.insert(provider_id.to_string(), keys);
        }
        // Even an invalid/empty legacy API credential is removed from the
        // credentials map: free API credentials have one canonical home.
        self.credentials.remove(provider_id);
        true
    }

    /// Migrate every legacy free-provider API credential into `keys`, normalize
    /// existing free key pools, and persist the canonical result once. OAuth
    /// and non-free credentials stay in `credentials` untouched.
    pub fn migrate_legacy_free_credentials(&mut self) -> bool {
        let providers: Vec<String> = self.credentials.keys().cloned().collect();
        let mut changed = false;
        for provider in providers {
            changed |= self.migrate_free_credential_to_keys(&provider);
        }

        let free_key_providers: Vec<String> = self
            .keys
            .keys()
            .filter(|provider| Self::is_free_upstream(provider))
            .cloned()
            .collect();
        for provider in free_key_providers {
            let old = self.keys.remove(&provider).unwrap_or_default();
            let clean = Self::clean_free_keys(old.clone());
            if clean != old {
                changed = true;
            }
            if clean.is_empty() {
                // Removing the empty entry is canonical and avoids a false
                // "configured" signal in dialogs and diagnostics.
                continue;
            }
            self.keys.insert(provider, clean);
        }

        if changed {
            self.save();
        }
        changed
    }

    /// Canonical free-provider key write. Free catalog keys always land in the
    /// multi-key rotation map; non-free providers retain the legacy `set`
    /// credential path. Duplicate keys are ignored.
    pub fn set_free_key(&mut self, provider_id: &str, key: String) -> bool {
        if !Self::is_free_upstream(provider_id) {
            return false;
        }
        // Migrate/remove any legacy credential before validating the new
        // input, so even an invalid replacement cannot leave a second free
        // credential destination behind. Normalize the existing pool first as
        // well, so a rejected replacement cannot preserve malformed slots.
        let mut changed = self.migrate_free_credential_to_keys(provider_id);
        let old = self.keys.remove(provider_id).unwrap_or_default();
        let mut keys = Self::clean_free_keys(old.clone());
        changed |= keys != old;
        let key = key.trim().to_string();
        if key.len() >= 8 && !keys.contains(&key) {
            keys.push(key);
            changed = true;
        }
        if keys.is_empty() {
            self.keys.remove(provider_id);
        } else {
            self.keys.insert(provider_id.to_string(), keys);
        }
        if changed {
            self.save();
        }
        changed
    }

    /// Canonical replacement of a free provider's full rotation pool.
    pub fn set_free_keys(&mut self, provider_id: &str, keys: Vec<String>) -> bool {
        if !Self::is_free_upstream(provider_id) {
            return false;
        }
        let clean = Self::clean_free_keys(keys);
        let old = self.keys.remove(provider_id).unwrap_or_default();
        let removed_legacy = matches!(
            self.credentials.get(provider_id),
            Some(StoredCredential::ApiKey { .. })
        ) && self.credentials.remove(provider_id).is_some();
        let changed = old != clean || removed_legacy;
        if clean.is_empty() {
            self.keys.remove(provider_id);
        } else {
            self.keys.insert(provider_id.to_string(), clean);
        }
        if changed {
            self.save();
        }
        changed
    }

    /// Fingerprint the exact serialized bytes on disk. This is an optimistic
    /// concurrency token, not a credential-derived identifier exposed to users.
    fn file_hash(raw: &str) -> String {
        hex::encode(Sha256::digest(raw.as_bytes()))
    }

    /// Path to the auth store file.
    pub fn path() -> PathBuf {
        crate::config::Settings::config_dir().join("auth.json")
    }

    /// Read the current OpenCode CLI API key without modifying either auth
    /// store. OpenCode writes a flat map at `~/.local/share/opencode/auth.json`
    /// with records such as `{ "type": "api", "key": "..." }` keyed by the
    /// provider ID. The current Zen ID is `opencode`; the two Clawde IDs are
    /// accepted as compatibility aliases because OpenCode has changed naming
    /// across releases.
    ///
    /// This deliberately does not accept the generic `{ "providers": ...,
    /// "apiKey": ... }` shape: importing arbitrary provider entries could send
    /// an OpenAI or Anthropic key to Zen by mistake. File and parse failures are
    /// silent and fail closed; credential values are never logged.
    pub fn opencode_cli_api_key() -> Option<String> {
        let path = dirs::data_local_dir()?.join("opencode").join("auth.json");
        let raw = std::fs::read_to_string(path).ok()?;
        Self::parse_opencode_cli_api_key(&raw)
    }

    fn parse_opencode_cli_api_key(raw: &str) -> Option<String> {
        let value = serde_json::from_str::<serde_json::Value>(raw).ok()?;
        for provider_id in ["opencode", "opencode-zen", "opencode-go"] {
            let Some(record) = value
                .get(provider_id)
                .and_then(serde_json::Value::as_object)
            else {
                continue;
            };
            if record.get("type").and_then(serde_json::Value::as_str) != Some("api") {
                continue;
            }
            let Some(key) = record.get("key").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let key = key.trim();
            if key.len() >= 8 {
                return Some(key.to_string());
            }
        }
        None
    }

    /// Load the store from disk (returns default if missing or invalid).
    ///
    /// A partially corrupt file is not all-or-nothing: whichever `credentials`
    /// and `keys` entries still parse are recovered, the rest are dropped with
    /// a recorded reason ([`Self::load_error`]), and the store is marked as a
    /// fallback so [`Self::save`] cannot clobber the original before backing
    /// it up.
    pub fn load() -> Self {
        let path = Self::path();
        if !path.exists() {
            return Self::default();
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("failed to read auth store at {}: {}", path.display(), e);
                return Self::from_fallback(format!("failed to read {}: {}", path.display(), e));
            }
        };
        let observed_hash = Self::file_hash(&raw);
        match serde_json::from_str::<AuthStore>(&raw) {
            Ok(store) => {
                *store.loaded_file_hash.lock().unwrap() = Some(observed_hash);
                // Do not mutate or persist during load. Callers that construct
                // the free provider invoke the explicit migration method before
                // building the chain, so loading remains side-effect free.
                store
            }
            Err(e) => {
                let salvaged = Self::salvage_auth_store(&raw);
                let mut store = Self {
                    from_fallback: true,
                    file_corrupt: true,
                    load_error: None,
                    credentials: salvaged.credentials,
                    keys: salvaged.keys,
                    loaded_file_hash: std::sync::Arc::new(std::sync::Mutex::new(Some(
                        observed_hash,
                    ))),
                };
                let dropped = salvaged.dropped;
                let summary = if dropped.is_empty() {
                    "no entries could be recovered".to_string()
                } else {
                    format!(
                        "recovered {} credential(s) and {} key slot(s); dropped: {}",
                        store.credentials.len(),
                        store.keys.len(),
                        dropped.join("; ")
                    )
                };
                let msg = format!(
                    "auth store at {} is corrupt ({}); {}. The corrupt file is backed up \
                     before the next save; fix or remove it to restore the dropped entries.",
                    path.display(),
                    e,
                    summary
                );
                tracing::warn!("{msg}");
                store.load_error = Some(msg);
                store
            }
        }
    }

    /// An empty store that marks itself as having failed to load from disk,
    /// so [`Self::save`] will refuse to clobber the real file. Records why the
    /// load failed for user-facing diagnostics.
    fn from_fallback(reason: impl Into<String>) -> Self {
        Self {
            from_fallback: true,
            load_error: Some(reason.into()),
            ..Self::default()
        }
    }

    /// Best-effort recovery of a corrupt auth store: parse the `credentials`
    /// and `keys` maps independently so one malformed entry cannot hide the
    /// other keys.
    fn salvage_auth_store(raw: &str) -> SalvageResult {
        let mut credentials = HashMap::new();
        let mut keys = HashMap::new();
        let mut dropped = Vec::new();

        let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
            dropped.push("file is not valid JSON".to_string());
            return SalvageResult {
                credentials,
                keys,
                dropped,
            };
        };

        if let Some(obj) = value.get("credentials").and_then(|v| v.as_object()) {
            for (provider, entry) in obj {
                match serde_json::from_value::<StoredCredential>(entry.clone()) {
                    Ok(cred) => {
                        credentials.insert(provider.clone(), cred);
                    }
                    Err(e) => dropped.push(format!("credentials[{provider}]: {e}")),
                }
            }
        }
        if let Some(obj) = value.get("keys").and_then(|v| v.as_object()) {
            for (provider, entries) in obj {
                match serde_json::from_value::<Vec<String>>(entries.clone()) {
                    Ok(list) => {
                        keys.insert(provider.clone(), list);
                    }
                    Err(e) => dropped.push(format!("keys[{provider}]: {e}")),
                }
            }
        }
        SalvageResult {
            credentials,
            keys,
            dropped,
        }
    }

    /// Reload state from disk, discarding any in-memory changes.
    ///
    /// Long-lived `AuthStore` instances (e.g. held by the TUI) go stale when
    /// another process writes `auth.json`. Mutating a stale snapshot and
    /// saving it would clobber the newer on-disk keys, so call this
    /// immediately before any read-modify-write that originates from a long-
    /// lived instance.
    pub fn reload(&mut self) {
        *self = Self::load();
    }

    /// Persist the store to disk (best-effort).
    ///
    /// Writes to a temp file then renames over the destination so a crash or
    /// disk-full mid-write can never truncate `auth.json` (which would
    /// silently wipe the user's stored credentials on the next load).
    ///
    /// Refuses to write when the in-memory store is an empty fallback that
    /// failed to load from disk — overwriting the real file would destroy
    /// keys that may still be recoverable from it.
    pub fn save(&self) {
        let path = Self::path();
        if self.from_fallback && self.credentials.is_empty() && self.keys.is_empty() {
            tracing::warn!(
                "refusing to persist empty auth store over existing {} (store failed to load); \
                 not saving",
                path.display()
            );
            return;
        }

        // Serialize the compare-and-rename sequence across Clawde processes.
        // The guard is held until after the final rename below.
        let Some(_file_lock) = AuthFileLock::acquire(&path) else {
            tracing::warn!(
                "refusing to save auth store at {}; another writer holds the lock",
                path.display()
            );
            return;
        };

        // Optimistic concurrency guard. A long-lived caller may have loaded
        // auth.json before another process (or another Clawde session) saved
        // newer keys. Never replace a changed file with that stale snapshot.
        let current_exists = path.exists();
        let current_hash = std::fs::read_to_string(&path)
            .ok()
            .map(|raw| Self::file_hash(&raw));
        let expected_hash = self.loaded_file_hash.lock().unwrap().clone();
        let changed = match (expected_hash.as_ref(), current_hash.as_ref()) {
            (None, None) => current_exists,
            (Some(expected), Some(current)) => expected != current,
            _ => true,
        };
        if changed {
            tracing::warn!(
                "refusing to overwrite changed auth store at {}; reload before saving",
                path.display()
            );
            return;
        }

        // One-time backup: when the last load found a corrupt file, preserve
        // the original (possibly recoverable) content before the first
        // overwrite. If the file has since been repaired on disk, leave it
        // alone and discard the stale in-memory state instead.
        if self.file_corrupt && path.exists() {
            let still_corrupt = std::fs::read_to_string(&path)
                .map(|s| serde_json::from_str::<AuthStore>(&s).is_err())
                .unwrap_or(true);
            if still_corrupt {
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let backup = path.with_file_name(format!("auth.json.corrupt-{stamp}"));
                if std::fs::rename(&path, &backup).is_ok() {
                    tracing::warn!(
                        "backed up corrupt auth store to {} before overwriting",
                        backup.display()
                    );
                }
            } else {
                tracing::warn!(
                    "auth store at {} was repaired on disk; discarding stale in-memory state",
                    path.display()
                );
                return;
            }
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
            crate::accounts::set_user_only_dir_perms(parent);
        }
        let json = match serde_json::to_string_pretty(self) {
            Ok(j) => j,
            Err(_) => return,
        };
        let tmp = path.with_file_name(format!(
            ".auth.json.clawde-tmp-{}-{}",
            std::process::id(),
            AUTH_TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        if std::fs::write(&tmp, &json).is_ok() {
            // auth.json holds API keys + OAuth tokens. Lock the temp file to
            // 0o600 *before* the rename so the live credential file is never
            // even momentarily world/group readable (issue #212).
            crate::accounts::set_user_only_perms(&tmp);
            if std::fs::rename(&tmp, &path).is_err() {
                let _ = std::fs::remove_file(&tmp);
            } else {
                *self.loaded_file_hash.lock().unwrap() = Some(Self::file_hash(&json));
            }
        }
    }

    /// Store a credential for the given provider (persists immediately).
    pub fn set(&mut self, provider_id: &str, cred: StoredCredential) {
        if Self::is_free_upstream(provider_id) {
            if let StoredCredential::ApiKey { key } = cred {
                self.set_free_key(provider_id, key);
            } else {
                // Free catalog OAuth credentials (currently only the
                // GitHub-Copilot flow) remain in the credential map; only API
                // keys are canonicalized into rotation slots.
                self.credentials.insert(provider_id.to_string(), cred);
                self.save();
            }
            return;
        }
        self.credentials.insert(provider_id.to_string(), cred);
        self.save();
    }

    /// Get the stored credential for a provider.
    pub fn get(&self, provider_id: &str) -> Option<&StoredCredential> {
        self.credentials.get(provider_id)
    }

    /// Remove all credentials and canonical key slots for a provider
    /// (persists immediately). For free providers this is the destructive
    /// logout/remove operation; callers that only need to discard a legacy
    /// single credential should use [`Self::remove_credential`].
    pub fn remove(&mut self, provider_id: &str) {
        self.credentials.remove(provider_id);
        if Self::is_free_upstream(provider_id) {
            self.keys.remove(provider_id);
        }
        self.save();
    }

    /// Remove only a legacy API-key credential while preserving any canonical
    /// multi-key pool and OAuth credentials. This is used when a rotation pool
    /// has just been written and the old API entry must be discarded without
    /// deleting keys or an unrelated OAuth token.
    pub fn remove_credential(&mut self, provider_id: &str) -> bool {
        let remove = matches!(
            self.credentials.get(provider_id),
            Some(StoredCredential::ApiKey { .. })
        );
        let removed = remove && self.credentials.remove(provider_id).is_some();
        if removed {
            self.save();
        }
        removed
    }

    // -----------------------------------------------------------------------
    // Multi-key helpers
    // -----------------------------------------------------------------------

    /// Replace all keys for a provider (persists immediately).
    ///
    /// Empty keys in the input are stripped. If the resulting list is empty the
    /// provider's key entry is removed entirely.
    pub fn set_keys(&mut self, provider_id: &str, keys: Vec<String>) {
        if Self::is_free_upstream(provider_id) {
            self.set_free_keys(provider_id, keys);
            return;
        }
        let clean: Vec<String> = keys.into_iter().filter(|k| !k.is_empty()).collect();
        if clean.is_empty() {
            self.keys.remove(provider_id);
        } else {
            self.keys.insert(provider_id.to_string(), clean);
        }
        self.save();
    }

    /// Append a single key to the provider's key list (persists immediately).
    /// Silently ignores empty keys.
    pub fn add_key(&mut self, provider_id: &str, key: String) {
        if Self::is_free_upstream(provider_id) {
            self.set_free_key(provider_id, key);
            return;
        }
        if key.is_empty() {
            return;
        }
        self.keys
            .entry(provider_id.to_string())
            .or_default()
            .push(key);
        self.save();
    }

    /// Remove the key at `index` for a provider (persists immediately).
    /// Returns `true` if a key was removed, `false` if the index was out of
    /// bounds or the provider has no keys.
    pub fn remove_key(&mut self, provider_id: &str, index: usize) -> bool {
        let removed = self
            .keys
            .get_mut(provider_id)
            .and_then(|keys| {
                if index < keys.len() {
                    Some(keys.remove(index))
                } else {
                    None
                }
            })
            .is_some();
        if removed {
            // Clean up empty vectors.
            if self.keys.get(provider_id).is_none_or(|k| k.is_empty()) {
                self.keys.remove(provider_id);
            }
            self.save();
        }
        removed
    }

    /// Get all keys stored for a provider, or `None` if none are configured.
    pub fn keys_for(&self, provider_id: &str) -> Option<&[String]> {
        self.keys.get(provider_id).map(|v| v.as_slice())
    }

    /// Build a deduplicated rotation pool by merging an existing single key
    /// into a list of prior rotation keys together with a freshly typed key.
    ///
    /// Order is preserved: `existing` is first, then `prior` entries in
    /// their original order (with duplicates dropped), then `new_key`
    /// (skipped if it already appears in the merged list). The caller is
    /// expected to call [`Self::set_keys`] with the returned vector and
    /// remove the legacy single-key credential afterwards.
    pub fn merge_keys_for_rotation(existing: &str, prior: &[String], new_key: &str) -> Vec<String> {
        let mut merged: Vec<String> = Vec::with_capacity(prior.len() + 2);
        if !existing.is_empty() {
            merged.push(existing.to_string());
        }
        for k in prior {
            if !merged.iter().any(|m| m == k) {
                merged.push(k.clone());
            }
        }
        if !new_key.is_empty() && !merged.contains(&new_key.to_string()) {
            merged.push(new_key.to_string());
        }
        merged
    }

    // -----------------------------------------------------------------------
    // Key resolution
    // -----------------------------------------------------------------------

    /// Get the API key for a provider, checking stored credentials first, then
    /// the multi-key store, then falling back to the relevant environment
    /// variable.
    ///
    /// Precedence:
    ///   1. `credentials[provider_id]` — a single stored credential (legacy)
    ///   2. `keys[provider_id][0]` — first key from the multi-key store
    ///   3. Environment variable
    pub fn api_key_for(&self, provider_id: &str) -> Option<String> {
        // Check stored credentials first. Whitespace-only values are treated
        // as absent so a blank slot never resolves to an apparently
        // "configured" key that 401s confusingly.
        if let Some(stored) = self.get(provider_id) {
            match stored {
                StoredCredential::ApiKey { key } if !Self::is_free_upstream(provider_id) => {
                    let key = key.trim();
                    if !key.is_empty() {
                        return Some(key.to_string());
                    }
                }
                StoredCredential::OAuthToken {
                    access, refresh, ..
                } if provider_id == "github-copilot" => {
                    let refresh = refresh.trim();
                    if !refresh.is_empty() {
                        return Some(refresh.to_string());
                    }
                    let access = access.trim();
                    if !access.is_empty() {
                        return Some(access.to_string());
                    }
                }
                _ => {}
            }
        }
        // Multi-key store: first non-empty, whitespace-trimmed key (mirrors
        // the >=8-char placeholder guard the free resolvers apply).
        if let Some(first) = self
            .keys
            .get(provider_id)
            .and_then(|k| k.iter().map(|s| s.trim()).find(|k| !k.is_empty()))
        {
            return Some(first.to_string());
        }
        // Fall back to environment variable.
        //
        // These mappings must match the env var each provider's adapter
        // actually reads in `crates/api/src/providers/openai_compat_providers.rs`
        // (and the bespoke adapters next to it). When they drift, keys that
        // were exported via env vars look "configured" to the dialog but
        // resolve to empty at request time. If you add a provider there,
        // mirror its env var here.
        let env_var = match provider_id {
            "anthropic" => "ANTHROPIC_API_KEY",
            "openai" => "OPENAI_API_KEY",
            "google" => "GOOGLE_API_KEY",
            "groq" => "GROQ_API_KEY",
            "cerebras" => "CEREBRAS_API_KEY",
            "deepseek" => "DEEPSEEK_API_KEY",
            "mistral" => "MISTRAL_API_KEY",
            "xai" => "XAI_API_KEY",
            "openrouter" => "OPENROUTER_API_KEY",
            "togetherai" | "together-ai" => "TOGETHER_API_KEY",
            "perplexity" => "PERPLEXITY_API_KEY",
            "cohere" => "COHERE_API_KEY",
            "deepinfra" => "DEEPINFRA_API_KEY",
            "venice" => "VENICE_API_KEY",
            "github-copilot" => "GITHUB_TOKEN",
            "azure" => "AZURE_API_KEY",
            "huggingface" => "HF_TOKEN",
            "nvidia" => "NVIDIA_API_KEY",
            "zai" => "ZAI_API_KEY",
            "opencode-zen" | "opencode-go" => "OPENCODE_API_KEY",
            "crof" => "CROF_API_KEY",
            "sambanova" => "SAMBANOVA_API_KEY",
            // qwen adapter reads DASHSCOPE_API_KEY (Alibaba's DashScope is the
            // backing service), not QWEN_API_KEY.
            "qwen" | "alibaba" => "DASHSCOPE_API_KEY",
            "moonshot" | "moonshotai" => "MOONSHOT_API_KEY",
            "zhipu" | "zhipuai" => "ZHIPU_API_KEY",
            "siliconflow" => "SILICONFLOW_API_KEY",
            "nebius" => "NEBIUS_API_KEY",
            "novita" => "NOVITA_API_KEY",
            "ovhcloud" => "OVHCLOUD_API_KEY",
            "scaleway" => "SCALEWAY_API_KEY",
            "vultr" | "vultr-ai" => "VULTR_API_KEY",
            "baseten" => "BASETEN_API_KEY",
            // friendli adapter reads FRIENDLI_TOKEN (Friendli's docs use that
            // name), not FRIENDLI_API_KEY.
            "friendli" => "FRIENDLI_TOKEN",
            "upstage" => "UPSTAGE_API_KEY",
            "stepfun" => "STEPFUN_API_KEY",
            "fireworks" => "FIREWORKS_API_KEY",
            "minimax" => "MINIMAX_API_KEY",
            "synthetic" => "SYNTHETIC_API_KEY",
            "routing" => "ROUTING_API_KEY",
            "neuralwatt" => "NEURALWATT_API_KEY",
            "cline" => "CLINE_API_KEY",
            // cloudflare adapter reads CLOUDFLARE_API_TOKEN (plus
            // CLOUDFLARE_ACCOUNT_ID for the URL path; the token env var
            // carries the composite ACCOUNT_ID:API_TOKEN when no separate
            // account var is set).
            "cloudflare" => "CLOUDFLARE_API_TOKEN",
            "custom-openai" => "CUSTOM_OPENAI_API_KEY",
            "ollama" | "lm-studio" | "llama-cpp" => "", // No API key required
            _ => return None,
        };
        if !env_var.is_empty() {
            if let Some(key) = std::env::var(env_var).ok().filter(|k| !k.trim().is_empty()) {
                return Some(key.trim().to_string());
            }
        }
        if matches!(provider_id, "opencode-zen" | "opencode-go") {
            return Self::opencode_cli_api_key();
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{AuthStore, StoredCredential};

    /// Redirect `CLAWDE_HOME` to a temp dir for the lifetime of the guard so
    /// that `AuthStore` persistence can never touch the real
    /// `~/.clawde/auth.json`. Restores the original env var on drop — even
    /// during unwinding from a panic.
    ///
    /// Serialized against every other env-mutating test in this crate via
    /// `crate::paths::ENV_LOCK` (all platforms). Without this, the store-level
    /// tests below (`set_keys`, `add_key`, `remove_key`, `remove`, `set`) all
    /// call `save()`, which writes placeholder keys into the user's real config
    /// dir whenever `cargo test` runs.
    struct TestHome {
        _tmp: tempfile::TempDir,
        prev_clawde_home: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl TestHome {
        fn new() -> Self {
            let _lock = crate::paths::ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let prev = std::env::var_os("CLAWDE_HOME");
            let tmp = tempfile::tempdir().unwrap();
            std::env::set_var("CLAWDE_HOME", tmp.path());
            TestHome {
                _tmp: tmp,
                prev_clawde_home: prev,
                _lock,
            }
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            match &self.prev_clawde_home {
                Some(v) => std::env::set_var("CLAWDE_HOME", v),
                None => std::env::remove_var("CLAWDE_HOME"),
            }
        }
    }

    #[test]
    fn github_copilot_oauth_prefers_refresh_token() {
        let mut store = AuthStore::default();
        store.credentials.insert(
            "github-copilot".to_string(),
            StoredCredential::OAuthToken {
                access: "access-token".to_string(),
                refresh: "refresh-token".to_string(),
                expires: 0,
            },
        );

        assert_eq!(
            store.api_key_for("github-copilot").as_deref(),
            Some("refresh-token")
        );
    }

    #[test]
    fn opencode_cli_parser_accepts_current_flat_api_schema() {
        let raw = r#"{
            "opencode": {"type": "api", "key": "zen-key-12345678"},
            "openai": {"type": "api", "key": "openai-key-12345678"}
        }"#;
        assert_eq!(
            AuthStore::parse_opencode_cli_api_key(raw).as_deref(),
            Some("zen-key-12345678")
        );
    }

    #[test]
    fn opencode_cli_parser_rejects_generic_provider_shape_and_non_api_records() {
        let generic = r#"{"providers":{"opencode":{"apiKey":"zen-key-12345678"}}}"#;
        assert_eq!(AuthStore::parse_opencode_cli_api_key(generic), None);

        let oauth = r#"{"opencode":{"type":"oauth","access":"token-12345678"}}"#;
        assert_eq!(AuthStore::parse_opencode_cli_api_key(oauth), None);
    }

    #[test]
    fn api_key_for_regular_provider_uses_stored_key() {
        let mut store = AuthStore::default();
        store.credentials.insert(
            "openai".to_string(),
            StoredCredential::ApiKey {
                key: "openai-key".to_string(),
            },
        );

        assert_eq!(store.api_key_for("openai").as_deref(), Some("openai-key"));
    }

    // -----------------------------------------------------------------------
    // Multi-key tests
    // -----------------------------------------------------------------------

    #[test]
    fn default_keys_is_empty() {
        let store = AuthStore::default();
        assert!(store.keys.is_empty());
        assert!(store.keys_for("groq").is_none());
    }

    #[test]
    fn set_keys_stores_and_overwrites() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store.set_keys("firecrawl", vec!["k1".into(), "k2".into(), "k3".into()]);

        let keys = store.keys_for("firecrawl").expect("should have keys");
        assert_eq!(keys.len(), 3);
        assert_eq!(keys[0], "k1");
        assert_eq!(keys[1], "k2");
        assert_eq!(keys[2], "k3");

        // Overwrite
        store.set_keys("firecrawl", vec!["k4".into()]);
        let keys = store.keys_for("firecrawl").expect("should have keys");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0], "k4");
    }

    #[test]
    fn set_keys_strips_empty() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store.set_keys("firecrawl", vec!["k1".into(), "".into(), "k2".into()]);

        let keys = store.keys_for("firecrawl").expect("should have keys");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], "k1");
        assert_eq!(keys[1], "k2");
    }

    #[test]
    fn set_keys_all_empty_removes_entry() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store
            .keys
            .insert("firecrawl".to_string(), vec!["k1".into()]);
        store.set_keys("firecrawl", vec!["".into(), "".into()]);
        assert!(store.keys_for("firecrawl").is_none());
    }

    #[test]
    fn add_key_appends() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store.add_key("firecrawl", "k1".into());
        store.add_key("firecrawl", "k2".into());

        let keys = store.keys_for("firecrawl").expect("should have keys");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], "k1");
        assert_eq!(keys[1], "k2");
    }

    #[test]
    fn add_key_ignores_empty() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store.add_key("firecrawl", "".into());
        assert!(store.keys_for("firecrawl").is_none());
    }

    #[test]
    fn merge_keys_for_rotation_empty_prior() {
        let merged = AuthStore::merge_keys_for_rotation("existing", &[], "new");
        assert_eq!(merged, vec!["existing", "new"]);
    }

    #[test]
    fn merge_keys_for_rotation_dedupes_overlap_with_prior() {
        let prior = vec!["a".to_string(), "b".to_string()];
        let merged = AuthStore::merge_keys_for_rotation("a", &prior, "c");
        assert_eq!(merged, vec!["a", "b", "c"]);
    }

    #[test]
    fn merge_keys_for_rotation_dedupes_typed_key_against_existing() {
        let prior = vec!["p".to_string()];
        let merged = AuthStore::merge_keys_for_rotation("e", &prior, "e");
        assert_eq!(merged, vec!["e", "p"]);
    }

    #[test]
    fn merge_keys_for_rotation_dedupes_typed_key_against_prior() {
        let prior = vec!["p".to_string()];
        let merged = AuthStore::merge_keys_for_rotation("e", &prior, "p");
        assert_eq!(merged, vec!["e", "p"]);
    }

    #[test]
    fn merge_keys_for_rotation_preserves_prior_order() {
        let prior = vec![
            "k1".to_string(),
            "k2".to_string(),
            "k3".to_string(),
            "k1".to_string(), // duplicate mid-list
        ];
        let merged = AuthStore::merge_keys_for_rotation("anchor", &prior, "k2");
        assert_eq!(merged, vec!["anchor", "k1", "k2", "k3"]);
    }

    #[test]
    fn merge_keys_for_rotation_skips_empty_inputs() {
        let prior = vec!["p".to_string()];
        let merged = AuthStore::merge_keys_for_rotation("", &prior, "");
        assert_eq!(merged, vec!["p"]);
    }

    #[test]
    fn merge_then_set_keys_matches_round_trip() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store.credentials.insert(
            "firecrawl".into(),
            crate::auth_store::StoredCredential::ApiKey {
                key: "anchor".into(),
            },
        );
        store.set_keys("firecrawl", vec!["a".into(), "b".into()]);

        let prior = store.keys_for("firecrawl").unwrap_or(&[]).to_vec();
        let existing_key = match store.get("firecrawl").cloned() {
            Some(crate::auth_store::StoredCredential::ApiKey { key }) => key,
            _ => String::new(),
        };
        let merged = AuthStore::merge_keys_for_rotation(&existing_key, &prior, "typed");
        store.set_keys("firecrawl", merged);
        store.remove_credential("firecrawl");

        let keys = store.keys_for("firecrawl").expect("should have keys");
        assert_eq!(keys, &["anchor", "a", "b", "typed"]);
    }

    #[test]
    fn remove_key_removes_at_index() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store.set_keys("firecrawl", vec!["k1".into(), "k2".into(), "k3".into()]);

        assert!(store.remove_key("firecrawl", 1));
        let keys = store.keys_for("firecrawl").expect("should have keys");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], "k1");
        assert_eq!(keys[1], "k3");
    }

    #[test]
    fn remove_key_out_of_bounds_returns_false() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store.set_keys("firecrawl", vec!["k1".into()]);
        assert!(!store.remove_key("firecrawl", 5));
        assert!(store.keys_for("firecrawl").is_some());
    }

    #[test]
    fn remove_key_last_removes_entry() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store.set_keys("firecrawl", vec!["k1".into()]);
        assert!(store.remove_key("firecrawl", 0));
        assert!(store.keys_for("firecrawl").is_none());
    }

    #[test]
    fn api_key_for_free_provider_ignores_legacy_credential_and_uses_keys() {
        let mut store = AuthStore::default();
        store.credentials.insert(
            "groq".into(),
            StoredCredential::ApiKey {
                key: "legacy-groq-key".into(),
            },
        );
        store
            .keys
            .insert("groq".into(), vec!["canonical-groq-key".into()]);

        assert_eq!(
            store.api_key_for("groq").as_deref(),
            Some("canonical-groq-key"),
            "free API credentials must never bypass auth.json.keys"
        );
    }

    #[test]
    fn free_api_credentials_are_canonicalized_into_keys() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store.set(
            "groq",
            StoredCredential::ApiKey {
                key: "gsk-free-12345678".into(),
            },
        );
        assert!(!store.credentials.contains_key("groq"));
        assert_eq!(store.keys_for("groq").map(|keys| keys.len()), Some(1));

        // Repeated writes are idempotent and do not create duplicate slots.
        store.set_free_key("groq", "gsk-free-12345678".into());
        assert_eq!(store.keys_for("groq").unwrap().len(), 1);

        let reloaded = AuthStore::load();
        assert!(!reloaded.credentials.contains_key("groq"));
        assert_eq!(reloaded.keys_for("groq").map(|keys| keys.len()), Some(1));
    }

    #[test]
    fn legacy_free_credentials_migrate_without_touching_non_free_credentials() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store.credentials.insert(
            "nvidia".into(),
            StoredCredential::ApiKey {
                key: "nv-legacy-12345678".into(),
            },
        );
        store.credentials.insert(
            "openai".into(),
            StoredCredential::ApiKey {
                key: "sk-openai-12345678".into(),
            },
        );
        assert!(store.migrate_legacy_free_credentials());
        assert!(!store.credentials.contains_key("nvidia"));
        assert!(store.credentials.contains_key("openai"));
        assert_eq!(store.keys_for("nvidia").map(|keys| keys.len()), Some(1));
    }

    #[test]
    fn free_key_replacement_deduplicates_and_filters_placeholders() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        assert!(store.set_free_keys(
            "google",
            vec![
                "short".into(),
                "google-key-12345678".into(),
                "google-key-12345678 ".into()
            ]
        ));
        assert_eq!(
            store.keys_for("google").unwrap(),
            &["google-key-12345678".to_string()]
        );
    }

    #[test]
    fn invalid_legacy_free_credentials_are_removed_without_creating_slots() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store.credentials.insert(
            "groq".into(),
            StoredCredential::ApiKey {
                key: "short".into(),
            },
        );

        assert!(store.migrate_legacy_free_credentials());
        assert!(!store.credentials.contains_key("groq"));
        assert!(store.keys_for("groq").is_none());
    }

    #[test]
    fn invalid_free_write_normalizes_existing_pool() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store.keys.insert(
            "groq".into(),
            vec!["short".into(), "gsk-valid-12345678".into()],
        );

        assert!(store.set_free_key("groq", "bad".into()));
        assert_eq!(
            store.keys_for("groq").unwrap(),
            &["gsk-valid-12345678".to_string()]
        );
    }

    #[test]
    fn free_key_writes_preserve_github_copilot_oauth() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store.credentials.insert(
            "github-copilot".into(),
            StoredCredential::OAuthToken {
                access: "access-token".into(),
                refresh: "refresh-token".into(),
                expires: 0,
            },
        );

        assert!(store.set_free_keys("github-copilot", vec!["copilot-key-12345678".into()]));
        assert!(matches!(
            store.credentials.get("github-copilot"),
            Some(StoredCredential::OAuthToken { .. })
        ));
        assert_eq!(
            store.keys_for("github-copilot").map(|keys| keys.len()),
            Some(1)
        );

        assert!(!store.remove_credential("github-copilot"));
        assert!(matches!(
            store.credentials.get("github-copilot"),
            Some(StoredCredential::OAuthToken { .. })
        ));
    }

    #[test]
    fn remove_clears_free_keys_but_preserves_non_free_key_pools() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store.set_free_key("groq", "gsk-free-12345678".into());
        store.set_keys("firecrawl", vec!["fire-key-12345678".into()]);

        store.remove("groq");
        store.remove("firecrawl");

        assert!(store.keys_for("groq").is_none());
        assert!(
            store.keys_for("firecrawl").is_some(),
            "non-free remove must retain the independent multi-key pool"
        );
    }

    #[test]
    fn non_free_set_keys_keeps_legacy_multi_key_behavior() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store.set_keys("firecrawl", vec!["fire-key-12345678".into()]);
        assert_eq!(store.keys_for("firecrawl").map(|keys| keys.len()), Some(1));
    }

    #[test]
    fn api_key_for_falls_through_to_keys() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store.set_keys("groq", vec!["gsk-key1".into(), "gsk-key2".into()]);

        assert_eq!(store.api_key_for("groq").as_deref(), Some("gsk-key1"));
    }

    #[test]
    fn api_key_for_prefers_credentials_over_keys() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store.credentials.insert(
            "anthropic".to_string(),
            StoredCredential::ApiKey {
                key: "sk-credential".into(),
            },
        );
        store.set_keys("anthropic", vec!["sk-keys-first".into()]);

        // Credential wins over keys
        assert_eq!(
            store.api_key_for("anthropic").as_deref(),
            Some("sk-credential")
        );
    }

    #[test]
    fn serialization_round_trip_old_format() {
        // Old format with only credentials — keys should deserialize as empty.
        let old_json = r#"{"credentials":{"openai":{"type":"api","key":"sk-old"}}}"#;
        let store: AuthStore = serde_json::from_str(old_json).unwrap();
        assert_eq!(store.credentials.len(), 1);
        assert!(store.keys.is_empty());
    }

    #[test]
    fn serialization_round_trip_new_format() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store.credentials.insert(
            "anthropic".into(),
            StoredCredential::ApiKey {
                key: "sk-ant".into(),
            },
        );
        store.set_keys("firecrawl", vec!["gsk-1".into(), "gsk-2".into()]);

        let json = serde_json::to_string_pretty(&store).unwrap();
        let restored: AuthStore = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.credentials.len(), 1);
        assert_eq!(restored.keys_for("firecrawl").map(|k| k.len()), Some(2));
        assert_eq!(restored.keys_for("firecrawl").unwrap()[0], "gsk-1");
    }

    #[test]
    fn serialization_omits_keys_when_empty() {
        let store = AuthStore::default();
        let json = serde_json::to_string_pretty(&store).unwrap();
        // Old-format: no "keys" key.
        assert!(
            !json.contains("\"keys\""),
            "JSON should not contain keys field when empty: {}",
            json
        );
    }

    #[test]
    fn save_refuses_to_clobber_real_file_after_failed_load() {
        let _home = TestHome::new();
        // Seed a real store, then corrupt the file on disk.
        let mut store = AuthStore::default();
        store.set(
            "groq",
            StoredCredential::ApiKey {
                key: "gsk-real".into(),
            },
        );
        assert!(AuthStore::path().exists());

        // Corrupt the file (truncate) so load() falls back to an empty store.
        std::fs::write(AuthStore::path(), "{ not valid json ").unwrap();
        let mut failed = AuthStore::load();
        assert!(failed.credentials.is_empty() && failed.keys.is_empty());

        // save() must NOT overwrite the (possibly recoverable) real file.
        failed.save();
        let on_disk = std::fs::read_to_string(AuthStore::path()).unwrap();
        assert_eq!(on_disk, "{ not valid json ");

        // Once the user deliberately adds a key, saving proceeds.
        failed.set(
            "groq",
            StoredCredential::ApiKey {
                key: "gsk-real".into(),
            },
        );
        let on_disk = std::fs::read_to_string(AuthStore::path()).unwrap();
        assert!(on_disk.contains("gsk-real"));
    }

    #[test]
    fn keys_only_file_loads_cleanly() {
        let _home = TestHome::new();
        // A file with only a `keys` map (no `credentials`) must load as a
        // healthy store — previously the missing `credentials` field made the
        // whole file look corrupt and hid every stored key.
        std::fs::write(
            AuthStore::path(),
            r#"{"keys":{"groq":["gsk-abc-12345678"]}}"#,
        )
        .unwrap();
        let store = AuthStore::load();
        assert_eq!(store.keys_for("groq").map(|k| k.len()), Some(1));
        assert!(store.credentials.is_empty());
        assert!(
            store.load_error.is_none(),
            "keys-only file must not be treated as corrupt"
        );
    }

    #[test]
    fn partially_corrupt_store_salvages_valid_entries() {
        let _home = TestHome::new();
        std::fs::write(
            AuthStore::path(),
            r#"{"credentials":{"openai":{"type":"api","key":"sk-ok-12345678"},"broken":{"type":"api"}},"keys":{"groq":["gsk-good-12345678"],"bad":"not-a-list"}}"#,
        )
        .unwrap();
        let store = AuthStore::load();
        // Valid entries survive the salvage...
        assert!(matches!(
            store.get("openai"),
            Some(StoredCredential::ApiKey { key }) if key == "sk-ok-12345678"
        ));
        assert_eq!(store.keys_for("groq").map(|k| k.len()), Some(1));
        // ...the broken ones are dropped with a recorded reason.
        assert!(store.get("broken").is_none());
        assert!(store.keys_for("bad").is_none());
        let err = store.load_error.as_deref().unwrap_or_default();
        assert!(err.contains("credentials[broken]"), "err: {err}");
        assert!(err.contains("keys[bad]"), "err: {err}");
    }

    #[test]
    fn missing_store_can_be_created_by_default_save() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store.set_keys("groq", vec!["gsk-new-12345678".into()]);
        assert_eq!(
            AuthStore::load().keys_for("groq").map(|keys| keys.len()),
            Some(1)
        );
    }

    #[test]
    fn save_fails_closed_when_another_writer_holds_lock() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store.set(
            "groq",
            StoredCredential::ApiKey {
                key: "gsk-existing-12345678".into(),
            },
        );
        let lock_path = AuthStore::path().with_file_name("auth.json.lock");
        std::fs::write(&lock_path, "owner").unwrap();

        store.set_keys("nvidia", vec!["nv-blocked-12345678".into()]);
        let on_disk = AuthStore::load();
        assert!(on_disk.keys_for("nvidia").is_none());
        assert_eq!(
            on_disk
                .keys_for("groq")
                .map(|keys| keys.first().map(String::as_str)),
            Some(Some("gsk-existing-12345678"))
        );
        let _ = std::fs::remove_file(lock_path);
    }

    #[test]
    fn stale_store_cannot_clobber_newer_keys() {
        let _home = TestHome::new();
        let mut initial = AuthStore::default();
        initial.set(
            "groq",
            StoredCredential::ApiKey {
                key: "gsk-initial-12345678".into(),
            },
        );

        let mut stale = AuthStore::load();
        let mut fresh = AuthStore::load();
        fresh.set_keys("nvidia", vec!["nv-initial-12345678".into()]);

        // The stale writer must not erase the newer provider/key pool.
        stale.set(
            "groq",
            StoredCredential::ApiKey {
                key: "gsk-stale-12345678".into(),
            },
        );
        let on_disk = AuthStore::load();
        assert_eq!(on_disk.keys_for("nvidia").map(|keys| keys.len()), Some(1));
        assert_eq!(
            on_disk
                .keys_for("groq")
                .map(|keys| keys.first().map(String::as_str)),
            Some(Some("gsk-initial-12345678"))
        );
    }

    #[test]
    fn save_backs_up_corrupt_file_before_overwrite() {
        let _home = TestHome::new();
        let corrupt = r#"{"credentials":{"groq":{"type":"api","key":"gsk-still-recoverable"}}"#;
        std::fs::write(AuthStore::path(), corrupt).unwrap();
        let mut store = AuthStore::load();
        assert!(store.load_error.is_some());

        // The user deliberately adds a key — save proceeds, but the original
        // (possibly recoverable) content is preserved as a backup first.
        store.set(
            "openai",
            StoredCredential::ApiKey {
                key: "sk-new-12345678".into(),
            },
        );
        let on_disk = std::fs::read_to_string(AuthStore::path()).unwrap();
        assert!(on_disk.contains("sk-new-12345678"));

        let backups: Vec<_> = std::fs::read_dir(AuthStore::path().parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("auth.json.corrupt-")
            })
            .collect();
        assert_eq!(backups.len(), 1, "expected exactly one corrupt backup");
        assert_eq!(std::fs::read_to_string(backups[0].path()).unwrap(), corrupt);
    }

    #[test]
    fn api_key_for_skips_blank_and_whitespace_slots() {
        let _home = TestHome::new();
        // Blank credential + blank ring slots resolve to nothing — never a
        // phantom key. Unknown providers have no env fallback, so this is
        // env-independent in tests.
        let mut store = AuthStore::default();
        store.credentials.insert(
            "mystery-a".into(),
            StoredCredential::ApiKey { key: "   ".into() },
        );
        store.set_keys("mystery-a", vec!["".into(), " \t ".into()]);
        assert_eq!(store.api_key_for("mystery-a"), None);

        // A whitespace-padded real key is trimmed.
        let mut store2 = AuthStore::default();
        store2.set_keys("mystery-b", vec!["secret-key-12345678 ".into()]);
        assert_eq!(
            store2.api_key_for("mystery-b").as_deref(),
            Some("secret-key-12345678")
        );
    }
}
