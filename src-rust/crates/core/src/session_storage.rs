// session_storage.rs — JSONL transcript persistence for Clawde.
//
// File layout:  ~/.clawde/projects/{base64url(project_root)}/{session_id}.jsonl
//
// Each line is a JSON object ("entry") whose `type` field is the discriminant.
// The schema is kept compatible with the TypeScript `Entry` union in
// `src/types/logs.ts` so that files written by the TS CLI can be read here
// and vice-versa.
//
// Only the entry types that the Rust port generates are implemented here.
// Unknown/future entry types round-trip as `Other(Value)` so they are
// preserved when rewriting the file (tombstone path).

use std::path::{Path, PathBuf};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::Message;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum transcript file size for read operations (load / tombstone rewrite).
/// Files larger than this are not read to avoid OOM on huge sessions.
pub const MAX_TRANSCRIPT_BYTES: u64 = 50 * 1024 * 1024; // 50 MB

// ---------------------------------------------------------------------------
// TranscriptEntry — the wire-format discriminated union
// ---------------------------------------------------------------------------

/// A single line in a `.jsonl` transcript file.
///
/// Variants are serialised with a `"type"` field that matches the TypeScript
/// `Entry` union.  Only variants the Rust port actively uses are named; every
/// other entry type is preserved as a raw `serde_json::Value` via `Other`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum TranscriptEntry {
    /// A user turn.
    User(TranscriptMessage),
    /// An assistant turn.
    Assistant(TranscriptMessage),
    /// An inline attachment (image, file, etc.).
    Attachment(TranscriptMessage),
    /// A system message.
    System(TranscriptMessage),
    /// A compacted-context summary produced by the compaction logic.
    Summary(SummaryEntry),
    /// An AI-generated session title (written by the auto-titler, not the user).
    #[serde(rename = "ai-title")]
    AiTitle(AiTitleEntry),
    /// A user-set custom session title.
    #[serde(rename = "custom-title")]
    CustomTitle(CustomTitleEntry),
    /// The most-recent user prompt, re-appended at session exit for fast tail reads.
    #[serde(rename = "last-prompt")]
    LastPrompt(LastPromptEntry),
    /// Marks an entry as deleted. The Rust port uses this to remove messages
    /// without rewriting the entire file.
    #[serde(rename = "tombstone")]
    Tombstone(TombstoneEntry),
    /// Marks the active tip ("leaf") of the session tree — the entry the active
    /// branch ends at. Written append-only whenever the active branch changes
    /// (e.g. after a non-destructive revert/rewind). The LAST `leaf` entry in
    /// the file wins; earlier leaf pointers are superseded. Sessions written
    /// before #234 have no `leaf` entry and default to "leaf = last message"
    /// (identical linear behavior) — see [`active_branch_messages`].
    #[serde(rename = "leaf")]
    Leaf(LeafEntry),
    /// A runtime agent-state observation (evidence, decisions, validation,
    /// complexity signals). Append-only; consumed by task-state replay on
    /// resume. Sessions written before this variant existed contain none,
    /// and resume identically.
    #[serde(rename = "state-event")]
    StateEvent(StateEventEntry),
    /// A compacted task-state projection at an event watermark (incremental
    /// replay). Cache-only: validated on read and discarded on any doubt.
    /// Sessions written before this variant existed contain none, and resume
    /// identically via the full event replay.
    #[serde(rename = "state-snapshot")]
    StateSnapshot(StateSnapshotEntry),
    /// Marks a rewind point for state-event branch anchoring. Append-only;
    /// the LAST cut before an event decides its branch membership. Sessions
    /// written before this variant existed contain none.
    #[serde(rename = "state-cut")]
    StateCut(StateCutEntry),
    /// Any other entry type we do not need to inspect — round-tripped verbatim.
    #[serde(other, skip_serializing)]
    Unknown,
}

impl TranscriptEntry {
    /// Returns the `uuid` of the underlying message, if this is a transcript
    /// message type (user / assistant / attachment / system).
    pub fn uuid(&self) -> Option<&str> {
        match self {
            Self::User(m) | Self::Assistant(m) | Self::Attachment(m) | Self::System(m) => {
                m.uuid.as_deref()
            }
            _ => None,
        }
    }

    /// Return true if this entry is a user or assistant message
    /// (i.e. contributes to the conversation chain).
    pub fn is_chain_participant(&self) -> bool {
        matches!(self, Self::User(_) | Self::Assistant(_))
    }

    /// Returns the state event payload if this is a `state-event` entry.
    pub fn state_event(&self) -> Option<&StateEvent> {
        match self {
            Self::StateEvent(entry) => Some(&entry.event),
            _ => None,
        }
    }

    /// Returns the snapshot payload if this is a `state-snapshot` entry.
    pub fn state_snapshot(&self) -> Option<&StateSnapshot> {
        match self {
            Self::StateSnapshot(entry) => Some(&entry.snapshot),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// TranscriptMessage — the shape shared by user / assistant / system entries.
//
// Fields map to the TypeScript `TranscriptMessage` type in `src/types/logs.ts`.
// ---------------------------------------------------------------------------

/// A conversation message as stored in the transcript JSONL.
///
/// This is the serialised form of a single `user` or `assistant` turn.
/// It embeds a `message` object (the payload sent/received by the Anthropic API)
/// plus session-book-keeping fields used by the resume / history UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptMessage {
    /// Stable UUID for this entry (used as the primary key in the chain).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,

    /// UUID of the preceding chain-participant entry, or `null` at the start.
    pub parent_uuid: Option<String>,

    /// ISO-8601 timestamp when this entry was written.
    pub timestamp: String,

    /// Session ID (UUID) this entry belongs to.
    pub session_id: String,

    /// Working directory when this entry was written.
    pub cwd: String,

    /// The API message payload (role + content).
    pub message: Message,

    /// Whether this message belongs to a sidechain / sub-agent transcript.
    #[serde(default)]
    pub is_sidechain: bool,

    /// `"external"` | `"internal"` — mirrors TS `getUserType()`.
    #[serde(default = "default_user_type")]
    pub user_type: String,

    /// Version of the Clawde binary, mirrors `MACRO.VERSION`.
    #[serde(default)]
    pub version: String,

    /// Git branch at the time this message was written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,

    /// Agent role in the managed-agent architecture: "manager" | "executor".
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub agent_role: Option<String>,

    /// Managed session ID linking manager and executor transcripts.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub managed_session_id: Option<String>,

    /// Catch-all for any other fields written by the TS CLI that we don't
    /// need to inspect.
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, Value>,
}

fn default_user_type() -> String {
    "external".to_string()
}

// ---------------------------------------------------------------------------
// StateEvent — runtime agent-state events for task-state replay.
//
// The transcript stores messages; these events store the OBSERVED runtime
// facts that a transcript-derived rebuild cannot recover (verified checks,
// decisions, complexity signals, focus transitions). Events are append-only
// metadata: a session with zero state events must resume identically to one
// loaded before this schema existed.
// ---------------------------------------------------------------------------

/// Verdict carried by a validation event. Mirrors the query crate's
/// `ValidationVerdict` (core cannot depend on query).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateValidationVerdict {
    Passed,
    Failed,
    Unknown,
}

/// A single runtime observation about agent task state. Serde-tagged with
/// `snake_case` variant names; unknown future variants round-trip as
/// [`TranscriptEntry::Unknown`] at the entry level.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StateEvent {
    /// A tool result was observed. Failed results feed retry-loop detection.
    ToolObserved {
        failed: bool,
        /// Bounded summary of the failure (empty on success).
        #[serde(default)]
        summary: String,
        /// File paths referenced by the tool call, when the tool exposed one.
        #[serde(default)]
        file_paths: Vec<String>,
        /// Whether the tool mutates files (drives changed-file tracking).
        #[serde(default)]
        mutating: bool,
    },
    /// A validation round completed. Only `Passed` may produce Verified
    /// evidence on replay — the claim/proof boundary is preserved here.
    ValidationRecorded {
        verdict: StateValidationVerdict,
        headline: String,
    },
    /// A snapshot observed files changed on disk.
    SnapshotObserved { files: Vec<String> },
    /// An explicit user decision was recorded.
    DecisionRecorded {
        statement: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence: Option<String>,
    },
    /// The focus state changed (blocked/suspended/etc.) with the reason.
    FocusChanged {
        /// New focus as the query crate's `FocusState::as_str()`.
        focus: String,
        reason: String,
    },
    /// The active plan step was set (from the approved-plan harness).
    PlanStepSet { step: String },
    /// The one-shot simplification review fired for this run.
    SimplificationReviewed,
}

/// Metadata-only entry payload carrying one [`StateEvent`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateEventEntry {
    pub session_id: String,
    /// ISO-8601 timestamp when the event was observed.
    pub timestamp: String,
    pub event: StateEvent,
    /// Branch anchor: the index of the assistant message whose tool round
    /// produced this event, counted in the loop's in-memory `messages` vec —
    /// the exact vec a rewind truncates. Extraction compares it against the
    /// `state-cut` marker so events written on an abandoned branch are
    /// excluded from replay. `None` on legacy events and pre-turn emissions
    /// (both always kept).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg_index: Option<u32>,
}

// ---------------------------------------------------------------------------
// StateSnapshot — compacted task-state projection for incremental replay.
//
// A long session accumulates one `state-event` line per tool result; replaying
// every event from scratch on each resume grows with history. A snapshot is
// the projected fold of the state events written so far (plus the
// transcript-derived facts those events depend on), persisted at an event
// watermark. On load, the snapshot replaces events `0..event_count` and only
// the events after it are folded — replay cost tracks the increment since the
// last snapshot, not the whole session.
//
// Snapshots are CACHED DERIVED VALUES, never a source of truth: they are
// validated on read (schema version + event-count watermark against the lines
// actually present) and silently discarded on any doubt, in which case the
// caller falls back to the full event replay. Sessions written before this
// variant existed contain no snapshot and resume identically.
// ---------------------------------------------------------------------------

/// Version of the state fold the snapshot body represents. Bump whenever
/// [`crate::session_storage`] fold semantics or the body shape change;
/// stored snapshots with an older version are discarded on read and
/// re-derived by full replay. "Wiping every snapshot is always safe" — a
/// stale snapshot must never outrank the event log.
pub const STATE_SNAPSHOT_SCHEMA_VERSION: u32 = 1;

/// A single projected task-state snapshot at an event watermark.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct StateSnapshot {
    /// [`STATE_SNAPSHOT_SCHEMA_VERSION`] this body was folded with. Mismatched
    /// snapshots are discarded by the loader.
    pub schema_version: u32,
    /// Number of `state-event` lines folded into this snapshot. The loader
    /// counts the events written before the snapshot's line and discards the
    /// snapshot on mismatch (a rewritten/compacted transcript invalidates it).
    pub event_count: u64,
    /// The projected event-derived state.
    pub body: StateSnapshotBody,
}

/// Serialized event-derived projection. Plain strings (not enums) keep the
/// core schema stable and dependency-free; the query crate maps them back to
/// its typed enums with a safe fallback. Fields are deliberately limited to
/// what the EVENT fold produces — objective, constraints, scope-expansion
/// count, and turn are transcript-derived and re-derived from messages on
/// load, so storing them would only age stale.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct StateSnapshotBody {
    pub decisions: Vec<StateSnapshotDecision>,
    pub evidence: Vec<StateSnapshotEvidence>,
    pub changed_files: Vec<String>,
    /// Failures are the source of the replayed focus signal: replay() blocks
    /// focus when any failure event exists and nothing unblocks it, so the
    /// snapshot needs only the failure set, not a stored focus.
    pub failures: Vec<StateSnapshotFailure>,
    pub simplification_reviewed: bool,
    pub files_touched: u64,
    pub tool_calls: u64,
    pub failed_tools: u64,
    pub repeated_failures_per_target: u64,
    pub plan_step: Option<String>,
    pub validation: Option<String>,
    pub snapshot_files: Vec<String>,
}

/// Mirror of the query crate's `TaskDecision`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct StateSnapshotDecision {
    pub statement: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

/// Mirror of the query crate's `EvidenceItem`; `source` and `status` are the
/// `as_str()` spellings of its enums.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct StateSnapshotEvidence {
    pub summary: String,
    pub source: String,
    pub status: String,
}

/// Mirror of the query crate's `TaskFailure`; `source` is "tool" or
/// "validation" (used to restore the validation-failure next-action).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct StateSnapshotFailure {
    pub source: String,
    pub summary: String,
}

