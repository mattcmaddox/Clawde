//! Degeneration guards for the query loop (spec
//! `docs/plans/reasoning-improvements-spec.md` §6.5).
//!
//! Three of the spec's four guards live here; the fourth — loop detection, the
//! same `(tool, args)` repeating with no state change — is already implemented
//! in [`crate::repeat_guard`], so it is not duplicated.
//!
//! - **Stall** (§6.5 #1): consecutive turns with negligible output, no tool
//!   calls, and no writes. Narrate first (one continuation turn asking the model
//!   for a progress update), then surface to the user if it does not answer.
//! - **Divergence** (§6.5 #3): the run's *strong* drift signals — user turns
//!   that grew the scope, and repeated failures — crossing the session's own
//!   baseline. Narrate and offer to refocus; never a hard stop, because the
//!   work may be useful adjacency the user did not name.
//! - **Rabbit hole** (§6.5 #4): the loop's no-progress streak one turn short of
//!   its stop bar. Narration is the model's one chance to change approach
//!   before the shipped stop owns the turn.
//!
//! Thresholds are reused from the constants the loop already ships
//! ([`NO_PROGRESS_TOKEN_THRESHOLD`], [`NO_PROGRESS_STALL_TURNS`],
//! [`NO_PROGRESS_STOP_STREAK`], [`COMPLEXITY_THRESHOLDS`], [`DriftWeights`]) —
//! §6.5's grounding table is explicit that these guards must not introduce a
//! parallel set of numbers.
//!
//! Every guard keys off observable trajectory state (token counts, tool calls,
//! file writes, the complexity ledger) and never off what the model says about
//! its own progress. That is deliberate: §9 Q5's silent-failures research found
//! model self-report unreliable, so a guard that trusted it would be blinded by
//! exactly the failure it is meant to catch.

use crate::drift::DriftWeights;
use crate::goal_loop::{NO_PROGRESS_STALL_TURNS, NO_PROGRESS_TOKEN_THRESHOLD};
use crate::task_state::{TaskState, COMPLEXITY_THRESHOLDS};
use crate::NO_PROGRESS_STOP_STREAK;

// ---------------------------------------------------------------------------
// Stall (§6.5 #1)

/// Output tokens below which a turn that ran no tools and wrote no file counts
/// as stalled. Reuses the goal guard's measured floor rather than adding a
/// second one (§6.5 grounding table: 500 ≈ 2× the measured median turn).
pub const STALL_TOKEN_FLOOR: u64 = NO_PROGRESS_TOKEN_THRESHOLD;

/// Consecutive stalled turns before the guard asks the model for a progress
/// update. Same bar as the goal guard's stall pause, so the two cannot disagree
/// about when a stretch of silence has gone on too long.
pub const STALL_NARRATE_TURNS: u32 = NO_PROGRESS_STALL_TURNS;

/// Consecutive stalled turns before the user is told as well as the model.
pub const STALL_SURFACE_TURNS: u32 = NO_PROGRESS_STALL_TURNS + 1;

/// Narration turns one run may spend. Mirrors `PROSE_TOOL_NUDGE_CAP`: a model
/// that cannot report progress gets two chances, and after that the loop stops
/// overriding its stop decision.
pub const STALL_NARRATE_CAP: u32 = 2;

/// What a completed turn says about stall state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallSignal {
    /// Not stalled, or not stalled long enough to act on.
    None,
    /// The stall has reached the narration bar: ask the model for a progress
    /// update instead of letting the turn end silently.
    Narrate(u32),
    /// The stall outlived the narration: tell the user too.
    Surface(u32),
}

/// A turn is stalled when it produced negligible visible output, ran no tools,
/// and wrote no file. Any one of the three clears the streak — a short turn
/// that edited a file is progress (§6.5's "negligible when … AND").
pub fn is_stalled_turn(output_tokens: u64, tool_calls: usize, wrote_files: bool) -> bool {
    output_tokens < STALL_TOKEN_FLOOR && tool_calls == 0 && !wrote_files
}

/// Counts consecutive stalled turns and reports when the bar is crossed.
#[derive(Debug, Default)]
pub struct StallTracker {
    streak: u32,
}

impl StallTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Current consecutive-stall count. Test-only: the loop acts on the
    /// [`StallSignal`], not on the raw count.
    #[cfg(test)]
    pub fn streak(&self) -> u32 {
        self.streak
    }

    /// Fold one completed turn in and report any signal for it.
    pub fn observe(
        &mut self,
        output_tokens: u64,
        tool_calls: usize,
        wrote_files: bool,
    ) -> StallSignal {
        if !is_stalled_turn(output_tokens, tool_calls, wrote_files) {
            self.streak = 0;
            return StallSignal::None;
        }
        self.streak = self.streak.saturating_add(1);
        match self.streak {
            STALL_NARRATE_TURNS => StallSignal::Narrate(self.streak),
            STALL_SURFACE_TURNS => StallSignal::Surface(self.streak),
            _ => StallSignal::None,
        }
    }
}

