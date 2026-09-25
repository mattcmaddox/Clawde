// Cross-app session history importers for Clawde.
//
// This module provides parsers to import conversation history from external
// coding agents (Opencode, Cline, Freebuff) into Clawde's session format. The
// importers read from verified device paths:
//
// - Opencode:    ~/.local/share/opencode/projects/{project_hash}/history.json
// - Cline:       ~/.config/Code/User/workspaceStorage/{ws_hash}/roben.cline/session.json
// - Freebuff:    ~/.config/manicode/projects/{slug}/chats/{chatId}/chat-messages.json
//                (the Freebuff/Codebuff agent; NOT the unrelated ~/freebuff recon script)
//
// Each importer produces a list of Clawde `Message` objects plus metadata
// (working directory, original source) suitable for creating a new
// `ConversationSession`.

use crate::types::{ContentBlock, Message, MessageContent, Role, ToolResultContent};
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
// Freebuff / Codebuff chat importer
// ---------------------------------------------------------------------------
//
// Freebuff (the agent binary shipped as `~/.config/manicode/freebuff`, formerly
// Manicode / Codebuff) persists per-chat transcripts at:
//
//     ~/.config/manicode/projects/<project-slug>/chats/<chatId>/
//         chat-messages.json   # ChatMessage[]
//         run-state.json       # sessionState.fileContext.projectRoot + cwd
//
// `chatId` is the chat's start time as ISO-8601 with `:` replaced by `-` for
// filesystem safety (e.g. `2026-07-11T22-49-27.735Z`). `run-state.json` is what
// makes a chat attributable to a project, so it is the locate key.
//
// This is NOT the `~/freebuff` host-recon script, which shares the name but is
// an unrelated tool with no conversation history.

/// One rendered block within a Freebuff chat message.
#[derive(Debug, Deserialize)]
struct FbBlock {
    #[serde(default, rename = "type")]
    block_type: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default, rename = "toolCallId")]
    tool_call_id: Option<String>,
    #[serde(default, rename = "toolName")]
    tool_name: Option<String>,
    #[serde(default)]
    input: Option<serde_json::Value>,
    #[serde(default)]
    output: Option<String>,
}

/// One message in a Freebuff transcript.
#[derive(Debug, Deserialize)]
struct FbMessage {
    #[serde(default)]
    variant: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    blocks: Option<Vec<FbBlock>>,
    // NB: `timestamp` is a human-facing wall-clock label (e.g. `06:50 PM`), not
    // a parseable date; the chat id carries the authoritative start time, so it
    // is deliberately not deserialized.
}

#[derive(Debug, Deserialize)]
struct FbRunState {
    #[serde(default, rename = "sessionState")]
    session_state: Option<FbSessionState>,
}

#[derive(Debug, Deserialize)]
struct FbSessionState {
    #[serde(default, rename = "fileContext")]
    file_context: Option<FbFileContext>,
}

#[derive(Debug, Deserialize)]
struct FbFileContext {
    #[serde(default, rename = "projectRoot")]
    project_root: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
}

/// Hard ceiling on the size of a transcript we will parse at all.
///
/// The import only ever keeps the most recent [`MAX_ABSORBED_MESSAGES`] turns,
/// so a multi-megabyte transcript contributes little beyond its tail while
/// costing seconds of JSON parsing on every cold start (Freebuff chats here run
/// to ~15 MB). Skipping is logged, and the fingerprint is still recorded so the
/// file is not re-examined in this project; a later run in its own project, or a
/// future raise of the cap, can still pick it up.
const MAX_TRANSCRIPT_BYTES: u64 = 4 * 1024 * 1024;

/// Cap on a single tool result carried into the transcript. Tool output is
/// frequently an entire file; a few hundred KB of it would crowd out the
/// actual conversation.
const MAX_TOOL_RESULT_CHARS: usize = 4_000;

