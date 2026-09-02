//! Runtime agent-state event emission (event-persistence Phase 2).
//!
//! The transcript alone cannot reconstruct every runtime fact the query
//! loop's `TaskState` tracks: verify verdicts, snapshot evidence, the active
//! plan step, and the simplification-review flag are never stored as
//! messages, and compaction can remove tool-result messages from the active
//! branch. This module appends those facts to the session's JSONL transcript
//! as `state-event` entries so a resumed session can replay them (Phase 3).
//!
//! Emission is strictly best-effort: a transcript write failure never
//! affects the query loop's behavior or outcome. Deduplication applies only
//! to repeating evidence (plan step, snapshot files); tool and validation
//! events are written for every occurrence because replay relies on their
//! multiplicity (retry-loop counting, failure history).

use crate::task_state::truncate;
use clawde_core::session_storage::{
    make_state_event_entry_at, make_state_snapshot_entry, write_transcript_entry, StateEvent,
    StateSnapshot, StateSnapshotBody, StateSnapshotDecision, StateSnapshotEvidence,
    StateSnapshotFailure, StateValidationVerdict,
};
use clawde_core::types::{ContentBlock, ToolResultContent};
use serde_json::Value;
use std::path::PathBuf;

/// Snapshot cadence: a projected state snapshot is written once this many
/// state events have accumulated since the newest snapshot. Every event
/// written counts toward the session-global total, so long sessions cross
/// this roughly every `STATE_SNAPSHOT_INTERVAL` tool results/verdicts; short
/// sessions may never cross it and stay on the full replay path.
pub(crate) const STATE_SNAPSHOT_INTERVAL: u64 = 64;

/// Loop-local emitter created once per query loop.
pub(crate) struct StateEventEmitter {
    /// Transcript path resolved at construction; `None` disables writing
    /// (dedup tracking still runs, keeping the type simple).
    path: Option<PathBuf>,
    session_id: String,
    last_plan_step: Option<String>,
    last_snapshot_files: Option<Vec<String>>,
    /// State events actually written to the transcript (dedup'd plan/snapshot
    /// events excluded — the counter is incremented only on real writes),
    /// seeded with the count already present at loop start so the cadence
    /// accumulates across runs within one session.
    total_events: u64,
    /// Branch anchor: index of the assistant message currently being
    /// processed, counted in the loop's in-memory `messages` vec — the same
    /// vec a rewind truncates. Stamped onto every written event so the
    /// cut-based extractor can drop abandoned-branch events on replay. `None`
    /// (before the first assistant push of the run) writes unanchored events,
    /// which replay always keeps.
    current_msg_index: Option<u32>,
}

impl StateEventEmitter {
    /// `initial_event_count` is the number of state events already in the
    /// transcript when this loop starts (from the snapshot-aware load), so
    /// the snapshot cadence is session-global, not per-process.
    pub(crate) fn new(
        working_dir: &std::path::Path,
        session_id: &str,
        initial_event_count: u64,
    ) -> Self {
        let project_root = clawde_core::git_utils::project_root(working_dir);
        Self {
            path: clawde_core::session_storage::transcript_path(&project_root, session_id).ok(),
            session_id: session_id.to_string(),
            last_plan_step: None,
            last_snapshot_files: None,
            total_events: initial_event_count,
            current_msg_index: None,
        }
    }

    /// An emitter that never writes (sub-agent loops that share the parent
    /// session id must not pollute the parent's event stream). Dedup tracking
    /// still runs so the type behaves identically everywhere.
    pub(crate) fn disabled() -> Self {
        Self {
            path: None,
            session_id: String::new(),
            last_plan_step: None,
            last_snapshot_files: None,
            total_events: 0,
            current_msg_index: None,
        }
    }

    /// Anchor subsequent event writes to the given assistant-message index
    /// (the loop's in-memory push order). Called right after each assistant
    /// push so tool/validation emissions of that turn carry the index a
    /// rewind can cut against.
    pub(crate) fn set_message_index(&mut self, index: u32) {
        self.current_msg_index = Some(index);
    }