/// Metadata-only entry payload carrying one [`StateSnapshot`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateSnapshotEntry {
    pub session_id: String,
    /// ISO-8601 timestamp when the snapshot was written.
    pub timestamp: String,
    pub snapshot: StateSnapshot,
}

// ---------------------------------------------------------------------------
// Metadata-only entry types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryEntry {
    /// UUID of the leaf message that this summary replaces.
    pub leaf_uuid: String,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiTitleEntry {
    pub session_id: String,
    pub ai_title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomTitleEntry {
    pub session_id: String,
    pub custom_title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LastPromptEntry {
    pub session_id: String,
    pub last_prompt: String,
}

/// Written to mark a message UUID as deleted (soft-delete via append-only
/// tombstoning). The loader skips any entry whose uuid appears in a tombstone.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TombstoneEntry {
    /// UUID of the entry being deleted.
    pub deleted_uuid: String,
}

/// Points the session's active tip at a specific entry uuid (issue #234).
///
/// Appended, never rewritten, so that history is retained: pointing the leaf at
/// an *earlier* entry keeps every later entry on disk as a sibling branch that
/// can be returned to (by appending a newer leaf pointing back at it).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeafEntry {
    /// UUID (entry-level `uuid`) of the entry that is the current active tip.
    ///
    /// `None` (field absent) resets the active branch to empty — before any
    /// message — mirroring pi's nullable leaf pointer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leaf_uuid: Option<String>,
}

/// Marks a rewind point for state-event replay (abandoned-branch anchoring).
///
/// Appended by the CLI when the user rewinds (the conversation shrinks). The
/// transcript itself is append-only — abandoned-branch entries stay on disk —
/// but state events carry no uuid chain of their own, so without a cut marker
/// a rewind's abandoned-branch events would replay onto the rewound branch as
/// stale failures/evidence.
///
/// `active_message_count` is the length of the conversation after the rewind,
/// in the same in-memory push-order coordinate that [`StateEventEntry`]'s
/// `msg_index` uses. An event belongs to the active branch iff every cut after
/// its line has a count greater than the event's index (no later cut → keep).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateCutEntry {
    /// Number of conversation messages retained by the rewind that wrote this
    /// cut. Events with `msg_index >= count` are abandoned-branch events.
    pub active_message_count: u32,
}

// ---------------------------------------------------------------------------
// SessionSummary — lightweight metadata returned by list_sessions()
// ---------------------------------------------------------------------------

/// Lightweight metadata for one session, extracted from the last entry in
/// the transcript without loading the full file.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub session_id: String,
    pub path: PathBuf,
    pub mtime: std::time::SystemTime,
    /// The last-prompt text found in the tail, if any.
    pub last_prompt: Option<String>,
    /// The custom title found in the tail, if any.
    pub title: Option<String>,
    /// The AI-generated title found in the tail, if any (written by the
    /// auto-titler at session exit).
    pub ai_title: Option<String>,
    /// Approximate message count (user + assistant entries in the tail).
    pub message_count: usize,
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Returns the base projects directory: `~/.clawde/projects/`.
pub fn projects_dir() -> PathBuf {
    crate::config::Settings::config_dir().join("projects")
}

/// Returns the per-project transcript directory.
///
/// The project root path is encoded using **URL-safe base64 without padding**
/// to produce a stable, platform-safe directory name that is fully reversible
/// (unlike the TS `sanitizePath` which just replaces chars with hyphens).
pub fn transcript_dir(project_root: &Path) -> PathBuf {
    transcript_dir_in(&crate::config::Settings::config_dir(), project_root)
}

/// Like [`transcript_dir`] but rooted at an explicit config directory instead
/// of the detected `~/.clawde`. Lets tests stage transcripts in a tempdir
/// without writing under HOME (unwritable in sandboxed builds).
pub fn transcript_dir_in(config_dir: &Path, project_root: &Path) -> PathBuf {
    let encoded = URL_SAFE_NO_PAD.encode(project_root.to_string_lossy().as_bytes());
    config_dir.join("projects").join(encoded)
}

/// Migrate transcript buckets that were keyed on a raw cwd (a subdirectory of
/// a git repo) into the git-root bucket, so `/stats` and the transcript writer
/// agree on the project identifier.
///
/// Before the path-consistency fix, transcripts were written under
/// `projects/<base64(git_root)>/` (via `get_repo_root`) while `/stats` looked
/// under `projects/<base64(cwd)>/`. Sessions launched from a subdirectory were
/// stored in a bucket that nothing read. This scans every encoded bucket,
/// decodes the path, and if it is a subdirectory of a git repo, moves the
/// `.jsonl` files into the git-root bucket (never overwriting an existing
/// file). Non-repo buckets and buckets already at the git root are left alone.
///
/// Idempotent and safe to run at every startup: after the first pass the
/// subdirectory buckets no longer exist, so later runs are no-ops. Returns the
/// number of files moved.
pub fn migrate_cwd_transcript_buckets(config_dir: &Path) -> usize {
    let projects = config_dir.join("projects");
    let mut moved = 0usize;

    let entries = match std::fs::read_dir(&projects) {
        Ok(entries) => entries,
        Err(_) => return 0, // no projects dir yet
    };

    for entry in entries.flatten() {
        let old_bucket = entry.path();
        if !old_bucket.is_dir() {
            continue;
        }
        let encoded = match old_bucket.file_name().and_then(|s| s.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };
        // Skip buckets whose names are not reversible base64 (corrupt or
        // foreign entries) — they are already unreadable by the rest of the
        // system and must not be touched.
        let decoded = match URL_SAFE_NO_PAD.decode(&encoded) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(_) => continue,
            },
            Err(_) => continue,
        };
        let old_path = PathBuf::from(&decoded);

        // Only migrate paths that are a strict subdirectory of a git repo.
        let Some(git_root) = crate::git_utils::get_repo_root(&old_path) else {
            continue;
        };
        if git_root == old_path {
            continue; // already at the git root
        }

        let new_bucket = transcript_dir_in(config_dir, &git_root);
        if std::fs::create_dir_all(&new_bucket).is_err() {
            continue;
        }

        let files: Vec<PathBuf> = match std::fs::read_dir(&old_bucket) {
            Ok(read) => read
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
                .collect(),
            Err(_) => Vec::new(),
        };
        for file in files {
            let dest = new_bucket.join(file.file_name().unwrap_or_default());
            if dest.exists() {
                continue; // never clobber an existing transcript
            }
            if std::fs::rename(&file, &dest).is_ok() {
                moved += 1;
            }
        }

        // Remove the old bucket when it is now empty.
        let empty = std::fs::read_dir(&old_bucket)
            .map(|mut r| r.next().is_none())
            .unwrap_or(false);
        if empty {
            let _ = std::fs::remove_dir(&old_bucket);
        }
    }

    moved
}

/// Returns the full path to a session's JSONL transcript file.
///
/// # Errors
/// Returns `crate::ClaudeError::Other` if `session_id` contains path components
/// (`/`, `\`, or `..`) that could be used for directory traversal (issue #204).
pub fn transcript_path(project_root: &Path, session_id: &str) -> crate::Result<PathBuf> {
    if session_id.contains('/') || session_id.contains('\\') || session_id.contains("..") {
        return Err(crate::ClaudeError::Other(
            "session_id contains illegal characters".into(),
        ));
    }
    Ok(transcript_dir(project_root).join(format!("{}.jsonl", session_id)))
}

// ---------------------------------------------------------------------------
// Core I/O operations
// ---------------------------------------------------------------------------

/// Append a single entry to a JSONL transcript file.
///
/// * Creates parent directories if they do not exist.
/// * Is a no-op (returns `Ok(())`) when the file already exceeds
///   [`MAX_TRANSCRIPT_BYTES`] to avoid unbounded growth.
/// * Uses `OpenOptions::append(true)` which results in an atomic positional
///   write on POSIX (O_APPEND) and a best-effort append on Windows.
pub async fn write_transcript_entry(path: &Path, entry: &TranscriptEntry) -> crate::Result<()> {
    // Guard: do not grow files beyond the cap.
    if let Ok(meta) = tokio::fs::metadata(path).await {
        if meta.len() >= MAX_TRANSCRIPT_BYTES {
            return Ok(());
        }
    }

    // Serialise to a single compact JSON line terminated by '\n'.
    let mut line = serde_json::to_string(entry)?;
    line.push('\n');

    // Ensure parent directory exists before attempting the write.
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
        crate::accounts::set_user_only_dir_perms(parent);
    }

    // Open in append mode; create if absent.
    use tokio::io::AsyncWriteExt;
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;

    file.write_all(line.as_bytes()).await?;
    // flush() is REQUIRED here, not optional hygiene: tokio's `fs::File`
    // returns Ok from `write_all` once the bytes are queued to a blocking
    // task, and dropping the file detaches that task (tokio fs/file.rs has
    // no Drop impl; its docs require `flush` before drop). Without this,
    // appends are fire-and-forget and lose ~1-2% of lines under load —
    // observed as `Ok`-returned writes whose line never reached the file.
    // flush() completes the pending write before returning.
    file.flush().await?;
    // Transcripts may contain secrets read into context; keep them
    // owner-only (issue #212).
    crate::accounts::set_user_only_perms(path);
    Ok(())
}

/// Load all non-tombstoned entries from a JSONL transcript file.
///
/// * Returns an empty `Vec` if the file does not exist.
/// * Bails out with an error if the file exceeds [`MAX_TRANSCRIPT_BYTES`]
///   to protect against OOM.
/// * Lines that fail to parse are silently skipped (forward-compatibility).
/// * Any entry whose uuid appears in a `Tombstone` entry is excluded.
pub async fn load_transcript(path: &Path) -> crate::Result<Vec<TranscriptEntry>> {
    // Fast-path: file absent → empty session.
    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.len() > MAX_TRANSCRIPT_BYTES => {
            return Err(crate::ClaudeError::Other(format!(
                "Transcript file too large to load ({} bytes, max {})",
                meta.len(),
                MAX_TRANSCRIPT_BYTES,
            )));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(vec![]);
        }
        Err(e) => return Err(e.into()),
        Ok(_) => {}
    }

    let raw = tokio::fs::read_to_string(path).await?;

    // First pass: collect tombstoned UUIDs.
    let mut tombstoned: std::collections::HashSet<String> = std::collections::HashSet::new();

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Cheap structural check before full parse.
        if trimmed.contains("\"type\":\"tombstone\"") || trimmed.contains("\"type\": \"tombstone\"")
        {
            if let Ok(TranscriptEntry::Tombstone(t)) =
                serde_json::from_str::<TranscriptEntry>(trimmed)
            {
                tombstoned.insert(t.deleted_uuid);
            }
        }
    }

    // Second pass: collect valid non-tombstoned entries.
    let mut entries = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let entry: TranscriptEntry = match serde_json::from_str(trimmed) {
            Ok(e) => e,
            Err(_) => continue, // skip malformed lines
        };

        // Skip tombstones themselves and tombstoned entries.
        match &entry {
            TranscriptEntry::Tombstone(_) => continue,
            TranscriptEntry::Unknown => continue,
            _ => {}
        }

        if let Some(uuid) = entry.uuid() {
            if tombstoned.contains(uuid) {
                continue;
            }
        }

        entries.push(entry);
    }

    Ok(entries)
}

/// List all `.jsonl` session files under the project's transcript directory,
/// sorted by modification time (newest first).
///
/// For each file, a cheap tail-read extracts the `last-prompt` and
/// `custom-title` metadata without loading the full transcript.
pub async fn list_sessions(project_root: &Path) -> crate::Result<Vec<SessionSummary>> {
    list_sessions_in(&crate::config::Settings::config_dir(), project_root).await
}

/// Like [`list_sessions`] but rooted at an explicit config directory. See
/// [`transcript_dir_in`].
pub async fn list_sessions_in(
    config_dir: &Path,
    project_root: &Path,
) -> crate::Result<Vec<SessionSummary>> {
    let dir = transcript_dir_in(config_dir, project_root);

    let mut dir_entries = match tokio::fs::read_dir(&dir).await {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(vec![]);
        }
        Err(e) => return Err(e.into()),
    };

    let mut sessions: Vec<SessionSummary> = Vec::new();

    while let Ok(Some(entry)) = dir_entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }

        // Extract session ID from the stem.
        let session_id = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };

        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);

        // Read the tail of the file (up to 64 KB) to extract metadata.
        let (last_prompt, title, ai_title, message_count) = read_session_tail_metadata(&path).await;

        sessions.push(SessionSummary {
            session_id,
            path,
            mtime,
            last_prompt,
            title,
            ai_title,
            message_count,
        });
    }

    // Sort newest-first.
    sessions.sort_by_key(|b| std::cmp::Reverse(b.mtime));
    Ok(sessions)
}

