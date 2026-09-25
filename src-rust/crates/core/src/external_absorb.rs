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

/// Compute a content fingerprint for a path, or `None` if it cannot be read.
///
/// * A **file** is hashed by its bytes.
/// * A **directory** (a Freebuff chat dir) is hashed from a stable
///   manifest of its immediate entries' `(name, size, mtime)`, so an unchanged
///   run is skipped cheaply and a changed run is re-absorbed. This lets the
///   fingerprint-before-parse flow operate on directory-based sources too.
///
/// Returning `None` (rather than hashing empty bytes) keeps an unreadable path
/// distinct from a genuinely empty one.
fn fingerprint_of_path(path: &Path) -> Option<String> {
    let mut hasher = Sha256::new();
    if path.is_dir() {
        let mut entries: Vec<(String, u64, u64)> = Vec::new();
        for entry in std::fs::read_dir(path).ok()?.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let meta = entry.metadata().ok();
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let mtime = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            entries.push((name, size, mtime));
        }
        entries.sort();
        for (name, size, mtime) in entries {
            hasher.update(name.as_bytes());
            hasher.update(size.to_le_bytes());
            hasher.update(mtime.to_le_bytes());
        }
    } else {
        hasher.update(&std::fs::read(path).ok()?);
    }
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
    // Newest first so the cap deterministically keeps recent history. For a
    // directory source (a Freebuff chat dir) use the newest child mtime, which
    // reflects the actual capture time.
    fn newest_mtime(p: &Path) -> std::time::SystemTime {
        let Ok(md) = std::fs::metadata(p) else {
            return std::time::UNIX_EPOCH;
        };
        if !md.is_dir() {
            return md.modified().unwrap_or(std::time::UNIX_EPOCH);
        }
        let mut newest = md.modified().unwrap_or(std::time::UNIX_EPOCH);
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                if let Ok(t) = e.metadata().and_then(|m| m.modified()) {
                    if t > newest {
                        newest = t;
                    }
                }
            }
        }
        newest
    }
    paths.sort_by(|a, b| {
        let ma = newest_mtime(&a.1);
        let mb = newest_mtime(&b.1);
        ma.cmp(&mb).reverse().then_with(|| a.1.cmp(&b.1))
    });

    let mut absorbed: Vec<Message> = Vec::new();
    let mut skipped = 0usize;
    // Oversized-session truncation is summarized once, after the loop: one
    // warning per chat would bury the TUI under a wall of noise when a project
    // has many long transcripts.
    let mut truncated = 0usize;
    let mut dropped_turns = 0usize;
    for (source, path) in paths {
        let key = path.to_string_lossy().to_string();
        // Fingerprint first: an unchanged source is skipped WITHOUT parsing.
        let Some(fingerprint) = fingerprint_of_path(&path) else {
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
            truncated += 1;
            dropped_turns += drop;
            m
        } else {
            session.messages
        };
        // Role boundaries only make sense for imported *conversation* history.
        // System-scoped sources (host recon) emit a single assistant-role
        // context card rather than a dialogue, so trimming their boundaries
        // would delete the import entirely. Framing keeps the request valid.
        let msgs = if crate::session_import::is_system_scoped_source(source) {
            frame_system_context(msgs)
        } else {
            normalize_imported_boundaries(msgs)
        };
        absorbed.extend(msgs);
        state.sessions.insert(key, AbsorbedEntry { fingerprint });
    }
    if truncated > 0 {
        tracing::warn!(
            sessions = truncated,
            dropped_turns,
            "Some external sessions exceeded the import cap; only their most \
             recent turns were imported."
        );
    }
    (absorbed, skipped)
}

/// Wrap system-scoped host context so the combined history stays valid.
///
/// Host-context cards are model-authored prose (an `Assistant` turn), and they
/// are prepended ahead of the real user prompt. A request whose *first* message
/// is an assistant turn is rejected by Anthropic, so the block is framed with a
/// short synthetic user turn. The result is
/// `user(frame) -> assistant(card) -> user(real prompt)`, which satisfies both
/// the "first message must be user" rule and the no-consecutive-user-turns rule.
fn frame_system_context(msgs: Vec<Message>) -> Vec<Message> {
    use crate::types::Role;
    if msgs.is_empty() {
        return msgs;
    }
    let mut out = Vec::with_capacity(msgs.len() + 1);
    out.push(crate::types::Message::user(
        "Host context captured from this machine (imported automatically):",
    ));
    out.extend(msgs);
    // A host-context block must not END on a user turn either, or it would sit
    // adjacent to the real prompt. Trim the card list (never the frame) so the
    // frame->card->prompt shape is preserved.
    while out.len() > 1 && out.last().is_some_and(|m| m.role == Role::User) {
        out.pop();
    }
    out
}

