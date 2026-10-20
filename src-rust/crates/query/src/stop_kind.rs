//! Why a model turn stopped, and whether the loop should resume it.
//!
//! `StopReason` is the wire-level reason a provider reports. This module turns
//! that plus the assembled turn into a policy decision: a turn cut at the
//! upstream's own tool-call cap, or one whose tool batch was truncated at the
//! output limit, must not end the run as a clean `end_turn`. Those kinds feed
//! the same bounded continuation ladder the local step budget uses
//! (`MAX_STEP_CONTINUATIONS` in `lib.rs`), so a mid-task stop resumes with
//! tools instead of returning cold.
//!
//! Provider adapters pass unknown wire reasons through `StopReason::Other`
//! verbatim, so spellings differ (`max_tool_calls`, `MAX_TOOL_CALLS`,
//! `tool-call-limit`). Comparison here is case-insensitive and treats `-` and
//! spaces as `_`.

use clawde_core::types::{ContentBlock, ToolResultContent};

/// Policy-relevant classification of a stopped model turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopKind {
    /// Natural completion. Nothing to resume.
    EndTurn,
    /// The model asked for tools and the batch is complete. The loop already
    /// executes the calls regardless of the stop string some providers send
    /// (`stop` instead of `tool_use`), so this only records the shape.
    ToolBatch,
    /// Output-token cap with no tool call in flight. The existing
    /// `MAX_TOKENS_RECOVERY_*` loop resumes it with a plain continuation nudge.
    OutputCap,
    /// Output-token cap, or a `tool_use` signal with no calls, left the tool
    /// batch truncated. Any calls received were cut mid-argument: they must be
    /// answered in the transcript but never executed.
    TruncatedToolBatch,
    /// The upstream's own tool/function-call cap ended the turn mid-task.
    ProviderToolCap,
    /// Safety filtering ended the turn; resuming would resend the same content.
    ContentFiltered,
    /// Unrecognized stop string; the loop treats it as `EndTurn`.
    Unknown,
}

impl StopKind {
    /// Short human-readable label for status lines, logs, and the model-switch
    /// notice the TUI shows.
    pub fn label(self) -> &'static str {
        match self {
            StopKind::EndTurn => "end turn",
            StopKind::ToolBatch => "tool batch",
            StopKind::OutputCap => "output cap",
            StopKind::TruncatedToolBatch => "truncated tool batch",
            StopKind::ProviderToolCap => "provider tool-call cap",
            StopKind::ContentFiltered => "content filtered",
            StopKind::Unknown => "unknown stop",
        }
    }

    /// Whether this stop cut a task short and the ladder should resume it.
    pub fn is_resumable_cap(self) -> bool {
        matches!(
            self,
            StopKind::ProviderToolCap | StopKind::TruncatedToolBatch
        )
    }
}

/// Upstream spellings that mean "my tool/function-call cap stopped this turn".
/// The gateway's own chat-completions agent loop reports `max_tool_calls` as
/// its `incomplete` reason, and OpenRouter-style proxies pass the same word
/// through, so the spelling is treated as protocol rather than one vendor's
/// quirk.
const PROVIDER_TOOL_CAPS: &[&str] = &[
    "max_tool_calls",
    "max_tool_use",
    "max_function_calls",
    "tool_call_limit",
    "tool_calls_limit",
    "tool_use_limit",
    "function_call_limit",
    "function_calls_limit",
];

/// Stop strings that mean the turn finished normally.
const END_TURNS: &[&str] = &["", "end_turn", "stop", "stop_sequence", "completed"];

/// Stop strings that mean the model produced a tool batch.
const TOOL_STOPS: &[&str] = &["tool_use", "tool_calls", "function_call", "function_calls"];

/// Stop strings that mean the output-token budget ran out.
const OUTPUT_CAPS: &[&str] = &["max_tokens", "length", "max_output_tokens"];

/// Stop strings that mean the provider's safety system removed the content.
const CONTENT_FILTERS: &[&str] = &["content_filter", "content_filtered", "safety", "recitation"];