/// The continuation message a narrate signal injects. The model produces the
/// update; the harness only asks for it (spec §6.5: narration first, so a stuck
/// stretch is never silent).
pub fn stall_narration(streak: u32) -> String {
    format!(
        "Progress check: the last {streak} turns produced almost no output and made no \
         changes. Before anything else, state in two lines where the task stands and the \
         single next concrete step, then take that step. If you are blocked — on a \
         permission, a missing file, an ambiguous requirement — say what is blocking you \
         instead of waiting."
    )
}

/// The status line shown to the user once the stall outlives the narration.
pub fn stall_surface_message(streak: u32) -> String {
    format!(
        "No progress for {streak} consecutive turns (no output, no tool calls, no file \
         changes) and the model did not answer a progress check. Press Esc to interrupt, or \
         send a message to redirect."
    )
}

// ---------------------------------------------------------------------------
// Divergence (§6.5 #3)

/// Drift score from the two *strong* signals only.
///
/// The composite in [`crate::drift::drift_score`] also weighs failed tools
/// (1.0) and files touched (0.25). Those are volume signals: a large multi-file
/// task legitimately touches many files, which is why §6.5 warns that an "N
/// extra files" count alone must not trip this guard. Baselining the composite
/// minus its weak terms makes that impossible by construction rather than by
/// picking a margin big enough to hide it.
pub fn strong_drift_score(scope_expansions: usize, repeated_failures: usize) -> f64 {
    scope_expansions as f64 * DriftWeights::SCOPE_EXPANSION
        + repeated_failures as f64 * DriftWeights::REPEATED_FAILURE
}

/// How far the strong-signal score must grow past the session's baseline to
/// count as material drift. One scope expansion's weight: a single new
/// scope-expansion turn, or the accumulation of repeated failures on one
/// target, is enough; a wider blast radius alone is not.
pub const DIVERGENCE_GROWTH_BAR: f64 = DriftWeights::SCOPE_EXPANSION;

/// The strong-signal score for a state, factored out so [`DivergenceTracker`]
/// has one definition of "how far out of scope is this now".
fn strong_score_of(state: &TaskState) -> f64 {
    strong_drift_score(
        state.complexity.scope_expansions,
        state.complexity.repeated_failures_per_target,
    )
}

/// Tracks the run's strong-drift baseline and narrates when it is crossed.
///
/// The harness cannot compare the work against the original prompt
/// semantically without spending a model call, so the guard watches the
/// observable proxies §6.5 names and asks the *model* to make the semantic call.
/// Narration is rate-limited by construction: the baseline advances only when a
/// narration fires, so a steady elevated-but-unchanged state is not re-flagged.
///
/// [`Self::prime`] must be called as the run opens. Without it a tracker's first
/// [`Self::observe`] would become the baseline, and since that first observation
/// happens after the first re-anchor milestone, drift accumulated during the
/// opening turns would be absorbed into the baseline and never narrated. Live
/// consumers therefore prime at loop start; an unprimed `observe` primes and
/// reports nothing rather than narrating on entry.
#[derive(Debug, Default)]
pub struct DivergenceTracker {
    baseline: Option<f64>,
}

impl DivergenceTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the session-opening strong-drift baseline. Idempotent: only the
    /// first call sets it.
    pub fn prime(&mut self, state: &TaskState) -> f64 {
        let strong = strong_score_of(state);
        if self.baseline.is_none() {
            self.baseline = Some(strong);
        }
        strong
    }

    /// Observe the current ledger and return the refocus narration when the
    /// strong-signal score has crossed a new bar since the session opened or
    /// since the last narration.
    pub fn observe(&mut self, state: &TaskState) -> Option<String> {
        let scope_expansions = state.complexity.scope_expansions;
        let repeated_failures = state.complexity.repeated_failures_per_target;
        let strong = strong_score_of(state);
        let Some(baseline) = self.baseline else {
            self.baseline = Some(strong);
            return None;
        };

        // The ledger's own bar for scope expansions, or one strong signal's
        // worth of growth past the baseline.
        let over_ledger_bar = scope_expansions > COMPLEXITY_THRESHOLDS.scope_expansions;
        let grew = strong - baseline >= DIVERGENCE_GROWTH_BAR;
        if !over_ledger_bar && !grew {
            return None;
        }
        self.baseline = Some(strong);
        Some(divergence_narration(
            scope_expansions,
            repeated_failures,
            state.complexity.files_touched,
        ))
    }
}

