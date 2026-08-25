// verify_sandbox.rs — `VerifySandbox::Worktree` execution for the verify loop.
//
// Instead of running tests/lints directly in the project directory (which has
// side effects: build artifacts, modified caches, stray files), the worktree
// sandbox verifies against an isolated copy of the repository:
//
//   1. find the enclosing git repository;
//   2. `git worktree add --detach <tmp>` — a fresh checkout of HEAD under the
//      system temp dir;
//   3. apply the session's uncommitted changes onto it — tracked edits via
//      `git diff HEAD --binary` + `git apply`, new files by copying untracked
//      (non-ignored) files over — so the worktree mirrors the working tree
//      exactly as the user sees it;
//   4. run the detected test/lint commands inside the worktree (via the same
//      bounded command runner as the `direct` sandbox);
//   5. remove the worktree (and prune stale registration) afterwards.
//
// Any failure at steps 1–3 is reported to the caller as an `Err` and surfaced
// as a clear stop note — verification is never silently skipped, nor run
// un-sandboxed. Cleanup is best-effort: a leftover temp worktree from a crash
// is preferable to silently aborting a verification round.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use clawde_core::config::VerifyConfig;

use crate::verify::{run_checks_direct, CheckResult};

/// Unique name counter (per-process) so concurrent verifications cannot
/// collide on the same worktree or patch-file path.
static WT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Run the configured test/lint checks inside a temporary git worktree that
/// mirrors the session's uncommitted changes, then clean up.
///
/// Returns `Err` only when the sandbox itself cannot be set up (not a git
/// repository, git missing, worktree creation or patch application failed).
pub fn run_checks_in_worktree(
    config: &VerifyConfig,
    working_dir: &Path,
) -> Result<Vec<CheckResult>, String> {
    let repo_root = clawde_core::git_utils::get_repo_root(working_dir).ok_or_else(|| {
        format!(
            "Verify sandbox 'git worktree' requires a git repository, but the project at \
             '{}' is not inside one — verification skipped. Set \"verify\": {{\"sandbox\": \
             \"direct\"}} in settings.json to verify in place.",
            working_dir.display()
        )
    })?;

    let worktree_path = temp_path("wt");
    // A stale directory from a previously crashed run (same pid + counter) must
    // not make `git worktree add` fail; `git worktree prune` later reaps any
    // leftover registration in the repo metadata.
    if worktree_path.exists() {
        let _ = std::fs::remove_dir_all(&worktree_path);
    }

    git(
        &repo_root,
        &["worktree", "add", "--detach", &path_arg(&worktree_path)],
    )
    .map_err(|e| {
        format!(
            "Verify sandbox 'git worktree' could not be set up: {e}. Set \"verify\": \
                 {{\"sandbox\": \"direct\"}} in settings.json to verify in place."
        )
    })?;

    if let Err(e) = apply_working_tree_changes(&repo_root, &worktree_path) {
        let _ = remove_worktree(&repo_root, &worktree_path);
        return Err(format!(
            "Verify sandbox 'git worktree' could not apply the session's changes: {e}. \
             Set \"verify\": {{\"sandbox\": \"direct\"}} in settings.json to verify in place."
        ));
    }

    // Run the checks inside the isolated worktree. Note this is a cold
    // checkout: there is no shared build-artifact cache (e.g. `target/`), so
    // the first build can take noticeably longer than in `direct` mode.
    let results = run_checks_direct(config, &worktree_path);

    if let Err(e) = remove_worktree(&repo_root, &worktree_path) {
        // Best-effort cleanup: never fail the round because a temp directory
        // could not be removed — surface a warning and move on.
        eprintln!(
            "clawde: warning: failed to remove verify worktree at {}: {e}",
            worktree_path.display()
        );
    }

    Ok(results)
}

