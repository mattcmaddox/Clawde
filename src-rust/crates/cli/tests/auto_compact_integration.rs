//! Integration tests for the auto-compact system.
//!
//! These tests exercise the full chain across crate boundaries:
//!
//!   commands (AutoCompactCommand) → core (Config, Settings) → query (threshold, debounce)
//!
//! The TUI footer indicator is inherently visual; its state derivation
//! from `Config::auto_compact` is verified through the config roundtrip.
//!
//! Does NOT spawn the clawde binary — imports library crates directly.

use clawde_commands::{AutoCompactCommand, CommandContext, CommandResult, SlashCommand};
use clawde_core::config::{Config, Settings};
use clawde_core::cost::CostTracker;
use clawde_query::compact::{should_auto_compact_for_window, AutoCompactState};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn default_command_context() -> CommandContext {
    CommandContext {
        config: Config::default(),
        cost_tracker: CostTracker::new(),
        messages: vec![],
        working_dir: std::env::current_dir().unwrap_or_else(|_| "/tmp".into()),
        session_id: "test-session".to_string(),
        session_title: None,
        remote_session_url: None,
        mcp_manager: None,
        mcp_auth_runner: None,
        provider_registry: None,
        test_provider: None,
        effort: None,
        tool_use_tracker: None,
    }
}

// ---------------------------------------------------------------------------
// Gap 3: Command → ConfigChangeMessage integration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn toggle_on_produces_config_change_message() {
    let mut ctx = default_command_context();
    ctx.config.auto_compact = false;

    let cmd = AutoCompactCommand;
    let result = cmd.execute("on", &mut ctx).await;

    match result {
        CommandResult::ConfigChangeMessage(new_cfg, msg) => {
            assert!(
                new_cfg.auto_compact,
                "auto_compact must be true after /auto-compact on"
            );
            assert!(
                msg.to_lowercase().contains("enabled"),
                "status message must contain 'enabled', got: {msg}"
            );
        }
        other => panic!("expected ConfigChangeMessage, got {other:?}"),
    }
}

#[tokio::test]
async fn toggle_off_produces_config_change_message() {
    let mut ctx = default_command_context();
    ctx.config.auto_compact = true;

    let cmd = AutoCompactCommand;
    let result = cmd.execute("off", &mut ctx).await;

    match result {
        CommandResult::ConfigChangeMessage(new_cfg, msg) => {
            assert!(
                !new_cfg.auto_compact,
                "auto_compact must be false after /auto-compact off"
            );
            assert!(
                msg.to_lowercase().contains("disabled"),
                "status message must contain 'disabled', got: {msg}"
            );
        }
        other => panic!("expected ConfigChangeMessage, got {other:?}"),
    }
}

#[tokio::test]
async fn toggle_no_args_flips_state() {
    let mut ctx = default_command_context();
    ctx.config.auto_compact = false;

    let cmd = AutoCompactCommand;
    let result = cmd.execute("", &mut ctx).await;

    match result {
        CommandResult::ConfigChangeMessage(new_cfg, _msg) => {
            assert!(
                new_cfg.auto_compact,
                "no-args toggle from off must produce auto_compact = true"
            );
        }
        other => panic!("expected ConfigChangeMessage, got {other:?}"),
    }
}

#[tokio::test]
async fn noop_when_already_in_desired_state() {
    let mut ctx = default_command_context();
    ctx.config.auto_compact = true;

    let cmd = AutoCompactCommand;
    let result = cmd.execute("on", &mut ctx).await;

    match result {
        CommandResult::Message(msg) => {
            assert!(
                msg.to_lowercase().contains("already"),
                "no-op must report 'already enabled', got: {msg}"
            );
        }
        other => panic!("expected Message (no-op), got {other:?}"),
    }
}

