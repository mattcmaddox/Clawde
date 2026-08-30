//! Katban board runner (spec §12): execute ready cards as headless clawde
//! subprocesses, each in its own git worktree.
//!
//! Cline Kanban's model, mapped onto Clawde's own CLI: a ready card
//! (dependencies met, a free parallel slot) gets a fresh worktree off the
//! project repo, a headless `clawde --print "<prompt>"` runs inside it, and
//! the card's status tracks the run: `running` while it works, then `review`
//! on success or `failed` on a non-zero exit. Transient failures auto-retry up
//! to the board's `auto_retry` cap (§16a E6), then the card stays failed and
//! its dependents stay blocked via `board::blocked_reason`.
//!
//! Scheduler semantics:
//! - Ready = `ready_to_run` (Backlog/Queued/Failed, deps done) AND
//!   `retries <= auto_retry` (the count of past failures; a fresh card with 0
//!   retries runs immediately) AND not already running/inflight. So
//!   `auto_retry` is the number of retries after the initial attempt —
//!   `auto_retry: 2` runs a card up to 3 times total (§5a "tries again twice"),
//!   and `auto_retry: 0` still runs it once (no retries).
//! - Parallelism honors `parallel_cap`, counting every `running` card
//!   (admin-set or runner-spawned) against the cap.
//! - On start, any `running` card is reset to `queued` (crash recovery): the
//!   new process holds no handle for a card a killed runner left running, so
//!   it would pin a slot forever. The runner is the board's sole executor.
//! - Every load->change->save holds the per-project `BoardLock`, so the runner
//!   never races the web UI / CLI / `/katban`.
//! - A spawned card is marked `running` (with a worktree dir) under the lock
//!   before its subprocess starts, so its slot is reserved atomically, and
//!   finalization only fires if the card is *still* running: if the admin
//!   moved it meanwhile, their edit wins.

use crate::board::{self, BoardLock, CardStatus};
use crate::git;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// How long the scheduler sleeps between poll cycles.
const POLL_INTERVAL: Duration = Duration::from_millis(1500);

/// Executor abstraction so tests can substitute a scripted runner for the real
/// headless clawde subprocess.
pub trait CardExecutor: Send + Sync {
    fn execute(&self, work_dir: &Path, prompt: &str) -> Result<String, String>;
}

/// Real executor: spawn the current clawde binary headless in the worktree.
pub struct ClawdeExecutor {
    clawde_bin: PathBuf,
}

impl ClawdeExecutor {
    pub fn new() -> Self {
        ClawdeExecutor {
            clawde_bin: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("clawde")),
        }
    }
}

impl Default for ClawdeExecutor {
    fn default() -> Self {
        ClawdeExecutor::new()
    }
}

impl CardExecutor for ClawdeExecutor {
    fn execute(&self, work_dir: &Path, prompt: &str) -> Result<String, String> {
        let output = std::process::Command::new(&self.clawde_bin)
            .current_dir(work_dir)
            .args(["--print", prompt])
            .output()
            .map_err(|e| format!("could not start clawde: {e}"))?;
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            let digest: Vec<&str> = text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .take(4)
                .collect();
            Ok(if digest.is_empty() {
                "completed".to_string()
            } else {
                digest.join("\n")
            })
        } else {
            let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Err(if err.is_empty() {
                "non-zero exit".to_string()
            } else {
                // Compact stderr tail for the card's result field.
                err.split_whitespace()
                    .rev()
                    .take(60)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
        }
    }
}

/// Run the board scheduler for one project until cancelled. `spawn_fn` lets
/// tests inject the executor; in production it's always `ClawdeExecutor`.
pub async fn run_loop(project: &str, spawn_fn: Arc<dyn CardExecutor>) -> anyhow::Result<()> {
    recover_stale_running(project)?;

    let mut inflight: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();

    loop {
        tokio::time::sleep(POLL_INTERVAL).await;

        // Reap finished agents (finalization ran inside each task).
        inflight.retain(|_, handle| !handle.is_finished());

        // Spawn whatever is now ready within the free parallel slots.
        let repo_root = crate::projects::repo_root(project);
        spawn_ready(project, repo_root.as_deref(), &mut inflight, &spawn_fn).await;
    }
}

/// Run one scheduler per *registered* project and keep picking up new ones as
/// they are registered — the `board serve --run all` refresh path.
///
/// Unlike `run_loop` (which owns a single project forever), this coordinator
/// resolves the current registry, spawns one `run_loop` per project, and then
/// reconciles on every poll: a project newly registered (e.g. via `clawde
/// katban project set`) gets a scheduler within a poll cycle, with no restart
/// and no re-`expose`. The initial empty set is fine — if nothing is
/// registered yet it just waits and joins the first registration. Schedulers
/// are never torn down (a removed project's loop just goes idle); the unit's
/// `Restart=always` covers the whole process on a real crash.
pub async fn run_all(spawn_fn: Arc<dyn CardExecutor>) -> anyhow::Result<()> {
    let mut running: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();

    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        // Reap finished agents (a scheduler exits only on a hard error; keep
        // the board process healthy by not wedging on a zombie handle).
        running.retain(|_, handle| !handle.is_finished());

        // `registered_projects()` is already filtered to projects with a
        // valid git repo, so anything returned here is immediately runnable.
        for project in crate::projects::registered_projects() {
            if running.contains_key(&project) {
                continue;
            }
            tracing::info!(project = %project, "board runner joining project (--run all)");
            let executor = spawn_fn.clone();
            let name = project.clone();
            let log_name = project.clone();
            let handle = tokio::spawn(async move {
                if let Err(error) = run_loop(&name, executor).await {
                    tracing::error!(project = %log_name, error = %error, "board runner exited");
                }
            });
            running.insert(project, handle);
        }
    }
}

