//! AutoDream: automatic memory consolidation daemon
//!
//! Background memory consolidation. Fires a consolidation prompt as a forked
//! subagent when enough *work* has accumulated since the last consolidation
//! (importance model from Generative Agents / MemGPT-style memory tiers), with
//! a max-time backstop so a trickle of work is still consolidated, and an
//! exponential backoff on failed attempts so a broken provider does not hammer
//! the gate every turn.
//!
//! Gate order (cheapest first):
//! 1. Backoff: hours since last attempt (success or failure) >= backoff, where
//!    backoff = min_hours * 2^consecutive_failures (capped).
//! 2. Work: accumulated transcript importance >= min_importance, OR min_hours
//!    elapsed since last success with > 0 new work.
//! 3. Lock: no other process mid-consolidation (stale after 1 hour).

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::fs;

// Scan throttle: the importance scan is bounded by this interval so an
// importance gate that has not yet tripped doesn't rescan the transcript dir
// (and rewrite state) on every single turn.
pub const SESSION_SCAN_INTERVAL_SECS: u64 = 10 * 60; // 10 minutes

/// Per-file cap for a single importance scan. A massive session cannot dwarf
/// the work signal of many normal sessions.
const IMPORTANCE_CAP_PER_FILE: u64 = 100_000; // 100 KB

/// Upper bound on the failure backoff, so a permanently broken provider does
/// not push the next attempt out past one week.
const MAX_BACKOFF_HOURS: f64 = 168.0; // 7 days

/// Wall-clock budget for a single consolidation run (Phase 1c). Combined with
/// `max_turns: 20` this bounds the cost of a dream.
pub const DREAM_TIMEOUT_SECS: u64 = 20 * 60; // 20 minutes

/// GrowthBook-sourced scheduling config (with defaults)
#[derive(Debug, Clone)]
pub struct AutoDreamConfig {
    /// Normal cadence: hours between consolidations and the base backoff unit
    /// for failed attempts (default: 24)
    pub min_hours: f64,
    /// Accumulated transcript importance (bytes of new activity, per-file
    /// capped) that triggers consolidation regardless of the time cap
    /// (default: 150 KB ≈ a few meaty sessions)
    pub min_importance: f64,
}

impl Default for AutoDreamConfig {
    fn default() -> Self {
        Self {
            min_hours: 24.0,
            min_importance: 150_000.0,
        }
    }
}

/// Persisted state written to `.consolidation_state.json`
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConsolidationState {
    /// Unix timestamp (seconds) of the last *successful* consolidation.
    /// `None` means never consolidated.
    pub last_consolidated_at: Option<u64>,
    /// Unix timestamp (seconds) of the last attempt (success or failure).
    /// `None` means never attempted.
    #[serde(default)]
    pub last_attempt_at: Option<u64>,
    /// Consecutive failed attempts; drives the exponential backoff.
    #[serde(default)]
    pub consecutive_failures: u32,
    /// Accumulated importance since the last successful consolidation
    /// (bytes of new transcript activity, per-file capped). Reset to 0 on
    /// success; preserved on failure so the work signal is not lost.
    #[serde(default)]
    pub importance: f64,
    /// Per-file size watermark from the last importance scan. Kept across
    /// consolidations so already-counted bytes are never re-counted.
    #[serde(default)]
    pub scanned_sizes: BTreeMap<String, u64>,
    /// Unix timestamp (seconds) of the last importance scan (throttle).
    #[serde(default)]
    pub last_scan_secs: Option<u64>,
    /// ETag / opaque lock token – reserved for future distributed locking.
    pub lock_etag: Option<String>,
}

/// Data returned by `AutoDream::maybe_trigger` when consolidation should proceed.
/// Pass this to `AutoDream::finish_consolidation` after the agent completes.
#[derive(Debug, Clone)]
pub struct ConsolidationTask {
    /// The consolidation prompt to send to the sub-agent.
    pub prompt: String,
    /// Working directory for the sub-agent.
    pub memory_dir: PathBuf,
    /// Path to the state file (written after consolidation completes).
    pub state_file: PathBuf,
    /// Path to the lock file (removed after consolidation completes).
    pub lock_file: PathBuf,
}

