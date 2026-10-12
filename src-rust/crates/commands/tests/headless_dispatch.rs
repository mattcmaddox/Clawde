// Integration coverage for the headless single-shot path's slash-command
// dispatch.
//
// The CLI headless entrypoint (`run_headless` in `crates/cli/src/main.rs`)
// routes any `/...` prompt through `clawde_commands::find_command` and
// `SlashCommand::execute` instead of forwarding it to the model. These tests
// exercise the public `execute_command` entrypoint the headless path relies
// on, so a change that stops resolving a real command (or that starts
// swallowing non-command prompts) fails here.
//
// `/context` is the vehicle: it is a pure built-in that reads only the
// in-memory `CommandContext` and never touches disk, so it is safe to run
// against a scratch context.

use clawde_commands::{execute_command, CommandContext, CommandResult};
use clawde_core::config::Config;
use clawde_core::cost::CostTracker;

/// Build a `CommandContext` whose working directory is a scratch temp dir, so
/// skill discovery and plugin loading see an empty project rather than this
/// repository.
fn scratch_context(working_dir: std::path::PathBuf) -> CommandContext {
    CommandContext {
        config: Config::default(),
        cost_tracker: CostTracker::new(),
        messages: Vec::new(),
        working_dir,
        session_id: "headless-dispatch-test".to_string(),
        session_title: None,
        remote_session_url: None,
        mcp_manager: None,
        mcp_auth_runner: None,
        provider_registry: None,
        test_provider: None,
        effort: None,
        tool_use_tracker: None,
        autonomy: None,
        transient_prev_config: None,
    }
}

fn assert_context_message(result: Option<CommandResult>) {
    match result {
        Some(CommandResult::Message(text)) => {
            assert!(
                text.contains("Context Window Usage"),
                "expected /context output to contain the usage header, got:\n{text}"
            );
        }
        other => panic!("expected CommandResult::Message, got {other:?}"),
    }
}

#[tokio::test]
async fn headless_dispatch_resolves_a_real_slash_command() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ctx = scratch_context(dir.path().to_path_buf());

    let result = execute_command("/context", &mut ctx).await;
    assert_context_message(result);
}

#[tokio::test]
async fn headless_dispatch_resolves_the_context_window_alias() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ctx = scratch_context(dir.path().to_path_buf());

    let result = execute_command("/context-window", &mut ctx).await;
    assert_context_message(result);
}

#[tokio::test]
async fn headless_dispatch_returns_none_for_an_unknown_slash_command() {
    // The headless path fails loudly on this (exit 1) rather than sending the
    // string to the model; `execute_command` is what signals "no command
    // matched" by returning `None`.
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ctx = scratch_context(dir.path().to_path_buf());

    let result = execute_command("/definitely-not-a-real-command", &mut ctx).await;
    assert!(result.is_none(), "unknown slash command must not resolve");
}

#[tokio::test]
async fn headless_dispatch_ignores_plain_prompts() {
    // Non-slash text is a model prompt and must not be intercepted.
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ctx = scratch_context(dir.path().to_path_buf());

    let result = execute_command("explain this repository", &mut ctx).await;
    assert!(result.is_none(), "plain prompt must flow to the model");
}
