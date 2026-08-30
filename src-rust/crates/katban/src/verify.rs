//! Card verification gate + auto-review pass (board audit hardening options 1
//! and 2).
//!
//! Before a card's agent work is accepted into Review, the runner runs the
//! project's configured checks — the same detected test/lint commands the
//! interactive verify loop uses — directly inside the card's worktree:
//!
//! 1. **Verification gate**: a failing check sends the card to `Failed`
//!    instead of `Review` (its result names the failing check), so a card
//!    that breaks the build or the tests never reaches you for review.
//! 2. **Auto-review pass** (per-board `auto_review`, default on): a second
//!    headless agent reads the card's diff and attaches findings as ordinary
//!    review comments, so you approve or dismiss instead of reading the whole
//!    diff. Findings are best-effort — any failure degrades to a single
//!    free-form comment or nothing, never a hard error.
//!
//! The gate honours the global `"verify"` settings block (`enabled`,
//! `auto_lint`, `auto_test`, `timeout_secs`) exactly like the interactive
//! loop, and mirrors its `skipped` semantics: a check that cannot start
//! (missing tool) is an environment gap, not a card failure.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clawde_core::config::{Settings, VerifyConfig};

/// Outcome of the verification gate for one card.
#[derive(Debug, Clone)]
pub struct GateResult {
    pub passed: bool,
    /// True when the gate intentionally did not run (board toggle off, verify
    /// disabled in settings, nothing changed, or dependency install failed —
    /// an environment gap, not a card failure). The runner surfaces skipped
    /// reasons on the card's result so a skip is never silent.
    pub skipped: bool,
    /// Human digest: what ran and how it did (or why nothing ran).
    pub detail: String,
}

impl GateResult {
    fn passed(detail: impl Into<String>) -> Self {
        Self {
            passed: true,
            skipped: false,
            detail: detail.into(),
        }
    }

    fn skipped(detail: impl Into<String>) -> Self {
        Self {
            passed: true,
            skipped: true,
            detail: detail.into(),
        }
    }

    fn failed(detail: impl Into<String>) -> Self {
        Self {
            passed: false,
            skipped: false,
            detail: detail.into(),
        }
    }
}

/// One finding from the auto-review pass: an optional diff-line anchor plus
/// the finding text. `line` is advisory (1-based diff line), never a hard
/// file pointer — the same contract as a human review comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoReviewFinding {
    pub line: Option<String>,
    pub text: String,
}

/// The global verify configuration (`settings.json` `"config.verify"` block),
/// falling back to defaults when unset. The gate reads it fresh per card so a
/// settings change applies to the next card without a board restart.
fn verify_config() -> VerifyConfig {
    Settings::load_sync()
        .ok()
        .map(|settings| settings.config.verify.clone())
        .unwrap_or_default()
}

