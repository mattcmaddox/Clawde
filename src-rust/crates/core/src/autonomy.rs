//! Session-scoped autonomy state: the autopilot policy and its deferred queue.
//!
//! Autopilot (spec `docs/plans/bypass-autopilot-safety-spec.md` Phase 4B) is a
//! separate, explicitly activated execution posture. When active, tools that
//! need user review are deferred into a bounded in-memory queue and the agent
//! loop continues with safe work instead of blocking on a dialog.
//!
//! Safety rules enforced here:
//! - The queue is bounded; overflow denies rather than silently dropping.
//! - Every item is stamped with its session id, so a session mismatch makes
//!   the whole state inert (`is_active` is false).
//! - Items are never executed here — replay (Phase 4D) is a separate, later
//!   authorization decision.
//! - Items are never executed here — replay (Phase 4D) is a separate, later
//!   authorization decision.
//! - Raw secrets are not captured; the queue stores the typed request so a
//!   review surface can display a redacted summary.
//! - Optional disk persistence (Phase 4E) is a restart-recovery snapshot only:
//!   restored items are downgraded to `Stale` (review-only) and must be
//!   re-approved before any execution. Autopilot itself never auto-reactivates
//!   on restart, and an approval never survives a restart.

use crate::action_risk::ActionRisk;
use crate::permissions::PermissionRequest;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::PathBuf;

/// Default maximum number of deferred items held per session.
pub const DEFAULT_QUEUE_CAPACITY: usize = 64;

/// How long a deferred item stays approvable before it expires (24 hours).
/// Expiry is checked at approval/execution time, so a stale approval can
/// never be replayed later.
pub const DEFAULT_ITEM_TTL_SECS: i64 = 86_400;

/// On-disk format version for the persisted queue. Bump when the serialized
/// shape changes; older/newer files are ignored rather than misread.
pub const PERSIST_VERSION: u32 = 1;

/// The active autonomy posture for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AutonomyMode {
    /// Normal blocking approval behavior; no deferral.
    #[default]
    Off,
    /// Unattended execution: safe actions run, review-required actions are
    /// deferred, irreversible actions are denied.
    Autopilot,
}

/// What a deferred item represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeferredKind {
    /// A permission-gated tool call that needs user review before execution.
    ToolCall,
    /// A question the agent asked via `AskUserQuestion`; answerable later.
    UserQuestion,
}

/// Lifecycle state of a deferred item. Phase 4B only produces `Pending`;
/// the review/replay phases set the rest: `Approved` is set by
/// `/autopilot approve` (Phase 4D) and consumed to `Completed` when the
/// matching tool call is retried through the central dispatcher. `Stale` is
/// the post-restart state: restored items are review-only until re-approved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum DeferredState {
    #[default]
    Pending,
    Approved,
    Rejected,
    Stale,
    Expired,
    Completed,
}

/// The typed payload of a deferred item. Deliberately not erased: replay must
/// retain the typed tool boundary, and a question is never an executable call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DeferredPayload {
    ToolCall {
        tool_name: String,
        request: PermissionRequest,
    },
    UserQuestion {
        question: String,
        options: Option<Vec<String>>,
    },
}

/// One deferred action or question awaiting user review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeferredItem {
    /// Stable, model-visible id, e.g. `AP-001`.
    pub id: String,
    /// Session the item belongs to. Items from another session are inert.
    pub session_id: String,
    /// Unix seconds at creation.
    pub created_at_unix: i64,
    /// Unix seconds at which the item stops being approvable.
    pub expires_at_unix: i64,
    /// Project root captured at deferral time, for later identity validation.
    pub project_root: String,
    pub kind: DeferredKind,
    pub risk: ActionRisk,
    /// Why the item was deferred (bounded, user-facing text).
    pub reason: String,
    pub state: DeferredState,
    pub payload: DeferredPayload,
}

impl DeferredItem {
    /// True when the item's approval window has elapsed.
    pub fn is_expired(&self, now_unix: i64) -> bool {
        now_unix > self.expires_at_unix
    }

    /// The typed tool name for a `ToolCall` item, if any.
    pub fn tool_name(&self) -> Option<&str> {
        match &self.payload {
            DeferredPayload::ToolCall { tool_name, .. } => Some(tool_name.as_str()),
            DeferredPayload::UserQuestion { .. } => None,
        }
    }
}

/// On-disk snapshot of a session's deferred queue (Phase 4E).
///
/// Deliberately does NOT store `mode`: autopilot must be explicitly
/// re-activated each session, and a restart must never silently reactivate it.
/// `next_id` is recomputed from the items on load.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedAutonomy {
    pub version: u32,
    pub session_id: String,
    pub items: Vec<DeferredItem>,
}

/// Session-scoped runtime state for the autopilot policy.
#[derive(Debug)]
pub struct AutonomyState {
    pub mode: AutonomyMode,
    /// Session the state is currently bound to. Changing the session id
    /// (directly or via [`Self::start_autopilot`]) makes stale items inert.
    pub session_id: String,
    pub items: VecDeque<DeferredItem>,
    pub next_id: u64,
    pub capacity: usize,
    /// Directory that holds this session's persisted queue snapshot. `None`
    /// disables persistence entirely (tests, headless harnesses) — fail
    /// closed by default.
    base_dir: Option<PathBuf>,
    /// Test-only clock override. `None` in production (wall clock).
    #[cfg(test)]
    pub now_override: Option<i64>,
}

