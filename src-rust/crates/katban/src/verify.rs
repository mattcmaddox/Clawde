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

use std::path::Path;
use std::time::Duration;

use clawde_core::config::{Settings, VerifyConfig};

/// Outcome of the verification gate for one card.
#[derive(Debug, Clone)]
pub struct GateResult {
    pub passed: bool,
    /// Human digest: what ran and how it did (or why nothing ran).
    pub detail: String,
}

impl GateResult {
    fn passed(detail: impl Into<String>) -> Self {
        Self {
            passed: true,
            detail: detail.into(),
        }
    }

    fn failed(detail: impl Into<String>) -> Self {
        Self {
            passed: false,
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

/// Run the verification gate in the card's worktree: detect the project's
/// test/lint commands and run them (tests first, then lints), each bounded by
/// the configured per-command timeout. A project with no detectable commands,
/// or a card in a non-code directory, passes trivially (nothing to run).
pub async fn run_gate(work_dir: &Path) -> GateResult {
    let config = verify_config();
    if !config.enabled {
        return GateResult::passed(
            "verification disabled (settings.json \"config.verify.enabled\": false)",
        );
    }
    let info = clawde_tools::detect_project::detect_project_info(work_dir);
    let mut labels: Vec<String> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    let mut ran = 0usize;
    if config.auto_test {
        if let Some(cmd) = info.test_commands.first() {
            ran += 1;
            let label = format!("test: {cmd}");
            if let Some(err) = run_check(work_dir, cmd, config.timeout_secs).await {
                failures.push(format!("{label}: {err}"));
            }
            labels.push(label);
        }
    }
    if config.auto_lint {
        if let Some(cmd) = info.lint_commands.first() {
            ran += 1;
            let label = format!("lint: {cmd}");
            if let Some(err) = run_check(work_dir, cmd, config.timeout_secs).await {
                failures.push(format!("{label}: {err}"));
            }
            labels.push(label);
        }
    }
    if ran == 0 {
        return GateResult::passed("no test/lint commands detected for this project");
    }
    if failures.is_empty() {
        GateResult::passed(format!("all checks passed ({})", labels.join(", ")))
    } else {
        GateResult::failed(failures.join("; "))
    }
}

/// Run one check command to completion. Returns `Some(err)` when it failed or
/// timed out; `None` when it passed or could not start (missing tool — an
/// environment gap, mirroring the interactive loop's `skipped`). The child is
/// killed if it exceeds the timeout, so a hung check cannot pin a slot.
async fn run_check(work_dir: &Path, command: &str, timeout_secs: u64) -> Option<String> {
    // Detected commands are fixed strings (never user input) without quotes,
    // so a whitespace split is a faithful tokenization.
    let parts: Vec<&str> = command.split_whitespace().collect();
    let (program, args) = parts.split_first()?;
    let timeout = Duration::from_secs(timeout_secs.max(1));
    let child = tokio::process::Command::new(program)
        .args(args)
        .current_dir(work_dir)
        .kill_on_drop(true)
        .spawn()
        .ok()?; // spawn failure = skipped
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
/// findings. Best-effort by contract: a spawn/exit failure returns `Err` (the
/// runner logs and skips); unparseable output degrades to one free-form
/// comment via `parse_findings`.
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
    let output = tokio::process::Command::new(&clawde_bin)
        .current_dir(work_dir)
        .args(["--print", &reviewer_prompt])
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("could not start auto-review agent: {e}"))?;
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
        let line = item
            .get("line")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|l| !l.trim().is_empty());
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
    // crate's board_server tests do the same).
    #[allow(clippy::await_holding_lock)]
    async fn gate_in_home(
        settings: Option<&str>,
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
        let result = run_gate(work.path()).await;
        match previous {
            Some(value) => std::env::set_var("CLAWDE_HOME", value),
            None => std::env::remove_var("CLAWDE_HOME"),
        }
        result
    }

    #[tokio::test]
    async fn gate_passes_trivially_with_no_project() {
        let result = gate_in_home(None, |work| {
            std::fs::write(work.join("notes.txt"), "no code here\n").unwrap();
        })
        .await;
        assert!(result.passed);
        assert!(result.detail.contains("no test/lint commands"));
    }

    #[tokio::test]
    async fn gate_passes_when_verify_disabled() {
        let result = gate_in_home(Some(r#"{"config":{"verify":{"enabled":false}}}"#), |work| {
            std::fs::create_dir_all(work.join("src")).unwrap();
            std::fs::write(
                work.join("Cargo.toml"),
                "[package]\nname=\"x\"\nversion=\"0.1.0\"\n",
            )
            .unwrap();
        })
        .await;
        assert!(result.passed);
        assert!(result.detail.contains("disabled"));
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
}
