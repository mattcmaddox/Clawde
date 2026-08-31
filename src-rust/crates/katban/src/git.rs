//! A small, validation-first wrapper over the git CLI for the board runner
//! (spec §12/§16a): create a per-card worktree, capture its diff, and tear the
//! worktree down. Modeled on Vibe Kanban's `crates/git` ethos (path/command
//! validation, safety-first) rather than copied — Katban stays zero-dep for
//! git by shelling out, but only with fixed subcommands and validated paths.
//!
//! Path discipline: every path that reaches a git command is a validated,
//! canonical absolute path from the projects registry (or one derived under
//! `~/.clawde/katban/worktrees`), and worktree dirs always fall under a
//! fixed root we own (never the user's working tree). We never pass a
//! user-supplied string into an argv slot without validation, because git
//! treats args that begin with `-` as options.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Where this board's runner worktrees live: `~/.clawde/katban/worktrees`.
/// A card's worktree is `<root>/<project-encoding>/<card-id>`.
pub fn worktree_root() -> PathBuf {
    crate::config::katban_data_dir().join("worktrees")
}

/// Run `git <args>` in `cwd`, returning stdout on success / stderr on failure
/// (for the caller to surface). Never passes a dash-leading arg from caller
/// data unchecked: callers supply only fixed subcommand fragments.
fn git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// The worktree dir for a card, guaranteed under `worktree_root()` and using
/// the lossless project encoding (mirrors `board::project_dir_name`), so a
/// project name can never escape the root via `..` or a separator.
pub fn card_worktree_dir(project: &str, card_id: &str) -> PathBuf {
    worktree_root()
        .join(crate::board::project_dir_name(project))
        .join(safe_id(card_id))
}

/// A card id is always a short hex string we minted; but if a caller ever got
/// here with something hostile, refuse to derive a path from it.
fn safe_id(id: &str) -> String {
    if id.as_bytes().iter().all(u8::is_ascii_hexdigit) && !id.is_empty() {
        id.to_string()
    } else {
        crate::board::project_dir_name(id)
    }
}

/// Whether the given repo root has a git repository (so the runner can give a
/// clear \"register a git repo for this project\" instead of a confusing git
/// error at spawn time).
pub fn is_repo(repo_root: &Path) -> bool {
    git(repo_root, &["rev-parse", "--is-inside-work-tree"]).is_ok()
}

/// Create a detached worktree at `work_dir` based on `base_ref` (default:
/// repo HEAD) and return the branch name it checked out (empty for detached).
/// The dir is created under our owned `worktree_root`, so the `git worktree
/// add` path arg is a value we know is safe.
pub fn create_worktree(
    repo_root: &Path,
    work_dir: &Path,
    base_ref: Option<&str>,
) -> Result<(), String> {
    // `--detach` pins the commit regardless of base_ref spelling; we validate
    // that base_ref is not dash-leading so `--force`-style injection is
    // impossible (a refname literally cannot begin with '-' in git anyway).
    let base = base_ref.unwrap_or("HEAD");
    if base.starts_with('-') {
        return Err(format!("refusing ref '{base}' (starts with '-')"));
    }
    let dir = work_dir
        .to_str()
        .ok_or_else(|| "worktree path is not valid UTF-8".to_string())?;
    git(repo_root, &["worktree", "add", "--detach", dir, base]).map(|_| ())
}

/// Remove a worktree (unregister + delete the checkout). Best-effort by
/// design: callers treat failure as a warning, not a hard error, so a stale
/// leftover worktree can never wedge the runner.
pub fn remove_worktree(repo_root: &Path, work_dir: &Path) {
    let dir = work_dir.to_str().unwrap_or_default();
    // `force` handles a dirty worktree; safe here because only the runner ever
    // registers under this root and the dir path is one we derived.
    let _ = git(repo_root, &["worktree", "remove", "--force", dir]);
    let _ = git(repo_root, &["worktree", "prune"]);
    // If git couldn't remove it (e.g. the repo metadata already vanished),
    // delete the checkout dir itself as a last resort.
    if work_dir.exists() {
        let _ = std::fs::remove_dir_all(work_dir);
    }
}

