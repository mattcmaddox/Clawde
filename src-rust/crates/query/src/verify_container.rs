// verify_container.rs — `VerifySandbox::Container` execution for the verify loop.
//
// Instead of running tests/lints directly in the project directory (which has
// side effects: build artifacts, modified caches, stray files), the container
// sandbox verifies inside a disposable Docker/podman container:
//
//   1. detect a container runtime (docker preferred, podman fallback);
//   2. pick an image — the `CLAWDE_VERIFY_IMAGE` env var when set, otherwise a
//      language-appropriate default (rust:latest, node:latest, ...) derived
//      from project detection;
//   3. ensure the image is present (`image inspect`, then a bounded `pull`);
//   4. run each detected test/lint command inside a fresh `--rm` container
//      with the project directory mounted at `/workspace`, so the checks see
//      the exact working tree while the container isolates the toolchain and
//      the filesystem outside the mount;
//   5. no cleanup needed — `--rm` removes each container on exit.
//
// Any failure at steps 1–3 is reported to the caller as an `Err` and surfaced
// as a clear stop note — verification is never silently skipped, nor run
// un-sandboxed. A check that fails to start inside the container (missing
// toolchain in the image) is a skipped result, the same as `direct` mode.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use clawde_core::config::VerifyConfig;
use clawde_tools::detect_project::{detect_project_info, ProjectLanguage};

use crate::verify::{run_argv_sync, CheckResult};

/// Unique container-name counter (per-process) so concurrent verifications
/// cannot collide on the same container name.
static CT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Setup commands (image inspect/pull) are bounded by this hard cap so a hung
/// registry can never stall the verify loop, which is otherwise strictly
/// bounded by `VerifyConfig::timeout_secs` (see `crate::verify::run_command_sync`).
const CONTAINER_SETUP_TIMEOUT_SECS: u64 = 120;

/// Run the configured test/lint checks inside disposable containers, then
/// return the results.
///
/// Returns `Err` only when the sandbox itself cannot be set up (no container
/// runtime installed, image cannot be resolved or pulled).
pub fn run_checks_in_container(
    config: &VerifyConfig,
    working_dir: &Path,
) -> Result<Vec<CheckResult>, String> {
    let runtime = detect_runtime().ok_or_else(|| {
        "Verify sandbox 'container' requires Docker or Podman, but neither is installed — \
         verification skipped. Install a container runtime, or set \"verify\": \
         {\"sandbox\": \"direct\"} in settings.json to verify in place."
            .to_string()
    })?;
    let image = resolve_image(working_dir)?;
    ensure_image(runtime, &image, working_dir)?;

    // Mirror `run_checks_direct`: tests first, then lints, each in its own
    // `--rm` container with the project mounted read-write at /workspace.
    let info = detect_project_info(working_dir);
    let mut results = Vec::new();
    if config.auto_test {
        if let Some(cmd) = info.test_commands.first() {
            results.push(run_container_check(
                format!("test: {cmd}"),
                cmd,
                runtime,
                &image,
                working_dir,
                config.timeout_secs,
            ));
        }
    }
    if config.auto_lint {
        if let Some(cmd) = info.lint_commands.first() {
            results.push(run_container_check(
                format!("lint: {cmd}"),
                cmd,
                runtime,
                &image,
                working_dir,
                config.timeout_secs,
            ));
        }
    }
    Ok(results)
}

/// The first available container runtime, or `None` when neither docker nor
/// podman is on PATH. Docker wins when both are present.
fn detect_runtime() -> Option<&'static str> {
    for runtime in ["docker", "podman"] {
        let ok = Command::new(runtime)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return Some(runtime);
        }
    }
    None
}