/// Normalize a stop string for comparison.
fn normalized(stop_reason: &str) -> String {
    stop_reason
        .trim()
        .to_ascii_lowercase()
        .replace(['-', ' '], "_")
}

fn matches(stop: &str, spellings: &[&str]) -> bool {
    spellings.contains(&stop)
}

/// Classify a stopped turn from its wire stop string and the number of tool
/// calls the turn actually produced.
///
/// `tool_call_count` is what matters for the output cap: the same `max_tokens`
/// is a plain truncation when no tool was in flight and a truncated *batch*
/// when one was, and only the latter needs the synthetic `tool_result` repair.
pub fn classify(stop_reason: &str, tool_call_count: usize) -> StopKind {
    let stop = normalized(stop_reason);
    if matches(&stop, PROVIDER_TOOL_CAPS) {
        return StopKind::ProviderToolCap;
    }
    if matches(&stop, OUTPUT_CAPS) {
        return if tool_call_count > 0 {
            StopKind::TruncatedToolBatch
        } else {
            StopKind::OutputCap
        };
    }
    if matches(&stop, TOOL_STOPS) {
        // The model signalled tool use. With no calls assembled, the batch was
        // dropped rather than delivered — the same hazard as a truncation.
        return if tool_call_count == 0 {
            StopKind::TruncatedToolBatch
        } else {
            StopKind::ToolBatch
        };
    }
    if matches(&stop, END_TURNS) {
        return StopKind::EndTurn;
    }
    if matches(&stop, CONTENT_FILTERS) {
        return StopKind::ContentFiltered;
    }
    StopKind::Unknown
}

/// Synthetic `tool_result` blocks for a truncated batch, plus the names of the
/// calls they answer.
///
/// The transcript must keep one `tool_result` per `tool_use`, but a call cut
/// mid-argument cannot be executed: its input is either invalid JSON or a
/// prefix of what the model meant. Each block therefore reports the drop as an
/// error and is never dispatched. The returned names let the resume prompt say
/// which calls the model still owes.
pub fn truncated_batch_repair(calls: &[(String, String)]) -> (Vec<ContentBlock>, Vec<String>) {
    let mut blocks = Vec::with_capacity(calls.len());
    let mut dropped = Vec::with_capacity(calls.len());
    for (id, name) in calls {
        dropped.push(name.clone());
        blocks.push(ContentBlock::ToolResult {
            tool_use_id: id.clone(),
            content: ToolResultContent::Text(format!(
                "Not executed: the provider stopped at its output limit while this {name} call \
                 was still being generated, so its arguments are incomplete."
            )),
            is_error: Some(true),
        });
    }
    (blocks, dropped)
}

