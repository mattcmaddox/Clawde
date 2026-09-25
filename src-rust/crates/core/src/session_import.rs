// Cross-app session history importers for Clawde.
//
// This module provides parsers to import conversation history from external
// coding agents (Opencode, Cline, Freebuff) into Clawde's session format. The
// importers read from verified device paths:
//
// - Opencode:    ~/.local/share/opencode/projects/{project_hash}/history.json
// - Cline:       ~/.config/Code/User/workspaceStorage/{ws_hash}/roben.cline/session.json
// - Freebuff:    ~/freebuff/snapshots/{date}/recon.json (+ snapshot.txt)
//
// Each importer produces a list of Clawde `Message` objects plus metadata
// (working directory, original source) suitable for creating a new
// `ConversationSession`.

use crate::types::{Message, MessageContent, Role};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Result returned by an importer: a session's worth of messages plus
/// metadata captured from the source format.
#[derive(Debug, Clone)]
pub struct ImportedSession {
    /// Human-readable name for the external session (from its file/title).
    pub name: String,
    /// Absolute path of the project/workspace the original session ran in.
    pub working_dir: Option<String>,
    /// Original source app identifier (e.g. "opencode", "cline").
    pub source: &'static str,
    /// Source file path for reference/debugging.
    pub source_path: PathBuf,
    /// Ordered conversation messages in Clawde's role/content model.
    pub messages: Vec<Message>,
}

// ---------------------------------------------------------------------------
// Opencode importer
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct OpencodeMessage {
    role: String,
    #[serde(default)]
    content: Option<String>,
    /// Some Opencode entries nest text under `parts`.
    #[serde(default)]
    parts: Option<Vec<OpencodePart>>,
}

#[derive(Debug, Deserialize)]
struct OpencodePart {
    #[serde(default, rename = "type")]
    part_type: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

/// Import a single Opencode `history.json` file.
pub fn import_opencode_session(path: &Path) -> anyhow::Result<ImportedSession> {
    let raw = std::fs::read_to_string(path)?;
    let messages_raw: Vec<OpencodeMessage> = serde_json::from_str(&raw)?;

    let parent = path.parent().map(|p| p.to_path_buf());
    let project_hash = parent
        .as_ref()
        .and_then(|p| p.file_name().and_then(|s| s.to_str()))
        .unwrap_or("unknown")
        .to_string();

    let mut messages = Vec::new();
    for m in &messages_raw {
        let role = match m.role.to_lowercase().as_str() {
            "user" | "human" => Role::User,
            "assistant" | "model" => Role::Assistant,
            _ => continue,
        };
        let content = m
            .content
            .clone()
            .or_else(|| {
                m.parts.as_ref().and_then(|parts| {
                    parts
                        .iter()
                        .find(|p| p.part_type.as_deref() == Some("text"))
                        .and_then(|p| p.text.clone())
                })
            })
            .unwrap_or_default();
        if content.trim().is_empty() {
            continue;
        }
        messages.push(Message {
            role,
            content: MessageContent::Text(content),
            uuid: None,
            cost: None,
            snapshot_patch: None,
            turn_meta: None,
        });
    }

    let working_dir = None::<String>; // Opencode history.json doesn't store cwd

    Ok(ImportedSession {
        name: format!("opencode-{}", &project_hash[..project_hash.len().min(12)]),
        working_dir,
        source: "opencode",
        source_path: path.to_path_buf(),
        messages,
    })
}

// ---------------------------------------------------------------------------
// Cline importer
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ClineSession {
    #[serde(default, rename = "workspacePath")]
    workspace_path: Option<String>,
    #[serde(default)]
    conversation: Option<Vec<ClineMessage>>,
}

#[derive(Debug, Deserialize)]
struct ClineMessage {
    role: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    content: Option<String>,
}

/// Import a single Cline `session.json` file.
pub fn import_cline_session(path: &Path) -> anyhow::Result<ImportedSession> {
    let raw = std::fs::read_to_string(path)?;
    let session: ClineSession = serde_json::from_str(&raw)?;

    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("cline-session")
        .to_string();

    let mut messages = Vec::new();
    if let Some(conv) = &session.conversation {
        for m in conv {
            let role = match m.role.to_lowercase().as_str() {
                "user" | "human" => Role::User,
                "assistant" => Role::Assistant,
                _ => continue,
            };
            let content = m
                .text
                .as_deref()
                .or(m.content.as_deref())
                .unwrap_or("")
                .to_string();
            if content.trim().is_empty() {
                continue;
            }
            messages.push(Message {
                role,
                content: MessageContent::Text(content),
                uuid: None,
                cost: None,
                snapshot_patch: None,
                turn_meta: None,
            });
        }
    }

    Ok(ImportedSession {
        name,
        working_dir: session.workspace_path,
        source: "cline",
        source_path: path.to_path_buf(),
        messages,
    })
}

// ---------------------------------------------------------------------------
// Freebuff (system reconnaissance snapshots) importer
// ---------------------------------------------------------------------------

/// Freebuff stores periodic system-state snapshots as `YYYYMMDDTHHMMSS_index.md`
/// markdown index files under `~/freebuff/snapshots/`. Each index file is a
/// human-readable report of captured host/network/config state for a category
/// (e.g. `drone`, `hive`). We treat each index as a single Assistant "snapshot"
/// message, since it represents a point-in-time system context a developer
/// might want Clawde to be aware of.
/// Parse a Freebuff `*_index.md` file into a single Assistant message.
///
/// If the markdown file begins with `# <title>`, that title is used; otherwise
/// the filename stem is. The remainder of the file body becomes message text,
/// prefixed with a `Source: freebuff` header so the model can tell it apart.
pub fn import_freebuff_session(path: &Path) -> anyhow::Result<ImportedSession> {
    let raw = std::fs::read_to_string(path)?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("freebuff-session")
        .to_string();

    // Pull a title from a leading "# ..." line if present.
    let mut title = stem.clone();
    let mut body_start = 0usize;
    for (i, line) in raw.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("# ") {
            title = trimmed.trim_start_matches("# ").trim().to_string();
            body_start = i + 1;
        } else if trimmed.is_empty() {
            continue;
        } else {
            break;
        }
    }
    let body = if body_start < raw.lines().count() {
        raw.lines()
            .skip(body_start)
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string()
    } else {
        String::new()
    };

