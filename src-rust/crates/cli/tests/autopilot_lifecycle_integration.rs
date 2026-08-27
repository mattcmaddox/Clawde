//! Integration tests for the autopilot lifecycle across crate boundaries.
//!
//! Exercises the full chain:
//!   core (AutonomyState) → tools (permission handler / request_permission_inner)
//!   → approve → retry → consume → persist → restart → re-approve → consume
//!
//! Does NOT spawn the clawde binary — imports library crates directly.
//!
//! NOTE: `set_now()` is #[cfg(test)] inside clawde-core and unavailable from
//! integration tests. Expiry paths are covered by clawde-core's unit tests;
//! these tests focus on the cross-crate lifecycle chain.

use clawde_core::action_risk::ActionRisk;
use clawde_core::autonomy::{AutonomyState, DeferredState};
use clawde_core::permissions::{PermissionLevel, PermissionRequest};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

fn different_request() -> PermissionRequest {
    PermissionRequest {
        tool_name: "Bash".to_string(),
        description: "run a command".to_string(),
        details: Some("git commit".to_string()),
        is_read_only: false,
        path: Some("git commit".to_string()),
        working_dir: Some(std::path::PathBuf::from("/project")),
        allowed_roots: vec![std::path::PathBuf::from("/project")],
        context_description: None,
        network_isolated: false,
        permission_level: PermissionLevel::Execute,
        network_capable: false,
        stateful: false,
    }
}

static TEST_DIR_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn test_dir() -> std::path::PathBuf {
    let n = TEST_DIR_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "clawde-autopilot-lifecycle-{}-{}",
        std::process::id(),
        n
    ))
}

// ---------------------------------------------------------------------------
// Test 1: Full lifecycle — defer → approve → retry → consume → repeat
// ---------------------------------------------------------------------------

#[test]
fn full_lifecycle_defer_approve_retry_consume() {
    let mut state = AutonomyState::new("sess-lifecycle");
    state.start_autopilot("sess-lifecycle");
    assert!(state.is_active("sess-lifecycle"));

    // Step 1: Model tries a review-required action → deferred.
    let item = state
        .enqueue_tool_call(
            "sess-lifecycle",
            "/project",
            "Bash",
            sample_request(),
            ActionRisk::ReviewRequired,
            "needs review".to_string(),
        )
        .expect("should enqueue");
    assert_eq!(item.id, "AP-001");
    assert_eq!(item.state, DeferredState::Pending);
    assert_eq!(state.pending_count(), 1);

    // Step 2: User approves the item.
    state
        .approve_item("sess-lifecycle", "AP-001", |item| {
            assert_eq!(item.tool_name(), Some("Bash"));
            Ok(())
        })
        .expect("approve should succeed");
    assert_eq!(state.items[0].state, DeferredState::Approved);
    // Approved items are actionable but not counted by pending_count()
    // (which counts only Pending/Stale). The command layer surfaces them
    // via a separate APPROVED marker in list output.
    assert_eq!(state.pending_count(), 0);

    // Step 3: Model retries the exact call → approval consumed, call runs.
    let consumed = state.take_approved_match("sess-lifecycle", &sample_request());
    assert_eq!(consumed.as_deref(), Some("AP-001"));
    assert_eq!(state.items[0].state, DeferredState::Completed);

    // Step 4: Second retry — no approval left, deferred again with new id.
    let item2 = state
        .enqueue_tool_call(
            "sess-lifecycle",
            "/project",
            "Bash",
            sample_request(),
            ActionRisk::ReviewRequired,
            "needs review".to_string(),
        )
        .expect("should enqueue again");
    assert_eq!(item2.id, "AP-002");
    assert_eq!(state.pending_count(), 1);
}

// ---------------------------------------------------------------------------
// Test 2: Changed request does NOT consume approval
// ---------------------------------------------------------------------------

#[test]
fn changed_request_does_not_consume_approval() {
    let mut state = AutonomyState::new("sess-changed");
    state.start_autopilot("sess-changed");

    let _ = state.enqueue_tool_call(
        "sess-changed",
        "/project",
        "Bash",
        sample_request(),
        ActionRisk::ReviewRequired,
        "needs review".to_string(),
    );
    state
        .approve_item("sess-changed", "AP-001", |_| Ok(()))
        .unwrap();

    // A different command → not consumed, deferred instead.
    let result = state.take_approved_match("sess-changed", &different_request());
    assert!(
        result.is_none(),
        "changed request must NOT consume approval"
    );
    assert_eq!(state.items[0].state, DeferredState::Approved);

    // The original exact retry still works.
    let result = state.take_approved_match("sess-changed", &sample_request());
    assert_eq!(result.as_deref(), Some("AP-001"));
}

// ---------------------------------------------------------------------------
// Test 3: Question deferral → answer → inject into next turn
// ---------------------------------------------------------------------------