/// Reset any card stuck in `running` to `queued` at runner start (crash
/// recovery) — a fresh process holds no handle for it, so it would pin a slot
/// forever.
fn recover_stale_running(project: &str) -> anyhow::Result<()> {
    let _guard = BoardLock::acquire(project)?;
    let mut board = board::load_board(project)?.unwrap_or_default();
    let mut changed = false;
    let mut orphaned = Vec::new();
    for card in &mut board.cards {
        if card.status == CardStatus::Running {
            card.status = CardStatus::Queued;
            card.updated_at = crate::guest::now_secs();
            changed = true;
            // The crashed runner may have left this card's worktree behind;
            // remember it so we can tear it down after the lock releases.
            if let Some(dir) = card.work_dir.clone() {
                if Path::new(&dir).starts_with(git::worktree_root()) {
                    orphaned.push(PathBuf::from(dir));
                }
            }
            card.work_dir = None;
        }
    }
    if changed {
        board::save_board(&board, project)?;
    }
    drop(_guard);
    for dir in orphaned {
        cleanup_worktree(project, &dir);
    }
    Ok(())
}

/// Decide which cards to start this cycle and launch them. Card selection and
/// the `running` reservation happen under the board lock; the long-running
/// subprocesses start only after the lock is released.
async fn spawn_ready(
    project: &str,
    repo_root: Option<&Path>,
    inflight: &mut HashMap<String, tokio::task::JoinHandle<()>>,
    executor: &Arc<dyn CardExecutor>,
) {
    // Guard held only for the load->select->reserve->save section.
    let to_spawn: Vec<(String, PathBuf, Option<String>)> = {
        let _guard = match BoardLock::acquire(project) {
            Ok(g) => g,
            Err(_) => return,
        };
        let mut board = match board::load_board(project) {
            Ok(Some(b)) => b,
            _ => return,
        };

        // Running set: every `running` card plus ids already inflight.
        let mut running_ids: HashSet<String> = board
            .cards
            .iter()
            .filter(|c| c.status == CardStatus::Running)
            .map(|c| c.id.clone())
            .collect();
        running_ids.extend(inflight.keys().cloned());

        let slots = board.parallel_cap.saturating_sub(running_ids.len());
        if slots == 0 {
            return;
        }
        let mut candidates: Vec<String> = board
            .cards
            .iter()
            .filter(|c| {
                !running_ids.contains(&c.id)
                    && c.retries <= board.auto_retry
                    && board.ready_to_run(&c.id)
            })
            .map(|c| c.id.clone())
            .collect();
        candidates.sort_by_key(|id| board.card(id).map(|c| c.created_at).unwrap_or(0));
        candidates.truncate(slots);

        // Reserve slots: mark each selected card running + assign a worktree
        // dir, all in this single load->change->save.
        for id in &candidates {
            board.set_status(id, CardStatus::Running);
            let work_dir = git::card_worktree_dir(project, id);
            if let Some(c) = board.cards.iter_mut().find(|c| &c.id == id) {
                c.work_dir = Some(work_dir.to_string_lossy().into_owned());
                if c.branch.is_none() {
                    c.branch = Some(format!("katban/{id}"));
                }
            }
        }
        // For each spawn we also record the card's pinned branch when this is a
        // real follow-up (review feedback was sent) so `run_one_card` bases the
        // new worktree on the card's own prior commit — the agent iterates on
        // its work, not from HEAD. On a *first* run the branch name is invented
        // here purely as a placeholder and does not exist yet in git, so we
        // must NOT base on it: only use the branch when `followup_feedback` is
        // set (which `send_feedback_to_agent` stores on the card).
        let spawned: Vec<(String, PathBuf, Option<String>)> = candidates
            .iter()
            .map(|id| {
                let branch = board
                    .card(id)
                    .filter(|c| c.followup_feedback.is_some())
                    .and_then(|c| c.branch.clone());
                (id.clone(), git::card_worktree_dir(project, id), branch)
            })
            .collect();
        if !spawned.is_empty() {
            if let Err(e) = board::save_board(&board, project) {
                tracing::warn!(project, error = %e, "could not persist running cards");
                return;
            }
        }
        spawned // _guard drops here, releasing the lock before subprocesses start.
    };

    for (id, work_dir, base_ref) in to_spawn {
        if inflight.contains_key(&id) {
            continue;
        }
        let _ = std::fs::create_dir_all(&work_dir);
        // Pull the prompt fresh (the board just persisted it as running). A
        // follow-up run appends the review-feedback block (via
        // `Card::effective_prompt`) so the agent knows what the reviewer asked
        // it to change.
        let prompt = board::load_board(project)
            .ok()
            .flatten()
            .map(|b| match b.card(&id) {
                Some(c) => c.effective_prompt(),
                None => String::new(),
            })
            .unwrap_or_default();
        let project = project.to_string();
        let repo_root = repo_root.map(|p| p.to_path_buf());
        let executor = executor.clone();
        let handle = tokio::spawn(async move {
            run_one_card(
                &project,
                repo_root.as_deref(),
                &work_dir,
                &prompt,
                &executor,
                base_ref,
            )
            .await;
        });
        inflight.insert(id, handle);
    }
}