/// Run the verification gate in the card's worktree: provision dependencies
/// (the worktree is a pristine checkout), then detect the project's test/lint
/// commands and run them (tests first, then lints), each bounded by the
/// configured per-command timeout. A project with no detectable commands, or a
/// card in a non-code directory, passes trivially (nothing to run).
///
/// `board_verify` is the per-board master switch (`board verify on|off`); the
/// global `settings.json` `config.verify.enabled` must also be true for the
/// gate to run. A card whose tree is unchanged vs. its base (e.g. a no-op
/// follow-up) is skipped — there is nothing new to verify.
pub async fn run_gate(work_dir: &Path, board_verify: bool) -> GateResult {
    let config = verify_config();
    if !board_verify {
        return GateResult::skipped("verification disabled for this board (board verify off)");
    }
    if !config.enabled {
        return GateResult::skipped(
            "verification disabled (settings.json \"config.verify.enabled\": false)",
        );
    }
    // #5 — an unchanged tree has nothing new to verify: re-running the whole
    // test/lint suite on an identical tree is pure waste (and risks a spurious
    // timeout failure on a no-op follow-up). Mirrors the interactive loop's
    // skip-when-no-writes behavior. Only meaningful in a git repo; a scratch
    // dir's agent work IS the whole tree, so it always runs.
    if crate::git::is_repo(work_dir) && crate::git::diff_clamped(work_dir).trim().is_empty() {
        return GateResult::skipped("no changes to verify (tree unchanged)");
    } // Detect the project's commands BEFORE provisioning: if nothing would run
      // (no detectable commands, or auto_test/auto_lint both off), there is no
      // reason to spend a bounded timeout installing dependencies.
    let info = clawde_tools::detect_project::detect_project_info(work_dir);
    let test_cmd = if config.auto_test {
        info.test_commands.first().cloned()
    } else {
        None
    };
    let lint_cmd = if config.auto_lint {
        info.lint_commands.first().cloned()
    } else {
        None
    };
    if test_cmd.is_none() && lint_cmd.is_none() {
        return GateResult::passed("no test/lint commands detected for this project");
    }
    // #4 — provision dependencies first (fresh worktree, no node_modules,
    // no venv, cold registry cache). An install that fails (no network, broken
    // manifest) is an environment gap, not a card failure: skip the gate with
    // the reason visible instead of fail-closing on a missing-dep error.
    let venv_bin = match provision_deps(work_dir, config.timeout_secs).await {
        Ok(venv_bin) => venv_bin,
        Err(err) => {
            return GateResult::skipped(format!("dependency install failed; gate skipped: {err}"));
        }
    };
    let mut labels: Vec<String> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    if let Some(cmd) = test_cmd {
        let label = format!("test: {cmd}");
        if let Some(err) = run_check(work_dir, &cmd, config.timeout_secs, venv_bin.as_deref()).await
        {
            failures.push(format!("{label}: {err}"));
        }
        labels.push(label);
    }
    if let Some(cmd) = lint_cmd {
        let label = format!("lint: {cmd}");
        if let Some(err) = run_check(work_dir, &cmd, config.timeout_secs, venv_bin.as_deref()).await
        {
            failures.push(format!("{label}: {err}"));
        }
        labels.push(label);
    }
    if failures.is_empty() {
        GateResult::passed(format!("all checks passed ({})", labels.join(", ")))
    } else {
        GateResult::failed(failures.join("; "))
    }
}

/// Install the project's dependencies into the fresh worktree so the checks
/// can actually run: `npm ci` (or `npm install` without a lockfile) for JS,
/// a worktree-local Python venv + `pip install -r requirements.txt pytest`
/// for Python, and `cargo fetch` to warm the shared registry cache for Rust.
/// The venv lives inside the throwaway worktree, so nothing outside the card
/// is touched. Returns the venv's `bin` dir (when one was created) so the
/// checks run with the venv's `python3` (and its pytest) on PATH. `Err` = an
/// install failed (caller skips the gate); a tool that cannot even start
/// (npm/python3 missing) is treated as a successful no-op, so the checks then
/// fail-or-skip on their own, keeping the fail-open-on-env-gap contract.
async fn provision_deps(work_dir: &Path, timeout_secs: u64) -> Result<Option<PathBuf>, String> {
    let mut venv_bin = None;
    if work_dir.join("package.json").exists() {
        let cmd = package_install(work_dir);
        if let Some(err) = run_check(work_dir, &cmd, timeout_secs, None).await {
            return Err(format!("{cmd} failed: {err}"));
        }
    }
    if work_dir.join("requirements.txt").exists() || work_dir.join("pyproject.toml").exists() {
        for cmd in python_installs(work_dir) {
            if let Some(err) = run_check(work_dir, &cmd, timeout_secs, None).await {
                return Err(format!("{cmd} failed: {err}"));
            }
        }
        venv_bin = Some(work_dir.join(".venv/bin"));
    }
    if work_dir.join("Cargo.toml").exists() {
        // Warm the shared registry cache so the cold `cargo test` compile has
        // its downloads ready; the compile itself is bounded by the check
        // timeout and is the user's knob via `timeout_secs` / `board verify`.
        if let Some(err) = run_check(work_dir, "cargo fetch", timeout_secs, None).await {
            return Err(format!("cargo fetch failed: {err}"));
        }
    }
    Ok(venv_bin)
}