    let content_text = format!("Source: freebuff ({})\nTitle: {}\n\n{}", stem, title, body);

    let messages = if content_text.trim().is_empty() || body.is_empty() {
        Vec::new()
    } else {
        vec![Message {
            role: Role::Assistant,
            content: MessageContent::Text(content_text),
            uuid: None,
            cost: None,
            snapshot_patch: None,
            turn_meta: None,
        }]
    };

    // Freebuff snapshots are captured for a project directory.
    // The freebuff tool itself runs from ~/freebuff, but snapshots are
    // intended to represent system state that may be relevant to any
    // project the user is working on. We set working_dir to the freebuff
    // snapshots root so it can be matched against the cwd during filtering.
    let working_dir = std::fs::canonicalize(path).ok().and_then(|p| {
        let parent = p.parent()?;
        let grand = parent.parent()?;
        Some(grand.display().to_string())
    });

    let project_hash = stem.split('_').next().unwrap_or("unknown").to_string();

    Ok(ImportedSession {
        name: format!("freebuff-{}", &project_hash[..project_hash.len().min(12)]),
        working_dir,
        source: "freebuff",
        source_path: path.to_path_buf(),
        messages,
    })
}

// ---------------------------------------------------------------------------
// Unified discovery helpers
// ---------------------------------------------------------------------------

/// Where each external app stores its session files on this Linux device.
pub struct KnownLocations {
    pub opencode_projects: PathBuf,
    pub cline_workspace: PathBuf,
    pub freebuff_snapshots: PathBuf,
}

impl Default for KnownLocations {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
        KnownLocations {
            opencode_projects: home
                .join(".local")
                .join("share")
                .join("opencode")
                .join("projects"),
            cline_workspace: home
                .join(".config")
                .join("Code")
                .join("User")
                .join("workspaceStorage"),
            freebuff_snapshots: home.join("freebuff").join("snapshots"),
        }
    }
}