    /// Total state events this session's transcript holds or will hold once
    /// this loop's writes land (initial count + writes so far).
    pub(crate) fn total_events(&self) -> u64 {
        self.total_events
    }

    /// Append one state event. The counter is incremented on every write
    /// attempt for an enabled emitter so the snapshot watermark tracks the
    /// events the file holds; a failed write leaves the counter ahead of the
    /// file, which the snapshot loader's watermark check then detects and
    /// falls back from (safe, cache-only degradation).
    async fn write(&mut self, event: StateEvent) {
        if let Some(path) = &self.path {
            self.total_events += 1;
            let entry = make_state_event_entry_at(&self.session_id, event, self.current_msg_index);
            let _ = write_transcript_entry(path, &entry).await;
        }
    }

    /// Write a state snapshot covering `event_count` state events (the
    /// session-global total at write time). Best-effort like every other
    /// write; the loader validates the watermark against the file before
    /// trusting it.
    pub(crate) async fn record_state_snapshot(&self, snapshot: StateSnapshot) {
        if let Some(path) = &self.path {
            let entry = make_state_snapshot_entry(&self.session_id, snapshot);
            let _ = write_transcript_entry(path, &entry).await;
        }
    }

    /// Compute the plan-step event to write, deduplicating unchanged values.
    fn plan_step_event(&mut self, step: Option<String>) -> Option<StateEvent> {
        if step.is_some() && self.last_plan_step != step {
            self.last_plan_step = step.clone();
            return step.map(|step| StateEvent::PlanStepSet { step });
        }
        None
    }

    /// Record the active plan step (deduplicated: only changes are written).
    pub(crate) async fn record_plan_step(&mut self, step: Option<String>) {
        if let Some(event) = self.plan_step_event(step) {
            self.write(event).await;
        }
    }

    /// Compute the snapshot event to write, deduplicating by normalized set.
    fn snapshot_event(&mut self, files: Vec<PathBuf>) -> Option<StateEvent> {
        if files.is_empty() {
            return None;
        }
        let mut names: Vec<String> = files.iter().map(|p| p.display().to_string()).collect();
        names.sort();
        names.dedup();
        if self.last_snapshot_files.as_ref() != Some(&names) {
            self.last_snapshot_files = Some(names.clone());
            return Some(StateEvent::SnapshotObserved { files: names });
        }
        None
    }

    /// Record snapshot/context file evidence (deduplicated by set identity).
    pub(crate) async fn record_snapshot_files(&mut self, files: Vec<PathBuf>) {
        if let Some(event) = self.snapshot_event(files) {
            self.write(event).await;
        }
    }

    /// Record a user-redirect decision (correction, related subtask, or
    /// objective replace). Dedup happens at the call site
    /// (`TaskState::catch_up_decisions`), so this writes unconditionally.
    pub(crate) async fn record_decision(&mut self, statement: String) {
        self.write(StateEvent::DecisionRecorded {
            statement,
            evidence: None,
        })
        .await;
    }

    /// Record a completed validation round. Only `Passed` may verify on
    /// replay — the claim/proof boundary survives persistence.
    pub(crate) async fn record_validation(
        &mut self,
        verdict: StateValidationVerdict,
        headline: String,
    ) {
        self.write(StateEvent::ValidationRecorded { verdict, headline })
            .await;
    }

    /// Record the one-shot simplification-review marker.
    pub(crate) async fn record_simplification_reviewed(&mut self) {
        self.write(StateEvent::SimplificationReviewed).await;
    }

    /// Write the given tool observation events in order.
    pub(crate) async fn record_tool_events(&mut self, events: Vec<StateEvent>) {
        for event in events {
            self.write(event).await;
        }
    }
}