/// Core AutoDream logic; owns path state, delegates I/O to async methods.
pub struct AutoDream {
    config: AutoDreamConfig,
    memory_dir: PathBuf,
    conversations_dir: PathBuf,
    lock_file: PathBuf,
    state_file: PathBuf,
}

impl AutoDream {
    pub fn new(memory_dir: PathBuf, conversations_dir: PathBuf) -> Self {
        let lock_file = memory_dir.join(".consolidation_lock");
        let state_file = memory_dir.join(".consolidation_state.json");
        Self {
            config: AutoDreamConfig::default(),
            memory_dir,
            conversations_dir,
            lock_file,
            state_file,
        }
    }

    /// Construct with explicit config (for testing / feature-flag overrides).
    pub fn with_config(
        config: AutoDreamConfig,
        memory_dir: PathBuf,
        conversations_dir: PathBuf,
    ) -> Self {
        let lock_file = memory_dir.join(".consolidation_lock");
        let state_file = memory_dir.join(".consolidation_state.json");
        Self {
            config,
            memory_dir,
            conversations_dir,
            lock_file,
            state_file,
        }
    }

    // -------------------------------------------------------------------------
    // Gate checks
    // -------------------------------------------------------------------------

    /// Check all gates cheapest-first.  Returns `true` if consolidation should run.
    pub async fn should_consolidate(&self, state: &ConsolidationState) -> Result<bool> {
        // Gate 1: Backoff gate (cheapest – one arithmetic check)
        if !self.backoff_gate_passes(state) {
            return Ok(false);
        }

        // Gate 2: Work gate (importance accumulator, no I/O)
        if !self.work_gate_passes(state) {
            return Ok(false);
        }

        // Gate 3: Lock gate (no other process mid-consolidation)
        if !self.lock_gate_passes().await? {
            return Ok(false);
        }

        Ok(true)
    }

    /// Backoff since the last attempt (success or failure). A failed attempt
    /// doubles the required wait, so a broken provider cannot retry every
    /// turn; a successful attempt restores the normal `min_hours` cadence.
    fn backoff_gate_passes(&self, state: &ConsolidationState) -> bool {
        let Some(last_attempt) = state.last_attempt_at else {
            return true; // never attempted
        };
        let backoff_hours = (self.config.min_hours
            * 2f64.powi(state.consecutive_failures.min(10) as i32))
        .min(MAX_BACKOFF_HOURS);
        let elapsed_hours = (now_secs().saturating_sub(last_attempt)) as f64 / 3600.0;
        elapsed_hours >= backoff_hours
    }

    /// Work signal: consolidate when enough importance has accumulated, or
    /// when the max-time cap elapsed with at least some new work (so a slow
    /// trickle is still consolidated, but a completely idle project never
    /// spends tokens dreaming about nothing).
    fn work_gate_passes(&self, state: &ConsolidationState) -> bool {
        if state.importance >= self.config.min_importance {
            return true;
        }
        if state.importance <= 0.0 {
            return false;
        }
        let Some(last_success) = state.last_consolidated_at else {
            return true; // never consolidated, some work pending
        };
        let hours_elapsed = (now_secs().saturating_sub(last_success)) as f64 / 3600.0;
        hours_elapsed >= self.config.min_hours
    }

    /// Accumulate importance from new transcript bytes since the last scan
    /// and persist the watermark. Throttled to `SESSION_SCAN_INTERVAL_SECS`.
    ///
    /// Importance = total bytes added to the transcript dir since the last
    /// scan, per-file capped at `IMPORTANCE_CAP_PER_FILE` so one giant session
    /// cannot dwarf the work signal. A brand-new session counts its full size.
    pub async fn record_importance(&self, state: &mut ConsolidationState) -> Result<f64> {
        let now = now_secs();
        if state
            .last_scan_secs
            .is_some_and(|last| now.saturating_sub(last) < SESSION_SCAN_INTERVAL_SECS)
        {
            return Ok(state.importance);
        }

        if self.conversations_dir.is_dir() {
            let mut dir = fs::read_dir(&self.conversations_dir).await?;
            while let Some(entry) = dir.next_entry().await? {
                let path = entry.path();
                let is_jsonl = path.extension().map(|e| e == "jsonl").unwrap_or(false);
                if !is_jsonl {
                    continue;
                }
                let Ok(meta) = entry.metadata().await else {
                    continue;
                };
                if !meta.is_file() {
                    continue;
                }
                let file_name = entry.file_name().to_string_lossy().into_owned();
                let size = meta.len();
                let prev = state.scanned_sizes.get(&file_name).copied().unwrap_or(0);
                let delta = size.saturating_sub(prev).min(IMPORTANCE_CAP_PER_FILE);
                if delta > 0 {
                    state.importance += delta as f64;
                    state.scanned_sizes.insert(file_name, size);
                }
            }
        }

        state.last_scan_secs = Some(now);
        self.save_state(state).await?;
        Ok(state.importance)
    }

