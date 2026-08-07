// verify.rs — Execute-and-verify continuation policy (audit spec Phase 1).
//
// After a turn that wrote files, `VerifyPolicy` automatically runs the
// project's test suite and linter (reusing `DetectProjectTool`'s detection),
// then either:
//
// - all checks pass → `Stop` with a "checks passed" note;
// - a check fails and attempts remain → `Continue` with a message describing
//   the failures, so the model fixes them and the loop verifies again;
// - a check fails and `max_retries` is exhausted → `Stop` with the remaining
//   failures surfaced to the user.
//
// Command execution is synchronous and bounded by `VerifyConfig::timeout_secs`
// per command (a child that exceeds the deadline is killed, so a hung test
// suite can never stall the loop forever). `decide` is called at most once per
// `end_turn` from the async loop; the blocking call is acceptable precisely
// because the timeout bounds it and the loop continues or returns immediately
// afterwards. Output is redirected to a temp file instead of a pipe so a
// chatty suite can't deadlock on a full pipe buffer while we wait.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use clawde_core::config::VerifyConfig;

use crate::continuation::{ContinuationDecision, ContinuationPolicy, TurnEndContext};

/// Result of running one verification command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    /// Display label, e.g. `test: cargo test --workspace`.
    pub label: String,
    /// True when the command exited 0 and did not time out.
    pub ok: bool,
    /// Captured stdout+stderr, truncated for the continuation message.
    pub output: String,
    /// True when the command was killed after the configured timeout.
    pub timed_out: bool,
    /// True when the command could not be started at all (e.g. the detected
    /// binary is not installed). An environment gap, not a code failure the
    /// model can fix — never counts as a failure.
    pub skipped: bool,
}

impl CheckResult {
    fn pass(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            ok: true,
            output: String::new(),
            timed_out: false,
            skipped: false,
        }
    }

    fn fail(label: impl Into<String>, output: impl Into<String>, timed_out: bool) -> Self {
        Self {
            label: label.into(),
            ok: false,
            output: truncate_output(&output.into(), 4_000),
            timed_out,
            skipped: false,
        }
    }

    fn skipped(label: impl Into<String>, output: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            ok: false,
            output: truncate_output(&output.into(), 4_000),
            timed_out: false,
            skipped: true,
        }
    }

    /// One-line reason for a skipped check (the first line of the spawn error).
    fn skip_reason(&self) -> &str {
        self.output
            .lines()
            .next()
            .unwrap_or("could not start")
            .trim()
    }
}

/// Structured outcome of one verification round, surfaced to the TUI so it
/// can render the boxed per-check indicator (audit spec §15.1).
#[derive(Debug, Clone)]
pub struct VerifyReport {
    /// Per-check results in execution order (tests first, then lints).
    pub results: Vec<CheckResult>,
    /// Which auto-fix attempt this round was (1-based). 0 when no round ran
    /// (verification skipped).
    pub attempt: u32,
    /// Configured max auto-fix attempts.
    pub max_retries: u32,
    /// One-line summary of the round's outcome, e.g. "All checks passed" or
    /// "Auto-fix exhausted (3 attempts)".
    pub headline: String,
}

/// Execute-and-verify continuation policy (audit spec Phase 1).
///
/// See the module docs for the decision flow. The attempt counter is shared
/// state across the continuation turns of a single run; it is reset to zero
/// whenever a verification round passes.
pub struct VerifyPolicy {
    config: VerifyConfig,
    working_dir: PathBuf,
    attempts: AtomicU32,
    /// The most recent round's structured report, read by the query loop to
    /// emit `QueryEvent::Verify` for the TUI indicator. Cleared whenever a
    /// round is skipped (preflight) so a skipped turn never re-displays an
    /// old box.
    last_report: std::sync::Mutex<Option<VerifyReport>>,
}

impl VerifyPolicy {
    /// Build the policy for a run. `working_dir` is the project root that
    /// commands are detected against and executed in.
    pub fn new(config: VerifyConfig, working_dir: PathBuf) -> Self {
        Self {
            config,
            working_dir,
            attempts: AtomicU32::new(0),
            last_report: std::sync::Mutex::new(None),
        }
    }