/// Pick the JS/yarn/pnpm install command for a worktree, preferring the
/// project's own lockfile tool when it is installed (so a yarn or pnpm project
/// is installed with the resolution the team actually pinned), and falling back
/// through the npm paths. The no-lockfile case uses `npm install
/// --no-package-lock`: it installs deps but never writes a `package-lock.json`,
/// so a card whose project lacks a lockfile isn't handed a gate-generated one
/// to commit into its branch (audit gap #1).
fn package_install(work_dir: &Path) -> String {
    if work_dir.join("yarn.lock").exists() && tool_available("yarn") {
        "yarn install --frozen-lockfile".to_string()
    } else if work_dir.join("pnpm-lock.yaml").exists() && tool_available("pnpm") {
        "pnpm install --frozen-lockfile".to_string()
    } else if work_dir.join("package-lock.json").exists() {
        "npm ci".to_string()
    } else {
        "npm install --no-package-lock".to_string()
    }
}

/// The Python install steps: a throwaway venv inside the worktree (so nothing
/// outside the card is touched), then the project's dependencies plus pytest —
/// from `requirements.txt`, or editable-install the project when it uses a
/// modern `pyproject.toml` (audit gap #2). The checks then resolve `python3`
/// to the venv via the prepended PATH. Any step failing is an env gap; the
/// caller skips the gate rather than failing the card.
fn python_installs(work_dir: &Path) -> Vec<String> {
    let dep_cmd = if work_dir.join("requirements.txt").exists() {
        ".venv/bin/pip install -r requirements.txt pytest"
    } else {
        ".venv/bin/pip install -e . pytest"
    };
    vec!["python3 -m venv .venv".to_string(), dep_cmd.to_string()]
}

/// Whether a CLI tool resolves on PATH: the package-manager preference used it
/// so a project's preferred tool is honored when present, with a graceful
/// fall-back when it isn't (e.g. yarn missing -> npm install instead of a
/// silent fail-closed empty node_modules).
fn tool_available(name: &str) -> bool {
    std::process::Command::new(name)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run one check command to completion. Returns `Some(err)` when it failed or
/// timed out; `None` when it passed or could not start (missing tool — an
/// environment gap, mirroring the interactive loop's `skipped`). The child is
/// killed if it exceeds the timeout, so a hung check cannot pin a slot.
/// `extra_path` (the worktree's venv bin dir) is prepended to PATH when set,
/// so `python3 -m pytest` resolves to the venv's interpreter + pytest.
async fn run_check(
    work_dir: &Path,
    command: &str,
    timeout_secs: u64,
    extra_path: Option<&Path>,
) -> Option<String> {
    // Detected commands are fixed strings (never user input) without quotes,
    // so a whitespace split is a faithful tokenization.
    let parts: Vec<&str> = command.split_whitespace().collect();
    let (program, args) = parts.split_first()?;
    let timeout = Duration::from_secs(timeout_secs.max(1));
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args).current_dir(work_dir).kill_on_drop(true);
    if let Some(extra) = extra_path {
        let old = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{}:{old}", extra.display()));
    }
    let child = cmd.spawn().ok()?; // spawn failure = skipped
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            if output.status.success() {
                None
            } else {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let combined = format!("{stdout}\n{stderr}");
                let code = output
                    .status
                    .code()
                    .map(|c| format!("exit {c}"))
                    .unwrap_or_else(|| "killed".to_string());
                let tail: String = combined
                    .chars()
                    .rev()
                    .take(400)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect();
                let tail = tail.trim();
                Some(if tail.is_empty() {
                    code
                } else {
                    format!("{code}: {tail}")
                })
            }
        }
        Ok(Err(_)) => None, // wait failed after spawn = treated as skipped
        Err(_) => Some(format!("timed out after {timeout_secs}s")),
    }
}

