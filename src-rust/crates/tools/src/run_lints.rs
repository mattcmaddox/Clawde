// run_lints.rs — Execute the project's linter / typechecker and report issues.
//
// Part of the execute-and-verify loop: after a code change, the agent calls
// this to catch style and type errors the test suite may miss. When no
// explicit command is given, the tool detects one from the project structure
// (see `detect_project::detect_project_info`).
//
// Security: executes arbitrary commands, so it self-gates via
// `ctx.check_permission*` with an `Execute` permission level.

use crate::detect_project::detect_project_info;
use crate::run_tests::{run_command_with_timeout, truncate_output};
use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::debug;

pub struct RunLintsTool;

#[derive(Debug, Deserialize)]
struct RunLintsInput {
    /// Optional explicit lint/typecheck command. Detected from the project
    /// when omitted.
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

#[async_trait]
impl Tool for RunLintsTool {
    // Gates itself: calls `ctx.check_permission_with_details_and_path` (Execute).
    fn self_gates(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "RunLints"
    }

    fn description(&self) -> &str {
        "Run the project's linter and/or typechecker and report issues. Use \\\n\
         after editing code to catch style and type errors before finalizing. \\\n\
         When `command` is omitted the tool detects one from the project \\\n\
         structure (cargo clippy, ruff, eslint, tsc --noEmit, go vet, etc.)."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Execute
    }

    fn network_capable(&self) -> bool {
        // Explicit lint commands can install dependencies or make arbitrary
        // outbound requests.
        true
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Optional explicit lint/typecheck command. Detected when omitted."
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
        let params: RunLintsInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
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
                match info.lint_commands.first() {
                    Some(c) => c.clone(),
                    None => {
                        return ToolResult::error(
                            "No lint command detected for this project. Pass an explicit \
                             `command` (e.g. \"cargo clippy -- -D warnings\").",
                        )
                    }
                }
            }
        };

        // Execute-level permission: show the command in the permission dialog.
        let desc = format!("[RunLints] {}", command);
        let details = format!(
            "Runs the linter/typechecker for the project at {}",
            project_root.display()
        );
        if let Err(e) = ctx.check_permission_with_details_and_path(
            self.name(),
            &desc,
            &details,
            std::path::PathBuf::from(&command),
            false,
        ) {
            return ToolResult::error(e.to_string());
        }

        debug!(command = %command, root = %project_root.display(), "Running lints");

        let timeout_secs = params.timeout.clamp(1, 600);
        let (output, exit_code, timed_out) =
            run_command_with_timeout(&command, &project_root, timeout_secs).await;

        let truncated = truncate_output(&output);

        match exit_code {
            Some(0) => ToolResult::success(format!("Lints passed ({}).\n{}", command, truncated)),
            Some(code) => ToolResult::error(format!(
                "Lint issues found — '{}' exited with code {}\n{}",
                command, code, truncated
            )),
            None => {
                if timed_out {
                    ToolResult::error(format!(
                        "'{}' timed out after {}s\n{}",
                        command, timeout_secs, truncated
                    ))
                } else {
                    ToolResult::error(format!("'{}' could not be run.\n{}", command, truncated))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    /// Real-cargo integration test: requires `cargo` + the clippy component on
    /// PATH, so it is skipped (not failed) when the toolchain is unavailable.
    async fn execute_detects_rust_lint_command() {
        if which::which("cargo").is_err() {
            eprintln!("skipping: cargo not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "pub fn f() -> i32 { 1 }\n").unwrap();

        let ctx = crate::test_support::allow_all_context(dir.path().to_path_buf());
        let res = RunLintsTool.execute(json!({"timeout": 120}), &ctx).await;
        // A tiny valid crate should pass clippy with no warnings.
        assert!(!res.is_error, "lint run failed: {}", res.content);
        assert!(res.content.contains("Lints passed"));
    }

    #[tokio::test]
    async fn execute_with_explicit_command_reports_failure() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::test_support::allow_all_context(dir.path().to_path_buf());
        let res = RunLintsTool
            .execute(json!({"command": "sh -c 'echo warning; exit 2'"}), &ctx)
            .await;
        assert!(res.is_error, "expected error: {}", res.content);
        assert!(
            res.content.contains("exited with code 2"),
            "content: {}",
            res.content
        );
        assert!(res.content.contains("warning"));
    }
    #[test]
    fn split_command_reused_from_run_tests() {
        // The lint tool reuses the shared splitter — pin the contract.
        use crate::run_tests::split_command;
        assert_eq!(
            split_command("cargo clippy --all-targets"),
            vec!["cargo", "clippy", "--all-targets"]
        );
    }
}