/// Cap for a diff stored on a card. Full diffs belong in a checkout; the card
/// keeps a bounded digest so `board.json` cannot balloon to gigabytes from one
/// card touching a huge generated tree.
pub const DIFF_CAP: usize = 16 * 1024;

/// Summarize a unified diff without invoking another parser or model.
/// Binary changes are counted as changed files but contribute no line counts.
pub fn diff_summary(diff: &str) -> crate::board::DiffSummary {
    let mut summary = crate::board::DiffSummary::default();
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            summary.files_changed += 1;
        } else if line.starts_with('+') && !line.starts_with("+++") {
            summary.additions += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            summary.deletions += 1;
        }
    }
    summary
}

/// Marker line that identifies this file's katban-managed excludes.
const ARTIFACT_EXCLUDE_MARKER: &str = "# katban: ignore build artifacts";

/// Exclude common build artifacts from the card's diff and pinned commit.
/// The verification gate provisions dependencies inside the fresh worktree
/// (`npm ci`, a `.venv`, `cargo` builds), so a project without a `.gitignore`
/// would otherwise commit `node_modules`/`target` into the card's branch and
/// into the captured review diff.
///
/// Scoped per-worktree so the ignore rules never leak into the user's repo:
/// the excludes are written to this worktree's own git-metadata dir and wired
/// up via a per-worktree `core.excludesFile` (enabled with the standard
/// `extensions.worktreeConfig` extension). Nothing is a tracked `.gitignore`
/// (so excludes never appear in the diff/commit), and the rules vanish when
/// the worktree is torn down instead of persisting in `.git/info/exclude`.
/// Idempotent (guarded by the marker); a non-repo scratch dir is a silent
/// no-op.
pub fn ensure_artifact_excludes(work_dir: &Path) {
    // `config --worktree` needs the worktreeConfig extension; enabling it is
    // idempotent and inert on its own, and this is the mechanism git ships for
    // exactly this. Runs from the worktree root (writes to the common config).
    let _ = git(work_dir, &["config", "extensions.worktreeConfig", "true"]);
    // The per-worktree metadata dir (`<common>/worktrees/<name>`) is where
    // this worktree's own config lives; keep the exclude file there too.
    let Ok(config_path) = git(work_dir, &["rev-parse", "--git-path", "config.worktree"]) else {
        return; // not a git worktree (scratch dir)
    };
    let Some(meta_dir) = Path::new(&config_path).parent() else {
        return;
    };
    let exclude = meta_dir.join("katban.exclude");
    if let Some(exclude_str) = exclude.to_str() {
        let _ = git(
            work_dir,
            &["config", "--worktree", "core.excludesFile", exclude_str],
        );
    }
    if let Ok(existing) = std::fs::read_to_string(&exclude) {
        if existing.contains(ARTIFACT_EXCLUDE_MARKER) {
            return;
        }
    }
    if let Some(parent) = exclude.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    let body = format!(
        "{ARTIFACT_EXCLUDE_MARKER}\nnode_modules/\ntarget/\n__pycache__/\n*.pyc\n.venv/\ndist/\nbuild/\n"
    );
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&exclude)
    {
        let _ = file.write_all(body.as_bytes());
    }
}

/// `git --no-pager diff HEAD` for the card's worktree, capped at `DIFF_CAP`
/// chars. Runs inside the worktree (the changes the agent made live there),
/// and treats untracked files as additions via `git add -N` so they show up
/// too. Returns an empty string when there's no diff, the command failed, or
/// the dir doesn't hold a repo (all safe defaults for display).
pub fn diff_clamped(work_dir: &Path) -> String {
    // Gate-generated artifacts (node_modules, target, venvs) must never leak
    // into the captured review diff; excluded first so `git add -N` skips them.
    ensure_artifact_excludes(work_dir);
    // `git add -N` records intent-to-add so untracked files appear in diff;
    // local to the worktree index and harmless. Best-effort.
    let _ = git(work_dir, &["add", "-N", "."]);
    let full = git(work_dir, &["--no-pager", "diff", "HEAD"])
        .unwrap_or_default()
        .replace('\r', "");
    if full.chars().count() <= DIFF_CAP {
        full
    } else {
        full.chars().take(DIFF_CAP).collect()
    }
}