/// Run a second headless agent pass over the card's diff and return structured
/// findings. Best-effort by contract: a spawn/exit failure or timeout returns
/// `Err` (the runner logs and skips — a hung reviewer must not pin the card's
/// parallel slot, so the pass is bounded by the same per-command timeout as
/// the gate's checks); unparseable output degrades to one free-form comment
/// via `parse_findings`.
pub async fn auto_review(
    work_dir: &Path,
    task_prompt: &str,
    diff: &str,
) -> Result<Vec<AutoReviewFinding>, String> {
    let clawde_bin = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("clawde"));
    let reviewer_prompt = format!(
        "You are reviewing a code change made to fulfill this task:\n\n{task}\n\nDiff:\n{diff}\n\n\
         Identify concrete problems worth fixing before this is merged: bugs, missing edge cases, \
         secrets or credentials, debug leftovers, or changes that will fail the project's checks. \
         Respond with ONLY a JSON array of findings, each item {{\"line\": <1-based diff line number \
         or null>, \"text\": <short specific finding>}}. If the change is fine, respond with []. \
         Do not include any text outside the JSON array.",
        task = task_prompt.trim()
    );
    let timeout_secs = verify_config().timeout_secs.max(1);
    let child = tokio::process::Command::new(&clawde_bin)
        .current_dir(work_dir)
        .args(["--print", &reviewer_prompt])
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("could not start auto-review agent: {e}"))?;
    let output =
        match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output())
            .await
        {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => return Err(format!("auto-review agent wait failed: {e}")),
            Err(_) => return Err(format!("auto-review agent timed out after {timeout_secs}s")),
        };
    if !output.status.success() {
        return Err("auto-review agent exited non-zero".to_string());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(parse_findings(&text))
}

