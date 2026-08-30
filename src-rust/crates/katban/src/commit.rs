//! Option B — "pin then merge-or-discard": the admin actions that turn a
//! review card's pinned commit into a real git change (spec §20.3 #3).
//!
//! The runner commits a successful card's work to its branch (`katban/<id>` in
//! `card.branch`) at finalize and records the hash in `card.commit`, then tears
//! the worktree down — so nothing lingers. This module owns the two review
//! decisions:
//! - `merge_card`: merge the pinned branch into the project's current checkout
//!   branch (fast-forward when possible), delete the branch, mark the card
//!   Done. Dependents unblock automatically via the existing readiness logic.
//! - `discard_card`: delete the branch (if any) and mark the card Done.
//!
//! Each action takes the per-project `BoardLock`, does its git work while the
//! board is locked (so a concurrent advance/trash can't race the branch state),
//! then persists. Merge conflicts abort the merge and report — the admin
//! resolves manually and the card stays in review.

use crate::board::{self, BoardLock, CardStatus};

/// Merge a review card's pinned branch into the project's current branch and
/// close the card. Errors carry a human-readable reason (no repo, no commit,
/// merge conflict, missing card) — callers surface them as-is.
pub fn merge_card(project: &str, card_id: &str) -> Result<(), String> {
    let _guard = BoardLock::acquire(project).map_err(|e| e.to_string())?;
    let mut board = board::load_board(project)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no board for project '{project}'"))?;

    let (branch, commit) = {
        let card = board
            .card(card_id)
            .ok_or_else(|| format!("no card with id '{card_id}'"))?;
        if card.status != CardStatus::Review {
            return Err(format!(
                "card is not in review ({}) — only reviewed cards can be merged",
                status_name(card.status)
            ));
        }
        let commit = card
            .commit
            .clone()
            .ok_or_else(|| "card has no pinned commit to merge".to_string())?;
        let branch = card
            .branch
            .clone()
            .ok_or_else(|| "card has no branch to merge".to_string())?;
        (branch, commit)
    };

    let repo = crate::projects::repo_root(project)
        .ok_or_else(|| format!("project '{project}' has no registered git repo"))?;
    crate::git::merge_branch(&repo, &branch)?;

    // The branch is fully merged (or the merge errored out above); tidy it up.
    clean_card_worktree(&repo, project, card_id);
    let _ = crate::git::delete_branch(&repo, &branch);
    let short = commit
        .get(..commit.len().min(12))
        .unwrap_or(&commit)
        .to_string();
    board.set_status(card_id, CardStatus::Done);
    if let Some(c) = board.cards.iter_mut().find(|c| c.id == card_id) {
        c.result = Some(format!("merged {short}"));
        c.updated_at = crate::guest::now_secs();
    }
    board::save_board(&board, project).map_err(|e| e.to_string())
}

/// Discard a card's work: delete its pinned branch (if any) and mark the card
/// Done. Used by both a Review "trash" and a plain card remove/archive — the
/// branch-delete is a no-op for cards that produced no commit.
pub fn discard_card(project: &str, card_id: &str) -> Result<(), String> {
    let _guard = BoardLock::acquire(project).map_err(|e| e.to_string())?;
    let mut board = board::load_board(project)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no board for project '{project}'"))?;

    let branch = board.card(card_id).and_then(|c| c.branch.clone());
    if let Some(branch) = branch {
        if let Some(repo) = crate::projects::repo_root(project) {
            // A crashed runner can leave this branch checked out in a registered
            // worktree; `git branch -D` would refuse and leak the ref forever, so
            // tear the card's worktree down first (no-op when none exists).
            clean_card_worktree(&repo, project, card_id);
            let _ = crate::git::delete_branch(&repo, &branch);
        }
    }
    if !board.trash_card(card_id) {
        return Err(format!("no card with id '{card_id}'"));
    }
    board::save_board(&board, project).map_err(|e| e.to_string())
}

/// Best-effort removal of a card's worktree checkout before its branch is
/// deleted. The runner normally tears the worktree down at finalize; this
/// covers the crash case (orphaned worktree registered on the card's branch,
/// which would make `git branch -D` fail and leak the ref). Only ever touches
/// paths under our owned `worktree_root()` (`remove_worktree` guards that).
fn clean_card_worktree(repo: &std::path::Path, project: &str, card_id: &str) {
    crate::git::remove_worktree(repo, &crate::git::card_worktree_dir(project, card_id));
}

fn status_name(status: CardStatus) -> &'static str {
    match status {
        CardStatus::Backlog => "backlog",
        CardStatus::Queued => "queued",
        CardStatus::Running => "running",
        CardStatus::Blocked => "blocked",
        CardStatus::Review => "review",
        CardStatus::Failed => "failed",
        CardStatus::Done => "done",
    }
}