/// A unique, not-yet-used temp path for a worktree or patch file.
fn temp_path(kind: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "clawde-verify-{kind}-{}-{}",
        std::process::id(),
        WT_COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Mirror the working tree's uncommitted state onto `worktree_path`:
/// 1. tracked changes (staged + unstaged vs HEAD, binary included) via a diff;
/// 2. new (untracked, non-ignored) files copied over verbatim.
fn apply_working_tree_changes(repo_root: &Path, worktree_path: &Path) -> Result<(), String> {
    let diff = git(repo_root, &["diff", "HEAD", "--binary"])?;
    if !diff.trim().is_empty() {
        // Stage the diff through a temp file rather than stdin: `git apply`
        // cannot read a huge diff safely through `--stdin` plumbing here, and
        // the file also keeps the command line short.
        let patch_path = temp_path("patch");
        std::fs::write(&patch_path, &diff).map_err(|e| e.to_string())?;
        let result = git(
            worktree_path,
            &[
                "apply",
                "--whitespace=nowarn",
                "--binary",
                &path_arg(&patch_path),
            ],
        );
        let _ = std::fs::remove_file(&patch_path);
        result?;
    }

    // Untracked files are not part of `git diff HEAD`; copy them over so a
    // brand-new file written this turn is present for the checks.
    let untracked = git(repo_root, &["ls-files", "--others", "--exclude-standard"])?;
    for rel in untracked.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let src = repo_root.join(rel);
        let dst = worktree_path.join(rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create dir {}: {e}", parent.display()))?;
        }
        std::fs::copy(&src, &dst).map_err(|e| format!("copy {}: {e}", src.display()))?;
    }

    Ok(())
}

/// Remove `worktree_path`: `git worktree remove --force`, then a directory
/// fallback plus `git worktree prune` so no stale registration or junk dir is
/// left behind.
fn remove_worktree(repo_root: &Path, worktree_path: &Path) -> Result<(), String> {
    let _ = git(
        repo_root,
        &["worktree", "remove", "--force", &path_arg(worktree_path)],
    );
    if worktree_path.exists() {
        std::fs::remove_dir_all(worktree_path)
            .map_err(|e| format!("remove dir {}: {e}", worktree_path.display()))?;
    }
    // Reap any registration the forced removal left behind (e.g. when the
    // directory was already gone but the metadata entry remained).
    let _ = git(repo_root, &["worktree", "prune"]);
    Ok(())
}

/// Setup git commands are bounded by this hard cap so a hung git — most
/// plausibly a network fetch triggered by `git diff` on a blobless partial
/// clone (`--filter=blob:none`) — can never stall the verify loop, which is
/// otherwise strictly bounded by `VerifyConfig::timeout_secs` (see
/// `crate::verify::run_command_sync`).
const GIT_SETUP_TIMEOUT_SECS: u64 = 30;

/// Run `git` in `cwd`, returning stdout on success or the error text on
/// failure, with a hard timeout.
fn git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let (stdout, _stderr) = git_bounded(cwd, args, GIT_SETUP_TIMEOUT_SECS)?;
    Ok(stdout)
}

/// Bounded `git` runner: stdout and stderr are redirected to separate temp
/// files (so a large diff can never deadlock on a full pipe buffer), the
/// child is polled against a deadline, and it is killed and reaped if the
/// deadline passes — the same pattern as `run_command_sync`.
fn git_bounded(cwd: &Path, args: &[&str], timeout_secs: u64) -> Result<(String, String), String> {
    let id = WT_COUNTER.fetch_add(1, Ordering::Relaxed);
    let out_path = std::env::temp_dir().join(format!(
        "clawde-verify-git-{}-{}.out",
        std::process::id(),
        id
    ));
    let err_path = std::env::temp_dir().join(format!(
        "clawde-verify-git-{}-{}.err",
        std::process::id(),
        id
    ));
    let out_file = std::fs::File::create(&out_path).map_err(|e| e.to_string())?;
    let err_file = std::fs::File::create(&err_path).map_err(|e| e.to_string())?;

    let mut child = match Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(err_file))
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&out_path);
            let _ = std::fs::remove_file(&err_path);
            return Err(format!("Failed to spawn 'git': {e}"));
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
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&out_path);
                let _ = std::fs::remove_file(&err_path);
                return Err(format!("Failed to wait for 'git': {e}"));
            }
        }
    };

    let stdout = std::fs::read_to_string(&out_path).unwrap_or_default();
    let stderr = std::fs::read_to_string(&err_path).unwrap_or_default();
    let _ = std::fs::remove_file(&out_path);
    let _ = std::fs::remove_file(&err_path);

    if timed_out {
        return Err(format!(
            "'git {}' timed out after {timeout_secs}s",
            args.join(" ")
        ));
    }
    match exit_status.and_then(|s| s.code()) {
        Some(0) => Ok((stdout, stderr)),
        Some(code) => {
            let msg = stderr.trim();
            let msg = if msg.is_empty() { stdout.trim() } else { msg };
            let msg = if msg.is_empty() { "no output" } else { msg };
            Err(format!(
                "'git {}' exited with {code}: {msg}",
                args.join(" ")
            ))
        }
        None => Err(format!("'git {}' exited abnormally", args.join(" "))),
    }
}