/// The image to verify in: `CLAWDE_VERIFY_IMAGE` wins, otherwise a default per
/// detected project language. Unknown languages fall back to a small Linux
/// base so a project with no recognized toolchain still gets a (fast-failing,
/// reported) round rather than a setup error.
fn resolve_image(working_dir: &Path) -> Result<String, String> {
    if let Ok(over) = std::env::var("CLAWDE_VERIFY_IMAGE") {
        let over = over.trim();
        if !over.is_empty() {
            return Ok(over.to_string());
        }
    }
    let language = detect_project_info(working_dir).language;
    Ok(match language {
        ProjectLanguage::Rust => "rust:latest".to_string(),
        ProjectLanguage::Python => "python:latest".to_string(),
        ProjectLanguage::TypeScript | ProjectLanguage::JavaScript => "node:latest".to_string(),
        ProjectLanguage::Go => "golang:latest".to_string(),
        ProjectLanguage::Java => "eclipse-temurin:latest".to_string(),
        ProjectLanguage::Cpp => "gcc:latest".to_string(),
        ProjectLanguage::Unknown(_) => {
            return Err(
                "Verify sandbox 'container' could not pick a default image for this project — \
                 no recognized toolchain was detected. Set the CLAWDE_VERIFY_IMAGE environment \
                 variable to the image to verify in, or set \\\"verify\\\": {{\\\"sandbox\\\": \
                 \\\"direct\\\"}} in settings.json."
                    .to_string(),
            )
        }
    })
}
/// Ensure `image` is available locally: a quick `image inspect` (bounded), and
/// if missing a bounded `image pull`. A pull that fails (registry down, image
/// does not exist) is a setup error — the round stops with a clear note.
fn ensure_image(runtime: &str, image: &str, working_dir: &Path) -> Result<(), String> {
    let (_out, code, _timed_out) =
        run_setup_command(runtime, &["image", "inspect", image], working_dir);
    if code == Some(0) {
        return Ok(());
    }
    // Inspect failed (missing image, or a daemon error). Attempt a pull; the
    // pull's own failure message is more useful than inspect's.
    let (out, code, _timed_out) =
        run_setup_command(runtime, &["image", "pull", image], working_dir);
    if code == Some(0) {
        return Ok(());
    }
    let msg = out.trim();
    let msg = if msg.is_empty() { "no output" } else { msg };
    Err(format!(
        "Verify sandbox 'container' could not prepare image '{image}': {msg}"
    ))
}

/// Bounded runtime setup command (image inspect/pull): the shared argv runner
/// with the setup-timeout cap instead of the per-check timeout.
fn run_setup_command(
    runtime: &str,
    args: &[&str],
    working_dir: &Path,
) -> (String, Option<i32>, bool) {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(runtime.to_string());
    argv.extend(args.iter().map(|s| s.to_string()));
    run_argv_sync(&argv, working_dir, CONTAINER_SETUP_TIMEOUT_SECS)
}

