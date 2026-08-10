// auth_store.rs — JSON-based credential store at ~/.clawde/auth.json.
//
// Stores API keys and OAuth tokens for providers so users don't have to rely
// solely on environment variables.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Monotonic counter for unique tmp filenames. Two saves racing in the same
/// process must never share a `.auth.json.clawde-tmp-*` path or one rename
/// would steal the other's file (ENOENT race).
static AUTH_TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
}

/// Result of [`AuthStore::salvage_auth_store`]: the recoverable maps plus a
/// human-readable reason for every dropped entry.
struct SalvageResult {
    credentials: HashMap<String, StoredCredential>,
    keys: HashMap<String, Vec<String>>,
    dropped: Vec<String>,
}

impl AuthStore {
    /// Path to the auth store file.
    pub fn path() -> PathBuf {
        crate::config::Settings::config_dir().join("auth.json")
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
        match serde_json::from_str::<AuthStore>(&raw) {
            Ok(store) => store,
            Err(e) => {
                let salvaged = Self::salvage_auth_store(&raw);
                let mut store = Self {
                    from_fallback: true,
                    file_corrupt: true,
                    load_error: None,
                    credentials: salvaged.credentials,
                    keys: salvaged.keys,
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
            }
        }
    }

    /// Store a credential for the given provider (persists immediately).
    pub fn set(&mut self, provider_id: &str, cred: StoredCredential) {
        self.credentials.insert(provider_id.to_string(), cred);
        self.save();
    }

    /// Get the stored credential for a provider.
    pub fn get(&self, provider_id: &str) -> Option<&StoredCredential> {
        self.credentials.get(provider_id)
    }

    /// Remove the credential for a provider (persists immediately).
    pub fn remove(&mut self, provider_id: &str) {
        self.credentials.remove(provider_id);
        self.save();
    }

    // -----------------------------------------------------------------------
    // Multi-key helpers
    // -----------------------------------------------------------------------

    /// Replace all keys for a provider (persists immediately).
    ///
    /// Empty keys in the input are stripped. If the resulting list is empty the
    /// provider's key entry is removed entirely.
    pub fn set_keys(&mut self, provider_id: &str, keys: Vec<String>) {
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
                StoredCredential::ApiKey { key } => {
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
        if env_var.is_empty() {
            None
        } else {
            std::env::var(env_var).ok().filter(|k| !k.is_empty())
        }
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
    fn api_key_for_regular_provider_uses_stored_key() {
        let mut store = AuthStore::default();
        store.credentials.insert(
            "openrouter".to_string(),
            StoredCredential::ApiKey {
                key: "or-key".to_string(),
            },
        );

        assert_eq!(store.api_key_for("openrouter").as_deref(), Some("or-key"));
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
        store.set_keys("groq", vec!["k1".into(), "k2".into(), "k3".into()]);

        let keys = store.keys_for("groq").expect("should have keys");
        assert_eq!(keys.len(), 3);
        assert_eq!(keys[0], "k1");
        assert_eq!(keys[1], "k2");
        assert_eq!(keys[2], "k3");

        // Overwrite
        store.set_keys("groq", vec!["k4".into()]);
        let keys = store.keys_for("groq").expect("should have keys");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0], "k4");
    }

    #[test]
    fn set_keys_strips_empty() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store.set_keys("groq", vec!["k1".into(), "".into(), "k2".into()]);

        let keys = store.keys_for("groq").expect("should have keys");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], "k1");
        assert_eq!(keys[1], "k2");
    }

    #[test]
    fn set_keys_all_empty_removes_entry() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store.keys.insert("groq".to_string(), vec!["k1".into()]);
        store.set_keys("groq", vec!["".into(), "".into()]);
        assert!(store.keys_for("groq").is_none());
    }

    #[test]
    fn add_key_appends() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store.add_key("groq", "k1".into());
        store.add_key("groq", "k2".into());

        let keys = store.keys_for("groq").expect("should have keys");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], "k1");
        assert_eq!(keys[1], "k2");
    }

    #[test]
    fn add_key_ignores_empty() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store.add_key("groq", "".into());
        assert!(store.keys_for("groq").is_none());
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
            "groq".into(),
            crate::auth_store::StoredCredential::ApiKey {
                key: "anchor".into(),
            },
        );
        store.set_keys("groq", vec!["a".into(), "b".into()]);

        let prior = store.keys_for("groq").unwrap_or(&[]).to_vec();
        let existing_key = match store.get("groq").cloned() {
            Some(crate::auth_store::StoredCredential::ApiKey { key }) => key,
            _ => String::new(),
        };
        let merged = AuthStore::merge_keys_for_rotation(&existing_key, &prior, "typed");
        store.set_keys("groq", merged);
        store.remove("groq");

        let keys = store.keys_for("groq").expect("should have keys");
        assert_eq!(keys, &["anchor", "a", "b", "typed"]);
    }

    #[test]
    fn remove_key_removes_at_index() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store.set_keys("groq", vec!["k1".into(), "k2".into(), "k3".into()]);

        assert!(store.remove_key("groq", 1));
        let keys = store.keys_for("groq").expect("should have keys");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], "k1");
        assert_eq!(keys[1], "k3");
    }

    #[test]
    fn remove_key_out_of_bounds_returns_false() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store.set_keys("groq", vec!["k1".into()]);
        assert!(!store.remove_key("groq", 5));
        assert!(store.keys_for("groq").is_some());
    }

    #[test]
    fn remove_key_last_removes_entry() {
        let _home = TestHome::new();
        let mut store = AuthStore::default();
        store.set_keys("groq", vec!["k1".into()]);
        assert!(store.remove_key("groq", 0));
        assert!(store.keys_for("groq").is_none());
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
        store.set_keys("groq", vec!["gsk-1".into(), "gsk-2".into()]);

        let json = serde_json::to_string_pretty(&store).unwrap();
        let restored: AuthStore = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.credentials.len(), 1);
        assert_eq!(restored.keys_for("groq").map(|k| k.len()), Some(2));
        assert_eq!(restored.keys_for("groq").unwrap()[0], "gsk-1");
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