/// Test/tooling convenience (the old two-arg `diff`): the clamped diff for a
/// worktree. `repo_root` is ignored — the diff is inherently worktree-local.
pub fn diff(repo_root: &Path, work_dir: &Path) -> String {
    let _ = repo_root;
    diff_clamped(work_dir)
}

/// Guard a branch ref before it reaches an argv slot: refs first argv position
/// would be parsed as a git option if they began with `-`, and an empty ref
/// is meaningless. Branch names here are always `katban/<hex>` we minted, but
/// defense-in-depth keeps a malformed ref from ever confusing git.
fn validate_branch(branch: &str) -> Result<(), String> {
    if branch.trim().is_empty() || branch.starts_with('-') {
        return Err(format!("refusing branch ref '{branch}'"));
    }
    Ok(())
}

/// Option B — "pin the commit": commit the card's worktree to its branch so
/// the admin has a real commit to merge or discard after review, instead of a
/// teardown that throws the agent's changes away.
///
/// The worktree was created detached, so this creates (or resets) the card's
/// branch at the current worktree HEAD and commits the staged tree onto it.
/// `git checkout -B` is safe for retries — each successful run's commit simply
/// resets the branch to that run's result — because the branch is only ever
/// checked out in this card's single worktree. Returns the commit hash.
pub fn commit_card(
    repo_root: &Path,
    work_dir: &Path,
    branch: &str,
    message: &str,
) -> Result<String, String> {
    validate_branch(branch)?;
    git(work_dir, &["checkout", "-B", branch])?;
    // Gate-generated artifacts must not be committed into the card's branch;
    // excluded before `add -A` so it stages everything else.
    ensure_artifact_excludes(work_dir);
    // `add -A` records additions, modifications, and deletions (where the
    // runner's diff helper used `add -N` intent-to-add for display only).
    git(work_dir, &["add", "-A"])?;
    // Single `-m` arg — git parses `--message`'s value, never as an option.
    git(work_dir, &["commit", "-m", message])?;
    git(repo_root, &["rev-parse", branch])
}

/// Merge the (pinned) card branch into the project's current checkout branch.
/// On conflict the merge is aborted so the admin's checkout is never left
/// mid-merge: they resolve manually and the card stays in review. `--no-edit`
/// reuses the merge message. Only hits the crop of git that a locked board
/// write should, never the user's uncommitted state.
pub fn merge_branch(repo_root: &Path, branch: &str) -> Result<(), String> {
    validate_branch(branch)?;
    match git(repo_root, &["merge", "--no-edit", branch]) {
        Ok(_) => Ok(()),
        Err(e) => {
            // Abort only if THIS merge actually started (a `MERGE_HEAD` exists);
            // never unwind a merge the user was already mid-way through.
            if git(repo_root, &["rev-parse", "-q", "--verify", "MERGE_HEAD"]).is_ok() {
                let _ = git(repo_root, &["merge", "--abort"]);
            }
            Err(e)
        }
    }
}

/// Force-delete a card's leftover branch. Safe here because the branch is one
/// we minted (`katban/<hex>`) and only the runner/commit flow ever creates it;
/// `-D` handles the unmerged case (discarding an unreviewed card).
pub fn delete_branch(repo_root: &Path, branch: &str) -> Result<(), String> {
    validate_branch(branch)?;
    git(repo_root, &["branch", "-D", branch]).map(|_| ())
}