/// Run a single check command inside a fresh `--rm` container with the
/// project directory mounted at `/workspace` and the container's working dir
/// set to it. Command execution is delegated to the shared bounded runner.
fn run_container_check(
    label: String,
    command: &str,
    runtime: &str,
    image: &str,
    working_dir: &Path,
    timeout_secs: u64,
) -> CheckResult {
    // A unique name lets the cleanup assertion (and operators) identify these
    // containers; `--rm` still removes them on exit.
    let name = format!(
        "clawde-verify-{}-{}",
        std::process::id(),
        CT_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let argv = vec![
        runtime.to_string(),
        "run".to_string(),
        "--rm".to_string(),
        "--name".to_string(),
        name,
        "-v".to_string(),
        format!("{}:/workspace", working_dir.display()),
        "-w".to_string(),
        "/workspace".to_string(),
        image.to_string(),
        "sh".to_string(),
        "-c".to_string(),
        command.to_string(),
    ];
    let (output, code, timed_out) = run_argv_sync(&argv, working_dir, timeout_secs);
    if !timed_out && code == Some(0) {
        CheckResult::pass(label)
    } else if !timed_out && code.is_none() {
        // The runtime never started the container (daemon down, image cannot
        // run). An environment gap, not a code failure — skipped.
        CheckResult::skipped(label, output)
    } else {
        CheckResult::fail(label, output, timed_out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::continuation::ContinuationPolicy;
    use std::process::Command as Proc;

    // Env mutations (CLAWDE_VERIFY_IMAGE) must serialize on a module-level
    // lock — the same pattern as `crates/core/src/paths.rs::ENV_LOCK` and the
    // query crate's coordinator tests — so the parallel test runner cannot
    // interleave two image-resolution tests.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn container_available() -> bool {
        let out = Proc::new("docker")
            .arg("info")
            .stdin(Stdio::null())
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if out {
            return true;
        }
        Proc::new("podman")
            .arg("info")
            .stdin(Stdio::null())
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// A tiny, fast-pulling image guaranteed to have a POSIX shell.
    const TEST_IMAGE: &str = "alpine:latest";

    fn container_config() -> VerifyConfig {
        VerifyConfig {
            enabled: true,
            max_retries: 3,
            sandbox: clawde_core::config::VerifySandbox::Container,
            auto_lint: true,
            auto_test: true,
            skip_when_no_writes: true,
            timeout_secs: 120,
        }
    }

    #[test]
    fn container_sandbox_is_implemented() {
        assert!(
            clawde_core::config::VerifySandbox::Container.is_implemented(),
            "container must no longer report 'not implemented'"
        );
    }

    #[test]
    fn container_image_resolution_prefers_env_override() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("CLAWDE_VERIFY_IMAGE", "custom/verify:tag");
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_image(dir.path()).unwrap(),
            "custom/verify:tag",
            "CLAWDE_VERIFY_IMAGE must win over the language default"
        );
        std::env::remove_var("CLAWDE_VERIFY_IMAGE");
    }

    #[test]
    fn container_image_resolution_language_defaults() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::remove_var("CLAWDE_VERIFY_IMAGE");

        // A Rust project resolves to the Rust image.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        assert_eq!(resolve_image(dir.path()).unwrap(), "rust:latest");

        // An unrecognized project is a setup error telling the user to set
        // the image explicitly.
        let bare = tempfile::tempdir().unwrap();
        assert!(
            resolve_image(bare.path()).is_err(),
            "unknown-language projects must demand an explicit image"
        );
    }

    #[test]
    fn container_requires_runtime() {
        // detect_runtime must return a runtime when docker/podman is present.
        if !container_available() {
            eprintln!("skipping: no container runtime");
            return;
        }
        assert!(
            detect_runtime().is_some(),
            "a runtime should be detected when one is available"
        );
    }

    /// End-to-end container round against a small base image: detection finds
    /// test/lint commands, they execute inside a real container, the round's
    /// decision is computed from the results, and every container is removed.
    ///
    /// Alpine has no `npm`, so both detected commands exit 127 (toolchain
    /// missing) — that is itself the meaningful assertion here: the checks
    /// genuinely ran inside the container and failed there, and the loop
    /// treats that as a fixable failure (Continue 1/3) rather than a setup
    /// error.
    #[test]
    fn container_sandbox_runs_checks_and_leaves_no_containers() {
        if !container_available() {
            eprintln!("skipping: no container runtime");
            return;
        }
        let dir = tempfile::tempdir().unwrap();

        // Fixture: a minimal npm-style project so detection finds test/lint
        // commands (npm test / npm run lint) to run in the container.
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts": {"test": "echo ok", "lint": "exit 1"}}"#,
        )
        .unwrap();

        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("CLAWDE_VERIFY_IMAGE", TEST_IMAGE);

        let cfg = container_config();
        let decision = crate::verify::VerifyPolicy::new(cfg, dir.path().to_path_buf()).decide(
            &crate::continuation::TurnEndContext {
                session_id: "sess",
                total_tokens_used: 0,
                turn_elapsed_secs: 0,
                working_dir: dir.path(),
                turn_made_writes: true,
            },
        );

        // Both commands fail (npm missing in alpine, exit 127), which is a
        // fixable code failure, not a setup error: the loop must continue as
        // auto-fix attempt 1/3.
        match &decision {
            crate::continuation::ContinuationDecision::Continue { message } => {
                assert!(
                    message.contains("1/3"),
                    "in-container failures must continue as attempt 1/3: {message}"
                );
                assert!(
                    message.contains("npm"),
                    "failure output should mention the toolchain that was missing: {message}"
                );
            }
            _ => {
                panic!("container round must continue on in-container failures, got: {decision:?}")
            }
        }
        std::env::remove_var("CLAWDE_VERIFY_IMAGE");

        // No leaked containers: every run used `--rm` plus a unique
        // `clawde-verify-*` name, so a ps filter catches any straggler.
        let (out, code, _) = run_setup_command(
            detect_runtime().unwrap(),
            &["ps", "-aq", "--filter", "name=clawde-verify"],
            dir.path(),
        );
        assert_eq!(code, Some(0), "docker ps must run");
        assert!(
            out.trim().is_empty(),
            "no verify containers may remain after the round: {out}"
        );
    }
}
