// key_ring.rs — In-memory key rotation state machine with cooldown tracking.
//
// Manages a set of API keys for a single provider. Tracks which keys are
// currently usable and which are in cooldown after being exhausted (quota
// exceeded, rate limited, auth failure). Cooldowns expire after a configured
// duration, at which point the key becomes usable again.
//
// Thread-safe for use behind `Arc<Mutex<KeyRing>>` — all mutation is through
// `&mut self` methods. The struct itself does not synchronise internally;
// callers are responsible for external synchronisation.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Instant;

// ---------------------------------------------------------------------------
// KeyRing
// ---------------------------------------------------------------------------

/// One entry in a [`KeyRing`].
#[derive(Debug, Clone)]
struct KeyRingEntry {
    /// The API key string.
    key: String,
    /// `None` = usable now. `Some(instant)` = exhausted until this time.
    cooldown_until: Option<Instant>,
    /// Human-readable description of the last exhaustion reason, if any.
    last_error: Option<String>,
}

/// Snapshot of a single key's status, safe to pass across thread boundaries
/// and display in the TUI.
#[derive(Debug, Clone)]
pub struct KeyStatus {
    /// Index of this key in the ring (0-based).
    pub index: usize,
    /// The full key string.
    pub key: String,
    /// A preview of the key for display (last 4 chars).
    pub key_preview: String,
    /// Whether this key is currently active (not in cooldown).
    pub active: bool,
    /// When this key's cooldown expires, if exhausted.
    pub exhausted_until: Option<Instant>,
    /// Seconds remaining until this key becomes usable again, if in cooldown.
    pub cooldown_remaining_secs: Option<u64>,
    /// The error message from the last exhaustion, if any.
    pub last_error: Option<String>,
}

/// In-memory key rotation state machine for a single provider.
///
/// Manages a list of API keys, tracking which are in cooldown after being
/// exhausted. Cooldowns are tracked via [`Instant`] and expire automatically
/// when time passes — call [`prune_expired`](Self::prune_expired) before
/// querying available keys.
#[derive(Debug, Clone)]
pub struct KeyRing {
    /// Provider identifier (e.g. "groq", "openai").
    provider_id: String,
    /// Ordered list of key entries. Indices are stable — removing a key
    /// shifts later keys down.
    entries: Vec<KeyRingEntry>,
    /// Index of the last key used. Round-robins on each call to
    /// [`next_available`](Self::next_available).
    cursor: usize,
}

impl KeyRing {
    /// Create a new key ring for the given provider with the given keys.
    ///
    /// All keys start in the active (usable) state.
    pub fn new(provider_id: impl Into<String>, keys: Vec<String>) -> Self {
        Self {
            provider_id: provider_id.into(),
            entries: keys
                .into_iter()
                .map(|key| KeyRingEntry {
                    key,
                    cooldown_until: None,
                    last_error: None,
                })
                .collect(),
            cursor: 0,
        }
    }

    /// The provider identifier this key ring belongs to.
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    /// Total number of keys in the ring (active + exhausted).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the ring has any keys at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The key string at `index`, or `None` if out of bounds.
    #[allow(dead_code)]
    pub fn key_at(&self, index: usize) -> Option<&str> {
        self.entries.get(index).map(|e| e.key.as_str())
    }

    // -----------------------------------------------------------------------
    // Core rotation
    // -----------------------------------------------------------------------

    /// Remove any keys whose cooldown has expired, returning them to the
    /// active pool. Call this before [`next_available`](Self::next_available)
    /// if time may have passed since the last check.
    pub fn prune_expired(&mut self) {
        let now = Instant::now();
        for entry in &mut self.entries {
            if let Some(cooldown_until) = entry.cooldown_until {
                if now >= cooldown_until {
                    entry.cooldown_until = None;
                    entry.last_error = None;
                }
            }
        }
    }

