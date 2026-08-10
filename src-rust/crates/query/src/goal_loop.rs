// goal_loop.rs — Goal continuation engine for the /goal feature.
//
// `check_and_continue_goal` is called by the CLI REPL after each query loop
// turn completes.  When an active goal exists it:
//   1. Checks runaway / budget guards
//   2. Records the turn in the GoalStore
//   3. Returns `GoalContinuation::Continue { message }` with the continuation
//      user message to inject, signalling the caller to dispatch another turn.
//
// The caller (cli/src/main.rs) is responsible for the actual dispatch so that
// TUI event handling and cancellation tokens stay in the right place.

use clawde_core::{goal_continuation_message, GoalStatus, GoalStore, MAX_GOAL_TURNS};

/// Minimum model output tokens for a goal turn to count as progress. A turn
/// that generates less than this and writes no files is a no-progress turn
/// (arXiv:2607.00038's no-progress detector).
pub const NO_PROGRESS_TOKEN_THRESHOLD: u64 = 500;
/// Consecutive no-progress turns before the goal pauses as stalled.
pub const NO_PROGRESS_STALL_TURNS: u32 = 3;

/// Result returned to the caller after a completed query loop turn.
pub enum GoalContinuation {
    /// Inject this user message and run another turn.
    Continue { message: String },
    /// Goal is done (complete, paused, cleared, budget hit, runaway).
    Stop { reason: StopReason },
    /// No goal is set for this session.
    NoGoal,
}

#[derive(Debug, Clone)]
pub enum StopReason {
    GoalComplete,
    Paused,
    BudgetLimited,
    RunawayGuard {
        turns_used: u32,
    },
    /// The loop generated negligible output and no writes across several
    /// consecutive turns; paused rather than burning unlimited turns.
    Stalled {
        turns_used: u32,
    },
    Error(String),
}

impl StopReason {
    pub fn user_message(&self) -> Option<String> {
        match self {
            StopReason::GoalComplete => Some("Goal marked complete by the model.".to_string()),
            StopReason::Paused => None, // user-initiated, no extra message needed
            StopReason::BudgetLimited => Some(
                "Soft token budget reached — goal paused. Use /goal resume to continue."
                    .to_string(),
            ),
            StopReason::RunawayGuard { turns_used } => Some(format!(
                "Goal paused after {} turns (runaway guard). Use /goal resume to continue.",
                turns_used
            )),
            StopReason::Stalled { turns_used } => Some(format!(
                "Goal paused after {} turns without measurable progress (no-progress guard). Use /goal resume to continue.",
                turns_used
            )),
            StopReason::Error(msg) => Some(format!("Goal error: {}", msg)),
        }
    }
}

/// Inspect the current goal for `session_id` after a completed turn and decide
/// whether to continue.
///
/// `total_tokens_used` is the session-wide cumulative token count from the
/// cost tracker. The goal's own counter is fed the per-turn delta from it
/// (G7) and the soft budget is enforced against that goal-scoped counter.
/// `turn_elapsed_secs` is how long this turn took (for time accounting).
/// `turn_output_tokens` and `turn_made_writes` feed the no-progress guard.
pub fn check_and_continue_goal(
    session_id: &str,
    total_tokens_used: u64,
    turn_elapsed_secs: u64,
    turn_output_tokens: u64,
    turn_made_writes: bool,
) -> GoalContinuation {
    let store = match GoalStore::open_default() {
        Some(s) => s,
        None => return GoalContinuation::NoGoal,
    };

    decide_goal_continuation(
        &store,
        session_id,
        total_tokens_used,
        turn_elapsed_secs,
        turn_output_tokens,
        turn_made_writes,
    )
}

