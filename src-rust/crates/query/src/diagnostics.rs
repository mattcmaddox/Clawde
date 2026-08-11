//! Native diagnostics for the writer/verifier pipeline.
//!
//! This harness is deliberately deterministic and self-contained. It creates a
//! synthetic temporary project, runs Clawde's real deterministic verification
//! policy, injects a fake semantic verifier, and checks the read-only request
//! boundary. It never loads credentials, contacts a provider, or touches the
//! user's project.

use crate::agent_tool::{build_semantic_verifier_tools, semantic_verifier_tool_names};
use crate::continuation::{
    ContinuationDecision, ContinuationPolicy, SemanticAfterVerifyPolicy, SemanticVerdict,
    SemanticVerifyReport, SemanticVerifyRequest, SemanticVerifyRunner, TurnEndContext,
};
use crate::verify::VerifyVerdict;
use clawde_core::config::{VerifyConfig, VerifySandbox};
use clawde_core::snapshot::Patch;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const DIAGNOSTICS_SCHEMA_VERSION: &str = "native-diagnostics.v1";

/// Redacted operational evidence for the semantic pipeline.
///
/// This deliberately reports only bounded policy state and outcome metadata:
/// provider family, clamped limits, gate/runner/fixer reachability, terminal
/// verdict, and elapsed time. It never contains credentials, project paths, or
/// raw model output.
#[derive(Debug, Clone, Serialize)]
pub struct NativeSemanticEvidence {
    pub mode: &'static str,
    pub configured_provider_family: &'static str,
    pub verifier_max_turns: u32,
    pub fixer_max_turns: u32,
    pub max_attempts: u32,
    pub fixer_max_attempts: u32,
    pub deterministic_gate_passed: bool,
    pub verifier_reached: bool,
    pub fixer_configured: bool,
    pub fixer_ran: bool,
    pub terminal_verdict: Option<String>,
    pub elapsed_ms: u64,
}

/// One redacted diagnostic assertion.
#[derive(Debug, Clone, Serialize)]
pub struct NativeDiagnosticCheck {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// Safe result returned by the native diagnostics harness.
///
/// The report intentionally contains no fixture paths, credentials, raw model
/// output, or project source. It is suitable for text, JSON, and CI output.
#[derive(Debug, Clone, Serialize)]
pub struct NativeDiagnosticsReport {
    pub schema_version: &'static str,
    pub ok: bool,
    pub live_provider_calls: bool,
    pub project_mutated: bool,
    pub checks: Vec<NativeDiagnosticCheck>,
    pub semantic_verdict: Option<String>,
    pub read_only_tools: Vec<String>,
    pub semantic: NativeSemanticEvidence,
}

impl NativeDiagnosticsReport {
    fn from_checks(
        checks: Vec<NativeDiagnosticCheck>,
        semantic_verdict: Option<String>,
        project_mutated: bool,
        semantic: NativeSemanticEvidence,
    ) -> Self {
        Self {
            schema_version: DIAGNOSTICS_SCHEMA_VERSION,
            ok: checks.iter().all(|check| check.ok) && !project_mutated,
            live_provider_calls: false,
            project_mutated,
            checks,
            semantic_verdict,
            read_only_tools: semantic_verifier_tool_names(),
            semantic,
        }
    }
}

fn check(name: &str, ok: bool, detail: impl Into<String>) -> NativeDiagnosticCheck {
    NativeDiagnosticCheck {
        name: name.to_string(),
        ok,
        detail: detail.into(),
    }
}

struct FixtureGuard {
    path: PathBuf,
}

impl FixtureGuard {
    fn new() -> Self {
        Self {
            path: unique_fixture_dir(),
        }
    }
}

impl Drop for FixtureGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn unique_fixture_dir() -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!(
        "clawde-native-diagnostics-{}-{nonce}",
        std::process::id()
    ))
}

fn write_fixture(root: &Path) -> std::io::Result<std::collections::BTreeMap<PathBuf, String>> {
    std::fs::create_dir_all(root.join("src"))?;
    std::fs::create_dir_all(root.join(".cargo"))?;
    let files = [
        (
            PathBuf::from(".cargo/config.toml"),
            "[net]\noffline = true\n".to_string(),
        ),
        (
            PathBuf::from("Cargo.toml"),
            "[package]\nname = \"clawde_native_diagnostics_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n".to_string(),
        ),
        (
            PathBuf::from("src/lib.rs"),
            // `pub(crate)` (not `pub`) so the workspace dead-code guard's
            // naive `pub fn` declaration scan never mistakes this embedded
            // fixture source for a live workspace function.
            "pub(crate) fn diagnostic_value() -> u32 { 1 }\n".to_string(),
        ),
    ];
    for (relative, contents) in &files {
        std::fs::write(root.join(relative), contents)?;
    }
    Ok(files.into_iter().collect())
}