/// Append a `Tombstone` entry that marks `uuid` as deleted.
///
/// This is the append-only soft-delete path.  On resume,
/// [`load_transcript`] will skip the tombstoned entry.
///
/// If the file exceeds [`MAX_TRANSCRIPT_BYTES`] the tombstone is not written
/// (same guard as [`write_transcript_entry`]).
pub async fn tombstone_entry(path: &Path, uuid: &str) -> crate::Result<()> {
    let entry = TranscriptEntry::Tombstone(TombstoneEntry {
        deleted_uuid: uuid.to_string(),
    });
    write_transcript_entry(path, &entry).await
}

/// Truncate a session transcript at the entry whose `uuid` matches `from_uuid`,
/// removing that entry and all subsequent entries.
///
/// Used by `/revert` to discard assistant turns after a given message.
/// Rewrites the file atomically (load → filter → overwrite).
pub async fn truncate_after(path: &Path, from_uuid: &str) -> crate::Result<()> {
    let entries = load_transcript(path).await?;
    let mut keep = Vec::new();
    let mut found = false;
    for entry in entries {
        if found {
            continue;
        }
        match &entry {
            TranscriptEntry::User(m) | TranscriptEntry::Assistant(m)
                if m.message.uuid.as_deref() == Some(from_uuid) =>
            {
                found = true;
                continue; // drop this entry and everything after
            }
            _ => {}
        }
        keep.push(entry);
    }
    // Rewrite the file with only the kept entries.
    let mut lines = String::new();
    for e in &keep {
        lines.push_str(&serde_json::to_string(e).map_err(crate::error::ClaudeError::from)?);
        lines.push('\n');
    }
    tokio::fs::write(path, lines).await?;
    // Preserve owner-only perms across the full rewrite (issue #212).
    crate::accounts::set_user_only_perms(path);
    Ok(())
}

/// Append a `leaf` entry pointing the active tip at `leaf_uuid` (or reset the
/// active branch to empty when `leaf_uuid` is `None`).
///
/// This is append-only and therefore NON-destructive: later entries stay on
/// disk as a sibling branch. On the next load, [`active_branch_messages`]
/// follows this pointer to reconstruct the active conversation. This is the
/// storage primitive behind non-destructive revert/fork (#234).
pub async fn set_leaf(path: &Path, leaf_uuid: Option<&str>) -> crate::Result<()> {
    let entry = TranscriptEntry::Leaf(LeafEntry {
        leaf_uuid: leaf_uuid.map(|s| s.to_string()),
    });
    write_transcript_entry(path, &entry).await
}

/// Append a `last-prompt` metadata entry recording the most recent user prompt.
///
/// Written at session exit so `list_sessions` can show a preview without
/// loading the full transcript. Later writes supersede earlier ones (tail reads
/// take the last occurrence).
pub async fn write_last_prompt(
    path: &Path,
    session_id: &str,
    last_prompt: &str,
) -> crate::Result<()> {
    let entry = TranscriptEntry::LastPrompt(LastPromptEntry {
        session_id: session_id.to_string(),
        last_prompt: last_prompt.to_string(),
    });
    write_transcript_entry(path, &entry).await
}

/// Append an `ai-title` metadata entry with the auto-generated session name.
///
/// Written by the session-exit auto-titler. The title survives as a first-class
/// transcript entry so it is visible to the recent-sessions UI, `/stats`, and
/// any future consumers of the project transcript.
pub async fn write_ai_title(path: &Path, session_id: &str, ai_title: &str) -> crate::Result<()> {
    let entry = TranscriptEntry::AiTitle(AiTitleEntry {
        session_id: session_id.to_string(),
        ai_title: ai_title.to_string(),
    });
    write_transcript_entry(path, &entry).await
}

/// Non-destructive counterpart to [`truncate_after`].
///
/// Finds the entry whose *message* uuid matches `target_message_uuid` — the
/// same key [`truncate_after`] uses — and points the active leaf at that
/// entry's parent, so the target turn and everything after it are retained on a
/// sibling branch instead of being deleted. On the next load,
/// [`active_branch_messages`] reconstructs the conversation ending just before
/// the target.
///
/// Returns `Ok(true)` if a leaf pointer was written, `Ok(false)` if the target
/// uuid was not found (a no-op, matching `truncate_after`'s not-found case).
///
/// Back-compat guard: if the target is *not* the first turn yet has no
/// `parentUuid` (an unchained/legacy transcript where a leaf walk cannot
/// recover the retained prefix), this falls back to the destructive
/// [`truncate_after`] so behavior is never worse than before.
pub async fn branch_before(path: &Path, target_message_uuid: &str) -> crate::Result<bool> {
    let entries = load_transcript(path).await?;

    let mut first_participant: Option<&str> = None;
    let mut target: Option<&TranscriptMessage> = None;
    for e in &entries {
        let m = match e {
            TranscriptEntry::User(m) | TranscriptEntry::Assistant(m) => m,
            _ => continue,
        };
        let mid = m.message.uuid.as_deref();
        if first_participant.is_none() {
            first_participant = mid;
        }
        if mid == Some(target_message_uuid) {
            target = Some(m);
            break;
        }
    }

    let target = match target {
        Some(m) => m,
        None => return Ok(false),
    };

    let parent = target.parent_uuid.as_deref();
    let is_first = first_participant == Some(target_message_uuid);

    if parent.is_none() && !is_first {
        // Legacy transcript with no walkable parent chain: pointing the leaf at
        // "before the target" would drop the retained prefix on reconstruction.
        // Preserve exact legacy behavior instead.
        truncate_after(path, target_message_uuid).await?;
        return Ok(true);
    }

    set_leaf(path, parent).await?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Internal helper: read tail metadata without a full parse
// ---------------------------------------------------------------------------

/// Reads up to 64 KB from the end of `path` and extracts `last-prompt`,
/// `custom-title`, and `ai-title` values by scanning JSONL lines.
///
/// Returns `(last_prompt, custom_title, ai_title)`.  All three are `None` if
/// the relevant entries are absent or the file cannot be read.
async fn read_session_tail_metadata(
    path: &Path,
) -> (Option<String>, Option<String>, Option<String>, usize) {
    const TAIL_BUF: u64 = 65_536; // 64 KB

    let file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return (None, None, None, 0),
    };
    let meta = match file.metadata().await {
        Ok(m) => m,
        Err(_) => return (None, None, None, 0),
    };
    let file_size = meta.len();
    if file_size == 0 {
        return (None, None, None, 0);
    }

    // Seek to the start of the tail window.
    let offset = file_size.saturating_sub(TAIL_BUF);
    let mut buf = vec![0u8; (file_size - offset) as usize];

    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut file = file;
    if file.seek(std::io::SeekFrom::Start(offset)).await.is_err() {
        return (None, None, None, 0);
    }
    if file.read_exact(&mut buf).await.is_err() {
        return (None, None, None, 0);
    }

    // Scan lines in reverse order so we get the last occurrence of each field.
    let text = String::from_utf8_lossy(&buf);
    let mut last_prompt: Option<String> = None;
    let mut title: Option<String> = None;
    let mut ai_title: Option<String> = None;
    // Count user + assistant entries for an approximate message count.
    // We scan the full tail (up to 64 KB) to get a reasonable count; for
    // sessions that fit in the tail this is exact.
    let mut message_count: usize = 0;

    for line in text.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Count user and assistant entries for approximate message count.
        // These two cheap `contains` checks avoid serde for every line.
        if trimmed.contains("\"type\":\"user\"")
            || trimmed.contains("\"type\": \"user\"")
            || trimmed.contains("\"type\":\"assistant\"")
            || trimmed.contains("\"type\": \"assistant\"")
        {
            message_count += 1;
        }

        if last_prompt.is_none()
            && (trimmed.contains("\"type\":\"last-prompt\"")
                || trimmed.contains("\"type\": \"last-prompt\""))
        {
            if let Ok(TranscriptEntry::LastPrompt(lp)) =
                serde_json::from_str::<TranscriptEntry>(trimmed)
            {
                last_prompt = Some(lp.last_prompt);
            }
        }

        if title.is_none()
            && (trimmed.contains("\"type\":\"custom-title\"")
                || trimmed.contains("\"type\": \"custom-title\""))
        {
            if let Ok(TranscriptEntry::CustomTitle(ct)) =
                serde_json::from_str::<TranscriptEntry>(trimmed)
            {
                title = Some(ct.custom_title);
            }
        }

        if ai_title.is_none()
            && (trimmed.contains("\"type\":\"ai-title\"")
                || trimmed.contains("\"type\": \"ai-title\""))
        {
            if let Ok(TranscriptEntry::AiTitle(at)) =
                serde_json::from_str::<TranscriptEntry>(trimmed)
            {
                ai_title = Some(at.ai_title);
            }
        }

        if last_prompt.is_some() && title.is_some() && ai_title.is_some() {
            break;
        }
    }

    (last_prompt, title, ai_title, message_count)
}

// ---------------------------------------------------------------------------
// Convenience constructor helpers used by main.rs
// ---------------------------------------------------------------------------

/// Build a `TranscriptEntry::StateEvent` for the given session, without a
/// branch anchor (legacy shape; such events always replay).
pub fn make_state_event_entry(session_id: &str, event: StateEvent) -> TranscriptEntry {
    make_state_event_entry_at(session_id, event, None)
}

/// Build a `TranscriptEntry::StateEvent` for the given session.
///
/// `msg_index` is the loop-side message index anchoring this event to the
/// active branch (see [`StateEventEntry::msg_index`]); `None` is written for
/// emissions that cannot be attributed to a specific turn.
pub fn make_state_event_entry_at(
    session_id: &str,
    event: StateEvent,
    msg_index: Option<u32>,
) -> TranscriptEntry {
    TranscriptEntry::StateEvent(StateEventEntry {
        session_id: session_id.to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        event,
        msg_index,
    })
}

/// Build a `TranscriptEntry::StateSnapshot` for the given session.
pub fn make_state_snapshot_entry(session_id: &str, snapshot: StateSnapshot) -> TranscriptEntry {
    TranscriptEntry::StateSnapshot(StateSnapshotEntry {
        session_id: session_id.to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        snapshot,
    })
}

/// Build a `TranscriptEntry::StateCut` rewind marker.
pub fn make_state_cut_entry(active_message_count: u32) -> TranscriptEntry {
    TranscriptEntry::StateCut(StateCutEntry {
        active_message_count,
    })
}

/// Build a `TranscriptEntry::User` from a bare `Message`.
///
/// `parent_uuid` is the UUID of the preceding chain-participant entry.
pub fn make_user_entry(
    message: Message,
    uuid: &str,
    parent_uuid: Option<&str>,
    session_id: &str,
    cwd: &str,
) -> TranscriptEntry {
    TranscriptEntry::User(TranscriptMessage {
        uuid: Some(uuid.to_string()),
        parent_uuid: parent_uuid.map(|s| s.to_string()),
        timestamp: chrono::Utc::now().to_rfc3339(),
        session_id: session_id.to_string(),
        cwd: cwd.to_string(),
        message,
        is_sidechain: false,
        user_type: "external".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        git_branch: None,
        agent_role: None,
        managed_session_id: None,
        extra: Default::default(),
    })
}

/// Build a `TranscriptEntry::Assistant` from a bare `Message`.
pub fn make_assistant_entry(
    message: Message,
    uuid: &str,
    parent_uuid: Option<&str>,
    session_id: &str,
    cwd: &str,
) -> TranscriptEntry {
    TranscriptEntry::Assistant(TranscriptMessage {
        uuid: Some(uuid.to_string()),
        parent_uuid: parent_uuid.map(|s| s.to_string()),
        timestamp: chrono::Utc::now().to_rfc3339(),
        session_id: session_id.to_string(),
        cwd: cwd.to_string(),
        message,
        is_sidechain: false,
        user_type: "external".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        git_branch: None,
        agent_role: None,
        managed_session_id: None,
        extra: Default::default(),
    })
}

