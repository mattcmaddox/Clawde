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
use crate::run_tests::{
    network_isolation_available, run_command_with_timeout, run_command_with_timeout_isolated,
    truncate_output,
};
use crate::{PermissionLevel, Tool, ToolContext, ToolErrorCode, ToolResult};
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

/// Return whether a command is a direct local lint/typecheck invocation that
/// can run inside the isolated execution sandbox.
pub fn is_local_lint_command(command: &str) -> bool {
    clawde_core::bash_classifier::is_direct_lint_command(command)
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
        // The unrestricted form can install dependencies or make arbitrary
        // outbound requests. The isolated form is retained only for validated
        // direct lint/typecheck commands.
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
                match info.lint_commands.first() {
                    Some(c) => c.clone(),
                    None => {
                        return ToolResult::error_with_code(
                            ToolErrorCode::ExecutionFailed,
                            "No lint command detected for this project. Pass an explicit \
                             `command` (e.g. \"cargo clippy -- -D warnings\").",
                        )
                    }
                }
            }
        };

        // Execute-level permission: show the command in the permission dialog.
        // Prefer the session config, while retaining the process-global toggle
        // as a compatibility fallback for `/ollama` sessions that have not yet
        // rebuilt their runtime config.
        let isolated = ctx.config.resolve_ollama_mode() == clawde_core::OllamaMode::Isolated
            || clawde_core::is_ollama_network_blocked();
        if isolated {
            if !is_local_lint_command(&command) {
                return ToolResult::error_with_code(
                    ToolErrorCode::NetworkIsolationBlocked,
                    "Ollama offline mode only permits a direct local lint/typecheck command "
                        .to_string()
                        + "(for example, `cargo clippy -- -D warnings`); shell wrappers and "
                        + "arbitrary commands are blocked.",
                );
            }
            if !network_isolation_available() {
                return ToolResult::error_with_code(
                    ToolErrorCode::NetworkSandboxUnavailable,
                    "Cannot run local lint/typecheck commands in Ollama offline mode: no network "
                        .to_string()
                        + "namespace backend (bwrap or unshare) is available.",
                );
            }
        }
        let desc = format!("[RunLints] {}", command);
        let details = format!(
            "Runs the linter/typechecker for the project at {}{}",
            project_root.display(),
            if isolated {
                " inside a network-isolated local lint sandbox"
            } else {
                ""
            }
        );
        if let Err(e) = ctx.check_permission_with_details_and_path_for_capability(
            self.name(),
            &desc,
            &details,
            std::path::PathBuf::from(&command),
            false,
            !isolated,
        ) {
            return ToolResult::error_with_code(ToolErrorCode::PermissionDenied, e.to_string());
        }

        debug!(command = %command, root = %project_root.display(), "Running lints");

        let timeout_secs = params.timeout.clamp(1, 600);
        let (output, exit_code, timed_out) = if isolated {
            run_command_with_timeout_isolated(&command, &project_root, timeout_secs).await
        } else {
            run_command_with_timeout(&command, &project_root, timeout_secs).await
        };

        let truncated = truncate_output(&output);

        match exit_code {
            Some(0) => ToolResult::success(format!("Lints passed ({}).\n{}", command, truncated)),
            Some(code) => ToolResult::error_with_code(
                ToolErrorCode::LintFailed,
                format!(
                    "Lint issues found — '{}' exited with code {}\n{}",
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

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn execute_direct_lint_in_isolated_sandbox() {
        if !network_isolation_available() || which::which("cargo").is_err() {
            eprintln!("skipping: isolated lint prerequisites unavailable");
            return;
        }
        let clippy_available = std::process::Command::new("cargo")
            .args(["clippy", "--version"])
            .status()
            .is_ok_and(|status| status.success());
        if !clippy_available {
            eprintln!("skipping: cargo clippy unavailable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname=\"isolated-lint\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn answer() -> i32 { 42 }\n",
        )
        .unwrap();

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
        let result = RunLintsTool
            .execute(
                json!({"command": "cargo clippy --quiet", "timeout": 120}),
                &ctx,
            )
            .await;

        assert!(!result.is_error, "isolated lint failed: {}", result.content);
        assert!(result.content.contains("Lints passed"));
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
        assert_eq!(res.error_code, Some(ToolErrorCode::LintFailed));
    }
    #[test]
    fn isolated_lint_capability_is_narrow() {
        assert!(RunLintsTool.available_in_ollama_isolated_mode());
        assert!(RunLintsTool.network_capable());
        for command in ["cargo clippy --all-targets", "python3 -m ruff check ."] {
            assert!(is_local_lint_command(command), "should accept {command}");
        }
        for command in [
            "sh -c 'cargo clippy'",
            "npm install",
            "cargo clippy; curl x",
        ] {
            assert!(!is_local_lint_command(command), "should reject {command}");
        }
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