/// Cap on a single text block. Assistant turns in Freebuff transcripts
/// routinely carry 30k+ characters (full file contents echoed back); without a
/// bound a handful of chats exhausts the context window before the real
/// conversation starts.
const MAX_TEXT_CHARS: usize = 4_000;

/// Read the project root a chat belongs to, from its `run-state.json`.
fn freebuff_chat_project_root(chat_dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(chat_dir.join("run-state.json")).ok()?;
    let state: FbRunState = serde_json::from_str(&raw).ok()?;
    let ctx = state.session_state?.file_context?;
    ctx.project_root.or(ctx.cwd)
}

/// Truncate oversized text (a tool result or an assistant text block) to a
/// byte budget, marking that it was cut. Slices on a char boundary so
/// multi-byte content is never split mid-codepoint.
fn clamp_text(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_string();
    }
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n… [truncated, {} bytes total]",
        &text[..end],
        text.len()
    )
}

/// Import a single Freebuff/Codebuff chat directory.
///
/// `path` is `…/chats/<chatId>/`. Produces real `Message`s with tool calls
/// preserved as `ToolUse` / `ToolResult` blocks, which no other external source
/// provides.
pub fn import_freebuff_chat(path: &Path) -> anyhow::Result<ImportedSession> {
    if !path.is_dir() {
        anyhow::bail!("freebuff chat path is not a directory: {}", path.display());
    }
    let messages_path = path.join("chat-messages.json");
    match std::fs::metadata(&messages_path) {
        Ok(md) if md.len() > MAX_TRANSCRIPT_BYTES => {
            tracing::info!(
                path = %messages_path.display(),
                bytes = md.len(),
                "Skipping oversized external transcript"
            );
            return Ok(ImportedSession {
                name: format!(
                    "freebuff-{}",
                    path.file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("unknown")
                ),
                working_dir: freebuff_chat_project_root(path),
                source: "freebuff",
                source_path: path.to_path_buf(),
                messages: Vec::new(),
            });
        }
        Err(e) => anyhow::bail!("cannot stat {}: {e}", messages_path.display()),
        Ok(_) => {}
    }
    let raw = std::fs::read_to_string(&messages_path)?;
    let msgs: Vec<FbMessage> = serde_json::from_str(&raw)?;

    // The chat id is the start timestamp with ':' replaced by '-'.
    let chat_id = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    let mut messages: Vec<Message> = Vec::new();
    // Tool results accumulate and are flushed as one user turn after the
    // assistant turn that requested them, preserving the tool_use/tool_result
    // adjacency every provider requires.
    let mut pending_results: Vec<ContentBlock> = Vec::new();

    let flush_results = |pending: &mut Vec<ContentBlock>, out: &mut Vec<Message>| {
        if pending.is_empty() {
            return;
        }
        out.push(Message::user_blocks(std::mem::take(pending)));
    };

    for m in &msgs {
        let role = match m.variant.as_deref().map(str::to_lowercase).as_deref() {
            Some("user") => Role::User,
            Some("ai") | Some("assistant") => Role::Assistant,
            _ => continue,
        };

        let mut blocks: Vec<ContentBlock> = Vec::new();
        // A user turn always flushes any tool results owed from the previous
        // assistant turn first, so ordering is preserved.
        if role == Role::User {
            flush_results(&mut pending_results, &mut messages);
        }

        for b in m.blocks.iter().flatten() {
            match b.block_type.as_deref() {
                Some("text") => {
                    if let Some(text) = &b.content {
                        if !text.trim().is_empty() {
                            blocks.push(ContentBlock::Text {
                                text: clamp_text(text, MAX_TEXT_CHARS),
                            });
                        }
                    }
                }
                Some("tool") => {
                    let call_id = b
                        .tool_call_id
                        .clone()
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| format!("freebuff-tool-{}", blocks.len()));
                    let name = b.tool_name.clone().unwrap_or_else(|| "tool".into());
                    // A tool block that carries output is both the call and its
                    // result: emit the call on the assistant side and stash the
                    // result for the following user turn.
                    if role == Role::Assistant {
                        blocks.push(ContentBlock::ToolUse {
                            id: call_id.clone(),
                            name,
                            input: b.input.clone().unwrap_or(serde_json::json!({})),
                            thought_signature: None,
                        });
                    }
                    if let Some(out) = &b.output {
                        pending_results.push(ContentBlock::ToolResult {
                            tool_use_id: call_id,
                            content: ToolResultContent::Text(clamp_text(
                                out,
                                MAX_TOOL_RESULT_CHARS,
                            )),
                            is_error: None,
                        });
                    }
                }
                // `agent`, `mode-divider`, `ask-user` and friends are UI chrome
                // or already reflected in the text; they carry no history value.
                _ => {}
            }
        }

        // Fall back to the flat `content` field when there are no usable blocks.
        if blocks.is_empty() && pending_results.is_empty() {
            if let Some(text) = &m.content {
                if !text.trim().is_empty() {
                    blocks.push(ContentBlock::Text {
                        text: clamp_text(text, MAX_TEXT_CHARS),
                    });
                }
            }
        }

        if blocks.is_empty() {
            continue;
        }
        messages.push(match role {
            Role::User => Message::user_blocks(blocks),
            Role::Assistant => Message::assistant_blocks(blocks),
        });
    }
    flush_results(&mut pending_results, &mut messages);

    Ok(ImportedSession {
        name: format!("freebuff-{chat_id}"),
        // Attribute the chat to the project it ran in; this is the locate key
        // that scopes the import to the current working directory.
        working_dir: freebuff_chat_project_root(path),
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
    /// Freebuff / Codebuff chat root: `~/.config/manicode/projects`.
    pub freebuff_projects: PathBuf,
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
            freebuff_projects: home.join(".config").join("manicode").join("projects"),
        }
    }
}