fn verify_config() -> VerifyConfig {
    VerifyConfig {
        enabled: true,
        max_retries: 1,
        sandbox: VerifySandbox::Direct,
        auto_lint: false,
        auto_test: true,
        skip_when_no_writes: true,
        timeout_secs: 120,
        container_image: None,
        ..Default::default()
    }
}

fn semantic_evidence(
    config: &VerifyConfig,
    deterministic_gate_passed: bool,
    verifier_reached: bool,
    fixer_configured: bool,
    fixer_ran: bool,
    terminal_verdict: Option<String>,
    started_at: std::time::Instant,
) -> NativeSemanticEvidence {
    NativeSemanticEvidence {
        mode: "native-fake-runner",
        configured_provider_family: "free",
        verifier_max_turns: config
            .semantic_max_turns
            .clamp(1, clawde_core::config::MAX_SEMANTIC_TURNS),
        fixer_max_turns: config
            .semantic_fix_max_turns
            .clamp(1, clawde_core::config::MAX_SEMANTIC_TURNS),
        max_attempts: config
            .semantic_max_attempts
            .clamp(1, clawde_core::config::MAX_SEMANTIC_ATTEMPTS),
        fixer_max_attempts: config
            .semantic_fix_max_attempts
            .clamp(1, clawde_core::config::MAX_SEMANTIC_ATTEMPTS),
        deterministic_gate_passed,
        verifier_reached,
        fixer_configured,
        fixer_ran,
        terminal_verdict,
        elapsed_ms: started_at.elapsed().as_millis() as u64,
    }
}

