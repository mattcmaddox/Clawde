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

/// Scrub credentials from a block of text before it can become a model message.
///
/// Freebuff snapshots document the cluster's shared SSH password in plain text
/// (`sshpass "…"`, `password '…'`, `REMOTE_PW=…`) and SSH material lives in
/// files like `10-ssh.txt`. Since imported context is sent to third-party LLM
/// providers, anything on a line that looks like a secret, key, or token is
/// replaced with a redaction marker before it can leak. Deliberately
/// conservative: it may occasionally redact a non-secret, but it will not ship
/// a live credential to a model.
pub fn redact_secrets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        let sensitive = [
            "sshpass",
            "password",
            "passwd",
            "remote_pw",
            "private key",
            "begin openssh",
            "id_rsa",
            "id_ed25519",
            "api_key",
            "apikey",
            "secret",
            "token",
            "credential",
        ]
        .iter()
        .any(|k| lower.contains(k));
        if sensitive {
            out.push_str("[REDACTED — credential-like line omitted]\n");
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
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

/// Import a single Freebuff *capture run* for one host.
///
/// `path` is a run directory of the form
/// `~/freebuff/snapshots/<host>/<UTC ts>/` containing `_meta.txt`,
/// `_runner.txt`, and numbered context files (`01-identity.txt`, …). This
/// produces one compact, credential-redacted host-context card. The large
/// recon dumps (network/listeners/processes/mounts) are intentionally NOT
/// imported: they are tens of kilobytes each and low-signal for a coding
/// assistant. Identity + capture metadata is the useful, bounded subset.
pub fn import_freebuff_session(path: &Path) -> anyhow::Result<ImportedSession> {
    if !path.is_dir() {
        anyhow::bail!("freebuff run path is not a directory: {}", path.display());
    }
    // Run timestamp is the directory name; host is its parent.
    let run_ts = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();
    let host = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    // Parse a `key=value` meta file into a small map.
    fn kv(path: &Path) -> std::collections::HashMap<String, String> {
        let mut m = std::collections::HashMap::new();
        if let Ok(text) = std::fs::read_to_string(path) {
            for line in text.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    m.insert(k.trim().to_string(), v.trim().to_string());
                }
            }
        }
        m
    }
    let meta = kv(&path.join("_meta.txt"));
    let runner = kv(&path.join("_runner.txt"));

    // Bounded identity excerpt (hostname / OS / kernel), redacted.
    let mut identity = String::new();
    if let Ok(text) = std::fs::read_to_string(path.join("01-identity.txt")) {
        for line in text.lines().take(40) {
            identity.push_str(line);
            identity.push('\n');
        }
    }

    let mode = runner.get("mode").cloned().unwrap_or_default();
    let status = runner
        .get("capture_status")
        .cloned()
        .unwrap_or_else(|| "unknown".into());
    let os = meta
        .get("host_nick")
        .cloned()
        .unwrap_or_else(|| host.clone());

    let mut card = format!(
        "Freebuff host snapshot — host={host} run={run_ts}\n\
         capture_mode={mode} status={status} os_label={os}\n"
    );
    if !identity.trim().is_empty() {
        card.push_str("\n--- identity (excerpt) ---\n");
        card.push_str(&identity);
    }
    // NOTE: no redaction here — `import_one_source` redacts every source's
    // messages centrally before they can reach a model.

    let messages = if card.trim().is_empty() {
        Vec::new()
    } else {
        vec![Message {
            role: Role::Assistant,
            content: MessageContent::Text(card),
            uuid: None,
            cost: None,
            snapshot_patch: None,
            turn_meta: None,
        }]
    };

    // Freebuff snapshots are system context relevant to any project. Anchor the
    // working_dir at the snapshots root so the importer's cwd filter always
    // includes it (and treat it as unscoped host context).
    let working_dir = std::fs::canonicalize(path).ok().and_then(|p| {
        p.parent()
            .and_then(|q| q.parent())
            .map(|r| r.display().to_string())
    });

    Ok(ImportedSession {
        name: format!("freebuff-{host}-{run_ts}"),
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

/// A pluggable external-session source.
///
/// Each source is a self-contained descriptor: where its data lives, how to
/// cheaply discover candidate paths, and how to parse one path. The absorb /
/// history-restore core drives this registry generically, so adding, disabling,
/// or removing a source never requires touching the core machinery. Freebuff is
/// one isolated, **off-by-default** entry (its `~/freebuff` folder is agent
/// scratch, not a dependency Clawde should read by default). Opt in from
/// `settings.json` by listing it in `externalImportSources`
/// (e.g. `["opencode", "cline", "freebuff"]`) — no code change needed.
pub struct ExternalSource {
    /// Stable identifier, also used to look the source up in the registry.
    pub id: &'static str,
    /// Root directory where this source stores its data.
    pub root: PathBuf,
    /// System-scoped sources (host recon) are relevant to every project and
    /// bypass the cwd filter; chat-history sources are project-scoped.
    pub system_scoped: bool,
    /// Whether this source participates in the default (no-config) import.
    pub default_enabled: bool,
    /// Cheap discovery: candidate paths for this source, opening no file.
    pub discover: fn(&Path) -> Vec<PathBuf>,
    /// Parse a single discovered path into messages (or `None` if unusable).
    pub import: fn(&Path) -> Option<ImportedSession>,
}

/// The active external sources, in catalog order. Callers filter by
/// `default_enabled` (or an explicit allow-list) before discovering.
pub fn external_sources() -> Vec<ExternalSource> {
    let locs = KnownLocations::default();
    vec![
        ExternalSource {
            id: "opencode",
            root: locs.opencode_projects,
            system_scoped: false,
            default_enabled: true,
            discover: discover_opencode_paths,
            import: |p| import_opencode_session(p).ok(),
        },
        ExternalSource {
            id: "cline",
            root: locs.cline_workspace,
            system_scoped: false,
            default_enabled: true,
            discover: discover_cline_paths,
            import: |p| import_cline_session(p).ok(),
        },
        ExternalSource {
            id: "freebuff",
            root: locs.freebuff_snapshots,
            system_scoped: true,
            // Off by default: `~/freebuff` is agent scratch, not a normal
            // dependency. Opt in explicitly if you actually want host context.
            default_enabled: false,
            discover: discover_freebuff_paths,
            import: |p| import_freebuff_session(p).ok(),
        },
    ]
}

/// Look up a source descriptor by id.
pub fn external_source(id: &str) -> Option<ExternalSource> {
    external_sources().into_iter().find(|s| s.id == id)
}

/// Whether a source id participates in the default (no-config) import.
pub fn is_source_default_enabled(id: &str) -> bool {
    external_source(id)
        .map(|s| s.default_enabled)
        .unwrap_or(false)
}

/// Whether a source id refers to a system-scoped (always-relevant) source.
pub fn is_system_scoped_source(id: &str) -> bool {
    external_source(id)
        .map(|s| s.system_scoped)
        .unwrap_or(false)
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
    // System-scoped sources (e.g. host recon) are relevant to every project.
    // This is a property of the source in the registry, not a hardcoded name.
    if is_system_scoped_source(session.source) {
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

/// Freebuff: the per-host capture-run directories under
/// `<dir>/<host>/<UTC ts>/`, each holding `_meta.txt`, `_runner.txt`, and the
/// numbered context files. Returns those run directories (not the top-level
/// `*_index.md` report), because the run dirs are where the real per-host
/// snapshot data lives.
pub fn discover_freebuff_paths(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for host_entry in rd.flatten() {
        let host_dir = host_entry.path();
        if !host_dir.is_dir() {
            continue;
        }
        let Ok(runs) = std::fs::read_dir(&host_dir) else {
            continue;
        };
        for run in runs.flatten() {
            let run_dir = run.path();
            // A run dir is identified by containing _meta.txt.
            if run_dir.is_dir() && run_dir.join("_meta.txt").is_file() {
                out.push(run_dir);
            }
        }
    }
    out
}

/// All candidate external session paths across the *enabled* sources, as
/// `(source_id, path)`. Drives the [`ExternalSource`] registry generically and
/// honours each source's `default_enabled` flag, so a disabled source (e.g.
/// Freebuff) is invisible to the absorb path. Cheap: directory walks only, no
/// file is opened.
/// Discovery filtered by an explicit allow-list of source ids.
///
/// `allow` of `None` uses each source's `default_enabled`. `Some(ids)` is
/// exclusive: only the listed sources participate, which is how a user opts
/// into a non-default source (e.g. `freebuff`) from `settings.json` without a
/// code change and rebuild. Unknown ids are ignored (a typo degrades to
/// importing nothing rather than erroring at startup).
pub fn discover_all_external_paths_for(allow: Option<&[String]>) -> Vec<(&'static str, PathBuf)> {
    let mut out = Vec::new();
    for source in external_sources() {
        let enabled = match allow {
            Some(ids) => ids.iter().any(|id| id == source.id),
            None => source.default_enabled,
        };
        if !enabled {
            continue;
        }
        for p in (source.discover)(&source.root) {
            out.push((source.id, p));
        }
    }
    out
}

/// Parse a single external session path via the registry, then redact every
/// imported text message before it can become part of a model request. Returns
/// `None` if the file cannot be parsed into any messages. Redaction is applied
/// centrally here — not per-source — so *every* source (not just Freebuff) is
/// protected from leaking credentials to a provider.
pub fn import_one_source(source: &str, path: &Path) -> Option<ImportedSession> {
    let src = external_source(source)?;
    let mut session = (src.import)(path)?;
    if session.messages.is_empty() {
        return None;
    }
    for msg in &mut session.messages {
        if let MessageContent::Text(text) = &mut msg.content {
            *text = redact_secrets(text);
        }
    }
    Some(session)
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
    fn imports_freebuff_run_dir_as_redacted_host_card() {
        let dir = tempdir().unwrap();
        let run = dir.path().join("drone").join("20260629T023103Z");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(
            run.join("_meta.txt"),
            "host_nick=drone\nsnapshot_ts=2026-06-29T02:31:03Z\n",
        )
        .unwrap();
        std::fs::write(
            run.join("_runner.txt"),
            "whoami=churl\nhost_nick=drone\nmode=local\ncapture_status=ok\n",
        )
        .unwrap();
        // Identity plus a line that must be redacted before reaching a model.
        std::fs::write(
            run.join("01-identity.txt"),
            "## hostname/OS\nTheDrone\nPRETTY_NAME=\"Ubuntu 24.04\"\nsshpass \"supersecretpw\" via ssh hive\n",
        )
        .unwrap();

        let session = import_one_source("freebuff", &run).expect("freebuff import via registry");
        assert_eq!(session.source, "freebuff");
        assert_eq!(session.messages.len(), 1);
        let text = session.messages[0].get_text().unwrap_or("");
        assert!(text.contains("host=drone"), "card names the host: {text}");
        assert!(text.contains("TheDrone"), "identity present: {text}");
        // Central redaction in import_one_source: the shared password must never
        // survive into a model message.
        assert!(!text.contains("supersecretpw"), "credential leaked: {text}");
        assert!(!text.contains("sshpass"), "sshpass line leaked: {text}");
        assert!(text.contains("REDACTED"), "redaction marker present");
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
        // freebuff: per-host run dirs containing _meta.txt (not the top-level index)
        let fb = tempdir().unwrap();
        let run = fb.path().join("drone").join("20260629T023103Z");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join("_meta.txt"), "host_nick=drone\n").unwrap();
        // A top-level index file is NOT a discoverable run source anymore.
        std::fs::write(fb.path().join("20260629T023103Z_index.md"), "# x").unwrap();
        let found = discover_freebuff_paths(fb.path());
        assert_eq!(found.len(), 1, "only the run dir is discovered");
        assert!(found[0].ends_with("drone/20260629T023103Z"));
        // A missing dir yields nothing.
        assert!(discover_opencode_paths(&oc.path().join("nope")).is_empty());
    }

    #[test]
    fn freebuff_is_off_by_default_in_discovery() {
        // The registry must not discover Freebuff unless explicitly enabled:
        // `~/freebuff` is agent scratch, not a normal dependency.
        assert!(
            !is_source_default_enabled("freebuff"),
            "freebuff off by default"
        );
        assert!(is_source_default_enabled("opencode"));
        assert!(is_source_default_enabled("cline"));
        // The source still exists and is system-scoped (opt-in-able), just not on.
        assert!(is_system_scoped_source("freebuff"));
    }

    #[test]
    fn central_redaction_covers_opencode_and_cline() {
        // Redaction lives in import_one_source, so even chat sources are
        // protected from leaking a pasted credential to a model provider.
        let dir = tempdir().unwrap();
        let oc = dir.path().join("proj_hash");
        std::fs::create_dir_all(&oc).unwrap();
        let file = oc.join("history.json");
        std::fs::write(
            &file,
            r#"[{"role":"user","content":"my sshpass \"topsecret123\" for hive"}]"#,
        )
        .unwrap();
        let s = import_one_source("opencode", &file).expect("opencode import");
        let text = s.messages[0].get_text().unwrap_or("");
        assert!(
            !text.contains("topsecret123"),
            "opencode credential leaked: {text}"
        );
        assert!(
            text.contains("REDACTED"),
            "opencode redaction marker present"
        );
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

    #[test]
    fn allow_list_opts_in_a_non_default_source() {
        // Freebuff is off by default; an allow-list that names it must enable it
        // and exclude sources that were not named.
        let allow = vec!["freebuff".to_string()];
        let ids: std::collections::BTreeSet<&'static str> =
            discover_all_external_paths_for(Some(&allow))
                .into_iter()
                .map(|(id, _)| id)
                .collect();
        // On a machine with no freebuff data this is simply empty, which is
        // still correct: no unlisted source may appear.
        assert!(
            ids.iter().all(|id| *id == "freebuff"),
            "only allow-listed sources may be discovered, got {ids:?}"
        );
    }

    #[test]
    fn allow_list_excludes_unlisted_sources() {
        let allow = vec!["cline".to_string()];
        let ids: std::collections::BTreeSet<&'static str> =
            discover_all_external_paths_for(Some(&allow))
                .into_iter()
                .map(|(id, _)| id)
                .collect();
        assert!(
            !ids.contains("opencode"),
            "unlisted default-enabled source leaked into discovery: {ids:?}"
        );
    }

    #[test]
    fn unknown_allow_list_ids_are_ignored() {
        // A typo must degrade to importing nothing, not error at startup.
        let allow = vec!["not-a-source".to_string()];
        assert!(discover_all_external_paths_for(Some(&allow)).is_empty());
    }
}