    /// Get the next available (non-exhausted) key, round-robining through
    /// the ring. Calls [`prune_expired`](Self::prune_expired) first.
    ///
    /// Returns `Some((index, key_str))` if any key is available, or `None`
    /// if all keys are in cooldown.
    pub fn next_available(&mut self) -> Option<(usize, &str)> {
        self.prune_expired();
        if self.entries.is_empty() {
            return None;
        }

        let n = self.entries.len();
        for offset in 0..n {
            let idx = (self.cursor + offset) % n;
            if self.entries[idx].cooldown_until.is_none() {
                self.cursor = (idx + 1) % n; // advance for next call
                return Some((idx, self.entries[idx].key.as_str()));
            }
        }
        None
    }

    /// Mark the key at `index` as exhausted for `cooldown_secs` seconds.
    ///
    /// After `cooldown_secs` have elapsed, the key will be returned to the
    /// active pool by the next call to [`prune_expired`](Self::prune_expired)
    /// or [`next_available`](Self::next_available).
    ///
    /// A minimum cooldown of 1 second is always enforced to prevent tight
    /// retry loops when callers supply 0 (which would make `prune_expired`
    /// immediately re-activate the key on the next `next_available` call).
    ///
    /// Returns `false` if `index` is out of bounds.
    pub fn mark_exhausted(
        &mut self,
        index: usize,
        cooldown_secs: u64,
        error_message: Option<String>,
    ) -> bool {
        if let Some(entry) = self.entries.get_mut(index) {
            // Floor cooldown at 1s to prevent prune_expired from immediately
            // re-activating the key (which would create an infinite retry loop).
            let clamped = cooldown_secs.max(1);
            entry.cooldown_until = Some(Instant::now() + std::time::Duration::from_secs(clamped));
            entry.last_error = error_message;
            true
        } else {
            false
        }
    }

    // -----------------------------------------------------------------------
    // Status queries
    // -----------------------------------------------------------------------

    /// Clear an exhausted key's cooldown (e.g. the health poller has
    /// confirmed the key is working again). Returns `true` if the index was
    /// valid.
    pub fn mark_healthy(&mut self, index: usize) -> bool {
        if let Some(entry) = self.entries.get_mut(index) {
            entry.cooldown_until = None;
            entry.last_error = None;
            true
        } else {
            false
        }
    }

    /// Returns `true` when every key in the ring is in cooldown (none usable).
    pub fn all_exhausted(&self) -> bool {
        self.entries.iter().all(|e| e.cooldown_until.is_some())
    }

    /// The earliest [`Instant`] at which *any* exhausted key becomes usable
    /// again. Returns `None` if no keys are exhausted.
    pub fn earliest_retry(&self) -> Option<Instant> {
        self.entries.iter().filter_map(|e| e.cooldown_until).min()
    }

    /// Seconds until the earliest exhausted key becomes usable again.
    /// Returns `None` if no keys are exhausted.
    pub fn earliest_retry_secs(&self) -> Option<u64> {
        let now = Instant::now();
        self.earliest_retry().map(|t| {
            let d = t.saturating_duration_since(now);
            d.as_secs().max(1)
        })
    }

    /// Number of keys that are currently active (not in cooldown).
    pub fn active_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.cooldown_until.is_none())
            .count()
    }

    /// Number of keys currently in cooldown.
    pub fn exhausted_count(&self) -> usize {
        self.entries.len() - self.active_count()
    }

    /// Produce a snapshot of every key's current status, suitable for TUI
    /// display or debugging.
    pub fn statuses(&self) -> Vec<KeyStatus> {
        let now = Instant::now();
        self.entries
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                let active = entry.cooldown_until.is_none();
                let cooldown_remaining = entry
                    .cooldown_until
                    .map(|t| t.saturating_duration_since(now).as_secs());

                // Build a preview showing first 3 + last 4 chars for keys > 7.
                // For keys 5-7 chars, show the full key (elision looks weird
                // at that length).
                let preview = if entry.key.len() > 7 {
                    format!(
                        "{}...{}",
                        &entry.key[..3],
                        &entry.key[entry.key.len() - 4..]
                    )
                } else {
                    entry.key.clone()
                };

                KeyStatus {
                    index: idx,
                    key: entry.key.clone(),
                    key_preview: preview,
                    active,
                    exhausted_until: entry.cooldown_until,
                    cooldown_remaining_secs: cooldown_remaining,
                    last_error: entry.last_error.clone(),
                }
            })
            .collect()
    }
}

