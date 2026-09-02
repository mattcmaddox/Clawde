//! Runtime-owned task state for the agent loop.
//!
//! The transcript remains the historical record. `TaskState` is the compact,
//! deterministic projection used to keep the active task, evidence, and next
//! action visible to the model on every turn.

use clawde_core::types::{ContentBlock, Message, MessageContent, Role};
use std::collections::HashSet;
use std::path::PathBuf;

const MAX_TEXT_CHARS: usize = 700;
const MAX_ITEMS: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusState {
    Active,
    Blocked,
    AwaitingClarification,
    Complete,
    Suspended,
}

impl FocusState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Blocked => "blocked",
            Self::AwaitingClarification => "awaiting clarification",
            Self::Complete => "complete",
            Self::Suspended => "suspended",
        }
    }
}

/// How a new user turn relates to the active objective. Classification is
/// conservative and deterministic; only unambiguous heuristics mutate state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserTurnRelation {
    /// Refines or continues the active objective; keep the objective.
    ClarifiesCurrentTask,
    /// Adds a constraint or requirement; objective unchanged, constraint recorded.
    Constraint,
    /// Corrects the current approach ("actually", "instead"); objective replaced.
    CorrectsCurrentTask,
    /// Grows the scope ("also", "and then"); objective kept, expansion noted.
    ExpandsCurrentTask,
    /// A related but distinct piece of work on the same subject area.
    StartsRelatedSubtask,
    /// A new objective; replaces the old one (the old one is not auto-suspended:
    /// Clawde runs one objective at a time and the transcript keeps history).
    StartsNewTask,
    /// A standalone question; must not mutate the objective or constraints.
    UnrelatedQuestion,
}

impl UserTurnRelation {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ClarifiesCurrentTask => "clarification",
            Self::Constraint => "constraint",
            Self::CorrectsCurrentTask => "correction",
            Self::ExpandsCurrentTask => "scope expansion",
            Self::StartsRelatedSubtask => "related subtask",
            Self::StartsNewTask => "new task",
            Self::UnrelatedQuestion => "unrelated question",
        }
    }
}

/// Marker phrases that signal a correction of the current approach.
const CORRECTION_MARKERS: [&str; 6] = [
    "actually,",
    "actually ",
    "instead ",
    "instead of",
    "change that",
    "scratch that",
];

/// Marker words that signal scope expansion of the current task.
const EXPANSION_MARKERS: [&str; 6] = [
    "also ",
    "and then ",
    "in addition ",
    "additionally ",
    "as well",
    "plus ",
];

/// Imperative verb prefixes that mark a directive ("use the existing X")
/// rather than a restriction. Directives win over constraint markers because
/// a sentence like "Use the existing tokenizer and keep the API stable" is a
/// new instruction that happens to contain "keep".
const IMPERATIVE_PREFIXES: [&str; 14] = [
    "use ",
    "implement ",
    "write ",
    "add ",
    "remove ",
    "fix ",
    "update ",
    "refactor ",
    "create ",
    "build ",
    "make ",
    "delete ",
    "replace ",
    "change ",
];

/// Marker phrases that signal a constraint rather than a new objective.
const CONSTRAINT_MARKERS: [&str; 8] = [
    "must ", "must not", "don't ", "do not ", "never ", "only ", "without ", "keep ",
];

/// Classify a user turn against the current objective.
///
/// Conservative ordering: corrections win over expansions ("actually, also..."
/// is a correction), constraints beat new-task (a constraint sentence without
/// shared terms still governs the active work), and a short question-shaped
/// turn with no overlap is an unrelated question.
fn classify_user_turn(turn_text: &str, objective: Option<&str>) -> UserTurnRelation {
    let text = turn_text.trim();
    let lower = text.to_ascii_lowercase();
    let is_question = text.ends_with('?')
        || [
            "what ",
            "why ",
            "how ",
            "when ",
            "where ",
            "who ",
            "can you explain",
        ]
        .iter()
        .any(|marker| lower.starts_with(marker));

    if CORRECTION_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return UserTurnRelation::CorrectsCurrentTask;
    }
    if IMPERATIVE_PREFIXES
        .iter()
        .any(|marker| lower.starts_with(marker))
    {
        return UserTurnRelation::StartsNewTask;
    }
    if CONSTRAINT_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return UserTurnRelation::Constraint;
    }
    if EXPANSION_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return UserTurnRelation::ExpandsCurrentTask;
    }

    let shared_terms = objective.map_or(0, |objective| {
        let objective_terms = terms(objective);
        terms(text)
            .iter()
            .filter(|term| objective_terms.contains(*term))
            .count()
    });

    if is_question {
        if shared_terms > 0 {
            UserTurnRelation::ClarifiesCurrentTask
        } else {
            UserTurnRelation::UnrelatedQuestion
        }
    } else if shared_terms > 0 {
        UserTurnRelation::ClarifiesCurrentTask
    } else {
        UserTurnRelation::StartsNewTask
    }
}

/// Split text into lowercase identifier-like terms (len >= 3). Shared with
/// classification so term overlap is computed consistently.
fn terms(value: &str) -> HashSet<String> {
    value
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .map(str::to_ascii_lowercase)
        .filter(|term| term.len() >= 3)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDecision {
    pub statement: String,
    pub evidence: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskFailure {
    pub source: String,
    pub summary: String,
}

/// Where a piece of evidence came from. Provenance is the core of the
/// claim/observation distinction: only runtime sources can produce
/// `Verified` items, and `ModelProposal` never can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceSource {
    User,
    Tool,
    Snapshot,
    Validation,
    Plan,
    ModelProposal,
}

impl EvidenceSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Tool => "tool",
            Self::Snapshot => "snapshot",
            Self::Validation => "validation",
            Self::Plan => "plan",
            Self::ModelProposal => "model-proposal",
        }
    }
}

/// Lifecycle of an evidence item. `Verified` is reachable only through
/// [`TaskState::record_validation`] with [`ValidationVerdict::Passed`] —
/// i.e. only from a deterministic runtime check. Model text and tool output
/// are recorded as observed facts, never as verified work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceStatus {
    Observed,
    Verified,
    Failed,
    Superseded,
}

impl EvidenceStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Observed => "observed",
            Self::Verified => "verified",
            Self::Failed => "failed",
            Self::Superseded => "superseded",
        }
    }
}

/// A single bounded evidence entry with provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceItem {
    pub summary: String,
    pub source: EvidenceSource,
    pub status: EvidenceStatus,
}

/// Verdict for a validation round. Callers with a real `VerifyReport` pass
/// the report's verdict; callers with only a headline string pass `Unknown`,
/// which records the text as observed without claiming a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationVerdict {
    Passed,
    Failed,
    Unknown,
}