/// Best-effort load of the state events a session-owning loop should replay.
///
/// Resolves the transcript path exactly like [`StateEventEmitter::new`] and
/// extracts the active-branch events. Any failure (missing transcript, unread-
/// able file, parse error) returns an empty list — the caller falls back to
/// the pre-event `from_messages` path, so this never alters loop behavior.
///
/// Uses the streaming state-event loader rather than a full-transcript parse:
/// interactive mode calls this once per prompt, so the cost must track the
/// number of events, not the size of the conversation.
pub(crate) async fn load_session_state_events(
    working_dir: &std::path::Path,
    session_id: &str,
) -> Vec<clawde_core::session_storage::StateEvent> {
    let project_root = clawde_core::git_utils::project_root(working_dir);
    let Ok(path) = clawde_core::session_storage::transcript_path(&project_root, session_id) else {
        return Vec::new();
    };
    clawde_core::session_storage::load_state_events_from_file(&path)
        .await
        .unwrap_or_default()
}

/// Best-effort snapshot-aware load: returns the newest valid state snapshot
/// plus the events written after it. `None` means no usable snapshot (never
/// crossed the cadence, leaf present, stale schema, or watermark mismatch) —
/// the caller then falls back to the full event load. Any failure degrades to
/// `None`; this never alters loop behavior.
pub(crate) async fn load_session_state_snapshot(
    working_dir: &std::path::Path,
    session_id: &str,
) -> Option<(clawde_core::session_storage::StateSnapshot, Vec<StateEvent>)> {
    let project_root = clawde_core::git_utils::project_root(working_dir);
    let Ok(path) = clawde_core::session_storage::transcript_path(&project_root, session_id) else {
        return None;
    };
    clawde_core::session_storage::load_state_snapshot(&path)
        .await
        .unwrap_or_default()
}

/// Serialize a live `TaskState` into the core-owned snapshot body (the
/// event-derived half — see [`StateSnapshotBody`] docs for what is excluded).
pub(crate) fn build_state_snapshot_body(state: &crate::task_state::TaskState) -> StateSnapshotBody {
    StateSnapshotBody {
        decisions: state
            .decisions
            .iter()
            .map(|decision| StateSnapshotDecision {
                statement: decision.statement.clone(),
                evidence: decision.evidence.clone(),
            })
            .collect(),
        evidence: state
            .evidence
            .iter()
            .map(|item| StateSnapshotEvidence {
                summary: item.summary.clone(),
                source: item.source.as_str().to_string(),
                status: item.status.as_str().to_string(),
            })
            .collect(),
        changed_files: state
            .changed_files
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        failures: state
            .failures
            .iter()
            .map(|failure| StateSnapshotFailure {
                source: failure.source.clone(),
                summary: failure.summary.clone(),
            })
            .collect(),
        simplification_reviewed: state.simplification_reviewed,
        files_touched: state.complexity.files_touched as u64,
        tool_calls: state.complexity.tool_calls as u64,
        failed_tools: state.complexity.failed_tools as u64,
        repeated_failures_per_target: state.complexity.repeated_failures_per_target as u64,
        plan_step: state.runtime.plan_step.clone(),
        validation: state.runtime.validation.clone(),
        snapshot_files: state
            .runtime
            .snapshot_files
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
    }
}

