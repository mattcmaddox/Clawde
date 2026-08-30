//! Katban board backend (spec §12/§12a): cards, dependencies, queue logic.
//!
//! This is the Cline-Kanban-style data layer, minus the web UI and agent
//! spawning (later slices). Everything here is pure and unit-tested:
//! - Dependencies are cycle-checked at link time (a cycle is refused with a
//!   warning naming the two cards).
//! - Status is partially derived: a card with an unmet or failed dependency is
//!   blocked; `ready_to_run` tells the scheduler what can start.
//! - Boards persist per project under `~/.clawde/katban/boards/<project>/`.

use crate::caddy::write_atomic;
use anyhow::Context;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub const BOARD_VERSION: u32 = 1;
pub const DEFAULT_PARALLEL_CAP: usize = 3;
pub const DEFAULT_PROJECT: &str = "default";
/// Default transient-failure retry count (§16a E16).
pub const DEFAULT_AUTO_RETRY: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum CardStatus {
    #[default]
    Backlog,
    Queued,
    Running,
    Blocked,
    Review,
    Failed,
    Done,
}

impl CardStatus {
    pub fn parse(value: &str) -> Option<CardStatus> {
        match value {
            "backlog" => Some(CardStatus::Backlog),
            "queued" => Some(CardStatus::Queued),
            "running" => Some(CardStatus::Running),
            "blocked" => Some(CardStatus::Blocked),
            "review" => Some(CardStatus::Review),
            "failed" => Some(CardStatus::Failed),
            "done" => Some(CardStatus::Done),
            _ => None,
        }
    }

    /// The next status when a card is "advanced" (moved forward on the
    /// board). Blocked and failed cards go back to queued — the admin
    /// retry action. Done is terminal (no next).
    pub fn next(self) -> Option<CardStatus> {
        match self {
            CardStatus::Backlog => Some(CardStatus::Queued),
            CardStatus::Queued => Some(CardStatus::Running),
            CardStatus::Running => Some(CardStatus::Review),
            CardStatus::Review => Some(CardStatus::Done),
            CardStatus::Blocked | CardStatus::Failed => Some(CardStatus::Queued),
            CardStatus::Done => None,
        }
    }
}

/// A task on the board. The runner slice adds the fields an executing card
/// needs: a `branch` (git worktree branch), a worktree `work_dir` (where the
/// headless clawde subprocess runs), a `retries` counter (transient failures
/// auto-retry up to the configured cap, then the card stays failed), and a
/// `result` summary (the card's last exit note / transcript tail). All new
/// fields are `#[serde(default)]` so older `board.json` files still load.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Card {
    pub id: String,
    pub prompt: String,
    #[serde(default)]
    pub status: CardStatus,
    #[serde(default)]
    pub branch: Option<String>,
    /// Working directory the card's agent ran in (a git worktree). Set when
    /// the runner first spawns the card; cleared when the worktree is removed.
    #[serde(default)]
    pub work_dir: Option<String>,
    /// Transient-failure retry count (the runner slice). A card retries up to
    /// the board's `auto_retry` cap before staying failed.
    #[serde(default)]
    pub retries: u32,
    /// Human digest of the last run: the card's resolved outcome. Stored on the
    /// card so `card list` / the web UI can show what happened without a git
    /// checkout.
    #[serde(default)]
    pub result: Option<String>,
    /// The card's worktree diff at completion (capped), captured before the
    /// worktree is torn down, so review works even after the checkout is gone.
    #[serde(default)]
    pub diff: Option<String>,
    /// Option B — the pinned commit hash of this card's branch (`katban/<id>`
    /// in `branch`), written by the runner at finalize before the worktree is
    /// torn down. Review then decides merge-or-discard: `Some` means there is
    /// a real commit an admin can merge into the project or throw away.
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub updated_at: u64,
}

/// `from` may only start once `to` is Done.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Dependency {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Board {
    pub version: u32,
    #[serde(default)]
    pub cards: Vec<Card>,
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
    #[serde(default = "default_parallel_cap")]
    pub parallel_cap: usize,
    /// Transient failures auto-retry up to this many times before the card
    /// stays failed (spec §16a E6 — never retry user/invalid errors).
    #[serde(default = "default_auto_retry")]
    pub auto_retry: u32,
}

fn default_parallel_cap() -> usize {
    DEFAULT_PARALLEL_CAP
}

fn default_auto_retry() -> u32 {
    DEFAULT_AUTO_RETRY
}