// -----------------------------------------------------------------------
// Persistence — serializable snapshots
// -----------------------------------------------------------------------

/// Serializable snapshot of a single key ring entry for disk persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyRingEntrySnapshot {
    /// Full key string for identity matching across restarts.
    pub key: String,
    /// Remaining cooldown seconds at snapshot time. 0 means active.
    #[serde(default)]
    pub cooldown_remaining_secs: u64,
    /// Last error message, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Serializable snapshot of a single provider's key ring cooldown state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderKeyRingSnapshot {
    pub entries: Vec<KeyRingEntrySnapshot>,
    /// Unix timestamp (seconds since epoch) when this snapshot was saved.
    /// Used to adjust cooldowns on load: if 300s remained at save time and
    /// 120s have elapsed, only 180s remain. Old snapshots without this field
    /// are treated as if they were saved just now (no adjustment).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_at_unix: Option<u64>,
}

impl KeyRing {
    /// Produce a snapshot of current cooldown state for persistence.
    /// Cooldowns are stored as *remaining seconds* so the snapshot is
    /// portable across process restarts.
    pub fn to_snapshot(&self) -> ProviderKeyRingSnapshot {
        let now = Instant::now();
        let saved_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .ok();
        ProviderKeyRingSnapshot {
            entries: self
                .entries
                .iter()
                .map(|e| {
                    let remaining = e
                        .cooldown_until
                        .map(|t| t.saturating_duration_since(now).as_secs())
                        .unwrap_or(0);
                    KeyRingEntrySnapshot {
                        key: e.key.clone(),
                        cooldown_remaining_secs: remaining,
                        last_error: e.last_error.clone(),
                    }
                })
                .collect(),
            saved_at_unix: saved_at,
        }
    }

