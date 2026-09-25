// External session history absorption with persistence + dedup watermarking.
//
// This module implements the "absorb once, never re-poll old as new" semantics:
//
//   1. On startup in a project directory, Clawde scans the configured external
//      agent locations (Opencode, Cline, Freebuff) for sessions.
//   2. For each session file, a content fingerprint (SHA-256 of the file bytes)
//      is computed and compared to the last recorded fingerprint in a per-project
//      state file.
//   3. If the fingerprint matches what we already absorbed → skip entirely.
//   4. If the fingerprint differs (new file or file changed) → re-parse the full
//      file and re-absorb (replace). This is simpler than incremental watermarks
//      and handles content edits correctly.
//   5. The new fingerprints are persisted to `~/.clawde/projects/{b64(project)}/external_import_state.json`
//      so the *next* startup can distinguish new/changed sessions from stale ones.
//
// The absorbed messages are returned to the caller (CLI), which prepends them
// to the new Clawde session. The state file ensures:
//   - Old external sessions are never re-polled on every startup.
//   - New external sessions created since the last startup are detected.
//   - Changed sessions (new messages appended) are re-absorbed fully.

use crate::types::Message;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;

/// One absorbed external session's tracking record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbsorbedEntry {
    /// SHA-256 of the session file's contents at time of absorption.
    #[serde(rename = "fp")]
    fingerprint: String,
    /// Number of messages parsed from this session file.
    #[serde(rename = "count")]
    message_count: usize,
}

/// Per-project absorption state. Maps external session file paths to their
/// last-absorbed fingerprints.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExternalAbsorptionState {
    /// Keyed by source file path (to_string_lossy).
    #[serde(default)]
    sessions: HashMap<String, AbsorbedEntry>,
}

/// Compute a SHA-256 fingerprint of a file's contents.
fn fingerprint_of_file(path: &Path) -> String {
    let mut hasher = Sha256::new();
    if let Ok(contents) = std::fs::read(path) {
        hasher.update(&contents);
    }
    let result = hasher.finalize();
    format!("{:x}", result)
}

/// Load the absorption state for a project from disk.
///
/// Returns an empty state if the file doesn't exist or is malformed.
pub fn load_state(state_path: &Path) -> ExternalAbsorptionState {
    if !state_path.exists() {
        return ExternalAbsorptionState::default();
    }
    match std::fs::read_to_string(state_path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => ExternalAbsorptionState::default(),
    }
}

/// Persist the absorption state to disk atomically (write tmp + rename).
pub fn save_state(state: &ExternalAbsorptionState, state_path: &Path) {
    let dir = state_path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).ok();

    let state_json = match serde_json::to_string_pretty(state) {
        Ok(json) => json,
        Err(_) => return,
    };

    let tmp_path = state_path.with_extension("json.tmp");
    if std::fs::write(&tmp_path, &state_json).is_ok() {
        std::fs::rename(&tmp_path, state_path).ok();
    }
}

/// Upper bound on the number of external messages prepended into a new session
/// in a single absorb. Absorbing every matching session (which, across many
/// projects, can be hundreds of messages) risks blowing the context window on
/// the first run. When the cap is reached the remaining sessions are skipped
/// (and left eligible for a later run) rather than dropped silently.
pub const MAX_ABSORBED_MESSAGES: usize = 200;