impl Board {
    pub fn new() -> Self {
        Board {
            version: BOARD_VERSION,
            cards: Vec::new(),
            dependencies: Vec::new(),
            parallel_cap: DEFAULT_PARALLEL_CAP,
            auto_retry: DEFAULT_AUTO_RETRY,
        }
    }

    pub fn card(&self, id: &str) -> Option<&Card> {
        self.cards.iter().find(|card| card.id == id)
    }

    pub fn add_card(&mut self, prompt: &str) -> String {
        let now = now_secs();
        // Random hex ids (same scheme as guest links): an id derived from the
        // wall clock (old `now * 1000 + len`) collides when the clock rewinds
        // (NTP) or when 1000 cards are added in one second.
        let id = random_hex(8);
        self.cards.push(Card {
            id: id.clone(),
            prompt: prompt.trim().to_string(),
            status: CardStatus::Backlog,
            branch: None,
            work_dir: None,
            retries: 0,
            result: None,
            diff: None,
            commit: None,
            created_at: now,
            updated_at: now,
        });
        id
    }

    /// Mark a card done (the board's "trash" = archive; its worktree cleanup
    /// is the agent-execution slice's job).
    pub fn trash_card(&mut self, id: &str) -> bool {
        self.set_status(id, CardStatus::Done)
    }

    pub fn set_status(&mut self, id: &str, status: CardStatus) -> bool {
        let Some(card) = self.cards.iter_mut().find(|card| card.id == id) else {
            return false;
        };
        card.status = status;
        card.updated_at = now_secs();
        true
    }

    /// Link `from -> to`: `from` may start only once `to` is Done. Refuses a
    /// self-link and any dependency cycle, naming the two cards involved.
    /// Error messages carry no "cannot link" prefix — callers wrap them.
    pub fn add_dependency(&mut self, from: &str, to: &str) -> Result<(), String> {
        if from == to {
            return Err(format!("'{from}' cannot depend on itself"));
        }
        if self.card(from).is_none_or(|c| is_terminal(c.status)) {
            return Err(format!("'{from}' is not an active card"));
        }
        if self.card(to).is_none_or(|c| is_terminal(c.status)) {
            return Err(format!("'{to}' is not an active card"));
        }
        if self.depends_on(to, from) {
            return Err(format!(
                "'{from}' needs '{to}', but '{to}' already needs '{from}' — that would loop forever"
            ));
        }
        if self
            .dependencies
            .iter()
            .any(|d| d.from == from && d.to == to)
        {
            return Err(format!("'{from}' already depends on '{to}'"));
        }
        self.dependencies.push(Dependency {
            from: from.to_string(),
            to: to.to_string(),
        });
        Ok(())
    }

    pub fn remove_dependency(&mut self, from: &str, to: &str) -> bool {
        let before = self.dependencies.len();
        self.dependencies
            .retain(|d| !(d.from == from && d.to == to));
        self.dependencies.len() != before
    }

    /// Does `a` transitively depend on `b`?
    pub fn depends_on(&self, a: &str, b: &str) -> bool {
        let mut visited = HashSet::new();
        self.depends_on_visit(a, b, &mut visited)
    }

    fn depends_on_visit(&self, a: &str, b: &str, visited: &mut HashSet<String>) -> bool {
        for dep in self.dependencies.iter().filter(|d| d.from == a) {
            if dep.to == b {
                return true;
            }
            if visited.insert(dep.to.clone()) && self.depends_on_visit(&dep.to, b, visited) {
                return true;
            }
        }
        false
    }

    /// Why a card cannot start yet, if it can't: an unmet dependency, or a
    /// dependency that failed.
    pub fn blocked_reason(&self, id: &str) -> Option<String> {
        for dep in self.dependencies.iter().filter(|d| d.from == id) {
            let Some(other) = self.card(&dep.to) else {
                continue;
            };
            match other.status {
                CardStatus::Done => {}
                CardStatus::Failed => {
                    return Some(format!(
                        "waiting on '{}' which failed — review it before this can start",
                        other.prompt
                    ));
                }
                _ => return Some(format!("waiting on '{}' to finish", other.prompt)),
            }
        }
        None
    }

