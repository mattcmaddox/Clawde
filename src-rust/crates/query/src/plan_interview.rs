//! Plan Mode interview (spec `docs/plans/reasoning-improvements-spec.md` §6.4).
//!
//! Plan Mode is already read-only: `PermissionManager::evaluate` denies every
//! non-read-only call while the session's mode is `PermissionMode::Plan`, so no
//! write can happen before the user approves the work. What Plan Mode never had
//! is an instruction for what to do *instead* — the model presented a plan in
//! prose and waited, with nothing requiring it to surface the questions the code
//! could not answer. This module supplies that instruction, plus the gate that
//! keeps it from turning every Plan Mode session into an interrogation.
//!
//! Two rules shape it:
//!
//! - **Gated to non-trivial tasks.** §9 Q4's research is explicit that always-on
//!   planning friction destroys the benefit of planning; a read-only lookup or a
//!   mechanical one-liner in Plan Mode stays an ordinary turn. The gate reuses
//!   the §6.1 non-trivial definition
//!   ([`DepthSignals::non_trivial`]), so "what needs the ceremony" is answered in
//!   exactly one place rather than by a second classifier.
//! - **Never in a run that cannot answer.** Non-interactive runs (`--print`,
//!   CI, gateway) have no user to reply — `AskUserQuestionTool` errors there — so
//!   the interview would only add instructions the model cannot follow.
//!
//! The block is prompt-level guidance, not a new loop primitive: nothing here
//! pauses the loop or rewrites tool permissions. The enforcement that pairs with
//! it is the Plan permission mode itself.
//!
//! Spec-mode (`Config::spec_mode`) is deliberately *not* a trigger: it enforces
//! its own approval by requiring an accepted `/spec` artifact for the task, which
//! is already a confirmation step.

use clawde_api::providers::reasoning_depth::DepthSignals;

/// The Plan Mode interview procedure, injected as per-turn system guidance.
pub const INTERVIEW_BLOCK: &str = "\
## Plan Mode interview

Plan Mode is active and this task is non-trivial, so interview the user before
touching anything:

1. Gather the context you need first: read the files the task names and the
   patterns they follow. Read-only work is allowed; writes are not.
2. Present a short plan together with its open questions — goal, approach, the
   files you expect to touch, the risks, and anything the code did not answer.
   A few lines, not an essay.
3. Ask about the genuinely ambiguous items — requirements that conflict or are
   underspecified, a change that is wide-reaching or hard to reverse, or several
   designs with materially different tradeoffs — through the AskUserQuestion
   tool. One question at a time. Never ask about mechanical or obvious choices.
4. Then stop and wait for confirmation. Do not write, edit, or run anything that
   changes state until the user confirms the plan; if they adjust it, revise and
   confirm again.";

/// Whether this turn is a Plan Mode interview.
///
/// `plan_mode` is the session's live permission mode; `non_interactive` is the
/// run's ability to surface a question at all.
pub fn should_interview(plan_mode: bool, non_interactive: bool, signals: &DepthSignals) -> bool {
    plan_mode && !non_interactive && signals.non_trivial()
}

/// The interview guidance for this turn, if it applies.
///
/// `None` for an ordinary turn — non-Plan-Mode, a trivial task, or a run that
/// cannot ask.
pub fn guidance_for_turn(
    plan_mode: bool,
    non_interactive: bool,
    signals: &DepthSignals,
) -> Option<&'static str> {
    should_interview(plan_mode, non_interactive, signals).then_some(INTERVIEW_BLOCK)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals(task: clawde_api::providers::free::TaskType) -> DepthSignals {
        DepthSignals {
            task,
            prompt_chars: 80,
            files_mentioned: 1,
            retry_after_failure: false,
        }
    }

    #[test]
    fn plan_mode_on_a_non_trivial_task_interviews() {
        let code = signals(clawde_api::providers::free::TaskType::CodeEdit);
        assert!(should_interview(true, false, &code));
        let block = guidance_for_turn(true, false, &code).expect("interview guidance");
        // The four steps of §6.4, in order: context, plan + open questions, the
        // ambiguous items via the ask tool, then wait for confirmation.
        assert!(block.contains("## Plan Mode interview"));
        assert!(block.contains("Gather the context"));
        assert!(block.contains("AskUserQuestion"));
        assert!(block.contains("wait for confirmation"));
        assert!(block.contains("Do not write, edit, or run anything"));
    }

    #[test]
    fn only_plan_mode_interviews() {
        let code = signals(clawde_api::providers::free::TaskType::CodeEdit);
        assert!(!should_interview(false, false, &code));
        assert!(guidance_for_turn(false, false, &code).is_none());
    }

    #[test]
    fn a_trivial_task_is_never_interviewed() {
        for task in [
            clawde_api::providers::free::TaskType::Search,
            clawde_api::providers::free::TaskType::SimpleEdit,
        ] {
            let trivial = signals(task);
            assert!(!should_interview(true, false, &trivial), "{task:?}");
            assert!(
                guidance_for_turn(true, false, &trivial).is_none(),
                "{task:?}"
            );
        }
    }

    #[test]
    fn a_run_that_cannot_ask_never_interviews() {
        let code = signals(clawde_api::providers::free::TaskType::CodeEdit);
        assert!(!should_interview(true, true, &code));
        assert!(guidance_for_turn(true, true, &code).is_none());
    }
}