    async fn lock_gate_passes(&self) -> Result<bool> {
        if !self.lock_file.exists() {
            return Ok(true);
        }

        // Stale lock (>1 hour) is treated as released
        match fs::metadata(&self.lock_file).await {
            Ok(meta) => {
                if let Ok(mtime) = meta.modified() {
                    let age_secs = SystemTime::now()
                        .duration_since(mtime)
                        .unwrap_or(Duration::ZERO)
                        .as_secs();
                    Ok(age_secs > 3600)
                } else {
                    // Cannot stat mtime → conservative: gate passes (treat as stale)
                    Ok(true)
                }
            }
            Err(_) => Ok(true), // File disappeared between exists() and metadata()
        }
    }

    // -------------------------------------------------------------------------
    // Lock management
    // -------------------------------------------------------------------------

    /// Write a timestamp to the lock file, creating it if absent.
    pub async fn acquire_lock(&self) -> Result<()> {
        if let Some(parent) = self.lock_file.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&self.lock_file, now_secs().to_string()).await?;
        Ok(())
    }

    /// Remove the lock file.  No-op if it doesn't exist.
    pub async fn release_lock(&self) -> Result<()> {
        if self.lock_file.exists() {
            fs::remove_file(&self.lock_file).await?;
        }
        Ok(())
    }

    // -------------------------------------------------------------------------
    // State persistence
    // -------------------------------------------------------------------------

    /// Persist `state` to the state file (creating parents as needed).
    pub async fn save_state(&self, state: &ConsolidationState) -> Result<()> {
        let json = serde_json::to_string_pretty(state)?;
        if let Some(parent) = self.state_file.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&self.state_file, json).await?;
        Ok(())
    }

    /// Stamp `last_consolidated_at = now`, reset the importance accumulator
    /// (the watermark stays, so already-counted bytes are never re-counted),
    /// and persist.
    pub async fn update_state(&self, state: &mut ConsolidationState) -> Result<()> {
        state.last_consolidated_at = Some(now_secs());
        state.last_attempt_at = Some(now_secs());
        state.consecutive_failures = 0;
        state.importance = 0.0;
        self.save_state(state).await
    }

    /// Load persisted state; returns `Default` on any error (missing file, parse failure).
    pub async fn load_state(&self) -> ConsolidationState {
        match fs::read_to_string(&self.state_file).await {
            Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
            Err(_) => ConsolidationState::default(),
        }
    }

    // -------------------------------------------------------------------------
    // High-level trigger
    // -------------------------------------------------------------------------

    /// Scan transcript activity, check all gates and, if they pass, acquire
    /// the lock and return the info needed to run the consolidation subagent.
    ///
    /// Returns `Ok(Some(task))` if consolidation should proceed (lock acquired),
    /// `Ok(None)` if gated out, or `Err` for hard I/O failures.
    ///
    /// The caller is responsible for actually running the agent and calling
    /// `finish_consolidation(task, success)` when done.
    pub async fn maybe_trigger(&self) -> Result<Option<ConsolidationTask>> {
        let mut state = self.load_state().await;
        // Fold new transcript activity into the importance accumulator first,
        // so a burst of work since the last scan can trip the work gate.
        self.record_importance(&mut state).await?;
        if !self.should_consolidate(&state).await? {
            return Ok(None);
        }
        self.acquire_lock().await?;
        Ok(Some(ConsolidationTask {
            prompt: self.consolidation_prompt(),
            memory_dir: self.memory_dir.clone(),
            state_file: self.state_file.clone(),
            lock_file: self.lock_file.clone(),
        }))
    }

    /// Persist the updated consolidation state and release the lock.
    /// Call this after the consolidation subagent completes (or fails).
    ///
    /// On success: stamp `last_consolidated_at`, reset the importance
    /// accumulator and failure counter. On failure: leave the success
    /// watermark untouched, bump the failure counter (which doubles the
    /// backoff), and preserve the accumulated importance so the work signal
    /// survives for the next attempt.
    pub async fn finish_consolidation(task: &ConsolidationTask, success: bool) {
        let mut state = match tokio::fs::read_to_string(&task.state_file).await {
            Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
            Err(_) => ConsolidationState::default(),
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(std::time::Duration::ZERO)
            .as_secs();
        state.last_attempt_at = Some(now);
        if success {
            state.last_consolidated_at = Some(now);
            state.consecutive_failures = 0;
            state.importance = 0.0;
        } else {
            state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        }
        if let Ok(json) = serde_json::to_string_pretty(&state) {
            if let Some(parent) = task.state_file.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            let _ = tokio::fs::write(&task.state_file, json).await;
        }
        if task.lock_file.exists() {
            let _ = tokio::fs::remove_file(&task.lock_file).await;
        }
    }

    // -------------------------------------------------------------------------
    // Prompt construction
    // -------------------------------------------------------------------------

    /// Build the consolidation prompt for the forked subagent.
    pub fn consolidation_prompt(&self) -> String {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        format!(
            r#"# Dream: Memory Consolidation

You are performing a dream — a reflective pass over your memory files. Synthesize what you have learned recently into durable, well-organized memories so that future sessions can orient quickly.

Memory directory: `{memory_dir}`

Session transcripts: `{conv_dir}` (large JSONL files — grep narrowly, do not read whole files)

---

## Phase 1 — Orient

- `ls` the memory directory to see what already exists
- Read `MEMORY.md` to understand the current index
- Skim existing topic files so you improve them rather than creating duplicates

## Phase 2 — Gather recent signal

Look for new information worth persisting:

1. **Daily logs** (`logs/YYYY/MM/YYYY-MM-DD.md`) if present
2. **Existing memories that drifted** — facts that contradict what you see now
3. **Transcript search** — grep narrowly for specific terms:
   `grep -rn "<narrow term>" {conv_dir}/ --include="*.jsonl" | tail -50`

Do not exhaustively read transcripts. Look only for things you already suspect matter.

## Phase 3 — Consolidate

For each thing worth remembering, write or update a memory file. Focus on:
- Merging new signal into existing topic files rather than creating near-duplicates
- Converting relative dates to absolute dates
- **Superseding, not deleting, contradicted facts**: when a new fact replaces an
  old one, write the new fact into the current file (or a new `*-v2` file) and set
  its frontmatter `supersedes:` to the old file's name (comma-separated if several).
  Leave the old file in place — never delete it. A superseded file stays on disk as
  auditable history; the `supersedes:` link marks it stale.
- **Route uncertain contradictions to `conflicts:`, not `supersedes:`**: a
  `supersedes:` demotes the old fact immediately — only do that when you are
  confident the old fact is verifiably wrong (e.g. a project fact you can check
  against the current code). When the contradiction concerns a `user` or
  `feedback` memory, or you are not confident, write the new fact with
  `conflicts: <old-file>` instead. A `conflicts:` claim never demotes anything:
  both facts stay active and the user adjudicates in a future session.
- **Never re-litigate an existing conflict**: if a file already has `conflicts:`
  or `asked:` (naming the target) frontmatter, leave the pair alone — do not
  change its status, do not set `supersedes:` on it, do not re-flag it. The
  same applies to a pair the user already adjudicated: if `resolved:` names the
  target, the contradiction is a settled decision — never re-flag it even if
  the two files still disagree.
- **Never flag a conflict against an already-superseded file**: if another
  memory already lists the target in its `supersedes:`, the target is stale —
  writing a new `conflicts:` claim against it is dead weight (nothing will
  ever adjudicate it). The old fact is settled; there is nothing to ask the
  user about.
- **Re-flagging an `asked:` pair requires new evidence**: an `asked:` entry
  records that the user did not know on that date. You may re-flag the pair
  ONLY when you find substantial new contradicting evidence dated after the
  ask date (e.g. a later transcript that settles it). Otherwise leave it alone.

Memory files may carry YAML frontmatter: `name`, `description`, `type`
(`user` | `feedback` | `project` | `reference`), `created` (first-written date),
`updated` (last-touched date), `supersedes` (filenames this memory replaces,
confirmed), `conflicts` (filenames this memory claims are wrong, pending user
adjudication — the target stays active), `asked` (per-pair conflict targets the
user was consulted about and left unresolved, written `target:YYYY-MM-DD`;
never ask again after this, see the re-flag rule above), and `resolved`
(conflict targets the user already adjudicated; never re-flag a pair named
here). Stamp `updated: {today}` on every file you modify, and set `created:`
when you create a file.

Then write the consolidation-window recap to `sessions/{today}.md` under the memory
directory (create the `sessions/` dir if needed). This recap is injected into the
system prompt of the next session as the "Recent Session Summary", so it must stand
alone. Format:

```
# Session summary — {today}

- One bullet per durable topic touched this window: what changed, decisions made,
  and any facts that supersede older memories.
- One bullet per conflict you flagged this window: the pair (`X.md` vs `Y.md`),
  why you flagged it, and the transcript evidence that prompted it (file + line
  or quote), so the user can adjudicate with the source in hand.
```

Keep it under ~60 lines / 4 KB. Overwrite the file if it already exists (one recap
per day). If nothing was worth recording this window, still write a one-line recap
so the summary is never stale.

## Phase 4 — Prune and index

Update `MEMORY.md` so it stays under 200 lines and ~25 KB. It is an **index**, not a dump.
Each entry: `- [Title](file.md) — one-line hook`

- Remove pointers to stale, wrong, or superseded memories — a file listed in
  another memory's `supersedes:` is stale and must not appear in the index
- A file that is the target of a `conflicts:` claim is under review, not stale:
  keep it in the index until the user adjudicates
- Shorten verbose entries; move detail into topic files
- Add pointers to newly important memories
- Never resolve a `user`/`feedback` conflict yourself — the user adjudicates
  those in-session; your job here is only index accuracy (drop confirmed
  supersedes, keep under-review entries).

---

Return a brief summary of what you consolidated, updated, or pruned. If nothing changed, say so.

**Tool constraints for this run (enforced):** You have no shell access. Use Read, Glob,
and Grep to inspect files and transcripts, and Write ONLY inside the memory directory
`{memory_dir}` (memory files, the `sessions/` recap, `MEMORY.md`). Never write anywhere
else.
"#,
            memory_dir = self.memory_dir.display(),
            conv_dir = self.conversations_dir.display(),
            today = today,
        )
    }
}