    /// True when the card can start right now: a backlog/queued card (or a
    /// failed one, which auto-retries per the "retry automatically" decision)
    /// with every dependency done. Running and done are excluded, and so are
    /// review (work is done — a human must look at it before it restarts) and
    /// manually-blocked cards (the admin said "hold"): those must never be
    /// handed back to the scheduler.
    pub fn ready_to_run(&self, id: &str) -> bool {
        let Some(card) = self.card(id) else {
            return false;
        };
        match card.status {
            CardStatus::Backlog | CardStatus::Queued => {}
            // A failed card auto-retries *within the cap*; once it has exhausted
            // its `auto_retry` budget it stays failed (its dependents stay
            // blocked via `blocked_reason`) and is never handed back. Keeping
            // this here means the web/CLI `ready` list agrees with the runner.
            // `auto_retry` counts retries after the initial attempt, so a card
            // with `retries <= auto_retry` still has retry budget left (§5a
            // "tries again twice" = auto_retry 2 runs up to 3 times total).
            CardStatus::Failed => {
                if card.retries > self.auto_retry {
                    return false;
                }
            }
            _ => return false,
        }
        self.blocked_reason(id).is_none()
    }

    /// Cards ready to start, in oldest-first order, honoring the parallel cap
    /// (the running set counts against the cap).
    pub fn queued_ids(&self, running: &HashSet<String>, cap: usize) -> Vec<String> {
        let slots = cap.saturating_sub(running.len());
        let mut ready: Vec<&Card> = self
            .cards
            .iter()
            .filter(|card| self.ready_to_run(&card.id) && !running.contains(&card.id))
            .collect();
        ready.sort_by_key(|card| card.created_at);
        ready.truncate(slots);
        ready.into_iter().map(|card| card.id.clone()).collect()
    }
}

impl Default for Board {
    fn default() -> Self {
        Board::new()
    }
}

fn is_terminal(status: CardStatus) -> bool {
    matches!(status, CardStatus::Done)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(&buf)
}

/// Turn a project name into a lossless, safe directory name: safe
/// characters are kept verbatim, everything else is `%XX`-encoded. Unlike a
/// naive slug this is injective — `"My Repo"`, `"my-repo"`, and `"my_repo"`
/// get distinct directories instead of silently sharing one board — and it
/// can never produce `..`, a path separator, or an empty name.
/// `project_name_from_dir` decodes it back for display.
pub fn project_dir_name(name: &str) -> String {
    let mut out = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    if out.is_empty() {
        out.push_str("project");
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// `project_dir_name`'s inverse: decode a board directory name back to the
/// project name. Unrecognized `%` sequences are kept verbatim.
pub fn project_name_from_dir(dir: &str) -> String {
    let bytes = dir.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[index + 1]), hex_val(bytes[index + 2])) {
                out.push(hi * 16 + lo);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
pub fn board_dir() -> PathBuf {
    crate::config::katban_data_dir().join("boards")
}

pub fn board_path(project: &str) -> PathBuf {
    board_dir()
        .join(project_dir_name(project))
        .join("board.json")
}

/// The advisory lock file for a project's board (a sibling of `board.json`).
pub fn board_lock_path(project: &str) -> PathBuf {
    board_dir()
        .join(project_dir_name(project))
        .join("board.lock")
}

/// An exclusive per-project lock on the board, held until dropped.
///
/// Backed by an advisory `flock` made available through the `nix` crate, so
/// it serializes writers across separate processes (CLI, TUI `/katban`, and a
/// future agent-runner) and — crucially — auto-releases if a process dies, so
/// a crash can never leave a stale lock that blocks everyone. Every mutation
/// takes this lock before its load -> change -> save so two writers can never
/// read-modify-write the same board at once.
///
/// On non-Unix this is a no-op guard: the board is a Linux-server surface and
/// `flock` has no portable std equivalent, so the guard is still held (and
/// serializing within a process) but does not advise other processes.
pub struct BoardLock {
    _guard: Option<nix::fcntl::Flock<std::fs::File>>,
}

impl BoardLock {
    /// Take the project's board lock, blocking until it is free.
    pub fn acquire(project: &str) -> anyhow::Result<BoardLock> {
        #[cfg(unix)]
        {
            let file = Self::open_lock_file(project)?;
            nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusive)
                .map(|flock| BoardLock {
                    _guard: Some(flock),
                })
                .map_err(|(_file, errno)| {
                    anyhow::anyhow!("could not lock board '{project}': {errno}")
                })
        }
        #[cfg(not(unix))]
        {
            let _ = project;
            Ok(BoardLock { _guard: None })
        }
    }

    /// Take the lock without blocking; `Ok(None)` when another writer holds it.
    pub fn try_acquire(project: &str) -> anyhow::Result<Option<BoardLock>> {
        #[cfg(unix)]
        {
            let file = Self::open_lock_file(project)?;
            match nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock) {
                Ok(flock) => Ok(Some(BoardLock {
                    _guard: Some(flock),
                })),
                Err((_, nix::errno::Errno::EWOULDBLOCK)) => Ok(None),
                Err((_, errno)) => {
                    anyhow::bail!("could not lock board '{project}': {errno}")
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = project;
            Ok(Some(BoardLock { _guard: None }))
        }
    }

    fn open_lock_file(project: &str) -> anyhow::Result<std::fs::File> {
        let path = board_lock_path(project);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))
    }
}