/// Trim imported messages so they can be safely **prepended** to the live
/// conversation without breaking provider request validation or re-triggering
/// the "small model answers the stale question" bug:
///
/// * drop leading `Assistant` turns — the first message of an Anthropic /
///   OpenAI request must be `user`, and `sanitize_history` does not repair a
///   leading assistant (it passes it through);
/// * drop a trailing dangling `User` turn — the real user prompt follows the
///   imported block, and two consecutive user turns get merged (documented
///   stale-answer hazard).
///
/// This is a light touch: it only trims the boundaries, never rewrites content
/// or merges distinct user messages.
fn normalize_imported_boundaries(msgs: Vec<Message>) -> Vec<Message> {
    use crate::types::Role;
    let mut msgs = msgs;
    // Drop leading assistant turns.
    msgs.drain(
        ..msgs
            .iter()
            .take_while(|m| m.role == Role::Assistant)
            .count(),
    );
    // Drop a trailing dangling user turn.
    while msgs.last().is_some_and(|m| m.role == Role::User) {
        msgs.pop();
    }
    msgs
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
///      any source.
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
    absorb_new_external_sessions_for(cwd, None)
}

/// As [`absorb_new_external_sessions`], but with an explicit allow-list of
/// external source ids. `Some(ids)` is exclusive; `None` uses each source's
/// default-enabled flag.
pub fn absorb_new_external_sessions_for(
    cwd: &Path,
    allow: Option<&[String]>,
) -> (Vec<Message>, AbsorbCommit) {
    let project = crate::git_utils::project_root(cwd);
    let transcript_bucket = crate::session_storage::transcript_dir(&project);
    let state_path = transcript_bucket.join("external_import_state.json");
    let mut state = load_state(&state_path);

    let cwd_canonical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let paths = crate::session_import::discover_all_external_paths_for(allow);
    let candidate_count = paths.len();

    // Per-session boundary normalization happens inside absorb_paths, so that
    // system-scoped host context is exempt (see the call site there).
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
        let a = fingerprint_of_path(&file).unwrap();
        std::fs::write(&file, "new").unwrap();
        let b = fingerprint_of_path(&file).unwrap();
        assert_ne!(a, b);
        // A missing file has no fingerprint (distinct from empty content).
        assert!(fingerprint_of_path(&dir.path().join("missing")).is_none());
    }

    #[test]
    fn fingerprint_of_directory_tracks_manifest() {
        let dir = tempdir().unwrap();
        let sub = dir.path().join("run");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("_meta.txt"), "host_nick=drone").unwrap();
        let a = fingerprint_of_path(&sub).unwrap();
        // Stable across calls when nothing changed.
        assert_eq!(a, fingerprint_of_path(&sub).unwrap());
        // Adding a file changes the manifest fingerprint.
        std::fs::write(sub.join("01-identity.txt"), "TheDrone").unwrap();
        assert_ne!(a, fingerprint_of_path(&sub).unwrap());
    }

    /// Write a valid Cline session.json for `workspace` with `n` messages that
    /// alternate user/assistant, the way a real transcript does. A block of
    /// same-role messages is not representative and would be trimmed by
    /// `normalize_imported_boundaries`.
    fn write_cline(dir: &Path, name: &str, workspace: &Path, n: usize) -> PathBuf {
        let file = dir.join(name);
        let msgs: Vec<String> = (0..n)
            .map(|i| {
                let role = if i % 2 == 0 { "user" } else { "assistant" };
                format!(r#"{{"role":"{role}","text":"m{i}"}}"#)
            })
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
        // 3 alternating messages = user/assistant/user; the trailing dangling
        // user turn is trimmed so it cannot merge with the real prompt.
        let file = write_cline(dir.path(), "s.json", &cwd, 3);
        let mut state = ExternalAbsorptionState::default();
        let paths = vec![("cline", file.clone())];
        let (absorbed, skipped) = absorb_paths(paths.clone(), &cwd, &mut state);
        assert_eq!(
            absorbed.len(),
            2,
            "relevant session absorbed, trailing user trimmed"
        );
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

    fn m(role: crate::types::Role) -> Message {
        Message {
            role,
            content: crate::types::MessageContent::Text("x".into()),
            uuid: None,
            cost: None,
            snapshot_patch: None,
            turn_meta: None,
        }
    }

    #[test]
    fn normalize_drops_leading_assistant_and_trailing_user() {
        use crate::types::Role;
        // Leading assistant (invalid first message) + trailing dangling user
        // (would merge with the real prompt).
        let msgs = vec![
            m(Role::Assistant),
            m(Role::Assistant),
            m(Role::User),
            m(Role::Assistant),
            m(Role::User),
        ];
        let out = normalize_imported_boundaries(msgs);
        assert_eq!(out.len(), 2, "dropped leading assistants + trailing user");
        assert_eq!(out[0].role, Role::User, "starts on a user turn");
        assert_eq!(out[1].role, Role::Assistant);
    }

    #[test]
    fn system_host_context_is_framed_not_dropped() {
        use crate::types::Role;
        // A host-context card is assistant-authored. It must survive (it is not
        // a conversation to trim) and be framed so the request starts on a user
        // turn.
        let card = vec![m(Role::Assistant)];
        let out = frame_system_context(card);
        assert_eq!(out.len(), 2, "frame + card");
        assert_eq!(out[0].role, Role::User, "request must start on a user turn");
        assert_eq!(out[1].role, Role::Assistant, "card preserved");
    }

    #[test]
    fn normalize_keeps_a_well_formed_block() {
        use crate::types::Role;
        let msgs = vec![m(Role::User), m(Role::Assistant)];
        let out = normalize_imported_boundaries(msgs);
        assert_eq!(out.len(), 2, "well-formed block untouched");
        assert_eq!(out[0].role, Role::User);
    }
}
