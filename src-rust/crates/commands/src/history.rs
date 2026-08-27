// History commands: `/undo`, `/revert`, `/checkpoints`, `/snapshot`.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct UndoCommand;
pub struct RevertCommand;
pub struct CheckpointsCommand;
pub struct SnapshotDiffCommand;

// ---- /undo (per-prompt-request revert, with confirmation) ----
//
// Undo targets the group of assistant turns that belong to ONE prompt
// (everything since the user's last real message), rather than a single
// turn like /revert. Fuzzy by nature (interleaved chat makes the boundary
// soft), so it previews and requires confirmation unless `--yes` is passed;
// /revert remains the explicit, confirmation-free single-turn tool.

#[async_trait]
impl SlashCommand for UndoCommand {
    fn name(&self) -> &str {
        "undo"
    }
    fn aliases(&self) -> Vec<&str> {
        vec![]
    }
    fn description(&self) -> &str {
        "Revert all file changes since your last prompt (confirmation required)"
    }
    fn help(&self) -> &str {
        "Usage: /undo [<n>] [--yes]\n\n\
         Reverts every file change made in response to your most recent prompt\n\
         (all assistant turns since your last message). With <n>, go back n\n\
         prompts (1 = the latest). Tool-result rounds within one task count as\n\
         part of the same prompt, so /undo rolls back a whole agent run.\n\n\
         Shows what would be reverted and asks for confirmation; pass --yes to\n\
         skip the prompt. For single-turn control use /revert; list turns with\n\
         /checkpoints."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        // Parse `[<n>] [--yes]` in any order.
        let mut n = 1usize;
        let mut confirmed = false;
        let mut saw_number = false;
        for tok in args.split_whitespace() {
            match tok {
                "--yes" | "-y" | "confirm" | "--confirm" => confirmed = true,
                t => match t.parse::<usize>() {
                    Ok(v) if !saw_number => {
                        n = v;
                        saw_number = true;
                    }
                    _ => return CommandResult::Error("Usage: /undo [<n>] [--yes]".to_string()),
                },
            }
        }
        if n == 0 {
            return CommandResult::Error(
                "Usage: /undo [<n>] [--yes]\n\nn is 1-based: 1 = since your last prompt."
                    .to_string(),
            );
        }

        let snap = match clawde_core::snapshot::get_or_create(&ctx.working_dir) {
            Some(s) => s,
            None => {
                return CommandResult::Error(
                    "Snapshot system unavailable (git not found or not a git repo).".into(),
                )
            }
        };
        let prompt_count = undo_prompt_count(&ctx.messages);
        let Some(group) = undo_group(&ctx.messages, n) else {
            if prompt_count == 0 {
                return CommandResult::Message("Nothing to undo: no prompts yet.".into());
            }
            if n > prompt_count {
                return CommandResult::Error(format!(
                    "Cannot go back {n} prompts — this session has {prompt_count}."
                ));
            }
            return CommandResult::Message(
                "Nothing to undo: no file changes recorded since that prompt.".into(),
            );
        };
        let patched: Vec<&clawde_core::types::Message> =
            group.patched.iter().map(|&i| &ctx.messages[i]).collect();
        if patched.is_empty() {
            return CommandResult::Message(
                "Nothing to undo: no file changes recorded since that prompt.".into(),
            );
        }

        let file_count: usize = patched
            .iter()
            .filter_map(|m| m.snapshot_patch.as_ref())
            .map(|p| p.files.len())
            .sum();
        let prompt_preview = prompt_text_preview(&ctx.messages[group.prompt_index]);
        let group_turns = ctx.messages[group.prompt_index + 1..]
            .iter()
            .filter(|m| m.role == clawde_core::types::Role::Assistant)
            .count();

        if !confirmed {
            // Preview: what will be reverted, without touching anything.
            let mut lines = vec![format!(
                "Undo everything changed since your prompt: \"{}\"",
                prompt_preview
            )];
            lines.push(format!(
                "  {} assistant turn(s) in this group; {} file change(s) across {} turn(s):",
                group_turns,
                file_count,
                patched.len()
            ));
            for m in &patched {
                let uuid_short = m
                    .uuid
                    .as_deref()
                    .map(|u| &u[..u.len().min(8)])
                    .unwrap_or("?");
                let files: Vec<String> = m
                    .snapshot_patch
                    .as_ref()
                    .map(|p| {
                        p.files
                            .iter()
                            .take(3)
                            .map(|f| {
                                f.file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_default()
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                lines.push(format!(
                    "    [{}] {} file(s): {}",
                    uuid_short,
                    files.len(),
                    files.join(", ")
                ));
            }
            lines.push(
                "Run /undo [<n>] --yes to confirm. Use /snapshot <n> for diffs; /revert <n> \
                 reverts a single turn without confirmation."
                    .to_string(),
            );
            return CommandResult::Message(lines.join("\n"));
        }

        // Confirmed: revert the whole group, then branch the transcript so the
        // user prompt survives and the work moves to a sibling branch.
        let patches: Vec<clawde_core::snapshot::Patch> = ctx.messages[group.prompt_index + 1..]
            .iter()
            .filter(|m| m.role == clawde_core::types::Role::Assistant)
            .filter_map(|m| m.snapshot_patch.clone())
            .collect();
        snap.revert(&patches).await;

        let project_root = clawde_core::git_utils::project_root(&ctx.working_dir);
        let path =
            match clawde_core::session_storage::transcript_path(&project_root, &ctx.session_id) {
                Ok(p) => p,
                Err(e) => return CommandResult::Error(format!("Invalid session ID: {e}")),
            };
        let mut note = String::new();
        if path.exists() {
            // Branch before the FIRST assistant turn after the prompt so the
            // prompt (and everything before it) stays on the active leaf.
            let first_uuid = ctx.messages[group.prompt_index + 1..]
                .iter()
                .find(|m| m.role == clawde_core::types::Role::Assistant)
                .and_then(|m| m.uuid.clone());
            if let Some(first_uuid) = first_uuid {
                if let Err(e) =
                    clawde_core::session_storage::branch_before(&path, &first_uuid).await
                {
                    return CommandResult::Error(format!(
                        "Reverted files but could not update transcript: {e}"
                    ));
                }
            } else {
                note = "\nNote: the transcript has no message id at the undo boundary, so it was \
                        not branched — files were still restored."
                    .to_string();
            }
        }

        let prompt_word = if n == 1 { "your prompt" } else { "that prompt" };
        CommandResult::Message(format!(
            "Reverted {} file(s) changed since {}. Later turns kept on a branch.\n\
             /undo {} goes back further; /revert <n> for single-turn control.{}",
            file_count,
            prompt_word,
            n + 1,
            note
        ))
    }
}

// ---- /revert ---------------------------------------------------------------

#[async_trait]
impl SlashCommand for RevertCommand {
    fn name(&self) -> &str {
        "revert"
    }
    fn description(&self) -> &str {
        "Revert file changes from an assistant turn back to pre-turn state"
    }
    fn help(&self) -> &str {
        "Usage: /revert [<n>|<uuid>]\n\n\
         Without args: revert the most recent assistant turn.\n\
         With a number n: revert the n-th most recent assistant turn (1 = latest).\n\
         With a uuid: revert the turn whose message id starts with that string.\n\n\
         This uses the shadow-git snapshot to restore all files that were\n\
         changed during the target turn, and removes that turn (and any later\n\
         turns) from the session transcript.\n\n\
         Examples:\n\
           /revert        — revert last turn\n\
           /revert 2      — revert the second-to-last turn\n\
           /revert abc123 — revert the turn with uuid starting 'abc123'"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let snap = match clawde_core::snapshot::get_or_create(&ctx.working_dir) {
            Some(s) => s,
            None => {
                return CommandResult::Error(
                    "Snapshot system unavailable (git not found or not a git repo).".into(),
                )
            }
        };

        // Collect assistant messages that have a snapshot patch (newest last).
        let checkpoints: Vec<&clawde_core::types::Message> = ctx
            .messages
            .iter()
            .filter(|m| m.role == clawde_core::types::Role::Assistant && m.snapshot_patch.is_some())
            .collect();

        if checkpoints.is_empty() {
            return CommandResult::Message(
                "No revertible turns found. Run /checkpoints to see recorded file changes.".into(),
            );
        }

        // Select the target turn.
        let args = args.trim();
        let target = if args.is_empty() {
            checkpoints.last().copied()
        } else if let Ok(n) = args.parse::<usize>() {
            if n == 0 || n > checkpoints.len() {
                return CommandResult::Error(format!(
                    "Turn {} out of range (1–{}).",
                    n,
                    checkpoints.len()
                ));
            }
            Some(checkpoints[checkpoints.len() - n])
        } else {
            checkpoints
                .iter()
                .copied()
                .find(|m| m.uuid.as_deref().is_some_and(|u| u.starts_with(args)))
        };

        let target = match target {
            Some(m) => m,
            None => return CommandResult::Error(format!("No turn found matching '{args}'.")),
        };

        // Collect all patches from this turn onward to revert.
        let target_uuid = match target.uuid.clone() {
            Some(u) => u,
            None => return CommandResult::Error("Target turn has no uuid; cannot revert.".into()),
        };

        let patches: Vec<clawde_core::snapshot::Patch> = ctx
            .messages
            .iter()
            .skip_while(|m| m.uuid.as_deref() != Some(&target_uuid))
            .filter_map(|m| m.snapshot_patch.clone())
            .collect();

        if patches.is_empty() {
            return CommandResult::Message("No file changes recorded for that turn.".into());
        }

        // Revert files.
        snap.revert(&patches).await;

        // Record the revert in the session transcript. NON-DESTRUCTIVE (#234):
        // rather than truncating, point the active leaf at the turn *before* the
        // target so the reverted turn (and everything after it) is retained on a
        // sibling branch that can be returned to. `branch_before` only falls
        // back to a destructive truncate for legacy/unchained transcripts.
        let project_root = clawde_core::git_utils::project_root(&ctx.working_dir);
        let path =
            match clawde_core::session_storage::transcript_path(&project_root, &ctx.session_id) {
                Ok(p) => p,
                Err(e) => return CommandResult::Error(format!("Invalid session ID: {e}")),
            };
        if path.exists() {
            if let Err(e) = clawde_core::session_storage::branch_before(&path, &target_uuid).await {
                return CommandResult::Error(format!(
                    "Reverted files but could not update transcript: {e}"
                ));
            }
        }

        let file_count: usize = patches.iter().map(|p| p.files.len()).sum();
        CommandResult::Message(format!(
            "Reverted {} file(s) changed during turn {}. Later turns kept on a branch.",
            file_count,
            &target_uuid[..target_uuid.len().min(8)],
        ))
    }
}

// ---- /checkpoints ----------------------------------------------------------

#[async_trait]
impl SlashCommand for CheckpointsCommand {
    fn name(&self) -> &str {
        "checkpoints"
    }
    fn description(&self) -> &str {
        "List assistant turns that have recorded file changes"
    }
    fn help(&self) -> &str {
        "Usage: /checkpoints\n\nShows all assistant turns in this session that modified files,\n\
         with file counts.  Use /revert <n> to roll back to a specific turn."
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        let checkpoints: Vec<(usize, &clawde_core::types::Message)> = ctx
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                m.role == clawde_core::types::Role::Assistant && m.snapshot_patch.is_some()
            })
            .collect();

        if checkpoints.is_empty() {
            return CommandResult::Message(
                "No file-change checkpoints recorded yet for this session.\n\
                 Checkpoints are created automatically when the assistant modifies files."
                    .into(),
            );
        }

        let total = checkpoints.len();
        let mut lines = vec![format!("{} checkpoint(s):", total)];

        // Lazy descriptions: load the per-session cache, batch-generate any
        // missing ones in a single best-effort model call, persist, render.
        let mut cache = std::collections::HashMap::new();
        if let Some(cache_path) = checkpoint_cache_path(
            &clawde_core::git_utils::project_root(&ctx.working_dir),
            &ctx.session_id,
        ) {
            cache = load_checkpoint_cache(&cache_path).await;
            let missing: Vec<(String, String)> = checkpoints
                .iter()
                .filter_map(|(_, m)| {
                    let uuid = m.uuid.as_deref()?;
                    if cache.contains_key(uuid) {
                        return None;
                    }
                    let hash = m.snapshot_patch.as_ref().map(|p| p.hash.clone())?;
                    Some((uuid.to_string(), hash))
                })
                .collect();
            if !missing.is_empty() {
                if let Some(snap) = clawde_core::snapshot::get_or_create(&ctx.working_dir) {
                    if let Some(generated) =
                        generate_checkpoint_descriptions(ctx, &snap, &missing).await
                    {
                        for (uuid, desc) in generated {
                            cache.insert(uuid, desc);
                        }
                        save_checkpoint_cache(&cache_path, &cache).await;
                    }
                }
            }
        }

        for (rank, (_, msg)) in checkpoints.iter().rev().enumerate() {
            let uuid_short = msg
                .uuid
                .as_deref()
                .map(|u| &u[..u.len().min(8)])
                .unwrap_or("?");
            let file_count = msg.snapshot_patch.as_ref().map_or(0, |p| p.files.len());
            let preview: Vec<String> = msg
                .snapshot_patch
                .as_ref()
                .map(|p| {
                    p.files
                        .iter()
                        .take(3)
                        .map(|f| {
                            f.file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_default()
                        })
                        .collect()
                })
                .unwrap_or_default();
            let preview_str = if preview.len() == file_count {
                preview.join(", ")
            } else {
                format!("{}, …", preview.join(", "))
            };
            lines.push(format!(
                "  [{}] {} — {} file(s): {}",
                rank + 1,
                uuid_short,
                file_count,
                preview_str
            ));
            if let Some(desc) = msg.uuid.as_deref().and_then(|u| cache.get(u)) {
                lines.push(format!("      {desc}"));
            }
        }
        lines.push(String::new());
        lines.push(
            "Use /undo to revert everything since your last prompt, or /revert <n> for a single turn."
                .into(),
        );
        CommandResult::Message(lines.join("\n"))
    }
}

// ---- /snapshot (show snapshot diff for a recorded turn) ------------------

#[async_trait]
impl SlashCommand for SnapshotDiffCommand {
    fn name(&self) -> &str {
        "snapshot"
    }
    fn description(&self) -> &str {
        "Show shadow-git diff of file changes from an assistant turn"
    }
    fn help(&self) -> &str {
        "Usage: /snapshot [<n>|<hash>]\n\n\
         Without args: show unified diff for the most recent assistant turn.\n\
         With a number: show diff for the n-th most recent turn (1 = latest).\n\
         With a hash: show diff against that explicit snapshot tree hash.\n\n\
         See also: /checkpoints (list turns), /revert (roll back files)."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let snap = match clawde_core::snapshot::get_or_create(&ctx.working_dir) {
            Some(s) => s,
            None => {
                return CommandResult::Error(
                    "Snapshot system unavailable (git not found or not a git repo).".into(),
                )
            }
        };

        let args = args.trim();

        // If a raw hash was passed, use it directly.
        let hash = if !args.is_empty()
            && args.chars().all(|c| c.is_ascii_hexdigit())
            && args.len() >= 8
        {
            args.to_string()
        } else {
            // Otherwise find the n-th most recent checkpoint.
            let checkpoints: Vec<&clawde_core::snapshot::Patch> = ctx
                .messages
                .iter()
                .filter_map(|m| {
                    if m.role == clawde_core::types::Role::Assistant {
                        m.snapshot_patch.as_ref()
                    } else {
                        None
                    }
                })
                .collect();

            if checkpoints.is_empty() {
                return CommandResult::Message(
                    "No snapshot checkpoints recorded yet. File changes will appear here after the next assistant turn.".into()
                );
            }

            let idx = if args.is_empty() {
                0
            } else {
                match args.parse::<usize>() {
                    Ok(n) if n >= 1 && n <= checkpoints.len() => n - 1,
                    _ => {
                        return CommandResult::Error(format!(
                            "Turn '{}' out of range (1–{}).",
                            args,
                            checkpoints.len()
                        ))
                    }
                }
            };
            // Reverse so idx=0 is newest.
            let patch = checkpoints[checkpoints.len() - 1 - idx];
            patch.hash.clone()
        };

        let diff = snap.diff(&hash).await;
        if diff.is_empty() {
            CommandResult::Message(format!(
                "No changes since snapshot {}.",
                &hash[..hash.len().min(8)]
            ))
        } else {
            CommandResult::Message(diff)
        }
    }
}

// ---------------------------------------------------------------------------
// Per-prompt undo helpers
// ---------------------------------------------------------------------------

/// A "prompt" is a user message that is not a tool-result round: tool
/// results come back as `Role::User` messages containing `ToolResult`
/// blocks, so they must not count as a fresh prompt boundary.
fn is_user_prompt(msg: &clawde_core::types::Message) -> bool {
    if msg.role != clawde_core::types::Role::User {
        return false;
    }
    match &msg.content {
        clawde_core::types::MessageContent::Text(_) => true,
        clawde_core::types::MessageContent::Blocks(blocks) => !blocks
            .iter()
            .any(|b| matches!(b, clawde_core::types::ContentBlock::ToolResult { .. })),
    }
}

/// The assistant-turn group belonging to the n-th most recent prompt
/// (1 = latest). Returns the prompt's message index plus the indices of all
/// assistant messages with a recorded snapshot patch after it.
struct UndoGroup {
    prompt_index: usize,
    patched: Vec<usize>,
}

/// Number of real prompts in the transcript (for out-of-range messages).
fn undo_prompt_count(messages: &[clawde_core::types::Message]) -> usize {
    messages.iter().filter(|m| is_user_prompt(m)).count()
}

fn undo_group(messages: &[clawde_core::types::Message], n: usize) -> Option<UndoGroup> {
    let prompts: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| is_user_prompt(m))
        .map(|(i, _)| i)
        .collect();
    if n == 0 || n > prompts.len() {
        return None;
    }
    let prompt_index = prompts[prompts.len() - n];
    let patched: Vec<usize> = messages[prompt_index + 1..]
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            m.role == clawde_core::types::Role::Assistant && m.snapshot_patch.is_some()
        })
        .map(|(i, _)| prompt_index + 1 + i)
        .collect();
    Some(UndoGroup {
        prompt_index,
        patched,
    })
}