/// Guard/decision core of [`check_and_continue_goal`], operating on an explicit
/// [`GoalStore`] so the runaway / budget / no-progress guards can be exercised
/// against an in-memory store in tests. `check_and_continue_goal` is the
/// production wrapper that opens the default store.
pub fn decide_goal_continuation(
    store: &GoalStore,
    session_id: &str,
    total_tokens_used: u64,
    turn_elapsed_secs: u64,
    turn_output_tokens: u64,
    turn_made_writes: bool,
) -> GoalContinuation {
    let goal = match store.get_goal(session_id) {
        Some(g) => g,
        None => return GoalContinuation::NoGoal,
    };

    // If model (or user) already marked complete/paused, stop.
    match goal.status {
        GoalStatus::Complete => {
            return GoalContinuation::Stop {
                reason: StopReason::GoalComplete,
            };
        }
        GoalStatus::Paused => {
            return GoalContinuation::Stop {
                reason: StopReason::Paused,
            };
        }
        GoalStatus::BudgetLimited => {
            return GoalContinuation::Stop {
                reason: StopReason::BudgetLimited,
            };
        }
        GoalStatus::Active => {}
    }

    // Runaway guard: check before incrementing so first fire is at MAX_GOAL_TURNS.
    if goal.turns_used >= MAX_GOAL_TURNS {
        let _ = store.set_status(session_id, GoalStatus::Paused);
        return GoalContinuation::Stop {
            reason: StopReason::RunawayGuard {
                turns_used: goal.turns_used,
            },
        };
    }

    // G7: goal-scoped token accounting. Record this turn's token delta on the
    // goal, then enforce the soft budget against the goal's OWN counter so a
    // goal created after heavy session usage does not trip its budget
    // immediately (session-wide usage used to be compared against the goal
    // budget — that is the latent bug this replaces).
    //
    // Legacy-goal migration: a goal created before the baseline column existed
    // reads `turns_used > 0` with `last_session_tokens == 0` and an unfed
    // `tokens_used`. Prime its baseline on the first post-upgrade turn so the
    // entire session so far is not attributed to the goal (which would trip a
    // small budget once, reproducing the old bug). Fresh goals always have a
    // correctly seeded baseline, so the discriminator never fires for them.
    if goal.turns_used > 0 && goal.last_session_tokens == 0 && goal.tokens_used == 0 {
        if let Err(e) = store.rebaseline_tokens(session_id, total_tokens_used) {
            return GoalContinuation::Stop {
                reason: StopReason::Error(e.to_string()),
            };
        }
    } else if let Err(e) = store.record_token_usage(session_id, total_tokens_used) {
        return GoalContinuation::Stop {
            reason: StopReason::Error(e.to_string()),
        };
    }
    let goal = match store.get_goal(session_id) {
        Some(g) => g,
        None => return GoalContinuation::NoGoal,
    };
    if goal.is_over_budget(goal.tokens_used) {
        let _ = store.set_status(session_id, GoalStatus::BudgetLimited);
        return GoalContinuation::Stop {
            reason: StopReason::BudgetLimited,
        };
    }

    // No-progress guard: several consecutive turns with negligible model
    // output and no file writes suggest the loop is spinning without making
    // progress. Pause as `stalled` instead of burning unlimited turns. A turn
    // that wrote files or produced substantial output resets the streak.
    let stalled = turn_output_tokens < NO_PROGRESS_TOKEN_THRESHOLD && !turn_made_writes;
    let streak = if stalled {
        goal.low_progress_streak + 1
    } else {
        0
    };
    if let Err(e) = store.set_low_progress_streak(session_id, streak) {
        return GoalContinuation::Stop {
            reason: StopReason::Error(e.to_string()),
        };
    }
    if streak >= NO_PROGRESS_STALL_TURNS {
        let _ = store.set_status(session_id, GoalStatus::Paused);
        return GoalContinuation::Stop {
            reason: StopReason::Stalled {
                turns_used: goal.turns_used,
            },
        };
    }

    // Record this turn.
    if let Err(e) = store.record_turn(session_id, turn_elapsed_secs) {
        return GoalContinuation::Stop {
            reason: StopReason::Error(e.to_string()),
        };
    }

    // Reload after the update so turns_used is current.
    let goal = match store.get_goal(session_id) {
        Some(g) => g,
        None => return GoalContinuation::NoGoal,
    };

    // Build the continuation message.
    let message = goal_continuation_message(&goal);
    GoalContinuation::Continue { message }
}