/// Thresholds for the complexity warning. Only exceeded thresholds render —
/// the signal exists to catch scope drift, not to nag on normal work.
pub const COMPLEXITY_THRESHOLDS: ComplexityLedger = ComplexityLedger {
    files_touched: 8,
    tool_calls: 60,
    failed_tools: 4,
    repeated_failures_per_target: 3,
    scope_expansions: 2,
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComplexityLedger {
    pub files_touched: usize,
    pub tool_calls: usize,
    pub failed_tools: usize,
    /// Distinct failure summaries seen 2+ times — signals retry loops on the
    /// same error rather than progress through different failures.
    pub repeated_failures_per_target: usize,
    /// User turns classified as scope expansions for the current objective.
    pub scope_expansions: usize,
}

impl ComplexityLedger {
    /// Counters whose value exceeds the configured threshold, in render order.
    /// Single-file requests typically stay well under all of these; a hit
    /// means the run is drifting (touching many files, retrying one error, or
    /// accumulating scope mid-task).
    pub fn exceeded(&self) -> Vec<(&'static str, usize, usize)> {
        let mut items = Vec::new();
        let checks = [
            (
                "files touched",
                self.files_touched,
                COMPLEXITY_THRESHOLDS.files_touched,
            ),
            (
                "tool calls",
                self.tool_calls,
                COMPLEXITY_THRESHOLDS.tool_calls,
            ),
            (
                "failed tools",
                self.failed_tools,
                COMPLEXITY_THRESHOLDS.failed_tools,
            ),
            (
                "repeated failures",
                self.repeated_failures_per_target,
                COMPLEXITY_THRESHOLDS.repeated_failures_per_target,
            ),
            (
                "scope expansions",
                self.scope_expansions,
                COMPLEXITY_THRESHOLDS.scope_expansions,
            ),
        ];
        for (label, value, threshold) in checks {
            if value > threshold {
                items.push((label, value, threshold));
            }
        }
        items
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeEvidence {
    pub plan_step: Option<String>,
    pub validation: Option<String>,
    pub snapshot_files: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskState {
    pub objective: Option<String>,
    pub focus: FocusState,
    pub active_step: Option<String>,
    pub constraints: Vec<String>,
    pub decisions: Vec<TaskDecision>,
    /// Bounded provenance log. Newest last, deduplicated, capped at MAX_ITEMS.
    pub evidence: Vec<EvidenceItem>,
    pub changed_files: Vec<PathBuf>,
    pub failures: Vec<TaskFailure>,
    /// One-shot simplification-review marker (mirrors the loop-local flag so
    /// it survives resume via replay).
    pub simplification_reviewed: bool,
    pub next_action: Option<String>,
    pub complexity: ComplexityLedger,
    pub runtime: RuntimeEvidence,
    pub turn: u32,
    /// The newest user-text turn's redirect statement, when that turn was a
    /// redirect (correction, related subtask, or a new objective replacing an
    /// existing one). Derived during `apply_message` — never persisted — and
    /// consumed by [`Self::catch_up_decisions`] so the loop can emit a
    /// `DecisionRecorded` event for it. `None` when the newest turn is a
    /// continuation/constraint/expansion/question or there is no user text.
    pub newest_redirect: Option<String>,
}

impl Default for TaskState {
    fn default() -> Self {
        Self {
            objective: None,
            focus: FocusState::Active,
            active_step: None,
            constraints: Vec::new(),
            decisions: Vec::new(),
            evidence: Vec::new(),
            changed_files: Vec::new(),
            simplification_reviewed: false,
            failures: Vec::new(),
            next_action: None,
            complexity: ComplexityLedger::default(),
            runtime: RuntimeEvidence {
                plan_step: None,
                validation: None,
                snapshot_files: Vec::new(),
            },
            turn: 0,
            newest_redirect: None,
        }
    }
}

impl TaskState {
    pub fn from_messages(messages: &[Message]) -> Self {
        let mut state = Self::default();
        state.apply_messages(messages);
        state
    }

    pub fn apply_messages(&mut self, messages: &[Message]) {
        for message in messages {
            self.apply_message(message);
        }
        self.trim();
    }

    pub fn apply_message(&mut self, message: &Message) {
        match message.role {
            Role::User => self.apply_user_message(message),
            Role::Assistant => self.apply_assistant_message(message),
        }
        self.turn = self.turn.saturating_add(1);
    }

    fn apply_user_message(&mut self, message: &Message) {
        if is_tool_result(message) {
            self.apply_tool_results(message);
            return;
        }
        let text = message.get_all_text();
        let text = text.trim();
        if text.is_empty() || text.contains("<compact-summary>") {
            return;
        }

        // The first substantive turn always establishes the objective — a
        // constraint on nothing is still a task definition. That definition is
        // transcript-derived, not a redirect decision; only later turns that
        // REPLACE or pivot the objective are decisions worth persisting.
        let relation = if self.objective.is_none() {
            UserTurnRelation::StartsNewTask
        } else {
            classify_user_turn(text, self.objective.as_deref())
        };
        // Redirect tracking (see `newest_redirect`): a correction or related
        // subtask always pivots; a new objective only replaces something.
        self.newest_redirect = match relation {
            UserTurnRelation::CorrectsCurrentTask | UserTurnRelation::StartsRelatedSubtask => {
                Some(truncate(text))
            }
            UserTurnRelation::StartsNewTask if self.objective.is_some() => Some(truncate(text)),
            _ => None,
        };
        self.apply_user_relation(relation, text);
    }

    /// Apply a classified user turn. Unrelated questions never touch the
    /// objective, constraints, or focus; everything else is recorded
    /// according to its relation.
    fn apply_user_relation(&mut self, relation: UserTurnRelation, text: &str) {
        match relation {
            UserTurnRelation::UnrelatedQuestion => {
                self.focus = FocusState::Active;
                self.next_action = Some(
                    "Answer the user's question without changing the current task.".to_string(),
                );
            }
            UserTurnRelation::ClarifiesCurrentTask => {
                self.focus = FocusState::Active;
                self.next_action = Some(
                    "Continue from the latest user instruction using existing evidence."
                        .to_string(),
                );
                self.extract_constraints(text);
            }
            UserTurnRelation::Constraint => {
                self.focus = FocusState::Active;
                self.extract_constraints(text);
                self.next_action = Some(
                    "Continue the current task while respecting the stated constraint.".to_string(),
                );
            }
            UserTurnRelation::ExpandsCurrentTask => {
                self.focus = FocusState::Active;
                self.extract_constraints(text);
                self.complexity.scope_expansions =
                    self.complexity.scope_expansions.saturating_add(1);
                self.next_action = Some(
                    "Incorporate the expanded scope into the existing plan without discarding completed work.".to_string(),
                );
            }
            UserTurnRelation::CorrectsCurrentTask
            | UserTurnRelation::StartsRelatedSubtask
            | UserTurnRelation::StartsNewTask => {
                // A correction or new instruction replaces the objective.
                self.objective = Some(truncate(text));
                self.focus = FocusState::Active;
                self.next_action = Some(
                    "Continue from the latest user instruction using existing evidence."
                        .to_string(),
                );
                self.extract_constraints(text);
            }
        }
    }

    fn apply_assistant_message(&mut self, message: &Message) {
        let mut saw_tool = false;
        if let MessageContent::Blocks(blocks) = &message.content {
            for block in blocks {
                if let ContentBlock::ToolUse { name, input, .. } = block {
                    saw_tool = true;
                    self.complexity.tool_calls = self.complexity.tool_calls.saturating_add(1);
                    if let Some(path) = input
                        .get("file_path")
                        .or_else(|| input.get("path"))
                        .and_then(|value| value.as_str())
                    {
                        let path = PathBuf::from(path);
                        if !self.changed_files.contains(&path) && is_mutating_tool(name) {
                            self.changed_files.push(path);
                            self.complexity.files_touched =
                                self.complexity.files_touched.saturating_add(1);
                        }
                    }
                    self.next_action = Some(format!(
                        "Process the `{name}` result before taking another action."
                    ));
                }
            }
        }
        if !saw_tool && !message.get_all_text().trim().is_empty() {
            // Completion claims in assistant text are proposals, not proof.
            // Record them with ModelProposal provenance so rendering can show
            // them as unverified while `verified_evidence()` stays empty.
            let text = message.get_all_text();
            let lower = text.to_ascii_lowercase();
            if ["implemented", "completed", "fixed", "all tests pass"]
                .iter()
                .any(|marker| lower.contains(marker))
            {
                self.record_proposal(text.trim());
            }
            self.next_action = Some(
                "Check whether the requested work is complete and report evidence.".to_string(),
            );
        }
    }

    fn apply_tool_results(&mut self, message: &Message) {
        if let MessageContent::Blocks(blocks) = &message.content {
            for block in blocks {
                if let ContentBlock::ToolResult {
                    content, is_error, ..
                } = block
                {
                    if is_error.unwrap_or(false) {
                        self.complexity.failed_tools =
                            self.complexity.failed_tools.saturating_add(1);
                        let summary = truncate(&tool_result_text(content));
                        // Retry-loop detection: total occurrences of this exact
                        // failure summary including the current one.
                        let prior = self
                            .failures
                            .iter()
                            .filter(|failure| failure.summary == summary)
                            .count();
                        self.complexity.repeated_failures_per_target = self
                            .complexity
                            .repeated_failures_per_target
                            .max(prior.saturating_add(1));
                        self.failures.push(TaskFailure {
                            source: "tool".to_string(),
                            summary: summary.clone(),
                        });
                        self.push_evidence(EvidenceItem {
                            summary,
                            source: EvidenceSource::Tool,
                            status: EvidenceStatus::Failed,
                        });
                        self.focus = FocusState::Blocked;
                        self.next_action = Some(
                            "Diagnose the latest tool failure and retry with a changed approach."
                                .to_string(),
                        );
                    } else {
                        self.focus = FocusState::Active;
                        self.next_action = Some(
                            "Use the successful tool evidence to advance the current task."
                                .to_string(),
                        );
                    }
                }
            }
        }
    }

    fn extract_constraints(&mut self, text: &str) {
        let lower = text.to_ascii_lowercase();
        if CONSTRAINT_MARKERS
            .iter()
            .any(|marker| lower.contains(marker))
        {
            let constraint = truncate(text);
            if !self.constraints.contains(&constraint) {
                self.constraints.push(constraint);
            }
        }
    }

    fn trim(&mut self) {
        self.constraints.truncate(MAX_ITEMS);
        self.decisions.truncate(MAX_ITEMS);
        self.changed_files.truncate(MAX_ITEMS);
        self.failures.truncate(MAX_ITEMS);
    }

    /// Apply one persisted runtime event (event-persistence Phase 3). The
    /// claim/proof boundary is preserved: only `ValidationRecorded` with a
    /// `Passed` verdict produces `Verified` evidence, exactly as the live
    /// loop's `record_validation` does.
    pub fn apply_event(&mut self, event: &clawde_core::session_storage::StateEvent) {
        use clawde_core::session_storage::StateEvent;
        match event {
            StateEvent::ToolObserved {
                failed,
                summary,
                file_paths,
                mutating,
            } => {
                self.complexity.tool_calls = self.complexity.tool_calls.saturating_add(1);
                if *mutating {
                    for path in file_paths {
                        let path = PathBuf::from(path);
                        if !self.changed_files.contains(&path) {
                            self.changed_files.push(path);
                            self.complexity.files_touched =
                                self.complexity.files_touched.saturating_add(1);
                        }
                    }
                }
                if *failed {
                    self.complexity.failed_tools = self.complexity.failed_tools.saturating_add(1);
                    let summary = truncate(summary);
                    let prior = self
                        .failures
                        .iter()
                        .filter(|failure| failure.summary == summary)
                        .count();
                    self.complexity.repeated_failures_per_target = self
                        .complexity
                        .repeated_failures_per_target
                        .max(prior.saturating_add(1));
                    self.failures.push(TaskFailure {
                        source: "tool".to_string(),
                        summary: summary.clone(),
                    });
                    self.push_evidence(EvidenceItem {
                        summary,
                        source: EvidenceSource::Tool,
                        status: EvidenceStatus::Failed,
                    });
                    self.focus = FocusState::Blocked;
                }
            }
            StateEvent::ValidationRecorded { verdict, headline } => {
                let mapped = match verdict {
                    clawde_core::session_storage::StateValidationVerdict::Passed => {
                        ValidationVerdict::Passed
                    }
                    clawde_core::session_storage::StateValidationVerdict::Failed => {
                        ValidationVerdict::Failed
                    }
                    clawde_core::session_storage::StateValidationVerdict::Unknown => {
                        ValidationVerdict::Unknown
                    }
                };
                self.record_validation(mapped, headline.clone());
            }
            StateEvent::SnapshotObserved { files } => {
                let paths = files.iter().map(PathBuf::from).collect();
                self.record_snapshot(paths);
            }
            StateEvent::DecisionRecorded {
                statement,
                evidence,
            } => {
                self.record_decision(statement.clone(), evidence.clone());
                // Objective restoration: the only producer of decisions is
                // the loop's redirect emitter, whose statement is exactly the
                // objective pivot text (correction / subtask / replace — the
                // same string `apply_user_relation` stores). Replaying events
                // in order makes the newest redirect win, so a compaction
                // that removed the redirect MESSAGE still replays the
                // corrected objective instead of drifting back to the
                // pre-correction one. Live runs are unaffected: the
                // transcript pass already derived this objective.
                self.objective = Some(statement.clone());
            }
            StateEvent::FocusChanged { focus, .. } => match focus.as_str() {
                "blocked" => self.focus = FocusState::Blocked,
                "awaiting clarification" => self.focus = FocusState::AwaitingClarification,
                "complete" => self.focus = FocusState::Complete,
                "suspended" => self.focus = FocusState::Suspended,
                _ => self.focus = FocusState::Active,
            },
            StateEvent::PlanStepSet { step } => {
                self.runtime.plan_step = Some(step.clone());
            }
            StateEvent::SimplificationReviewed => {
                self.simplification_reviewed = true;
            }
        }
    }

    /// Build a state by first reducing the transcript (objectives, focus
    /// classification, constraints) and then replaying persisted runtime
    /// events on top (evidence, verdicts, counters, plan step). This is the
    /// resume-path counterpart of the live loop's accumulation.
    ///
    /// Counters are ZEROED after the transcript pass and rebuilt exclusively
    /// from events: the live loop counts each tool call once (via the assistant
    /// ToolUse) and emits one event per result, so replaying both would
    /// double-count. Events are the authoritative counter source because they
    /// carry the file/mutating metadata the transcript lacks.
    ///
    /// Tool failures are cleared with the counters for the same reason: every
    /// post-emission tool result has a `ToolObserved` event, so re-adding the
    /// transcript pass's failures on top of the event pass would duplicate
    /// them (sessions that predate emission carry no events and never reach
    /// `replay` — they take the `from_messages` path).
    ///
    /// Focus/next_action are transcript-authoritative when the newest message
    /// is a user TEXT turn: that prompt is chronologically newer than every
    /// persisted event (events are only written while a turn executes), so a
    /// stale `ToolObserved{failed}` event must not resurrect a Blocked focus
    /// over the user's latest instruction. When the transcript ends on a tool
    /// result or assistant turn, events may legitimately be the newest fact
    /// and their focus effect is kept.
    pub fn replay(
        messages: &[Message],
        events: &[clawde_core::session_storage::StateEvent],
    ) -> Self {
        let mut state = Self::from_messages(messages);
        // Scope expansions are transcript-derived (user turns), never events,
        // so they must survive the counter reset below.
        let scope_expansions = state.complexity.scope_expansions;
        let newest_is_user_text = matches!(messages.last(), Some(m) if is_user_text_turn(m));
        // Capture what the transcript pass concluded for the newest user turn.
        let transcript_focus = state.focus.clone();
        let transcript_next = state.next_action.clone();
        state.complexity = ComplexityLedger::default();
        state.complexity.scope_expansions = scope_expansions;
        state.failures.clear();
        // Same authority as the counters: the transcript pass populated
        // changed_files from ToolUse blocks, but `files_touched` was zeroed
        // with the ledger — leaving the list populated would make the event
        // pass skip every path it already contains (contains-check) and
        // files_touched would stay 0. Events carry the same paths (one event
        // per result, mutating + file_paths derived from the originating
        // call), so rebuild both list and counter from events only.
        state.changed_files.clear();
        for event in events {
            state.apply_event(event);
        }
        if newest_is_user_text {
            state.focus = transcript_focus;
            state.next_action = transcript_next;
        }
        state
    }

    /// Replay from a persisted state snapshot plus the events after it
    /// (snapshot-based incremental replay). Equivalent to [`Self::replay`]
    /// over the FULL event list when the snapshot is valid: the transcript
    /// pass runs over all messages exactly as `replay` does, the snapshot
    /// body supplies the fold of events `0..event_count`, and only
    /// `tail_events` (the events written after the snapshot) are applied
    /// individually.
    ///
    /// Focus/next_action mirror `replay`'s semantics without re-applying the
    /// folded events: replay blocks focus on any failure event and nothing in
    /// `apply_event` unblocks it, so the replayed focus is Blocked exactly
    /// when a failure exists and the newest message is not user text (the
    /// transcript-derived focus then wins — see [`Self::replay`]). A failed
    /// validation additionally restores the diagnostic next-action.
    pub fn replay_with_snapshot(
        messages: &[Message],
        snapshot: &clawde_core::session_storage::StateSnapshot,
        tail_events: &[clawde_core::session_storage::StateEvent],
    ) -> Self {
        use clawde_core::session_storage::StateSnapshot;
        let StateSnapshot {
            schema_version: _,
            event_count: _,
            body,
        } = snapshot;
        let mut state = Self::from_messages(messages);
        let newest_is_user_text = matches!(messages.last(), Some(m) if is_user_text_turn(m));
        let transcript_focus = state.focus.clone();
        let transcript_next = state.next_action.clone();
        // Same event-authoritative reset as `replay`: counters (except the
        // transcript-derived scope-expansion count), failures, and changed
        // files are rebuilt from the snapshot + tail events only.
        let scope_expansions = state.complexity.scope_expansions;
        state.complexity = ComplexityLedger::default();
        state.complexity.scope_expansions = scope_expansions;
        state.failures.clear();
        state.changed_files.clear();
        state.apply_snapshot_body(body);
        if !newest_is_user_text {
            // The folded events (≤ watermark) all applied before any tail
            // event. Replay's focus is write-mostly: any failure blocks and
            // nothing unblocks until a user text turn (handled above) — so
            // the body's failure set IS the focus signal.
            if !state.failures.is_empty() {
                state.focus = FocusState::Blocked;
            }
            if state.failures.iter().any(|f| f.source == "validation") {
                state.next_action = Some(
                    "Diagnose the failed validation and change the implementation approach."
                        .to_string(),
                );
            }
        }
        for event in tail_events {
            state.apply_event(event);
        }
        if newest_is_user_text {
            state.focus = transcript_focus;
            state.next_action = transcript_next;
        }
        state
    }

    /// Overlay the event-derived fields of a snapshot body onto a state that
    /// already ran the transcript pass (`from_messages`). Evidence is UNIONED
    /// (the snapshot holds message-pass items plus event items; pushing over
    /// the fresh transcript items deduplicates by equality), everything else
    /// event-derived is replaced. Transcript-derived fields — objective,
    /// constraints, scope-expansion count, turn — are never touched.
    fn apply_snapshot_body(&mut self, body: &clawde_core::session_storage::StateSnapshotBody) {
        for decision in &body.decisions {
            self.decisions.push(TaskDecision {
                statement: truncate(&decision.statement),
                evidence: decision.evidence.clone(),
            });
        }
        self.decisions.truncate(MAX_ITEMS);
        // Objective restoration, mirroring `apply_event`'s DecisionRecorded
        // arm: the redirect emitter's statement IS the objective pivot text,
        // so the newest folded decision restores the corrected objective when
        // compaction removed the redirect message. Without this, the event
        // and snapshot replay paths would diverge on the same log.
        if let Some(latest) = body.decisions.last() {
            self.objective = Some(truncate(&latest.statement));
        }
        self.decisions.truncate(MAX_ITEMS);
        for item in &body.evidence {
            let source = evidence_source_from_str(&item.source);
            let mut status = evidence_status_from_str(&item.status);
            // Provenance guard: only a `validation` source can legitimately
            // carry `Verified` status (enforced live by `record_validation`).
            // A snapshot item whose source no longer maps to a known enum
            // degraded to `ModelProposal` above — it must not smuggle a
            // Verified claim through a stale/foreign snapshot, so downgrade.
            if source == EvidenceSource::ModelProposal && status == EvidenceStatus::Verified {
                status = EvidenceStatus::Observed;
            }
            self.push_evidence(EvidenceItem {
                summary: item.summary.clone(),
                source,
                status,
            });
        }
        self.failures = body
            .failures
            .iter()
            .map(|failure| TaskFailure {
                source: failure.source.clone(),
                summary: truncate(&failure.summary),
            })
            .collect();
        self.failures.truncate(MAX_ITEMS);
        self.changed_files = body.changed_files.iter().map(PathBuf::from).collect();
        self.changed_files.truncate(MAX_ITEMS);
        self.simplification_reviewed = body.simplification_reviewed;
        self.complexity.files_touched = body.files_touched as usize;
        self.complexity.tool_calls = body.tool_calls as usize;
        self.complexity.failed_tools = body.failed_tools as usize;
        self.complexity.repeated_failures_per_target = body.repeated_failures_per_target as usize;
        self.runtime.plan_step = body.plan_step.clone();
        self.runtime.validation = body.validation.clone();
        self.runtime.snapshot_files = body.snapshot_files.iter().map(PathBuf::from).collect();
        self.runtime.snapshot_files.truncate(MAX_ITEMS);
    }

    /// Refresh the projection from the transcript WITHOUT double-counting:
    /// transcript-derived facts are rebuilt into a clean base state, then all
    /// runtime evidence (plan/validation/snapshot and typed evidence log) is
    /// re-applied on top. The loop previously called `apply_messages` on an
    /// already-accumulated state every turn, inflating counters and
    /// duplicating failures.
    /// Consume the newest redirect (if any) and record it as a decision,
    /// returning the statement for the caller to persist as a
    /// `DecisionRecorded` event. The session-owning loop calls this every
    /// turn and emits the returned statement.
    ///
    /// Why events and not transcript: user statements are authoritative task
    /// facts, but compaction replaces the message list — a correction stated
    /// twenty turns ago would vanish on resume and the replayed state would
    /// drift back to the pre-correction objective. `DecisionRecorded` survives
    /// compaction like every other event.
    ///
    /// Exactly-once: `refresh_from_messages` re-derives `newest_redirect` from
    /// the same transcript every turn, so a redirect already recorded (in
    /// `decisions`, which replay seeds from the event fold) is never emitted
    /// twice. An older persisted redirect does not suppress a NEW one — the
    /// texts differ. Consumed via `take()`: one emission per call.
    pub fn catch_up_decisions(&mut self) -> Option<String> {
        let statement = self.newest_redirect.take()?;
        if self.decisions.iter().any(|d| d.statement == statement) {
            return None;
        }
        self.record_decision(statement.clone(), None);
        Some(statement)
    }

    pub fn refresh_from_messages(&mut self, messages: &[Message]) {
        let objective = self.objective.take();
        let preserved = std::mem::take(&mut self.evidence);
        let runtime_plan = self.runtime.plan_step.clone();
        let runtime_validation = self.runtime.validation.clone();
        let runtime_files = self.runtime.snapshot_files.clone();
        let decisions = std::mem::take(&mut self.decisions);
        let constraints = std::mem::take(&mut self.constraints);
        let changed_files = self.changed_files.clone();
        let complexity = self.complexity.clone();
        let failures = std::mem::take(&mut self.failures);
        let focus = self.focus.clone();
        let next_action = self.next_action.clone();
        let turn = self.turn;
        let simplification_reviewed = self.simplification_reviewed;

        *self = Self::from_messages(messages);

        self.objective = objective.or(self.objective.take());
        self.evidence = preserved;
        self.simplification_reviewed = simplification_reviewed;
        self.runtime.plan_step = runtime_plan;
        self.runtime.validation = runtime_validation;
        self.runtime.snapshot_files = runtime_files;
        self.decisions = decisions;
        self.constraints = constraints;
        self.changed_files = changed_files;
        self.complexity = complexity;
        self.failures = failures;
        self.focus = focus;
        self.next_action = next_action;
        self.turn = turn;
        self.trim();
    }

    pub fn set_runtime_evidence(
        &mut self,
        plan_step: Option<String>,
        validation: Option<String>,
        snapshot_files: Vec<PathBuf>,
    ) {
        self.runtime.plan_step = plan_step;
        self.record_snapshot(snapshot_files);
        // String-only callers carry no verdict: a headline is observed fact,
        // and only an explicit "failed" marker is actionable. Neither can
        // produce `Verified` evidence.
        if let Some(text) = validation {
            let verdict = if text.contains("failed") {
                ValidationVerdict::Failed
            } else {
                ValidationVerdict::Unknown
            };
            self.record_validation(verdict, text);
        }
        self.trim();
    }

    fn push_evidence(&mut self, item: EvidenceItem) {
        if !self.evidence.contains(&item) {
            self.evidence.push(item);
        }
        if self.evidence.len() > MAX_ITEMS {
            let excess = self.evidence.len() - MAX_ITEMS;
            self.evidence.drain(0..excess);
        }
    }

    /// Record a validation outcome. Only [`ValidationVerdict::Passed`] — a
    /// deterministic runtime check — produces `Verified` evidence. Failed
    /// verdicts block focus; unknown verdicts stay observed and never touch
    /// focus.
    pub fn record_validation(&mut self, verdict: ValidationVerdict, summary: impl Into<String>) {
        let raw = summary.into();
        let summary = truncate(&raw);
        let status = match verdict {
            ValidationVerdict::Passed => EvidenceStatus::Verified,
            ValidationVerdict::Failed => EvidenceStatus::Failed,
            ValidationVerdict::Unknown => EvidenceStatus::Observed,
        };
        self.runtime.validation = Some(summary.clone());
        // Supersede an identical string-only (Observed) entry for this summary
        // so a promoted Pass doesn't leave a duplicate observed item behind.
        self.evidence.retain(|item| {
            !(item.source == EvidenceSource::Validation
                && item.summary == summary
                && item.status == EvidenceStatus::Observed)
        });
        self.push_evidence(EvidenceItem {
            summary: summary.clone(),
            source: EvidenceSource::Validation,
            status,
        });
        if verdict == ValidationVerdict::Failed {
            self.focus = FocusState::Blocked;
            if !self
                .failures
                .iter()
                .any(|failure| failure.source == "validation" && failure.summary == summary)
            {
                self.failures.push(TaskFailure {
                    source: "validation".to_string(),
                    summary: summary.clone(),
                });
            }
            self.next_action = Some(
                "Diagnose the failed validation and change the implementation approach."
                    .to_string(),
            );
        }
    }

    /// Record snapshot-derived file evidence. Snapshot observation means files
    /// changed on disk — an observed fact, never a verified one.
    pub fn record_snapshot(&mut self, files: Vec<PathBuf>) {
        self.runtime.snapshot_files = files;
        self.runtime.snapshot_files.truncate(MAX_ITEMS);
        if !self.runtime.snapshot_files.is_empty() {
            self.changed_files = self.runtime.snapshot_files.clone();
            self.complexity.files_touched =
                self.complexity.files_touched.max(self.changed_files.len());
            self.push_evidence(EvidenceItem {
                summary: format!(
                    "snapshot: {} file(s) changed on disk",
                    self.runtime.snapshot_files.len()
                ),
                source: EvidenceSource::Snapshot,
                status: EvidenceStatus::Observed,
            });
        }
    }

    /// Record an explicit user decision. User statements are authoritative
    /// input but are still observations, not verified work.
    pub fn record_decision(&mut self, statement: impl Into<String>, evidence: Option<String>) {
        let raw = statement.into();
        let statement = truncate(&raw);
        self.decisions.push(TaskDecision {
            statement: statement.clone(),
            evidence,
        });
        self.push_evidence(EvidenceItem {
            summary: statement,
            source: EvidenceSource::User,
            status: EvidenceStatus::Observed,
        });
    }

    /// Record a model completion claim. Proposals are always `Observed` —
    /// there is no API path from a model claim to `Verified`.
    pub fn record_proposal(&mut self, claim: impl Into<String>) {
        let claim = claim.into();
        self.push_evidence(EvidenceItem {
            summary: truncate(&claim),
            source: EvidenceSource::ModelProposal,
            status: EvidenceStatus::Observed,
        });
    }

    /// Evidence that deterministic runtime checks have verified. Empty unless
    /// `record_validation` was called with [`ValidationVerdict::Passed`].
    pub fn verified_evidence(&self) -> impl Iterator<Item = &EvidenceItem> {
        self.evidence
            .iter()
            .filter(|item| item.status == EvidenceStatus::Verified)
    }

    pub fn render(&self) -> String {
        let mut lines = Vec::new();
        if let Some(objective) = &self.objective {
            lines.push(format!("Objective: {objective}"));
        }
        lines.push(format!("Focus: {}", self.focus.as_str()));
        if let Some(step) = self
            .runtime
            .plan_step
            .as_ref()
            .or(self.active_step.as_ref())
        {
            lines.push(format!("Active step: {step}"));
        }
        if let Some(validation) = &self.runtime.validation {
            lines.push(format!("Validation: {validation}"));
        }
        let verified: Vec<&EvidenceItem> = self.verified_evidence().collect();
        if !verified.is_empty() {
            lines.push(format!(
                "Verified: {}",
                verified
                    .iter()
                    .map(|item| item.summary.as_str())
                    .collect::<Vec<_>>()
                    .join(" | ")
            ));
        }
        if !self.constraints.is_empty() {
            lines.push(format!("Constraints: {}", self.constraints.join(" | ")));
        }
        if !self.changed_files.is_empty() {
            let files = self
                .changed_files
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>();
            lines.push(format!("Changed files: {}", files.join(", ")));
        }
        if !self.failures.is_empty() {
            lines.push(format!(
                "Recent failures: {}",
                self.failures
                    .iter()
                    .map(|f| format!("{}: {}", f.source, f.summary))
                    .collect::<Vec<_>>()
                    .join(" | ")
            ));
        }
        lines.push(format!(
            "Activity: {} tool calls, {} failed",
            self.complexity.tool_calls, self.complexity.failed_tools
        ));
        // Complexity warning renders ONLY when a threshold is exceeded — a
        // healthy run sees nothing. Signals drift, never authorizes undo.
        let exceeded = self.complexity.exceeded();
        if !exceeded.is_empty() {
            let details = exceeded
                .iter()
                .map(|(label, value, threshold)| format!("{label}: {value} (limit {threshold})"))
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(format!(
                "Complexity warning: {details}. Finish the requested work before adding more; consolidate rather than layering new abstractions."
            ));
        }
        if let Some(next) = &self.next_action {
            lines.push(format!("Next action: {next}"));
        }
        lines.push("Preserve the objective and constraints. Do not restart completed exploration or expand scope without evidence. Treat 'implemented' as unproven until validation passes.".to_string());
        lines.join("\n")
    }
}

fn is_tool_result(message: &Message) -> bool {
    matches!(&message.content, MessageContent::Blocks(blocks) if blocks.iter().any(|block| matches!(block, ContentBlock::ToolResult { .. })))
}

/// True when the message is a real user TEXT turn (not a tool-result message).
/// Only these carry the user's newest instruction; tool results are User-role
/// blocks and must not be treated as intent.
fn is_user_text_turn(message: &Message) -> bool {
    message.role == Role::User && !is_tool_result(message)
}

/// Canonical mutating-tool predicate for the LIVE reducer.
///
/// Deliberately the superset of core's `is_file_mutator` (canonical tool
/// names incl. `NotebookEdit`) and the legacy snake_case aliases that older
/// transcripts may still carry — the replay path (which feeds on event
/// `mutating` flags computed via core's predicate) must agree with this
/// live-path list for every tool the loop can name. `NotebookEdit` is in the
/// core list; the aliases are legacy-only and never emitted as events, so
/// they only affect the transcript pass of old sessions.
fn is_mutating_tool(name: &str) -> bool {
    matches!(
        name,
        "file_write" | "file_edit" | "batch_edit" | "apply_patch" | "write_file" | "edit_file"
    ) || clawde_core::constants::is_file_mutator(name)
}

fn tool_result_text(content: &clawde_core::types::ToolResultContent) -> String {
    match content {
        clawde_core::types::ToolResultContent::Text(text) => text.clone(),
        clawde_core::types::ToolResultContent::Blocks(_) => {
            "tool returned structured content".to_string()
        }
    }
}

pub(crate) fn truncate(value: &str) -> String {
    if value.chars().count() <= MAX_TEXT_CHARS {
        return value.to_string();
    }
    format!(
        "{}…",
        value.chars().take(MAX_TEXT_CHARS).collect::<String>()
    )
}

/// Parse a persisted snapshot evidence source (`EvidenceSource::as_str()`
/// spelling) back into the enum. Unknown sources map to `ModelProposal` —
/// the one variant that can never produce `Verified` evidence — so a future
/// source string degrades safely instead of over-claiming.
fn evidence_source_from_str(value: &str) -> EvidenceSource {
    match value {
        "user" => EvidenceSource::User,
        "tool" => EvidenceSource::Tool,
        "snapshot" => EvidenceSource::Snapshot,
        "validation" => EvidenceSource::Validation,
        "plan" => EvidenceSource::Plan,
        _ => EvidenceSource::ModelProposal,
    }
}

/// Parse a persisted snapshot evidence status back into the enum. Unknown
/// statuses map to `Observed` (never `Verified`) — same safe degradation.
fn evidence_status_from_str(value: &str) -> EvidenceStatus {
    match value {
        "verified" => EvidenceStatus::Verified,
        "failed" => EvidenceStatus::Failed,
        "superseded" => EvidenceStatus::Superseded,
        _ => EvidenceStatus::Observed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_instruction_replaces_objective_and_preserves_constraints() {
        let messages = vec![
            Message::user("Implement the parser. Do not add dependencies."),
            Message::assistant("I inspected the parser."),
            Message::user("Use the existing tokenizer and keep the API stable."),
        ];
        let state = TaskState::from_messages(&messages);
        assert_eq!(
            state.objective.as_deref(),
            Some("Use the existing tokenizer and keep the API stable.")
        );
        assert_eq!(state.constraints.len(), 2);
        assert_eq!(state.focus, FocusState::Active);
    }

    #[test]
    fn runtime_evidence_updates_plan_validation_and_snapshot_files() {
        let mut state = TaskState::from_messages(&[Message::user("Implement the feature")]);
        state.set_runtime_evidence(
            Some("Implement parser changes".to_string()),
            Some("tests passed".to_string()),
            vec![PathBuf::from("src/parser.rs")],
        );
        assert_eq!(
            state.runtime.plan_step.as_deref(),
            Some("Implement parser changes")
        );
        assert_eq!(state.runtime.validation.as_deref(), Some("tests passed"));
        assert_eq!(state.changed_files, vec![PathBuf::from("src/parser.rs")]);
        assert_eq!(state.focus, FocusState::Active);
    }

    #[test]
    fn failed_validation_blocks_focus() {
        let mut state = TaskState::from_messages(&[Message::user("Implement the feature")]);
        state.set_runtime_evidence(None, Some("tests failed: parser".to_string()), Vec::new());
        assert_eq!(state.focus, FocusState::Blocked);
        assert_eq!(state.failures.len(), 1);
    }

    #[test]
    fn model_proposals_never_mark_work_verified() {
        let mut state = TaskState::from_messages(&[Message::user("Implement the parser")]);
        state.record_proposal("Implemented the parser and it is complete.");
        let claim = Message::assistant("Implemented the feature, all tests pass.");
        state.apply_message(&claim);
        assert_eq!(state.verified_evidence().count(), 0);
        state.record_validation(ValidationVerdict::Passed, "3 checks passed");
        assert_eq!(state.verified_evidence().count(), 1);
        assert!(state
            .verified_evidence()
            .all(|item| item.source == EvidenceSource::Validation));
    }

    #[test]
    fn unknown_validation_is_observed_not_verified() {
        let mut state = TaskState::from_messages(&[Message::user("Do the thing")]);
        state.record_validation(ValidationVerdict::Unknown, "checks unavailable");
        assert_eq!(state.verified_evidence().count(), 0);
        assert_eq!(state.focus, FocusState::Active);
        assert_eq!(
            state.runtime.validation.as_deref(),
            Some("checks unavailable")
        );
    }

    #[test]
    fn repeated_failed_validation_does_not_duplicate_failures() {
        let mut state = TaskState::default();
        state.record_validation(ValidationVerdict::Failed, "tests failed: parser");
        state.record_validation(ValidationVerdict::Failed, "tests failed: parser");
        assert_eq!(state.failures.len(), 1);
        assert_eq!(state.focus, FocusState::Blocked);
    }

    #[test]
    fn passed_validation_supersedes_duplicate_observed_entry() {
        let mut state = TaskState::default();
        state.record_validation(ValidationVerdict::Unknown, "All checks passed");
        state.record_validation(ValidationVerdict::Passed, "All checks passed");
        let validation_items: Vec<_> = state
            .evidence
            .iter()
            .filter(|item| item.source == EvidenceSource::Validation)
            .collect();
        assert_eq!(validation_items.len(), 1);
        assert_eq!(validation_items[0].status, EvidenceStatus::Verified);
    }

    #[test]
    fn user_decision_records_authoritative_but_unverified_evidence() {
        let mut state = TaskState::default();
        state.record_decision(
            "Keep task state out of the transcript",
            Some("compaction architecture".to_string()),
        );
        assert_eq!(state.decisions.len(), 1);
        assert_eq!(state.evidence[0].source, EvidenceSource::User);
        assert_eq!(state.verified_evidence().count(), 0);
    }

    #[test]
    fn unrelated_question_does_not_mutate_objective() {
        let mut state = TaskState::from_messages(&[Message::user("Refactor the parser module")]);
        state.record_validation(ValidationVerdict::Passed, "checks passed");
        let question = Message::user("What is the capital of France?");
        state.apply_message(&question);
        assert_eq!(
            state.objective.as_deref(),
            Some("Refactor the parser module")
        );
        assert_eq!(state.verified_evidence().count(), 1);
        assert_eq!(state.focus, FocusState::Active);
    }

    #[test]
    fn correction_replaces_objective() {
        let mut state = TaskState::from_messages(&[Message::user("Build the REST client")]);
        let correction = Message::user("Actually, use the existing HTTP module instead");
        state.apply_message(&correction);
        assert_eq!(
            state.objective.as_deref(),
            Some("Actually, use the existing HTTP module instead")
        );
    }

    #[test]
    fn constraint_turn_keeps_objective_and_adds_constraint() {
        let mut state = TaskState::from_messages(&[Message::user("Speed up the build")]);
        let before = state.objective.clone();
        let constraint = Message::user("Do not touch the release profile");
        state.apply_message(&constraint);
        assert_eq!(state.objective, before);
        assert!(!state.constraints.is_empty());
    }

    #[test]
    fn related_question_on_shared_terms_clarifies() {
        let mut state = TaskState::from_messages(&[Message::user("Refactor the parser module")]);
        let question = Message::user("Why is the parser slow on large inputs?");
        state.apply_message(&question);
        assert_eq!(
            state.objective.as_deref(),
            Some("Refactor the parser module")
        );
    }
    #[test]
    fn notebook_edit_counts_as_mutating_tool_in_transcript_pass() {
        // The live transcript pass and the event path must agree on which
        // tools mutate files (audit fix: the reducer's hand-rolled list was
        // missing NotebookEdit, so live changed-file tracking diverged from
        // replay, which derives `mutating` from core's `is_file_mutator`).
        let message = Message::assistant_blocks(vec![ContentBlock::ToolUse {
            id: "nb-1".to_string(),
            name: "NotebookEdit".to_string(),
            input: serde_json::json!({
                "notebook_path": "notebooks/eda.ipynb",
                "cells": []
            }),
            thought_signature: None,
        }]);
        let state = TaskState::from_messages(&[message]);
        assert!(
            state.changed_files.is_empty(),
            "NotebookEdit carries notebook_path, not file_path — it must NOT be tracked via path-based changed-file logic"
        );
        assert_eq!(state.complexity.tool_calls, 1);
        // The parity contract lives in the predicate itself.
        assert!(is_mutating_tool("NotebookEdit"));
        assert!(is_mutating_tool("Edit"));
        assert!(is_mutating_tool("Write"));
        assert!(is_mutating_tool("BatchEdit"));
        assert!(is_mutating_tool("ApplyPatch"));
        assert!(!is_mutating_tool("Read"));
        assert!(!is_mutating_tool("Bash"));
    }

    #[test]
    fn refresh_from_messages_is_idempotent() {
        let messages = vec![
            Message::user("Fix the login bug. Never add dependencies."),
            Message::assistant_blocks(vec![ContentBlock::ToolUse {
                id: "1".to_string(),
                name: "Read".to_string(),
                input: serde_json::json!({"file_path": "src/auth.rs"}),
                thought_signature: None,
            }]),
            Message::user_blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "1".to_string(),
                content: clawde_core::types::ToolResultContent::Text("contents".to_string()),
                is_error: None,
            }]),
        ];
        let mut state = TaskState::from_messages(&messages);
        assert_eq!(state.complexity.tool_calls, 1);
        state.record_validation(ValidationVerdict::Passed, "3 checks passed");
        state.record_snapshot(vec![PathBuf::from("src/auth.rs")]);
        state.set_runtime_evidence(Some("Step 2: patch auth".to_string()), None, Vec::new());
        let verified_before = state.verified_evidence().count();

        state.refresh_from_messages(&messages);
        assert_eq!(
            state.complexity.tool_calls, 1,
            "refresh must not double-count"
        );
        assert_eq!(state.verified_evidence().count(), verified_before);
        assert_eq!(
            state.runtime.plan_step.as_deref(),
            Some("Step 2: patch auth")
        );
        // Runtime snapshot evidence survives the refresh (a fresh transcript
        // build would not have it — that preservation is the point).
        assert!(state.changed_files.contains(&PathBuf::from("src/auth.rs")));
        assert_eq!(state.runtime.validation.as_deref(), Some("3 checks passed"));

        state.refresh_from_messages(&messages);
        assert_eq!(
            state.complexity.tool_calls, 1,
            "second refresh is also stable"
        );
    }

    #[test]
    fn complexity_warning_only_renders_when_thresholds_exceeded() {
        let mut state = TaskState::default();
        state.complexity.tool_calls = COMPLEXITY_THRESHOLDS.tool_calls;
        assert!(!state.render().contains("Complexity warning"));
        state.complexity.tool_calls = COMPLEXITY_THRESHOLDS.tool_calls + 1;
        let rendered = state.render();
        assert!(rendered.contains("Complexity warning"));
        assert!(rendered.contains("tool calls"));
    }

    #[test]
    fn repeated_identical_failures_raise_retry_signal() {
        let mut state = TaskState::default();
        for _ in 0..4 {
            state.apply_message(&Message::user_blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "t".to_string(),
                content: clawde_core::types::ToolResultContent::Text(
                    "connection refused".to_string(),
                ),
                is_error: Some(true),
            }]));
        }
        // 4 identical failures -> the counter reports 4 total occurrences.
        assert_eq!(state.complexity.repeated_failures_per_target, 4);
        assert!(state.render().contains("repeated failures"));
    }

    #[test]
    fn scope_expansion_counter_tracks_expansion_turns() {
        let mut state = TaskState::from_messages(&[Message::user("Build the CLI")]);
        state.apply_message(&Message::user("Also add a config file reader"));
        assert_eq!(state.complexity.scope_expansions, 1);
    }

    #[test]
    fn replayed_events_match_live_accumulation() {
        // Realistic transcript: the user prompt AND the tool result that
        // followed it are both in the message chain (as they are on disk).
        let messages = vec![
            Message::user("Fix the login bug"),
            Message::user_blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "t".to_string(),
                content: clawde_core::types::ToolResultContent::Text(
                    "connection refused".to_string(),
                ),
                is_error: Some(true),
            }]),
        ];

        // Live path: reduce the same chain, then accumulate the runtime
        // records the events persist. The decision models the loop's redirect
        // emitter: its statement is the objective pivot text.
        let mut live = TaskState::from_messages(&messages);
        live.record_snapshot(vec![PathBuf::from("src/auth.rs")]);
        live.record_validation(ValidationVerdict::Passed, "All checks passed");
        live.record_decision("Fix the login bug", None);

        // Replay path: the same message chain plus the same facts as events.
        use clawde_core::session_storage::StateEvent;
        let events = vec![
            StateEvent::SnapshotObserved {
                files: vec!["src/auth.rs".to_string()],
            },
            StateEvent::ValidationRecorded {
                verdict: clawde_core::session_storage::StateValidationVerdict::Passed,
                headline: "All checks passed".to_string(),
            },
            StateEvent::DecisionRecorded {
                statement: "Fix the login bug".to_string(),
                evidence: None,
            },
            StateEvent::ToolObserved {
                failed: true,
                summary: "connection refused".to_string(),
                file_paths: vec![],
                mutating: false,
            },
        ];
        let replayed = TaskState::replay(&messages, &events);

        assert_eq!(replayed.objective, live.objective);
        assert_eq!(replayed.focus, live.focus);
        assert_eq!(replayed.evidence, live.evidence);
        assert_eq!(replayed.failures, live.failures);
        assert_eq!(replayed.changed_files, live.changed_files);
        // Counters: live counted the failure once via the tool result in the
        // chain; replay clears the chain-derived counters and rebuilds from
        // the single event — equal. `tool_calls` is 1 from the event because
        // replay counts observations, and live counts tool USE messages (none
        // in this chain).
        assert_eq!(
            replayed.complexity.failed_tools,
            live.complexity.failed_tools
        );
        assert_eq!(
            replayed.complexity.repeated_failures_per_target,
            live.complexity.repeated_failures_per_target
        );
        assert_eq!(replayed.complexity.tool_calls, 1);
        assert_eq!(
            replayed.complexity.tool_calls,
            live.complexity.tool_calls.saturating_add(1)
        );
        assert_eq!(replayed.decisions.len(), live.decisions.len());
    }

    #[test]
    fn replay_does_not_duplicate_failures_when_chain_and_event_agree() {
        // The transcript contains the failing tool result AND the persisted
        // ToolObserved event describes the same result. Before the audit fix,
        // replay pushed the failure twice (once from the chain pass, once from
        // the event) and inflated repeated_failures_per_target.
        use clawde_core::session_storage::StateEvent;
        let messages = vec![
            Message::user("Fix the login bug"),
            Message::user_blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "t".to_string(),
                content: clawde_core::types::ToolResultContent::Text("boom".to_string()),
                is_error: Some(true),
            }]),
        ];
        let events = vec![StateEvent::ToolObserved {
            failed: true,
            summary: "boom".to_string(),
            file_paths: vec![],
            mutating: false,
        }];
        let state = TaskState::replay(&messages, &events);
        assert_eq!(state.failures.len(), 1);
        assert_eq!(state.complexity.failed_tools, 1);
        assert_eq!(state.complexity.repeated_failures_per_target, 1);
        assert_eq!(state.focus, FocusState::Blocked);
    }

    #[test]
    fn replay_newest_user_text_keeps_transcript_focus() {
        // The user typed a NEW instruction after an old tool failure. The
        // failure event predates that prompt, so replay must NOT resurrect a
        // Blocked focus over the user's newest intent.
        use clawde_core::session_storage::StateEvent;
        let messages = vec![
            Message::user("Try approach A"),
            Message::user_blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "t".to_string(),
                content: clawde_core::types::ToolResultContent::Text("boom".to_string()),
                is_error: Some(true),
            }]),
            Message::user("Actually use approach B"),
        ];
        let events = vec![StateEvent::ToolObserved {
            failed: true,
            summary: "boom".to_string(),
            file_paths: vec![],
            mutating: false,
        }];
        let state = TaskState::replay(&messages, &events);
        // The transcript-derived focus for the newest user text wins; the
        // stale event failure is still recorded as evidence/failure.
        assert_eq!(state.focus, FocusState::Active);
        assert_eq!(state.failures.len(), 1);
        assert_eq!(
            state.next_action.as_deref(),
            Some("Continue from the latest user instruction using existing evidence.")
        );
    }

    #[test]
    fn replay_preserves_claim_proof_boundary() {
        // Realistic transcript: the user prompt, then the assistant turn whose
        // validation outcome the events describe. The chain ends on assistant
        // text, so event-derived focus (Blocked after the failed round) is the
        // newest fact and is kept.
        let messages = vec![
            Message::user("Implement the feature"),
            Message::assistant("implemented the feature and ran the checks"),
        ];
        use clawde_core::session_storage::StateEvent;
        let events = vec![
            // A failed round followed by a passing round: only the pass
            // verifies, and verified evidence comes only from validation.
            StateEvent::ValidationRecorded {
                verdict: clawde_core::session_storage::StateValidationVerdict::Failed,
                headline: "2 checks failed".to_string(),
            },
            StateEvent::ValidationRecorded {
                verdict: clawde_core::session_storage::StateValidationVerdict::Passed,
                headline: "All checks passed".to_string(),
            },
        ];
        let state = TaskState::replay(&messages, &events);
        assert_eq!(state.verified_evidence().count(), 1);
        assert!(state
            .verified_evidence()
            .all(|item| item.source == EvidenceSource::Validation));
        // Focus mirrors live loop semantics: a failed round blocks focus and a
        // later pass adds verified evidence but does not itself unblock (the
        // loop clears focus on the next successful tool result).
        assert_eq!(state.focus, FocusState::Blocked);
    }

    #[test]
    fn replay_validation_after_user_text_keeps_user_focus() {
        // The newest transcript message is a user TEXT turn; its focus effect
        // is newer than every event (events only exist from prior runs). A
        // failed validation must not resurrect Blocked over the user's latest
        // instruction — but the failure is still recorded.
        let messages = vec![Message::user("Implement the feature")];
        use clawde_core::session_storage::StateEvent;
        let events = vec![StateEvent::ValidationRecorded {
            verdict: clawde_core::session_storage::StateValidationVerdict::Failed,
            headline: "2 checks failed".to_string(),
        }];
        let state = TaskState::replay(&messages, &events);
        assert_eq!(state.focus, FocusState::Active);
        assert!(state
            .failures
            .iter()
            .any(|f| f.source == "validation" && f.summary == "2 checks failed"));
    }

    #[test]
    fn replay_counters_survive_and_are_stable() {
        use clawde_core::session_storage::StateEvent;
        let messages = vec![Message::user("Refactor the parser")];
        let events = vec![
            StateEvent::ToolObserved {
                failed: false,
                summary: String::new(),
                file_paths: vec!["src/parser.rs".to_string()],
                mutating: true,
            },
            StateEvent::ToolObserved {
                failed: true,
                summary: "boom".to_string(),
                file_paths: vec![],
                mutating: false,
            },
            StateEvent::ToolObserved {
                failed: true,
                summary: "boom".to_string(),
                file_paths: vec![],
                mutating: false,
            },
            StateEvent::PlanStepSet {
                step: "Step 3".to_string(),
            },
            StateEvent::SimplificationReviewed,
        ];
        let state = TaskState::replay(&messages, &events);
        assert_eq!(state.complexity.tool_calls, 3);
        assert_eq!(state.complexity.files_touched, 1);
        assert_eq!(state.complexity.failed_tools, 2);
        assert_eq!(state.complexity.repeated_failures_per_target, 2);
        assert_eq!(state.runtime.plan_step.as_deref(), Some("Step 3"));
        assert!(state.simplification_reviewed);
        assert!(state
            .changed_files
            .contains(&PathBuf::from("src/parser.rs")));

        // Idempotent under a second identical replay.
        let again = TaskState::replay(&messages, &events);
        assert_eq!(again.complexity, state.complexity);
    }

    #[test]
    fn failed_tool_changes_focus_and_records_failure() {
        let message = Message::user_blocks(vec![ContentBlock::ToolResult {
            tool_use_id: "tool-1".to_string(),
            content: clawde_core::types::ToolResultContent::Text("not found".to_string()),
            is_error: Some(true),
        }]);
        let state = TaskState::from_messages(&[message]);
        assert_eq!(state.focus, FocusState::Blocked);
        assert_eq!(state.failures.len(), 1);
    }

    // --- Snapshot-based incremental replay parity ---
    //
    // The snapshot path must equal the full `replay` on the same transcript:
    // a snapshot is a CACHE of the event fold, and any divergence would make
    // sessions with snapshots resume differently from sessions without them.

    fn tool_use(name: &str, id: &str, file_path: &str) -> Message {
        Message::assistant_blocks(vec![ContentBlock::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input: serde_json::json!({"file_path": file_path}),
            thought_signature: None,
        }])
    }

    fn tool_result(id: &str, text: &str, is_error: bool) -> Message {
        Message::user_blocks(vec![ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content: clawde_core::types::ToolResultContent::Text(text.to_string()),
            is_error: Some(is_error),
        }])
    }

    fn tool_observed(
        failed: bool,
        summary: &str,
        path: &str,
        mutating: bool,
    ) -> clawde_core::session_storage::StateEvent {
        use clawde_core::session_storage::StateEvent;
        StateEvent::ToolObserved {
            failed,
            summary: summary.to_string(),
            file_paths: if path.is_empty() {
                Vec::new()
            } else {
                vec![path.to_string()]
            },
            mutating,
        }
    }

    fn snapshot_of(
        state: &TaskState,
        event_count: u64,
    ) -> clawde_core::session_storage::StateSnapshot {
        use clawde_core::session_storage::{StateSnapshot, STATE_SNAPSHOT_SCHEMA_VERSION};
        StateSnapshot {
            schema_version: STATE_SNAPSHOT_SCHEMA_VERSION,
            event_count,
            body: crate::state_emission::build_state_snapshot_body(state),
        }
    }

    #[test]
    fn snapshot_replay_matches_full_replay_across_next_prompt() {
        // Run 1 ends at an event watermark; the loop persists a snapshot of
        // the projected state. Run 2 starts with a new user prompt (the
        // common interactive case): the snapshot path must equal replaying
        // the whole event history.
        use clawde_core::session_storage::{StateEvent, StateValidationVerdict};
        let messages_0 = vec![
            Message::user("Refactor the parser"),
            tool_use("Write", "w1", "src/parser.rs"),
            tool_result("w1", "wrote 42 lines", false),
            tool_use("Read", "r1", "docs/x.md"),
            tool_result("r1", "boom", true),
            Message::assistant("implemented the parser and ran the checks"),
        ];
        let events_0 = vec![
            tool_observed(false, "", "src/parser.rs", true),
            tool_observed(true, "boom", "", false),
            StateEvent::ValidationRecorded {
                verdict: StateValidationVerdict::Passed,
                headline: "All checks passed".to_string(),
            },
            StateEvent::PlanStepSet {
                step: "Step 3".to_string(),
            },
            StateEvent::SimplificationReviewed,
        ];
        // The state the loop would serialize at watermark 5.
        let at_watermark = TaskState::replay(&messages_0, &events_0);
        let snapshot = snapshot_of(&at_watermark, 5);

        let mut messages_all = messages_0.clone();
        messages_all.push(Message::user("Actually run the linter first"));
        let full = TaskState::replay(&messages_all, &events_0);
        let incremental = TaskState::replay_with_snapshot(&messages_all, &snapshot, &[]);
        assert_eq!(incremental, full, "snapshot path must equal full replay");
        // Spot-check the facts that motivated the snapshot feature.
        assert_eq!(incremental.verified_evidence().count(), 1);
        assert_eq!(incremental.runtime.plan_step.as_deref(), Some("Step 3"));
        assert!(incremental.simplification_reviewed);
        assert_eq!(incremental.complexity.tool_calls, 2);
        assert!(incremental
            .changed_files
            .contains(&PathBuf::from("src/parser.rs")));
        // The new user text wins over the stale failure event.
        assert_eq!(incremental.focus, FocusState::Active);
        assert_eq!(
            incremental.objective.as_deref(),
            Some("Actually run the linter first")
        );
    }

    #[test]
    fn snapshot_replay_matches_full_replay_with_tail_events() {
        // Crash-resume shape: the snapshot was written at watermark 4, then a
        // later turn wrote more events (and their transcript messages) before
        // the process died. The load folds the snapshot body + tail events;
        // the newest message is a tool result, so event-derived focus (and the
        // failed-validation next action) is the newest fact and is kept.
        use clawde_core::session_storage::{StateEvent, StateValidationVerdict};
        let messages_0 = vec![
            Message::user("Fix the login bug"),
            tool_use("Write", "w1", "src/auth.rs"),
            tool_result("w1", "patched", false),
        ];
        let events_0 = vec![
            tool_observed(false, "", "src/auth.rs", true),
            StateEvent::ValidationRecorded {
                verdict: StateValidationVerdict::Failed,
                headline: "2 checks failed".to_string(),
            },
            StateEvent::ToolObserved {
                failed: true,
                summary: "connection refused".to_string(),
                file_paths: vec![],
                mutating: false,
            },
        ];
        let at_watermark = TaskState::replay(&messages_0, &events_0);
        let snapshot = snapshot_of(&at_watermark, 3);

        // The crashed turn: another failing tool call after the snapshot.
        let mut messages_all = messages_0.clone();
        messages_all.push(tool_use("Bash", "b1", ""));
        messages_all.push(tool_result("b1", "file not found", true));
        let tail = vec![StateEvent::ToolObserved {
            failed: true,
            summary: "file not found".to_string(),
            file_paths: vec![],
            mutating: false,
        }];

        let full = TaskState::replay(
            &messages_all,
            &events_0
                .iter()
                .cloned()
                .chain(tail.iter().cloned())
                .collect::<Vec<_>>(),
        );
        let incremental = TaskState::replay_with_snapshot(&messages_all, &snapshot, &tail);
        assert_eq!(incremental, full, "snapshot + tail must equal full replay");
        assert_eq!(incremental.focus, FocusState::Blocked);
        assert_eq!(
            incremental.next_action.as_deref(),
            Some("Diagnose the failed validation and change the implementation approach.")
        );
        assert_eq!(incremental.complexity.tool_calls, 3);
        assert_eq!(incremental.complexity.failed_tools, 2);
        // Two failures in the snapshot history (validation + tool) plus the
        // crashed turn's tool failure.
        assert_eq!(incremental.failures.len(), 3);
    }

    #[test]
    fn snapshot_replay_validation_failure_yields_to_newest_user_text() {
        // A failed validation in the snapshot history must not resurrect
        // Blocked (or its diagnostic next action) over a newer user prompt.
        use clawde_core::session_storage::{StateEvent, StateValidationVerdict};
        let messages_0 = vec![
            Message::user("Implement the feature"),
            Message::assistant("implemented the feature and ran the checks"),
        ];
        let events_0 = vec![
            StateEvent::ValidationRecorded {
                verdict: StateValidationVerdict::Failed,
                headline: "2 checks failed".to_string(),
            },
            StateEvent::ToolObserved {
                failed: true,
                summary: "boom".to_string(),
                file_paths: vec![],
                mutating: false,
            },
        ];
        let at_watermark = TaskState::replay(&messages_0, &events_0);
        let snapshot = snapshot_of(&at_watermark, 2);

        let mut messages_all = messages_0.clone();
        messages_all.push(Message::user("Actually, use the simpler API instead"));
        let full = TaskState::replay(&messages_all, &events_0);
        let incremental = TaskState::replay_with_snapshot(&messages_all, &snapshot, &[]);
        assert_eq!(incremental, full);
        assert_eq!(incremental.focus, FocusState::Active);
        assert_eq!(
            incremental.next_action.as_deref(),
            Some("Continue from the latest user instruction using existing evidence.")
        );
        // The failure is still recorded — only focus/next are transcript-owned.
        assert_eq!(incremental.failures.len(), 2);
    }

    #[test]
    fn snapshot_body_round_trips_evidence_provenance() {
        // Evidence source/status survive the string round-trip with the
        // claim/proof boundary intact: Verified stays Verified only for
        // validation items; an unknown future source degrades to
        // ModelProposal (never Verified-capable).
        use clawde_core::session_storage::{StateEvent, StateValidationVerdict};
        let messages = vec![Message::user("Implement the feature")];
        let events = vec![StateEvent::ValidationRecorded {
            verdict: StateValidationVerdict::Passed,
            headline: "All checks passed".to_string(),
        }];
        let state = TaskState::replay(&messages, &events);
        let body = crate::state_emission::build_state_snapshot_body(&state);
        // Mutate an item source to an unknown spelling, then re-apply.
        let mut body = body;
        body.evidence[0].source = "future-source".to_string();
        let mut rebuilt = TaskState::from_messages(&messages);
        rebuilt.apply_snapshot_body(&body);
        assert_eq!(rebuilt.verified_evidence().count(), 0);
        assert_eq!(rebuilt.evidence[0].source, EvidenceSource::ModelProposal);
        // And a normal round trip keeps the Verified flag.
        let body2 = crate::state_emission::build_state_snapshot_body(&state);
        let mut rebuilt2 = TaskState::from_messages(&messages);
        rebuilt2.apply_snapshot_body(&body2);
        assert_eq!(rebuilt2.verified_evidence().count(), 1);
    }

    // --- Schema-driven field-survival inventory ---
    //
    // The persistence contract: every field the loop renders into
    // `<task_context>` (or consumes for loop behavior) must be reproducible
    // from the transcript + event log — a fact that only exists live and is
    // never persisted is gone after replay, in the worst place to discover
    // it ("not in the log means not after replay").
    //
    // `covered_fields` IS the schema: adding a consumer-relevant field to
    // `TaskState` means adding an entry here AND extending the kitchen-sink
    // scenario below to produce it. The survival test then fails with the
    // uncovered field named, which is the reminder that the field needs a
    // producing message classification, event, or snapshot body field.
    fn covered_fields(state: &TaskState) -> Vec<(&'static str, bool)> {
        vec![
            (
                "objective",
                state.objective.as_ref().is_some_and(|s| !s.is_empty()),
            ),
            ("focus", true),
            (
                "active step",
                state
                    .runtime
                    .plan_step
                    .as_ref()
                    .or(state.active_step.as_ref())
                    .is_some(),
            ),
            ("validation headline", state.runtime.validation.is_some()),
            ("constraints", !state.constraints.is_empty()),
            ("changed files", !state.changed_files.is_empty()),
            ("failures", !state.failures.is_empty()),
            ("decisions", !state.decisions.is_empty()),
            ("verified evidence", state.verified_evidence().count() > 0),
            ("snapshot files", !state.runtime.snapshot_files.is_empty()),
            ("simplification reviewed", state.simplification_reviewed),
            ("next action", state.next_action.is_some()),
            ("tool calls", state.complexity.tool_calls > 0),
            ("failed tools", state.complexity.failed_tools > 0),
            ("files touched", state.complexity.files_touched > 0),
            (
                "repeated failures",
                state.complexity.repeated_failures_per_target > 0,
            ),
            ("scope expansions", state.complexity.scope_expansions > 0),
        ]
    }

    #[test]
    fn every_consumer_relevant_field_survives_message_and_snapshot_replay() {
        use clawde_core::session_storage::{StateEvent, StateValidationVerdict};
        // Kitchen-sink session: message classifications + one event per
        // runtime producer, so a full replay must rebuild every field below.
        let messages = vec![
            Message::user("Refactor the parser module. Never add dependencies."),
            Message::user("Also add caching for the tokenizer"),
            tool_use("Write", "w1", "src/parser.rs"),
            tool_result("w1", "wrote 120 lines", false),
            tool_use("Write", "w2", "src/cache.rs"),
            tool_result("w2", "added tokenizer cache", false),
            tool_use("Write", "w3", "src/parser.rs"),
            tool_result("w3", "tests failed: 2", true),
            tool_use("Read", "r1", "docs/x.md"),
            tool_result("r1", "tests failed: 2", true),
        ];
        let events = vec![
            tool_observed(false, "", "src/parser.rs", true),
            tool_observed(false, "", "src/cache.rs", true),
            tool_observed(true, "tests failed: 2", "src/parser.rs", true),
            tool_observed(true, "tests failed: 2", "", false),
            StateEvent::SnapshotObserved {
                files: vec!["src/parser.rs".to_string(), "src/cache.rs".to_string()],
            },
            StateEvent::ValidationRecorded {
                verdict: StateValidationVerdict::Passed,
                headline: "3 checks passed".to_string(),
            },
            StateEvent::PlanStepSet {
                step: "Step 2: verify the tokenizer cache".to_string(),
            },
            StateEvent::DecisionRecorded {
                statement: "Keep the public API stable".to_string(),
                evidence: None,
            },
            StateEvent::SimplificationReviewed,
        ];

        // 1. Full message+event replay covers every field.
        let full = TaskState::replay(&messages, &events);
        let uncovered: Vec<&str> = covered_fields(&full)
            .into_iter()
            .filter_map(|(name, present)| (!present).then_some(name))
            .collect();
        assert!(
            uncovered.is_empty(),
            "fields not produced by message+event replay: {uncovered:?} — \
             add a producing event/classification or extend the scenario"
        );

        // 2. The snapshot path (body built at the full watermark) covers the
        // same fields AND equals the full replay exactly.
        let snapshot = snapshot_of(&full, events.len() as u64);
        let incremental = TaskState::replay_with_snapshot(&messages, &snapshot, &[]);
        assert_eq!(incremental, full, "snapshot must fold identically");
        let uncovered: Vec<&str> = covered_fields(&incremental)
            .into_iter()
            .filter_map(|(name, present)| (!present).then_some(name))
            .collect();
        assert!(
            uncovered.is_empty(),
            "fields lost across the snapshot boundary: {uncovered:?}"
        );

        // 3. Every field the inventory names as RENDERED appears in the
        // replayed <task_context> output — the inventory is the consumed
        // surface, not a private checklist.
        let rendered = incremental.render();
        let markers = [
            ("objective", "Objective:"),
            ("focus", "Focus:"),
            ("active step", "Active step:"),
            ("validation headline", "Validation:"),
            ("constraints", "Constraints:"),
            ("changed files", "Changed files:"),
            ("failures", "Recent failures:"),
            ("verified evidence", "Verified:"),
            ("next action", "Next action:"),
        ];
        for (field, marker) in markers {
            assert!(
                rendered.contains(marker),
                "rendered <task_context> is missing '{marker}' for the \
                 populated '{field}' field"
            );
        }
        assert!(
            rendered.contains("Activity:"),
            "activity line always renders"
        );
        assert!(
            rendered.contains("4 tool calls, 2 failed"),
            "counters must render exactly"
        );
        assert_eq!(incremental.complexity.scope_expansions, 1);
        assert_eq!(incremental.verified_evidence().count(), 1);
        assert_eq!(incremental.failures.len(), 2);
        assert_eq!(incremental.complexity.files_touched, 2);
    }

    #[test]
    fn redirect_turn_sets_newest_redirect_and_catch_up_emits_once() {
        // A correction pivot: `newest_redirect` captures the statement, the
        // first catch_up emits it, the second (refreshed from the same
        // transcript, redirect already in decisions) emits nothing.
        let mut state = TaskState::from_messages(&[Message::user("Build the CLI parser")]);
        assert!(
            state.newest_redirect.is_none(),
            "first turn is a definition"
        );
        state.apply_message(&Message::user(
            "Actually, use the Pest crate instead of a hand-rolled parser",
        ));
        assert!(state.newest_redirect.is_some(), "correction is a redirect");
        let statement = state.catch_up_decisions().expect("emits once");
        assert_eq!(statement, state.objective.as_deref().unwrap());
        assert!(state.decisions.iter().any(|d| d.statement == statement));
        // Refresh + re-catch-up: same transcript, decision already recorded.
        state.refresh_from_messages(&[
            Message::user("Build the CLI parser"),
            Message::user("Actually, use the Pest crate instead of a hand-rolled parser"),
        ]);
        assert!(state.catch_up_decisions().is_none(), "deduped");
    }

    #[test]
    fn non_redirect_turns_do_not_emit_decisions() {
        let mut state = TaskState::from_messages(&[Message::user("Build the CLI parser")]);
        state.apply_message(&Message::user("Never add new dependencies"));
        assert!(
            state.newest_redirect.is_none(),
            "constraint is not a redirect"
        );
        assert!(state.catch_up_decisions().is_none());
        state.apply_message(&Message::user("What is the current parser design?"));
        assert!(
            state.newest_redirect.is_none(),
            "question is not a redirect"
        );
        assert!(state.catch_up_decisions().is_none());
    }

    #[test]
    fn decision_replay_restores_objective_after_compaction() {
        // The scenario the redirect emitter exists for: compaction replaced
        // the message list, dropping the correction message. The transcript
        // pass derives the PRE-correction objective; the persisted decision
        // event (its statement is the pivot text) must restore the corrected
        // one — identically on the event path and the snapshot path.
        let pre_correction = vec![Message::user("Build the CLI parser")];
        let mut live = TaskState::from_messages(&pre_correction);
        live.apply_message(&Message::user(
            "Actually, use the Pest crate instead of a hand-rolled parser",
        ));
        let statement = live.catch_up_decisions().expect("redirect emitted");

        // Post-compaction transcript: the redirect message is GONE.
        let compacted = vec![Message::user("Build the CLI parser")];
        let events = vec![clawde_core::session_storage::StateEvent::DecisionRecorded {
            statement: statement.clone(),
            evidence: None,
        }];
        let replayed = TaskState::replay(&compacted, &events);
        assert_eq!(
            replayed.objective.as_deref(),
            Some(statement.as_str()),
            "event replay restores the corrected objective"
        );
        let snapshot = snapshot_of(&replayed, events.len() as u64);
        let incremental = TaskState::replay_with_snapshot(&compacted, &snapshot, &[]);
        assert_eq!(
            incremental.objective, replayed.objective,
            "snapshot path restores identically"
        );
    }
}
