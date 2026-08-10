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

use clawde_core::config::{VerifyConfig, VerifySandbox};

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
    /// Wall-clock seconds the check ran, when the command actually started.
    /// None when the command never spawned or timing was not captured.
    pub elapsed_secs: Option<u64>,
}

impl CheckResult {
    pub(crate) fn pass(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            ok: true,
            output: String::new(),
            timed_out: false,
            skipped: false,
            elapsed_secs: None,
        }
    }

    pub(crate) fn fail(
        label: impl Into<String>,
        output: impl Into<String>,
        timed_out: bool,
    ) -> Self {
        Self {
            label: label.into(),
            ok: false,
            output: truncate_output(&output.into(), 4_000),
            timed_out,
            skipped: false,
            elapsed_secs: None,
        }
    }

    pub(crate) fn skipped(label: impl Into<String>, output: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            ok: false,
            output: truncate_output(&output.into(), 4_000),
            timed_out: false,
            skipped: true,
            elapsed_secs: None,
        }
    }

    /// Attach the measured wall-clock duration to the result.
    pub(crate) fn with_elapsed(mut self, elapsed_secs: u64) -> Self {
        self.elapsed_secs = Some(elapsed_secs);
        self
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
/// Machine-actionable outcome of a verification round.
///
/// This is deliberately independent of any future semantic/model verifier:
/// low-level checks can already distinguish a passing round, a failure that
/// may be fixed within the bounded loop, and a result that needs escalation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyVerdict {
    /// Every executed check passed.
    Pass,
    /// At least one check failed and the bounded auto-fix loop may continue.
    Fixable,
    /// Verification could not establish correctness or the retry budget ended.
    Escalate,
}

/// Structured outcome of one verification round, surfaced to the TUI so it
/// can render the boxed per-check indicator (audit spec §15.1).
#[derive(Debug, Clone)]
pub struct VerifyReport {
    /// Machine-actionable classification of this round.
    pub verdict: VerifyVerdict,
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
    /// Sandbox mode the round ran in (`direct` vs `git worktree`).
    pub sandbox: VerifySandbox,
    /// True when verification could not run because the configured sandbox is
    /// unavailable. This is distinct from a passing round with skipped checks.
    pub unavailable: bool,
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

    fn unavailable_headline(&self) -> String {
        format!(
            "Verification unavailable: sandbox '{}' is not implemented. Set \"verify\": {{\"sandbox\": \"direct\"}} in settings.json.",
            self.config.sandbox.label()
        )
    }

    fn stash_unavailable_report(&self) {
        *self.last_report.lock().unwrap() = Some(VerifyReport {
            verdict: VerifyVerdict::Escalate,
            results: Vec::new(),
            attempt: 0,
            max_retries: self.config.max_retries.max(1),
            headline: self.unavailable_headline(),
            sandbox: self.config.sandbox,
            unavailable: true,
        });
    }