/// Filter a list of imported sessions to those relevant to `cwd`.
///
/// - Sessions whose `working_dir` matches `cwd` or its ancestor/descendant are
///   included (this covers Cline sessions that record a `workspacePath`).
/// - **Freebuff** snapshots are system-wide context, so they are always
///   included (a developer running in a project still benefits from the most
///   recent host/network recon).
/// - Sessions with **no** `working_dir` (e.g. Opencode, whose `history.json`
///   does not record a cwd) are **excluded**. They cannot be attributed to a
///   project, and including them would prepend every Opencode session from
///   every project on the machine into whatever Clawde session happens to be
///   running — a cross-project history leak. Resolving Opencode's per-project
///   directory hash back to a real path (so it can be scoped) is a follow-up;
///   until then, scoping beats coverage.
pub fn filter_sessions_by_cwd(sessions: Vec<ImportedSession>, cwd: &Path) -> Vec<ImportedSession> {
    let cwd_canonical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    sessions
        .into_iter()
        .filter(|s| session_relevant_to_cwd(s, &cwd_canonical))
        .collect()
}

/// Whether a single imported session is relevant to the (already canonicalized)
/// `cwd_canonical`. Shared by the bulk filter and the fingerprint-before-parse
/// absorber so both apply identical scoping rules.
pub fn session_relevant_to_cwd(session: &ImportedSession, cwd_canonical: &Path) -> bool {
    // Freebuff snapshots are system-wide context; always include them.
    if session.source == "freebuff" {
        return true;
    }
    // Sessions without working_dir info cannot be scoped to a project. Exclude
    // them rather than leaking unrelated projects' history.
    let Some(wd) = session.working_dir.as_ref() else {
        return false;
    };
    std::fs::canonicalize(wd)
        .ok()
        .map(|wd| {
            wd == cwd_canonical || cwd_canonical.starts_with(&wd) || wd.starts_with(cwd_canonical)
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Path-only discovery (fingerprint-before-parse)
//
// These locate candidate session FILES without opening/parsing them. The
// absorber uses them to fingerprint each file first and skip unchanged ones
// without paying the parse cost — the whole point of the "absorb once, never
// re-poll old as new" design. Parsing happens only for new/changed files, via
// `import_one_source`.
// ---------------------------------------------------------------------------

/// Opencode: `<dir>/{project_hash}/history.json`.
pub fn discover_opencode_paths(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in rd.flatten() {
        let history = entry.path().join("history.json");
        if history.is_file() {
            out.push(history);
        }
    }
    out
}

/// Cline: `<dir>/{ws_hash}/roben.cline/session.json`.
pub fn discover_cline_paths(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in rd.flatten() {
        let session = entry.path().join("roben.cline").join("session.json");
        if session.is_file() {
            out.push(session);
        }
    }
    out
}

/// Freebuff: `<dir>/*_index.md` snapshot reports.
pub fn discover_freebuff_paths(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let is_index = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.contains("_index"))
            .unwrap_or(false);
        if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("md") && is_index {
            out.push(path);
        }
    }
    out
}

/// All candidate external session files across every source, as
/// `(source_id, path)`. Cheap: directory walks only, no file is opened.
pub fn discover_all_external_paths() -> Vec<(&'static str, PathBuf)> {
    let locs = KnownLocations::default();
    let mut out = Vec::new();
    for p in discover_opencode_paths(&locs.opencode_projects) {
        out.push(("opencode", p));
    }
    for p in discover_cline_paths(&locs.cline_workspace) {
        out.push(("cline", p));
    }
    for p in discover_freebuff_paths(&locs.freebuff_snapshots) {
        out.push(("freebuff", p));
    }
    out
}

/// Parse a single external session file, dispatching on `source`. Returns `None`
/// if the file cannot be parsed into any messages.
pub fn import_one_source(source: &str, path: &Path) -> Option<ImportedSession> {
    let session = match source {
        "opencode" => import_opencode_session(path).ok()?,
        "cline" => import_cline_session(path).ok()?,
        "freebuff" => import_freebuff_session(path).ok()?,
        _ => return None,
    };
    if session.messages.is_empty() {
        None
    } else {
        Some(session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn imports_opencode_session() {
        let dir = tempdir().unwrap();
        let proj_dir = dir.path().join("opencode_abc123");
        std::fs::create_dir_all(&proj_dir).unwrap();
        let file = proj_dir.join("history.json");
        let content = r#"[
            {"role": "user", "content": "what's up"},
            {"role": "assistant", "parts": [{"type": "text", "text": "not much"}]}
        ]"#;
        std::fs::write(&file, content).unwrap();

        let session = import_opencode_session(&file).unwrap();
        assert_eq!(session.source, "opencode");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].get_text(), Some("what's up"));
    }

    #[test]
    fn imports_cline_session() {
        let dir = tempdir().unwrap();
        let ws_dir = dir.path().join("ws-123").join("roben.cline");
        std::fs::create_dir_all(&ws_dir).unwrap();
        let file = ws_dir.join("session.json");
        let content = r#"{
            "workspacePath": "/home/test/cline-project",
            "conversation": [
                {"role": "user", "text": "hi"},
                {"role": "assistant", "content": "hello"}
            ]
        }"#;
        std::fs::write(&file, content).unwrap();

        let session = import_cline_session(&file).unwrap();
        assert_eq!(session.source, "cline");
        assert_eq!(
            session.working_dir.as_deref(),
            Some("/home/test/cline-project")
        );
        assert_eq!(session.messages.len(), 2);
    }

    #[test]
    fn imports_freebuff_session() {
        let dir = tempdir().unwrap();
        let snapshots_dir = dir.path().join("snapshots");
        std::fs::create_dir_all(&snapshots_dir).unwrap();
        let file = snapshots_dir.join("20260629T023103Z_index.md");
        let content = "# Drone Capture Report\n\n| Host | Path | Status |\n|------|------|--------|\n| drone | OK |\n";
        std::fs::write(&file, content).unwrap();

        let session = import_freebuff_session(&file).unwrap();
        assert_eq!(session.source, "freebuff");
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].role, Role::Assistant);
        let text = session.messages[0].get_text().unwrap_or("");
        assert!(text.contains("Source: freebuff"));
        assert!(text.contains("Title: Drone Capture Report"));
    }

    #[test]
    fn path_discovery_finds_candidate_files() {
        // opencode: <dir>/{project}/history.json
        let oc = tempdir().unwrap();
        let proj = oc.path().join("proj_hash");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("history.json"), "[]").unwrap();
        assert_eq!(discover_opencode_paths(oc.path()).len(), 1);
        // cline: <dir>/{ws}/roben.cline/session.json
        let cl = tempdir().unwrap();
        let ws = cl.path().join("ws1");
        std::fs::create_dir_all(ws.join("roben.cline")).unwrap();
        std::fs::write(ws.join("roben.cline").join("session.json"), "{}").unwrap();
        assert_eq!(discover_cline_paths(cl.path()).len(), 1);
        // freebuff: <dir>/<ts>_index.md
        let fb = tempdir().unwrap();
        std::fs::write(fb.path().join("20260101T000000Z_index.md"), "# x").unwrap();
        std::fs::write(fb.path().join("notes.md"), "ignored").unwrap();
        assert_eq!(discover_freebuff_paths(fb.path()).len(), 1);
        // A missing dir yields nothing.
        assert!(discover_opencode_paths(&oc.path().join("nope")).is_empty());
    }

    fn session(source: &'static str, working_dir: Option<String>) -> ImportedSession {
        ImportedSession {
            name: "s".to_string(),
            working_dir,
            source,
            source_path: PathBuf::from("/tmp/s"),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("x".to_string()),
                uuid: None,
                cost: None,
                snapshot_patch: None,
                turn_meta: None,
            }],
        }
    }

    #[test]
    fn filter_excludes_unscopable_sessions_to_prevent_cross_project_leak() {
        let dir = tempdir().unwrap();
        let cwd = dir.path();
        let other = tempdir().unwrap();
        let sessions = vec![
            // Unscopable (e.g. Opencode: no cwd) -> EXCLUDED, even though it has
            // messages, so it cannot leak an unrelated project's history.
            session("opencode", None),
            // Same project -> included.
            session("cline", Some(cwd.display().to_string())),
            // Different project -> excluded.
            session("cline", Some(other.path().display().to_string())),
            // System-wide snapshots -> always included.
            session("freebuff", Some("/definitely/not/cwd".to_string())),
        ];
        let kept = filter_sessions_by_cwd(sessions, cwd);
        let kept_sources: Vec<&str> = kept.iter().map(|s| s.source).collect();
        assert!(
            !kept_sources.contains(&"opencode"),
            "unscopable session must not be imported (cross-project leak)"
        );
        assert!(kept_sources.contains(&"freebuff"));
        // Exactly the cwd-matching cline session + freebuff survive.
        assert_eq!(kept.len(), 2, "kept: {kept_sources:?}");
    }
}