/// Build `ToolObserved` events for a completed tool batch.
///
/// One event per tool result — including successes — because the tool-call
/// counter on replay counts every observation. `file_paths` and `mutating`
/// are derived from the originating tool call (matched by id) so the replayed
/// reducer can update changed-file tracking identically to the live loop.
pub(crate) fn tool_observed_events(
    tool_calls: &[(String, String, Value)],
    tool_results: &[ContentBlock],
) -> Vec<StateEvent> {
    tool_results
        .iter()
        .filter_map(|block| {
            let ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } = block
            else {
                return None;
            };
            let failed = is_error.unwrap_or(false);
            let call = tool_calls.iter().find(|(id, _, _)| id == tool_use_id);
            let file_paths: Vec<String> = call
                .and_then(|(_, _, input)| {
                    input
                        .get("file_path")
                        .or_else(|| input.get("path"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .into_iter()
                .collect();
            let mutating = call
                .map(|(_, name, _)| clawde_core::constants::is_file_mutator(name))
                .unwrap_or(false);
            let summary = if failed {
                truncate(&tool_result_summary(content))
            } else {
                String::new()
            };
            Some(StateEvent::ToolObserved {
                failed,
                summary,
                file_paths,
                mutating,
            })
        })
        .collect()
}

/// Bounded text extraction from a tool result, mirroring the reducer's
/// failure summaries (text content verbatim, structured content as a note).
fn tool_result_summary(content: &ToolResultContent) -> String {
    match content {
        ToolResultContent::Text(text) => text.clone(),
        ToolResultContent::Blocks(_) => "tool returned structured content".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawde_core::types::ToolResultContent;

    fn call(id: &str, name: &str, input: Value) -> (String, String, Value) {
        (id.to_string(), name.to_string(), input)
    }

    fn result(id: &str, text: &str, is_error: Option<bool>) -> ContentBlock {
        ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content: ToolResultContent::Text(text.to_string()),
            is_error,
        }
    }

    #[test]
    fn tool_observed_events_cover_success_and_failure_with_call_metadata() {
        let calls = vec![
            call("1", "Write", serde_json::json!({"file_path": "src/lib.rs"})),
            call("2", "Read", serde_json::json!({"path": "docs/x.md"})),
        ];
        let results = vec![
            result("1", "", Some(false)),
            result("2", "file not found", Some(true)),
        ];
        let events = tool_observed_events(&calls, &results);
        assert_eq!(events.len(), 2);
        let StateEvent::ToolObserved {
            failed,
            summary,
            file_paths,
            mutating,
        } = &events[0]
        else {
            panic!("expected ToolObserved");
        };
        assert!(!failed);
        assert!(summary.is_empty());
        assert_eq!(file_paths, &["src/lib.rs".to_string()]);
        assert!(mutating);
        let StateEvent::ToolObserved {
            failed,
            summary,
            file_paths,
            mutating,
        } = &events[1]
        else {
            panic!("expected ToolObserved");
        };
        assert!(failed);
        assert_eq!(summary, "file not found");
        assert_eq!(file_paths, &["docs/x.md".to_string()]);
        assert!(!mutating);
    }

    #[test]
    fn tool_observed_events_skip_non_result_blocks_and_unknown_calls() {
        let calls = vec![];
        let results = vec![
            ContentBlock::Text {
                text: "hi".to_string(),
            },
            result("ghost", "boom", Some(true)),
        ];
        let events = tool_observed_events(&calls, &results);
        assert_eq!(events.len(), 1);
        let StateEvent::ToolObserved {
            failed,
            file_paths,
            mutating,
            ..
        } = &events[0]
        else {
            panic!("expected ToolObserved");
        };
        assert!(failed);
        assert!(file_paths.is_empty());
        assert!(!mutating);
    }

    fn empty_snapshot_body() -> StateSnapshotBody {
        StateSnapshotBody {
            decisions: Vec::new(),
            evidence: Vec::new(),
            changed_files: Vec::new(),
            failures: Vec::new(),
            simplification_reviewed: false,
            files_touched: 0,
            tool_calls: 0,
            failed_tools: 0,
            repeated_failures_per_target: 0,
            plan_step: None,
            validation: None,
            snapshot_files: Vec::new(),
        }
    }

    #[tokio::test]
    async fn emitter_counter_tracks_writes_and_snapshot_loads_back() {
        use clawde_core::session_storage::{
            load_state_snapshot, StateSnapshot, STATE_SNAPSHOT_SCHEMA_VERSION,
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut emitter = StateEventEmitter {
            path: Some(path.clone()),
            session_id: "sess".to_string(),
            last_plan_step: None,
            last_snapshot_files: None,
            total_events: 0,
            current_msg_index: None,
        };
        emitter
            .record_validation(StateValidationVerdict::Passed, "ok".to_string())
            .await;
        emitter.record_simplification_reviewed().await;
        assert_eq!(emitter.total_events(), 2, "two writes");

        // Write a snapshot at the session-global watermark and reload it. The
        // watermark must equal the events actually in the file (the loader
        // validates it), which it does here.
        emitter
            .record_state_snapshot(StateSnapshot {
                schema_version: STATE_SNAPSHOT_SCHEMA_VERSION,
                event_count: emitter.total_events(),
                body: empty_snapshot_body(),
            })
            .await;
        let loaded = load_state_snapshot(&path).await.unwrap().expect("snapshot");
        assert_eq!(loaded.0.event_count, 2);
        assert!(loaded.1.is_empty(), "no events after the snapshot");

        // A later event becomes the incremental tail on the next load.
        emitter
            .record_validation(StateValidationVerdict::Failed, "boom".to_string())
            .await;
        assert_eq!(emitter.total_events(), 3);
        let loaded = load_state_snapshot(&path).await.unwrap().expect("snapshot");
        assert_eq!(loaded.0.event_count, 2);
        assert_eq!(loaded.1.len(), 1, "only the post-snapshot event replays");
    }

    #[tokio::test]
    async fn emitter_stamps_message_index_onto_written_events() {
        use clawde_core::session_storage::load_state_events_from_file;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut emitter = StateEventEmitter {
            path: Some(path.clone()),
            session_id: "sess".to_string(),
            last_plan_step: None,
            last_snapshot_files: None,
            total_events: 0,
            current_msg_index: None,
        };
        // Pre-turn emission: no anchor yet.
        emitter
            .record_validation(StateValidationVerdict::Passed, "early".to_string())
            .await;
        emitter.set_message_index(4);
        emitter
            .record_validation(StateValidationVerdict::Failed, "boom".to_string())
            .await;
        let raw = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(!lines[0].contains("msgIndex"), "pre-turn event unanchored");
        assert!(lines[1].contains("\"msgIndex\":4"), "turn event anchored");

        // And the file round-trips: the anchored event's index is preserved
        // through the streaming loader's wire format.
        let events = load_state_events_from_file(&path).await.unwrap();
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn plan_step_events_deduplicate_until_changed() {
        let mut emitter = StateEventEmitter {
            path: None,
            session_id: "s".to_string(),
            last_plan_step: None,
            last_snapshot_files: None,
            total_events: 0,
            current_msg_index: None,
        };
        assert!(emitter
            .plan_step_event(Some("step 1".to_string()))
            .is_some());
        assert!(emitter
            .plan_step_event(Some("step 1".to_string()))
            .is_none());
        assert!(emitter
            .plan_step_event(Some("step 2".to_string()))
            .is_some());
        // Unset plan (None) is never emitted; it would erase progress.
        assert!(emitter.plan_step_event(None).is_none());
    }

    #[test]
    fn snapshot_events_deduplicate_by_set_and_skip_empty() {
        let mut emitter = StateEventEmitter {
            path: None,
            session_id: "s".to_string(),
            last_plan_step: None,
            last_snapshot_files: None,
            total_events: 0,
            current_msg_index: None,
        };
        assert!(emitter.snapshot_event(vec![]).is_none());
        let first = vec![PathBuf::from("src/a.rs"), PathBuf::from("src/b.rs")];
        assert!(emitter.snapshot_event(first).is_some());
        // Same set in a different order is still a no-op.
        assert!(emitter
            .snapshot_event(vec![PathBuf::from("src/b.rs"), PathBuf::from("src/a.rs")])
            .is_none());
        // A changed set emits again.
        assert!(emitter
            .snapshot_event(vec![PathBuf::from("src/a.rs")])
            .is_some());
    }
}