/// Run one card: set up its worktree (creating it when the project has a repo;
/// otherwise an isolated scratch dir under our root), run the agent, finalize.
async fn run_one_card(
    project: &str,
    repo_root: Option<&Path>,
    work_dir: &Path,
    prompt: &str,
    executor: &Arc<dyn CardExecutor>,
    base_ref: Option<String>,
) {
    let card_id = work_dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    // A follow-up run bases its worktree on the card's own pinned branch (the
    // prior commit) so the agent iterates on its work rather than restarting
    // from repo HEAD. `base_ref` is the card branch captured at spawn time.
    let base_ref = base_ref.as_deref();

    // A configured-but-not-a-repo project is a config error: fail the card
    // clearly rather than leaving it running forever.
    if let Some(repo) = repo_root {
        if !git::is_repo(repo) {
            finalize(
                project,
                &card_id,
                work_dir,
                true,
                Some("project repo is not a git repository"),
            );
            return;
        }
        if let Err(e) = git::create_worktree(repo, work_dir, base_ref) {
            finalize(
                project,
                &card_id,
                work_dir,
                true,
                Some(&format!("could not create worktree: {e}")),
            );
            return;
        }
    }

    let outcome = executor.execute(work_dir, prompt);
    let mut failed = outcome.is_err();
    let mut note: Option<String> = outcome
        .as_ref()
        .err()
        .or(outcome.as_ref().ok())
        .map(|s| s.to_string());

    if !failed {
        // Verification gate (board audit option 1): the card's work must pass
        // the project's own checks before it is accepted into review. A failing
        // check fails the card with the check output in its result.
        let gate = crate::verify::run_gate(work_dir).await;
        if !gate.passed {
            failed = true;
            note = Some(gate.detail.clone());
        }
    }

    if !failed {
        // Auto-review pass (board audit option 2): a second headless agent
        // reads the diff and attaches findings as ordinary review comments
        // (best-effort — any failure just skips the pass). Findings are added
        // under the board lock while the card is still running, so they ride
        // into the final review state with it.
        let auto_review_on = board::load_board(project)
            .ok()
            .flatten()
            .map(|b| b.auto_review)
            .unwrap_or(false);
        if auto_review_on {
            let diff = git::diff_clamped(work_dir);
            match crate::verify::auto_review(work_dir, prompt, &diff).await {
                Ok(findings) => {
                    for finding in findings {
                        let text = format!("[auto-review] {}", finding.text);
                        let _ = board::add_review(project, &card_id, finding.line, &text);
                    }
                }
                Err(error) => {
                    tracing::info!(project, card = %card_id, error = %error, "auto-review skipped");
                }
            }
        }
    }

    finalize(project, &card_id, work_dir, failed, note.as_deref());
}