    /// Stash the round's structured report for `verify_report`.
    fn stash_report(
        &self,
        results: &[CheckResult],
        attempt: u32,
        max_retries: u32,
        headline: impl Into<String>,
    ) {
        *self.last_report.lock().unwrap() = Some(VerifyReport {
            results: results.to_vec(),
            attempt,
            max_retries,
            headline: headline.into(),
        });
    }

    /// Cheap pre-flight guards that avoid spawning any commands. Returns the
    /// stop decision when verification should not (or cannot) run.
    fn preflight(&self, ctx: &TurnEndContext<'_>) -> Option<ContinuationDecision> {
        if !self.config.enabled || !self.config.has_any_check() {
            return Some(ContinuationDecision::Stop { note: None });
        }
        if self.config.skip_when_no_writes && !ctx.turn_made_writes {
            return Some(ContinuationDecision::Stop { note: None });
        }
        if !self.config.sandbox.is_implemented() {
            return Some(ContinuationDecision::Stop {
                note: Some(format!(
                    "Verify sandbox '{}' is not implemented yet — skipped verification. \
                     Set \"verify\": {{\"sandbox\": \"direct\"}} in settings.json.",
                    self.config.sandbox.label()
                )),
            });
        }
        None
    }

    /// Full decision logic over already-computed check results. Kept separate
    /// (and pure apart from the attempt counter) so unit tests can exercise
    /// every branch without spawning real commands.
    fn decide_with_results(
        &self,
        ctx: &TurnEndContext<'_>,
        results: &[CheckResult],
    ) -> ContinuationDecision {
        if let Some(decision) = self.preflight(ctx) {
            self.clear_report();
            return decision;
        }
        if results.is_empty() {
            self.stash_report(
                results,
                0,
                self.config.max_retries.max(1),
                "No test or lint commands detected",
            );
            return ContinuationDecision::Stop {
                note: Some(
                    "No test or lint commands were detected for this project — \
                     verification skipped."
                        .to_string(),
                ),
            };
        }

        // Every detected command failed to start (missing binary, etc.) — an
        // environment gap, not a code failure. Stop with a clear note instead
        // of burning auto-fix retries on something the model cannot fix.
        if results.iter().all(|r| r.skipped) {
            let reasons = results
                .iter()
                .map(|r| format!("[{}] {}", r.label, r.output))
                .collect::<Vec<_>>()
                .join("\n");
            self.stash_report(
                results,
                0,
                self.config.max_retries.max(1),
                "Verification could not run — commands missing",
            );
            return ContinuationDecision::Stop {
                note: Some(format!(
                    "Verification could not run — none of the detected commands could be \
                     started:\n{reasons}",
                )),
            };
        }

        let max_retries = self.config.max_retries.max(1);
        let attempt = self.attempts.fetch_add(1, Ordering::Relaxed) + 1;
        let failures: Vec<&CheckResult> = results.iter().filter(|r| !r.ok && !r.skipped).collect();
        let skipped: Vec<&CheckResult> = results.iter().filter(|r| r.skipped).collect();

        if failures.is_empty() {
            self.attempts.store(0, Ordering::Relaxed);
            let summary = results
                .iter()
                .map(|r| {
                    if r.skipped {
                        format!("  {} … SKIPPED ({})", r.label, r.skip_reason())
                    } else {
                        format!("  {} … PASS", r.label)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            self.stash_report(results, attempt, max_retries, "All checks passed");
            return ContinuationDecision::Stop {
                note: Some(format!("All checks passed:\n{}", summary)),
            };
        }

        let mut failures_text = failures
            .iter()
            .map(|r| {
                let reason = if r.timed_out {
                    "timed out (killed)".to_string()
                } else {
                    "failed".to_string()
                };
                format!("[{}] {}\n{}", r.label, reason, r.output)
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        if !skipped.is_empty() {
            let skip_note = skipped
                .iter()
                .map(|r| format!("[{}] could not start: {}", r.label, r.skip_reason()))
                .collect::<Vec<_>>()
                .join("\n");
            failures_text.push_str(&format!("\n\nSkipped (not run):\n{}", skip_note));
        }

        // `attempt` counts verification rounds; the first failing round is
        // auto-fix attempt 1, so up to `max_retries` fix attempts are allowed.
        if attempt <= max_retries {
            self.stash_report(
                results,
                attempt,
                max_retries,
                format!("Auto-fix attempt {attempt}/{max_retries}"),
            );
            return ContinuationDecision::Continue {
                message: format!(
                    "Verify your changes before finishing — the last verification run reported \
                     failures (auto-fix attempt {attempt}/{max_retries}):\n\n{failures_text}\n\n\
                     Fix the failures, then re-run the checks (RunTests/RunLints) until they \
                     pass, or explain why each failure is a false positive.",
                ),
            };
        }

        self.stash_report(
            results,
            attempt,
            max_retries,
            format!("Auto-fix exhausted ({max_retries} attempts)"),
        );
        ContinuationDecision::Stop {
            note: Some(format!(
                "Auto-fix exhausted ({max_retries} attempts) — verification still failing:\n\n{failures_text}",
            )),
        }
    }

    /// Clear any stashed report (called when a round is skipped via preflight
    /// so a skipped turn never re-displays an old verify box).
    fn clear_report(&self) {
        *self.last_report.lock().unwrap() = None;
    }
}

impl ContinuationPolicy for VerifyPolicy {
    fn decide(&self, ctx: &TurnEndContext<'_>) -> ContinuationDecision {
        if let Some(decision) = self.preflight(ctx) {
            self.clear_report();
            return decision;
        }
        let results = run_checks(&self.config, &self.working_dir);
        self.decide_with_results(ctx, &results)
    }

    /// The structured report of the most recent verification round, if one
    /// ran. Consulted by the query loop after `decide` returns.
    fn verify_report(&self) -> Option<VerifyReport> {
        self.last_report.lock().unwrap().clone()
    }
}

/// Detect and run the project's configured test/lint commands, in order:
/// tests first (they find behavioral regressions), then lints.
fn run_checks(config: &VerifyConfig, working_dir: &Path) -> Vec<CheckResult> {
    let info = clawde_tools::detect_project::detect_project_info(working_dir);
    let mut results = Vec::new();
    if config.auto_test {
        if let Some(cmd) = info.test_commands.first() {
            results.push(run_check(
                &format!("test: {cmd}"),
                cmd,
                working_dir,
                config.timeout_secs,
            ));
        }
    }
    if config.auto_lint {
        if let Some(cmd) = info.lint_commands.first() {
            results.push(run_check(
                &format!("lint: {cmd}"),
                cmd,
                working_dir,
                config.timeout_secs,
            ));
        }
    }
    results
}

fn run_check(label: &str, command: &str, working_dir: &Path, timeout_secs: u64) -> CheckResult {
    let (output, code, timed_out) = run_command_sync(command, working_dir, timeout_secs);
    if !timed_out && code == Some(0) {
        CheckResult::pass(label)
    } else if !timed_out && code.is_none() {
        // The command never started (binary missing, spawn error). That is an
        // environment gap, not a code failure — mark it skipped so the loop
        // stops cleanly instead of auto-fixing a missing tool.
        CheckResult::skipped(label, output)
    } else {
        CheckResult::fail(label, output, timed_out)
    }
}

/// Unique log-file counter (per-process) so concurrent verifications cannot
/// collide on the same temp file.
static LOG_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Run `command` to completion in `working_dir`, returning
/// `(stdout+stderr, exit_code, timed_out)`.
///
/// The child's output is redirected to a temp file rather than piped: the
/// parent never reads pipes while the child runs, so a full pipe buffer would
/// block the child forever. When `timeout_secs` elapses, the child is killed
/// and reaped before returning.
pub fn run_command_sync(
    command: &str,
    working_dir: &Path,
    timeout_secs: u64,
) -> (String, Option<i32>, bool) {
    let parts = clawde_tools::run_tests::split_command(command);
    if parts.is_empty() {
        return (String::new(), None, false);
    }

    let log_path = std::env::temp_dir().join(format!(
        "clawde-verify-{}-{}.log",
        std::process::id(),
        LOG_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let file = match std::fs::File::create(&log_path) {
        Ok(f) => f,
        Err(e) => return (format!("Failed to create log file: {e}"), None, false),
    };
    let err_file = match file.try_clone() {
        Ok(f) => f,
        Err(e) => {
            let _ = std::fs::remove_file(&log_path);
            return (format!("Failed to set up log file: {e}"), None, false);
        }
    };

    let mut child = match Command::new(&parts[0])
        .args(&parts[1..])
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(err_file))
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&log_path);
            return (format!("Failed to spawn '{}': {e}", parts[0]), None, false);
        }
    };

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut timed_out = false;
    let exit_status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&log_path);
                return (
                    format!("Failed to wait for '{}': {e}", parts[0]),
                    None,
                    false,
                );
            }
        }
    };

    let output = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_file(&log_path);
    let code = exit_status.and_then(|s| s.code());
    (output, code, timed_out)
}