// -------------------------------------------------------------------------
// Helpers
// -------------------------------------------------------------------------

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_dream(tmp: &TempDir) -> AutoDream {
        let mem = tmp.path().join("memory");
        let conv = tmp.path().join("conversations");
        AutoDream::new(mem, conv)
    }

    // --- work_gate_passes ---

    #[test]
    fn test_work_gate_importance_over_threshold_passes() {
        let tmp = TempDir::new().unwrap();
        let dream = make_dream(&tmp);
        let state = ConsolidationState {
            importance: 200_000.0, // above the 150 KB default
            ..Default::default()
        };
        assert!(dream.work_gate_passes(&state));
    }

    #[test]
    fn test_work_gate_below_threshold_needs_time_cap() {
        let tmp = TempDir::new().unwrap();
        let dream = make_dream(&tmp);
        // Some work, but below the threshold and consolidated recently.
        let state = ConsolidationState {
            importance: 10_000.0,
            last_consolidated_at: Some(now_secs()),
            ..Default::default()
        };
        assert!(!dream.work_gate_passes(&state));

        // Same work, but the max-time cap elapsed → backstop fires.
        let old = now_secs().saturating_sub(25 * 3600);
        let state = ConsolidationState {
            importance: 10_000.0,
            last_consolidated_at: Some(old),
            ..Default::default()
        };
        assert!(dream.work_gate_passes(&state));
    }

    #[test]
    fn test_work_gate_zero_importance_never_passes() {
        let tmp = TempDir::new().unwrap();
        let dream = make_dream(&tmp);
        // Idle project must not spend tokens dreaming about nothing, even when
        // the time cap has long since elapsed.
        let old = now_secs().saturating_sub(30 * 3600);
        let state = ConsolidationState {
            importance: 0.0,
            last_consolidated_at: Some(old),
            ..Default::default()
        };
        assert!(!dream.work_gate_passes(&state));
    }

    // --- backoff_gate_passes ---

    #[test]
    fn test_backoff_never_attempted_passes() {
        let tmp = TempDir::new().unwrap();
        let dream = make_dream(&tmp);
        let state = ConsolidationState::default();
        assert!(dream.backoff_gate_passes(&state));
    }

    #[test]
    fn test_backoff_blocks_immediate_retry() {
        let tmp = TempDir::new().unwrap();
        let dream = make_dream(&tmp);
        let state = ConsolidationState {
            last_attempt_at: Some(now_secs()), // just failed
            consecutive_failures: 1,
            ..Default::default()
        };
        assert!(!dream.backoff_gate_passes(&state));
    }

    #[test]
    fn test_backoff_exponential_and_capped() {
        let tmp = TempDir::new().unwrap();
        let dream = make_dream(&tmp);
        // 1 failure → 48h wait; 2 failures → 96h; capped at 7 days.
        for (failures, min_wait_hours) in [(1u32, 48.0), (2, 96.0), (5, 168.0)] {
            let wait_secs = (min_wait_hours * 3600.0) as u64 + 60; // just past the wait
            let state = ConsolidationState {
                last_attempt_at: Some(now_secs().saturating_sub(wait_secs)),
                consecutive_failures: failures,
                ..Default::default()
            };
            assert!(
                dream.backoff_gate_passes(&state),
                "failures={} should pass after {}h",
                failures,
                min_wait_hours
            );
            let state = ConsolidationState {
                last_attempt_at: Some(now_secs().saturating_sub(wait_secs / 2)),
                consecutive_failures: failures,
                ..Default::default()
            };
            assert!(
                !dream.backoff_gate_passes(&state),
                "failures={} should block before {}h",
                failures,
                min_wait_hours
            );
        }
    }

    #[test]
    fn test_with_config_overrides_gates() {
        let tmp = TempDir::new().unwrap();
        // A low threshold makes the work gate trip almost immediately,
        // exercising the explicit-config constructor.
        let dream = AutoDream::with_config(
            AutoDreamConfig {
                min_hours: 1.0,
                min_importance: 100.0,
            },
            tmp.path().join("memory"),
            tmp.path().join("conversations"),
        );
        let state = ConsolidationState {
            importance: 500.0,
            ..Default::default()
        };
        assert!(dream.work_gate_passes(&state));
        assert!(!dream.work_gate_passes(&ConsolidationState::default()));
    }

    // --- record_importance ---

    #[tokio::test]
    async fn test_record_importance_counts_new_transcript_bytes() {
        let tmp = TempDir::new().unwrap();
        let dream = make_dream(&tmp);
        std::fs::create_dir_all(&dream.conversations_dir).unwrap();
        let a = dream.conversations_dir.join("sess-a.jsonl");
        std::fs::write(&a, vec![b'x'; 10_000]).unwrap();

        let mut state = ConsolidationState::default();
        let importance = dream.record_importance(&mut state).await.unwrap();
        assert_eq!(importance, 10_000.0);
        assert_eq!(state.scanned_sizes.get("sess-a.jsonl"), Some(&10_000));

        // Same bytes again → no double counting (throttle would also block
        // a rescan, so bump the watermark directly and rescan after clearing
        // the throttle).
        state.last_scan_secs = None;
        let importance = dream.record_importance(&mut state).await.unwrap();
        assert_eq!(importance, 10_000.0);

        // New activity on the same file adds only the delta.
        std::fs::write(&a, vec![b'x'; 25_000]).unwrap();
        state.last_scan_secs = None;
        let importance = dream.record_importance(&mut state).await.unwrap();
        assert_eq!(importance, 25_000.0);
    }

    #[tokio::test]
    async fn test_record_importance_throttles_rescans() {
        let tmp = TempDir::new().unwrap();
        let dream = make_dream(&tmp);
        std::fs::create_dir_all(&dream.conversations_dir).unwrap();
        let a = dream.conversations_dir.join("sess-a.jsonl");
        std::fs::write(&a, vec![b'x'; 5_000]).unwrap();

        let mut state = ConsolidationState::default();
        let first = dream.record_importance(&mut state).await.unwrap();
        assert_eq!(first, 5_000.0);

        // Second scan within the throttle window: no rescan, no persistence.
        let second = dream.record_importance(&mut state).await.unwrap();
        assert_eq!(second, 5_000.0);
    }

    // --- lock_gate_passes (sync-friendly via tokio::test) ---

    #[tokio::test]
    async fn test_lock_gate_no_lock_file() {
        let tmp = TempDir::new().unwrap();
        let dream = make_dream(&tmp);
        assert!(dream.lock_gate_passes().await.unwrap());
    }

    #[tokio::test]
    async fn test_lock_gate_fresh_lock_blocks() {
        let tmp = TempDir::new().unwrap();
        let dream = make_dream(&tmp);
        std::fs::create_dir_all(&dream.memory_dir).unwrap();
        std::fs::write(&dream.lock_file, "12345").unwrap();
        // Fresh file → gate blocked
        assert!(!dream.lock_gate_passes().await.unwrap());
    }

    // --- consolidation_prompt sanity ---

    #[test]
    fn test_consolidation_prompt_contains_paths() {
        let tmp = TempDir::new().unwrap();
        let dream = make_dream(&tmp);
        let prompt = dream.consolidation_prompt();
        assert!(prompt.contains("MEMORY.md"));
        assert!(prompt.contains("Memory Consolidation"));
        assert!(prompt.contains("Phase 1"));
        assert!(prompt.contains("Phase 4"));
    }

    #[test]
    fn test_consolidation_prompt_instructs_session_summary_write() {
        let tmp = TempDir::new().unwrap();
        let dream = make_dream(&tmp);
        let prompt = dream.consolidation_prompt();
        // The dream must write the recap the injection pipeline reads. The
        // `{today}` placeholder is substituted with the real local date.
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert!(
            prompt.contains(&format!("sessions/{}.md", today)),
            "got: {}",
            prompt
        );
        assert!(prompt.contains("Recent Session Summary"));
        assert!(prompt.contains("Session summary —")); // The tool constraint must state the enforced boundary, not Bash prose.
        assert!(
            prompt.contains("You have no shell access"),
            "got: {}",
            prompt
        );
        assert!(prompt.contains("Write ONLY inside the memory directory"));
        // Contradictions are superseded via frontmatter, never deleted.
        assert!(prompt.contains("Superseding, not deleting, contradicted facts"));
        assert!(prompt.contains("`supersedes:` to the old file's name"));
        assert!(prompt.contains("Leave the old file in place"));
        // `{today}` is substituted with the real local date by `format!`.
        assert!(prompt.contains(&format!("Stamp `updated: {}`", today)));
        // The index must drop superseded files.
        assert!(prompt.contains("must not appear in the index"));
        // Uncertain/user-truth contradictions route to `conflicts:`, never
        // `supersedes:`, and existing conflicts are never re-litigated.
        assert!(prompt.contains("Route uncertain contradictions to `conflicts:`"));
        assert!(prompt.contains("both facts stay active and the user adjudicates"));
        assert!(prompt.contains("Never re-litigate an existing conflict"));
        // Superseded targets are settled — never flag a new claim against them.
        assert!(prompt.contains("Never flag a conflict against an already-superseded file"));
        assert!(prompt.contains("dead weight"));
        assert!(prompt.contains("`asked` (per-pair conflict targets the"));
        assert!(prompt.contains("written `target:YYYY-MM-DD`"));
        assert!(prompt.contains("never ask again after this"));
        // User-adjudicated pairs (`resolved:`) are also off-limits.
        assert!(prompt.contains("`resolved`"));
        assert!(prompt.contains("conflict targets the user already adjudicated"));
        assert!(prompt.contains("settled decision — never re-flag it"));
        // The dream proposes conflicts, never resolves user conflicts itself.
        assert!(prompt.contains("Never resolve a `user`/`feedback` conflict yourself"));
        // Re-flagging an asked pair requires new post-date evidence.
        assert!(prompt.contains("Re-flagging an `asked:` pair requires new evidence"));
        assert!(prompt.contains("dated after the"));
        assert!(prompt.contains("ask date"));
        // Provenance: the recap lists flagged conflicts with their evidence.
        assert!(prompt.contains("One bullet per conflict you flagged this window"));
        assert!(prompt.contains("the transcript evidence that prompted it"));
    }

    // --- update_state / load_state round-trip ---

    #[tokio::test]
    async fn test_state_round_trip() {
        let tmp = TempDir::new().unwrap();
        let dream = make_dream(&tmp);
        std::fs::create_dir_all(&dream.memory_dir).unwrap();

        let mut state = ConsolidationState::default();
        dream.update_state(&mut state).await.unwrap();

        assert!(state.last_consolidated_at.is_some());
        let loaded = dream.load_state().await;
        assert_eq!(loaded.last_consolidated_at, state.last_consolidated_at);
    }

    // --- finish_consolidation (success vs failure) ---

    #[tokio::test]
    async fn test_finish_success_resets_and_stamps() {
        let tmp = TempDir::new().unwrap();
        let dream = make_dream(&tmp);
        std::fs::create_dir_all(&dream.memory_dir).unwrap();
        let mut state = ConsolidationState {
            importance: 500_000.0,
            consecutive_failures: 3,
            last_attempt_at: Some(now_secs()),
            ..Default::default()
        };
        state
            .scanned_sizes
            .insert("sess-a.jsonl".to_string(), 12_345);
        dream.save_state(&state).await.unwrap();
        dream.acquire_lock().await.unwrap();

        let task = ConsolidationTask {
            prompt: "x".to_string(),
            memory_dir: dream.memory_dir.clone(),
            state_file: dream.state_file.clone(),
            lock_file: dream.lock_file.clone(),
        };
        AutoDream::finish_consolidation(&task, true).await;

        let loaded = dream.load_state().await;
        assert!(loaded.last_consolidated_at.is_some());
        assert!(loaded.last_attempt_at.is_some());
        assert_eq!(loaded.consecutive_failures, 0);
        assert_eq!(loaded.importance, 0.0);
        // Watermark survives so already-counted bytes are never re-counted.
        assert_eq!(loaded.scanned_sizes.get("sess-a.jsonl"), Some(&12_345));
        assert!(!dream.lock_file.exists());
    }

    #[tokio::test]
    async fn test_finish_failure_preserves_work_and_backs_off() {
        let tmp = TempDir::new().unwrap();
        let dream = make_dream(&tmp);
        std::fs::create_dir_all(&dream.memory_dir).unwrap();
        let state = ConsolidationState {
            importance: 250_000.0,
            last_consolidated_at: Some(now_secs().saturating_sub(2 * 3600)),
            ..Default::default()
        };
        dream.save_state(&state).await.unwrap();
        dream.acquire_lock().await.unwrap();

        let task = ConsolidationTask {
            prompt: "x".to_string(),
            memory_dir: dream.memory_dir.clone(),
            state_file: dream.state_file.clone(),
            lock_file: dream.lock_file.clone(),
        };
        AutoDream::finish_consolidation(&task, false).await;

        let loaded = dream.load_state().await;
        // Success watermark untouched: the failure must not burn the time cap.
        assert_eq!(loaded.last_consolidated_at, state.last_consolidated_at);
        assert_eq!(loaded.consecutive_failures, 1);
        assert_eq!(loaded.importance, 250_000.0);
        assert!(loaded.last_attempt_at.is_some());
        assert!(!dream.lock_file.exists());
    }

    // --- acquire_lock / release_lock ---

    #[tokio::test]
    async fn test_acquire_release_lock() {
        let tmp = TempDir::new().unwrap();
        let dream = make_dream(&tmp);

        dream.acquire_lock().await.unwrap();
        assert!(dream.lock_file.exists());

        dream.release_lock().await.unwrap();
        assert!(!dream.lock_file.exists());
    }
}