/// Persist a card's final state after its agent exits. Only transitions a card
/// still `running`; if the admin moved it meanwhile, their edit is preserved.
/// The card's worktree is ALWAYS removed afterwards (best-effort), on every
/// path — an early return (card moved / lock held / card gone) must not leak
/// the checkout and its git registration, or they accumulate forever.
fn finalize(project: &str, card_id: &str, work_dir: &Path, failed: bool, note: Option<&str>) {
    {
        // The lock scope is separate so the cleanup below runs unconditionally.
        let _guard = BoardLock::acquire(project).ok();
        if let Ok(Some(mut board)) = board::load_board(project) {
            let Some(card) = board.cards.iter_mut().find(|c| c.id == card_id) else {
                return cleanup_worktree(project, work_dir);
            };
            if card.status != CardStatus::Running {
                // Admin moved it — their edit wins; still clean up the checkout.
                return cleanup_worktree(project, work_dir);
            }
            let note = note.unwrap_or("completed").to_string();
            // Capture the diff BEFORE the worktree is removed so review works
            // even after the checkout is gone. Harmless if it returns empty.
            let diff = crate::git::diff_clamped(work_dir);
            if failed {
                card.retries += 1;
                card.status = CardStatus::Failed;
            } else {
                // Option B — pin (or re-pin) the commit: commit the worktree to
                // the card's branch while the checkout still exists, so review
                // has a real, complete commit to merge or discard. `commit_card`
                // resets the branch to this run's tree (`checkout -B`), so a
                // follow-up run (review feedback sent back to the agent)
                // replaces the prior pinned commit — merging a reviewed follow-up
                // never silently drops the changes the agent made in response to
                // review. Only when this run actually changed the tree (a
                // non-empty diff vs its base) do we commit; a no-op follow-up
                // keeps the prior commit as the net result. Falls back to
                // diff-only review if the pin fails (no registered repo / git
                // hiccup) rather than failing the card.
                if !diff.is_empty() {
                    if let Some(repo) = crate::projects::repo_root(project) {
                        if let Some(branch) = card.branch.clone() {
                            match crate::git::commit_card(
                                &repo,
                                work_dir,
                                &branch,
                                &commit_message(&card.prompt),
                            ) {
                                Ok(sha) => {
                                    card.commit = Some(sha);
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        project,
                                        card = %card.id,
                                        error = %e,
                                        "could not pin card commit — diff-only review"
                                    );
                                }
                            }
                        }
                    }
                }
                // A review follow-up (feedback was pending when this run started)
                // is now complete: consume the feedback so a later manual requeue
                // doesn't re-append stale instructions, and so the next run's
                // worktree base falls back to HEAD rather than the branch this
                // run already folded in.
                card.followup_feedback = None;
                card.status = CardStatus::Review;
            }
            card.result = Some(note);
            if !diff.is_empty() {
                card.diff = Some(diff);
            }
            // The checkout is about to be torn down; drop the stale path.
            card.work_dir = None;
            card.updated_at = crate::guest::now_secs();
            let _ = board::save_board(&board, project);
        }
    } // lock released (Option<BoardLock> dropped)
    cleanup_worktree(project, work_dir);
}

/// A commit message for a card's pinned commit: the first line of the prompt,
/// prefixed and length-capped so `git log` stays readable.
fn commit_message(prompt: &str) -> String {
    let first = prompt.lines().next().unwrap_or("card").trim();
    let mut msg = format!("katban: {first}");
    msg.truncate(80);
    msg
}