/// The user-role prompt that resumes a turn ended by a provider-side cap.
///
/// Returns `None` for a kind the loop must not resume. The message names the
/// dropped work when the loop knows it, so the model re-issues only that work
/// instead of restarting the task or claiming the batch ran.
pub fn resume_message(kind: StopKind, dropped: &[String]) -> Option<String> {
    let dropped_detail = if dropped.is_empty() {
        String::new()
    } else {
        format!(
            " The tool calls that did not run were: {}.",
            dropped.join(", ")
        )
    };
    match kind {
        StopKind::ProviderToolCap => Some(format!(
            "The upstream provider stopped the previous turn at its own tool-call cap, so the \
             task is incomplete.{dropped_detail} Continue from the existing evidence and tool \
             results; do not restart completed work. Re-issue any calls that did not run, keep \
             each batch small enough for the provider's cap, and respond normally only when the \
             work is actually complete."
        )),
        StopKind::TruncatedToolBatch => Some(format!(
            "The previous turn was cut off mid tool call: the provider stopped at its output \
             limit while a tool call was still being generated.{dropped_detail} Those calls were \
             NOT executed — do not assume they ran. Re-issue them now, splitting large inputs \
             across smaller calls, then continue the same task from the existing evidence."
        )),
        StopKind::EndTurn
        | StopKind::ToolBatch
        | StopKind::OutputCap
        | StopKind::ContentFiltered
        | StopKind::Unknown => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{classify, resume_message, truncated_batch_repair, StopKind};
    use clawde_core::types::{ContentBlock, ToolResultContent};

    #[test]
    fn output_cap_with_no_tools_is_a_plain_truncation() {
        assert_eq!(classify("max_tokens", 0), StopKind::OutputCap);
        assert_eq!(classify("length", 0), StopKind::OutputCap);
    }

    #[test]
    fn output_cap_with_a_tool_call_is_a_truncated_batch() {
        assert_eq!(classify("max_tokens", 1), StopKind::TruncatedToolBatch);
        assert_eq!(classify("MAX_TOKENS", 2), StopKind::TruncatedToolBatch);
    }

    #[test]
    fn provider_tool_cap_spellings_all_classify() {
        for stop in [
            "max_tool_calls",
            "MAX_TOOL_CALLS",
            "max-tool-calls",
            "tool_call_limit",
            "function_call_limit",
            "max_function_calls",
        ] {
            assert_eq!(
                classify(stop, 0),
                StopKind::ProviderToolCap,
                "stop string {stop:?} must classify as a provider tool cap"
            );
        }
    }

    #[test]
    fn tool_use_with_no_calls_is_a_truncated_batch() {
        assert_eq!(classify("tool_use", 0), StopKind::TruncatedToolBatch);
        assert_eq!(classify("tool_calls", 0), StopKind::TruncatedToolBatch);
    }

    #[test]
    fn complete_forms_are_not_resumed() {
        assert_eq!(classify("end_turn", 0), StopKind::EndTurn);
        assert_eq!(classify("stop_sequence", 0), StopKind::EndTurn);
        assert_eq!(classify("tool_use", 1), StopKind::ToolBatch);
        assert_eq!(classify("content_filtered", 0), StopKind::ContentFiltered);
        assert_eq!(classify("something_else", 0), StopKind::Unknown);
        for kind in [
            StopKind::EndTurn,
            StopKind::ToolBatch,
            StopKind::OutputCap,
            StopKind::ContentFiltered,
            StopKind::Unknown,
        ] {
            assert!(!kind.is_resumable_cap(), "{kind:?} must not be resumed");
            assert!(
                resume_message(kind, &[]).is_none(),
                "{kind:?} must not produce a resume prompt"
            );
        }
    }

    #[test]
    fn truncated_batch_repair_answers_every_call_without_running_it() {
        let calls = vec![
            ("toolu_1".to_string(), "Edit".to_string()),
            ("toolu_2".to_string(), "Bash".to_string()),
        ];
        let (blocks, dropped) = truncated_batch_repair(&calls);

        assert_eq!(dropped, vec!["Edit".to_string(), "Bash".to_string()]);
        assert_eq!(blocks.len(), calls.len());
        for (block, (id, _)) in blocks.iter().zip(&calls) {
            let ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } = block
            else {
                panic!("repair must emit tool_result blocks, got {block:?}");
            };
            assert_eq!(tool_use_id, id);
            assert_eq!(*is_error, Some(true));
            let ToolResultContent::Text(text) = content else {
                panic!("repair result must be text, got {content:?}");
            };
            assert!(
                text.contains("Not executed"),
                "a repaired call must say it did not run: {text}"
            );
        }
    }

    #[test]
    fn resume_prompt_names_the_dropped_calls() {
        let dropped = vec!["Edit".to_string(), "Bash".to_string()];

        let cap = resume_message(StopKind::ProviderToolCap, &[]).expect("cap resumes");
        assert!(cap.contains("tool-call cap"));
        assert!(cap.contains("do not restart completed work"));

        let truncated =
            resume_message(StopKind::TruncatedToolBatch, &dropped).expect("truncation resumes");
        assert!(truncated.contains("NOT executed"));
        assert!(truncated.contains("Edit, Bash"));
    }
}