/// Absorb new/changed sessions into `state`, newest-file-first, up to
/// [`MAX_ABSORBED_MESSAGES`]. Returns the absorbed messages and the number of
/// sessions skipped because the cap was reached.
///
/// Ordering is by source-file mtime (newest first) so the cap deterministically
/// keeps the most recent history; ties break on the source path so repeated runs
/// absorb the same set. A cap-skipped session's fingerprint is *not* recorded,
/// so it stays eligible on a later run instead of being silently lost.
fn absorb_sessions_capped(
    mut sessions: Vec<crate::session_import::ImportedSession>,
    state: &mut ExternalAbsorptionState,
) -> (Vec<Message>, usize) {
    sessions.sort_by(|a, b| {
        let ma = std::fs::metadata(&a.source_path)
            .and_then(|m| m.modified())
            .ok();
        let mb = std::fs::metadata(&b.source_path)
            .and_then(|m| m.modified())
            .ok();
        // Newest first.
        ma.cmp(&mb)
            .reverse()
            .then_with(|| a.source_path.cmp(&b.source_path))
    });

    let mut absorbed: Vec<Message> = Vec::new();
    let mut skipped = 0usize;
    for session in sessions {
        let key = session.source_path.to_string_lossy().to_string();
        let fingerprint = fingerprint_of_file(&session.source_path);

        // Unchanged since last absorption — skip silently.
        // This is the "don't keep polling old history as new" guard.
        if let Some(entry) = state.sessions.get(&key) {
            if entry.fingerprint == fingerprint {
                continue;
            }
        }

        // Respect the cap; leave the session eligible for a later run.
        if absorbed.len().saturating_add(session.messages.len()) > MAX_ABSORBED_MESSAGES {
            skipped += 1;
            continue;
        }

        // New file or changed file: re-absorb fully.
        absorbed.extend(session.messages.iter().cloned());
        state.sessions.insert(
            key,
            AbsorbedEntry {
                fingerprint,
                message_count: session.messages.len(),
            },
        );
    }
    (absorbed, skipped)
}