/// Path argument: lossy string form is what git expects on the command line.
fn path_arg(p: &Path) -> String {
    p.to_string_lossy().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::continuation::ContinuationPolicy;
    use std::process::Command as Proc;

    fn git_available() -> bool {
        Proc::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn cargo_available() -> bool {
        Proc::new("cargo")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let out = Proc::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("git must run");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Init a git repo with one committed (passing) crate. Returns the dir.
    fn init_crate_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q"]);
        run_git(dir.path(), &["config", "user.email", "t@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test"]);
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"verify-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "#[cfg(test)]\nmod t { #[test] fn ok() {} }\n",
        )
        .unwrap();
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "init"]);
        dir
    }

    fn worktree_config() -> VerifyConfig {
        VerifyConfig {
            enabled: true,
            max_retries: 3,
            sandbox: clawde_core::config::VerifySandbox::Worktree,
            auto_lint: true,
            auto_test: true,
            skip_when_no_writes: true,
            timeout_secs: 120,
            container_image: None,
            ..Default::default()
        }
    }

    #[test]
    fn worktree_sandbox_is_implemented() {
        assert!(
            clawde_core::config::VerifySandbox::Worktree.is_implemented(),
            "worktree must no longer report 'not implemented'"
        );
    }

    #[test]
    fn worktree_sandbox_requires_git_repo() {
        // A plain temp dir with no git metadata: the sandbox must stop with a
        // clear note instead of running un-sandboxed.
        let dir = tempfile::tempdir().unwrap();
        let cfg = worktree_config();
        let decision = crate::verify::VerifyPolicy::new(cfg, dir.path().to_path_buf()).decide(
            &crate::continuation::TurnEndContext {
                session_id: "sess",
                total_tokens_used: 0,
                turn_elapsed_secs: 0,
                working_dir: dir.path(),
                turn_made_writes: true,
                turn_output_tokens: 0,
                changed_files: None,
                changed_diff: None,
                spec: None,
                plan_replan_headroom: None,
            },
        );
        assert!(!decision.is_continue());
        match decision {
            crate::continuation::ContinuationDecision::Stop { note } => {
                let note = note.expect("worktree requires-git note must be present");
                assert!(note.contains("requires a git repository"), "note: {note}");
                assert!(note.contains("direct"), "note: {note}");
            }
            _ => unreachable!(),
        }
    }

    /// End-to-end worktree round: an *untracked* failing test file is the only
    /// change in the working tree. The sandbox must (a) copy it into the
    /// worktree, (b) run cargo there and see the failure, and (c) clean the
    /// worktree up afterwards so the fixture repo has no linked worktrees.
    #[test]
    fn worktree_sandbox_runs_checks_on_untracked_change_and_cleans_up() {
        if !git_available() || !cargo_available() {
            eprintln!("skipping: git/cargo not available");
            return;
        }
        let dir = init_crate_repo();

        // New failing test file — untracked, so only the copy step brings it
        // into the worktree. The committed crate passes.
        std::fs::create_dir_all(dir.path().join("tests")).unwrap();
        std::fs::write(
            dir.path().join("tests/broken.rs"),
            "#[test]\nfn fails() { assert!(false); }\n",
        )
        .unwrap();

        let cfg = worktree_config();
        let decision = crate::verify::VerifyPolicy::new(cfg, dir.path().to_path_buf()).decide(
            &crate::continuation::TurnEndContext {
                session_id: "sess",
                total_tokens_used: 0,
                turn_elapsed_secs: 0,
                working_dir: dir.path(),
                turn_made_writes: true,
                turn_output_tokens: 0,
                changed_files: None,
                changed_diff: None,
                spec: None,
                plan_replan_headroom: None,
            },
        );

        // The untracked failing test was applied + run → auto-fix attempt 1/3.
        match &decision {
            crate::continuation::ContinuationDecision::Continue { message } => {
                assert!(
                    message.contains("1/3"),
                    "must continue as attempt 1/3: {message}"
                );
            }
            _ => panic!("untracked failing test must continue, got: {decision:?}"),
        }

        // Cleanup: the fixture repo must have exactly one (its own) worktree.
        let list = git(dir.path(), &["worktree", "list"]).unwrap();
        let paths = list.lines().filter(|l| !l.trim().is_empty()).count();
        assert_eq!(paths, 1, "worktree must be cleaned up: {list}");
    }

    /// A tracked modification — the most common session change — must be
    /// carried into the worktree via `git diff HEAD` + `git apply` (not just
    /// the untracked-copy path) and fail the round there.
    #[test]
    fn worktree_sandbox_applies_tracked_modifications() {
        if !git_available() || !cargo_available() {
            eprintln!("skipping: git/cargo not available");
            return;
        }
        let dir = init_crate_repo();

        // Overwrite the committed (passing) src/lib.rs with a failing body.
        // This is a tracked edit, so only the diff-apply branch can bring it
        // into the worktree.
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "#[test]\nfn fails() { assert!(false); }\n",
        )
        .unwrap();

        let cfg = worktree_config();
        let decision = crate::verify::VerifyPolicy::new(cfg, dir.path().to_path_buf()).decide(
            &crate::continuation::TurnEndContext {
                session_id: "sess",
                total_tokens_used: 0,
                turn_elapsed_secs: 0,
                working_dir: dir.path(),
                turn_made_writes: true,
                turn_output_tokens: 0,
                changed_files: None,
                changed_diff: None,
                spec: None,
                plan_replan_headroom: None,
            },
        );

        match &decision {
            crate::continuation::ContinuationDecision::Continue { message } => {
                assert!(
                    message.contains("1/3"),
                    "tracked failing edit must continue as attempt 1/3: {message}"
                );
            }
            _ => panic!("tracked failing edit must continue, got: {decision:?}"),
        }

        let list = git(dir.path(), &["worktree", "list"]).unwrap();
        let paths = list.lines().filter(|l| !l.trim().is_empty()).count();
        assert_eq!(paths, 1, "worktree must be cleaned up: {list}");
    }

    /// A passing, unchanged worktree round: no untracked files, HEAD passes →
    /// stop with "All checks passed" and no leftover worktree.
    #[test]
    fn worktree_sandbox_passing_round_stops_and_cleans_up() {
        if !git_available() || !cargo_available() {
            eprintln!("skipping: git/cargo not available");
            return;
        }
        let dir = init_crate_repo();
        let cfg = worktree_config();
        let decision = crate::verify::VerifyPolicy::new(cfg, dir.path().to_path_buf()).decide(
            &crate::continuation::TurnEndContext {
                session_id: "sess",
                total_tokens_used: 0,
                turn_elapsed_secs: 0,
                working_dir: dir.path(),
                turn_made_writes: true,
                turn_output_tokens: 0,
                changed_files: None,
                changed_diff: None,
                spec: None,
                plan_replan_headroom: None,
            },
        );
        match &decision {
            crate::continuation::ContinuationDecision::Stop { note } => {
                assert!(
                    note.as_deref().unwrap_or("").contains("All checks passed"),
                    "decision: {decision:?}"
                );
            }
            _ => panic!("clean worktree round must pass, got: {decision:?}"),
        }
        let list = git(dir.path(), &["worktree", "list"]).unwrap();
        let paths = list.lines().filter(|l| !l.trim().is_empty()).count();
        assert_eq!(paths, 1, "worktree must be cleaned up: {list}");
    }
}