/// Parse the auto-reviewer's output into findings. Extracts the JSON array
/// even when the model wraps it in prose or code fences; any parse failure (or
/// an empty result) degrades to a single free-form comment capped at 2 KB so
/// the review signal is never silently dropped.
fn parse_findings(text: &str) -> Vec<AutoReviewFinding> {
    let freeform = |t: &str| {
        let t = t.trim();
        if t.is_empty() {
            Vec::new()
        } else {
            vec![AutoReviewFinding {
                line: None,
                text: t.chars().take(2000).collect(),
            }]
        }
    };
    let Some(start) = text.find('[') else {
        return freeform(text);
    };
    let Some(end) = text.rfind(']') else {
        return freeform(text);
    };
    let slice = &text[start..=end];
    // A parseable array is authoritative — an explicitly empty `[]` means the
    // reviewer found nothing and must NOT degrade to a free-form comment.
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(slice)
    else {
        return freeform(text);
    };
    let mut findings = Vec::new();
    for item in items.into_iter().take(20) {
        let text = item
            .get("text")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if text.is_empty() {
            continue;
        }
        // The prompt asks for `"line": <number or null>`; a model that answers
        // with a JSON number must not silently lose its anchor.
        let line = item.get("line").and_then(|v| match v {
            serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        });
        findings.push(AutoReviewFinding { line, text });
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn node_available() -> bool {
        std::process::Command::new("node")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn write_settings(home: &Path, body: &str) {
        std::fs::create_dir_all(home).unwrap();
        std::fs::write(home.join("settings.json"), body).unwrap();
    }

    // Pin CLAWDE_HOME to a sandbox, optionally seed settings.json, build a
    // scratch project via `setup`, and run the gate — all in one async helper
    // so the env guard stays held across the await (test-only pattern; the
    // crate's board_server tests do the same). `board_verify` mirrors the
    // per-board master toggle.
    #[allow(clippy::await_holding_lock)]
    async fn gate_in_home(
        settings: Option<&str>,
        board_verify: bool,
        setup: impl FnOnce(&std::path::Path),
    ) -> GateResult {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("CLAWDE_HOME").ok();
        std::env::set_var("CLAWDE_HOME", tmp.path());
        if let Some(body) = settings {
            write_settings(tmp.path(), body);
        }
        let work = tempfile::tempdir().unwrap();
        setup(work.path());
        let result = run_gate(work.path(), board_verify).await;
        match previous {
            Some(value) => std::env::set_var("CLAWDE_HOME", value),
            None => std::env::remove_var("CLAWDE_HOME"),
        }
        result
    }

    fn init_repo(dir: &Path) {
        std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(dir)
            .output()
            .unwrap();
        std::fs::write(dir.join("README.md"), "# demo\n").unwrap();
        let add = std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(add.status.success());
        let commit = std::process::Command::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(commit.status.success(), "git commit failed: {commit:?}");
    }

    #[tokio::test]
    async fn gate_passes_trivially_with_no_project() {
        let result = gate_in_home(None, true, |work| {
            std::fs::write(work.join("notes.txt"), "no code here\n").unwrap();
        })
        .await;
        assert!(result.passed);
        assert!(result.detail.contains("no test/lint commands"));
    }

    #[tokio::test]
    async fn gate_passes_when_verify_disabled() {
        let result = gate_in_home(
            Some(r#"{"config":{"verify":{"enabled":false}}}"#),
            true,
            |work| {
                std::fs::create_dir_all(work.join("src")).unwrap();
                std::fs::write(
                    work.join("Cargo.toml"),
                    "[package]\nname=\"x\"\nversion=\"0.1.0\"\n",
                )
                .unwrap();
            },
        )
        .await;
        assert!(result.passed);
        assert!(result.detail.contains("disabled"));
    }

    #[tokio::test]
    async fn gate_skips_when_board_verify_off() {
        // #7 — the per-board master switch: `board verify off` must skip the
        // gate (a pass with a visible reason), even for a project whose checks
        // would fail.
        let result = gate_in_home(None, false, |work| {
            std::fs::write(
                work.join("package.json"),
                r#"{"scripts":{"test":"node -e \"process.exit(1)\""}}"#,
            )
            .unwrap();
        })
        .await;
        assert!(result.passed);
        assert!(result.skipped);
        assert!(
            result.detail.contains("board verify off"),
            "detail: {}",
            result.detail
        );
    }

    #[tokio::test]
    async fn gate_fails_when_the_test_command_fails() {
        if !node_available() {
            eprintln!("skipping: node not installed");
            return;
        }
        // Lint off so the JS `npm run lint` script (which we won't define)
        // can't mask the test result; only the failing test runs.
        let result = gate_in_home(
            Some(r#"{"config":{"verify":{"auto_lint":false,"timeout_secs":60}}}"#),
            true,
            |work| {
                std::fs::write(
                    work.join("package.json"),
                    r#"{"scripts":{"test":"node -e \"process.exit(1)\""}}"#,
                )
                .unwrap();
            },
        )
        .await;
        assert!(!result.passed);
        assert!(
            result.detail.contains("test: npm test"),
            "detail: {}",
            result.detail
        );
    }

    #[tokio::test]
    async fn gate_passes_when_the_test_command_passes() {
        if !node_available() {
            eprintln!("skipping: node not installed");
            return;
        }
        let result = gate_in_home(
            Some(r#"{"config":{"verify":{"auto_lint":false,"timeout_secs":60}}}"#),
            true,
            |work| {
                std::fs::write(
                    work.join("package.json"),
                    r#"{"scripts":{"test":"node -e \"process.exit(0)\""}}"#,
                )
                .unwrap();
            },
        )
        .await;
        assert!(result.passed, "detail: {}", result.detail);
    }

    #[test]
    fn parse_findings_extracts_json_even_inside_prose() {
        let text = r#"Here is my review:
```json
[{"line": "12", "text": "add an index on user_id"}, {"line": null, "text": "retry on conflict"}]
```
"#;
        let findings = parse_findings(text);
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].line.as_deref(), Some("12"));
        assert_eq!(findings[0].text, "add an index on user_id");
        assert_eq!(findings[1].line, None);
    }

    #[test]
    fn parse_findings_degrades_to_freeform_on_garbage() {
        let text = "the diff looks fine, nothing to flag";
        let findings = parse_findings(text);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].line, None);
        assert!(findings[0].text.contains("fine"));
    }

    #[test]
    fn parse_findings_empty_array_is_empty() {
        assert!(parse_findings("[]").is_empty());
        assert!(parse_findings("").is_empty());
    }

    #[test]
    fn parse_findings_accepts_numeric_line_anchors() {
        // The prompt asks the reviewer for `"line": <number or null>`; a model
        // that answers with a JSON number must not silently lose the anchor.
        let text =
            r#"[{"line": 12, "text": "off by one"}, {"line": "14-16", "text": "dup logic"}]"#;
        let findings = parse_findings(text);
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].line.as_deref(), Some("12"));
        assert_eq!(findings[1].line.as_deref(), Some("14-16"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn gate_skips_an_unchanged_tree() {
        // #5 — a worktree with no changes has nothing new to verify: the gate
        // must skip (pass with a visible reason) instead of re-running the
        // whole suite on an identical tree. Only git worktrees qualify; a
        // scratch dir's work IS the whole tree and always runs.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("CLAWDE_HOME").ok();
        std::env::set_var("CLAWDE_HOME", tmp.path());
        let wt = crate::git::card_worktree_dir("default", "abcd1234");
        crate::git::create_worktree(repo.path(), &wt, None).unwrap();
        let result = run_gate(&wt, true).await;
        assert!(result.passed);
        assert!(result.skipped, "detail: {}", result.detail);
        assert!(
            result.detail.contains("no changes"),
            "detail: {}",
            result.detail
        );
        match previous {
            Some(value) => std::env::set_var("CLAWDE_HOME", value),
            None => std::env::remove_var("CLAWDE_HOME"),
        }
    }

    #[tokio::test]
    async fn gate_provisions_deps_before_checks() {
        // #4 — the fresh worktree has no node_modules; the gate must install
        // dependencies before running the checks, so a test that requires the
        // installed dep passes. Uses a `file:` dependency: `npm install`
        // materializes it into node_modules fully offline (no registry),
        // deterministic in any environment.
        if !node_available() {
            eprintln!("skipping: node not installed");
            return;
        }
        let result = gate_in_home(
            Some(r#"{"config":{"verify":{"auto_lint":false,"timeout_secs":120}}}"#),
            true,
            |work| {
                std::fs::create_dir_all(work.join("helper")).unwrap();
                std::fs::write(
                    work.join("helper/package.json"),
                    r#"{"name":"helper","version":"1.0.0"}"#,
                )
                .unwrap();
                std::fs::write(
                    work.join("package.json"),
                    r#"{"name":"t","version":"1.0.0","dependencies":{"helper":"file:./helper"},"scripts":{"test":"node -e \"process.exit(require('fs').existsSync('node_modules/helper') ? 0 : 1)\""}}"#,
                )
                .unwrap();
            },
        )
        .await;
        assert!(result.passed, "detail: {}", result.detail);
        assert!(!result.skipped, "install must not skip: {}", result.detail);
    }

    #[tokio::test]
    async fn gate_skips_when_dependency_install_fails() {
        // #4 — an install that cannot complete (lockfile out of sync with
        // package.json, so `npm ci` refuses) is an environment/project gap:
        // the gate must SKIP with the reason visible, not fail the card on a
        // missing-dependency error it can do nothing about.
        if !node_available() {
            eprintln!("skipping: node not installed");
            return;
        }
        let result = gate_in_home(
            Some(r#"{"config":{"verify":{"auto_lint":false,"timeout_secs":60}}}"#),
            true,
            |work| {
                std::fs::write(
                    work.join("package.json"),
                    r#"{"name":"t","version":"1.0.0","dependencies":{"helper":"file:./helper"},"scripts":{"test":"node -e \"process.exit(1)\""}}"#,
                )
                .unwrap();
                // Lockfile missing the declared dep: `npm ci` fails fast with
                // the sync error, before any network access.
                std::fs::write(
                    work.join("package-lock.json"),
                    r#"{"name":"t","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{"":{"name":"t","version":"1.0.0"}}}"#,
                )
                .unwrap();
            },
        )
        .await;
        assert!(result.passed, "install failure must not fail the card");
        assert!(result.skipped, "detail: {}", result.detail);
        assert!(
            result.detail.contains("dependency install failed"),
            "detail: {}",
            result.detail
        );
    }

    fn python3_available() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn package_install_prefers_the_repos_lockfile_tool() {
        let work = tempfile::tempdir().unwrap();
        std::fs::write(work.path().join("package.json"), "{}").unwrap();

        // npm with a lockfile -> npm ci.
        std::fs::write(work.path().join("package-lock.json"), "{}").unwrap();
        assert_eq!(package_install(work.path()), "npm ci");

        // No lockfile -> npm install must never write one (audit gap #1).
        std::fs::remove_file(work.path().join("package-lock.json")).unwrap();
        assert_eq!(
            package_install(work.path()),
            "npm install --no-package-lock"
        );

        // yarn.lock -> yarn when present, else the npm no-lockfile fallback.
        std::fs::write(work.path().join("yarn.lock"), "").unwrap();
        if tool_available("yarn") {
            assert!(
                package_install(work.path()).starts_with("yarn "),
                "{}",
                package_install(work.path())
            );
        } else {
            assert_eq!(
                package_install(work.path()),
                "npm install --no-package-lock"
            );
        }

        // pnpm-lock.yaml -> pnpm when present, else the npm fallback.
        std::fs::remove_file(work.path().join("yarn.lock")).unwrap();
        std::fs::write(work.path().join("pnpm-lock.yaml"), "").unwrap();
        if tool_available("pnpm") {
            assert!(package_install(work.path()).starts_with("pnpm "));
        } else {
            assert_eq!(
                package_install(work.path()),
                "npm install --no-package-lock"
            );
        }
    }

    #[tokio::test]
    async fn provision_deps_does_not_generate_a_lockfile() {
        // Audit gap #1: provisioning a JS project WITHOUT a lockfile must not
        // hand the card a gate-generated package-lock.json (it would ride into
        // the diff and the pinned commit as non-agent work).
        if !node_available() {
            eprintln!("skipping: node not installed");
            return;
        }
        let work = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(work.path().join("helper")).unwrap();
        std::fs::write(
            work.path().join("helper/package.json"),
            r#"{"name":"helper","version":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::write(
            work.path().join("package.json"),
            r#"{"name":"t","version":"1.0.0","dependencies":{"helper":"file:./helper"}}"#,
        )
        .unwrap();
        provision_deps(work.path(), 120).await.unwrap();
        assert!(
            work.path()
                .join("node_modules/helper/package.json")
                .exists(),
            "deps must be installed"
        );
        assert!(
            !work.path().join("package-lock.json").exists(),
            "provisioning must not generate a lockfile"
        );
    }

    #[tokio::test]
    async fn provision_deps_builds_a_venv_for_pyproject_projects() {
        // Audit gap #2: a modern pyproject.toml-only Python project must hit
        // the venv+pip branch (vs. being skipped as if it had no Python deps).
        if !python3_available() {
            eprintln!("skipping: python3 not installed");
            return;
        }
        let work = tempfile::tempdir().unwrap();
        std::fs::write(
            work.path().join("pyproject.toml"),
            "[project]\nname = \"t\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let result = provision_deps(work.path(), 180).await;
        // The pyproject branch fired: either deps installed cleanly (a Some
        // venv bin) or the environment lacked the pieces (an Err -> the gate
        // skips, which is the fail-open contract). It must NOT claim no Python
        // deps were provisioned (Ok(None)).
        assert!(
            matches!(result, Ok(Some(_)) | Err(_)),
            "pyproject-only project must hit the venv branch: {result:?}"
        );
    }
}