/// Called by GoalCompleteTool to mark the goal complete.
pub fn mark_goal_complete(session_id: &str) -> Result<(), String> {
    let store = GoalStore::open_default().ok_or_else(|| "Could not open goal store".to_string())?;
    store
        .set_status(session_id, GoalStatus::Complete)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_tmp() -> GoalStore {
        GoalStore::open(std::path::Path::new(":memory:")).unwrap()
    }

    #[test]
    fn goal_created_after_heavy_session_usage_does_not_trip_budget() {
        // G7 regression: the session has already burned 100K tokens when the
        // goal (budget 50K) is created. Goal-scoped accounting must only count
        // the ~400 tokens consumed during the goal's own turns, so the goal
        // keeps running instead of tripping its budget on turn one.
        let store = open_tmp();
        store
            .set_goal("sess", "finish the feature", Some(50_000), 100_000)
            .unwrap();

        let decision = decide_goal_continuation(&store, "sess", 100_400, 12, 900, true);
        assert!(
            matches!(decision, GoalContinuation::Continue { .. }),
            "goal-scoped usage (400) must stay under the 50K budget"
        );
        let goal = store.get_goal("sess").unwrap();
        assert_eq!(goal.tokens_used, 400);
        assert_eq!(goal.turns_used, 1);
    }

    #[test]
    fn legacy_goal_baseline_is_primed_not_attributed() {
        // A goal created before the baseline column existed reads
        // last_session_tokens == 0 with turns already recorded and an unfed
        // tokens_used. The first post-upgrade turn must prime the baseline,
        // not attribute the entire session so far to the goal (which would
        // trip a small budget once, reproducing the old bug).
        let store = open_tmp();
        store
            .set_goal("sess", "legacy goal", Some(50_000), 0)
            .unwrap();
        store.record_turn("sess", 10).unwrap();
        store.record_turn("sess", 10).unwrap();
        let pre = store.get_goal("sess").unwrap();
        assert_eq!(pre.turns_used, 2);
        assert_eq!(pre.last_session_tokens, 0);
        assert_eq!(pre.tokens_used, 0);

        // The session has burned 100K tokens by the first post-upgrade turn.
        let decision = decide_goal_continuation(&store, "sess", 100_000, 5, 900, true);
        assert!(
            matches!(decision, GoalContinuation::Continue { .. }),
            "a legacy goal must not trip its 50K budget from session-wide usage"
        );
        let goal = store.get_goal("sess").unwrap();
        assert_eq!(goal.tokens_used, 0, "baseline primed without adding usage");
        assert_eq!(goal.last_session_tokens, 100_000);
    }

    #[test]
    fn goal_trips_budget_only_once_goal_scoped_usage_reaches_it() {
        let store = open_tmp();
        store.set_goal("sess", "big task", Some(100), 0).unwrap();

        // 300 tokens past the baseline exceed the 100-token budget.
        let decision = decide_goal_continuation(&store, "sess", 300, 1, 900, true);
        assert!(matches!(
            decision,
            GoalContinuation::Stop {
                reason: StopReason::BudgetLimited
            }
        ));
        assert_eq!(
            store.get_goal("sess").unwrap().status,
            GoalStatus::BudgetLimited
        );
    }

    #[test]
    fn no_progress_stalls_after_three_low_turns() {
        let store = open_tmp();
        store.set_goal("sess", "spin goal", None, 0).unwrap();

        // Three consecutive turns: negligible output, no writes.
        for i in 1..=3 {
            let total = 1_000 + (i as u64) * 50; // session advances a little
            let decision = decide_goal_continuation(&store, "sess", total, 2, 40, false);
            match i {
                1 | 2 => assert!(
                    matches!(decision, GoalContinuation::Continue { .. }),
                    "streak {i} must still continue"
                ),
                3 => assert!(matches!(
                    decision,
                    GoalContinuation::Stop {
                        reason: StopReason::Stalled { .. }
                    }
                )),
                _ => unreachable!(),
            }
        }
        assert_eq!(
            store.get_goal("sess").unwrap().status,
            GoalStatus::Paused,
            "stalled goal is persisted as paused"
        );
    }

    #[test]
    fn writing_turn_resets_no_progress_streak() {
        let store = open_tmp();
        store.set_goal("sess", "loop goal", None, 0).unwrap();

        // Two low turns push the streak to 2…
        decide_goal_continuation(&store, "sess", 100, 1, 30, false);
        decide_goal_continuation(&store, "sess", 150, 1, 40, false);
        assert_eq!(store.get_goal("sess").unwrap().low_progress_streak, 2);

        // …a writing turn (even with low output) resets it to 0.
        let decision = decide_goal_continuation(&store, "sess", 250, 1, 60, true);
        assert!(matches!(decision, GoalContinuation::Continue { .. }));
        assert_eq!(store.get_goal("sess").unwrap().low_progress_streak, 0);

        // And the loop needs a fresh 3-strike after the reset.
        decide_goal_continuation(&store, "sess", 300, 1, 30, false);
        decide_goal_continuation(&store, "sess", 350, 1, 30, false);
        let decision = decide_goal_continuation(&store, "sess", 400, 1, 30, false);
        assert!(matches!(
            decision,
            GoalContinuation::Stop {
                reason: StopReason::Stalled { .. }
            }
        ));
    }

    #[test]
    fn substantial_output_counts_as_progress_even_without_writes() {
        let store = open_tmp();
        store.set_goal("sess", "read-heavy goal", None, 0).unwrap();

        for i in 1..=5 {
            let total = (i as u64) * 1_000;
            let decision = decide_goal_continuation(&store, "sess", total, 1, 2_000, false);
            assert!(
                matches!(decision, GoalContinuation::Continue { .. }),
                "substantial output must keep the loop running (turn {i})"
            );
        }
    }
}
