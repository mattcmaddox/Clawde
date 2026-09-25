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
use std::path::{Path, PathBuf};

/// One external session file this project has already evaluated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbsorbedEntry {
    /// SHA-256 of the session file's contents at the time it was evaluated
    /// (absorbed into this project, or ruled irrelevant to it).
    #[serde(rename = "fp")]
    fingerprint: String,
}

/// Per-project absorption state. Maps external session file paths to the
/// fingerprint last evaluated for *this* project, so an unchanged file is
/// skipped without re-parsing it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExternalAbsorptionState {
    /// Keyed by source file path (to_string_lossy).
    #[serde(default)]
    sessions: HashMap<String, AbsorbedEntry>,
}

/// Compute a SHA-256 fingerprint of a file's contents, or `None` if the file
/// cannot be read. Returning `None` (rather than hashing empty bytes) keeps an
/// unreadable file distinct from a genuinely empty one.
fn fingerprint_of_file(path: &Path) -> Option<String> {
    let contents = std::fs::read(path).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&contents);
    Some(format!("{:x}", hasher.finalize()))
}

/// Load the absorption state for a project from disk.
///
/// A missing file yields an empty state. A *corrupt* file (truncated by a crash
/// or a torn concurrent write) is quarantined as `<name>.corrupt` and treated as
/// empty — but the corruption is logged, because a silently-reset watermark
/// causes the entire external history to be re-absorbed and duplicated.
pub fn load_state(state_path: &Path) -> ExternalAbsorptionState {
    if !state_path.exists() {
        return ExternalAbsorptionState::default();
    }
    let raw = match std::fs::read_to_string(state_path) {
        Ok(raw) => raw,
        Err(e) => {
            tracing::warn!(
                path = %state_path.display(),
                error = %e,
                "Could not read external import state; treating as empty"
            );
            return ExternalAbsorptionState::default();
        }
    };
    match serde_json::from_str(&raw) {
        Ok(state) => state,
        Err(e) => {
            // Quarantine before it is overwritten, so the user can inspect it.
            let quarantine = state_path.with_extension("json.corrupt");
            let _ = std::fs::rename(state_path, &quarantine);
            tracing::warn!(
                path = %state_path.display(),
                quarantined = %quarantine.display(),
                error = %e,
                "External import state was corrupt; moved aside and reset. \
                 External history may be re-absorbed this run."
            );
            ExternalAbsorptionState::default()
        }
    }
}

/// Persist the absorption state to disk atomically: write a per-process unique
/// temp file, fsync it, then rename over the target and fsync the directory.
/// The unique temp name prevents two concurrent clawde processes in the same
/// project from interleaving writes and producing a torn state file.
pub fn save_state(state: &ExternalAbsorptionState, state_path: &Path) {
    let dir = state_path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).ok();

    let state_json = match serde_json::to_string_pretty(state) {
        Ok(json) => json,
        Err(e) => {
            tracing::warn!(error = %e, "Could not serialize external import state");
            return;
        }
    };

    // Unique per process so concurrent writers never collide on the temp path.
    let tmp_path = state_path.with_extension(format!("json.tmp.{}", std::process::id()));
    let write = std::fs::write(&tmp_path, &state_json).and_then(|_| {
        // fsync the bytes before the rename publishes them; a crash after
        // rename must not leave an empty state file.
        std::fs::File::open(&tmp_path)?.sync_all()
    });
    if write.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
        return;
    }
    if std::fs::rename(&tmp_path, state_path).is_err() {
        let _ = std::fs::remove_file(&tmp_path);
        return;
    }
    // fsync the parent dir so the rename itself is durable.
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

/// Upper bound on the number of external messages prepended into a new session
/// in a single absorb. Absorbing every matching session (which, across many
/// projects, can be hundreds of messages) risks blowing the context window on
/// the first run. When the cap is reached the remaining sessions are skipped
/// (and left eligible for a later run) rather than dropped silently.
pub const MAX_ABSORBED_MESSAGES: usize = 200;