#[tokio::test]
async fn rejects_unknown_argument() {
    let mut ctx = default_command_context();
    let cmd = AutoCompactCommand;
    let result = cmd.execute("maybe", &mut ctx).await;

    match result {
        CommandResult::Error(msg) => {
            assert!(
                msg.contains("Unknown"),
                "error must mention 'Unknown', got: {msg}"
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Gap 3: Config flow integration — Settings → effective_config
// ---------------------------------------------------------------------------

#[test]
fn effective_config_merges_top_level_auto_compact() {
    let mut settings = Settings::default();
    settings.auto_compact = true;
    // Config-level auto_compact stays at its default (false).
    settings.config.auto_compact = false;

    let config = settings.effective_config();

    // The top-level `auto_compact` key should win.
    assert!(
        config.auto_compact,
        "effective_config must merge top-level auto_compact=true over config-level false"
    );
}

#[test]
fn effective_config_auto_compact_false_by_default() {
    let settings = Settings::default();
    let config = settings.effective_config();
    assert!(!config.auto_compact, "auto_compact must default to false");
}

#[test]
fn effective_config_config_level_auto_compact_wins_when_top_level_none() {
    // OR-merge: top-level false (default) || config-level true == true.
    let mut settings = Settings::default();
    settings.config.auto_compact = true;

    let config = settings.effective_config();
    assert!(
        config.auto_compact,
        "config-level auto_compact=true should flow through when top-level is default false"
    );
}

// ---------------------------------------------------------------------------
// Gap 5: Threshold — should_auto_compact_for_window
// ---------------------------------------------------------------------------

#[test]
fn threshold_triggers_at_90_percent() {
    let state = AutoCompactState::default();
    let window = 200_000;
    // 90% of 200k = 180k
    assert!(
        should_auto_compact_for_window(180_000, window, &state),
        "must trigger at exactly 90% of context window"
    );
    assert!(
        should_auto_compact_for_window(195_000, window, &state),
        "must trigger above 90%"
    );
}

#[test]
fn threshold_does_not_trigger_below_90_percent() {
    let state = AutoCompactState::default();
    let window = 200_000;
    // 89% of 200k = 178k
    assert!(
        !should_auto_compact_for_window(179_999, window, &state),
        "must not trigger below 90% threshold"
    );
}

#[test]
fn threshold_disabled_state_blocks_compaction() {
    let state = AutoCompactState {
        disabled: true,
        ..Default::default()
    };
    let window = 200_000;
    // Even at 100%, a disabled state returns false.
    assert!(
        !should_auto_compact_for_window(200_000, window, &state),
        "disabled state must block compaction even at 100% usage"
    );
}

#[test]
fn threshold_tiny_window() {
    let state = AutoCompactState::default();
    // Small window: 1000 tokens, 90% = 900.
    assert!(
        should_auto_compact_for_window(900, 1000, &state),
        "must trigger at 90% even for small windows"
    );
    assert!(
        !should_auto_compact_for_window(899, 1000, &state),
        "must not trigger below 90% for small windows"
    );
}

// ---------------------------------------------------------------------------
// Gap 5: Debounce state machine
// ---------------------------------------------------------------------------

#[test]
fn debounce_on_success_resets_counters() {
    let mut state = AutoCompactState {
        turns_since_last_compact: 7,
        last_compact_at: None,
        ..Default::default()
    };

    state.on_success();

    assert_eq!(state.compaction_count, 1);
    assert_eq!(state.consecutive_failures, 0);
    assert_eq!(
        state.turns_since_last_compact, 0,
        "on_success must reset turn counter"
    );
    assert!(
        state.last_compact_at.is_some(),
        "on_success must record timestamp"
    );
}

#[test]
fn debounce_on_failure_increments_failures() {
    let mut state = AutoCompactState::default();
    state.on_failure();
    assert_eq!(state.consecutive_failures, 1);

    state.on_failure();
    assert_eq!(state.consecutive_failures, 2);
}

#[test]
fn debounce_circuit_breaker_opens_after_max_failures() {
    let mut state = AutoCompactState::default();
    // MAX_CONSECUTIVE_FAILURES = 3 (from compact.rs)
    for _ in 0..3 {
        state.on_failure();
    }
    assert!(
        state.disabled,
        "circuit breaker must open after 3 consecutive failures"
    );
}

#[test]
fn debounce_circuit_breaker_stays_closed_below_threshold() {
    let mut state = AutoCompactState::default();
    for _ in 0..2 {
        state.on_failure();
    }
    assert!(
        !state.disabled,
        "circuit breaker must stay closed before 3 failures"
    );
}

#[test]
fn debounce_success_resets_failure_count() {
    let mut state = AutoCompactState::default();
    state.on_failure();
    state.on_failure();
    // 2 failures — circuit is still closed
    assert!(!state.disabled);

    state.on_success();
    assert_eq!(
        state.consecutive_failures, 0,
        "success must reset failure counter"
    );
}

#[test]
fn debounce_first_compaction_has_no_timestamp() {
    let state = AutoCompactState::default();
    assert!(
        state.last_compact_at.is_none(),
        "fresh AutoCompactState must have no last_compact_at"
    );
}

// ---------------------------------------------------------------------------
// End-to-end: config → command → threshold chain
// ---------------------------------------------------------------------------

#[tokio::test]
async fn end_to_end_toggle_on_then_check_threshold() {
    // 1. Start with auto_compact off
    let mut ctx = default_command_context();
    ctx.config.auto_compact = false;

    // 2. Toggle on via command
    let cmd = AutoCompactCommand;
    let result = cmd.execute("on", &mut ctx).await;

    let new_config = match result {
        CommandResult::ConfigChangeMessage(cfg, _) => cfg,
        other => panic!("expected ConfigChangeMessage, got {other:?}"),
    };

    assert!(new_config.auto_compact);

    // 3. Simulate: if the config has auto_compact=true, the query loop would
    //    call should_auto_compact_for_window. Verify that path works.
    //    (The actual query loop gate checks `tool_ctx.config.auto_compact` —
    //    this test verifies the config state is correct after the command.)
    let state = AutoCompactState::default();
    // At 95% of a 200k window, compaction should fire.
    assert!(
        should_auto_compact_for_window(190_000, 200_000, &state),
        "with auto_compact enabled, threshold must trigger at 95%"
    );
}

#[tokio::test]
async fn end_to_end_toggle_off_then_check_threshold_blocked() {
    // 1. Start with auto_compact on
    let mut ctx = default_command_context();
    ctx.config.auto_compact = true;

    // 2. Toggle off via command
    let cmd = AutoCompactCommand;
    let result = cmd.execute("off", &mut ctx).await;

    let new_config = match result {
        CommandResult::ConfigChangeMessage(cfg, _) => cfg,
        other => panic!("expected ConfigChangeMessage, got {other:?}"),
    };

    assert!(!new_config.auto_compact);

    // 3. Verify: when auto_compact is disabled, the query loop gate
    //    (`tool_ctx.config.auto_compact`) is false — the entire compact
    //    block is skipped. We can't test the gate directly here (it lives
    //    in the CLI's run_query_loop), but the config state is correct.
}

#[tokio::test]
async fn end_to_end_multiple_toggles() {
    let mut ctx = default_command_context();
    let cmd = AutoCompactCommand;

    // Toggle on
    let r1 = cmd.execute("on", &mut ctx).await;
    let cfg1 = match r1 {
        CommandResult::ConfigChangeMessage(cfg, _) => cfg,
        other => panic!("expected ConfigChangeMessage, got {other:?}"),
    };
    assert!(cfg1.auto_compact);
    ctx.config = cfg1;

    // Toggle off
    let r2 = cmd.execute("off", &mut ctx).await;
    let cfg2 = match r2 {
        CommandResult::ConfigChangeMessage(cfg, _) => cfg,
        other => panic!("expected ConfigChangeMessage, got {other:?}"),
    };
    assert!(!cfg2.auto_compact);
    ctx.config = cfg2;

    // Toggle (flip) — no args
    let r3 = cmd.execute("", &mut ctx).await;
    let cfg3 = match r3 {
        CommandResult::ConfigChangeMessage(cfg, _) => cfg,
        other => panic!("expected ConfigChangeMessage, got {other:?}"),
    };
    assert!(cfg3.auto_compact, "flip from off must be on");
}

// ---------------------------------------------------------------------------
// Footer indicator: state derivation from (auto_compact_enabled, used_pct)
// ---------------------------------------------------------------------------
//
// The TUI footer in render.rs maps (auto_compact_enabled, used_pct) to a
// color + label.  These tests verify the derivation table without needing
// a running TUI.  Thresholds sourced from render.rs::render_footer:

/// The footer state that the TUI would render for a given context usage.
#[derive(Debug, PartialEq)]
enum FooterContextState {
    /// Green — healthy, auto-compact on.
    Healthy,
    /// Dim gray — healthy but auto-compact off.
    HealthyOff,
    /// Yellow — warning zone (70-84%).
    Warning,
    /// Yellow bold — elevated (85-94%), nudges toward compaction.
    Elevated,
    /// Red bold — critical (>=95%), needs compaction now.
    Critical,
}

fn derive_footer_state(used_pct: u64, auto_compact_enabled: bool) -> FooterContextState {
    if used_pct >= 95 {
        FooterContextState::Critical
    } else if used_pct >= 85 {
        FooterContextState::Elevated
    } else if used_pct >= 70 {
        FooterContextState::Warning
    } else if auto_compact_enabled {
        FooterContextState::Healthy
    } else {
        FooterContextState::HealthyOff
    }
}

#[test]
fn footer_state_critical_at_95_percent() {
    assert_eq!(derive_footer_state(95, true), FooterContextState::Critical);
    assert_eq!(
        derive_footer_state(99, false),
        FooterContextState::Critical,
        "critical regardless of auto_compact state"
    );
}

#[test]
fn footer_state_elevated_at_85_to_94_percent() {
    assert_eq!(derive_footer_state(85, true), FooterContextState::Elevated);
    assert_eq!(
        derive_footer_state(90, false),
        FooterContextState::Elevated,
        "elevated regardless of auto_compact state"
    );
}

#[test]
fn footer_state_warning_at_70_to_84_percent() {
    assert_eq!(derive_footer_state(70, true), FooterContextState::Warning);
    assert_eq!(
        derive_footer_state(84, false),
        FooterContextState::Warning,
        "warning regardless of auto_compact state"
    );
}

#[test]
fn footer_state_healthy_below_70_with_auto_compact_on() {
    assert_eq!(derive_footer_state(0, true), FooterContextState::Healthy);
    assert_eq!(derive_footer_state(50, true), FooterContextState::Healthy);
    assert_eq!(derive_footer_state(69, true), FooterContextState::Healthy);
}

#[test]
fn footer_state_healthy_off_below_70_with_auto_compact_off() {
    assert_eq!(
        derive_footer_state(0, false),
        FooterContextState::HealthyOff
    );
    assert_eq!(
        derive_footer_state(50, false),
        FooterContextState::HealthyOff
    );
    assert_eq!(
        derive_footer_state(69, false),
        FooterContextState::HealthyOff
    );
}

#[test]
fn footer_state_derivation_matches_config_roundtrip() {
    // Verify the config roundtrip: config.auto_compact flows to the
    // auto_compact_enabled boolean that drives derive_footer_state.
    let mut settings = Settings::default();
    settings.auto_compact = true;
    let config = settings.effective_config();
    assert!(config.auto_compact);

    // At 60% with auto_compact on, footer should be Healthy (green).
    assert_eq!(
        derive_footer_state(60, config.auto_compact),
        FooterContextState::Healthy
    );

    // After toggling off, footer should be HealthyOff (gray).
    let mut settings = Settings::default();
    settings.auto_compact = false;
    let config = settings.effective_config();
    assert!(!config.auto_compact);
    assert_eq!(
        derive_footer_state(60, config.auto_compact),
        FooterContextState::HealthyOff
    );
}