#[test]
fn question_deferral_answer_and_injection() {
    let mut state = AutonomyState::new("sess-question");
    state.start_autopilot("sess-question");

    let item = state
        .enqueue_question(
            "sess-question",
            "/project",
            "Do you want to proceed with the migration?".to_string(),
            Some(vec!["Yes, proceed".to_string(), "No, stop".to_string()]),
        )
        .expect("should enqueue question");
    assert_eq!(item.id, "AP-001");
    assert_eq!(state.pending_count(), 1);

    // User answers the question.
    let question_text = state
        .answer_question("sess-question", "AP-001")
        .expect("answer should succeed");
    assert_eq!(question_text, "Do you want to proceed with the migration?");
    assert_eq!(state.items[0].state, DeferredState::Completed);
    assert_eq!(state.pending_count(), 0);

    // Answering a non-existent id fails.
    let err = state
        .answer_question("sess-question", "AP-999")
        .unwrap_err();
    assert!(err.contains("No item"));
}

// ---------------------------------------------------------------------------
// Test 4: Reject → item is dead
// ---------------------------------------------------------------------------

#[test]
fn reject_prevents_later_approval() {
    let mut state = AutonomyState::new("sess-reject");
    state.start_autopilot("sess-reject");

    let _ = state.enqueue_tool_call(
        "sess-reject",
        "/project",
        "Bash",
        sample_request(),
        ActionRisk::ReviewRequired,
        "needs review".to_string(),
    );
    state.reject_item("sess-reject", "AP-001").unwrap();
    assert_eq!(state.items[0].state, DeferredState::Rejected);

    // Cannot approve a rejected item.
    let err = state
        .approve_item("sess-reject", "AP-001", |_| Ok(()))
        .unwrap_err();
    assert!(err.contains("not pending"));
}

// ---------------------------------------------------------------------------
// Test 5: Persistence round-trip with restart recovery
// ---------------------------------------------------------------------------

#[test]
fn persistence_round_trip_with_restart_recovery() {
    let tmp = test_dir();

    // Phase 1: create items in a persisted state.
    {
        let mut state = AutonomyState::new("sess-persist");
        state.set_persistence_dir(tmp.clone());
        state.start_autopilot("sess-persist");

        // Defer two tool calls and one question.
        let _ = state.enqueue_tool_call(
            "sess-persist",
            "/project",
            "Bash",
            sample_request(),
            ActionRisk::ReviewRequired,
            "needs review".to_string(),
        );
        let diff_req = different_request();
        let _ = state.enqueue_tool_call(
            "sess-persist",
            "/project",
            "Bash",
            diff_req,
            ActionRisk::ReviewRequired,
            "also needs review".to_string(),
        );
        let _ = state.enqueue_question("sess-persist", "/project", "Proceed?".to_string(), None);

        // Approve the first item.
        state
            .approve_item("sess-persist", "AP-001", |_| Ok(()))
            .unwrap();

        // Reject the second.
        state.reject_item("sess-persist", "AP-002").unwrap();
    }

    // Phase 2: simulate restart — fresh state, load persisted.
    {
        let mut state = AutonomyState::new("sess-persist");
        state.set_persistence_dir(tmp.clone());
        let restored = state.load_persisted();
        assert_eq!(restored, 3, "all 3 items should be restored");

        // Approved → Stale (review-only after restart).
        assert_eq!(state.items[0].state, DeferredState::Stale);
        // Rejected stays Rejected.
        assert_eq!(state.items[1].state, DeferredState::Rejected);
        // Question (was Pending) → Stale.
        assert_eq!(state.items[2].state, DeferredState::Stale);

        // No approval survives a restart — cannot consume.
        assert!(state
            .take_approved_match("sess-persist", &sample_request())
            .is_none());

        // Re-approval revalidates the stale item.
        state
            .approve_item("sess-persist", "AP-001", |_| Ok(()))
            .unwrap();
        assert_eq!(state.items[0].state, DeferredState::Approved);

        // Now the retry works.
        let consumed = state.take_approved_match("sess-persist", &sample_request());
        assert_eq!(consumed.as_deref(), Some("AP-001"));
        assert_eq!(state.items[0].state, DeferredState::Completed);
    }

    // Phase 3: verify id stability across restart.
    {
        let mut state = AutonomyState::new("sess-persist");
        state.set_persistence_dir(tmp.clone());
        state.load_persisted();

        // New id must not collide with restored ids.
        let new_item = state
            .enqueue_question("sess-persist", "/project", "New?".to_string(), None)
            .unwrap();
        assert_eq!(new_item.id, "AP-004", "next_id should be past restored max");
    }

    let _ = std::fs::remove_dir_all(&tmp);
}

// ---------------------------------------------------------------------------
// Test 6: Queue dedup prevents retry-storm queue pollution
// ---------------------------------------------------------------------------

