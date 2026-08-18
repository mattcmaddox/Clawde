// run_tests.rs — Execute the project's test suite and report results.
//
// Part of the execute-and-verify loop: after a code change, the agent calls
// this to check whether the change broke anything. When no explicit command is
// given, the tool detects one from the project structure (see
// `detect_project::detect_project_info`).
//
// Security: this tool executes arbitrary commands, so it self-gates via
// `ctx.check_permission*` with an `Execute` permission level — the interactive
// TUI shows a permission dialog before the command runs.

use crate::detect_project::detect_project_info;
use crate::{PermissionLevel, Tool, ToolContext, ToolErrorCode, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tracing::debug;

pub struct RunTestsTool;

#[derive(Debug, Deserialize)]
struct RunTestsInput {
    /// Optional explicit command (e.g. "cargo test --workspace"). When
    /// omitted, the command is detected from the project structure.
    #[serde(default)]
    command: Option<String>,
    /// Optional project root. Defaults to the working directory.
    #[serde(default)]
    project_root: Option<String>,
    /// Timeout in seconds (default 300, max 600).
    #[serde(default = "default_timeout")]
    timeout: u64,
}

fn default_timeout() -> u64 {
    300
}

/// Return whether a command is a single, local test-runner invocation that can
/// be placed inside the isolated execution sandbox. This intentionally rejects
/// shell wrappers, pipelines, redirects, substitutions, package installation,
/// and arbitrary interpreter code: the sandbox is a test executor, not a
/// second Bash entry point.
pub fn is_local_test_command(command: &str) -> bool {
    clawde_core::bash_classifier::is_direct_test_command(command)
}

/// Resolve the offline mode for this session from its session-scoped config.
/// Live `/ollama` toggles update that config before the next turn.
fn is_network_isolated(ctx: &ToolContext) -> bool {
    clawde_core::network_isolation_enabled(&ctx.config)
}

#[derive(Debug, Clone)]
enum NetworkIsolationBackend {
    Bubblewrap(std::path::PathBuf),
    Unshare(std::path::PathBuf),
}

/// Find a backend that is installed *and usable by this process*.
///
/// Checking only `PATH` is insufficient: `unshare` is commonly installed but
/// denied to unprivileged processes. The probe must agree with execution so we
/// never report a sandbox as available and then silently fall through to an
/// unsandboxed command.
fn network_isolation_backend() -> Option<NetworkIsolationBackend> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(bwrap) = which::which("bwrap") {
            let usable = std::process::Command::new(&bwrap)
                .args([
                    "--die-with-parent",
                    "--unshare-net",
                    "--ro-bind",
                    "/",
                    "/",
                    "--",
                    "true",
                ])
                .status()
                .is_ok_and(|status| status.success());
            if usable {
                return Some(NetworkIsolationBackend::Bubblewrap(bwrap));
            }
        }
        if let Ok(unshare) = which::which("unshare") {
            let usable = std::process::Command::new(&unshare)
                .args(["--net", "--", "true"])
                .status()
                .is_ok_and(|status| status.success());
            if usable {
                return Some(NetworkIsolationBackend::Unshare(unshare));
            }
        }
    }
    None
}

/// Whether this host has a supported, usable network namespace backend.
/// Isolated test execution fails closed when neither backend is usable;
/// silently falling back to the host network would defeat Ollama isolation.
pub fn network_isolation_available() -> bool {
    network_isolation_backend().is_some()
}

