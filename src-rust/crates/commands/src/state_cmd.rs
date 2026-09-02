// /state — inspect the agent's task-state projection.
//
// The TaskState projection (objective, focus, verified evidence, failures,
// complexity counters) normally feeds only the model's system prompt via
// `<task_context>`. This command makes it human-visible: it rebuilds the
// projection from the session's transcript exactly the way the query loop's
// init does — snapshot-aware, cut-aware, branch-aware — and renders it.
//
// It is a pure viewer: reconstruction failures (missing transcript, malformed
// lines) degrade to the transcript-less projection rather than an error —
// the same silent degradation the query loop performs.

use crate::{CommandContext, CommandResult, SlashCommand};
use clawde_core::session_storage::{load_state_snapshot, state_events_from_transcript, StateEvent};
use clawde_query::TaskState;

/// Rebuild the current `TaskState` for `session_id` from its transcript.
///
/// Mirrors `run_query_loop`'s init routes in priority order: snapshot+tail
/// when a valid snapshot exists, full event replay otherwise, and the
/// pre-event `from_messages` projection when no events exist.
pub async fn reconstruct_state(
    working_dir: &std::path::Path,
    session_id: &str,
    messages: &[clawde_core::types::Message],
) -> TaskState {
    let fallback = || TaskState::from_messages(messages);
    let project_root = clawde_core::git_utils::project_root(working_dir);
    let Ok(path) = clawde_core::session_storage::transcript_path(&project_root, session_id) else {
        return fallback();
    };
    match load_state_snapshot(&path).await {
        Ok(Some((snapshot, tail))) => TaskState::replay_with_snapshot(messages, &snapshot, &tail),
        _ => {
            let events: Vec<StateEvent> =
                match clawde_core::session_storage::load_transcript(&path).await {
                    Ok(entries) => state_events_from_transcript(&entries)
                        .into_iter()
                        .cloned()
                        .collect(),
                    Err(_) => Vec::new(),
                };
            if events.is_empty() {
                fallback()
            } else {
                TaskState::replay(messages, &events)
            }
        }
    }
}

/// `/state` — show what the agent believes it is doing.
pub struct StateCommand;

#[async_trait::async_trait]
impl SlashCommand for StateCommand {
    fn name(&self) -> &str {
        "state"
    }

    fn description(&self) -> &str {
        "Show the agent's tracked task state (objective, focus, evidence)"
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        let state = reconstruct_state(&ctx.working_dir, &ctx.session_id, &ctx.messages).await;
        CommandResult::Message(format!(
            "Task State\n═══════════\n{}\n\n(Rebuilt from this session's persisted state events and messages — the same projection the model sees as <task_context>.)",
            state.render()
        ))
    }
}
