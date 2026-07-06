//! Message-history invariant pass (issue #229 / MI-2).
//!
//! Several code paths — auto/reactive compaction, `max_tokens` recovery, and
//! command-queue / pending-message injection — can each independently mutate the
//! conversation into a shape that violates the provider API's structural rules.
//! The most damaging violation is a broken `tool_use` ↔ `tool_result` pairing:
//!
//!   * an orphan `tool_result` block whose originating `tool_use` was sliced away
//!     by a compaction cut, or
//!   * a dangling `tool_use` whose answering `tool_result` never arrived (the
//!     turn was interrupted, or a plain user message was injected before it could
//!     be answered).
//!
//! Anthropic and OpenAI both reject such histories with HTTP 400. There is no
//! single choke point in the pipeline that guarantees the invariants — this
//! module is that choke point. [`sanitize_history`] is a pure function over the
//! `Vec<Message>` that is about to be dispatched; the query loop runs it at the
//! request boundary so a malformed history can never reach the model, regardless
//! of which path produced it.
//!
//! This is a *safety net*: it does not (and must not) change compaction /
//! recovery / command-queue logic. It only repairs whatever those paths hand it.

use claurst_core::types::{ContentBlock, Message, MessageContent, Role, ToolResultContent};

/// Content used for a synthesized placeholder `tool_result` that answers a
/// dangling `tool_use`. Marked `is_error` so the model can tell it apart from a
/// genuine result.
const UNAVAILABLE_RESULT_MSG: &str = "[tool result unavailable]";

/// Enforce the provider-API message invariants on `messages`, returning a
/// repaired copy with balanced `tool_use` ↔ `tool_result` pairing.
///
/// Invariants enforced:
///
/// 1. **Pairing.** Every `tool_result` block must answer a `tool_use` (matched
///    by id) in the *immediately preceding* assistant message; orphan
///    `tool_result` blocks (whose `tool_use` is gone) are dropped. Every
///    `tool_use` in an assistant message must be answered by a `tool_result` in
///    the *immediately following* user message; for a **dangling** `tool_use`
///    (no matching result) a placeholder `tool_result` is **synthesized** rather
///    than dropping the `tool_use`. Dropping a `tool_use` risks desyncing the
///    turn (the assistant "said" it called a tool); synthesizing a placeholder
///    keeps the pairing balanced without rewriting the assistant's output.
/// 2. **No empty messages.** A message whose block list becomes empty after
///    orphan removal is dropped.
/// 3. **Order preserved.** Real turns are neither reordered nor merged; a
///    non-`tool_result` first message is preserved intact. Synthesized results
///    are only ever inserted directly after the assistant `tool_use` they answer.
///
/// The function is idempotent: a well-formed history passes through unchanged.
pub fn sanitize_history(messages: Vec<Message>) -> Vec<Message> {
    let n = messages.len();
    let mut out: Vec<Message> = Vec::with_capacity(n);
    let mut i = 0usize;

    while i < n {
        let msg = &messages[i];

        match msg.role {
            Role::Assistant => {
                let tool_use_ids = collect_tool_use_ids(msg);
                out.push(msg.clone());

                if tool_use_ids.is_empty() {
                    i += 1;
                    continue;
                }

                // The tool_use blocks in this assistant message MUST be answered
                // by tool_result blocks in the immediately following user message.
                let next_is_user = i + 1 < n && messages[i + 1].role == Role::User;

                if next_is_user {
                    // Merge (clean orphans + synthesize missing) into the
                    // existing following user message. This keeps a single
                    // answering user turn and avoids inserting a redundant one.
                    let answered = answer_user_message(&messages[i + 1], &tool_use_ids);
                    out.push(answered);
                    i += 2; // both this assistant and its answering user consumed
                } else {
                    // No user message follows (end of history, or an assistant
                    // message follows — itself already malformed). Insert a fresh
                    // user message carrying a synthesized result for every
                    // tool_use so the pairing is balanced.
                    let synth: Vec<ContentBlock> =
                        tool_use_ids.iter().map(|id| synth_tool_result(id)).collect();
                    out.push(Message::user_blocks(synth));
                    i += 1;
                }
            }
            Role::User => {
                // A user message reaching this arm is NOT the immediate answer to
                // an assistant `tool_use` (those are consumed in the Assistant
                // arm above). Therefore ANY `tool_result` block here is an orphan
                // — its `tool_use` is absent or non-adjacent — and must be dropped.
                if let MessageContent::Blocks(blocks) = &msg.content {
                    if blocks
                        .iter()
                        .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
                    {
                        let kept: Vec<ContentBlock> = blocks
                            .iter()
                            .filter(|b| !matches!(b, ContentBlock::ToolResult { .. }))
                            .cloned()
                            .collect();
                        // Invariant 2: drop messages emptied by block removal.
                        if !kept.is_empty() {
                            let mut m = msg.clone();
                            m.content = MessageContent::Blocks(kept);
                            out.push(m);
                        }
                        i += 1;
                        continue;
                    }
                }
                // No tool_result blocks — pass through unchanged (Text messages,
                // the first user task, injected commands, etc.).
                out.push(msg.clone());
                i += 1;
            }
        }
    }

    out
}

/// Build the user message that answers `tool_use_ids`, starting from the
/// existing `following` user message: drop any `tool_result` whose id is not in
/// `tool_use_ids` (orphans), keep every other block, then append a synthesized
/// placeholder for each id that is still unanswered (dangling).
///
/// A `Text` user message is promoted to blocks so the synthesized results sit
/// alongside the preserved text — this answers the `tool_use` without inserting
/// an extra user turn (which would break role alternation).
fn answer_user_message(following: &Message, tool_use_ids: &[String]) -> Message {
    let mut kept: Vec<ContentBlock> = match &following.content {
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => {
                    tool_use_ids.iter().any(|id| id == tool_use_id)
                }
                _ => true,
            })
            .cloned()
            .collect(),
        MessageContent::Text(t) => {
            if t.is_empty() {
                Vec::new()
            } else {
                vec![ContentBlock::Text { text: t.clone() }]
            }
        }
    };

    // Which ids are already answered by a surviving tool_result?
    let answered: Vec<String> = kept
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
            _ => None,
        })
        .collect();

    // Synthesize a placeholder for every dangling (unanswered) tool_use.
    for id in tool_use_ids {
        if !answered.iter().any(|a| a == id) {
            kept.push(synth_tool_result(id));
        }
    }

    let mut answered_msg = following.clone();
    answered_msg.content = MessageContent::Blocks(kept);
    answered_msg
}

/// Collect the ids of every `tool_use` block in `msg`, in order.
fn collect_tool_use_ids(msg: &Message) -> Vec<String> {
    match &msg.content {
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect(),
        MessageContent::Text(_) => Vec::new(),
    }
}

/// Build a synthesized placeholder `tool_result` for the given `tool_use` id.
fn synth_tool_result(tool_use_id: &str) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: ToolResultContent::Text(UNAVAILABLE_RESULT_MSG.to_string()),
        is_error: Some(true),
    }
}