/// First line of a prompt's text, capped, for the /undo preview.
fn prompt_text_preview(msg: &clawde_core::types::Message) -> String {
    let text = msg.get_all_text();
    let first = text.lines().next().unwrap_or("");
    let capped: String = first.chars().take(80).collect();
    if capped.trim().is_empty() {
        "…".to_string()
    } else {
        capped
    }
}

// ---------------------------------------------------------------------------
// Checkpoint descriptions (lazy, batched, best-effort)
// ---------------------------------------------------------------------------

/// Per-session cache file next to the transcript: `{session}.checkpoints.json`
/// mapping turn uuid -> generated description. Survives restarts.
fn checkpoint_cache_path(
    project_root: &std::path::Path,
    session_id: &str,
) -> Option<std::path::PathBuf> {
    let transcript =
        clawde_core::session_storage::transcript_path(project_root, session_id).ok()?;
    Some(transcript.with_extension("checkpoints.json"))
}

async fn load_checkpoint_cache(
    path: &std::path::Path,
) -> std::collections::HashMap<String, String> {
    match tokio::fs::read_to_string(path).await {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => std::collections::HashMap::new(),
    }
}

async fn save_checkpoint_cache(
    path: &std::path::Path,
    cache: &std::collections::HashMap<String, String>,
) {
    let Ok(s) = serde_json::to_string(cache) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let _ = tokio::fs::write(path, s).await;
}

