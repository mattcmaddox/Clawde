// `/verify` command.
//
// Runs a single execute-and-verify round on demand (audit spec Phase 1):
// detects the project's test/lint commands and runs them in the configured
// sandbox (direct / git worktree / container), then hands the structured
// `VerifyReport` to the CLI so the TUI can render the boxed per-check
// indicator — the same box the auto-verify loop draws after writing turns.
//
// Works even when `verify.enabled` is false (the auto-loop's off-switch):
// `/verify` is a manual, always-available check.

use super::*;
use async_trait::async_trait;

pub struct VerifyCommand;

// ---- /verify -------------------------------------------------------------

/// Run a manual `/verify` round: apply the subset argument (`test` / `lint` /
/// `all`), then run the checks on a blocking thread.
///
/// Shared by [`VerifyCommand::execute`] and the CLI's async TUI dispatch (see
/// `crates/cli/src/main.rs`) so the two paths can never diverge — the command
/// registry stays the single source of truth for how a round runs.
pub async fn run_verify_command(
    config: &clawde_core::config::VerifyConfig,
    working_dir: &std::path::Path,
    args: &str,
) -> Result<clawde_query::VerifyReport, String> {
    let mut config = config.clone();
    clawde_query::verify::apply_verify_subset(&mut config, args)?;
    let working_dir = working_dir.to_path_buf();
    // Spawn on a blocking thread: the checks run bounded external commands
    // (tests/lints) and may take up to `timeout_secs` each.
    let run = move || clawde_query::verify::run_verify_round(&config, &working_dir);
    match tokio::task::spawn_blocking(run).await {
        Ok(result) => result,
        Err(e) => Err(format!("Verification task failed: {e}")),
    }
}

#[async_trait]
impl SlashCommand for VerifyCommand {
    fn name(&self) -> &str {
        "verify"
    }
    fn description(&self) -> &str {
        "Run one verification round (tests + lints) now and show the report"
    }
    fn help(&self) -> &str {
        "Usage: /verify [test|lint|all]\n\n\
         Runs the project's detected test suite and linter/typechecker once, in\n\
         the sandbox configured by `verify.sandbox` (direct, git worktree, or\n\
         container), and shows the boxed per-check report. This is the same\n\
         loop that runs automatically after writing turns — use it to check\n\
         the tree at any time, or after disabling auto-verify (it overrides\n\
         `verify.enabled: false`).\n\n\
         Args:\n\
           test  — run only the test suite\n\
           lint  — run only the linter/typechecker\n\
           all   — run both (default)\n\n\
         Configure via settings.json:\n\
           \"verify\": { \"sandbox\": \"worktree\", \"auto_test\": true, \"auto_lint\": true }"
    }

    fn arg_completions(&self, partial: &str) -> Vec<ArgCompletion> {
        let candidates = ["test", "lint", "all"];
        candidates
            .into_iter()
            .filter(|c| c.starts_with(partial))
            .map(|value| ArgCompletion {
                value: value.to_string(),
                description: match value {
                    "test" => "Run only the test suite",
                    "lint" => "Run only the linter/typechecker",
                    _ => "Run both tests and lints (default)",
                }
                .to_string(),
                available: true,
            })
            .collect()
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        // A manual /verify must work even when the auto-loop is disabled, so
        // only auto_test/auto_lint gate what runs — never `verify.enabled`.
        match run_verify_command(&ctx.config.verify, &ctx.working_dir, args).await {
            Ok(report) => CommandResult::Verify(report),
            Err(message) => CommandResult::Error(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx() -> CommandContext {
        CommandContext {
            config: clawde_core::config::Config::default(),
            cost_tracker: clawde_core::cost::CostTracker::new(),
            messages: Vec::new(),
            working_dir: std::env::temp_dir(),
            session_id: "test".to_string(),
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

    #[tokio::test]
    async fn verify_command_rejects_unknown_arg() {
        let cmd = VerifyCommand;
        let mut ctx = test_ctx();
        let result = cmd.execute("bogus", &mut ctx).await;
        match result {
            CommandResult::Error(msg) => assert!(msg.contains("Unknown /verify argument")),
            other => panic!("expected Error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn verify_command_subset_arg_selects_checks() {
        // test → lints disabled; lint → tests disabled. Run in an empty temp
        // dir so no checks are detected (no side effects); the report headline
        // must reflect that a round was attempted, not that it was disabled.
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx();
        ctx.working_dir = dir.path().to_path_buf();
        ctx.config.verify.enabled = false; // manual override must still run
        ctx.config.verify.sandbox = clawde_core::config::VerifySandbox::Direct;

        let cmd = VerifyCommand;
        for arg in ["test", "lint", "all"] {
            let result = cmd.execute(arg, &mut ctx).await;
            match result {
                CommandResult::Verify(report) => {
                    assert_eq!(
                        report.headline, "No test or lint commands detected",
                        "arg {arg}: expected empty-dir report, got {}",
                        report.headline
                    );
                }
                other => panic!("arg {arg}: expected Verify, got: {other:?}"),
            }
        }
    }
}