/// Resolve a ref to its full SHA, or `None` if it doesn't exist (the card's
/// branch hasn't been created / was deleted). Read-only test/status helper.
pub fn rev_parse(repo_root: &Path, reference: &str) -> Option<String> {
    git(repo_root, &["rev-parse", reference]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_home<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("CLAWDE_HOME").ok();
        std::env::set_var("CLAWDE_HOME", dir);
        let result = f();
        match previous {
            Some(value) => std::env::set_var("CLAWDE_HOME", value),
            None => std::env::remove_var("CLAWDE_HOME"),
        }
        result
    }

    fn init_repo(dir: &Path) {
        let out = std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(dir)
            .output();
        assert!(out.is_ok(), "git not available? {out:?}");
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

    #[test]
    fn worktree_dirs_stay_under_our_root() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let dir = card_worktree_dir("My Repo", "abcd1234");
            assert!(dir.starts_with(worktree_root()));
            assert!(dir.to_string_lossy().contains("My%20Repo"));
            // A hostile project name can't escape the root.
            let evil = card_worktree_dir("../../nope", "abcd1234");
            assert!(evil.starts_with(worktree_root()));
        });
    }

    #[test]
    fn create_and_remove_worktree_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        with_home(tmp.path(), || {
            assert!(is_repo(repo.path()));
            let wt = worktree_root().join("t1");
            create_worktree(repo.path(), &wt, None).unwrap();
            assert!(wt.join("README.md").exists(), "worktree checkout exists");
            // A dirty change in the worktree doesn't block removal (--force).
            std::fs::write(wt.join("foo.txt"), "x").unwrap();
            assert!(std::process::Command::new("git")
                .args(["add", "."])
                .current_dir(&wt)
                .output()
                .unwrap()
                .status
                .success());
            remove_worktree(repo.path(), &wt);
            assert!(!wt.exists());
        });
    }

    #[test]
    fn commit_card_pins_the_work_on_its_own_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        with_home(tmp.path(), || {
            let wt = worktree_root().join("c1");
            create_worktree(repo.path(), &wt, None).unwrap();
            std::fs::write(wt.join("feature.txt"), "new work\n").unwrap();

            let sha = commit_card(repo.path(), &wt, "katban/abcd", "katban: add feature").unwrap();
            assert!(!sha.is_empty());
            // The branch now points at the card's commit.
            assert_eq!(
                git(repo.path(), &["rev-parse", "katban/abcd"]).unwrap(),
                sha
            );
            // The work landed on the branch, not on the repo's main checkout.
            let main = git(repo.path(), &["rev-parse", "HEAD"]).unwrap();
            assert_ne!(sha, main);
            // Git sees the commit (object exists).
            assert!(git(repo.path(), &["cat-file", "-e", &sha]).is_ok());
        });
    }

    #[test]
    fn merge_branch_brings_the_pinned_commit_into_main() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        with_home(tmp.path(), || {
            let wt = worktree_root().join("c2");
            create_worktree(repo.path(), &wt, None).unwrap();
            std::fs::write(wt.join("feature.txt"), "shipped\n").unwrap();
            commit_card(repo.path(), &wt, "katban/ef01", "katban: ship").unwrap();

            // Main does not have the change yet (only the worktree copy does).
            let main_change = git(repo.path(), &["show", "HEAD:feature.txt"]);
            assert!(main_change.is_err(), "feature must not exist on main yet");

            merge_branch(repo.path(), "katban/ef01").unwrap();
            // Main now contains the change.
            let merged = git(repo.path(), &["show", "HEAD:feature.txt"]).unwrap();
            assert_eq!(merged, "shipped");
            // Card branch is fully merged; deleting it is clean (production
            // tears the worktree down at finalize, so it's no longer attached).
            remove_worktree(repo.path(), &wt);
            delete_branch(repo.path(), "katban/ef01").unwrap();
            assert!(git(repo.path(), &["rev-parse", "-q", "--verify", "katban/ef01"]).is_err());
        });
    }

    #[test]
    fn delete_branch_removes_a_card_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        with_home(tmp.path(), || {
            let wt = worktree_root().join("c3");
            create_worktree(repo.path(), &wt, None).unwrap();
            std::fs::write(wt.join("note.txt"), "discard me\n").unwrap();
            commit_card(repo.path(), &wt, "katban/1023", "katban: draft").unwrap();
            // A branch checked out elsewhere can't be deleted; remove the
            // worktree first (the runner always tears it down at finalize).
            remove_worktree(repo.path(), &wt);
            delete_branch(repo.path(), "katban/1023").unwrap();
            assert!(git(repo.path(), &["rev-parse", "-q", "--verify", "katban/1023"]).is_err());
        });
    }

    #[test]
    fn branch_refs_are_validated() {
        assert!(commit_card(Path::new("/tmp"), Path::new("/tmp"), "-x", "m").is_err());
        assert!(commit_card(Path::new("/tmp"), Path::new("/tmp"), "", "m").is_err());
    }

    #[test]
    fn diff_summary_counts_unified_diff_lines() {
        let diff = "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1,2 @@\n-old\n+new\n+added\n"
            .to_string();
        let summary = diff_summary(&diff);
        assert_eq!(summary.files_changed, 1);
        assert_eq!(summary.additions, 2);
        assert_eq!(summary.deletions, 1);

        let binary =
            "diff --git a/image.png b/image.png\nBinary files a/image.png and b/image.png differ\n";
        let summary = diff_summary(binary);
        assert_eq!(summary.files_changed, 1);
        assert_eq!(summary.additions, 0);
        assert_eq!(summary.deletions, 0);
    }

    #[test]
    fn diff_reports_worktree_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        with_home(tmp.path(), || {
            let wt = worktree_root().join("t2");
            create_worktree(repo.path(), &wt, None).unwrap();
            // Append to a tracked file so `git diff HEAD` sees the change.
            std::fs::write(wt.join("README.md"), "# demo\n\nchanged\n").unwrap();
            let d = diff(repo.path(), &wt);
            assert!(d.contains("changed"), "diff: {d}");
            assert!(d.contains("README.md"), "diff: {d}");
            assert!(!d.contains('\r'));
        });
    }

    #[test]
    fn gate_artifacts_are_excluded_from_diff_and_commit() {
        // #6 — a repo without a .gitignore: gate-generated artifacts
        // (node_modules, .venv, build output) must not leak into the captured
        // diff or the card's pinned commit.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        with_home(tmp.path(), || {
            let wt = worktree_root().join("t3");
            create_worktree(repo.path(), &wt, None).unwrap();
            // The agent's real change plus gate-generated artifacts.
            std::fs::write(wt.join("README.md"), "# demo\n\nfeature\n").unwrap();
            std::fs::create_dir_all(wt.join("node_modules/pkg")).unwrap();
            std::fs::write(wt.join("node_modules/pkg/index.js"), "artifact\n").unwrap();
            std::fs::create_dir_all(wt.join(".venv/bin")).unwrap();
            std::fs::write(wt.join(".venv/bin/python3"), "artifact\n").unwrap();

            let d = diff_clamped(&wt);
            assert!(d.contains("feature"), "diff: {d}");
            assert!(!d.contains("node_modules"), "diff: {d}");
            assert!(!d.contains(".venv"), "diff: {d}");

            // The pinned commit carries the real change, not the artifacts.
            let sha = commit_card(repo.path(), &wt, "katban/abcd", "katban: feature").unwrap();
            let tree_file =
                |path: &str| git(repo.path(), &["show", &format!("{sha}:{path}")]).map(|_| ());
            assert!(tree_file("README.md").is_ok());
            assert!(tree_file("node_modules/pkg/index.js").is_err());
            assert!(tree_file(".venv/bin/python3").is_err());

            // The excludes must NEVER leak into the repo's shared
            // .git/info/exclude (which would affect the user's main checkout):
            // they live in this worktree's own git-metadata dir instead.
            let shared =
                std::fs::read_to_string(repo.path().join(".git/info/exclude")).unwrap_or_default();
            assert!(
                !shared.contains("katban"),
                "shared info/exclude must not be polluted: {shared}"
            );
        });
    }
}