/// Best-effort removal of a card's worktree checkout + git registration. Only
/// ever touches paths under our owned `worktree_root()` (defense in depth: a
/// malformed work_dir can never escalate into deleting something else).
fn cleanup_worktree(project: &str, work_dir: &Path) {
    if !work_dir.starts_with(git::worktree_root()) {
        return;
    }
    if let Some(repo) = crate::projects::repo_root(project) {
        git::remove_worktree(&repo, work_dir);
    } else {
        let _ = std::fs::remove_dir_all(work_dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::Board;
    use std::path::Path;

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

    struct FakeExecutor {
        outcomes: Vec<bool>, // true = success per call (last wins for repeats)
    }
    impl FakeExecutor {
        fn new(success: bool) -> Self {
            FakeExecutor {
                outcomes: vec![success],
            }
        }
    }
    impl CardExecutor for FakeExecutor {
        fn execute(&self, _work_dir: &Path, _prompt: &str) -> Result<String, String> {
            if self.outcomes.is_empty() {
                return Ok("done".into());
            }
            match self.outcomes.last().copied().unwrap_or(true) {
                true => Ok("done".into()),
                false => Err("boom".into()),
            }
        }
    }

    #[test]
    fn stale_running_reset_to_queued() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let mut board = Board::new();
            let a = board.add_card("a");
            board.set_status(&a, CardStatus::Running);
            board::save_board(&board, "default").unwrap();
            recover_stale_running("default").unwrap();
            let b = board::load_board("default").unwrap().unwrap();
            assert_eq!(b.card(&a).unwrap().status, CardStatus::Queued);
        });
    }

    #[test]
    fn failed_card_increments_retries_and_stays_failed_after_cap() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let mut board = Board::new();
            board.auto_retry = 1;
            let a = board.add_card("a");
            board::save_board(&board, "default").unwrap();

            let run_and_fail = |id: &str| {
                let mut b = board::load_board("default").unwrap().unwrap();
                b.set_status(id, CardStatus::Running);
                board::save_board(&b, "default").unwrap();
                let wt = git::card_worktree_dir("default", id);
                std::fs::create_dir_all(&wt).unwrap();
                finalize("default", id, &wt, true, Some("boom"));
            };

            // Attempt 1 fails -> retries=1, still within budget (1 <= cap 1).
            run_and_fail(&a);
            let b = board::load_board("default").unwrap().unwrap();
            let card = b.card(&a).unwrap();
            assert_eq!(card.status, CardStatus::Failed);
            assert_eq!(card.retries, 1);
            assert_eq!(card.result.as_deref(), Some("boom"));
            assert!(b.ready_to_run(&a), "one retry left");

            // Attempt 2 (the retry) fails -> retries=2 > cap 1, stays failed.
            run_and_fail(&a);
            let b = board::load_board("default").unwrap().unwrap();
            let card = b.card(&a).unwrap();
            assert_eq!(card.retries, 2);
            assert!(!b.ready_to_run(&a), "retry budget exhausted");
        });
    }

    #[test]
    fn success_marks_review() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let mut board = Board::new();
            let a = board.add_card("a");
            board::save_board(&board, "default").unwrap();
            let mut b = board::load_board("default").unwrap().unwrap();
            b.set_status(&a, CardStatus::Running);
            board::save_board(&b, "default").unwrap();

            let wt = git::card_worktree_dir("default", &a);
            std::fs::create_dir_all(&wt).unwrap();
            finalize("default", &a, &wt, false, Some("all tests pass"));
            let b = board::load_board("default").unwrap().unwrap();
            let card = b.card(&a).unwrap();
            assert_eq!(card.status, CardStatus::Review);
            assert_eq!(card.result.as_deref(), Some("all tests pass"));
        });
    }

    #[test]
    fn finalize_removes_worktree_even_when_card_no_longer_running() {
        // The worktree leak regression: if the admin moved the card while the
        // agent ran (finalize's early return), the checkout + git registration
        // must still be torn down — not left to accumulate forever.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        with_home(tmp.path(), || {
            let mut board = Board::new();
            let a = board.add_card("a");
            board::save_board(&board, "default").unwrap();
            let wt = git::card_worktree_dir("default", &a);
            std::fs::create_dir_all(&wt).unwrap();
            git::create_worktree(repo.path(), &wt, None).unwrap();
            assert!(wt.exists());

            // Card is not running (admin moved it to Done) -> early return path.
            let mut b = board::load_board("default").unwrap().unwrap();
            b.set_status(&a, CardStatus::Done);
            board::save_board(&b, "default").unwrap();
            finalize("default", &a, &wt, true, Some("boom"));
            assert!(
                !wt.exists(),
                "worktree must be cleaned up on the early return"
            );
        });
    }

    #[test]
    fn recover_stale_running_removes_orphaned_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        with_home(tmp.path(), || {
            let mut board = Board::new();
            let a = board.add_card("a");
            board::save_board(&board, "default").unwrap();
            let wt = git::card_worktree_dir("default", &a);
            std::fs::create_dir_all(&wt).unwrap();
            git::create_worktree(repo.path(), &wt, None).unwrap();

            // Simulate a crashed runner: card stuck running with a work_dir set.
            let mut b = board::load_board("default").unwrap().unwrap();
            b.set_status(&a, CardStatus::Running);
            b.cards.iter_mut().find(|c| c.id == a).unwrap().work_dir =
                Some(wt.to_string_lossy().into_owned());
            board::save_board(&b, "default").unwrap();

            recover_stale_running("default").unwrap();
            let b = board::load_board("default").unwrap().unwrap();
            assert_eq!(b.card(&a).unwrap().status, CardStatus::Queued);
            assert!(b.card(&a).unwrap().work_dir.is_none());
            assert!(!wt.exists(), "orphaned worktree must be removed");
        });
    }

    #[test]
    fn finalize_captures_worktree_diff_before_teardown() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        with_home(tmp.path(), || {
            let mut board = Board::new();
            let a = board.add_card("add a feature");
            board::save_board(&board, "default").unwrap();
            let mut b = board::load_board("default").unwrap().unwrap();
            b.set_status(&a, CardStatus::Running);
            board::save_board(&b, "default").unwrap();

            // A real worktree with a real change, as the agent would leave it.
            let wt = git::card_worktree_dir("default", &a);
            std::fs::create_dir_all(&wt).unwrap();
            git::create_worktree(repo.path(), &wt, None).unwrap();
            std::fs::write(wt.join("README.md"), "# demo\n\nfeature\n").unwrap();

            finalize("default", &a, &wt, false, Some("done feature"));
            let b = board::load_board("default").unwrap().unwrap();
            let card = b.card(&a).unwrap();
            assert_eq!(card.status, CardStatus::Review);
            let diff = card.diff.as_deref().expect("diff captured");
            assert!(diff.contains("feature"), "diff: {diff}");
        });
    }

    #[test]
    fn follow_up_run_repins_commit_and_consumes_feedback() {
        // A review follow-up (feedback sent back to the agent) re-runs the card
        // on top of its own branch. Its new changes must be committed (re-pinned)
        // on the branch so `merge_card` lands them — not lost in the torn-down
        // worktree — and the pending feedback must be consumed once the run ends.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        with_home(tmp.path(), || {
            crate::projects::set_repo_root("default", repo.path()).unwrap();
            let mut board = Board::new();
            let a = board.add_card("add a feature");
            board::save_board(&board, "default").unwrap();

            let wt = git::card_worktree_dir("default", &a);
            let branch = format!("katban/{a}");

            // ---- Run 1: based off repo HEAD, produces v1 ----
            std::fs::create_dir_all(&wt).unwrap();
            git::create_worktree(repo.path(), &wt, None).unwrap();
            std::fs::write(wt.join("feature.txt"), "v1\n").unwrap();
            let mut b = board::load_board("default").unwrap().unwrap();
            b.set_status(&a, CardStatus::Running);
            b.cards.iter_mut().find(|c| c.id == a).unwrap().branch = Some(branch.clone());
            b.cards.iter_mut().find(|c| c.id == a).unwrap().work_dir =
                Some(wt.to_string_lossy().into_owned());
            board::save_board(&b, "default").unwrap();
            finalize("default", &a, &wt, false, Some("first run"));

            let b = board::load_board("default").unwrap().unwrap();
            let card = b.card(&a).unwrap();
            assert_eq!(card.status, CardStatus::Review);
            let run1_commit = card.commit.clone().expect("run 1 pins a commit");
            assert_eq!(
                git::rev_parse(repo.path(), &branch).as_deref(),
                Some(run1_commit.as_str())
            );

            // ---- send feedback -> requeues with follow-up pending ----
            board::add_review("default", &a, Some("5".to_string()), "make the feature v2").unwrap();
            board::send_feedback_to_agent("default", &a).unwrap();
            let b = board::load_board("default").unwrap().unwrap();
            assert_eq!(b.card(&a).unwrap().status, CardStatus::Queued);
            assert!(b.card(&a).unwrap().followup_feedback.is_some());
            assert!(!wt.exists(), "finalize removed the run-1 worktree");

            // ---- Run 2: follow-up bases on the card's branch, produces v2 ----
            std::fs::create_dir_all(&wt).unwrap();
            git::create_worktree(repo.path(), &wt, Some(&branch)).unwrap();
            std::fs::write(wt.join("feature.txt"), "v2\n").unwrap();
            let mut b = board::load_board("default").unwrap().unwrap();
            b.set_status(&a, CardStatus::Running);
            b.cards.iter_mut().find(|c| c.id == a).unwrap().work_dir =
                Some(wt.to_string_lossy().into_owned());
            board::save_board(&b, "default").unwrap();
            finalize("default", &a, &wt, false, Some("second run"));

            let b = board::load_board("default").unwrap().unwrap();
            let card = b.card(&a).unwrap();
            assert_eq!(card.status, CardStatus::Review);
            // The follow-up's changes are re-pinned on the branch (a NEW commit),
            // so merging lands v2 rather than silently losing the review work.
            let run2_commit = card.commit.clone().expect("follow-up re-pins a commit");
            assert_ne!(run2_commit, run1_commit);
            assert_eq!(
                git::rev_parse(repo.path(), &branch).as_deref(),
                Some(run2_commit.as_str())
            );
            assert!(card.diff.as_deref().unwrap().contains("v2"));
            // Pending feedback is consumed now that the follow-up completed.
            assert!(card.followup_feedback.is_none());
        });
    }

    #[test]
    fn follow_up_with_no_changes_keeps_prior_commit() {
        // A follow-up whose agent makes no further change is a no-op, not an
        // error: the card still reaches review with the prior pinned commit
        // intact (the net result is unchanged) and the pending feedback is
        // consumed — no empty commit is attempted, no warning spam.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        with_home(tmp.path(), || {
            crate::projects::set_repo_root("default", repo.path()).unwrap();
            let mut board = Board::new();
            let a = board.add_card("add a feature");
            board::save_board(&board, "default").unwrap();

            let wt = git::card_worktree_dir("default", &a);
            let branch = format!("katban/{a}");

            // Run 1: base off HEAD, produces v1, pinned.
            std::fs::create_dir_all(&wt).unwrap();
            git::create_worktree(repo.path(), &wt, None).unwrap();
            std::fs::write(wt.join("feature.txt"), "v1\n").unwrap();
            let mut b = board::load_board("default").unwrap().unwrap();
            b.set_status(&a, CardStatus::Running);
            b.cards.iter_mut().find(|c| c.id == a).unwrap().branch = Some(branch.clone());
            b.cards.iter_mut().find(|c| c.id == a).unwrap().work_dir =
                Some(wt.to_string_lossy().into_owned());
            board::save_board(&b, "default").unwrap();
            finalize("default", &a, &wt, false, Some("first run"));
            let run1_commit = board::load_board("default")
                .unwrap()
                .unwrap()
                .card(&a)
                .unwrap()
                .commit
                .clone()
                .expect("run 1 pins a commit");

            // Feedback -> requeued.
            board::add_review("default", &a, None, "make it better").unwrap();
            board::send_feedback_to_agent("default", &a).unwrap();

            // Run 2: follow-up bases on the branch but the agent changes nothing.
            std::fs::create_dir_all(&wt).unwrap();
            git::create_worktree(repo.path(), &wt, Some(&branch)).unwrap();
            let mut b = board::load_board("default").unwrap().unwrap();
            b.set_status(&a, CardStatus::Running);
            b.cards.iter_mut().find(|c| c.id == a).unwrap().work_dir =
                Some(wt.to_string_lossy().into_owned());
            board::save_board(&b, "default").unwrap();
            finalize("default", &a, &wt, false, Some("second run"));

            let b = board::load_board("default").unwrap().unwrap();
            let card = b.card(&a).unwrap();
            assert_eq!(card.status, CardStatus::Review);
            // Net result unchanged: the prior commit is still the branch tip.
            assert_eq!(card.commit.as_deref(), Some(run1_commit.as_str()));
            assert_eq!(
                git::rev_parse(repo.path(), &branch).as_deref(),
                Some(run1_commit.as_str())
            );
            // Feedback consumed even though nothing changed.
            assert!(card.followup_feedback.is_none());
        });
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn gate_failure_fails_the_card() {
        // Verification gate (option 1): an agent run that passes but leaves a
        // project whose checks fail must send the card to Failed — never Review
        // — with the failing check named in the result.
        fn node_available() -> bool {
            std::process::Command::new("node")
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        }
        if !node_available() {
            eprintln!("skipping: node not installed");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        // Env guard held across the await (test-only; same pattern as the
        // board_server and verify test modules).
        {
            let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var("CLAWDE_HOME").ok();
            std::env::set_var("CLAWDE_HOME", tmp.path());
            std::fs::write(
                tmp.path().join("settings.json"),
                r#"{"config":{"verify":{"auto_lint":false,"timeout_secs":60}}}"#,
            )
            .unwrap();
            crate::projects::set_repo_root("default", repo.path()).unwrap();
            let mut board = Board::new();
            let a = board.add_card("add a feature");
            board::save_board(&board, "default").unwrap();
            let wt = git::card_worktree_dir("default", &a);
            let mut b = board::load_board("default").unwrap().unwrap();
            b.set_status(&a, CardStatus::Running);
            b.cards.iter_mut().find(|c| c.id == a).unwrap().branch = Some(format!("katban/{a}"));
            b.cards.iter_mut().find(|c| c.id == a).unwrap().work_dir =
                Some(wt.to_string_lossy().into_owned());
            board::save_board(&b, "default").unwrap();

            struct FailingChecks;
            impl CardExecutor for FailingChecks {
                fn execute(&self, work_dir: &Path, _prompt: &str) -> Result<String, String> {
                    // The agent "writes" a JS project whose test command fails.
                    std::fs::write(
                        work_dir.join("package.json"),
                        r#"{"scripts":{"test":"node -e \"process.exit(1)\""}}"#,
                    )
                    .unwrap();
                    Ok("done".into())
                }
            }
            let executor: Arc<dyn CardExecutor> = Arc::new(FailingChecks);
            run_one_card(
                "default",
                Some(repo.path()),
                &wt,
                "add a feature",
                &executor,
                None,
            )
            .await;

            let b = board::load_board("default").unwrap().unwrap();
            let card = b.card(&a).unwrap();
            assert_eq!(card.status, CardStatus::Failed);
            let result = card.result.as_deref().unwrap();
            assert!(result.contains("test: npm test"), "result: {result}");
            assert!(card.commit.is_none(), "gate failure must not pin a commit");
            match previous {
                Some(value) => std::env::set_var("CLAWDE_HOME", value),
                None => std::env::remove_var("CLAWDE_HOME"),
            }
        }
    }

    #[test]
    fn finalize_pins_a_commit_on_the_cards_branch() {
        // Option B: a successful card run in a real repo leaves a pinned commit
        // on `katban/<id>` (recorded in `card.commit`) so the admin can merge
        // or discard it after the worktree is torn down.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        with_home(tmp.path(), || {
            crate::projects::set_repo_root("default", repo.path()).unwrap();
            let mut board = Board::new();
            let a = board.add_card("add a feature");
            board::save_board(&board, "default").unwrap();
            let mut b = board::load_board("default").unwrap().unwrap();
            b.set_status(&a, CardStatus::Running);
            b.cards.iter_mut().find(|c| c.id == a).unwrap().branch = Some(format!("katban/{a}"));
            board::save_board(&b, "default").unwrap();

            let wt = git::card_worktree_dir("default", &a);
            std::fs::create_dir_all(&wt).unwrap();
            git::create_worktree(repo.path(), &wt, None).unwrap();
            std::fs::write(wt.join("README.md"), "# demo\n\nfeature\n").unwrap();

            finalize("default", &a, &wt, false, Some("done feature"));

            let b = board::load_board("default").unwrap().unwrap();
            let card = b.card(&a).unwrap();
            assert_eq!(card.status, CardStatus::Review);
            let commit = card.commit.as_deref().expect("a commit is pinned");
            // The branch points at the pinned commit; main is still the base.
            let on_branch = git::rev_parse(repo.path(), &format!("katban/{a}"));
            assert_eq!(on_branch, Some(commit.to_string()));
            let main = git::rev_parse(repo.path(), "HEAD");
            assert_ne!(main, Some(commit.to_string()));
        });
    }

    #[test]
    fn finalize_does_not_pin_a_commit_on_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        with_home(tmp.path(), || {
            let mut board = Board::new();
            let a = board.add_card("a");
            board::save_board(&board, "default").unwrap();
            let mut b = board::load_board("default").unwrap().unwrap();
            b.set_status(&a, CardStatus::Running);
            b.cards.iter_mut().find(|c| c.id == a).unwrap().branch = Some(format!("katban/{a}"));
            board::save_board(&b, "default").unwrap();

            let wt = git::card_worktree_dir("default", &a);
            std::fs::create_dir_all(&wt).unwrap();
            std::fs::write(wt.join("README.md"), "# demo\n\nfeature\n").unwrap();

            finalize("default", &a, &wt, true, Some("boom"));
            let b = board::load_board("default").unwrap().unwrap();
            let card = b.card(&a).unwrap();
            assert_eq!(card.status, CardStatus::Failed);
            assert!(card.commit.is_none(), "failed cards keep no pinned commit");
            assert!(card.diff.is_none(), "failed runs capture no review diff");
        });
    }

    #[test]
    fn finalize_does_not_clobber_admin_edit() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let mut board = Board::new();
            let a = board.add_card("a");
            board::save_board(&board, "default").unwrap();
            let mut b = board::load_board("default").unwrap().unwrap();
            b.set_status(&a, CardStatus::Done);
            board::save_board(&b, "default").unwrap();

            let wt = git::card_worktree_dir("default", &a);
            std::fs::create_dir_all(&wt).unwrap();
            finalize("default", &a, &wt, true, Some("boom"));
            let b = board::load_board("default").unwrap().unwrap();
            assert_eq!(b.card(&a).unwrap().status, CardStatus::Done);
        });
    }

    #[test]
    fn run_all_live_joins_newly_registered_projects() {
        // `board serve --run all` refresh path: a project registered *while*
        // the coordinator is already running must get a scheduler without a
        // restart. We observe the join via the worktrees a scheduler executes:
        // `execute` records the project encoding from the worktree path.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        with_home(tmp.path(), || {
            crate::projects::set_repo_root("app", repo.path()).unwrap();
            let mut board = Board::new();
            board.add_card("first task");
            crate::board::save_board(&board, "app").unwrap();

            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            let seen: Arc<std::sync::Mutex<std::collections::HashSet<String>>> =
                Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
            struct Keyed(Arc<std::sync::Mutex<std::collections::HashSet<String>>>);
            impl CardExecutor for Keyed {
                fn execute(&self, work_dir: &Path, _prompt: &str) -> Result<String, String> {
                    // work_dir is <root>/<project-encoding>/<card>; recover the
                    // project encoding from the parent's file name.
                    if let Some(proj) = work_dir.parent().and_then(|p| p.file_name()) {
                        let mut set = self.0.lock().unwrap_or_else(|e| e.into_inner());
                        set.insert(proj.to_string_lossy().into_owned());
                    }
                    Ok("done".into())
                }
            }
            let executor: Arc<dyn CardExecutor> = Arc::new(Keyed(seen.clone()));

            let coordinator = rt.spawn(run_all(executor.clone()));
            std::thread::sleep(std::time::Duration::from_millis(2500));

            // Register a second project live — it must be joined without a
            // restart and without re-exposing.
            crate::projects::set_repo_root("api2", repo.path()).unwrap();
            let mut board = Board::new();
            board.add_card("api task");
            crate::board::save_board(&board, "api2").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(3500));

            coordinator.abort();
            let seen = seen.lock().unwrap_or_else(|e| e.into_inner());
            // Both the initially-registered and the live-registered project
            // schedulers spawned an execution (observed via their worktree
            // project encoding).
            assert!(
                seen.contains("app") && seen.contains("api2"),
                "live-join failed — schedulers only saw: {seen:?}"
            );
        });
    }

    #[test]
    fn spawn_ready_reserves_slots_and_respects_cap() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            // A small board with 3 ready cards, cap 2 -> only 2 spawned.
            let mut board = Board::new();
            board.parallel_cap = 2;
            let a = board.add_card("a");
            let bs = board.add_card("b");
            let c = board.add_card("c");
            board::save_board(&board, "default").unwrap();

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let executor = Arc::new(FakeExecutor::new(true)) as Arc<dyn CardExecutor>;
            let mut inflight: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
            rt.block_on(spawn_ready("default", None, &mut inflight, &executor));

            // Two cards reserved as running; the third stays backlog.
            let board = board::load_board("default").unwrap().unwrap();
            let running: Vec<String> = board
                .cards
                .iter()
                .filter(|c| c.status == CardStatus::Running)
                .map(|c| c.id.clone())
                .collect();
            assert_eq!(running.len(), 2);
            // The cap counts the running set, so the third card was not started.
            let _ = (a, bs, c);
            // Inflight map has 2 handles.
            // (Spawned tasks run finalize on the current_thread runtime; that's
            // fine — reservation already happened under the lock.)
            assert_eq!(inflight.len(), 2);
        });
    }
}