/// Reconstruct `Vec<Message>` from a loaded transcript, in conversation order.
///
/// Only `user` and `assistant` entries are returned; metadata entries
/// (summary, custom-title, etc.) are discarded.  The order matches the on-disk
/// parentUuid chain: messages are returned in the order they appear in the
/// file, which is append-order and therefore chronological for the main chain.
pub fn messages_from_transcript(entries: &[TranscriptEntry]) -> Vec<Message> {
    entries
        .iter()
        .filter_map(|e| match e {
            TranscriptEntry::User(m) | TranscriptEntry::Assistant(m) => Some(m.message.clone()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Session tree — active leaf / branch reconstruction (issue #234)
// ---------------------------------------------------------------------------

/// Return the most-recently-appended `leaf` entry in the transcript, if any.
///
/// The last leaf wins, so callers see the *current* active tip even after
/// several non-destructive reverts.
pub fn last_leaf(entries: &[TranscriptEntry]) -> Option<&LeafEntry> {
    entries.iter().rev().find_map(|e| match e {
        TranscriptEntry::Leaf(l) => Some(l),
        _ => None,
    })
}

/// Reconstruct the chain-participant entries (user/assistant) on the ACTIVE
/// branch, in root→leaf order.
///
/// * **No `leaf` entry** → chain participants in *file order*, identical to the
///   pre-#234 linear behavior. This is the back-compat guarantee: old sessions
///   load exactly as they did before, regardless of their `parentUuid` fields.
/// * **`leaf` present** → the active branch is reconstructed by walking
///   `parentUuid` links from the leaf back to the root, so entries on abandoned
///   sibling branches are excluded (they remain on disk).
/// * **reset leaf** (`leafUuid` absent/null) → empty branch.
/// * **dangling leaf** (points at a uuid not present, e.g. tombstoned) → safe
///   fallback to file order.
pub fn active_branch_entries(entries: &[TranscriptEntry]) -> Vec<&TranscriptEntry> {
    let leaf = match last_leaf(entries) {
        // Back-compat: no leaf pointer → linear file order.
        None => {
            return entries
                .iter()
                .filter(|e| e.is_chain_participant())
                .collect()
        }
        Some(l) => l,
    };

    // Reset leaf → empty active branch.
    let leaf_uuid = match leaf.leaf_uuid.as_deref() {
        None => return Vec::new(),
        Some(u) => u,
    };

    // Index every entry that carries an (entry-level) uuid so we can follow the
    // parent chain. Keep the first occurrence for any given uuid.
    let mut by_uuid: std::collections::HashMap<&str, &TranscriptEntry> =
        std::collections::HashMap::new();
    for e in entries {
        if let Some(u) = e.uuid() {
            by_uuid.entry(u).or_insert(e);
        }
    }

    if !by_uuid.contains_key(leaf_uuid) {
        // Dangling leaf → safe fallback to file order.
        return entries
            .iter()
            .filter(|e| e.is_chain_participant())
            .collect();
    }

    // Walk parentUuid links from the leaf back toward the root.
    let mut chain: Vec<&TranscriptEntry> = Vec::new();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut cursor = Some(leaf_uuid);
    while let Some(uuid) = cursor {
        if !seen.insert(uuid) {
            break; // cycle guard
        }
        let entry = match by_uuid.get(uuid) {
            Some(e) => *e,
            None => break, // broken chain — stop at the last reachable entry
        };
        chain.push(entry);
        cursor = match entry {
            TranscriptEntry::User(m)
            | TranscriptEntry::Assistant(m)
            | TranscriptEntry::Attachment(m)
            | TranscriptEntry::System(m) => m.parent_uuid.as_deref(),
            _ => None,
        };
    }

    chain.reverse();
    // Only user/assistant entries contribute to the conversation.
    chain.retain(|e| e.is_chain_participant());
    chain
}

/// Reconstruct `Vec<Message>` for the ACTIVE branch of a loaded transcript.
///
/// Leaf-aware counterpart of [`messages_from_transcript`]: with no `leaf` entry
/// it is identical (linear, file order); with a `leaf` entry it returns only
/// the messages on the active branch (root→leaf), so reverted-away turns held
/// on a sibling branch are excluded from the reconstructed conversation.
pub fn active_branch_messages(entries: &[TranscriptEntry]) -> Vec<Message> {
    active_branch_entries(entries)
        .into_iter()
        .filter_map(|e| match e {
            TranscriptEntry::User(m) | TranscriptEntry::Assistant(m) => Some(m.message.clone()),
            _ => None,
        })
        .collect()
}

/// Extract the state events belonging to the ACTIVE branch of a transcript.
///
/// State events carry no uuid/parent chain of their own — they are observed
/// facts about the run, interleaved with the message entries in file order.
/// Branch membership is anchored POSITIONALLY: every event written by the
/// query loop carries a `msg_index` (its assistant turn's index in the
/// in-memory conversation the loop was building — the exact vec a rewind
/// truncates), and a rewind appends a [`StateCutEntry`] carrying the retained
/// message count. An event belongs to the active branch iff the FIRST cut at
/// or after its line has a count greater than the event's index; an event
/// with no later cut is unconditionally on the active branch. Events without
/// an index (legacy sessions, pre-turn emissions) are always kept.
///
/// This replaces the earlier timestamp-cutoff approximation (leaf message
/// timestamp vs event timestamp), which retained abandoned-branch events
/// written after the rewind point. The `leaf` entry continues to govern
/// MESSAGE branch reconstruction in [`active_branch_messages`]; only its
/// reset form (empty active branch) also clears state events.
///
/// The returned events are in file (chronological) order, ready to feed the
/// reducer in [`TaskState::replay`]-style consumption on the query side.
pub fn state_events_from_transcript(entries: &[TranscriptEntry]) -> Vec<&StateEvent> {
    // Reset leaf → the active branch is empty, so no events either.
    if let Some(l) = last_leaf(entries) {
        if l.leaf_uuid.is_none() {
            return Vec::new();
        }
    }

    // Single ordered pass with a pending buffer: an event is final once the
    // first cut after its line has been seen (kept) or dropped by it; events
    // still pending at EOF have no later cut and are kept unconditionally.
    let mut finalized: Vec<&StateEvent> = Vec::new();
    let mut pending: Vec<(Option<u32>, &StateEvent)> = Vec::new();
    for entry in entries {
        match entry {
            TranscriptEntry::StateCut(cut) => {
                for (idx, event) in pending.drain(..) {
                    if idx.is_none_or(|i| i < cut.active_message_count) {
                        finalized.push(event);
                    }
                }
            }
            TranscriptEntry::StateEvent(state) => {
                pending.push((state.msg_index, &state.event));
            }
            _ => {}
        }
    }
    finalized.extend(pending.into_iter().map(|(_, event)| event));
    finalized
}

/// Streaming loader: extract owned state events from a transcript file without
/// parsing the full conversation.
///
/// The session-owning query loop calls this once per prompt, so the cost must
/// track the number of state events (and cut/leaf markers), not the transcript
/// size. Lines are cheaply pre-filtered by substring before deserialization;
/// unknown/malformed lines are skipped exactly like `load_transcript`. The
/// branch-anchoring semantics are identical to [`state_events_from_transcript`]:
/// events carry a positional `msg_index`, a rewind appends a `state-cut`
/// marker with the retained count, and the first cut at or after an event's
/// line decides its membership (no later cut → keep; no index → keep). A
/// reset leaf (empty active branch) yields no events.
///
/// Returns `Ok(empty)` for a missing file so callers can treat absence as a
/// session with no persisted events.
pub async fn load_state_events_from_file(path: &Path) -> crate::Result<Vec<StateEvent>> {
    use tokio::io::AsyncBufReadExt;

    // Fast-path: file absent → no events. Oversized files are read via the
    // buffered reader below which stops once the cap is exceeded.
    let file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut reader = tokio::io::BufReader::new(file);

    // Single ordered pass with the same pending buffer as the entry-based
    // extractor: events finalize when the first cut after their line arrives;
    // events still pending at EOF have no later cut and are kept.
    let mut finalized: Vec<StateEvent> = Vec::new();
    let mut pending: Vec<(Option<u32>, StateEvent)> = Vec::new();
    let mut reset_leaf = false;
    let mut bytes_read: u64 = 0;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        bytes_read += n as u64;
        if bytes_read > MAX_TRANSCRIPT_BYTES {
            return Err(crate::ClaudeError::Other(
                "Transcript file too large to load state events (over 50MB cap)".into(),
            ));
        }
        let trimmed = line.trim();
        if trimmed.contains("\"type\":\"leaf\"") || trimmed.contains("\"type\": \"leaf\"") {
            if let Ok(TranscriptEntry::Leaf(leaf)) =
                serde_json::from_str::<TranscriptEntry>(trimmed)
            {
                reset_leaf = leaf.leaf_uuid.is_none();
            }
            continue;
        }
        if trimmed.contains("state-cut") {
            if let Ok(TranscriptEntry::StateCut(cut)) =
                serde_json::from_str::<TranscriptEntry>(trimmed)
            {
                for (idx, event) in pending.drain(..) {
                    if idx.is_none_or(|i| i < cut.active_message_count) {
                        finalized.push(event);
                    }
                }
            }
            continue;
        }
        if !trimmed.contains("state-event") {
            continue;
        }
        let Ok(TranscriptEntry::StateEvent(state)) =
            serde_json::from_str::<TranscriptEntry>(trimmed)
        else {
            continue;
        };
        pending.push((state.msg_index, state.event));
    }
    if reset_leaf {
        // Reset leaf → empty active branch, so no events either.
        return Ok(Vec::new());
    }
    finalized.extend(pending.into_iter().map(|(_, event)| event));
    Ok(finalized)
}

/// Load the newest valid state snapshot plus the events written after it.
///
/// Incremental-replay read path: when a session crossed the snapshot cadence,
/// the snapshot already holds the fold of the kept events `0..event_count`
/// and only the kept events after its line need replaying. Returns `Ok(None)`
/// — and the caller falls back to the full event load — when any of the
/// following hold:
///
/// * no snapshot exists (session never crossed the cadence, or predates the
///   feature),
/// * a `leaf` entry exists (reset/branch message semantics — the branch-aware
///   full load owns those sessions),
/// * the newest snapshot carries a stale [`STATE_SNAPSHOT_SCHEMA_VERSION`],
/// * the snapshot's `event_count` watermark does not equal the number of
///   KEPT events written before it — raw lines minus those a `state-cut`
///   dropped (a rewritten transcript, or a snapshot claiming to fold events
///   the cut excludes, invalidates the cache),
/// * the file is absent (empty session) or unreadable.
///
/// Cut-awareness: a rewind appends a `state-cut` marker; events it excludes
/// no longer count toward the watermark and are never returned in the tail.
/// A snapshot written BEFORE a rewind stays valid — the emitter counts kept
/// events (its counter is re-seeded per prompt from the cut-filtered load),
/// so the body folded exactly the kept prefix and only the tail shrinks.
/// A snapshot whose watermark counts events the cut drops (impossible for a
/// correctly written file — the emitter never folds dropped events) fails the
/// watermark check and falls back, honoring delete-on-doubt.
///
/// A single forward pass: lines are substring-pre-filtered so only
/// `state-event` / `state-snapshot` / `state-cut` / `leaf` lines are
/// deserialized. Unknown or malformed lines are skipped exactly like
/// `load_transcript`. When multiple valid snapshots exist (one per cadence
/// crossing), the LAST one wins and only kept events after it are returned.
pub async fn load_state_snapshot(
    path: &Path,
) -> crate::Result<Option<(StateSnapshot, Vec<StateEvent>)>> {
    use tokio::io::AsyncBufReadExt;

    let file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut reader = tokio::io::BufReader::new(file);

    let mut line = String::new();
    let mut bytes_read: u64 = 0;
    let mut saw_leaf = false;
    // Raw `state-event` lines seen so far.
    let mut events_seen: u64 = 0;
    // Events resolved as dropped by the first cut after their line.
    let mut dropped: u64 = 0;
    // Anchors of events awaiting their first cut (keptness undecided).
    let mut pending_indices: Vec<Option<u32>> = Vec::new();
    // Events after the newest accepted snapshot, awaiting their first cut.
    let mut pending_tail: Vec<(Option<u32>, StateEvent)> = Vec::new();
    let mut last_snapshot: Option<StateSnapshot> = None;
    let mut tail_events: Vec<StateEvent> = Vec::new();
    // Count of pending events folded into the currently accepted snapshot
    // (those before its line whose first cut had not yet been seen). Used to
    // detect a later cut invalidating the snapshot's own fold.
    let mut pending_snapshot_boundary: Option<usize> = None;
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        bytes_read += n as u64;
        if bytes_read > MAX_TRANSCRIPT_BYTES {
            return Err(crate::ClaudeError::Other(
                "Transcript file too large to load state snapshot (over 50MB cap)".into(),
            ));
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.contains("\"type\":\"leaf\"") || trimmed.contains("\"type\": \"leaf\"") {
            saw_leaf = true;
            continue;
        }
        if trimmed.contains("state-cut") {
            if let Ok(TranscriptEntry::StateCut(cut)) =
                serde_json::from_str::<TranscriptEntry>(trimmed)
            {
                // Resolve every pending event's membership against this cut:
                // the first cut after an event's line is authoritative.
                let snapshot_boundary = pending_snapshot_boundary.take();
                for (position, idx) in pending_indices.drain(..).enumerate() {
                    if idx.is_none_or(|i| i < cut.active_message_count) {
                        continue; // kept
                    }
                    dropped += 1;
                    // A cut that drops an event the accepted snapshot folded
                    // (pending when the snapshot line was read) poisons that
                    // snapshot: its body contains abandoned-branch facts.
                    if snapshot_boundary.is_some_and(|boundary| position < boundary) {
                        last_snapshot = None;
                        tail_events.clear();
                        pending_tail.clear();
                    }
                }
                for (idx, event) in pending_tail.drain(..) {
                    if idx.is_none_or(|i| i < cut.active_message_count) {
                        tail_events.push(event);
                    }
                }
            }
            continue;
        }
        if trimmed.contains("state-snapshot") {
            // A snapshot line: keep it only when it is schema-current and its
            // watermark matches the KEPT events actually written before it.
            if let Ok(TranscriptEntry::StateSnapshot(entry)) =
                serde_json::from_str::<TranscriptEntry>(trimmed)
            {
                if entry.snapshot.schema_version == STATE_SNAPSHOT_SCHEMA_VERSION
                    && entry.snapshot.event_count == events_seen - dropped
                {
                    last_snapshot = Some(entry.snapshot);
                    tail_events.clear();
                    pending_tail.clear();
                    // Events still pending at this line belong to the fold.
                    pending_snapshot_boundary = Some(pending_indices.len());
                }
            }
            continue;
        }
        if trimmed.contains("state-event") {
            if let Ok(TranscriptEntry::StateEvent(entry)) =
                serde_json::from_str::<TranscriptEntry>(trimmed)
            {
                // Membership is not decidable until the first cut after this
                // line (or EOF, where everything pending is kept).
                pending_indices.push(entry.msg_index);
                if last_snapshot.is_some() {
                    pending_tail.push((entry.msg_index, entry.event));
                }
            }
            events_seen += 1;
        }
    }
    if saw_leaf {
        return Ok(None);
    }
    // EOF with no later cut: every pending event is on the active branch.
    // Only events pending after the newest snapshot contribute to the tail.
    tail_events.extend(pending_tail.into_iter().map(|(_, event)| event));
    Ok(last_snapshot.map(|snapshot| (snapshot, tail_events)))
}

/// Filter transcript entries by agent role ("manager" or "executor").
///
/// Returns only User and Assistant entries whose `agent_role` matches `role`.
#[allow(dead_code)]
pub fn filter_by_agent_role<'a>(
    entries: &'a [TranscriptEntry],
    role: &str,
) -> Vec<&'a TranscriptEntry> {
    entries
        .iter()
        .filter(|e| match e {
            TranscriptEntry::User(msg) | TranscriptEntry::Assistant(msg) => {
                msg.agent_role.as_deref() == Some(role)
            }
            _ => false,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Message, MessageContent, Role};
    use tempfile::tempdir;

    /// Regression test for silent append loss in `write_transcript_entry`:
    /// tokio's `fs::File` returns Ok from `write_all` once bytes are queued
    /// to a blocking task, and dropping the file detaches that task. Before
    /// the `flush()` fix, ~1-2% of appends under load were acknowledged but
    /// never reached the file (observed: 299/300 lines with zero errors).
    /// 500 sequential appends must land 500 lines, every run.
    #[tokio::test]
    async fn write_transcript_entry_never_silently_drops_appends() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("append-reliability.jsonl");
        for i in 0..500 {
            let entry = make_state_event_entry(
                "sess-reliability",
                StateEvent::ValidationRecorded {
                    verdict: StateValidationVerdict::Passed,
                    headline: format!("append-{i}"),
                },
            );
            write_transcript_entry(&path, &entry).await.unwrap();
        }
        let lines = std::fs::read_to_string(&path).unwrap().lines().count();
        assert_eq!(lines, 500, "every acknowledged append must be durable");
    }

    fn make_msg(role: Role) -> Message {
        Message {
            role,
            content: MessageContent::Text("hello".to_string()),
            uuid: Some(uuid::Uuid::new_v4().to_string()),
            cost: None,
            snapshot_patch: None,
            turn_meta: None,
        }
    }

    #[tokio::test]
    async fn round_trip_user_message() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.jsonl");

        let msg = make_msg(Role::User);
        let uuid = uuid::Uuid::new_v4().to_string();
        let entry = make_user_entry(msg.clone(), &uuid, None, "sess-1", "/home/user/proj");
        write_transcript_entry(&path, &entry).await.unwrap();

        let loaded = load_transcript(&path).await.unwrap();
        assert_eq!(loaded.len(), 1);
        if let TranscriptEntry::User(m) = &loaded[0] {
            assert_eq!(m.uuid.as_deref(), Some(uuid.as_str()));
        } else {
            panic!("expected User entry");
        }
    }

    #[tokio::test]
    async fn state_event_round_trips_through_transcript() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state-events.jsonl");

        let events = vec![
            StateEvent::ValidationRecorded {
                verdict: StateValidationVerdict::Passed,
                headline: "All checks passed".to_string(),
            },
            StateEvent::ToolObserved {
                failed: true,
                summary: "connection refused".to_string(),
                file_paths: vec!["src/auth.rs".to_string()],
                mutating: false,
            },
            StateEvent::SnapshotObserved {
                files: vec!["src/auth.rs".to_string()],
            },
            StateEvent::DecisionRecorded {
                statement: "Keep state out of the transcript".to_string(),
                evidence: Some("compaction architecture".to_string()),
            },
            StateEvent::FocusChanged {
                focus: "blocked".to_string(),
                reason: "tests failed".to_string(),
            },
            StateEvent::PlanStepSet {
                step: "Step 2: patch auth".to_string(),
            },
            StateEvent::SimplificationReviewed,
        ];
        for event in &events {
            let entry = make_state_event_entry("sess-events", event.clone());
            write_transcript_entry(&path, &entry).await.unwrap();
        }

        let loaded = load_transcript(&path).await.unwrap();
        assert_eq!(loaded.len(), events.len());
        for (entry, expected) in loaded.iter().zip(events.iter()) {
            assert_eq!(entry.state_event(), Some(expected));
        }
    }

    #[tokio::test]
    async fn state_events_survive_alongside_messages_and_tombstones() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mixed.jsonl");

        let msg_uuid = uuid::Uuid::new_v4().to_string();
        let msg = make_msg(Role::User);
        write_transcript_entry(
            &path,
            &make_user_entry(msg, &msg_uuid, None, "sess-mixed", "/proj"),
        )
        .await
        .unwrap();
        write_transcript_entry(
            &path,
            &make_state_event_entry(
                "sess-mixed",
                StateEvent::ValidationRecorded {
                    verdict: StateValidationVerdict::Passed,
                    headline: "ok".to_string(),
                },
            ),
        )
        .await
        .unwrap();

        let loaded = load_transcript(&path).await.unwrap();
        assert_eq!(loaded.len(), 2, "message + state event both survive");
        assert!(loaded[0].state_event().is_none());
        assert!(loaded[1].state_event().is_some());
    }

    #[tokio::test]
    async fn state_events_excluded_from_active_branch_messages() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("branch.jsonl");

        let msg_uuid = uuid::Uuid::new_v4().to_string();
        write_transcript_entry(
            &path,
            &make_user_entry(make_msg(Role::User), &msg_uuid, None, "sess-b", "/proj"),
        )
        .await
        .unwrap();
        write_transcript_entry(
            &path,
            &make_state_event_entry("sess-b", StateEvent::SnapshotObserved { files: vec![] }),
        )
        .await
        .unwrap();

        let entries = load_transcript(&path).await.unwrap();
        let messages = active_branch_messages(&entries);
        assert_eq!(
            messages.len(),
            1,
            "state events never join the message chain"
        );
        assert_eq!(
            entries.len(),
            2,
            "but remain in the raw entry list for replay"
        );
    }

    #[test]
    fn state_event_wire_format_is_stable_snake_case() {
        let entry = make_state_event_entry(
            "sess-wire",
            StateEvent::ValidationRecorded {
                verdict: StateValidationVerdict::Passed,
                headline: "ok".to_string(),
            },
        );
        let line = serde_json::to_string(&entry).unwrap();
        assert!(
            line.contains("\"type\":\"state-event\""),
            "entry tag must be state-event: {line}"
        );
        assert!(
            line.contains("\"kind\":\"validation_recorded\""),
            "event tag must be snake_case: {line}"
        );
        assert!(line.contains("\"verdict\":\"passed\""));
    }

    #[tokio::test]
    async fn state_events_from_transcript_no_leaf_returns_all() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("linear.jsonl");
        write_transcript_entry(
            &path,
            &make_state_event_entry("sess-linear", StateEvent::SimplificationReviewed),
        )
        .await
        .unwrap();
        write_transcript_entry(
            &path,
            &make_state_event_entry(
                "sess-linear",
                StateEvent::SnapshotObserved {
                    files: vec!["a.rs".to_string()],
                },
            ),
        )
        .await
        .unwrap();
        let entries = load_transcript(&path).await.unwrap();
        assert_eq!(state_events_from_transcript(&entries).len(), 2);
    }

    #[tokio::test]
    async fn state_events_from_transcript_reset_leaf_yields_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("reset.jsonl");
        write_transcript_entry(
            &path,
            &make_state_event_entry("sess-reset", StateEvent::SnapshotObserved { files: vec![] }),
        )
        .await
        .unwrap();
        set_leaf(&path, None).await.unwrap();
        let entries = load_transcript(&path).await.unwrap();
        assert!(state_events_from_transcript(&entries).is_empty());
    }

    #[tokio::test]
    async fn load_state_events_from_file_missing_file_is_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does-not-exist.jsonl");
        let events = load_state_events_from_file(&path).await.unwrap();
        assert!(events.is_empty(), "absent file must read as no events");
    }

    #[tokio::test]
    async fn load_state_events_from_file_linear_returns_all_in_order() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("linear.jsonl");
        let expected = vec![
            StateEvent::SimplificationReviewed,
            StateEvent::SnapshotObserved {
                files: vec!["a.rs".to_string()],
            },
        ];
        for event in &expected {
            write_transcript_entry(&path, &make_state_event_entry("sess-lin", event.clone()))
                .await
                .unwrap();
        }
        // Interleave a message to prove the loader skips non-event lines.
        let msg_uuid = uuid::Uuid::new_v4().to_string();
        write_transcript_entry(
            &path,
            &make_user_entry(make_msg(Role::User), &msg_uuid, None, "sess-lin", "/proj"),
        )
        .await
        .unwrap();
        let events = load_state_events_from_file(&path).await.unwrap();
        assert_eq!(events, expected, "all events survive, in file order");
    }

    #[tokio::test]
    async fn load_state_events_from_file_matches_entry_extractor() {
        // Property: the streaming loader must agree with the entry-based
        // extractor on the same file, both for a linear transcript and for
        // one with a reset leaf.
        let dir = tempdir().unwrap();
        let linear_path = dir.path().join("linear.jsonl");
        for event in [
            StateEvent::SimplificationReviewed,
            StateEvent::SnapshotObserved {
                files: vec!["b.rs".to_string()],
            },
        ] {
            write_transcript_entry(&linear_path, &make_state_event_entry("sess-prop", event))
                .await
                .unwrap();
        }
        let entries = load_transcript(&linear_path).await.unwrap();
        let from_entries: Vec<StateEvent> = state_events_from_transcript(&entries)
            .into_iter()
            .cloned()
            .collect();
        let from_file = load_state_events_from_file(&linear_path).await.unwrap();
        assert_eq!(from_file, from_entries, "linear: loaders agree");

        let reset_path = dir.path().join("reset.jsonl");
        write_transcript_entry(
            &reset_path,
            &make_state_event_entry("sess-prop", StateEvent::SimplificationReviewed),
        )
        .await
        .unwrap();
        set_leaf(&reset_path, None).await.unwrap();
        let entries = load_transcript(&reset_path).await.unwrap();
        let from_entries: Vec<StateEvent> = state_events_from_transcript(&entries)
            .into_iter()
            .cloned()
            .collect();
        let from_file = load_state_events_from_file(&reset_path).await.unwrap();
        assert!(from_entries.is_empty(), "reset: entry extractor empty");
        assert_eq!(from_file, from_entries, "reset: loaders agree");
    }

    fn tagged_event(tag: &str) -> StateEvent {
        StateEvent::SnapshotObserved {
            files: vec![tag.to_string()],
        }
    }

    #[tokio::test]
    async fn state_cut_excludes_abandoned_branch_events() {
        // Rewind to message 2: the abandoned branch's event (idx 3) must not
        // replay; the pre-rewind in-branch event (idx 1) and the post-rewind
        // event (idx 5) must.
        let dir = tempdir().unwrap();
        let path = dir.path().join("cut.jsonl");
        write_transcript_entry(
            &path,
            &make_state_event_entry_at("sess-cut", tagged_event("pre"), Some(1)),
        )
        .await
        .unwrap();
        write_transcript_entry(
            &path,
            &make_state_event_entry_at("sess-cut", tagged_event("abandoned"), Some(3)),
        )
        .await
        .unwrap();
        write_transcript_entry(&path, &make_state_cut_entry(2))
            .await
            .unwrap();
        write_transcript_entry(
            &path,
            &make_state_event_entry_at("sess-cut", tagged_event("post"), Some(5)),
        )
        .await
        .unwrap();

        let entries = load_transcript(&path).await.unwrap();
        let kept: Vec<String> = state_events_from_transcript(&entries)
            .into_iter()
            .map(|e| match e {
                StateEvent::SnapshotObserved { files } => files[0].clone(),
                _ => panic!("unexpected variant"),
            })
            .collect();
        assert_eq!(kept, vec!["pre".to_string(), "post".to_string()]);

        let from_file = load_state_events_from_file(&path).await.unwrap();
        assert_eq!(
            from_file.len(),
            2,
            "streaming loader agrees with the entry extractor"
        );
    }

    #[tokio::test]
    async fn multiple_cuts_apply_in_order() {
        // Two rewinds: the first cut drops idx 5, the second (count 3) keeps
        // idx 1. An event after the last cut has no later cut and is kept.
        let dir = tempdir().unwrap();
        let path = dir.path().join("multi-cut.jsonl");
        write_transcript_entry(
            &path,
            &make_state_event_entry_at("sess-mc", tagged_event("a"), Some(5)),
        )
        .await
        .unwrap();
        write_transcript_entry(&path, &make_state_cut_entry(2))
            .await
            .unwrap();
        write_transcript_entry(
            &path,
            &make_state_event_entry_at("sess-mc", tagged_event("b"), Some(1)),
        )
        .await
        .unwrap();
        write_transcript_entry(&path, &make_state_cut_entry(3))
            .await
            .unwrap();
        write_transcript_entry(
            &path,
            &make_state_event_entry_at("sess-mc", tagged_event("c"), Some(7)),
        )
        .await
        .unwrap();

        let entries = load_transcript(&path).await.unwrap();
        let kept: Vec<String> = state_events_from_transcript(&entries)
            .into_iter()
            .map(|e| match e {
                StateEvent::SnapshotObserved { files } => files[0].clone(),
                _ => panic!("unexpected variant"),
            })
            .collect();
        assert_eq!(kept, vec!["b".to_string(), "c".to_string()]);
        let from_file = load_state_events_from_file(&path).await.unwrap();
        assert_eq!(from_file.len(), 2, "loaders agree under multiple cuts");
    }

    #[tokio::test]
    async fn legacy_unanchored_events_survive_cuts() {
        // Events written before msg_index existed carry no anchor; the safe
        // fallback is to keep them (never lose history to a missing field).
        let dir = tempdir().unwrap();
        let path = dir.path().join("legacy-cut.jsonl");
        write_transcript_entry(
            &path,
            &make_state_event_entry("sess-lg", tagged_event("old")),
        )
        .await
        .unwrap();
        write_transcript_entry(&path, &make_state_cut_entry(0))
            .await
            .unwrap();
        let entries = load_transcript(&path).await.unwrap();
        assert_eq!(
            state_events_from_transcript(&entries).len(),
            1,
            "unanchored events are always kept"
        );
    }

    #[tokio::test]
    async fn state_snapshot_loader_survives_cut_before_snapshot() {
        // A cut before the snapshot line: the dropped event does not count
        // toward the watermark, and the kept event does. The snapshot written
        // over the kept prefix stays valid; nothing follows it.
        let dir = tempdir().unwrap();
        let path = dir.path().join("cut-snap.jsonl");
        write_transcript_entry(
            &path,
            &make_state_event_entry_at("sess-cs", tagged_event("abandoned"), Some(3)),
        )
        .await
        .unwrap();
        write_transcript_entry(&path, &make_state_cut_entry(2))
            .await
            .unwrap();
        write_transcript_entry(
            &path,
            &make_state_event_entry_at("sess-cs", tagged_event("kept"), Some(1)),
        )
        .await
        .unwrap();
        write_transcript_entry(
            &path,
            &make_state_snapshot_entry(
                "sess-cs",
                StateSnapshot {
                    schema_version: STATE_SNAPSHOT_SCHEMA_VERSION,
                    event_count: 1,
                    body: make_snapshot_body(),
                },
            ),
        )
        .await
        .unwrap();
        let loaded = load_state_snapshot(&path)
            .await
            .unwrap()
            .expect("cut-aware loader accepts the consistent snapshot");
        assert_eq!(loaded.0.event_count, 1);
        assert!(loaded.1.is_empty(), "nothing after the snapshot");
    }

    #[tokio::test]
    async fn state_snapshot_loader_tail_excludes_cut_dropped_events() {
        // Snapshot first, then a rewind: events after the snapshot line that
        // the cut drops must not appear in the tail (the emitter never folds
        // them, so the watermark stays consistent).
        let dir = tempdir().unwrap();
        let path = dir.path().join("cut-tail.jsonl");
        write_transcript_entry(
            &path,
            &make_state_event_entry_at("sess-ct", tagged_event("kept"), Some(1)),
        )
        .await
        .unwrap();
        write_transcript_entry(
            &path,
            &make_state_snapshot_entry(
                "sess-ct",
                StateSnapshot {
                    schema_version: STATE_SNAPSHOT_SCHEMA_VERSION,
                    event_count: 1,
                    body: make_snapshot_body(),
                },
            ),
        )
        .await
        .unwrap();
        write_transcript_entry(
            &path,
            &make_state_event_entry_at("sess-ct", tagged_event("abandoned"), Some(3)),
        )
        .await
        .unwrap();
        write_transcript_entry(&path, &make_state_cut_entry(2))
            .await
            .unwrap();
        write_transcript_entry(
            &path,
            &make_state_event_entry_at("sess-ct", tagged_event("post"), Some(5)),
        )
        .await
        .unwrap();
        let loaded = load_state_snapshot(&path).await.unwrap().expect("snapshot");
        assert_eq!(loaded.1.len(), 1, "only the post-rewind event tails");
    }

    #[tokio::test]
    async fn state_snapshot_loader_poisoned_by_cut_dropping_folded_events() {
        // A cut that drops events the snapshot already folded (they were
        // pending when the snapshot line was read) invalidates it: the body
        // contains abandoned-branch facts. Delete-on-doubt wins.
        let dir = tempdir().unwrap();
        let path = dir.path().join("cut-poison.jsonl");
        write_transcript_entry(
            &path,
            &make_state_event_entry_at("sess-cp", tagged_event("a"), Some(1)),
        )
        .await
        .unwrap();
        write_transcript_entry(
            &path,
            &make_state_event_entry_at("sess-cp", tagged_event("b"), Some(3)),
        )
        .await
        .unwrap();
        write_transcript_entry(
            &path,
            &make_state_snapshot_entry(
                "sess-cp",
                StateSnapshot {
                    schema_version: STATE_SNAPSHOT_SCHEMA_VERSION,
                    event_count: 2,
                    body: make_snapshot_body(),
                },
            ),
        )
        .await
        .unwrap();
        // The rewind drops idx 3 — an event the snapshot folded.
        write_transcript_entry(&path, &make_state_cut_entry(2))
            .await
            .unwrap();
        assert!(
            load_state_snapshot(&path).await.unwrap().is_none(),
            "snapshot whose fold contains dropped events is discarded"
        );
    }

    #[tokio::test]
    async fn state_snapshot_loader_rejects_watermark_counting_dropped_events() {
        // A snapshot claiming to fold events the cut drops can only come
        // from a broken writer: the kept-count watermark check rejects it.
        let dir = tempdir().unwrap();
        let path = dir.path().join("cut-wm.jsonl");
        write_transcript_entry(
            &path,
            &make_state_event_entry_at("sess-cw", tagged_event("abandoned"), Some(3)),
        )
        .await
        .unwrap();
        write_transcript_entry(&path, &make_state_cut_entry(2))
            .await
            .unwrap();
        write_transcript_entry(
            &path,
            &make_state_snapshot_entry(
                "sess-cw",
                StateSnapshot {
                    schema_version: STATE_SNAPSHOT_SCHEMA_VERSION,
                    event_count: 1,
                    body: make_snapshot_body(),
                },
            ),
        )
        .await
        .unwrap();
        assert!(
            load_state_snapshot(&path).await.unwrap().is_none(),
            "watermark must count kept events, not raw lines"
        );
    }

    #[test]
    fn state_event_and_cut_wire_format_round_trip() {
        let anchored = make_state_event_entry_at("sess-w", tagged_event("f"), Some(7));
        let line = serde_json::to_string(&anchored).unwrap();
        assert!(line.contains("\"msgIndex\":7"), "anchored index: {line}");
        let unanchored = make_state_event_entry("sess-w", tagged_event("g"));
        let line = serde_json::to_string(&unanchored).unwrap();
        assert!(!line.contains("msgIndex"), "None omits the field: {line}");

        let cut = make_state_cut_entry(4);
        let line = serde_json::to_string(&cut).unwrap();
        assert!(line.contains("\"type\":\"state-cut\""), "{line}");
        assert!(line.contains("\"activeMessageCount\":4"), "{line}");
        let parsed: TranscriptEntry = serde_json::from_str(&line).unwrap();
        match parsed {
            TranscriptEntry::StateCut(c) => assert_eq!(c.active_message_count, 4),
            _ => panic!("cut round-trip failed"),
        }
    }

    fn make_snapshot_body() -> StateSnapshotBody {
        StateSnapshotBody {
            decisions: vec![StateSnapshotDecision {
                statement: "keep the API stable".to_string(),
                evidence: None,
            }],
            evidence: vec![StateSnapshotEvidence {
                summary: "3 checks passed".to_string(),
                source: "validation".to_string(),
                status: "verified".to_string(),
            }],
            changed_files: vec!["src/parser.rs".to_string()],
            failures: vec![StateSnapshotFailure {
                source: "tool".to_string(),
                summary: "connection refused".to_string(),
            }],
            simplification_reviewed: true,
            files_touched: 3,
            tool_calls: 12,
            failed_tools: 1,
            repeated_failures_per_target: 1,
            plan_step: Some("step 2".to_string()),
            validation: Some("3 checks passed".to_string()),
            snapshot_files: vec!["src/parser.rs".to_string()],
        }
    }

    fn make_snapshot(event_count: u64) -> StateSnapshot {
        StateSnapshot {
            schema_version: STATE_SNAPSHOT_SCHEMA_VERSION,
            event_count,
            body: make_snapshot_body(),
        }
    }

    #[tokio::test]
    async fn state_snapshot_round_trips_through_transcript() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("snap.jsonl");
        write_transcript_entry(
            &path,
            &make_state_event_entry("sess-snap", StateEvent::SimplificationReviewed),
        )
        .await
        .unwrap();
        write_transcript_entry(
            &path,
            &make_state_snapshot_entry("sess-snap", make_snapshot(1)),
        )
        .await
        .unwrap();

        let entries = load_transcript(&path).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].state_snapshot().is_none());
        let snapshot = entries[1].state_snapshot().expect("snapshot accessor");
        assert_eq!(snapshot.event_count, 1);
        assert_eq!(snapshot.body.tool_calls, 12);
        assert_eq!(snapshot.body.evidence[0].status, "verified");
        assert!(snapshot.body.simplification_reviewed);
    }

    #[test]
    fn state_snapshot_wire_format_is_stable() {
        let entry = make_state_snapshot_entry("sess-wire", make_snapshot(7));
        let line = serde_json::to_string(&entry).unwrap();
        assert!(
            line.contains("\"type\":\"state-snapshot\""),
            "entry tag: {line}"
        );
        assert!(
            line.contains("\"event_count\":7"),
            "watermark field: {line}"
        );
        assert!(
            line.contains(&format!(
                "\"schema_version\":{STATE_SNAPSHOT_SCHEMA_VERSION}"
            )),
            "schema version pinned: {line}"
        );
        assert!(line.contains("\"tool_calls\":12"));
        assert!(line.contains("\"plan_step\":\"step 2\""));
    }

    #[tokio::test]
    async fn load_state_snapshot_none_when_file_has_no_snapshot() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events-only.jsonl");
        write_transcript_entry(
            &path,
            &make_state_event_entry("sess-e", StateEvent::SimplificationReviewed),
        )
        .await
        .unwrap();
        let loaded = load_state_snapshot(&path).await.unwrap();
        assert!(loaded.is_none(), "no snapshot -> full replay path");
    }

    #[tokio::test]
    async fn load_state_snapshot_absent_file_is_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("absent.jsonl");
        let loaded = load_state_snapshot(&path).await.unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn load_state_snapshot_returns_snapshot_and_tail_events_only() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("linear.jsonl");
        let mut tail = Vec::new();
        // Two events folded into the snapshot (event_count = 2).
        for event in [
            StateEvent::SimplificationReviewed,
            StateEvent::SnapshotObserved {
                files: vec!["a.rs".to_string()],
            },
        ] {
            write_transcript_entry(&path, &make_state_event_entry("sess-l", event.clone()))
                .await
                .unwrap();
        }
        write_transcript_entry(
            &path,
            &make_state_snapshot_entry("sess-l", make_snapshot(2)),
        )
        .await
        .unwrap();
        // Two events written AFTER the snapshot -> the increment to replay.
        for event in [
            StateEvent::PlanStepSet {
                step: "s3".to_string(),
            },
            StateEvent::SimplificationReviewed,
        ] {
            tail.push(event.clone());
            write_transcript_entry(&path, &make_state_event_entry("sess-l", event))
                .await
                .unwrap();
        }
        let loaded = load_state_snapshot(&path).await.unwrap().expect("snapshot");
        assert_eq!(loaded.0.event_count, 2);
        assert_eq!(loaded.0.body.tool_calls, 12);
        assert_eq!(loaded.1, tail, "only the events after the snapshot replay");
    }

    #[tokio::test]
    async fn load_state_snapshot_ignores_interleaved_messages() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mixed.jsonl");
        let msg_uuid = uuid::Uuid::new_v4().to_string();
        write_transcript_entry(
            &path,
            &make_user_entry(make_msg(Role::User), &msg_uuid, None, "sess-m", "/proj"),
        )
        .await
        .unwrap();
        write_transcript_entry(
            &path,
            &make_state_event_entry("sess-m", StateEvent::SimplificationReviewed),
        )
        .await
        .unwrap();
        write_transcript_entry(
            &path,
            &make_state_snapshot_entry("sess-m", make_snapshot(1)),
        )
        .await
        .unwrap();
        let msg_uuid2 = uuid::Uuid::new_v4().to_string();
        write_transcript_entry(
            &path,
            &make_user_entry(make_msg(Role::User), &msg_uuid2, None, "sess-m", "/proj"),
        )
        .await
        .unwrap();
        let loaded = load_state_snapshot(&path).await.unwrap().expect("snapshot");
        assert_eq!(loaded.0.event_count, 1);
        assert!(loaded.1.is_empty(), "messages are not events");
    }

    #[tokio::test]
    async fn load_state_snapshot_discards_on_watermark_mismatch() {
        // A rewritten/compacted transcript that dropped old events leaves the
        // watermark ahead of the file -> the cache is invalid.
        let dir = tempdir().unwrap();
        let path = dir.path().join("rewritten.jsonl");
        write_transcript_entry(
            &path,
            &make_state_event_entry("sess-r", StateEvent::SimplificationReviewed),
        )
        .await
        .unwrap();
        // Claims to have folded 5 events, but only 1 was written.
        write_transcript_entry(
            &path,
            &make_state_snapshot_entry("sess-r", make_snapshot(5)),
        )
        .await
        .unwrap();
        let loaded = load_state_snapshot(&path).await.unwrap();
        assert!(loaded.is_none(), "watermark mismatch must invalidate");
    }

    #[tokio::test]
    async fn load_state_snapshot_discards_on_leaf() {
        // Branch semantics make the linear event count untrustworthy; the
        // branch-aware full load owns leaf sessions.
        let dir = tempdir().unwrap();
        let path = dir.path().join("branch.jsonl");
        write_transcript_entry(
            &path,
            &make_state_event_entry("sess-b", StateEvent::SimplificationReviewed),
        )
        .await
        .unwrap();
        write_transcript_entry(
            &path,
            &make_state_snapshot_entry("sess-b", make_snapshot(1)),
        )
        .await
        .unwrap();
        set_leaf(&path, None).await.unwrap();
        let loaded = load_state_snapshot(&path).await.unwrap();
        assert!(loaded.is_none(), "leaf present -> full replay path");
    }

    #[tokio::test]
    async fn load_state_snapshot_discards_on_stale_schema_version() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("old.jsonl");
        write_transcript_entry(
            &path,
            &make_state_event_entry("sess-o", StateEvent::SimplificationReviewed),
        )
        .await
        .unwrap();
        let mut stale = make_snapshot(1);
        stale.schema_version = STATE_SNAPSHOT_SCHEMA_VERSION + 1;
        write_transcript_entry(&path, &make_state_snapshot_entry("sess-o", stale))
            .await
            .unwrap();
        let loaded = load_state_snapshot(&path).await.unwrap();
        assert!(loaded.is_none(), "stale projector version must invalidate");
    }

    #[tokio::test]
    async fn load_state_snapshot_last_snapshot_wins() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("multi.jsonl");
        for event in [
            StateEvent::SimplificationReviewed,
            StateEvent::SnapshotObserved {
                files: vec!["a.rs".to_string()],
            },
        ] {
            write_transcript_entry(&path, &make_state_event_entry("sess-m", event))
                .await
                .unwrap();
        }
        write_transcript_entry(
            &path,
            &make_state_snapshot_entry("sess-m", make_snapshot(2)),
        )
        .await
        .unwrap();
        // One more event, then a second snapshot folding all three.
        write_transcript_entry(
            &path,
            &make_state_event_entry(
                "sess-m",
                StateEvent::PlanStepSet {
                    step: "s4".to_string(),
                },
            ),
        )
        .await
        .unwrap();
        write_transcript_entry(
            &path,
            &make_state_snapshot_entry("sess-m", make_snapshot(3)),
        )
        .await
        .unwrap();
        let loaded = load_state_snapshot(&path).await.unwrap().expect("snapshot");
        assert_eq!(loaded.0.event_count, 3, "newest snapshot supersedes");
        assert!(loaded.1.is_empty(), "no events after the newest snapshot");
    }

    #[tokio::test]
    async fn tombstone_removes_entry() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.jsonl");

        let uuid = uuid::Uuid::new_v4().to_string();
        let msg = make_msg(Role::User);
        let entry = make_user_entry(msg, &uuid, None, "sess-1", "/proj");
        write_transcript_entry(&path, &entry).await.unwrap();

        tombstone_entry(&path, &uuid).await.unwrap();

        let loaded = load_transcript(&path).await.unwrap();
        assert_eq!(loaded.len(), 0, "tombstoned entry should be excluded");
    }

    #[tokio::test]
    async fn list_sessions_returns_sorted() {
        let tmp = tempdir().unwrap();
        let project_root = tmp.path().join("myproject");
        tokio::fs::create_dir_all(&project_root).await.unwrap();

        let tdir = transcript_dir_in(tmp.path(), &project_root);
        tokio::fs::create_dir_all(&tdir).await.unwrap();

        for id in ["aaaa", "bbbb"] {
            let p = tdir.join(format!("{}.jsonl", id));
            let msg = make_msg(Role::User);
            let uuid_val = uuid::Uuid::new_v4().to_string();
            let entry = make_user_entry(msg, &uuid_val, None, id, "/proj");
            write_transcript_entry(&p, &entry).await.unwrap();
            // Small sleep to ensure different mtimes.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let sessions = list_sessions_in(tmp.path(), &project_root).await.unwrap();
        assert_eq!(sessions.len(), 2);
        // Newest first.
        assert_eq!(sessions[0].session_id, "bbbb");
        assert_eq!(sessions[1].session_id, "aaaa");
    }

    // -----------------------------------------------------------------------
    // last-prompt / ai-title writer helpers (session-exit metadata)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn write_last_prompt_and_ai_title_round_trip_in_tail_metadata() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("meta.jsonl");

        // A couple of chain messages so the file has real content.
        let msg = make_msg(Role::User);
        let uuid = uuid::Uuid::new_v4().to_string();
        write_transcript_entry(
            &path,
            &make_user_entry(msg.clone(), &uuid, None, "sess", "/proj"),
        )
        .await
        .unwrap();

        write_last_prompt(&path, "sess", "Fix the flaky test")
            .await
            .unwrap();
        write_ai_title(&path, "sess", "Fix flaky test")
            .await
            .unwrap();

        // The tail reader extracts all three metadata fields plus message count.
        let (last_prompt, custom_title, ai_title, message_count) =
            read_session_tail_metadata(&path).await;
        assert_eq!(last_prompt.as_deref(), Some("Fix the flaky test"));
        assert_eq!(custom_title, None, "no custom title written");
        assert_eq!(ai_title.as_deref(), Some("Fix flaky test"));
        // The tail contains the user entry from write_last_prompt.
        assert!(message_count >= 1, "at least one entry: {message_count}");
    }

    #[tokio::test]
    async fn list_sessions_surfaces_ai_title() {
        let tmp = tempdir().unwrap();
        let project_root = tmp.path().join("proj");
        tokio::fs::create_dir_all(&project_root).await.unwrap();

        let tdir = transcript_dir_in(tmp.path(), &project_root);
        tokio::fs::create_dir_all(&tdir).await.unwrap();

        let p = tdir.join("aaaa.jsonl");
        let msg = make_msg(Role::User);
        let uuid = uuid::Uuid::new_v4().to_string();
        write_transcript_entry(&p, &make_user_entry(msg, &uuid, None, "aaaa", "/proj"))
            .await
            .unwrap();
        write_ai_title(&p, "aaaa", "Add auth tests").await.unwrap();

        let sessions = list_sessions_in(tmp.path(), &project_root).await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].ai_title.as_deref(), Some("Add auth tests"));
        assert_eq!(sessions[0].title, None, "no custom title set");
    }

    // -----------------------------------------------------------------------
    // Session tree / leaf reconstruction (issue #234)
    // -----------------------------------------------------------------------

    /// Build a chain-participant entry with an explicit entry-level `uuid`,
    /// `parent_uuid`, and a distinct text body so branches are identifiable.
    fn chain_entry(role: Role, uuid: &str, parent: Option<&str>, text: &str) -> TranscriptEntry {
        let is_assistant = role == Role::Assistant;
        let msg = Message {
            role,
            content: MessageContent::Text(text.to_string()),
            uuid: Some(format!("msg-{uuid}")),
            cost: None,
            snapshot_patch: None,
            turn_meta: None,
        };
        let tm = TranscriptMessage {
            uuid: Some(uuid.to_string()),
            parent_uuid: parent.map(|s| s.to_string()),
            timestamp: chrono::Utc::now().to_rfc3339(),
            session_id: "sess".to_string(),
            cwd: "/proj".to_string(),
            message: msg,
            is_sidechain: false,
            user_type: "external".to_string(),
            version: "test".to_string(),
            git_branch: None,
            agent_role: None,
            managed_session_id: None,
            extra: Default::default(),
        };
        if is_assistant {
            TranscriptEntry::Assistant(tm)
        } else {
            TranscriptEntry::User(tm)
        }
    }

    fn texts(msgs: &[Message]) -> Vec<String> {
        msgs.iter()
            .map(|m| match &m.content {
                MessageContent::Text(t) => t.clone(),
                _ => String::new(),
            })
            .collect()
    }

    /// BACK-COMPAT GUARANTEE: an old-format session with NO `leaf` entry loads
    /// exactly as before — chain participants in file order, identical to
    /// `messages_from_transcript`.
    #[test]
    fn old_format_no_leaf_loads_in_file_order() {
        let entries = vec![
            chain_entry(Role::User, "u1", None, "hello"),
            chain_entry(Role::Assistant, "a1", Some("u1"), "hi"),
            chain_entry(Role::User, "u2", Some("a1"), "again"),
            chain_entry(Role::Assistant, "a2", Some("u2"), "yes"),
        ];

        // No leaf entry present.
        assert!(last_leaf(&entries).is_none());

        let active = active_branch_messages(&entries);
        let linear = messages_from_transcript(&entries);
        // Identical to the pre-#234 linear reconstruction.
        assert_eq!(texts(&active), texts(&linear));
        assert_eq!(texts(&active), vec!["hello", "hi", "again", "yes"]);
    }

    /// A session with a `leaf` reconstructs the branch ending at that leaf by
    /// walking parent links, excluding the abandoned sibling branch (which is
    /// still present on disk).
    #[test]
    fn leaf_reconstructs_active_branch() {
        // Tree:
        //   u1 ── a1 ──┬── u2a ── a2a   (abandoned branch)
        //              └── u2b ── a2b   (active branch, leaf = a2b)
        let entries = vec![
            chain_entry(Role::User, "u1", None, "start"),
            chain_entry(Role::Assistant, "a1", Some("u1"), "ok"),
            chain_entry(Role::User, "u2a", Some("a1"), "path-A"),
            chain_entry(Role::Assistant, "a2a", Some("u2a"), "reply-A"),
            chain_entry(Role::User, "u2b", Some("a1"), "path-B"),
            chain_entry(Role::Assistant, "a2b", Some("u2b"), "reply-B"),
            TranscriptEntry::Leaf(LeafEntry {
                leaf_uuid: Some("a2b".to_string()),
            }),
        ];

        let active = active_branch_messages(&entries);
        // Only the B branch, in root→leaf order.
        assert_eq!(texts(&active), vec!["start", "ok", "path-B", "reply-B"]);

        // Re-pointing the leaf at the abandoned branch retrieves it — nothing
        // was destroyed.
        let mut back = entries.clone();
        back.push(TranscriptEntry::Leaf(LeafEntry {
            leaf_uuid: Some("a2a".to_string()),
        }));
        let restored = active_branch_messages(&back);
        assert_eq!(texts(&restored), vec!["start", "ok", "path-A", "reply-A"]);
    }

    /// The last `leaf` entry wins when several are present.
    #[test]
    fn last_leaf_wins() {
        let entries = vec![
            chain_entry(Role::User, "u1", None, "a"),
            chain_entry(Role::Assistant, "a1", Some("u1"), "b"),
            chain_entry(Role::User, "u2", Some("a1"), "c"),
            TranscriptEntry::Leaf(LeafEntry {
                leaf_uuid: Some("u2".to_string()),
            }),
            TranscriptEntry::Leaf(LeafEntry {
                leaf_uuid: Some("a1".to_string()),
            }),
        ];
        let active = active_branch_messages(&entries);
        assert_eq!(texts(&active), vec!["a", "b"]);
    }

    /// A reset leaf (no `leafUuid`) yields an empty active branch.
    #[test]
    fn reset_leaf_yields_empty_branch() {
        let entries = vec![
            chain_entry(Role::User, "u1", None, "a"),
            chain_entry(Role::Assistant, "a1", Some("u1"), "b"),
            TranscriptEntry::Leaf(LeafEntry { leaf_uuid: None }),
        ];
        assert!(active_branch_messages(&entries).is_empty());
    }

    /// A leaf pointing at a missing uuid falls back to file order.
    #[test]
    fn dangling_leaf_falls_back_to_file_order() {
        let entries = vec![
            chain_entry(Role::User, "u1", None, "a"),
            chain_entry(Role::Assistant, "a1", Some("u1"), "b"),
            TranscriptEntry::Leaf(LeafEntry {
                leaf_uuid: Some("nope".to_string()),
            }),
        ];
        assert_eq!(texts(&active_branch_messages(&entries)), vec!["a", "b"]);
    }

    /// The `leaf` entry round-trips through JSON and is skippable by readers
    /// that ignore it (it is not a chain participant).
    #[test]
    fn leaf_entry_json_round_trip() {
        let e = TranscriptEntry::Leaf(LeafEntry {
            leaf_uuid: Some("abc".to_string()),
        });
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("\"type\":\"leaf\""), "got {s}");
        assert!(s.contains("\"leafUuid\":\"abc\""), "got {s}");
        let back: TranscriptEntry = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, TranscriptEntry::Leaf(_)));
        assert!(!back.is_chain_participant());
        assert!(back.uuid().is_none());

        // Reset leaf omits leafUuid.
        let reset = TranscriptEntry::Leaf(LeafEntry { leaf_uuid: None });
        let s2 = serde_json::to_string(&reset).unwrap();
        assert_eq!(s2, "{\"type\":\"leaf\"}");
    }

    /// Build a `Message` whose inner uuid equals the entry uuid, so the tree
    /// (entry-level) and revert key (message-level) uuids coincide.
    fn msg_with_uuid(role: Role, uuid: &str, text: &str) -> Message {
        Message {
            role,
            content: MessageContent::Text(text.to_string()),
            uuid: Some(uuid.to_string()),
            cost: None,
            snapshot_patch: None,
            turn_meta: None,
        }
    }

    /// Write a linear, properly-chained transcript to disk and return the path.
    async fn write_chain(dir: &std::path::Path, chained: bool) -> PathBuf {
        let path = dir.join("chain.jsonl");
        // u1 -> a1 -> u2 -> a2
        let steps = [
            (Role::User, "u1", None, "start"),
            (Role::Assistant, "a1", Some("u1"), "ok"),
            (Role::User, "u2", Some("a1"), "next"),
            (Role::Assistant, "a2", Some("u2"), "reply"),
        ];
        for (role, uuid, parent, text) in steps {
            let parent = if chained { parent } else { None };
            let is_assistant = role == Role::Assistant;
            let msg = msg_with_uuid(role, uuid, text);
            let entry = if is_assistant {
                make_assistant_entry(msg, uuid, parent, "sess", "/proj")
            } else {
                make_user_entry(msg, uuid, parent, "sess", "/proj")
            };
            write_transcript_entry(&path, &entry).await.unwrap();
        }
        path
    }

    #[tokio::test]
    async fn set_leaf_appends_pointer() {
        let dir = tempdir().unwrap();
        let path = write_chain(dir.path(), true).await;
        set_leaf(&path, Some("a1")).await.unwrap();
        let entries = load_transcript(&path).await.unwrap();
        assert_eq!(
            last_leaf(&entries).and_then(|l| l.leaf_uuid.as_deref()),
            Some("a1")
        );
    }

    /// Non-destructive branch: reverting the last assistant turn points the leaf
    /// at its parent, keeps the later entry on disk, and yields the right
    /// active conversation on reload.
    #[tokio::test]
    async fn branch_before_retains_later_entries_and_sets_leaf() {
        let dir = tempdir().unwrap();
        let path = write_chain(dir.path(), true).await;

        let branched = branch_before(&path, "a2").await.unwrap();
        assert!(branched);

        // The reverted turn is still physically on disk (non-destructive).
        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(
            raw.contains("\"reply\""),
            "later entry must be retained on disk"
        );

        // Reconstructed active branch ends just before the reverted turn.
        let entries = load_transcript(&path).await.unwrap();
        assert_eq!(
            last_leaf(&entries).and_then(|l| l.leaf_uuid.as_deref()),
            Some("u2")
        );
        let active = active_branch_messages(&entries);
        assert_eq!(texts(&active), vec!["start", "ok", "next"]);

        // The abandoned turn can be recovered by re-pointing the leaf.
        set_leaf(&path, Some("a2")).await.unwrap();
        let entries = load_transcript(&path).await.unwrap();
        assert_eq!(
            texts(&active_branch_messages(&entries)),
            vec!["start", "ok", "next", "reply"]
        );
    }

    #[tokio::test]
    async fn branch_before_not_found_is_noop() {
        let dir = tempdir().unwrap();
        let path = write_chain(dir.path(), true).await;
        let before = tokio::fs::read_to_string(&path).await.unwrap();

        let branched = branch_before(&path, "no-such-uuid").await.unwrap();
        assert!(!branched);

        let after = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(before, after, "no-op must not modify the file");
    }

    /// When the transcript has no walkable parent chain, branch_before must fall
    /// back to the destructive truncate so the retained prefix is not lost.
    #[tokio::test]
    async fn branch_before_falls_back_when_unchained() {
        let dir = tempdir().unwrap();
        let path = write_chain(dir.path(), false).await; // parent_uuid all None

        let branched = branch_before(&path, "a2").await.unwrap();
        assert!(branched);

        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        // Destructive fallback dropped the reverted turn and wrote no leaf.
        assert!(
            !raw.contains("\"reply\""),
            "unchained fallback truncates the turn"
        );
        assert!(
            !raw.contains("\"type\":\"leaf\""),
            "fallback writes no leaf pointer"
        );

        let entries = load_transcript(&path).await.unwrap();
        assert!(last_leaf(&entries).is_none());
        assert_eq!(
            texts(&active_branch_messages(&entries)),
            vec!["start", "ok", "next"]
        );
    }

    #[test]
    fn transcript_path_encoding_is_reversible() {
        let root = Path::new("/Users/alice/my-project");
        let path = transcript_path(root, "test-session").unwrap();
        // The directory component after "projects/" should decode back to the root.
        let encoded_dir = path
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        let decoded = URL_SAFE_NO_PAD.decode(encoded_dir).unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), root.to_str().unwrap());
    }

    /// Create a throwaway git repository (needed so `get_repo_root` resolves).
    fn make_repo() -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        let out = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["init", "-q"])
            .output()
            .expect("git binary available");
        assert!(
            out.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        dir
    }

    /// A transcript staged under the *cwd* bucket (the pre-fix layout) must be
    /// moved into the git-root bucket, and the stale bucket removed. Re-running
    /// the migration is a no-op (idempotent).
    #[test]
    fn migrate_cwd_transcript_buckets_moves_subdirectory_transcripts() {
        let config = tempdir().unwrap();
        let repo = make_repo();
        let subdir = repo.path().join("src");
        std::fs::create_dir_all(&subdir).unwrap();

        // Stage a transcript under the cwd bucket (pre-fix layout).
        let cwd_bucket = transcript_dir_in(config.path(), &subdir);
        std::fs::create_dir_all(&cwd_bucket).unwrap();
        let staged = cwd_bucket.join("sess-legacy.jsonl");
        std::fs::write(&staged, "{\"type\":\"user\"}\n").unwrap();

        let moved = migrate_cwd_transcript_buckets(config.path());
        assert_eq!(moved, 1, "one transcript should be migrated");

        // File now lives in the git-root bucket.
        let git_root = crate::git_utils::get_repo_root(repo.path()).unwrap();
        let root_bucket = transcript_dir_in(config.path(), &git_root);
        assert!(
            root_bucket.join("sess-legacy.jsonl").exists(),
            "transcript must be in the git-root bucket"
        );
        // Old cwd bucket is gone (empty after the move).
        assert!(!cwd_bucket.exists(), "stale cwd bucket must be removed");

        // Idempotent: second run moves nothing.
        assert_eq!(migrate_cwd_transcript_buckets(config.path()), 0);
    }

    /// Buckets whose decoded path is already the git root, or is not inside a
    /// git repo at all, are left untouched.
    #[test]
    fn migrate_cwd_transcript_buckets_leaves_other_buckets_alone() {
        let config = tempdir().unwrap();
        let repo = make_repo();

        // Bucket keyed on the git root itself — already canonical, must not move.
        let root_bucket = transcript_dir_in(config.path(), repo.path());
        std::fs::create_dir_all(&root_bucket).unwrap();
        let root_file = root_bucket.join("sess-root.jsonl");
        std::fs::write(&root_file, "{\"type\":\"user\"}\n").unwrap();

        // Bucket keyed on a non-repo directory — must not move.
        let scratch = tempdir().unwrap();
        let scratch_bucket = transcript_dir_in(config.path(), scratch.path());
        std::fs::create_dir_all(&scratch_bucket).unwrap();
        let scratch_file = scratch_bucket.join("sess-scratch.jsonl");
        std::fs::write(&scratch_file, "{\"type\":\"user\"}\n").unwrap();

        let moved = migrate_cwd_transcript_buckets(config.path());
        assert_eq!(moved, 0, "nothing should move");
        assert!(root_file.exists());
        assert!(scratch_file.exists());
    }
}