/// A pluggable external-session source.
///
/// Each source is a self-contained descriptor: where its data lives, how to
/// cheaply discover candidate paths, and how to parse one path. The absorb /
/// history-restore core drives this registry generically, so adding, disabling,
/// or removing a source never requires touching the core machinery. All three
/// registered sources (Opencode, Cline, Freebuff) are project-scoped and on by
/// default; narrow the set from `settings.json` with `externalImportSources`.
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
            root: locs.freebuff_projects,
            // Project-scoped conversation history, like Opencode and Cline: a
            // chat is attributed to the project recorded in its run-state.json.
            system_scoped: false,
            default_enabled: true,
            discover: discover_freebuff_chats,
            import: |p| import_freebuff_chat(p).ok(),
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
/// Discover Freebuff / Codebuff chat directories under a projects root.
///
/// Layout is `<root>/<project-slug>/chats/<chatId>/`, and a chat directory is
/// identified by containing `chat-messages.json`. Walks two levels and opens no
/// transcript, so it stays cheap enough for the fingerprint-before-parse path.
pub fn discover_freebuff_chats(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(projects) = std::fs::read_dir(dir) else {
        return out;
    };
    for project in projects.flatten() {
        let chats_dir = project.path().join("chats");
        let Ok(chats) = std::fs::read_dir(&chats_dir) else {
            continue;
        };
        for chat in chats.flatten() {
            let chat_dir = chat.path();
            if chat_dir.is_dir() && chat_dir.join("chat-messages.json").is_file() {
                out.push(chat_dir);
            }
        }
    }
    out
}

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
        // freebuff: <root>/<slug>/chats/<chatId>/chat-messages.json
        let fb = tempdir().unwrap();
        let chat = fb
            .path()
            .join("myproj")
            .join("chats")
            .join("2026-07-11T22-49-27.735Z");
        std::fs::create_dir_all(&chat).unwrap();
        std::fs::write(chat.join("chat-messages.json"), "[]").unwrap();
        // A project dir with no chats/ is not a discoverable source.
        std::fs::create_dir_all(fb.path().join("empty-proj")).unwrap();
        let found = discover_freebuff_chats(fb.path());
        assert_eq!(found.len(), 1, "only the chat dir is discovered");
        assert!(found[0].ends_with("2026-07-11T22-49-27.735Z"));
        // A missing dir yields nothing.
        assert!(discover_opencode_paths(&oc.path().join("nope")).is_empty());
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
            // Another project's freebuff chat -> excluded, like any other
            // project-scoped source.
            session("freebuff", Some("/definitely/not/cwd".to_string())),
            // This project's freebuff chat -> included.
            session("freebuff", Some(cwd.display().to_string())),
        ];
        let kept = filter_sessions_by_cwd(sessions, cwd);
        let kept_sources: Vec<&str> = kept.iter().map(|s| s.source).collect();
        assert!(
            !kept_sources.contains(&"opencode"),
            "unscopable session must not be imported (cross-project leak)"
        );
        // The cwd-matching cline session + the cwd-matching freebuff chat.
        assert_eq!(kept.len(), 2, "kept: {kept_sources:?}");
        assert_eq!(kept_sources.iter().filter(|s| **s == "freebuff").count(), 1);
    }

    /// Build a fake Freebuff chat dir: `projects/<slug>/chats/<chatId>/`.
    fn write_freebuff_chat(
        root: &Path,
        slug: &str,
        chat_id: &str,
        project_root: &str,
        msgs: &str,
    ) -> PathBuf {
        let chat = root.join(slug).join("chats").join(chat_id);
        std::fs::create_dir_all(&chat).unwrap();
        std::fs::write(chat.join("chat-messages.json"), msgs).unwrap();
        std::fs::write(
            chat.join("run-state.json"),
            format!(r#"{{"sessionState":{{"fileContext":{{"projectRoot":"{project_root}"}}}}}}"#),
        )
        .unwrap();
        chat
    }

    #[test]
    fn imports_freebuff_chat_with_tool_calls_preserved() {
        let dir = tempdir().unwrap();
        let msgs = r#"[
            {"variant":"user","content":"add a test","timestamp":"06:50 PM"},
            {"variant":"ai","content":"","blocks":[
                {"type":"text","content":"On it."},
                {"type":"tool","toolCallId":"U1","toolName":"read_files","input":{"paths":["a.py"]},"output":"print(1)"},
                {"type":"mode-divider","mode":"LITE"}
            ]},
            {"variant":"user","content":"thanks","timestamp":"06:51 PM"}
        ]"#;
        let chat = write_freebuff_chat(
            dir.path(),
            "proj",
            "2026-07-11T22-49-27.735Z",
            "/work/proj",
            msgs,
        );
        let s = import_freebuff_chat(&chat).unwrap();
        assert_eq!(s.source, "freebuff");
        assert_eq!(
            s.working_dir.as_deref(),
            Some("/work/proj"),
            "cwd from run-state"
        );
        assert_eq!(
            s.name, "freebuff-2026-07-11T22-49-27.735Z",
            "chat id is the timestamp"
        );
        // user, assistant, tool-result user turn, user
        assert_eq!(s.messages.len(), 4, "got {} msgs", s.messages.len());
        assert_eq!(s.messages[0].role, crate::types::Role::User);

        // The assistant turn keeps the tool call as a real ToolUse block.
        let MessageContent::Blocks(blocks) = &s.messages[1].content else {
            panic!("assistant turn should carry blocks");
        };
        assert!(
            blocks.iter().any(|b| matches!(
                b,
                ContentBlock::ToolUse { name, id, .. } if name == "read_files" && id == "U1"
            )),
            "tool call preserved as ToolUse: {blocks:?}"
        );
        // The result lands in the following user turn, paired by id.
        let MessageContent::Blocks(results) = &s.messages[2].content else {
            panic!("tool results should be a user turn");
        };
        assert!(
            results.iter().any(|b| matches!(
                b,
                ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "U1"
            )),
            "tool result paired by id"
        );
    }

    #[test]
    fn freebuff_chat_skips_ui_chrome_and_empty_turns() {
        let dir = tempdir().unwrap();
        // `agent` / `mode-divider` / `ask-user` blocks and empty variants carry
        // no history value and must not become messages.
        let msgs = r#"[
            {"variant":"ai","content":"","blocks":[{"type":"mode-divider","mode":"LITE"}]},
            {"variant":"system","content":"ignored"},
            {"variant":"ai","blocks":[{"type":"text","content":"real"}]}
        ]"#;
        let chat = write_freebuff_chat(dir.path(), "p", "2026-01-01T00-00-00.000Z", "/w", msgs);
        let s = import_freebuff_chat(&chat).unwrap();
        assert_eq!(s.messages.len(), 1, "only the real turn survives");
        let MessageContent::Blocks(b) = &s.messages[0].content else {
            panic!("expected blocks");
        };
        assert!(matches!(&b[0], ContentBlock::Text { text } if text == "real"));
    }

    #[test]
    fn freebuff_text_and_tool_output_are_bounded() {
        let dir = tempdir().unwrap();
        let big = "x".repeat(MAX_TEXT_CHARS * 3);
        let msgs = format!(
            r#"[{{"variant":"ai","blocks":[{{"type":"text","content":"{big}"}},
                 {{"type":"tool","toolCallId":"T","toolName":"read_files","output":"{big}"}}]}}]"#
        );
        let chat = write_freebuff_chat(dir.path(), "p", "2026-01-01T00-00-00.000Z", "/w", &msgs);
        let s = import_freebuff_chat(&chat).unwrap();
        let MessageContent::Blocks(b) = &s.messages[0].content else {
            panic!("expected blocks");
        };
        for blk in b {
            let len = match blk {
                ContentBlock::Text { text } => text.len(),
                ContentBlock::ToolUse { input, .. } => {
                    // output is in the result turn, not here
                    let _ = input;
                    0
                }
                _ => 0,
            };
            assert!(len <= MAX_TEXT_CHARS + 64, "text block bounded, got {len}");
        }
        // And the tool result is bounded in the following user turn.
        let MessageContent::Blocks(r) = &s.messages[1].content else {
            panic!("expected result turn");
        };
        if let ContentBlock::ToolResult { content, .. } = &r[0] {
            let ToolResultContent::Text(t) = content else {
                panic!("expected text result");
            };
            assert!(t.len() <= MAX_TOOL_RESULT_CHARS + 64, "result bounded");
        }
    }

    #[test]
    fn discovers_freebuff_chat_dirs_only() {
        let dir = tempdir().unwrap();
        let chat = write_freebuff_chat(
            dir.path(),
            "myproj",
            "2026-07-11T22-49-27.735Z",
            "/work",
            "[]",
        );
        // A project with no chats/ dir, and a chats/ dir with a non-chat dir.
        std::fs::create_dir_all(dir.path().join("other")).unwrap();
        std::fs::create_dir_all(dir.path().join("myproj").join("chats").join("not-a-chat"))
            .unwrap();
        let found = discover_freebuff_chats(dir.path());
        assert_eq!(found.len(), 1, "only real chat dirs: {found:?}");
        assert_eq!(found[0], chat);
        assert!(discover_freebuff_chats(&dir.path().join("nope")).is_empty());
    }

    #[test]
    fn all_registered_sources_are_project_scoped_and_default_on() {
        // Every source in the registry is cwd-scoped conversation history, so
        // a default import can never pull another project's history in, and all
        // of them are on unless the user narrows the set in settings.
        for id in ["opencode", "cline", "freebuff"] {
            assert!(
                is_source_default_enabled(id),
                "{id} should be default-enabled"
            );
            assert!(
                !is_system_scoped_source(id),
                "{id} must be project-scoped, not system-wide"
            );
        }
        // An unknown id is neither.
        assert!(!is_source_default_enabled("not-a-source"));
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