/// Split a shell-style command line into program + args, honouring single
/// and double quoted segments (e.g. `cargo test -- "foo bar"` or
/// `sh -c 'exit 0'`) and backslash escapes. Not a full POSIX parser —
/// env-var expansion, globs, and nested substitutions are passed through
/// verbatim as arguments, which is fine for detected test/lint commands.
pub fn split_command(command: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' if quote.is_none() => quote = Some(c),
            '\'' if quote == Some('\'') => quote = None,
            '"' if quote == Some('"') => quote = None,
            ' ' | '\t' if quote.is_none() => {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
            }
            '\\' if quote != Some('\'') => {
                // Outside quotes (or inside double quotes) a backslash
                // escapes the next character.
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// Run a command to completion, returning `(stdout+stderr, exit_code, timed_out)`.
pub(crate) async fn run_command_with_timeout(
    command: &str,
    working_dir: &std::path::Path,
    timeout_secs: u64,
) -> (String, Option<i32>, bool) {
    run_command_with_timeout_inner(command, working_dir, timeout_secs, false).await
}

pub(crate) async fn run_command_with_timeout_isolated(
    command: &str,
    working_dir: &std::path::Path,
    timeout_secs: u64,
) -> (String, Option<i32>, bool) {
    run_command_with_timeout_inner(command, working_dir, timeout_secs, true).await
}

async fn run_command_with_timeout_inner(
    command: &str,
    working_dir: &std::path::Path,
    timeout_secs: u64,
    network_isolated: bool,
) -> (String, Option<i32>, bool) {
    let parts = split_command(command);
    if parts.is_empty() {
        return (String::new(), None, false);
    }
    let mut cmd;
    if network_isolated {
        #[cfg(target_os = "linux")]
        {
            match network_isolation_backend() {
                Some(NetworkIsolationBackend::Bubblewrap(bwrap)) => {
                    cmd = Command::new(bwrap);
                    cmd.args([
                        "--die-with-parent",
                        "--unshare-net",
                        "--ro-bind",
                        "/",
                        "/",
                        "--dev",
                        "/dev",
                        "--proc",
                        "/proc",
                        "--tmpfs",
                        "/tmp",
                        "--bind",
                    ])
                    .arg(working_dir)
                    .arg(working_dir)
                    .args(["--chdir"])
                    .arg(working_dir)
                    .arg(&parts[0])
                    .args(&parts[1..]);
                }
                Some(NetworkIsolationBackend::Unshare(unshare)) => {
                    cmd = Command::new(unshare);
                    cmd.args(["--net", "--", &parts[0]])
                        .args(&parts[1..])
                        .current_dir(working_dir);
                }
                None => {
                    return (
                        "No usable network namespace backend available (need a usable bwrap or unshare)".to_string(),
                        None,
                        false,
                    );
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            return (
                "Isolated local tests are unsupported on this platform".to_string(),
                None,
                false,
            );
        }
    } else {
        cmd = Command::new(&parts[0]);
        cmd.args(&parts[1..]).current_dir(working_dir);
    }
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return (
                format!("Failed to spawn '{}': {}", parts[0], e),
                None,
                false,
            )
        }
    };

    // Take the pipes before waiting so we keep a live handle to kill the
    // child if the timeout fires (wait_with_output would consume it).
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let timeout = Duration::from_secs(timeout_secs);
    let result = tokio::time::timeout(timeout, async {
        let mut text = String::new();
        if let Some(mut out) = stdout {
            let mut buf = Vec::new();
            if tokio::io::AsyncReadExt::read_to_end(&mut out, &mut buf)
                .await
                .is_ok()
            {
                text.push_str(&String::from_utf8_lossy(&buf));
            }
        }
        if let Some(mut err) = stderr {
            let mut buf = Vec::new();
            if tokio::io::AsyncReadExt::read_to_end(&mut err, &mut buf)
                .await
                .is_ok()
                && !buf.is_empty()
            {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str("STDERR:\n");
                text.push_str(&String::from_utf8_lossy(&buf));
            }
        }
        let status = child.wait().await;
        (text, status)
    })
    .await;

    match result {
        Ok((text, Ok(status))) => (text, status.code(), false),
        Ok((_, Err(e))) => (format!("Failed to run '{}': {}", command, e), None, false),
        Err(_) => {
            // Kill the child so a timed-out test/lint run never lingers.
            let _ = child.kill().await;
            let _ = child.wait().await;
            (
                format!("Command timed out after {}s", timeout_secs),
                None,
                true,
            )
        }
    }
}

#[async_trait]
impl Tool for RunTestsTool {
    // Gates itself: calls `ctx.check_permission_with_details_and_path` (Execute).
    fn self_gates(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "RunTests"
    }

    fn description(&self) -> &str {
        "Run the project's test suite and report results. After editing code, \\\n\
         call this to verify nothing broke. When `command` is omitted the tool \\\n\
         detects the test command from the project structure (cargo test, \\\n\
         pytest, npm test, go test, etc.). Output includes the full test \\\n\
         log; a non-zero exit code is reported as an error."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Execute
    }

    fn network_capable(&self) -> bool {
        // The unrestricted form can install dependencies or call arbitrary
        // network clients. `available_in_ollama_isolated_mode` is only a
        // declaration that execute() will validate and sandbox the command.
        true
    }

    fn available_in_ollama_isolated_mode(&self) -> bool {
        true
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Optional explicit test command. Detected from the project when omitted."
                },
                "project_root": {
                    "type": "string",
                    "description": "Optional project root. Defaults to the working directory."
                },
                "timeout": {
                    "type": "number",
                    "description": "Timeout in seconds (default 300, max 600)."
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: RunTestsInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => {
                return ToolResult::error_with_code(
                    ToolErrorCode::InvalidInput,
                    format!("Invalid input: {}", e),
                )
            }
        };

        let project_root = params
            .project_root
            .as_deref()
            .map(|p| ctx.resolve_path(p))
            .unwrap_or_else(|| ctx.working_dir.clone());

        let command = match params.command {
            Some(c) if !c.trim().is_empty() => c.trim().to_string(),
            _ => {
                let info = detect_project_info(&project_root);
                match info.test_commands.first() {
                    Some(c) => c.clone(),
                    None => {
                        return ToolResult::error_with_code(
                            ToolErrorCode::ExecutionFailed,
                            "No test command detected for this project. Pass an explicit \
                             `command` (e.g. \"cargo test --workspace\").",
                        )
                    }
                }
            }
        };

        // Execute-level permission: show the command in the permission dialog.
        let isolated = is_network_isolated(ctx);
        if isolated {
            if !is_local_test_command(&command) {
                return ToolResult::error_with_code(
                    ToolErrorCode::NetworkIsolationBlocked,
                    "Ollama offline mode only permits a direct local test command ".to_string()
                        + "(for example, `python3 -m pytest -q`); shell wrappers and "
                        + "arbitrary commands are blocked.",
                );
            }
            if !network_isolation_available() {
                return ToolResult::error_with_code(
                    ToolErrorCode::NetworkSandboxUnavailable,
                    "Cannot run local tests in Ollama offline mode: no network ".to_string()
                        + "namespace backend (bwrap or unshare) is available.",
                );
            }
        }

        let desc = format!("[RunTests] {}", command);
        let details = format!(
            "Runs the test suite for the project at {}{}",
            project_root.display(),
            if isolated {
                " inside a network-isolated local test sandbox"
            } else {
                ""
            }
        );
        if let Err(e) = ctx.check_permission_with_details_and_path_for_tool(
            self,
            &desc,
            &details,
            std::path::PathBuf::from(&command),
            false,
            Some(!isolated),
        ) {
            return ToolResult::error_with_code(ToolErrorCode::PermissionDenied, e.to_string());
        }

        debug!(command = %command, root = %project_root.display(), "Running tests");

        let timeout_secs = params.timeout.clamp(1, 600);
        let (output, exit_code, timed_out) = if isolated {
            run_command_with_timeout_isolated(&command, &project_root, timeout_secs).await
        } else {
            run_command_with_timeout(&command, &project_root, timeout_secs).await
        };

        let truncated = truncate_output(&output);

        match exit_code {
            Some(0) => ToolResult::success(format!("Tests passed ({}).\n{}", command, truncated)),
            Some(code) => ToolResult::error_with_code(
                ToolErrorCode::TestFailed,
                format!(
                    "Tests FAILED — '{}' exited with code {}\n{}",
                    command, code, truncated
                ),
            ),
            None => {
                if timed_out {
                    ToolResult::error_with_code(
                        ToolErrorCode::Timeout,
                        format!(
                            "'{}' timed out after {}s\n{}",
                            command, timeout_secs, truncated
                        ),
                    )
                } else {
                    ToolResult::error_with_code(
                        ToolErrorCode::ExecutionFailed,
                        format!("'{}' could not be run.\n{}", command, truncated),
                    )
                }
            }
        }
    }
}

/// Truncate very long command output so the model only sees the head and
/// tail (where failures are summarized) plus a marker line. Shared by
/// RunTests and RunLints.
///
/// Slices on UTF-8 char boundaries so multi-byte output (CJK, emoji, unicode
/// test names) never panics on a byte-index-outside-char-boundary.
pub fn truncate_output(output: &str) -> String {
    const MAX_LEN: usize = 60_000;
    if output.len() <= MAX_LEN {
        return output.to_string();
    }
    let keep = MAX_LEN / 2;
    // Find the last char boundary at or before `keep` bytes for the head.
    let head_end = output.floor_char_boundary(keep);
    // Find the first char boundary at or after `len - keep` for the tail.
    let tail_start = output.floor_char_boundary(output.len() - keep);
    let head = &output[..head_end];
    let tail = &output[tail_start..];
    format!(
        "{}\n\n... ({} characters truncated — showing head and tail) ...\n\n{}",
        head,
        output.len() - MAX_LEN,
        tail
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn split_command_handles_quotes_and_spaces() {
        assert_eq!(
            split_command("cargo test --workspace"),
            vec!["cargo", "test", "--workspace"]
        );
        assert_eq!(
            split_command(r#"cargo test -- "foo bar" "baz""#),
            vec!["cargo", "test", "--", "foo bar", "baz"]
        );
        assert_eq!(split_command("sh -c 'exit 0'"), vec!["sh", "-c", "exit 0"]);
        assert_eq!(split_command(""), Vec::<String>::new());
        assert_eq!(split_command("  single  "), vec!["single"]);
    }

    #[test]
    fn local_test_command_accepts_direct_runners_only() {
        for command in [
            "python3 -m pytest -q",
            "python3 -m unittest discover",
            "cargo test --workspace",
            "go test ./...",
            "npm test",
            "pnpm run test",
        ] {
            assert!(is_local_test_command(command), "should accept {command}");
        }
        for command in [
            "bash -c 'pytest'",
            "pytest; curl https://example.test",
            "python3 -c 'import pytest'",
            "npm install",
            "curl https://example.test",
        ] {
            assert!(!is_local_test_command(command), "should reject {command}");
        }
    }

    #[test]
    fn run_tests_is_retained_for_isolated_mode_but_remains_network_capable() {
        assert!(RunTestsTool.available_in_ollama_isolated_mode());
        assert!(RunTestsTool.network_capable());
    }

    #[tokio::test]
    async fn config_isolation_blocks_non_direct_test_command() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = crate::test_support::allow_all_context(dir.path().to_path_buf());
        ctx.config.provider_configs.insert(
            "ollama".to_string(),
            clawde_core::config::ProviderConfig {
                options: [("mode".to_string(), json!("isolated"))]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
        );

        let result = RunTestsTool
            .execute(json!({"command": "sh -c 'echo should-not-run'"}), &ctx)
            .await;

        assert!(result.is_error);
        assert_eq!(
            result.error_code,
            Some(ToolErrorCode::NetworkIsolationBlocked)
        );
        assert!(result.content.contains("direct local test command"));
        assert!(!result.content.contains("should-not-run"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn isolation_availability_uses_a_real_backend_probe() {
        assert_eq!(
            network_isolation_available(),
            network_isolation_backend().is_some()
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn isolated_runner_executes_inside_a_network_namespace() {
        if !network_isolation_available() {
            eprintln!("skipping: no bwrap/unshare backend");
            return;
        }
        let (output, code, timed_out) = run_command_with_timeout_isolated(
            "python3 -c 'import pathlib; print(any(line.split()[1] == \"00000000\" for line in pathlib.Path(\"/proc/net/route\").read_text().splitlines()[1:] if len(line.split()) > 1))'",
            std::path::Path::new("/tmp"),
            30,
        )
        .await;
        assert_eq!(code, Some(0), "isolated command failed: {output}");
        assert!(!timed_out);
        assert!(
            output.trim_end().ends_with("False"),
            "isolated namespace unexpectedly has a default route: {output}"
        );
    }

    #[tokio::test]
    async fn run_command_success_and_failure() {
        let (out, code, timed_out) =
            run_command_with_timeout("sh -c 'exit 0'", std::path::Path::new("."), 30).await;
        assert_eq!(code, Some(0));
        assert!(!timed_out);
        assert!(!out.contains("STDERR"));

        let (out, code, _) =
            run_command_with_timeout("sh -c 'echo boom; exit 3'", std::path::Path::new("."), 30)
                .await;
        assert_eq!(code, Some(3));
        assert!(out.contains("boom"));
    }

    #[tokio::test]
    async fn timeout_kills_child_process() {
        // A command that never exits must be killed when the timeout fires,
        // not left running as an orphan.
        let (out, code, timed_out) =
            run_command_with_timeout("sh -c 'sleep 60'", std::path::Path::new("."), 1).await;
        assert!(timed_out, "expected timeout: {}", out);
        assert_eq!(code, None);
        assert!(out.contains("timed out"));
    }

    /// Real-cargo integration test: requires `cargo` on PATH, so it is
    /// skipped (not failed) when the toolchain is unavailable.
    #[tokio::test]
    async fn execute_detects_rust_test_command() {
        if which::which("cargo").is_err() {
            eprintln!("skipping: cargo not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        // Create a tiny crate that compiles and has one passing test.
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "#[cfg(test)]\nmod t { #[test] fn ok() {} }\n",
        )
        .unwrap();

        let ctx = crate::test_support::allow_all_context(dir.path().to_path_buf());
        let res = RunTestsTool.execute(json!({"timeout": 120}), &ctx).await;
        assert!(!res.is_error, "test run failed: {}", res.content);
        assert!(res.content.contains("Tests passed"));
    }

    #[tokio::test]
    async fn execute_with_explicit_command_reports_failure() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::test_support::allow_all_context(dir.path().to_path_buf());
        let res = RunTestsTool
            .execute(json!({"command": "sh -c 'echo fail; exit 1'"}), &ctx)
            .await;
        assert!(res.is_error, "expected error: {}", res.content);
        assert!(
            res.content.contains("exited with code 1"),
            "content: {}",
            res.content
        );
        assert!(res.content.contains("fail"));
        assert_eq!(res.error_code, Some(ToolErrorCode::TestFailed));
    }
}