impl AutonomyState {
    pub fn new(session_id: &str) -> Self {
        Self {
            mode: AutonomyMode::Off,
            session_id: session_id.to_string(),
            items: VecDeque::new(),
            next_id: 1,
            capacity: DEFAULT_QUEUE_CAPACITY,
            base_dir: None,
            #[cfg(test)]
            now_override: None,
        }
    }

    /// Enable disk persistence under `base_dir`. The per-session file is
    /// derived from the session id, so a session rebind automatically targets
    /// the right file. Call once at state creation.
    pub fn set_persistence_dir(&mut self, base_dir: PathBuf) {
        self.base_dir = Some(base_dir);
    }

    /// Path of this session's persisted snapshot, if persistence is enabled.
    fn persist_path(&self) -> Option<PathBuf> {
        self.base_dir.as_ref().map(|dir| {
            let safe: String = self
                .session_id
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                        c
                    } else {
                        '_'
                    }
                })
                .take(64)
                .collect();
            dir.join(format!("autonomy-{safe}.json"))
        })
    }

    /// Write the current queue to disk (best-effort). Persistence is a
    /// restart-recovery snapshot, not a safety boundary: an IO failure warns
    /// and the in-memory queue stays authoritative for the current run.
    fn persist(&self) {
        let Some(path) = self.persist_path() else {
            return;
        };
        let snapshot = PersistedAutonomy {
            version: PERSIST_VERSION,
            session_id: self.session_id.clone(),
            items: self.items.iter().cloned().collect(),
        };
        let json = match serde_json::to_string_pretty(&snapshot) {
            Ok(j) => j,
            Err(_) => return,
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
            crate::accounts::set_user_only_dir_perms(parent);
        }
        let tmp = path.with_file_name(format!(
            ".{}.clawde-tmp-{}",
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "autonomy".to_string()),
            std::process::id()
        ));
        if std::fs::write(&tmp, &json).is_ok() {
            crate::accounts::set_user_only_perms(&tmp);
            if std::fs::rename(&tmp, &path).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }

    /// Restore this session's previously persisted queue (Phase 4E).
    ///
    /// Restart semantics — the queue is a REVIEW-ONLY recovery surface:
    /// - `Approved` and `Pending` items are downgraded to `Stale`; an approval
    ///   never survives a restart and nothing executes without a fresh
    ///   `/autopilot approve` (which revalidates tool existence and risk).
    /// - Items past their TTL are marked `Expired`.
    /// - `Rejected` / `Completed` / `Expired` history is preserved as-is.
    /// - A missing, corrupt, version-mismatched, or other-session file is
    ///   ignored (a warning is logged, nothing is loaded).
    /// - `next_id` is bumped past the restored ids so new ids never collide.
    ///
    /// Returns the number of items restored. Only loads into an empty queue.
    pub fn load_persisted(&mut self) -> usize {
        if !self.items.is_empty() {
            return 0;
        }
        let Some(path) = self.persist_path() else {
            return 0;
        };
        let Ok(bytes) = std::fs::read(&path) else {
            return 0;
        };
        let Ok(snapshot) = serde_json::from_slice::<PersistedAutonomy>(&bytes) else {
            tracing::warn!(
                "ignoring unreadable autopilot queue snapshot at {} (corrupt or old format)",
                path.display()
            );
            return 0;
        };
        if snapshot.version != PERSIST_VERSION || snapshot.session_id != self.session_id {
            return 0;
        }
        let now = self.now();
        let mut max_id = 0u64;
        for item in snapshot.items {
            if let Some(n) = item
                .id
                .strip_prefix("AP-")
                .and_then(|s| s.parse::<u64>().ok())
            {
                max_id = max_id.max(n);
            }
            let mut item = item;
            if item.is_expired(now) {
                item.state = DeferredState::Expired;
            } else if matches!(item.state, DeferredState::Pending | DeferredState::Approved) {
                item.state = DeferredState::Stale;
            }
            self.items.push_back(item);
        }
        self.next_id = max_id + 1;
        self.items.len()
    }

    /// True when autopilot is on AND the state is bound to `session_id`.
    /// Session mismatch fails closed: stale state can never defer actions.
    pub fn is_active(&self, session_id: &str) -> bool {
        self.mode == AutonomyMode::Autopilot && self.session_id == session_id
    }

    /// Activate autopilot for `session_id`. If the state was bound to a
    /// different session, the old queue is discarded rather than mixed in.
    pub fn start_autopilot(&mut self, session_id: &str) {
        if self.session_id != session_id {
            self.reset();
            self.session_id = session_id.to_string();
        }
        self.mode = AutonomyMode::Autopilot;
    }

    /// Deactivate autopilot. Pending items remain (visible for review) but
    /// no new items can be enqueued while inactive.
    pub fn stop_autopilot(&mut self) {
        self.mode = AutonomyMode::Off;
    }

    /// Number of actionable (unexpired `Pending` or `Stale`) items in the
    /// queue. Expired items are counted as gone so the badge/status reflects
    /// what is still actionable.
    pub fn pending_count(&self) -> usize {
        let now = self.now();
        self.items
            .iter()
            .filter(|item| {
                matches!(item.state, DeferredState::Pending | DeferredState::Stale)
                    && !item.is_expired(now)
            })
            .count()
    }

    /// Clear all runtime state (mode, queue, id counter). Called on `/new`.
    pub fn reset(&mut self) {
        self.mode = AutonomyMode::Off;
        self.items.clear();
        self.next_id = 1;
    }

    fn alloc_id(&mut self) -> String {
        let id = format!("AP-{:03}", self.next_id);
        self.next_id += 1;
        id
    }

    /// Current unix time; overridable in tests via [`Self::set_now`].
    pub fn now(&self) -> i64 {
        #[cfg(test)]
        {
            if let Some(t) = self.now_override {
                return t;
            }
        }
        chrono::Utc::now().timestamp()
    }

    /// Remove actionable (`Pending` or `Stale`) items whose TTL has elapsed.
    /// Expired items are dropped from the queue entirely — they are no longer
    /// actionable and must not occupy capacity. Returns the number removed.
    pub fn expire_stale_items(&mut self) -> usize {
        let now = self.now();
        let before = self.items.len();
        self.items.retain(|item| {
            !(matches!(item.state, DeferredState::Pending | DeferredState::Stale)
                && item.is_expired(now))
        });
        let count = before - self.items.len();
        if count > 0 {
            self.persist();
        }
        count
    }

    /// Number of items that occupy queue capacity: actionable (`Pending` /
    /// `Stale`) plus approved-but-not-yet-consumed. Rejected, completed, and
    /// expired history does not count toward the bound.
    fn capacity_used(&self) -> usize {
        self.items
            .iter()
            .filter(|item| {
                matches!(
                    item.state,
                    DeferredState::Pending | DeferredState::Stale | DeferredState::Approved
                )
            })
            .count()
    }

    /// Enqueue a permission-gated tool call for later review. Returns `None`
    /// when the queue is full (caller must deny, never drop silently).
    ///
    /// Expired items are swept first so dead entries cannot occupy capacity.
    /// If an identical actionable tool call is already queued for this session
    /// (same tool name, details and path), that item is returned instead of
    /// creating a duplicate — a retry-storming model gets the same stable id
    /// back and cannot fill the queue with copies of one action.
    pub fn enqueue_tool_call(
        &mut self,
        session_id: &str,
        project_root: &str,
        tool_name: &str,
        request: PermissionRequest,
        risk: ActionRisk,
        reason: String,
    ) -> Option<DeferredItem> {
        self.expire_stale_items();
        let now = self.now();
        if let Some(existing) = self.items.iter().find(|item| {
            item.session_id == session_id
                && matches!(item.state, DeferredState::Pending | DeferredState::Stale)
                && !item.is_expired(now)
                && item.kind == DeferredKind::ToolCall
                && matches!(&item.payload,
                    DeferredPayload::ToolCall { tool_name: tn, request: stored }
                    if tn.eq_ignore_ascii_case(tool_name)
                        && stored.details.as_deref() == request.details.as_deref()
                        && stored.path.as_deref() == request.path.as_deref())
        }) {
            return Some(existing.clone());
        }
        if self.capacity_used() >= self.capacity {
            return None;
        }
        let created = self.now();
        let item = DeferredItem {
            id: self.alloc_id(),
            session_id: session_id.to_string(),
            created_at_unix: created,
            expires_at_unix: created + DEFAULT_ITEM_TTL_SECS,
            project_root: project_root.to_string(),
            kind: DeferredKind::ToolCall,
            risk,
            reason,
            state: DeferredState::Pending,
            payload: DeferredPayload::ToolCall {
                tool_name: tool_name.to_string(),
                request,
            },
        };
        self.items.push_back(item.clone());
        self.persist();
        Some(item)
    }

    /// Enqueue a user question for later answering. Returns `None` when full.
    pub fn enqueue_question(
        &mut self,
        session_id: &str,
        project_root: &str,
        question: String,
        options: Option<Vec<String>>,
    ) -> Option<DeferredItem> {
        self.expire_stale_items();
        if self.capacity_used() >= self.capacity {
            return None;
        }
        let created = self.now();
        let item = DeferredItem {
            id: self.alloc_id(),
            session_id: session_id.to_string(),
            created_at_unix: created,
            expires_at_unix: created + DEFAULT_ITEM_TTL_SECS,
            project_root: project_root.to_string(),
            kind: DeferredKind::UserQuestion,
            risk: ActionRisk::ReviewRequired,
            reason: "The agent asked the user a question while autopilot was active".to_string(),
            state: DeferredState::Pending,
            payload: DeferredPayload::UserQuestion { question, options },
        };
        self.items.push_back(item.clone());
        self.persist();
        Some(item)
    }

    /// Approve a deferred tool call for replay (Phase 4D). Accepts both
    /// `Pending` items (fresh deferral) and `Stale` items (restored after a
    /// restart — approving them is the explicit revalidation step).
    ///
    /// Validation performed here (all must pass or the item is left untouched
    /// and an error is returned):
    /// - the item exists, belongs to `session_id`, and is `Pending`/`Stale`;
    /// - it is a `ToolCall` (questions are answered, not approved);
    /// - it has not expired;
    /// - `validate` (the tool-existence + risk re-check, supplied by the
    ///   command layer so `clawde-core` stays free of tool registry deps)
    ///   returns `Ok`.
    ///
    /// On success the item is marked `Approved`. The approval is consumed by
    /// [`Self::take_approved_match`] the first time a matching tool request
    /// flows through the permission backstop — one approval, one execution.
    pub fn approve_item(
        &mut self,
        session_id: &str,
        id: &str,
        validate: impl FnOnce(&DeferredItem) -> Result<(), String>,
    ) -> Result<(), String> {
        let now = self.now();
        let Some(item) = self
            .items
            .iter_mut()
            .find(|item| item.id == id && item.session_id == session_id)
        else {
            return Err(format!("No item with id {id} in this session."));
        };
        if !matches!(item.state, DeferredState::Pending | DeferredState::Stale) {
            return Err(format!("{} is not pending (state: {:?}).", id, item.state));
        }
        if item.kind != DeferredKind::ToolCall {
            return Err(format!(
                "{} is a deferred question, not a tool call. Use /autopilot answer {} <text> to \
                 answer it.",
                id, id
            ));
        }
        if item.is_expired(now) {
            item.state = DeferredState::Expired;
            self.persist();
            return Err(format!(
                "{} expired; the action must be re-requested and deferred again before it can be \
                 approved.",
                id
            ));
        }
        validate(item)?;
        item.state = DeferredState::Approved;
        self.persist();
        Ok(())
    }

    /// Reject a pending deferred item. The agent will never run it.
    pub fn reject_item(&mut self, session_id: &str, id: &str) -> Result<(), String> {
        let Some(item) = self
            .items
            .iter_mut()
            .find(|item| item.id == id && item.session_id == session_id)
        else {
            return Err(format!("No item with id {id} in this session."));
        };
        if !matches!(item.state, DeferredState::Pending | DeferredState::Stale) {
            return Err(format!("{} is not pending (state: {:?}).", id, item.state));
        }
        item.state = DeferredState::Rejected;
        self.persist();
        Ok(())
    }

    /// Complete a deferred question after the user answers it. Returns the
    /// question text so the caller can format the injected answer message.
    pub fn answer_question(&mut self, session_id: &str, id: &str) -> Result<String, String> {
        let now = self.now();
        let Some(item) = self
            .items
            .iter_mut()
            .find(|item| item.id == id && item.session_id == session_id)
        else {
            return Err(format!("No item with id {id} in this session."));
        };
        if matches!(item.state, DeferredState::Pending | DeferredState::Stale)
            && item.is_expired(now)
        {
            item.state = DeferredState::Expired;
            self.persist();
            return Err(format!(
                "{} expired; the question can no longer be answered.",
                id
            ));
        }
        if item.kind != DeferredKind::UserQuestion {
            return Err(format!(
                "{} is a deferred tool call, not a question. Use /autopilot reject {} to decline \
                 it; execution approval lands with /autopilot approve {}.",
                id, id, id
            ));
        }
        if !matches!(item.state, DeferredState::Pending | DeferredState::Stale) {
            return Err(format!("{} is not pending (state: {:?}).", id, item.state));
        }
        let question = match &item.payload {
            DeferredPayload::UserQuestion { question, .. } => question.clone(),
            _ => unreachable!("kind checked above"),
        };
        item.state = DeferredState::Completed;
        self.persist();
        Ok(question)
    }

    /// Consume an approval for a tool request flowing through the permission
    /// backstop.
    ///
    /// Finds the first `Approved` `ToolCall` item whose stored request matches
    /// `request` (same tool name case-insensitively, same details and path).
    /// On match the item transitions to `Completed` and its id is returned.
    /// This is the single execution grant: the tool call itself still runs
    /// through the normal typed executor, so the tool's own schema validation
    /// and side effects apply unchanged.
    pub fn take_approved_match(
        &mut self,
        session_id: &str,
        request: &PermissionRequest,
    ) -> Option<String> {
        let now = self.now();
        let found = self.items.iter().position(|item| {
            if item.session_id != session_id
                || item.state != DeferredState::Approved
                || item.kind != DeferredKind::ToolCall
            {
                return false;
            }
            let DeferredPayload::ToolCall {
                tool_name,
                request: stored,
                ..
            } = &item.payload
            else {
                return false;
            };
            if !tool_name.eq_ignore_ascii_case(&request.tool_name) {
                return false;
            }
            if item.is_expired(now) {
                return false;
            }
            // Exact-match the identifying fields so an approval cannot be
            // redirected to a different command/path. The working dir is part
            // of the identity: an approval captured in one workspace must not
            // authorize the same command in another.
            if stored.working_dir.as_ref() != request.working_dir.as_ref() {
                return false;
            }
            let details_match = stored.details.as_deref() == request.details.as_deref();
            let path_match = stored.path.as_deref() == request.path.as_deref();
            if stored.details.is_some()
                || stored.path.is_some()
                || request.details.is_some()
                || request.path.is_some()
            {
                // Either side carries a fingerprint: require exact details AND
                // path so an approval cannot be redirected to a different
                // command/path (a None on one side vs Some on the other fails).
                details_match && path_match
            } else {
                // No details/path on either side (e.g. stateful tools): fall
                // back to the description as a tiebreaker so an approval for
                // one invocation cannot authorize a different one.
                stored.description == request.description
            }
        });
        let idx = found?;
        let id = self.items[idx].id.clone();
        self.items[idx].state = DeferredState::Completed;
        self.persist();
        Some(id)
    }

    /// Test hook: fix the clock so expiry paths are deterministic.
    #[cfg(test)]
    pub fn set_now(&mut self, now_unix: i64) {
        self.now_override = Some(now_unix);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::PermissionLevel;

    fn sample_request() -> PermissionRequest {
        PermissionRequest {
            tool_name: "Bash".to_string(),
            description: "run a command".to_string(),
            details: Some("git push".to_string()),
            is_read_only: false,
            path: Some("git push".to_string()),
            working_dir: Some(std::path::PathBuf::from("/project")),
            allowed_roots: vec![std::path::PathBuf::from("/project")],
            context_description: None,
            network_isolated: false,
            permission_level: PermissionLevel::Execute,
            network_capable: false,
            stateful: false,
        }
    }

    #[test]
    fn new_state_is_off_and_inert() {
        let state = AutonomyState::new("s1");
        assert_eq!(state.mode, AutonomyMode::Off);
        assert!(!state.is_active("s1"));
        assert_eq!(state.pending_count(), 0);
    }

    #[test]
    fn autopilot_active_only_when_session_matches() {
        let mut state = AutonomyState::new("s1");
        state.start_autopilot("s1");
        assert!(state.is_active("s1"));
        // A different session must never see this state as active.
        assert!(!state.is_active("s2"));
    }

    #[test]
    fn start_autopilot_on_new_session_discards_old_queue() {
        let mut state = AutonomyState::new("s1");
        state.start_autopilot("s1");
        assert!(state
            .enqueue_tool_call(
                "s1",
                "/project",
                "Bash",
                sample_request(),
                ActionRisk::ReviewRequired,
                "needs review".to_string(),
            )
            .is_some());
        assert_eq!(state.pending_count(), 1);
        state.start_autopilot("s2");
        assert!(state.is_active("s2"));
        assert_eq!(
            state.pending_count(),
            0,
            "old session queue must not survive"
        );
    }

    #[test]
    fn queue_is_bounded_and_overflow_returns_none() {
        let mut state = AutonomyState {
            capacity: 2,
            ..AutonomyState::new("s1")
        };
        state.start_autopilot("s1");
        // Distinct requests (dedup would otherwise collapse identical ones).
        let mut r1 = sample_request();
        r1.details = Some("git push".to_string());
        r1.path = Some("git push".to_string());
        let mut r2 = sample_request();
        r2.details = Some("git commit".to_string());
        r2.path = Some("git commit".to_string());
        let mut r3 = sample_request();
        r3.details = Some("git pull".to_string());
        r3.path = Some("git pull".to_string());
        assert!(state
            .enqueue_tool_call(
                "s1",
                "/project",
                "Bash",
                r1,
                ActionRisk::ReviewRequired,
                "one".to_string(),
            )
            .is_some());
        assert!(state
            .enqueue_tool_call(
                "s1",
                "/project",
                "Bash",
                r2,
                ActionRisk::ReviewRequired,
                "two".to_string(),
            )
            .is_some());
        assert!(
            state
                .enqueue_tool_call(
                    "s1",
                    "/project",
                    "Bash",
                    r3,
                    ActionRisk::ReviewRequired,
                    "three".to_string(),
                )
                .is_none(),
            "overflow must deny, not drop silently"
        );
        assert_eq!(state.pending_count(), 2);
    }

    #[test]
    fn ids_are_stable_and_sequential() {
        let mut state = AutonomyState::new("s1");
        state.start_autopilot("s1");
        let a = state
            .enqueue_tool_call(
                "s1",
                "/project",
                "Bash",
                sample_request(),
                ActionRisk::ReviewRequired,
                "a".to_string(),
            )
            .unwrap();
        let b = state
            .enqueue_question("s1", "/project", "proceed?".to_string(), None)
            .unwrap();
        assert_eq!(a.id, "AP-001");
        assert_eq!(b.id, "AP-002");
        assert_eq!(a.kind, DeferredKind::ToolCall);
        assert_eq!(b.kind, DeferredKind::UserQuestion);
        assert_eq!(b.risk, ActionRisk::ReviewRequired);
    }

    #[test]
    fn reset_clears_everything() {
        let mut state = AutonomyState::new("s1");
        state.start_autopilot("s1");
        let _ = state.enqueue_question("s1", "/project", "q?".to_string(), None);
        state.reset();
        assert_eq!(state.mode, AutonomyMode::Off);
        assert_eq!(state.pending_count(), 0);
        assert_eq!(state.next_id, 1);
    }

    // ---- Phase 4D: approved replay -----------------------------------------

    #[test]
    fn approve_marks_pending_tool_call_approved() {
        let mut state = AutonomyState::new("s1");
        state.start_autopilot("s1");
        let _ = state.enqueue_tool_call(
            "s1",
            "/project",
            "Bash",
            sample_request(),
            ActionRisk::ReviewRequired,
            "needs review".to_string(),
        );
        state
            .approve_item("s1", "AP-001", |item| {
                assert_eq!(item.tool_name(), Some("Bash"));
                Ok(())
            })
            .unwrap();
        assert_eq!(state.items[0].state, DeferredState::Approved);
    }

    #[test]
    fn approve_rejects_questions_and_non_pending() {
        let mut state = AutonomyState::new("s1");
        state.start_autopilot("s1");
        let _ = state.enqueue_question("s1", "/project", "q?".to_string(), None);
        let _ = state.enqueue_tool_call(
            "s1",
            "/project",
            "Bash",
            sample_request(),
            ActionRisk::ReviewRequired,
            "needs review".to_string(),
        );
        assert!(state
            .approve_item("s1", "AP-001", |_| Ok(()))
            .unwrap_err()
            .contains("not a tool call"));
        assert!(state
            .approve_item("s1", "AP-002", |_| Err("tool missing".to_string()))
            .is_err());
        // Validation failure leaves the item Pending.
        assert_eq!(state.items[1].state, DeferredState::Pending);
    }

    #[test]
    fn approve_rejects_expired_items() {
        let mut state = AutonomyState::new("s1");
        state.start_autopilot("s1");
        state.set_now(1_000_000);
        let _ = state.enqueue_tool_call(
            "s1",
            "/project",
            "Bash",
            sample_request(),
            ActionRisk::ReviewRequired,
            "needs review".to_string(),
        );
        state.set_now(1_000_000 + DEFAULT_ITEM_TTL_SECS + 1);
        let err = state.approve_item("s1", "AP-001", |_| Ok(())).unwrap_err();
        assert!(err.contains("expired"), "{err}");
        assert_eq!(state.items[0].state, DeferredState::Expired);
    }

    #[test]
    fn take_approved_match_consumes_exact_request() {
        let mut state = AutonomyState::new("s1");
        state.start_autopilot("s1");
        let _ = state.enqueue_tool_call(
            "s1",
            "/project",
            "Bash",
            sample_request(),
            ActionRisk::ReviewRequired,
            "needs review".to_string(),
        );
        state.approve_item("s1", "AP-001", |_| Ok(())).unwrap();

        // Exact same request -> consumed, one execution grant.
        let id = state.take_approved_match("s1", &sample_request());
        assert_eq!(id.as_deref(), Some("AP-001"));
        assert_eq!(state.items[0].state, DeferredState::Completed);

        // Second retry -> no approval left, nothing granted.
        assert!(state.take_approved_match("s1", &sample_request()).is_none());
    }

    #[test]
    fn take_approved_match_rejects_changed_request() {
        let mut state = AutonomyState::new("s1");
        state.start_autopilot("s1");
        let _ = state.enqueue_tool_call(
            "s1",
            "/project",
            "Bash",
            sample_request(),
            ActionRisk::ReviewRequired,
            "needs review".to_string(),
        );
        state.approve_item("s1", "AP-001", |_| Ok(())).unwrap();

        // Changed command text must NOT consume the approval.
        let mut changed = sample_request();
        changed.details = Some("git commit".to_string());
        changed.path = Some("git commit".to_string());
        assert!(state.take_approved_match("s1", &changed).is_none());
        assert_eq!(state.items[0].state, DeferredState::Approved);
    }

    #[test]
    fn expired_approval_is_not_consumed() {
        let mut state = AutonomyState::new("s1");
        state.start_autopilot("s1");
        state.set_now(1_000_000);
        let _ = state.enqueue_tool_call(
            "s1",
            "/project",
            "Bash",
            sample_request(),
            ActionRisk::ReviewRequired,
            "needs review".to_string(),
        );
        state.approve_item("s1", "AP-001", |_| Ok(())).unwrap();
        state.set_now(1_000_000 + DEFAULT_ITEM_TTL_SECS + 1);
        assert!(state.take_approved_match("s1", &sample_request()).is_none());
        assert_eq!(state.items[0].state, DeferredState::Approved);
    }

    #[test]
    fn expire_stale_items_marks_old_pending() {
        let mut state = AutonomyState::new("s1");
        state.start_autopilot("s1");
        state.set_now(1_000_000);
        let _ = state.enqueue_tool_call(
            "s1",
            "/project",
            "Bash",
            sample_request(),
            ActionRisk::ReviewRequired,
            "one".to_string(),
        );
        let _ = state.enqueue_question("s1", "/project", "q?".to_string(), None);
        state.set_now(1_000_000 + DEFAULT_ITEM_TTL_SECS + 1);
        assert_eq!(state.expire_stale_items(), 2);
        assert_eq!(state.pending_count(), 0);
    }

    // ---- Phase 4E: persistence & restart recovery --------------------------

    static TEST_DIR_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// A unique temp dir per test (tests run in parallel within the crate).
    fn test_dir() -> std::path::PathBuf {
        let n = TEST_DIR_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("clawde-autonomy-test-{}-{}", std::process::id(), n))
    }

    /// A fresh state bound to `session` with persistence enabled under `dir`.
    fn persisted_state(session: &str, dir: &std::path::Path) -> AutonomyState {
        let mut state = AutonomyState::new(session);
        state.set_persistence_dir(dir.to_path_buf());
        state
    }

    #[test]
    fn persist_writes_snapshot_file() {
        let tmp = test_dir();
        let mut state = persisted_state("s1", &tmp);
        state.start_autopilot("s1");
        let _ = state.enqueue_tool_call(
            "s1",
            "/project",
            "Bash",
            sample_request(),
            ActionRisk::ReviewRequired,
            "needs review".to_string(),
        );
        assert!(
            tmp.join("autonomy-s1.json").exists(),
            "snapshot must be written on enqueue"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn load_restores_items_as_stale_and_blocks_replay() {
        let tmp = test_dir();
        {
            let mut state = persisted_state("s1", &tmp);
            state.start_autopilot("s1");
            let _ = state.enqueue_tool_call(
                "s1",
                "/project",
                "Bash",
                sample_request(),
                ActionRisk::ReviewRequired,
                "needs review".to_string(),
            );
            let _ = state.enqueue_question("s1", "/project", "q?".to_string(), None);
            // Approve the first item before "restart".
            state.approve_item("s1", "AP-001", |_| Ok(())).unwrap();
        }

        // Simulate a restart: a fresh state bound to the same session.
        let mut state = persisted_state("s1", &tmp);
        assert_eq!(state.load_persisted(), 2);
        assert_eq!(state.pending_count(), 2, "restored items are actionable");
        // Approved downgraded to Stale; Pending also Stale (review-only).
        assert_eq!(state.items[0].state, DeferredState::Stale);
        assert_eq!(state.items[1].state, DeferredState::Stale);
        // No approval survives a restart: nothing can be consumed.
        assert!(state.take_approved_match("s1", &sample_request()).is_none());
        // next_id bumped past restored ids.
        let _ = state.enqueue_question("s1", "/project", "new?".to_string(), None);
        assert_eq!(state.items[2].id, "AP-003");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn restored_stale_item_can_be_reapproved() {
        let tmp = test_dir();
        {
            let mut state = persisted_state("s1", &tmp);
            state.start_autopilot("s1");
            let _ = state.enqueue_tool_call(
                "s1",
                "/project",
                "Bash",
                sample_request(),
                ActionRisk::ReviewRequired,
                "needs review".to_string(),
            );
            state.approve_item("s1", "AP-001", |_| Ok(())).unwrap();
        }
        let mut state = persisted_state("s1", &tmp);
        state.load_persisted();
        // Re-approval revalidates the restored (Stale) item.
        state.approve_item("s1", "AP-001", |_| Ok(())).unwrap();
        assert_eq!(state.items[0].state, DeferredState::Approved);
        // Now the approval is live and consumes on the exact retry.
        assert_eq!(
            state
                .take_approved_match("s1", &sample_request())
                .as_deref(),
            Some("AP-001")
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn load_ignores_other_session_and_corrupt_files() {
        let tmp = test_dir();
        {
            let mut state = persisted_state("s1", &tmp);
            state.start_autopilot("s1");
            let _ = state.enqueue_tool_call(
                "s1",
                "/project",
                "Bash",
                sample_request(),
                ActionRisk::ReviewRequired,
                "needs review".to_string(),
            );
        }
        // A state bound to another session must not load s1's file.
        let mut other = persisted_state("s2", &tmp);
        assert_eq!(other.load_persisted(), 0);
        assert!(other.items.is_empty());
        // Corrupt the file; load must not panic and must restore nothing.
        std::fs::write(tmp.join("autonomy-s2.json"), "{ not json ").unwrap();
        assert_eq!(other.load_persisted(), 0);
        assert!(other.items.is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn load_sweeps_expired_items() {
        let tmp = test_dir();
        {
            let mut state = persisted_state("s1", &tmp);
            state.start_autopilot("s1");
            state.set_now(1_000_000);
            let _ = state.enqueue_tool_call(
                "s1",
                "/project",
                "Bash",
                sample_request(),
                ActionRisk::ReviewRequired,
                "needs review".to_string(),
            );
        }
        let mut state = persisted_state("s1", &tmp);
        state.set_now(1_000_000 + DEFAULT_ITEM_TTL_SECS + 1);
        state.load_persisted();
        assert_eq!(state.items.len(), 1);
        assert_eq!(state.items[0].state, DeferredState::Expired);
        assert_eq!(state.pending_count(), 0);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ---- Phase 4 audit fixes ----------------------------------------------

    #[test]
    fn expired_items_are_swept_before_enqueue() {
        let mut state = AutonomyState::new("s1");
        state.start_autopilot("s1");
        state.set_now(1_000_000);
        // Fill the queue to capacity so only a sweep can free space.
        state.capacity = 1;
        let _ = state.enqueue_tool_call(
            "s1",
            "/project",
            "Bash",
            sample_request(),
            ActionRisk::ReviewRequired,
            "one".to_string(),
        );
        // Item expires; a new enqueue must sweep it and succeed.
        state.set_now(1_000_000 + DEFAULT_ITEM_TTL_SECS + 1);
        let item = state.enqueue_tool_call(
            "s1",
            "/project",
            "Bash",
            sample_request(),
            ActionRisk::ReviewRequired,
            "two".to_string(),
        );
        assert!(
            item.is_some(),
            "expired item must be swept so capacity frees up"
        );
        assert_eq!(state.pending_count(), 1);
        assert_eq!(
            state.items.len(),
            1,
            "expired item is removed, not retained"
        );
    }

    #[test]
    fn identical_tool_call_is_deduplicated() {
        let mut state = AutonomyState::new("s1");
        state.start_autopilot("s1");
        let first = state
            .enqueue_tool_call(
                "s1",
                "/project",
                "Bash",
                sample_request(),
                ActionRisk::ReviewRequired,
                "needs review".to_string(),
            )
            .unwrap();
        // Retry-storm: the exact same request returns the SAME id, no dup.
        let second = state
            .enqueue_tool_call(
                "s1",
                "/project",
                "Bash",
                sample_request(),
                ActionRisk::ReviewRequired,
                "needs review".to_string(),
            )
            .unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(state.items.len(), 1);
        assert_eq!(state.pending_count(), 1);
        // A different command is NOT deduped.
        let mut other = sample_request();
        other.details = Some("git commit".to_string());
        other.path = Some("git commit".to_string());
        let third = state
            .enqueue_tool_call(
                "s1",
                "/project",
                "Bash",
                other,
                ActionRisk::ReviewRequired,
                "needs review".to_string(),
            )
            .unwrap();
        assert_ne!(first.id, third.id);
        assert_eq!(state.items.len(), 2);
    }

    #[test]
    fn approved_match_requires_same_working_dir() {
        let mut state = AutonomyState::new("s1");
        state.start_autopilot("s1");
        let _ = state.enqueue_tool_call(
            "s1",
            "/project",
            "Bash",
            sample_request(),
            ActionRisk::ReviewRequired,
            "needs review".to_string(),
        );
        state.approve_item("s1", "AP-001", |_| Ok(())).unwrap();
        // Same command but a different working dir must NOT consume.
        let mut moved = sample_request();
        moved.working_dir = Some(std::path::PathBuf::from("/elsewhere"));
        assert!(state.take_approved_match("s1", &moved).is_none());
        assert_eq!(state.items[0].state, DeferredState::Approved);
    }

    #[test]
    fn approved_match_uses_description_tiebreaker_when_no_fingerprint() {
        let mut state = AutonomyState::new("s1");
        state.start_autopilot("s1");
        let mut req = sample_request();
        req.details = None;
        req.path = None;
        req.description = "send message".to_string();
        let _ = state.enqueue_tool_call(
            "s1",
            "/project",
            "SendMessage",
            req,
            ActionRisk::ReviewRequired,
            "needs review".to_string(),
        );
        state.approve_item("s1", "AP-001", |_| Ok(())).unwrap();
        // Same description -> consumes.
        let mut same = sample_request();
        same.tool_name = "SendMessage".to_string();
        same.details = None;
        same.path = None;
        same.description = "send message".to_string();
        assert_eq!(
            state.take_approved_match("s1", &same).as_deref(),
            Some("AP-001")
        );
    }

    #[test]
    fn approved_match_description_tiebreaker_rejects_different_description() {
        let mut state = AutonomyState::new("s1");
        state.start_autopilot("s1");
        let mut req = sample_request();
        req.details = None;
        req.path = None;
        req.description = "send message".to_string();
        let _ = state.enqueue_tool_call(
            "s1",
            "/project",
            "SendMessage",
            req,
            ActionRisk::ReviewRequired,
            "needs review".to_string(),
        );
        state.approve_item("s1", "AP-001", |_| Ok(())).unwrap();
        // Different description must NOT consume the approval.
        let mut other = sample_request();
        other.tool_name = "SendMessage".to_string();
        other.details = None;
        other.path = None;
        other.description = "send a different message".to_string();
        assert!(state.take_approved_match("s1", &other).is_none());
        assert_eq!(state.items[0].state, DeferredState::Approved);
    }

    #[test]
    fn reject_and_answer_persist() {
        let tmp = test_dir();
        {
            let mut state = persisted_state("s1", &tmp);
            state.start_autopilot("s1");
            let _ = state.enqueue_tool_call(
                "s1",
                "/project",
                "Bash",
                sample_request(),
                ActionRisk::ReviewRequired,
                "needs review".to_string(),
            );
            let _ = state.enqueue_question("s1", "/project", "q?".to_string(), None);
            state.reject_item("s1", "AP-001").unwrap();
            let q = state.answer_question("s1", "AP-002").unwrap();
            assert_eq!(q, "q?");
        }
        let mut state = persisted_state("s1", &tmp);
        state.load_persisted();
        assert_eq!(state.items[0].state, DeferredState::Rejected);
        assert_eq!(state.items[1].state, DeferredState::Completed);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