/// Truncate `output` at a char boundary so a huge failure log cannot blow up
/// the continuation message (or panic on a multi-byte UTF-8 boundary).
fn truncate_output(output: &str, max_chars: usize) -> String {
    if output.chars().count() <= max_chars {
        return output.to_string();
    }
    let cut = output.floor_char_boundary(max_chars);
    format!(
        "{}…\n[output truncated at {max_chars} chars]",
        &output[..cut]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> TurnEndContext<'static> {
        TurnEndContext {
            session_id: "sess",
            total_tokens_used: 0,
            turn_elapsed_secs: 0,
            working_dir: std::path::Path::new("."),
            turn_made_writes: true,
        }
    }

    fn policy(config: VerifyConfig) -> VerifyPolicy {
        VerifyPolicy::new(config, std::path::PathBuf::from("."))
    }

    fn default_config() -> VerifyConfig {
        VerifyConfig {
            enabled: true,
            max_retries: 3,
            sandbox: clawde_core::config::VerifySandbox::Direct,
            auto_lint: true,
            auto_test: true,
            skip_when_no_writes: true,
            timeout_secs: 30,
        }
    }

    fn failing_check() -> CheckResult {
        CheckResult::fail(
            "test: cargo test",
            "2 tests failed\n  - orders::calculate_total",
            false,
        )
    }

    fn passing_check() -> CheckResult {
        CheckResult::pass("test: cargo test")
    }

    #[test]
    fn disabled_config_stops_silently() {
        let mut cfg = default_config();
        cfg.enabled = false;
        let decision = policy(cfg).decide(&ctx());
        assert!(!decision.is_continue());
        match decision {
            ContinuationDecision::Stop { note } => assert!(note.is_none()),
            _ => unreachable!(),
        }
    }

    #[test]
    fn read_only_turns_skip_verification() {
        let cfg = default_config();
        let mut read_ctx = ctx();
        read_ctx.turn_made_writes = false;
        let decision = policy(cfg).decide(&read_ctx);
        assert!(!decision.is_continue());
        match decision {
            ContinuationDecision::Stop { note } => assert!(note.is_none()),
            _ => unreachable!(),
        }
    }

    #[test]
    fn unimplemented_sandbox_reports_clearly() {
        let mut cfg = default_config();
        cfg.sandbox = clawde_core::config::VerifySandbox::Worktree;
        let decision = policy(cfg).decide(&ctx());
        assert!(!decision.is_continue());
        match decision {
            ContinuationDecision::Stop { note } => {
                let note = note.expect("sandbox note must be present");
                assert!(note.contains("not implemented"), "note: {note}");
                assert!(note.contains("worktree"), "note: {note}");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn no_detected_commands_stops_with_note() {
        // Empty temp dir: no config files, so no commands are detected. The
        // policy must stop with a clear note instead of looping forever.
        let dir = tempfile::tempdir().unwrap();
        let cfg = default_config();
        let decision = VerifyPolicy::new(cfg, dir.path().to_path_buf()).decide(&ctx());
        assert!(!decision.is_continue());
        match decision {
            ContinuationDecision::Stop { note } => {
                assert!(note
                    .unwrap_or_default()
                    .contains("No test or lint commands"))
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn failure_continues_until_retries_exhausted() {
        let mut cfg = default_config();
        cfg.max_retries = 2;
        let p = policy(cfg);

        // max_retries=2 allows two auto-fix attempts (verification rounds 1, 2).
        let first = p.decide_with_results(&ctx(), &[failing_check()]);
        match &first {
            ContinuationDecision::Continue { message } => {
                assert!(message.contains("2 tests failed"));
                assert!(message.contains("1/2"), "message: {message}");
            }
            _ => panic!("first failure must continue, got: {first:?}"),
        }

        let second = p.decide_with_results(&ctx(), &[failing_check()]);
        match &second {
            ContinuationDecision::Continue { message } => {
                assert!(message.contains("2/2"), "message: {message}");
            }
            _ => panic!("second failure must continue, got: {second:?}"),
        }

        let third = p.decide_with_results(&ctx(), &[failing_check()]);
        match &third {
            ContinuationDecision::Stop { note } => {
                let note = note.as_deref().expect("exhaustion note must be present");
                assert!(
                    note.contains("Auto-fix exhausted (2 attempts)"),
                    "note: {note}"
                );
                assert!(note.contains("2 tests failed"), "note: {note}");
            }
            _ => panic!("exhausted retries must stop, got: {third:?}"),
        }
    }

    #[test]
    fn spawn_failure_is_skipped_not_a_failure() {
        // A binary that does not exist cannot start — that is an environment
        // gap, so the check is skipped and never triggers auto-fix.
        let result = run_check(
            "test: no-such-binary-xyz",
            "no-such-binary-xyz --flag",
            std::path::Path::new("."),
            5,
        );
        assert!(!result.ok);
        assert!(result.skipped);

        let p = policy(default_config());
        let decision = p.decide_with_results(&ctx(), &[result]);
        match &decision {
            ContinuationDecision::Stop { note } => {
                let note = note.as_deref().expect("note must be present");
                assert!(note.contains("could not run"), "note: {note}");
                assert!(note.contains("no-such-binary-xyz"), "note: {note}");
            }
            _ => panic!("all-skipped must stop cleanly, got: {decision:?}"),
        }
    }

    #[test]
    fn skipped_with_real_failure_is_noted_but_continues() {
        // One real failure + one skipped check: the failure drives the
        // decision, and the skipped check is surfaced as a note.
        let skipped =
            CheckResult::skipped("lint: ruff check .", "Failed to spawn 'ruff': No such file");
        let p = policy(default_config());
        let decision = p.decide_with_results(&ctx(), &[failing_check(), skipped]);
        match &decision {
            ContinuationDecision::Continue { message } => {
                assert!(message.contains("2 tests failed"));
                assert!(message.contains("Skipped (not run)"), "message: {message}");
                assert!(message.contains("ruff"), "message: {message}");
            }
            _ => panic!("real failure must continue, got: {decision:?}"),
        }
    }

    #[test]
    fn verify_report_reflects_last_round_and_clears_on_skip() {
        let cfg = default_config();
        let p = policy(cfg);

        // Nothing ran yet.
        assert!(p.verify_report().is_none());

        // A failing round stashes a structured report (attempt 1/3).
        let decision = p.decide_with_results(&ctx(), &[failing_check()]);
        assert!(decision.is_continue());
        let report = p.verify_report().expect("report after failing round");
        assert_eq!(report.attempt, 1);
        assert_eq!(report.max_retries, 3);
        assert!(
            report.headline.contains("Auto-fix attempt"),
            "{}",
            report.headline
        );
        assert_eq!(report.results.len(), 1);
        assert!(!report.results[0].ok);

        // A passing round overwrites it with the pass headline.
        let decision = p.decide_with_results(&ctx(), &[passing_check()]);
        assert!(!decision.is_continue());
        let report = p.verify_report().expect("report after passing round");
        assert_eq!(report.headline, "All checks passed");
        assert!(report.results[0].ok);

        // A preflight-skipped round (read-only turn) clears the report so a
        // skipped turn never re-displays an old box.
        let mut read_ctx = ctx();
        read_ctx.turn_made_writes = false;
        let decision = p.decide_with_results(&read_ctx, &[failing_check()]);
        assert!(!decision.is_continue());
        assert!(p.verify_report().is_none(), "skip must clear the report");
    }

    #[test]
    fn success_stops_with_checks_passed_note_and_resets_attempts() {
        let cfg = default_config();
        let p = policy(cfg);

        // A passing round must stop with a note (no auto-fix needed).
        let passed = p.decide_with_results(&ctx(), &[passing_check()]);
        match &passed {
            ContinuationDecision::Stop { note } => {
                assert!(note.as_deref().unwrap_or("").contains("All checks passed"))
            }
            _ => panic!("passing checks must stop, got: {passed:?}"),
        }

        // The pass resets the counter, so the next round behaves like attempt
        // 1 again (continue on failure instead of exhausting).
        let again = p.decide_with_results(&ctx(), &[failing_check()]);
        match &again {
            ContinuationDecision::Continue { message } => {
                assert!(message.contains("1/3"), "message: {message}")
            }
            _ => panic!("counter must reset after a pass, got: {again:?}"),
        }
    }

    #[test]
    fn timed_out_check_is_reported_as_failure() {
        let result = run_check(
            "test: sleep 30",
            "sh -c 'sleep 30'",
            std::path::Path::new("."),
            1,
        );
        assert!(!result.ok);
        assert!(result.timed_out);
    }

    #[test]
    fn run_command_sync_captures_output_and_exit_code() {
        let (out, code, timed_out) = run_command_sync(
            "sh -c 'echo hello world; exit 0'",
            std::path::Path::new("."),
            10,
        );
        assert!(!timed_out);
        assert_eq!(code, Some(0));
        assert!(out.contains("hello world"), "out: {out}");

        let (out, code, timed_out) =
            run_command_sync("sh -c 'echo boom; exit 3'", std::path::Path::new("."), 10);
        assert!(!timed_out);
        assert_eq!(code, Some(3));
        assert!(out.contains("boom"), "out: {out}");
    }

    #[test]
    fn run_command_sync_kills_children_on_timeout() {
        let start = std::time::Instant::now();
        let (out, code, timed_out) =
            run_command_sync("sh -c 'sleep 30'", std::path::Path::new("."), 1);
        assert!(timed_out, "must report the timeout");
        assert_eq!(code, None);
        assert!(
            start.elapsed().as_secs() < 10,
            "child must be killed, not waited out"
        );
        assert!(out.is_empty() || !out.contains("elapsed"), "out: {out}");
    }

    #[test]
    fn truncate_output_is_char_boundary_safe() {
        let text = "é".repeat(100);
        let truncated = truncate_output(&text, 50);
        assert!(truncated.contains("truncated at 50 chars"));
        // Slicing must never panic on a multi-byte boundary.
        let _ = &text[..text.floor_char_boundary(50)];
        assert_eq!(truncate_output(&text, 10_000), text);
    }

    fn cargo_available() -> bool {
        Command::new("cargo")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn write_cargo_crate(dir: &std::path::Path, lib_body: &str) {
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"verify-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/lib.rs"), lib_body).unwrap();
    }

    // Real-cargo integration test: the full pipeline — detect the project,
    // run `cargo test`, and produce the right decision. Skips when cargo is
    // unavailable (e.g. a minimal CI image without a Rust toolchain).
    #[test]
    fn verify_loop_passes_and_continues_on_failure() {
        if !cargo_available() {
            eprintln!("skipping: cargo not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();

        // Passing crate → decide stops with "All checks passed".
        write_cargo_crate(dir.path(), "#[cfg(test)]\nmod t { #[test] fn ok() {} }\n");
        let cfg = default_config();
        let decision = VerifyPolicy::new(cfg, dir.path().to_path_buf()).decide(&ctx());
        match &decision {
            ContinuationDecision::Stop { note } => {
                assert!(
                    note.as_deref().unwrap_or("").contains("All checks passed"),
                    "decision: {decision:?}"
                );
            }
            _ => panic!("passing crate must stop with a pass note, got: {decision:?}"),
        }

        // Failing crate → decide continues with the failure summary (attempt 1/3).
        write_cargo_crate(dir.path(), "#[test]\nfn fails() { assert!(false); }\n");
        let cfg = default_config();
        let decision = VerifyPolicy::new(cfg, dir.path().to_path_buf()).decide(&ctx());
        match &decision {
            ContinuationDecision::Continue { message } => {
                assert!(message.contains("1/3"), "message: {message}");
            }
            _ => panic!("failing crate must continue for auto-fix, got: {decision:?}"),
        }
    }
}