/// Run the native semantic pipeline diagnostics.
///
/// The only subprocess is `cargo test --workspace` inside the synthetic
/// fixture, through the existing deterministic `VerifyPolicy` command path.
/// The semantic verifier is an injected fake that returns a fixed `pass`
/// response and records the request metadata for boundary assertions.
pub async fn run_native_diagnostics() -> NativeDiagnosticsReport {
    let started_at = std::time::Instant::now();
    let config = verify_config();
    let fixture = FixtureGuard::new();
    let mut checks = Vec::new();
    let mut semantic_verdict = None;
    let authored_files = match write_fixture(&fixture.path) {
        Ok(files) => files,
        Err(error) => {
            checks.push(check(
                "fixture_setup",
                false,
                "could not create synthetic fixture",
            ));
            let _ = error;
            return NativeDiagnosticsReport::from_checks(
                checks,
                None,
                false,
                semantic_evidence(&config, false, false, false, false, None, started_at),
            );
        }
    };

    let changed_file = fixture.path.join("src/lib.rs");
    let patch = Patch {
        hash: "native-diagnostics-fixture".to_string(),
        files: vec![changed_file],
    };
    let observed_request: Arc<Mutex<Option<SemanticVerifyRequest>>> = Arc::new(Mutex::new(None));
    let observed_for_runner = observed_request.clone();
    let runner: SemanticVerifyRunner = Arc::new(move |request| {
        *observed_for_runner.lock().expect("diagnostic request lock") = Some(request);
        Box::pin(async {
            Ok(
                r#"{"verdict":"pass","summary":"native diagnostic verifier pass","findings":[]}"#
                    .to_string(),
            )
        })
    });

    let policy = SemanticAfterVerifyPolicy::new(config.clone(), &fixture.path, Some(runner), None);
    let context = TurnEndContext {
        session_id: "native-diagnostics",
        total_tokens_used: 0,
        turn_elapsed_secs: 0,
        working_dir: &fixture.path,
        turn_made_writes: true,
        turn_output_tokens: 0,
        changed_files: Some(&patch),
        changed_diff: Some(
            "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@\n+// native diagnostics fixture diff\n",
        ),
        spec: None,
    };
    let decision = policy.decide_async(&context).await;
    let deterministic_report = policy.verify_report();
    let semantic_report: Option<SemanticVerifyReport> = policy.semantic_report();

    let deterministic_passed = matches!(
        deterministic_report.as_ref().map(|report| report.verdict),
        Some(VerifyVerdict::Pass)
    );
    checks.push(check(
        "deterministic_gate_passed",
        deterministic_passed,
        if deterministic_passed {
            "synthetic cargo test passed"
        } else {
            "deterministic verification did not produce a pass"
        },
    ));

    let runner_called = observed_request
        .lock()
        .expect("diagnostic request lock")
        .is_some();
    checks.push(check(
        "semantic_runner_reached",
        runner_called,
        if runner_called {
            "injected verifier was reached after deterministic pass"
        } else {
            "injected verifier was not reached"
        },
    ));

    if let Some(report) = semantic_report {
        semantic_verdict = Some(report.verdict.as_str().to_string());
        checks.push(check(
            "semantic_report_parsed",
            report.verdict == SemanticVerdict::Pass,
            if report.verdict == SemanticVerdict::Pass {
                "structured pass report parsed"
            } else {
                "semantic report was not pass"
            },
        ));
    } else {
        checks.push(check(
            "semantic_report_parsed",
            false,
            "no semantic report was produced",
        ));
    }

    let observed = observed_request
        .lock()
        .expect("diagnostic request lock")
        .clone();
    let expected_tools = semantic_verifier_tool_names();
    let actual_tools = build_semantic_verifier_tools()
        .iter()
        .map(|tool| tool.name().to_string())
        .collect::<Vec<_>>();
    let production_tool_boundary_ok = actual_tools == expected_tools;
    checks.push(check(
        "production_read_only_tool_boundary",
        production_tool_boundary_ok,
        if production_tool_boundary_ok {
            "production verifier tool builder exposes only the fixed read-only allowlist"
        } else {
            "production verifier tool builder exposed an unexpected tool"
        },
    ));

    let read_only_boundary_ok = observed
        .as_ref()
        .map(|request| request.read_only_tools == expected_tools)
        .unwrap_or(false);
    checks.push(check(
        "read_only_request_boundary",
        read_only_boundary_ok,
        if read_only_boundary_ok {
            "verifier request carried the fixed read-only allowlist"
        } else {
            "verifier request did not carry the fixed read-only allowlist"
        },
    ));

    let diff_bounded_and_present = observed
        .as_ref()
        .map(|request| {
            !request.diff.trim().is_empty()
                && request.diff.len() <= crate::continuation::SEMANTIC_VERIFY_MAX_DIFF_CHARS + 32
        })
        .unwrap_or(false);
    checks.push(check(
        "scoped_diff_present",
        diff_bounded_and_present,
        if diff_bounded_and_present {
            "bounded non-empty diff reached verifier"
        } else {
            "bounded diff was missing or exceeded the safety limit"
        },
    ));

    let project_mutated = authored_files.iter().any(|(relative, before)| {
        std::fs::read_to_string(fixture.path.join(relative))
            .map(|after| after != *before)
            .unwrap_or(true)
    });
    checks.push(check(
        "diagnostic_fixture_unchanged",
        !project_mutated,
        if project_mutated {
            "synthetic source changed during diagnostics"
        } else {
            "synthetic source remained unchanged"
        },
    ));

    let decision_is_stop = matches!(decision, ContinuationDecision::Stop { .. });
    checks.push(check(
        "pipeline_stopped_after_pass",
        decision_is_stop,
        if decision_is_stop {
            "semantic pass produced a terminal decision"
        } else {
            "semantic pass unexpectedly continued"
        },
    ));

    let evidence = semantic_evidence(
        &config,
        deterministic_passed,
        runner_called,
        false,
        false,
        semantic_verdict.clone(),
        started_at,
    );
    NativeDiagnosticsReport::from_checks(checks, semantic_verdict, project_mutated, evidence)
}

#[cfg(test)]
mod tests {
    use super::run_native_diagnostics;

    #[tokio::test]
    async fn native_diagnostics_prove_the_semantic_pipeline_without_live_provider() {
        let report = run_native_diagnostics().await;
        assert!(report.ok, "native diagnostics failed: {report:?}");
        assert!(!report.live_provider_calls);
        assert!(!report.project_mutated);
        assert_eq!(report.semantic_verdict.as_deref(), Some("pass"));
        assert_eq!(report.semantic.mode, "native-fake-runner");
        assert_eq!(report.semantic.configured_provider_family, "free");
        assert_eq!(report.semantic.verifier_max_turns, 3);
        assert_eq!(report.semantic.fixer_max_turns, 5);
        assert_eq!(report.semantic.max_attempts, 3);
        assert_eq!(report.semantic.fixer_max_attempts, 3);
        assert!(report.semantic.deterministic_gate_passed);
        assert!(report.semantic.verifier_reached);
        assert!(!report.semantic.fixer_ran);
        assert!(
            report.checks.iter().all(|check| check.ok),
            "checks: {:?}",
            report.checks
        );
    }
}