    /// Stash the round's structured report for `verify_report`.
    fn stash_report(
        &self,
        results: &[CheckResult],
        attempt: u32,
        max_retries: u32,
        verdict: VerifyVerdict,
        headline: impl Into<String>,
    ) {
        *self.last_report.lock().unwrap() = Some(VerifyReport {
            verdict,
            results: results.to_vec(),
            attempt,
            max_retries,
            headline: headline.into(),
            sandbox: self.config.sandbox,
            unavailable: false,
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
                note: Some(self.unavailable_headline()),
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
            if !self.config.sandbox.is_implemented() {
                self.stash_unavailable_report();
            } else {
                self.clear_report();
            }
            return decision;
        }
        if results.is_empty() {
            self.stash_report(
                results,
                0,
                self.config.max_retries.max(1),
                VerifyVerdict::Escalate,
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
                VerifyVerdict::Escalate,
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
            let incomplete = !skipped.is_empty();
            let verdict = if incomplete {
                VerifyVerdict::Escalate
            } else {
                VerifyVerdict::Pass
            };
            let headline = if incomplete {
                "Verification incomplete — checks skipped"
            } else {
                "All checks passed"
            };
            self.stash_report(results, attempt, max_retries, verdict, headline);
            return ContinuationDecision::Stop {
                note: Some(format!("{}:\n{}", headline, summary)),
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

        // Audit spec §10.4: when a structured spec exists, its acceptance
        // tests are the precise verification criteria — append them so the
        // model knows EXACTLY what to satisfy, not just "tests failed".
        if let Some((_, spec)) = clawde_core::spec::Spec::latest_in(ctx.working_dir) {
            if !spec.acceptance_tests.is_empty() {
                let criteria = spec
                    .acceptance_tests
                    .iter()
                    .enumerate()
                    .map(|(i, t)| format!("{}. {}", i + 1, t.description))
                    .collect::<Vec<_>>()
                    .join("\n");
                failures_text.push_str(&format!(
                    "\n\nSpec acceptance criteria (spec: \"{}\") — every criterion \
                     must pass:\n{criteria}",
                    spec.title
                ));
            }
        }

        // `attempt` counts verification rounds; the first failing round is
        // auto-fix attempt 1, so up to `max_retries` fix attempts are allowed.
        if attempt <= max_retries {
            self.stash_report(
                results,
                attempt,
                max_retries,
                VerifyVerdict::Fixable,
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
            VerifyVerdict::Escalate,
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
    /// Cheap pre-flight guard: will this round actually spawn checks?
    /// Mirror of [`Self::preflight`] (None there means checks run). The
    /// query loop calls this BEFORE the potentially slow checks start, so the
    /// TUI can show a `verifying…` indicator instead of silent wait.
    fn will_run_checks(&self, ctx: &TurnEndContext<'_>) -> bool {
        self.preflight(ctx).is_none()
    }

    fn decide(&self, ctx: &TurnEndContext<'_>) -> ContinuationDecision {
        if let Some(decision) = self.preflight(ctx) {
            if !self.config.sandbox.is_implemented() {
                self.stash_unavailable_report();
            } else {
                self.clear_report();
            }
            return decision;
        }
        let results = match run_checks(&self.config, &self.working_dir) {
            Ok(results) => results,
            Err(message) => {
                // Sandbox setup failed (e.g. the worktree sandbox needs a git
                // repository): stop with a clear note rather than silently
                // skipping verification or running un-sandboxed.
                self.clear_report();
                return ContinuationDecision::Stop {
                    note: Some(message),
                };
            }
        };
        self.decide_with_results(ctx, &results)
    }

    /// The structured report of the most recent verification round, if one
    /// ran. Consulted by the query loop after `decide` returns.
    fn verify_report(&self) -> Option<VerifyReport> {
        self.last_report.lock().unwrap().clone()
    }
}

/// Run a single verification round on demand (the `/verify` command) and
/// return the structured report for the TUI box. Unlike the continuation
/// policy this never asks the model to fix anything — it is a user-triggered
/// "is the tree green right now?" check.
///
/// Deliberately ignores `verify.enabled`: the auto-loop's off-switch must not
/// block a manual `/verify` (the help text promises it works "after disabling
/// auto-verify"). It only refuses when no checks are configured at all
/// (`auto_test` and `auto_lint` both false).
///
/// Returns `Err` only when the sandbox itself cannot be set up (e.g. the
/// `worktree` sandbox requires a git repository) — never for a failing check.
/// Apply a `/verify` subset argument (`test` / `lint` / `all`) to a config
/// clone. `Err` carries a user-facing message for an unknown argument.
///
/// Single source of truth shared by the `/verify` command's `execute` and the
/// CLI's async dispatch, so the subset parsing can never diverge between the
/// two paths.
pub fn apply_verify_subset(config: &mut VerifyConfig, args: &str) -> Result<(), String> {
    match args.trim() {
        "" | "all" => Ok(()),
        "test" => {
            config.auto_lint = false;
            Ok(())
        }
        "lint" => {
            config.auto_test = false;
            Ok(())
        }
        other => Err(format!(
            "Unknown /verify argument '{other}' — use test, lint, or all"
        )),
    }
}

pub fn run_verify_round(config: &VerifyConfig, working_dir: &Path) -> Result<VerifyReport, String> {
    if !config.auto_test && !config.auto_lint {
        return Ok(VerifyReport {
            verdict: VerifyVerdict::Escalate,
            results: Vec::new(),
            attempt: 0,
            max_retries: config.max_retries.max(1),
            headline: "No checks configured (verify.auto_test / auto_lint)".to_string(),
            sandbox: config.sandbox,
            unavailable: false,
        });
    }
    let results = run_checks(config, working_dir)?;

    // Audit spec §9.5 trigger 1: persist the detected test/lint commands into
    // the project's `conventions.md` so future sessions know how to build and
    // verify without re-discovery. Gated on project memory already existing
    // (i.e. the user opted in via `/memory init` or auto-dream) — verification
    // alone never creates the memory system, keeping zero-footprint for
    // projects without memory.
    {
        use clawde_core::memdir::{
            auto_memory_path, is_auto_memory_enabled, record_verify_conventions,
        };
        // Honor the settings toggle (`config.memory.autoMemoryEnabled`):
        // when the user disabled the memory system, the conventions recording
        // stops too. Loaded from Settings because `run_verify_round` only
        // receives the VerifyConfig slice.
        let memory_enabled = clawde_core::config::Settings::load_sync()
            .ok()
            .and_then(|s| s.config.memory.enabled);
        let memory_dir = auto_memory_path(working_dir);
        if is_auto_memory_enabled(memory_enabled) && memory_dir.is_dir() {
            let info = clawde_tools::detect_project::detect_project_info(working_dir);
            record_verify_conventions(
                &memory_dir,
                info.test_commands.first().map(String::as_str),
                info.lint_commands.first().map(String::as_str),
            );
        }
    }

    let max_retries = config.max_retries.max(1);
    if results.is_empty() {
        return Ok(VerifyReport {
            verdict: VerifyVerdict::Escalate,
            results,
            attempt: 0,
            max_retries,
            headline: "No test or lint commands detected".to_string(),
            sandbox: config.sandbox,
            unavailable: false,
        });
    }
    let failures: Vec<&CheckResult> = results.iter().filter(|r| !r.ok && !r.skipped).collect();
    let skipped = results.iter().any(|r| r.skipped);
    let headline = if results.iter().all(|r| r.skipped) {
        "Verification could not run — commands missing".to_string()
    } else if failures.is_empty() && skipped {
        "Verification incomplete — checks skipped".to_string()
    } else if failures.is_empty() {
        "All checks passed".to_string()
    } else {
        format!("{} check(s) failed", failures.len())
    };
    Ok(VerifyReport {
        verdict: if !failures.is_empty() {
            VerifyVerdict::Fixable
        } else if skipped {
            VerifyVerdict::Escalate
        } else {
            VerifyVerdict::Pass
        },
        results,
        attempt: 1,
        max_retries,
        headline,
        sandbox: config.sandbox,
        unavailable: false,
    })
}

/// Detect and run the project's configured test/lint commands inside the
/// configured sandbox, in order: tests first (they find behavioral
/// regressions), then lints.
///
/// Returns `Err` only when the sandbox itself cannot be set up (e.g. the
/// `worktree` sandbox requires a git repository) — never for a failing check.
fn run_checks(config: &VerifyConfig, working_dir: &Path) -> Result<Vec<CheckResult>, String> {
    match config.sandbox {
        VerifySandbox::Direct => Ok(run_checks_direct(config, working_dir)),
        VerifySandbox::Worktree => {
            crate::verify_sandbox::run_checks_in_worktree(config, working_dir)
        }
        VerifySandbox::Container => {
            crate::verify_container::run_checks_in_container(config, working_dir)
        }
    }
}

/// Detect and run the project's configured test/lint commands directly in
/// `working_dir`, in order: tests first, then lints.
pub(crate) fn run_checks_direct(config: &VerifyConfig, working_dir: &Path) -> Vec<CheckResult> {
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
    let (output, code, timed_out, elapsed) = run_command_sync(command, working_dir, timeout_secs);
    let result = if !timed_out && code == Some(0) {
        CheckResult::pass(label)
    } else if !timed_out && code.is_none() {
        // The command never started (binary missing, spawn error). That is an
        // environment gap, not a code failure — mark it skipped so the loop
        // stops cleanly instead of auto-fixing a missing tool.
        CheckResult::skipped(label, output)
    } else {
        CheckResult::fail(label, output, timed_out)
    };
    // Only attach a duration when the command actually started (spawn or
    // timeout); a skipped check never ran, so it shows no timing.
    if timed_out || code.is_some() {
        result.with_elapsed(elapsed)
    } else {
        result
    }
}

/// Unique log-file counter (per-process) so concurrent verifications cannot
/// collide on the same temp file.
static LOG_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Run `command` to completion in `working_dir`, returning
/// `(stdout+stderr, exit_code, timed_out, elapsed_secs)`.
///
/// The child's output is redirected to a temp file rather than piped: the
/// parent never reads pipes while the child runs, so a full pipe buffer would
/// block the child forever. When `timeout_secs` elapses, the child is killed
/// and reaped before returning.
pub fn run_command_sync(
    command: &str,
    working_dir: &Path,
    timeout_secs: u64,
) -> (String, Option<i32>, bool, u64) {
    let parts = clawde_tools::run_tests::split_command(command);
    if parts.is_empty() {
        return (String::new(), None, false, 0);
    }
    run_argv_sync(&parts, working_dir, timeout_secs)
}

/// Run `argv` to completion in `working_dir`, returning
/// `(stdout+stderr, exit_code, timed_out)`.
///
/// The argv form exists so sandbox modes can invoke commands with arguments
/// that must not be shell-split (e.g. a `docker run` line carrying a mount
/// path with spaces); `run_command_sync` is the string-splitting wrapper over
/// this same core.
pub fn run_argv_sync(
    parts: &[String],
    working_dir: &Path,
    timeout_secs: u64,
) -> (String, Option<i32>, bool, u64) {
    if parts.is_empty() {
        return (String::new(), None, false, 0);
    }
    let start = std::time::Instant::now();

    let log_path = std::env::temp_dir().join(format!(
        "clawde-verify-{}-{}.log",
        std::process::id(),
        LOG_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let file = match std::fs::File::create(&log_path) {
        Ok(f) => f,
        Err(e) => return (format!("Failed to create log file: {e}"), None, false, 0),
    };
    let err_file = match file.try_clone() {
        Ok(f) => f,
        Err(e) => {
            let _ = std::fs::remove_file(&log_path);
            return (format!("Failed to set up log file: {e}"), None, false, 0);
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
            return (
                format!("Failed to spawn '{}': {e}", parts[0]),
                None,
                false,
                0,
            );
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
                    0,
                );
            }
        }
    };

    let output = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_file(&log_path);
    let code = exit_status.and_then(|s| s.code());
    (output, code, timed_out, start.elapsed().as_secs())
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
            turn_output_tokens: 0,
            changed_files: None,
            changed_diff: None,
            spec: None,
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
            container_image: None,
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
    fn apply_verify_subset_selects_checks() {
        let full = default_config();

        // Default / all → both checks.
        let mut cfg = full.clone();
        apply_verify_subset(&mut cfg, "").unwrap();
        assert!(cfg.auto_test && cfg.auto_lint);
        apply_verify_subset(&mut cfg, "all").unwrap();
        assert!(cfg.auto_test && cfg.auto_lint);

        // test → lints off; lint → tests off.
        let mut cfg = full.clone();
        apply_verify_subset(&mut cfg, "test").unwrap();
        assert!(cfg.auto_test && !cfg.auto_lint);
        let mut cfg = full.clone();
        apply_verify_subset(&mut cfg, "lint").unwrap();
        assert!(!cfg.auto_test && cfg.auto_lint);

        // Unknown arg → user-facing error.
        let mut cfg = full.clone();
        let err = apply_verify_subset(&mut cfg, "bogus").unwrap_err();
        assert!(err.contains("Unknown /verify argument 'bogus'"));
        // Failed parse must not have mutated the config.
        assert!(cfg.auto_test && cfg.auto_lint);
    }

    #[test]
    fn will_run_checks_mirrors_preflight_guards() {
        let cfg = default_config();
        let p = policy(cfg.clone());
        let write_ctx = ctx();
        assert!(write_ctx.turn_made_writes);
        assert!(p.will_run_checks(&write_ctx));

        // Disabled -> checks never spawn.
        let mut off = cfg.clone();
        off.enabled = false;
        assert!(!policy(off).will_run_checks(&write_ctx));

        // Read-only turn -> checks never spawn.
        let mut read_ctx = ctx();
        read_ctx.turn_made_writes = false;
        assert!(!p.will_run_checks(&read_ctx));

        // No checks configured -> checks never spawn.
        let mut none = cfg.clone();
        none.auto_test = false;
        none.auto_lint = false;
        assert!(!policy(none).will_run_checks(&write_ctx));
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
        assert_eq!(p.verify_report().unwrap().verdict, VerifyVerdict::Fixable);
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
        assert_eq!(p.verify_report().unwrap().verdict, VerifyVerdict::Escalate);
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
    fn spec_acceptance_criteria_appended_to_failure_feedback() {
        // Audit spec §10.4: with a spec present, the auto-fix message carries
        // the acceptance criteria verbatim so the model knows the target.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("specs")).unwrap();
        let spec = clawde_core::spec::Spec {
            title: "Rate-Limiting Middleware".to_string(),
            acceptance_tests: vec![
                clawde_core::spec::AcceptanceTest {
                    description: "Requests under limit pass through".to_string(),
                },
                clawde_core::spec::AcceptanceTest {
                    description: "Requests over limit return 429".to_string(),
                },
            ],
            ..Default::default()
        };
        spec.write_to(&dir.path().join("specs/rate-limiting.json"))
            .unwrap();

        let ctx = TurnEndContext {
            session_id: "sess",
            total_tokens_used: 0,
            turn_elapsed_secs: 0,
            working_dir: dir.path(),
            turn_made_writes: true,
            turn_output_tokens: 0,
            changed_files: None,
            changed_diff: None,
            spec: None,
        };
        let p = VerifyPolicy::new(default_config(), dir.path().to_path_buf());
        let decision = p.decide_with_results(&ctx, &[failing_check()]);
        match &decision {
            ContinuationDecision::Continue { message } => {
                assert!(
                    message.contains("Spec acceptance criteria"),
                    "message: {message}"
                );
                assert!(message.contains("Rate-Limiting Middleware"));
                assert!(message.contains("Requests under limit pass through"));
                assert!(message.contains("Requests over limit return 429"));
                assert!(message.contains("every criterion must pass"));
            }
            _ => panic!("failure must continue for auto-fix, got: {decision:?}"),
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
    fn mixed_pass_and_skipped_escalates_incomplete_verification() {
        let skipped =
            CheckResult::skipped("lint: ruff check .", "Failed to spawn 'ruff': No such file");
        let p = policy(default_config());
        let decision = p.decide_with_results(&ctx(), &[passing_check(), skipped]);
        match &decision {
            ContinuationDecision::Stop { note } => {
                let note = note.as_deref().unwrap_or_default();
                assert!(note.contains("Verification incomplete"), "note: {note}");
                assert!(note.contains("SKIPPED"), "note: {note}");
            }
            _ => panic!("incomplete verification must stop, got: {decision:?}"),
        }
        assert_eq!(p.verify_report().unwrap().verdict, VerifyVerdict::Escalate);
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
        assert_eq!(p.verify_report().unwrap().verdict, VerifyVerdict::Pass);
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
        // A timed-out check ran for the full timeout, so timing is attached.
        assert!(result.elapsed_secs.is_some());
    }

    #[test]
    fn run_command_sync_captures_output_and_exit_code() {
        let (out, code, timed_out, _elapsed) = run_command_sync(
            "sh -c 'echo hello world; exit 0'",
            std::path::Path::new("."),
            10,
        );
        assert!(!timed_out);
        assert_eq!(code, Some(0));
        assert!(out.contains("hello world"), "out: {out}");

        let (out, code, timed_out, elapsed) =
            run_command_sync("sh -c 'echo boom; exit 3'", std::path::Path::new("."), 10);
        assert!(!timed_out);
        assert_eq!(code, Some(3));
        assert!(out.contains("boom"), "out: {out}");
        // Sub-second runs legitimately round down to 0 — the value must simply
        // be present (the timeout path below proves it measures real time).
        let _ = elapsed;
    }

    #[test]
    fn run_command_sync_kills_children_on_timeout() {
        let start = std::time::Instant::now();
        let (out, code, timed_out, elapsed) =
            run_command_sync("sh -c 'sleep 30'", std::path::Path::new("."), 1);
        assert!(timed_out, "must report the timeout");
        assert_eq!(code, None);
        assert!(
            start.elapsed().as_secs() < 10,
            "child must be killed, not waited out"
        );
        assert!(out.is_empty() || !out.contains("elapsed"), "out: {out}");
        assert!(elapsed > 0, "timeout run must report a duration");
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

    // Real-cargo test of the one-shot `/verify` round: report carries the
    // right headline and per-check results without any continuation logic.
    #[test]
    fn run_verify_round_reports_pass_and_fail_headlines() {
        if !cargo_available() {
            eprintln!("skipping: cargo not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();

        // Passing crate → "All checks passed".
        write_cargo_crate(dir.path(), "#[cfg(test)]\nmod t { #[test] fn ok() {} }\n");
        let report = run_verify_round(&default_config(), dir.path()).unwrap();
        assert_eq!(report.verdict, VerifyVerdict::Pass);
        assert_eq!(report.headline, "All checks passed");
        assert!(report.results.iter().all(|r| r.ok));
        assert_eq!(report.attempt, 1);
        assert_eq!(report.sandbox, default_config().sandbox);

        // Failing crate → "N check(s) failed" with the failure flagged.
        write_cargo_crate(dir.path(), "#[test]\nfn fails() { assert!(false); }\n");
        let report = run_verify_round(&default_config(), dir.path()).unwrap();
        // Both the test and lint checks fail on a broken crate; assert the
        // headline shape and that at least one real failure was flagged.
        assert_eq!(report.verdict, VerifyVerdict::Fixable);
        assert!(
            report.headline.ends_with("check(s) failed"),
            "headline: {}",
            report.headline
        );
        assert!(report.results.iter().any(|r| !r.ok && !r.skipped));

        // `enabled: false` must NOT block a manual /verify — only the
        // auto-loop's preflight honours the off-switch. The round still runs.
        write_cargo_crate(dir.path(), "#[cfg(test)]\nmod t { #[test] fn ok() {} }\n");
        let manual = VerifyConfig {
            enabled: false,
            ..default_config()
        };
        let report = run_verify_round(&manual, dir.path()).unwrap();
        assert_eq!(report.headline, "All checks passed");
        assert!(!report.results.is_empty());
        assert_eq!(report.attempt, 1);

        // Both checks off → no round, clear headline (the failing crate from
        // the previous block is still in the dir — irrelevant, nothing runs).
        let none = VerifyConfig {
            auto_test: false,
            auto_lint: false,
            ..default_config()
        };
        let report = run_verify_round(&none, dir.path()).unwrap();
        assert_eq!(
            report.headline,
            "No checks configured (verify.auto_test / auto_lint)"
        );
        assert!(report.results.is_empty());
        assert_eq!(report.attempt, 0);
    }
}