/// Generate descriptions for checkpoints missing one, in a SINGLE model call
/// (one summary per stored diff), best-effort: any failure returns `None` and
/// the caller falls back to raw diff stats.
async fn generate_checkpoint_descriptions(
    ctx: &CommandContext,
    snap: &clawde_core::snapshot::ShadowSnapshot,
    missing: &[(String, String)],
) -> Option<std::collections::HashMap<String, String>> {
    let provider = resolve_command_provider(ctx).await?;

    // One diff per missing checkpoint, capped in total size.
    const MAX_DIFF_CHARS: usize = 60_000;
    let mut parts: Vec<String> = Vec::new();
    let mut total = 0usize;
    for (uuid, hash) in missing {
        let diff = snap.diff(hash).await;
        if total + diff.len() > MAX_DIFF_CHARS && !parts.is_empty() {
            break;
        }
        total += diff.len();
        parts.push(format!("--- {uuid} ---\n{diff}"));
    }
    if parts.is_empty() {
        return None;
    }

    let prompt = format!(
        "You summarize git diffs for a coding assistant's checkpoint list.\n\
         Below are diff groups, each labeled with its id (--- <id> ---).\n\
         For EACH group write exactly one concise sentence describing what changed\n\
         and why it matters, under 200 characters.\n\n\
         Respond with ONLY a JSON object mapping each id to its summary —\n\
         no markdown fences, no prose outside the JSON.\n\n\
         {}",
        parts.join("\n")
    );
    let request = clawde_api::ProviderRequest {
        model: ctx.config.effective_model().to_string(),
        messages: vec![clawde_core::types::Message::user(prompt)],
        system_prompt: Some(clawde_api::SystemPrompt::Text(
            "You summarize diffs precisely and briefly.".to_string(),
        )),
        tools: vec![],
        max_tokens: 2048,
        temperature: None,
        top_p: None,
        top_k: None,
        stop_sequences: vec![],
        thinking: None,
        effort_level: ctx.effort,
        provider_options: serde_json::json!({}),
        strict_route: false,
    };
    let response = provider.create_message(request).await.ok()?;
    let text = text_from_content_blocks(&response.content);
    serde_json::from_str::<std::collections::HashMap<String, String>>(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawde_core::snapshot::Patch;
    use clawde_core::types::Message;
    use std::path::PathBuf;

    fn make_ctx(working_dir: PathBuf, messages: Vec<Message>) -> CommandContext {
        CommandContext {
            config: clawde_core::config::Config::default(),
            cost_tracker: clawde_core::cost::CostTracker::new(),
            messages,
            working_dir,
            session_id: "test-session".to_string(),
            session_title: None,
            remote_session_url: None,
            mcp_manager: None,
            mcp_auth_runner: None,
            provider_registry: None,
            test_provider: None,
            effort: None,
            tool_use_tracker: None,
        }
    }

    /// Assistant message with a recorded shadow-git patch, as the query loop
    /// would produce on a turn that modified files.
    fn assistant_with_patch(uuid: &str, files: &[&str]) -> Message {
        let mut m = Message::assistant("response text");
        m.uuid = Some(uuid.to_string());
        m.snapshot_patch = Some(Patch {
            hash: uuid.to_string(),
            files: files.iter().map(PathBuf::from).collect(),
        });
        m
    }

    /// Create a throwaway git repository (needed for the snapshot system to
    /// be available). No disk writes happen for the paths under test — they
    /// return before any `revert`/`diff` call touches the shadow gitdir.
    fn make_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
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

    // ---- /checkpoints (pure message inspection) ---------------------------

    #[tokio::test]
    async fn checkpoints_empty_conversation() {
        let mut ctx = make_ctx(PathBuf::from("."), vec![]);
        match CheckpointsCommand.execute("", &mut ctx).await {
            CommandResult::Message(m) => {
                assert!(m.contains("No file-change checkpoints"), "{}", m);
            }
            other => panic!("expected Message, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn checkpoints_lists_turns_newest_first() {
        // TestHome keeps the description-cache probe hermetic (no reads of a
        // real ~/.clawde and, if a provider env key is present in CI, no
        // accidental live generation call).
        let _home = TestHome::new();
        let messages = vec![
            assistant_with_patch("aaaaaaaaaaaa", &["old.rs"]),
            Message::user("do the thing"),
            assistant_with_patch("bbbbbbbbbbbb", &["new.rs", "other.rs"]),
        ];
        let mut ctx = make_ctx(PathBuf::from("."), messages);
        match CheckpointsCommand.execute("", &mut ctx).await {
            CommandResult::Message(m) => {
                assert!(m.contains("2 checkpoint(s):"), "{}", m);
                // Newest turn first.
                assert!(
                    m.contains("[1] bbbbbbbb — 2 file(s): new.rs, other.rs"),
                    "{}",
                    m
                );
                assert!(m.contains("[2] aaaaaaaa — 1 file(s): old.rs"), "{}", m);
                assert!(m.contains("/revert <n>"), "{}", m);
            }
            other => panic!("expected Message, got {:?}", other),
        }
    }

    // ---- /revert and /undo (early-return paths, no shadow-git writes) -----

    #[tokio::test]
    async fn revert_without_checkpoints_is_informative() {
        let repo = make_repo();
        let mut ctx = make_ctx(repo.path().to_path_buf(), vec![]);
        match RevertCommand.execute("", &mut ctx).await {
            CommandResult::Message(m) => {
                assert!(m.contains("No revertible turns found"), "{}", m);
            }
            other => panic!("expected Message, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn revert_out_of_range_errors() {
        let repo = make_repo();
        let mut ctx = make_ctx(
            repo.path().to_path_buf(),
            vec![assistant_with_patch("aaa", &["a.rs"])],
        );
        match RevertCommand.execute("5", &mut ctx).await {
            CommandResult::Error(e) => assert!(e.contains("out of range"), "{}", e),
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn revert_unmatched_uuid_errors() {
        let repo = make_repo();
        let mut ctx = make_ctx(
            repo.path().to_path_buf(),
            vec![assistant_with_patch("aaa", &["a.rs"])],
        );
        match RevertCommand.execute("zzz", &mut ctx).await {
            CommandResult::Error(e) => {
                assert!(e.contains("No turn found matching 'zzz'"), "{}", e);
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    // ---- per-prompt undo helpers ------------------------------------------

    /// User message carrying a tool result (must NOT count as a prompt
    /// boundary for /undo).
    fn tool_result_user(tool_use_id: &str) -> Message {
        Message::user_blocks(vec![clawde_core::types::ContentBlock::ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: clawde_core::types::ToolResultContent::Text("ok".into()),
            is_error: None,
        }])
    }

    #[test]
    fn is_user_prompt_distinguishes_prompts_from_tool_results() {
        assert!(is_user_prompt(&Message::user("hello")));
        assert!(!is_user_prompt(&Message::assistant("hi")));
        assert!(!is_user_prompt(&tool_result_user("call_1")));
    }

    #[test]
    fn undo_group_bounds_by_prompt() {
        let messages = vec![
            Message::user("task one"),
            assistant_with_patch("aaa", &["a.rs"]),
            Message::user("task two"),
            assistant_with_patch("bbb", &["b.rs"]),
        ];
        let g1 = undo_group(&messages, 1).unwrap();
        assert_eq!(g1.prompt_index, 2);
        assert_eq!(g1.patched, vec![3]);
        let g2 = undo_group(&messages, 2).unwrap();
        assert_eq!(g2.prompt_index, 0);
        assert_eq!(g2.patched, vec![1, 3]);
    }

    #[test]
    fn undo_group_skips_tool_result_rounds() {
        let messages = vec![
            Message::user("task"),
            assistant_with_patch("aaa", &["a.rs"]),
            tool_result_user("call_1"),
            assistant_with_patch("bbb", &["b.rs"]),
        ];
        let g = undo_group(&messages, 1).unwrap();
        // The tool-result round is NOT a prompt boundary: one task, two
        // patched turns.
        assert_eq!(g.prompt_index, 0);
        assert_eq!(g.patched, vec![1, 3]);
    }

    #[test]
    fn undo_group_out_of_range_is_none() {
        let messages = vec![
            Message::user("task"),
            assistant_with_patch("aaa", &["a.rs"]),
        ];
        assert!(undo_group(&messages, 2).is_none());
        assert!(undo_group(&messages, 0).is_none());
    }

    // ---- /undo -------------------------------------------------------------

    #[tokio::test]
    async fn undo_without_patches_is_informative() {
        let repo = make_repo();
        let mut ctx = make_ctx(repo.path().to_path_buf(), vec![]);
        match UndoCommand.execute("", &mut ctx).await {
            CommandResult::Message(m) => {
                assert!(m.contains("Nothing to undo: no prompts yet"), "{}", m);
            }
            other => panic!("expected Message, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn undo_out_of_range_reports_prompt_count() {
        let repo = make_repo();
        let mut ctx = make_ctx(
            repo.path().to_path_buf(),
            vec![Message::user("one"), assistant_with_patch("aaa", &["a.rs"])],
        );
        match UndoCommand.execute("5", &mut ctx).await {
            CommandResult::Error(e) => {
                assert!(e.contains("has 1"), "{}", e);
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn undo_preview_requires_confirmation() {
        let repo = make_repo();
        let mut ctx = make_ctx(
            repo.path().to_path_buf(),
            vec![
                Message::user("fix the bug in main"),
                assistant_with_patch("aaaaaaaaaaaa", &["src/main.rs", "src/lib.rs"]),
            ],
        );
        match UndoCommand.execute("", &mut ctx).await {
            CommandResult::Message(m) => {
                assert!(m.contains("fix the bug in main"), "{}", m);
                assert!(m.contains("main.rs"), "{}", m);
                assert!(m.contains("--yes"), "{}", m);
                assert!(!m.contains("Reverted"), "{}", m);
            }
            other => panic!("expected Message, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn undo_confirmed_reverts_last_prompt_group() {
        let repo = make_repo();
        let mut ctx = make_ctx(
            repo.path().to_path_buf(),
            vec![
                Message::user("task one"),
                assistant_with_patch("aaaaaaaaaaaa", &["a.rs", "b.rs"]),
                Message::user("task two"),
                assistant_with_patch("bbbbbbbbbbbb", &["c.rs"]),
            ],
        );
        // Confirmed undo of the LAST prompt reverts only its own work.
        match UndoCommand.execute("--yes", &mut ctx).await {
            CommandResult::Message(m) => {
                assert!(m.contains("Reverted 1 file(s)"), "{}", m);
                assert!(m.contains("since your prompt"), "{}", m);
            }
            other => panic!("expected Message, got {:?}", other),
        }
        // Going back two prompts reverts BOTH groups (everything after the
        // second-to-last prompt): a.rs + b.rs + c.rs = 3 files.
        match UndoCommand.execute("2 --yes", &mut ctx).await {
            CommandResult::Message(m) => {
                assert!(m.contains("Reverted 3 file(s)"), "{}", m);
            }
            other => panic!("expected Message, got {:?}", other),
        }
    }

    // ---- /snapshot ---------------------------------------------------------

    #[tokio::test]
    async fn snapshot_without_checkpoints_is_informative() {
        let repo = make_repo();
        let mut ctx = make_ctx(repo.path().to_path_buf(), vec![]);
        match SnapshotDiffCommand.execute("", &mut ctx).await {
            CommandResult::Message(m) => {
                assert!(m.contains("No snapshot checkpoints recorded yet"), "{}", m);
            }
            other => panic!("expected Message, got {:?}", other),
        }
    }

    // ---- /checkpoints descriptions -----------------------------------------

    /// Redirect `CLAWDE_HOME` to a fresh temp dir for the lifetime of the
    /// guard, serialised on the shared crate lock so parallel tests never
    /// race the env var (the checkpoint-description cache writes under the
    /// config dir).
    struct TestHome {
        _lock: std::sync::MutexGuard<'static, ()>,
        _tmp: tempfile::TempDir,
        prev: Option<std::ffi::OsString>,
    }

    impl TestHome {
        fn new() -> Self {
            let lock = crate::tests::CLAWDE_HOME_LOCK
                .get_or_init(|| std::sync::Mutex::new(()))
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let prev = std::env::var_os("CLAWDE_HOME");
            let tmp = tempfile::tempdir().unwrap();
            std::env::set_var("CLAWDE_HOME", tmp.path());
            TestHome {
                _lock: lock,
                _tmp: tmp,
                prev,
            }
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("CLAWDE_HOME", v),
                None => std::env::remove_var("CLAWDE_HOME"),
            }
        }
    }

    /// Provider that returns a canned uuid -> description JSON map, mirroring
    /// the `CannedSpecProvider` pattern used by /spec tests.
    struct CannedDescriptionProvider;

    #[async_trait::async_trait]
    impl clawde_api::LlmProvider for CannedDescriptionProvider {
        fn id(&self) -> &clawde_core::ProviderId {
            static ID: std::sync::LazyLock<clawde_core::ProviderId> =
                std::sync::LazyLock::new(|| clawde_core::ProviderId::new("canned-desc"));
            &ID
        }
        fn name(&self) -> &str {
            "canned-desc"
        }
        async fn create_message(
            &self,
            _request: clawde_api::ProviderRequest,
        ) -> Result<clawde_api::ProviderResponse, clawde_api::ProviderError> {
            Ok(clawde_api::ProviderResponse {
                id: "canned-desc".into(),
                model: "canned".into(),
                content: vec![clawde_core::ContentBlock::Text {
                    text: r#"{"aaaaaaaaaaaa":"Added the fix to main.","bbbbbbbbbbbb":"Refactored helpers."}"#
                        .to_string(),
                }],
                stop_reason: clawde_api::StopReason::EndTurn,
                usage: Default::default(),
                rate_limit: None,
            })
        }
        async fn create_message_stream(
            &self,
            _request: clawde_api::ProviderRequest,
        ) -> Result<
            std::pin::Pin<
                Box<
                    dyn futures::Stream<
                            Item = Result<clawde_api::StreamEvent, clawde_api::ProviderError>,
                        > + Send,
                >,
            >,
            clawde_api::ProviderError,
        > {
            unimplemented!("canned description provider does not support streaming")
        }
        async fn health_check(
            &self,
        ) -> Result<clawde_api::ProviderStatus, clawde_api::ProviderError> {
            Ok(clawde_api::ProviderStatus::Healthy)
        }
        fn capabilities(&self) -> clawde_api::ProviderCapabilities {
            clawde_api::ProviderCapabilities {
                streaming: false,
                tool_calling: false,
                thinking: false,
                image_input: false,
                pdf_input: false,
                audio_input: false,
                video_input: false,
                caching: false,
                structured_output: false,
                system_prompt_style: clawde_api::SystemPromptStyle::TopLevel,
            }
        }
    }

    #[tokio::test]
    async fn checkpoints_renders_generated_descriptions() {
        let _home = TestHome::new();
        let repo = make_repo();
        let mut ctx = make_ctx(
            repo.path().to_path_buf(),
            vec![
                assistant_with_patch("aaaaaaaaaaaa", &["main.rs"]),
                Message::user("second task"),
                assistant_with_patch("bbbbbbbbbbbb", &["helpers.rs"]),
            ],
        );
        ctx.test_provider = Some(std::sync::Arc::new(CannedDescriptionProvider));
        match CheckpointsCommand.execute("", &mut ctx).await {
            CommandResult::Message(m) => {
                assert!(m.contains("2 checkpoint(s):"), "{}", m);
                assert!(m.contains("Added the fix to main."), "{}", m);
                assert!(m.contains("Refactored helpers."), "{}", m);
            }
            other => panic!("expected Message, got {:?}", other),
        }
        // The cache was persisted next to the (virtual) transcript.
        let project_root = clawde_core::git_utils::project_root(repo.path());
        let cache_path = checkpoint_cache_path(&project_root, "test-session").expect("cache path");
        let cached = load_checkpoint_cache(&cache_path).await;
        assert_eq!(
            cached.get("aaaaaaaaaaaa").map(String::as_str),
            Some("Added the fix to main.")
        );
    }

    #[tokio::test]
    async fn checkpoints_falls_back_to_stats_without_provider() {
        let _home = TestHome::new();
        let repo = make_repo();
        let mut ctx = make_ctx(
            repo.path().to_path_buf(),
            vec![assistant_with_patch("aaaaaaaaaaaa", &["main.rs"])],
        );
        // No test_provider and default config has no credentials -> the
        // best-effort generation is skipped and raw stats are shown.
        match CheckpointsCommand.execute("", &mut ctx).await {
            CommandResult::Message(m) => {
                assert!(m.contains("1 checkpoint(s):"), "{}", m);
                assert!(m.contains("main.rs"), "{}", m);
                assert!(!m.contains("Added the fix"), "{}", m);
            }
            other => panic!("expected Message, got {:?}", other),
        }
    }
}