/// Absorb new/changed external session FILES for `cwd` into `state`,
/// newest-file-first, fingerprinting each file BEFORE parsing it so unchanged
/// files cost only a hash (never a JSON parse). Returns the absorbed messages
/// and the number of files skipped because the message cap was reached.
///
/// Per file:
///   - unreadable → skipped entirely;
///   - fingerprint already recorded for this project (unchanged) → skipped
///     without parsing (the "absorb once" guarantee);
///   - new/changed → parse just this file; if relevant to `cwd`, absorb up to
///     [`MAX_ABSORBED_MESSAGES`] and record its fingerprint; if irrelevant to
///     this project, record its fingerprint WITHOUT absorbing, so it is never
///     re-parsed here but stays evaluable in the project it belongs to (which
///     has its own state file).
///
/// A cap-skipped file's fingerprint is *not* recorded, so it stays eligible on a
/// later run instead of being silently lost.
fn absorb_paths(
    mut paths: Vec<(&'static str, std::path::PathBuf)>,
    cwd_canonical: &Path,
    state: &mut ExternalAbsorptionState,
) -> (Vec<Message>, usize) {
    // Newest file first so the cap deterministically keeps recent history.
    paths.sort_by(|a, b| {
        let ma = std::fs::metadata(&a.1).and_then(|m| m.modified()).ok();
        let mb = std::fs::metadata(&b.1).and_then(|m| m.modified()).ok();
        ma.cmp(&mb).reverse().then_with(|| a.1.cmp(&b.1))
    });

    let mut absorbed: Vec<Message> = Vec::new();
    let mut skipped = 0usize;
    for (source, path) in paths {
        let key = path.to_string_lossy().to_string();
        // Fingerprint first: an unchanged file is skipped WITHOUT parsing.
        let Some(fingerprint) = fingerprint_of_file(&path) else {
            continue;
        };
        if state
            .sessions
            .get(&key)
            .is_some_and(|e| e.fingerprint == fingerprint)
        {
            continue;
        }

        // New or changed: parse just this file.
        let Some(session) = crate::session_import::import_one_source(source, &path) else {
            continue;
        };

        if !crate::session_import::session_relevant_to_cwd(&session, cwd_canonical) {
            // Irrelevant to this project: record so it is not re-parsed here,
            // but leave it evaluable in its own project (separate state file).
            state.sessions.insert(key, AbsorbedEntry { fingerprint });
            continue;
        }

        // Respect the cap, but never starve a session outright. If a single
        // session is larger than the cap, absorb its most recent `remaining`
        // turns (recent context is the useful part) instead of skipping it
        // forever — a long session would otherwise never satisfy the cap and be
        // re-parsed on every startup, permanently.
        let remaining = MAX_ABSORBED_MESSAGES.saturating_sub(absorbed.len());
        if remaining == 0 {
            // Cap already full from earlier (newer) sessions: leave this one
            // eligible for a future run.
            skipped += 1;
            continue;
        }
        let msgs = if session.messages.len() > remaining {
            let mut m = session.messages;
            let drop = m.len() - remaining;
            m.drain(..drop); // keep the most recent tail
            m
        } else {
            session.messages
        };
        absorbed.extend(msgs);
        state.sessions.insert(key, AbsorbedEntry { fingerprint });
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
///   2. Load the per-project absorption state from
///      `~/.clawde/projects/{b64(project)}/external_import_state.json`.
///   3. Discover external session FILES (no parsing) across Opencode, Cline,
///      Freebuff.
///   4. Fingerprint each; absorb new/changed relevant ones up to
///      [`MAX_ABSORBED_MESSAGES`], newest first.
///
/// Returns `(absorbed_messages, commit)`. The state is NOT persisted here: the
/// caller must invoke `commit.commit()` only after the session that carries
/// these messages has been durably saved. Committing eagerly would record the
/// fingerprint even if the user quits before the session is written, permanently
/// losing the imported history. If the caller crashes before committing, the
/// next run simply re-imports (safe direction).
pub fn absorb_new_external_sessions(cwd: &Path) -> (Vec<Message>, AbsorbCommit) {
    let project = crate::git_utils::project_root(cwd);
    let transcript_bucket = crate::session_storage::transcript_dir(&project);
    let state_path = transcript_bucket.join("external_import_state.json");
    let mut state = load_state(&state_path);

    let cwd_canonical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let paths = crate::session_import::discover_all_external_paths();
    let candidate_count = paths.len();

    let (absorbed, skipped) = absorb_paths(paths, &cwd_canonical, &mut state);

    if !absorbed.is_empty() {
        // tracing (not eprintln!) so it is routed through the app's log/event
        // system instead of writing over the TUI alternate screen.
        tracing::info!(
            messages = absorbed.len(),
            candidates = candidate_count,
            skipped,
            project = %project.display(),
            "Absorbed external session history (pending durable-save commit)"
        );
    }

    let commit = AbsorbCommit {
        state,
        path: state_path,
    };
    (absorbed, commit)
}

/// A pending absorption-state commit. Persist it with [`AbsorbCommit::commit`]
/// only once the session carrying the absorbed messages has been durably saved.
pub struct AbsorbCommit {
    state: ExternalAbsorptionState,
    path: PathBuf,
}

impl AbsorbCommit {
    /// Persist the updated absorption state. Call this AFTER the session
    /// containing the imported messages is durably saved; otherwise a crash
    /// between absorb and save would lose the import for good.
    pub fn commit(self) {
        if !self.state.sessions.is_empty() {
            save_state(&self.state, &self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn state_with(fp: &str, key: &str) -> ExternalAbsorptionState {
        ExternalAbsorptionState {
            sessions: HashMap::from([(
                key.to_string(),
                AbsorbedEntry {
                    fingerprint: fp.to_string(),
                },
            )]),
        }
    }

    #[test]
    fn loads_state_from_disk() {
        let dir = tempdir().unwrap();
        save_state(
            &state_with("abc123", "/some/file.json"),
            &dir.path().join("external_import_state.json"),
        );
        let state = load_state(&dir.path().join("external_import_state.json"));
        assert_eq!(state.sessions.len(), 1);
        assert_eq!(
            state.sessions.get("/some/file.json").unwrap().fingerprint,
            "abc123"
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
        let bad = dir.path().join("external_import_state.json");
        std::fs::write(&bad, "{ not valid json").unwrap();
        assert!(load_state(&bad).sessions.is_empty());
    }

    #[test]
    fn fingerprint_changes_when_content_changes_and_none_when_unreadable() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("s.json");
        std::fs::write(&file, "old").unwrap();
        let a = fingerprint_of_file(&file).unwrap();
        std::fs::write(&file, "new").unwrap();
        let b = fingerprint_of_file(&file).unwrap();
        assert_ne!(a, b);
        // A missing file has no fingerprint (distinct from empty content).
        assert!(fingerprint_of_file(&dir.path().join("missing")).is_none());
    }

    /// Write a valid Cline session.json for `workspace` with `n` user messages.
    fn write_cline(dir: &Path, name: &str, workspace: &Path, n: usize) -> PathBuf {
        let file = dir.join(name);
        let msgs: Vec<String> = (0..n)
            .map(|i| format!(r#"{{"role":"user","text":"m{i}"}}"#))
            .collect();
        let body = format!(
            r#"{{"workspacePath":{:?},"conversation":[{}]}}"#,
            workspace.display().to_string(),
            msgs.join(",")
        );
        std::fs::write(&file, body).unwrap();
        file
    }

    #[test]
    fn absorb_absorbs_relevant_session_then_skips_it_as_unchanged() {
        let dir = tempdir().unwrap();
        let cwd = dir.path().to_path_buf();
        let file = write_cline(dir.path(), "s.json", &cwd, 3);
        let mut state = ExternalAbsorptionState::default();
        let paths = vec![("cline", file.clone())];
        let (absorbed, skipped) = absorb_paths(paths.clone(), &cwd, &mut state);
        assert_eq!(absorbed.len(), 3, "relevant session absorbed");
        assert_eq!(skipped, 0);
        // Second run: unchanged fingerprint -> not re-absorbed (fingerprint-before-parse).
        let (absorbed2, _) = absorb_paths(paths, &cwd, &mut state);
        assert_eq!(absorbed2.len(), 0, "unchanged session not re-absorbed");
    }

    #[test]
    fn absorb_routes_out_irrelevant_session_without_absorbing() {
        let dir = tempdir().unwrap();
        let cwd = dir.path().to_path_buf();
        let other = tempdir().unwrap();
        let file = write_cline(dir.path(), "s.json", other.path(), 3);
        let mut state = ExternalAbsorptionState::default();
        let (absorbed, _) = absorb_paths(vec![("cline", file)], &cwd, &mut state);
        assert_eq!(absorbed.len(), 0, "other-project session not absorbed");
        // But its fingerprint IS recorded so it is not re-parsed for this project.
        assert_eq!(state.sessions.len(), 1);
    }

    #[test]
    fn absorb_respects_message_cap() {
        let dir = tempdir().unwrap();
        let cwd = dir.path().to_path_buf();
        let a = write_cline(dir.path(), "a.json", &cwd, 100);
        let b = write_cline(dir.path(), "b.json", &cwd, 100);
        let c = write_cline(dir.path(), "c.json", &cwd, 100);
        let mut state = ExternalAbsorptionState::default();
        let paths = vec![("cline", a), ("cline", b), ("cline", c)];
        let (absorbed, skipped) = absorb_paths(paths, &cwd, &mut state);
        assert_eq!(absorbed.len(), MAX_ABSORBED_MESSAGES, "capped");
        assert!(skipped >= 1, "at least one file skipped by the cap");
    }
}