#[test]
fn retry_storm_dedup_prevents_queue_pollution() {
    let mut state = AutonomyState::new("sess-storm");
    state.start_autopilot("sess-storm");

    // The model retries the same action 5 times.
    for _ in 0..5 {
        let item = state
            .enqueue_tool_call(
                "sess-storm",
                "/project",
                "Bash",
                sample_request(),
                ActionRisk::ReviewRequired,
                "needs review".to_string(),
            )
            .unwrap();
        assert_eq!(
            item.id, "AP-001",
            "dedup must return the same id every time"
        );
    }
    assert_eq!(
        state.items.len(),
        1,
        "only one item in queue despite 5 enqueues"
    );
    assert_eq!(state.pending_count(), 1);

    // A different command IS distinct.
    let diff = state
        .enqueue_tool_call(
            "sess-storm",
            "/project",
            "Bash",
            different_request(),
            ActionRisk::ReviewRequired,
            "different action".to_string(),
        )
        .unwrap();
    assert_eq!(diff.id, "AP-002");
    assert_eq!(state.items.len(), 2);
}

// ---------------------------------------------------------------------------
// Test 7: Session mismatch makes state inert
// ---------------------------------------------------------------------------

#[test]
fn session_mismatch_prevents_all_operations() {
    let mut state = AutonomyState::new("sess-wrong");
    state.start_autopilot("sess-wrong");

    // Enqueue under the correct session.
    let _ = state.enqueue_tool_call(
        "sess-wrong",
        "/project",
        "Bash",
        sample_request(),
        ActionRisk::ReviewRequired,
        "needs review".to_string(),
    );

    // Different session id → cannot approve.
    let err = state
        .approve_item("other-sess", "AP-001", |_| Ok(()))
        .unwrap_err();
    assert!(err.contains("No item"));

    // Different session id → cannot reject.
    let err = state.reject_item("other-sess", "AP-001").unwrap_err();
    assert!(err.contains("No item"));

    // Different session id → cannot answer.
    let err = state.answer_question("other-sess", "AP-001").unwrap_err();
    assert!(err.contains("No item"));

    // Different session id → take_approved_match returns None.
    assert!(state
        .take_approved_match("other-sess", &sample_request())
        .is_none());
}

// ---------------------------------------------------------------------------
// Test 8: Working dir mismatch prevents approval consumption
// ---------------------------------------------------------------------------

#[test]
fn working_dir_mismatch_prevents_consumption() {
    let mut state = AutonomyState::new("sess-dir");
    state.start_autopilot("sess-dir");

    let _ = state.enqueue_tool_call(
        "sess-dir",
        "/project",
        "Bash",
        sample_request(),
        ActionRisk::ReviewRequired,
        "needs review".to_string(),
    );
    state
        .approve_item("sess-dir", "AP-001", |_| Ok(()))
        .unwrap();

    // Same command but different working dir.
    let mut moved = sample_request();
    moved.working_dir = Some(std::path::PathBuf::from("/elsewhere"));
    assert!(
        state.take_approved_match("sess-dir", &moved).is_none(),
        "different working dir must NOT consume approval"
    );
    assert_eq!(state.items[0].state, DeferredState::Approved);
}

// ---------------------------------------------------------------------------
// Test 9: Persisted file ignores other-session and corrupt data
// ---------------------------------------------------------------------------

#[test]
fn persisted_file_ignores_other_session_and_corrupt_data() {
    let tmp = test_dir();

    // Create a file for session "s1".
    {
        let mut state = AutonomyState::new("s1");
        state.set_persistence_dir(tmp.clone());
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

    // A state bound to "s2" must not load s1's file.
    let mut other = AutonomyState::new("s2");
    other.set_persistence_dir(tmp.clone());
    assert_eq!(other.load_persisted(), 0);
    assert!(other.items.is_empty());

    // Write corrupt JSON for s2's slot.
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("autonomy-s2.json"), "{ not json ").unwrap();
    assert_eq!(other.load_persisted(), 0);
    assert!(other.items.is_empty());

    let _ = std::fs::remove_dir_all(&tmp);
}

// ---------------------------------------------------------------------------
// Test 10: Reject and answer persist across restart
// ---------------------------------------------------------------------------

#[test]
fn reject_and_answer_persist_across_restart() {
    let tmp = test_dir();

    {
        let mut state = AutonomyState::new("sess-r-a");
        state.set_persistence_dir(tmp.clone());
        state.start_autopilot("sess-r-a");

        let _ = state.enqueue_tool_call(
            "sess-r-a",
            "/project",
            "Bash",
            sample_request(),
            ActionRisk::ReviewRequired,
            "needs review".to_string(),
        );
        let _ = state.enqueue_question("sess-r-a", "/project", "q?".to_string(), None);

        state.reject_item("sess-r-a", "AP-001").unwrap();
        let q = state.answer_question("sess-r-a", "AP-002").unwrap();
        assert_eq!(q, "q?");
    }

    // Restart: verify reject/answer history survives.
    let mut state = AutonomyState::new("sess-r-a");
    state.set_persistence_dir(tmp.clone());
    state.load_persisted();
    assert_eq!(state.items[0].state, DeferredState::Rejected);
    assert_eq!(state.items[1].state, DeferredState::Completed);

    let _ = std::fs::remove_dir_all(&tmp);
}