/// A convenience for tests: set up a board with one `review` card that has a
/// pinned commit on its branch and its worktree removed (the steady state an
/// admin sees after the runner finalizes). Returns the card id. Callers run it
/// inside their own `with_home` so `CLAWDE_HOME` already points at the sandbox.
#[cfg(test)]
fn reviewed_card_with_pin(repo: &std::path::Path, project: &str) -> (String, String) {
    let mut board = board::Board::new();
    let id = board.add_card("ship a feature");
    board.set_status(&id, CardStatus::Review);
    board.cards.iter_mut().find(|c| c.id == id).unwrap().branch = Some(format!("katban/{id}"));
    let wt = crate::git::card_worktree_dir(project, &id);
    std::fs::create_dir_all(&wt).unwrap();
    crate::git::create_worktree(repo, &wt, None).unwrap();
    std::fs::write(wt.join("feature.txt"), "shipped\n").unwrap();
    // Pin the commit exactly like the runner's finalize, then remove the
    // worktree the way finalize always does.
    let sha = crate::git::commit_card(repo, &wt, &format!("katban/{id}"), "katban: ship").unwrap();
    let id_own = id.clone();
    let sha_own = sha.clone();
    board.cards.iter_mut().find(|c| c.id == id).unwrap().commit = Some(sha);
    board::save_board(&board, project).unwrap();
    crate::git::remove_worktree(repo, &wt);
    (id_own, sha_own)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::{load_board, CardStatus};

    fn with_home<T>(dir: &std::path::Path, f: impl FnOnce() -> T) -> T {
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

    fn init_repo(dir: &std::path::Path) {
        let out = std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(dir)
            .output();
        assert!(out.is_ok(), "git not available? {out:?}");
        std::fs::write(dir.join("README.md"), "# demo\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(dir)
            .output()
            .unwrap();
    }

    #[test]
    fn merge_card_lands_the_pinned_commit_and_closes_the_card() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        with_home(tmp.path(), || {
            crate::projects::set_repo_root("default", repo.path()).unwrap();
            let (id, sha) = reviewed_card_with_pin(repo.path(), "default");

            merge_card("default", &id).unwrap();
            let board = load_board("default").unwrap().unwrap();
            assert_eq!(board.card(&id).unwrap().status, CardStatus::Done);
            assert!(
                board
                    .card(&id)
                    .unwrap()
                    .result
                    .as_deref()
                    .unwrap()
                    .starts_with("merged"),
                "result: {:?}",
                board.card(&id).unwrap().result
            );
            // The change is now fast-forwarded onto main (HEAD == the pinned
            // commit), and the card branch is deleted.
            assert_eq!(
                crate::git::rev_parse(repo.path(), "HEAD"),
                Some(sha),
                "card commit fused into main"
            );
            assert!(crate::git::rev_parse(repo.path(), &format!("katban/{id}")).is_none());
        });
    }

    #[test]
    fn merge_card_requires_review_and_a_pinned_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        with_home(tmp.path(), || {
            crate::projects::set_repo_root("default", repo.path()).unwrap();

            // Backlog card (no commit) can't be merged.
            let mut board = board::Board::new();
            let id = board.add_card("backlog");
            board::save_board(&board, "default").unwrap();
            let err = merge_card("default", &id).unwrap_err();
            assert!(err.contains("review"), "err: {err}");

            // Review but no commit: still reject.
            let mut board = board::load_board("default").unwrap().unwrap();
            board.set_status(&id, CardStatus::Review);
            board::save_board(&board, "default").unwrap();
            let err = merge_card("default", &id).unwrap_err();
            assert!(err.contains("no pinned commit"), "err: {err}");
        });
    }

    #[test]
    fn discard_card_deletes_the_branch_and_archives() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        with_home(tmp.path(), || {
            crate::projects::set_repo_root("default", repo.path()).unwrap();
            let (id, _sha) = reviewed_card_with_pin(repo.path(), "default");
            assert!(crate::git::rev_parse(repo.path(), &format!("katban/{id}")).is_some());

            discard_card("default", &id).unwrap();
            let board = load_board("default").unwrap().unwrap();
            assert_eq!(board.card(&id).unwrap().status, CardStatus::Done);
            assert!(
                crate::git::rev_parse(repo.path(), &format!("katban/{id}")).is_none(),
                "branch cleaned up on discard"
            );
            // Main is untouched by a discard.
            let main = crate::git::rev_parse(repo.path(), "HEAD");
            assert!(main.is_some());
        });
    }
}