/// The refocus pin injected ahead of the next request.
fn divergence_narration(
    scope_expansions: usize,
    repeated_failures: usize,
    files_touched: usize,
) -> String {
    format!(
        "[Drift check] The run has moved past the original request: {scope_expansions} user \
         turn(s) that grew the scope, {repeated_failures} repeated failure target(s), and \
         {files_touched} file(s) touched. In one line, before continuing: say what you are \
         working on now and why it still serves the original request. If it does not, stop \
         the tangential work, say what you are dropping, and return to the original task — \
         or ask the user which of the two they want."
    )
}

// ---------------------------------------------------------------------------
// Rabbit hole (§6.5 #4)

/// Whether the loop's no-progress streak is one turn short of its stop bar.
///
/// At the bar itself the shipped stop owns the turn, so this is the last point
/// where a narration can still change the outcome — which is exactly the
/// "narrate and offer to refocus" action §6.5 #4 specifies, without weakening
/// the stop that bounds the loop.
pub fn rabbit_hole_due(no_progress_streak: u32) -> bool {
    no_progress_streak.saturating_add(1) == NO_PROGRESS_STOP_STREAK
}

/// The continuation message a rabbit-hole narration injects.
pub fn rabbit_hole_narration(no_progress_streak: u32) -> String {
    format!(
        "You have spent {no_progress_streak} turns on the same tool calls without changing a \
         file or passing a check. That looks like a rabbit hole on one sub-problem. State in \
         one line what you are trying to establish, then either take a different approach or \
         ask the user how to proceed. Repeating the same call again will stop the loop."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawde_core::types::Message;

    // ---- stall ----------------------------------------------------------

    #[test]
    fn a_stalled_turn_needs_all_three_conditions() {
        assert!(is_stalled_turn(10, 0, false));
        // Substantial output is progress even with no tools and no writes.
        assert!(!is_stalled_turn(STALL_TOKEN_FLOOR, 0, false));
        assert!(!is_stalled_turn(10, 1, false));
        assert!(!is_stalled_turn(10, 0, true));
    }

    #[test]
    fn stall_narrates_after_three_and_surfaces_on_the_fourth() {
        let mut tracker = StallTracker::new();
        assert_eq!(tracker.observe(10, 0, false), StallSignal::None);
        assert_eq!(tracker.streak(), 1);
        assert_eq!(tracker.observe(10, 0, false), StallSignal::None);
        assert_eq!(
            tracker.observe(10, 0, false),
            StallSignal::Narrate(STALL_NARRATE_TURNS)
        );
        assert_eq!(
            tracker.observe(10, 0, false),
            StallSignal::Surface(STALL_SURFACE_TURNS)
        );
        // Nothing further to say: the run is bounded by the turn cap from here.
        assert_eq!(tracker.observe(10, 0, false), StallSignal::None);
    }

    #[test]
    fn progress_of_any_kind_resets_the_stall_streak() {
        let mut tracker = StallTracker::new();
        assert_eq!(tracker.observe(10, 0, false), StallSignal::None);
        assert_eq!(tracker.observe(10, 0, false), StallSignal::None);
        // A turn that wrote a file is progress.
        assert_eq!(tracker.observe(10, 0, true), StallSignal::None);
        assert_eq!(tracker.streak(), 0);
        // So is a substantial answer.
        assert_eq!(
            tracker.observe(STALL_TOKEN_FLOOR + 1, 0, false),
            StallSignal::None
        );
        assert_eq!(tracker.streak(), 0);
        // And so is a turn that ran a tool.
        assert_eq!(tracker.observe(10, 1, false), StallSignal::None);
        assert_eq!(tracker.streak(), 0);
    }

    #[test]
    fn stall_messages_tell_the_model_what_to_do() {
        let ask = stall_narration(STALL_NARRATE_TURNS);
        assert!(ask.contains("Progress check"));
        assert!(ask.contains("next concrete step"));
        let surfaced = stall_surface_message(STALL_SURFACE_TURNS);
        assert!(surfaced.contains("No progress for 4 consecutive turns"));
    }

    // ---- divergence -----------------------------------------------------

    fn state_with(scope_expansions: usize, repeated_failures: usize, files: usize) -> TaskState {
        let mut state = TaskState::from_messages(&[Message::user("Refactor the parser")]);
        state.complexity.scope_expansions = scope_expansions;
        state.complexity.repeated_failures_per_target = repeated_failures;
        state.complexity.files_touched = files;
        state
    }

    #[test]
    fn strong_drift_score_ignores_the_volume_terms() {
        assert!((strong_drift_score(0, 0) - 0.0).abs() < f64::EPSILON);
        assert!((strong_drift_score(1, 0) - DriftWeights::SCOPE_EXPANSION).abs() < f64::EPSILON);
        assert!(
            (strong_drift_score(0, 2) - 2.0 * DriftWeights::REPEATED_FAILURE).abs() < f64::EPSILON
        );
        // Files touched and failed tools are not inputs at all.
        let clean = state_with(0, 0, 40);
        let mut tracker = DivergenceTracker::new();
        assert!(tracker.observe(&clean).is_none());
    }

    #[test]
    fn a_file_count_alone_never_diverges() {
        let mut tracker = DivergenceTracker::new();
        // The baseline is the opening score; touching many files does not move
        // it, because files are not part of the strong-signal score.
        assert!(tracker.observe(&state_with(0, 0, 1)).is_none());
        assert!(tracker.observe(&state_with(0, 0, 50)).is_none());
    }

    #[test]
    fn divergence_fires_when_scope_expansions_exceed_the_ledger_bar() {
        let mut tracker = DivergenceTracker::new();
        assert!(tracker.observe(&state_with(0, 0, 1)).is_none());
        let narration = tracker
            .observe(&state_with(
                COMPLEXITY_THRESHOLDS.scope_expansions + 1,
                0,
                1,
            ))
            .expect("scope past the ledger bar must narrate");
        assert!(narration.contains("[Drift check]"));
        assert!(narration.contains("ask the user which of the two they want"));
    }

    #[test]
    fn divergence_narrates_once_per_crossing() {
        let mut tracker = DivergenceTracker::new();
        tracker.prime(&state_with(0, 0, 1));
        assert!(
            tracker.observe(&state_with(1, 0, 1)).is_some(),
            "one scope expansion past the baseline is a strong signal"
        );
        // The same state does not narrate again — the baseline advanced.
        assert!(tracker.observe(&state_with(1, 0, 1)).is_none());
        // A further cross of the bar does.
        assert!(tracker.observe(&state_with(2, 0, 1)).is_some());
    }

    #[test]
    fn the_baseline_is_the_run_opening_state_not_the_first_milestone() {
        // A run that opens already carrying one scope expansion: priming at loop
        // start keeps that turn in the baseline, so the guard does not narrate
        // twice for the same growth.
        let mut tracker = DivergenceTracker::new();
        tracker.prime(&state_with(1, 0, 1));
        assert!(tracker.observe(&state_with(1, 0, 1)).is_none());
        // Without priming, the first observation *becomes* the baseline — the
        // opening drift is absorbed and never narrated.
        let mut unprimed = DivergenceTracker::new();
        assert!(unprimed.observe(&state_with(1, 0, 1)).is_none());
        assert!(unprimed.observe(&state_with(1, 0, 1)).is_none());
    }

    #[test]
    fn a_scope_expansion_count_past_the_ledger_bar_fires_even_without_growth() {
        // The ledger bar is absolute: a run whose opening state is already past
        // it narrates at the first milestone rather than waiting for growth.
        let mut tracker = DivergenceTracker::new();
        let expanded = state_with(COMPLEXITY_THRESHOLDS.scope_expansions + 1, 0, 1);
        tracker.prime(&expanded);
        assert!(tracker.observe(&expanded).is_some());
    }

    #[test]
    fn repeated_failures_grow_the_strong_score() {
        let mut tracker = DivergenceTracker::new();
        assert!(tracker.observe(&state_with(0, 0, 1)).is_none());
        assert!(tracker.observe(&state_with(0, 1, 1)).is_none());
        assert!(
            tracker.observe(&state_with(0, 2, 1)).is_some(),
            "repeated failures are a strong signal"
        );
    }

    // ---- rabbit hole ----------------------------------------------------

    #[test]
    fn rabbit_hole_narration_fires_the_turn_before_the_stop() {
        assert!(!rabbit_hole_due(NO_PROGRESS_STOP_STREAK));
        assert!(rabbit_hole_due(NO_PROGRESS_STOP_STREAK - 1));
        assert!(!rabbit_hole_due(1));
        let text = rabbit_hole_narration(NO_PROGRESS_STOP_STREAK - 1);
        assert!(text.contains("rabbit hole"));
        assert!(text.contains("different approach"));
        assert!(text.contains("will stop the loop"));
    }
}