/// Absorb new/changed external session history for the project containing `cwd`.
///
/// This is the single entry point called at startup (both headless and TUI modes)
/// when `Settings::import_external_sessions_on_start` is enabled.
///
/// Steps:
///   1. Resolve the project root (git root, fallback to cwd canonicalized).
///   2. Load the existing absorption state from
///      `~/.clawde/projects/{b64(project)}/external_import_state.json`.
///   3. Discover all external sessions (Opencode, Cline, Freebuff) scoped to cwd.
///   4. For each, compute a fingerprint; skip if unchanged, else re-absorb (up to
///      [`MAX_ABSORBED_MESSAGES`], newest first).
///   5. Save the updated state file.
///   6. Return the Vec<Message> of newly absorbed messages.
///
/// Returns an empty Vec if nothing new was absorbed (or on any error) — the
/// caller treats that as "no extra context to prepend."
pub fn absorb_new_external_sessions(cwd: &Path) -> Vec<Message> {
    // Resolve project root. Use git_utils if available, fallback to canonicalized cwd.
    let project = crate::git_utils::project_root(cwd);

    // State file lives in the project's transcript bucket.
    let transcript_bucket = crate::session_storage::transcript_dir(&project);
    let state_path = transcript_bucket.join("external_import_state.json");

    let mut state = load_state(&state_path);

    let sessions = crate::session_import::import_sessions_for_cwd(cwd);
    let session_count = sessions.len();

    let (absorbed, skipped) = absorb_sessions_capped(sessions, &mut state);

    // Persist updated state for the next startup.
    if !absorbed.is_empty() {
        save_state(&state, &state_path);
    }

    if !absorbed.is_empty() {
        // tracing (not eprintln!) so this is routed through the app's log/event
        // system instead of writing over the TUI alternate screen.
        tracing::info!(
            messages = absorbed.len(),
            sessions = session_count,
            skipped,
            project = %project.display(),
            "Absorbed external session history"
        );
    }

    absorbed
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Helper: create a state file with one pre-absorbed session.
    fn write_state(dir: &Path, file_path: &str, fp: &str, count: usize) {
        let state = ExternalAbsorptionState {
            sessions: HashMap::from([(
                file_path.to_string(),
                AbsorbedEntry {
                    fingerprint: fp.to_string(),
                    message_count: count,
                },
            )]),
        };
        save_state(&state, &dir.join("external_import_state.json"));
    }

    #[test]
    fn loads_state_from_disk() {
        let dir = tempdir().unwrap();
        write_state(dir.path(), "/some/file.json", "abc123", 5);
        let state = load_state(&dir.path().join("external_import_state.json"));
        assert_eq!(state.sessions.len(), 1);
        assert_eq!(
            state.sessions.get("/some/file.json").unwrap().fingerprint,
            "abc123"
        );
        assert_eq!(
            state.sessions.get("/some/file.json").unwrap().message_count,
            5
        );
    }

    #[test]
    fn returns_empty_when_no_state_exists() {
        let dir = tempdir().unwrap();
        let state = load_state(&dir.path().join("nonexistent.json"));
        assert!(state.sessions.is_empty());
    }

    #[test]
    fn handles_malformed_state_gracefully() {
        let dir = tempdir().unwrap();
        let bad_path = dir.path().join("external_import_state.json");
        std::fs::write(&bad_path, "{ this is not valid json").unwrap();
        let state = load_state(&bad_path);
        assert!(state.sessions.is_empty());
    }

    #[test]
    fn fingerprint_changes_when_content_changes() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("session.json");
        std::fs::write(&file, "old content").unwrap();
        let fp1 = fingerprint_of_file(&file);

        std::fs::write(&file, "new content").unwrap();
        let fp2 = fingerprint_of_file(&file);

        assert_ne!(fp1, fp2);
    }

    #[test]
    fn fingerprint_is_stable_for_unchanged_content() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("session.json");
        std::fs::write(&file, "same content").unwrap();
        let fp1 = fingerprint_of_file(&file);
        let fp2 = fingerprint_of_file(&file);
        assert_eq!(fp1, fp2);
    }

    /// Build an ImportedSession backed by a real temp file with `n` messages.
    fn fake_session(dir: &Path, name: &str, n: usize) -> crate::session_import::ImportedSession {
        let file = dir.join(name);
        std::fs::write(&file, format!("content-{name}")).unwrap();
        let messages = (0..n)
            .map(|i| Message {
                role: crate::types::Role::User,
                content: crate::types::MessageContent::Text(format!("m{i}")),
                uuid: None,
                cost: None,
                snapshot_patch: None,
                turn_meta: None,
            })
            .collect();
        crate::session_import::ImportedSession {
            name: name.to_string(),
            working_dir: None,
            source: "cline",
            source_path: file,
            messages,
        }
    }

    #[test]
    fn absorb_respects_message_cap_and_skips_the_rest() {
        let dir = tempdir().unwrap();
        let mut state = ExternalAbsorptionState::default();
        // 3 sessions x 100 messages = 300 > cap(200).
        let sessions = vec![
            fake_session(dir.path(), "a.json", 100),
            fake_session(dir.path(), "b.json", 100),
            fake_session(dir.path(), "c.json", 100),
        ];
        let (absorbed, skipped) = absorb_sessions_capped(sessions, &mut state);
        assert_eq!(absorbed.len(), 200, "capped at MAX_ABSORBED_MESSAGES");
        assert_eq!(skipped, 1, "one session skipped by the cap");
        // Skipped session left eligible (no fingerprint recorded).
        assert_eq!(state.sessions.len(), 2, "only absorbed sessions recorded");
    }

    #[test]
    fn absorb_skips_unchanged_sessions() {
        let dir = tempdir().unwrap();
        let mut state = ExternalAbsorptionState::default();
        let sessions = vec![fake_session(dir.path(), "a.json", 5)];
        let (absorbed, _) = absorb_sessions_capped(sessions.clone(), &mut state);
        assert_eq!(absorbed.len(), 5);
        // Same content -> fingerprint unchanged -> nothing re-absorbed.
        let sessions2 = vec![fake_session(dir.path(), "a.json", 5)];
        let (absorbed2, _) = absorb_sessions_capped(sessions2, &mut state);
        assert_eq!(absorbed2.len(), 0, "unchanged session not re-absorbed");
    }
}