pub fn save_board(board: &Board, project: &str) -> anyhow::Result<()> {
    let path = board_path(project);
    let text = serde_json::to_string_pretty(board)?;
    write_atomic(&path, &text).with_context(|| format!("write {}", path.display()))
}

pub fn load_board(project: &str) -> anyhow::Result<Option<Board>> {
    let path = board_path(project);
    if !path.exists() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let board: Board =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(board))
}

/// Project names that have a board file under the data dir.
pub fn existing_projects() -> Vec<String> {
    let dir = board_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut projects = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.join("board.json").exists() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                // Directory names are the lossless encoding; decode back to
                // the real project name for display (dedupe: two encodings
                // could decode to the same name).
                let decoded = project_name_from_dir(name);
                if seen.insert(decoded.clone()) {
                    projects.push(decoded);
                }
            }
        }
    }
    projects.sort();
    projects
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{clawde_home, katban_data_dir};

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

    #[test]
    fn add_and_trash_cards() {
        let mut board = Board::new();
        let id = board.add_card("build the landing page");
        assert!(board.card(&id).is_some());
        assert_eq!(board.card(&id).unwrap().status, CardStatus::Backlog);
        assert!(board.trash_card(&id));
        assert_eq!(board.card(&id).unwrap().status, CardStatus::Done);
        assert!(!board.trash_card("missing"));
    }

    #[test]
    fn status_next_advances_along_the_board() {
        assert_eq!(CardStatus::Backlog.next(), Some(CardStatus::Queued));
        assert_eq!(CardStatus::Queued.next(), Some(CardStatus::Running));
        assert_eq!(CardStatus::Running.next(), Some(CardStatus::Review));
        assert_eq!(CardStatus::Review.next(), Some(CardStatus::Done));
        // Blocked / failed retry: back to queued.
        assert_eq!(CardStatus::Blocked.next(), Some(CardStatus::Queued));
        assert_eq!(CardStatus::Failed.next(), Some(CardStatus::Queued));
        // Done is terminal.
        assert_eq!(CardStatus::Done.next(), None);
    }

    #[test]
    fn dependencies_gate_readiness() {
        let mut board = Board::new();
        let db = board.add_card("set up database");
        let ui = board.add_card("build UI");
        board.add_dependency(&ui, &db).unwrap();

        assert_eq!(
            board.blocked_reason(&ui).as_deref(),
            Some("waiting on 'set up database' to finish")
        );
        assert!(!board.ready_to_run(&ui));
        assert!(board.ready_to_run(&db));

        board.set_status(&db, CardStatus::Done);
        assert!(board.ready_to_run(&ui));
        assert!(board.blocked_reason(&ui).is_none());
    }

    #[test]
    fn failed_card_past_retry_cap_is_not_ready() {
        let mut board = Board::new();
        board.auto_retry = 2;
        let a = board.add_card("flaky");

        // Within budget a failed card is ready again (auto-retry).
        board.set_status(&a, CardStatus::Failed);
        board.cards.iter_mut().find(|c| c.id == a).unwrap().retries = 1;
        assert!(board.ready_to_run(&a));
        // Retries == auto_retry still has one attempt left (the 2nd retry).
        board.cards.iter_mut().find(|c| c.id == a).unwrap().retries = 2;
        assert!(board.ready_to_run(&a), "retries==cap means one retry left");

        // Once retries exceed the cap it must not be handed back, and a
        // dependent waiting on it stays blocked.
        board.cards.iter_mut().find(|c| c.id == a).unwrap().retries = 3;
        assert!(!board.ready_to_run(&a));
        let b = board.add_card("depends on flaky");
        board.add_dependency(&b, &a).unwrap();
        assert!(!board.ready_to_run(&b));
        assert!(
            board.blocked_reason(&b).unwrap().contains("failed"),
            "{} : {}",
            board.blocked_reason(&b).unwrap(),
            "dependent must reference the failed dep"
        );
    }

    #[test]
    fn auto_retry_zero_still_runs_cards_once() {
        // auto_retry 0 means "no retries", not "never run": a fresh card (0
        // retries) must still be ready. The runner's spawn filter is
        // `retries <= auto_retry`, so 0 <= 0 passes for the initial attempt.
        let mut board = Board::new();
        board.auto_retry = 0;
        let a = board.add_card("one shot");
        assert!(board.ready_to_run(&a));
        // After the first failure there is no retry budget left.
        board.set_status(&a, CardStatus::Failed);
        board.cards.iter_mut().find(|c| c.id == a).unwrap().retries = 1;
        assert!(!board.ready_to_run(&a));
    }

    #[test]
    fn failed_dependency_blocks_with_review_hint() {
        let mut board = Board::new();
        let db = board.add_card("set up database");
        let ui = board.add_card("build UI");
        board.add_dependency(&ui, &db).unwrap();
        board.set_status(&db, CardStatus::Failed);
        let reason = board.blocked_reason(&ui).unwrap();
        assert!(reason.contains("failed"));
        assert!(!board.ready_to_run(&ui));
    }

    #[test]
    fn cycle_is_refused_naming_both_cards() {
        let mut board = Board::new();
        let a = board.add_card("a");
        let b = board.add_card("b");
        let c = board.add_card("c");
        board.add_dependency(&b, &a).unwrap();
        board.add_dependency(&c, &b).unwrap();
        // a -> c would close the loop.
        let err = board.add_dependency(&a, &c).unwrap_err();
        assert!(err.contains("loop forever"), "error: {err}");
        assert!(err.contains(&a));
        assert!(err.contains(&c));
        assert_eq!(board.dependencies.len(), 2);

        // Self-link refused too.
        assert!(board.add_dependency(&a, &a).is_err());
    }

    #[test]
    fn duplicate_dependency_is_rejected() {
        let mut board = Board::new();
        let a = board.add_card("a");
        let b = board.add_card("b");
        board.add_dependency(&b, &a).unwrap();
        assert!(board.add_dependency(&b, &a).is_err());
        assert!(board.remove_dependency(&b, &a));
        assert!(board.add_dependency(&b, &a).is_ok());
    }

    #[test]
    fn queued_ids_respects_cap_and_dependencies() {
        let mut board = Board::new();
        board.parallel_cap = 2;
        let a = board.add_card("a");
        let b = board.add_card("b");
        let c = board.add_card("c");
        let db = board.add_card("db");
        board.add_dependency(&c, &db).unwrap();

        let running: HashSet<String> = HashSet::new();
        let ready = board.queued_ids(&running, 2);
        assert_eq!(ready.len(), 2); // a, b ready; c blocked on db
        assert_eq!(ready[0], a);

        // One running slot + c still blocked -> one new start.
        let running: HashSet<String> = [a.clone()].into_iter().collect();
        let ready = board.queued_ids(&running, 2);
        assert_eq!(ready, vec![b]);

        // db done -> c becomes ready (verify readiness + wider cap so it fits).
        board.set_status(&db, CardStatus::Done);
        assert!(board.ready_to_run(&c));
        let ready = board.queued_ids(&running, 3);
        assert!(ready.contains(&c));
    }

    #[test]
    fn project_dir_names_are_lossless_and_safe() {
        // Names that a naive slug would collapse into the same directory get
        // distinct, decodable names — no silent board sharing.
        assert_ne!(project_dir_name("My Repo"), project_dir_name("my-repo"));
        assert_ne!(project_dir_name("my_repo"), project_dir_name("my-repo"));
        assert_eq!(project_dir_name("my-repo"), "my-repo");
        assert_eq!(project_dir_name("default"), "default");

        // Path traversal and separators can't leak out of the boards dir.
        assert_eq!(project_dir_name(".."), "%2E%2E");
        assert_eq!(project_dir_name("a/b"), "a%2Fb");
        assert_eq!(project_dir_name("a\\b"), "a%5Cb");
        assert_eq!(project_dir_name(""), "project");
        assert_eq!(project_dir_name("%"), "%25");

        // Decode round-trips.
        for name in ["My Repo", "my-repo", "a/b", "..", "100% sure"] {
            assert_eq!(project_name_from_dir(&project_dir_name(name)), name);
        }
    }

    #[test]
    fn review_and_blocked_cards_never_auto_start() {
        let mut board = Board::new();
        let review = board.add_card("needs a human");
        let blocked = board.add_card("on hold");
        let failed = board.add_card("auto-retry me");
        let backlog = board.add_card("normal");
        board.set_status(&review, CardStatus::Review);
        board.set_status(&blocked, CardStatus::Blocked);
        board.set_status(&failed, CardStatus::Failed);

        // Review and manually-blocked cards are never handed back to the
        // scheduler, even with no dependency issues.
        assert!(!board.ready_to_run(&review));
        assert!(!board.ready_to_run(&blocked));
        // Failed auto-retries (the "retry automatically" decision); backlog
        // cards start normally.
        assert!(board.ready_to_run(&failed));
        assert!(board.ready_to_run(&backlog));

        let ready = board.queued_ids(&HashSet::new(), 10);
        assert!(ready.contains(&failed));
        assert!(ready.contains(&backlog));
        assert!(!ready.contains(&review));
        assert!(!ready.contains(&blocked));
    }

    #[test]
    fn card_ids_are_random_and_do_not_collide() {
        let mut board = Board::new();
        let a = board.add_card("a");
        let b = board.add_card("b");
        let c = board.add_card("c");
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_eq!(a.len(), 16); // 8 random bytes, hex-encoded
                                 // Ids are opaque strings — everything downstream treats them as such.
        for id in [&a, &b, &c] {
            assert!(board.card(id).is_some());
        }
    }

    #[test]
    fn board_round_trips_per_project() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            assert_eq!(katban_data_dir(), clawde_home().join("katban"));

            let mut board = Board::new();
            let db = board.add_card("db");
            let ui = board.add_card("ui");
            board.add_dependency(&ui, &db).unwrap();
            save_board(&board, "my-repo").unwrap();
            assert!(board_path("my-repo").exists());

            let loaded = load_board("my-repo").unwrap().unwrap();
            assert_eq!(loaded.cards.len(), 2);
            assert_eq!(loaded.dependencies.len(), 1);

            assert_eq!(existing_projects(), vec!["my-repo"]);
            assert!(load_board("other-repo").unwrap().is_none());
        });
    }

    #[test]
    fn projects_that_slug_collide_stay_separate() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            // "My Repo" and "my-repo" used to map to the same directory
            // (boards/my-repo) and silently share one board. Now they are
            // distinct files that round-trip independently.
            let mut board_a = Board::new();
            board_a.add_card("card in My Repo");
            save_board(&board_a, "My Repo").unwrap();

            let mut board_b = Board::new();
            board_b.add_card("card in my-repo");
            save_board(&board_b, "my-repo").unwrap();

            assert_ne!(board_path("My Repo"), board_path("my-repo"));
            let loaded_a = load_board("My Repo").unwrap().unwrap();
            let loaded_b = load_board("my-repo").unwrap().unwrap();
            assert_eq!(loaded_a.cards[0].prompt, "card in My Repo");
            assert_eq!(loaded_b.cards[0].prompt, "card in my-repo");
            assert_eq!(loaded_a.cards.len(), 1);
            assert_eq!(loaded_b.cards.len(), 1);

            // existing_projects decodes the real names back.
            assert_eq!(existing_projects(), vec!["My Repo", "my-repo"]);
        });
    }

    #[test]
    fn board_lock_is_exclusive_across_openers() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            // First opener takes it; a second open (even in the same process,
            // since flock is per open-file-description) is refused non-blocking.
            let first = BoardLock::try_acquire("default").unwrap();
            assert!(first.is_some());
            assert!(
                BoardLock::try_acquire("default").unwrap().is_none(),
                "second opener must contend while the first holds it"
            );
            // Dropping the guard releases it; the next acquire succeeds.
            drop(first);
            assert!(BoardLock::try_acquire("default").unwrap().is_some());

            // Blocking acquire succeeds once free.
            let held = BoardLock::acquire("default").unwrap();
            assert!(BoardLock::try_acquire("default").unwrap().is_none());
            drop(held);
        });
    }

    #[test]
    fn traversal_project_names_stay_inside_the_boards_dir() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let path = board_path("..");
            assert!(
                path.starts_with(board_dir()),
                "board path escaped the boards dir: {}",
                path.display()
            );
            // Saving under a traversal-ish name lands in the boards dir, not
            // above it.
            let mut board = Board::new();
            board.add_card("x");
            save_board(&board, "..").unwrap();
            assert!(path.exists());
            assert!(!clawde_home().join("board.json").exists());
        });
    }
}