    /// Apply a previously-saved snapshot, restoring cooldowns for keys that
    /// match by key string. Keys in the snapshot that don't exist in this
    /// ring are silently ignored. Keys in this ring that don't appear in the
    /// snapshot stay active — this naturally handles the case where new keys
    /// were added while the app was closed.
    ///
    /// When `saved_at_unix` is present, elapsed time since the snapshot was
    /// written is subtracted from each cooldown so that a key saved with 300s
    /// remaining and loaded 120s later only has 180s remaining. Snapshots
    /// without this field (old format) are treated as if saved just now.
    pub fn apply_snapshot(&mut self, snapshot: &ProviderKeyRingSnapshot) {
        let now = Instant::now();
        // Compute wall-clock elapsed seconds since the snapshot was saved.
        // This adjusts cooldowns that span process restarts.
        let elapsed = snapshot.saved_at_unix.and_then(|saved_at| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|now_unix| now_unix.as_secs().saturating_sub(saved_at))
        });
        for entry in &mut self.entries {
            if let Some(saved) = snapshot.entries.iter().find(|s| s.key == entry.key) {
                let adjusted = match elapsed {
                    Some(elapsed_secs) => {
                        saved.cooldown_remaining_secs.saturating_sub(elapsed_secs)
                    }
                    None => saved.cooldown_remaining_secs, // old snapshot, no adjustment
                };
                if adjusted > 0 {
                    entry.cooldown_until = Some(now + std::time::Duration::from_secs(adjusted));
                    entry.last_error = saved.last_error.clone();
                }
                // If adjusted ≤ 0, the cooldown already expired while the app
                // was closed — leave the key active (clear last_error too).
            }
        }
    }

    /// Persist current cooldown state to a JSON file at `path`.
    ///
    /// Uses atomic write (temp file + rename) so a crash mid-write
    /// can never corrupt the saved state.
    pub fn save_to_file(&self, path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let snapshot = self.to_snapshot();
        let json = match serde_json::to_string_pretty(&snapshot) {
            Ok(j) => j,
            Err(_) => return,
        };
        let tmp = path.with_file_name(format!(
            ".{}.tmp-{}",
            path.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id(),
        ));
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }

    /// Load previously-saved cooldown state from a JSON file at `path`.
    /// Silently ignores missing files and corrupt files (same strategy as
    /// [`crate::auth_store::AuthStore::load`]).
    pub fn load_from_file(&mut self, path: &Path) {
        if !path.exists() {
            return;
        }
        let json = match std::fs::read_to_string(path) {
            Ok(j) => j,
            Err(_) => return,
        };
        let snapshot: ProviderKeyRingSnapshot = match serde_json::from_str(&json) {
            Ok(s) => s,
            Err(_) => return,
        };
        self.apply_snapshot(&snapshot);
    }

    /// Default path for this provider's persisted key ring state:
    /// `{clawde_home}/key-ring-state/{provider_id}.json`
    pub fn default_state_path(provider_id: &str) -> std::path::PathBuf {
        crate::config::Settings::config_dir()
            .join("key-ring-state")
            .join(format!("{provider_id}.json"))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ring(keys: &[&str]) -> KeyRing {
        KeyRing::new("groq", keys.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn empty_ring_has_no_available_keys() {
        let mut ring = make_ring(&[]);
        assert!(ring.next_available().is_none());
        assert!(ring.is_empty());
        assert_eq!(ring.len(), 0);
    }

    #[test]
    fn single_key_is_always_available() {
        let mut ring = make_ring(&["gsk-key1"]);
        let (idx, key) = ring.next_available().expect("should have key");
        assert_eq!(idx, 0);
        assert_eq!(key, "gsk-key1");
    }

    #[test]
    fn round_robins_through_keys() {
        let mut ring = make_ring(&["k1", "k2", "k3"]);

        let (idx1, _) = ring.next_available().unwrap();
        assert_eq!(idx1, 0);

        let (idx2, _) = ring.next_available().unwrap();
        assert_eq!(idx2, 1);

        let (idx3, _) = ring.next_available().unwrap();
        assert_eq!(idx3, 2);

        // Wraps around
        let (idx4, _) = ring.next_available().unwrap();
        assert_eq!(idx4, 0);
    }

    #[test]
    fn exhausted_key_is_skipped() {
        let mut ring = make_ring(&["k1", "k2", "k3"]);
        // Exhaust key 1 for 60 seconds
        assert!(ring.mark_exhausted(1, 60, Some("rate limited".into())));

        let (idx, key) = ring.next_available().expect("should have a key");
        assert_eq!(idx, 0);
        assert_eq!(key, "k1");
    }

    #[test]
    fn all_keys_exhausted_returns_none() {
        let mut ring = make_ring(&["k1", "k2"]);
        ring.mark_exhausted(0, 60, None);
        ring.mark_exhausted(1, 60, None);

        assert!(ring.all_exhausted());
        assert!(ring.next_available().is_none());
    }

    #[test]
    fn mark_exhausted_out_of_bounds_returns_false() {
        let mut ring = make_ring(&["k1"]);
        assert!(!ring.mark_exhausted(5, 60, None));
    }

    #[test]
    fn cooldown_min_1_prevents_immediate_re_activation() {
        let mut ring = make_ring(&["k1"]);
        // Cooldown of 0 is clamped to 1 second, so the key stays exhausted
        // after a prune.
        ring.mark_exhausted(0, 0, None);
        ring.prune_expired();

        assert!(ring.all_exhausted(), "0s cooldown is clamped to 1s minimum");
        assert!(ring.next_available().is_none());
    }

    #[test]
    fn cooldown_1_allows_expiry_after_second() {
        let mut ring = make_ring(&["k1"]);
        ring.mark_exhausted(0, 1, None);
        // Can't actually wait 1 second in a unit test, so verify the key
        // is exhausted immediately after marking.
        assert!(ring.all_exhausted());
        assert_eq!(ring.active_count(), 0);
    }

    #[test]
    fn earliest_retry_returns_min_cooldown() {
        let mut ring = make_ring(&["k1", "k2", "k3"]);
        ring.mark_exhausted(0, 120, None); // 2 min
        ring.mark_exhausted(1, 60, None); // 1 min — earliest

        let earliest = ring.earliest_retry();
        assert!(earliest.is_some());

        let secs = ring.earliest_retry_secs();
        assert!(secs.is_some());
        // Should be close to 60s
        assert!(
            secs.unwrap() >= 1 && secs.unwrap() <= 65,
            "expected ~60s, got {}s",
            secs.unwrap()
        );
    }

    #[test]
    fn earliest_retry_none_when_no_keys_exhausted() {
        let ring = make_ring(&["k1", "k2"]);
        assert!(ring.earliest_retry().is_none());
        assert!(ring.earliest_retry_secs().is_none());
    }

    #[test]
    fn active_and_exhausted_counts() {
        let mut ring = make_ring(&["k1", "k2", "k3", "k4"]);
        assert_eq!(ring.active_count(), 4);
        assert_eq!(ring.exhausted_count(), 0);

        ring.mark_exhausted(1, 60, None);
        assert_eq!(ring.active_count(), 3);
        assert_eq!(ring.exhausted_count(), 1);

        ring.mark_exhausted(2, 60, None);
        assert_eq!(ring.active_count(), 2);
        assert_eq!(ring.exhausted_count(), 2);
    }

    #[test]
    fn statuses_shows_correct_state() {
        let mut ring = make_ring(&["abc123key", "def456key"]);
        ring.mark_exhausted(1, 60, Some("quota exceeded".into()));

        let statuses = ring.statuses();
        assert_eq!(statuses.len(), 2);

        // Key 0: active
        assert_eq!(statuses[0].index, 0);
        assert!(statuses[0].active);
        assert!(statuses[0].cooldown_remaining_secs.is_none());
        assert!(statuses[0].last_error.is_none());
        assert_eq!(statuses[0].key_preview, "abc...3key"); // first 3 + last 4

        // Key 1: exhausted
        assert_eq!(statuses[1].index, 1);
        assert!(!statuses[1].active);
        assert!(statuses[1].cooldown_remaining_secs.is_some());
        assert_eq!(statuses[1].last_error.as_deref(), Some("quota exceeded"));
        assert_eq!(statuses[1].key_preview, "def...6key");
    }

    #[test]
    fn short_key_preview_shows_full_key() {
        let ring = make_ring(&["ab"]);
        let statuses = ring.statuses();
        assert_eq!(statuses[0].key_preview, "ab");
    }

    #[test]
    fn medium_key_preview_shows_full_key() {
        // Keys 5-7 chars: show full key (elision looks weird)
        let ring = make_ring(&["abcdefg"]); // 7 chars
        let statuses = ring.statuses();
        assert_eq!(statuses[0].key_preview, "abcdefg");
    }

    #[test]
    fn round_robin_skips_exhausted_then_resumes_normal() {
        let mut ring = make_ring(&["k1", "k2", "k3"]);
        ring.mark_exhausted(1, 60, None);

        // Should get k1 (index 0), then k3 (index 2), wrapping to k1 again
        let (i1, _) = ring.next_available().unwrap();
        assert_eq!(i1, 0);

        let (i2, _) = ring.next_available().unwrap();
        assert_eq!(i2, 2); // skips exhausted k2

        let (i3, _) = ring.next_available().unwrap();
        assert_eq!(i3, 0); // wraps around to k1
    }

    #[test]
    fn partial_exhaustion_allows_some_keys() {
        let mut ring = make_ring(&["k1", "k2", "k3", "k4"]);
        ring.mark_exhausted(0, 60, None);
        ring.mark_exhausted(2, 60, None);

        assert!(!ring.all_exhausted());
        assert_eq!(ring.active_count(), 2);

        // Should only get k1 alternative: k2 (idx 1) then k4 (idx 3)
        // Note: cursor starts at 0, but 0 is exhausted, so we skip to 1
        let (i1, _) = ring.next_available().unwrap();
        assert_eq!(i1, 1); // k2

        let (i2, _) = ring.next_available().unwrap();
        assert_eq!(i2, 3); // k4

        let (i3, _) = ring.next_available().unwrap();
        assert_eq!(i3, 1); // wraps to k2
    }

    #[test]
    fn provider_id_is_stored() {
        let ring = KeyRing::new("openai", vec!["sk-123".into()]);
        assert_eq!(ring.provider_id(), "openai");
    }

    #[test]
    fn debug_format_does_not_panic() {
        let ring = make_ring(&["k1", "k2"]);
        let _ = format!("{:?}", ring);
    }

    // -------------------------------------------------------------------
    // Persistence tests
    // -------------------------------------------------------------------

    #[test]
    fn snapshot_and_apply_round_trip() {
        let mut ring = make_ring(&["k1", "k2"]);
        ring.mark_exhausted(1, 120, Some("rate limited".into()));

        let snapshot = ring.to_snapshot();
        assert_eq!(snapshot.entries.len(), 2);
        // Key 0: active → remaining 0
        assert_eq!(snapshot.entries[0].key, "k1");
        assert_eq!(snapshot.entries[0].cooldown_remaining_secs, 0);
        assert!(snapshot.entries[0].last_error.is_none());
        // Key 1: exhausted → remaining ~120
        assert_eq!(snapshot.entries[1].key, "k2");
        assert!(snapshot.entries[1].cooldown_remaining_secs > 0);
        assert_eq!(
            snapshot.entries[1].last_error.as_deref(),
            Some("rate limited")
        );

        // Create a fresh ring and apply the snapshot
        let mut fresh = make_ring(&["k1", "k2"]);
        fresh.apply_snapshot(&snapshot);

        // Key 0 should still be active
        assert!(fresh.next_available().is_some());
        let (idx, key) = fresh.next_available().unwrap();
        assert_eq!(idx, 0);
        assert_eq!(key, "k1");
        // Key 1 should be exhausted
        assert_eq!(fresh.active_count(), 1);
        assert_eq!(fresh.exhausted_count(), 1);
    }

    #[test]
    fn snapshot_unknown_keys_ignored() {
        // Keys in the snapshot that don't exist in the ring should be
        // silently ignored (e.g. a key was removed while the app was closed).
        let mut ring = make_ring(&["k1"]);

        let snapshot = ProviderKeyRingSnapshot {
            entries: vec![
                KeyRingEntrySnapshot {
                    key: "k1".into(),
                    cooldown_remaining_secs: 0,
                    last_error: None,
                },
                KeyRingEntrySnapshot {
                    key: "k-ghost".into(),
                    cooldown_remaining_secs: 60,
                    last_error: Some("deleted key".into()),
                },
            ],
            saved_at_unix: None,
        };

        ring.apply_snapshot(&snapshot);
        // k1 should still be active (cooldown_remaining_secs: 0)
        assert!(ring.next_available().is_some());
        assert_eq!(ring.active_count(), 1);
    }

    #[test]
    fn snapshot_new_keys_stay_active() {
        // Keys in the ring that don't appear in the snapshot should stay
        // active (e.g. new keys were added while the app was closed).
        let mut ring = make_ring(&["k1", "k2"]);

        let snapshot = ProviderKeyRingSnapshot {
            entries: vec![KeyRingEntrySnapshot {
                key: "k1".into(),
                cooldown_remaining_secs: 60,
                last_error: Some("rate limited".into()),
            }],
            saved_at_unix: None,
        };

        ring.apply_snapshot(&snapshot);
        // k1 should be exhausted, k2 should be active
        let (idx, key) = ring.next_available().expect("k2 should be active");
        assert_eq!(idx, 1);
        assert_eq!(key, "k2");
        assert_eq!(ring.active_count(), 1);
        assert_eq!(ring.exhausted_count(), 1);
    }

    #[test]
    fn file_save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("groq.json");

        // Create, exhaust a key, save
        {
            let mut ring = make_ring(&["k1", "k2"]);
            ring.mark_exhausted(1, 60, Some("rate limited".into()));
            ring.save_to_file(&path);
            assert!(path.exists(), "state file should exist after save");
        }

        // Fresh ring, load from file
        {
            let mut ring = make_ring(&["k1", "k2"]);
            ring.load_from_file(&path);

            // k2 should be exhausted, k1 active
            assert_eq!(ring.active_count(), 1);
            assert_eq!(ring.exhausted_count(), 1);
            let (idx, _) = ring.next_available().unwrap();
            assert_eq!(idx, 0, "k1 should be the active key after load");
        }
    }

    #[test]
    fn load_from_missing_file_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");

        let mut ring = make_ring(&["k1"]);
        ring.load_from_file(&path); // should not panic
        assert_eq!(ring.active_count(), 1);
    }

    #[test]
    fn load_from_corrupt_file_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.json");
        std::fs::write(&path, "not valid json").unwrap();

        let mut ring = make_ring(&["k1"]);
        ring.load_from_file(&path); // should not panic
        assert_eq!(ring.active_count(), 1);
    }

    #[test]
    fn exhausted_keys_become_active_after_cooldown_expires_on_disk() {
        // Simulate: key was saved with 0s cooldown (cooldown already expired
        // while the app was closed). On load, it should be active.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("groq.json");

        let snapshot = ProviderKeyRingSnapshot {
            entries: vec![KeyRingEntrySnapshot {
                key: "k1".into(),
                cooldown_remaining_secs: 0, // expired
                last_error: Some("rate limited".into()),
            }],
            saved_at_unix: None,
        };
        serde_json::to_writer(std::fs::File::create(&path).unwrap(), &snapshot).unwrap();

        let mut ring = make_ring(&["k1"]);
        ring.load_from_file(&path);
        assert_eq!(ring.active_count(), 1, "expired cooldown should be active");
        // last_error should also be cleared since cooldown expired
        let statuses = ring.statuses();
        assert!(statuses[0].active);
        assert!(
            statuses[0].last_error.is_none(),
            "last_error should be None after expired cooldown"
        );
    }

    #[test]
    fn saved_at_unix_adjusts_cooldown_for_elapsed_time() {
        // A snapshot saved with 300s remaining + saved_at_unix from 120s ago:
        // cooldown should be 300 - 120 = 180s (not the full 300s).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stale.json");

        let saved_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(120); // 120s ago

        let snapshot = ProviderKeyRingSnapshot {
            entries: vec![KeyRingEntrySnapshot {
                key: "k1".into(),
                cooldown_remaining_secs: 300,
                last_error: Some("rate limited".into()),
            }],
            saved_at_unix: Some(saved_at),
        };
        serde_json::to_writer(std::fs::File::create(&path).unwrap(), &snapshot).unwrap();

        let mut ring = make_ring(&["k1"]);
        ring.load_from_file(&path);

        // Key should be exhausted (180s remaining > 0).
        assert_eq!(ring.active_count(), 0);
        assert_eq!(ring.exhausted_count(), 1);

        let statuses = ring.statuses();
        assert!(!statuses[0].active);
        // ~180s remaining, not the full 300s
        let remaining = statuses[0].cooldown_remaining_secs.unwrap();
        assert!(
            (170..=190).contains(&remaining),
            "expected ~180s remaining after 120s elapsed, got {}s",
            remaining
        );
        assert_eq!(
            statuses[0].last_error.as_deref(),
            Some("rate limited"),
            "last_error should be preserved for active cooldown"
        );
    }

    #[test]
    fn saved_at_unix_with_expired_cooldown_leaves_key_active() {
        // A snapshot saved with 60s remaining + saved_at_unix from 120s ago:
        // cooldown should be 0 → key is active.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("expired.json");

        let saved_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(120); // 120s ago

        let snapshot = ProviderKeyRingSnapshot {
            entries: vec![KeyRingEntrySnapshot {
                key: "k1".into(),
                cooldown_remaining_secs: 60, // less than elapsed
                last_error: Some("rate limited".into()),
            }],
            saved_at_unix: Some(saved_at),
        };
        serde_json::to_writer(std::fs::File::create(&path).unwrap(), &snapshot).unwrap();

        let mut ring = make_ring(&["k1"]);
        ring.load_from_file(&path);

        // Key should be active (cooldown fully expired).
        assert_eq!(ring.active_count(), 1, "expired cooldown => active");
        assert_eq!(ring.exhausted_count(), 0);

        let statuses = ring.statuses();
        assert!(statuses[0].active);
        assert!(
            statuses[0].last_error.is_none(),
            "last_error should be None for active key"
        );
    }
}
