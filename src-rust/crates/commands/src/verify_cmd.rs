// `/verify` command.
//
// Runs a single execute-and-verify round on demand (audit spec Phase 1):
// detects the project's test/lint commands and runs them in the configured
// sandbox (direct / git worktree / container), then hands the structured
// `VerifyReport` to the CLI so the TUI can render the boxed per-check
// indicator — the same box the auto-verify loop draws after writing turns.

use super::*;
use async_trait::async_trait;

pub struct VerifyCommand;

// ---- /verify -------------------------------------------------------------

#[async_trait]
impl SlashCommand for VerifyCommand {
    fn name(&self) -> &str {
        "verify"
    }
    fn description(&self) -> &str {
        "Run one verification round (tests + lints) now and show the report"
    }
    fn help(&self) -> &str {
        "Usage: /verify\n\n\
         Runs the project's detected test suite and linter/typechecker once, in\n\
         the sandbox configured by `verify.sandbox` (direct, git worktree, or\n\
         container), and shows the boxed per-check report. This is the same\n\
         loop that runs automatically after writing turns — use it to check\n\
         the tree at any time, or after disabling auto-verify.\n\n\
         Configure via settings.json:\n\
           \"verify\": { \"sandbox\": \"worktree\", \"auto_test\": true, \"auto_lint\": true }"
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        let working_dir = ctx.working_dir.clone();
        let config = ctx.config.verify.clone();
        // Spawn on a blocking thread: the checks run bounded external commands
        // (tests/lints) and may take up to `timeout_secs` each.
        let run = move || clawde_query::verify::run_verify_round(&config, &working_dir);
        match tokio::task::spawn_blocking(run).await {
            Ok(Ok(report)) => CommandResult::Verify(report),
            Ok(Err(message)) => CommandResult::Error(message),
            Err(e) => CommandResult::Error(format!("Verification task failed: {}", e)),
        }
    }
}
