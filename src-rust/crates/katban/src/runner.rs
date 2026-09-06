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

use crate::board::{self, AttemptOutcome, BoardLock, CardStatus, FailureKind, MAX_ATTEMPTS};
use crate::git;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// How long the scheduler sleeps between poll cycles.
const POLL_INTERVAL: Duration = Duration::from_millis(1500);

/// Pause before the single bounded retry of a rate-limited attempt (spec
/// §4.5): short enough to keep the ladder snappy, long enough for a
/// per-second 429 burst to clear. Bounded — the retry happens at most once
/// per attempt, then the ladder moves on.
const RATE_LIMIT_RETRY_PAUSE: Duration = Duration::from_secs(5);

/// Structured result of one agent run (spec §4.1). The executor parses the
/// headless `--output-format stream-json` events natively (the same shapes
/// the eval harness validates) and hands the runner attribution + usage so
/// the ladder can enforce pin honesty and the empty-completion guard.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttemptOutput {
    /// Human digest of the response text (what `card.result` showed before
    /// the structured output existed): first non-empty lines of the text.
    pub digest: String,
    /// Attribution: the free-catalog upstream that actually served, when the
    /// stream reported one (`None` = no attribution event, e.g. a non-free
    /// provider answered or the run failed before dispatch).
    pub served_upstream: Option<String>,
    /// The model id attribution reported (when present).
    pub model: Option<String>,
    pub output_tokens: u64,
    pub text_chars: usize,
    pub tool_calls: usize,
    /// An in-stream error event (`{"type":"error",...}`), e.g. the pinned
    /// upstream's rate limit after the chain exhausted.
    pub stream_error: Option<String>,
}

impl AttemptOutput {
    /// The eval's empty-completion guard (`is_empty_completion` semantics):
    /// no text + no tool calls + zero output tokens is a provider flake (the
    /// model spent the whole reply in a thinking block), not a pass. A no-op
    /// must never reach the verify gate — real projects have pass-as-shipped
    /// checks, and an unchanged tree skips the gate today (spec §4.6).
    pub fn is_empty_completion(&self) -> bool {
        self.text_chars == 0 && self.tool_calls == 0 && self.output_tokens == 0
    }
}

/// Executor abstraction so tests can substitute a scripted runner for the
/// real headless clawde subprocess. `model` is the attempt's pin
/// (`free/<upstream>/<model>` route string) or `None` for the default chain.
pub trait CardExecutor: Send + Sync {
    fn execute(
        &self,
        work_dir: &Path,
        prompt: &str,
        model: Option<&str>,
    ) -> Result<AttemptOutput, String>;
}

/// Real executor: spawn the current clawde binary headless in the worktree,
/// with `--output-format stream-json` so the run yields attribution and usage
/// instead of just prose.
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
    fn execute(
        &self,
        work_dir: &Path,
        prompt: &str,
        model: Option<&str>,
    ) -> Result<AttemptOutput, String> {
        let mut command = std::process::Command::new(&self.clawde_bin);
        // bypass-permissions is required for headless execution: there is no
        // TUI to approve tool prompts, so without it every Bash/Edit/Write is
        // denied and the agent can only talk (the card's live smoke caught
        // this — tools all denied, tree unchanged, gate skipped). The card's
        // worktree is the safety lane on the host tier (spec §2); the
        // container tier (§8) is the hard boundary.
        command.current_dir(work_dir).args([
            "--print",
            prompt,
            "--output-format",
            "stream-json",
            "--permission-mode",
            "bypass-permissions",
        ]);
        if let Some(model) = model {
            command.args(["--model", model]);
        }
        let output = command
            .output()
            .map_err(|e| format!("could not start clawde: {e}"))?;
        if output.status.success() {
            Ok(parse_attempt_stream(&String::from_utf8_lossy(
                &output.stdout,
            )))
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

/// Parse one attempt's stream-json lines into an [`AttemptOutput`]. Mirrors
/// the eval harness's `parse_stream_events` event shapes (`text_delta`,
/// `tool_start`, `provider_attribution`, `result`, `error`). A stream with no
/// parseable events degrades to the old plain-text digest so an unexpected
/// writer still produces a useful result.
fn parse_attempt_stream(stdout: &str) -> AttemptOutput {
    let mut out = AttemptOutput::default();
    let mut text = String::new();
    let mut saw_event = false;
    for line in stdout.lines() {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        match event.get("type").and_then(|t| t.as_str()) {
            Some("text_delta") => {
                saw_event = true;
                if let Some(delta) = event.get("text").and_then(|t| t.as_str()) {
                    text.push_str(delta);
                }
            }
            Some("tool_start") => {
                saw_event = true;
                out.tool_calls += 1;
            }
            Some("provider_attribution") => {
                saw_event = true;
                out.served_upstream = event
                    .get("upstream_id")
                    .and_then(|u| u.as_str())
                    .map(str::to_string);
                out.model = event
                    .get("model")
                    .and_then(|m| m.as_str())
                    .map(str::to_string);
            }
            Some("result") => {
                saw_event = true;
                if let Some(tokens) = event
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(|t| t.as_u64())
                {
                    out.output_tokens = tokens;
                }
                // The final result event is authoritative for attribution.
                if let Some(upstream) = event.get("upstream").and_then(|u| u.as_str()) {
                    out.served_upstream = Some(upstream.to_string());
                }
                if let Some(model) = event.get("model").and_then(|m| m.as_str()) {
                    out.model = Some(model.to_string());
                }
            }
            Some("error") => {
                saw_event = true;
                if let Some(error) = event.get("error").and_then(|e| e.as_str()) {
                    out.stream_error = Some(error.to_string());
                }
            }
            _ => {}
        }
    }
    out.text_chars = text.chars().count();
    out.digest = digest_of(&text, saw_event);
    out
}

/// The card's human digest: the first non-empty lines of the response text.
/// With no stream events at all, the raw stdout was prose — digest that.
fn digest_of(text: &str, saw_event: bool) -> String {
    let digest: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(4)
        .collect();
    if digest.is_empty() {
        if saw_event {
            "completed".to_string()
        } else {
            String::new()
        }
    } else {
        digest.join("\n")
    }
}

/// Whether an error message is a transient rate limit (the one failure class
/// that gets a single bounded retry before the ladder moves on, spec §4.5).
fn is_rate_limited(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("429")
        || lower.contains("too many requests")
        || lower.contains("retry after")
        || lower.contains("retry-after")
        || lower.contains("quota")
}

/// One pinned rung of the attempts:N ladder: the catalog upstream the attempt
/// is pinned to plus the exact `--model` route string (`free/<id>/<model>`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct AttemptPin {
    upstream: String,
    model: String,
}

impl AttemptPin {
    fn route_string(&self) -> &str {
        &self.model
    }
}

/// Build the attempt ladder's pin list (spec §4.2 + §6):
/// - `attempt_upstreams` (admin-set) wins verbatim, cycled to N rungs.
/// - Otherwise derive from the free catalog: keyed upstreams (auth store)
///   in catalog order, distinct `model_family` first, then distinct hosts of
///   the same family (groq+nvidia gpt-oss is still real diversity — the
///   serving stacks differ measurably).
/// - Fewer available upstreams than N: run min(N, available) rungs — never
///   reuse the same upstream within one ladder unless families are
///   exhausted (admin lists are cycled, not truncated, per §4.2).
/// - Upstreams the free chain recently put into cooldown (5xx circuit
///   breaker or empty-completion track — the persisted snapshot the
///   dispatching provider writes) are ranked **after** healthy ones: they
///   only fill rungs when healthy upstreams can't, and an all-cooling list
///   runs as-is rather than producing no ladder at all (spec §6.3).
///
/// Returns an empty vec for `attempts <= 1` (single-attempt default run — no
/// pin, no diversity machinery).
fn ladder_pins(
    attempts: u32,
    attempt_upstreams: &[String],
    keyed_upstreams: &HashSet<String>,
    cooling_upstreams: &HashSet<String>,
) -> Vec<AttemptPin> {
    let rungs = attempts.clamp(1, MAX_ATTEMPTS) as usize;
    if rungs <= 1 {
        return Vec::new();
    }
    if !attempt_upstreams.is_empty() {
        // Admin intent: healthy entries cycle first; cooling entries only fill
        // leftover rungs. If *everything* listed is cooling, run the list as-is.
        let (healthy, cooling): (Vec<&String>, Vec<&String>) = attempt_upstreams
            .iter()
            .partition(|id| !cooling_upstreams.contains(*id));
        let ordered: Vec<&String> = if healthy.is_empty() {
            attempt_upstreams.iter().collect()
        } else {
            let mut o = healthy.clone();
            o.extend(cooling);
            o
        };
        return (0..rungs)
            .map(|i| {
                let upstream = ordered[i % ordered.len()].clone();
                let model = catalog_entry_default(&upstream)
                    .map(|m| format!("free/{upstream}/{m}"))
                    .unwrap_or_else(|| format!("free/{upstream}"));
                AttemptPin { upstream, model }
            })
            .collect();
    }
    // Auto-derive: keyed catalog entries in order, healthy entries before
    // cooling ones — a cooling upstream only fills rungs the healthy set
    // can't; all-cooling degrades to the plain family ordering.
    let keyed: Vec<&clawde_api::providers::free::FreeUpstream> =
        clawde_api::providers::free::FREE_CATALOG
            .iter()
            .filter(|e| keyed_upstreams.contains(e.id))
            .collect();
    let (healthy, cooling): (Vec<_>, Vec<_>) = keyed
        .into_iter()
        .partition(|e| !cooling_upstreams.contains(e.id));
    let mut ordered = family_ranked(healthy);
    ordered.extend(family_ranked(cooling));
    let available = ordered.len();
    if available == 0 {
        return Vec::new();
    }
    ordered
        .into_iter()
        .take(rungs)
        .map(|entry| AttemptPin {
            upstream: entry.id.to_string(),
            model: format!("free/{}/{}", entry.id, entry.default_model),
        })
        .collect()
}

/// A catalog upstream's default model id, for admin-set pin routes.
fn catalog_entry_default(upstream: &str) -> Option<&'static str> {
    clawde_api::providers::free::FREE_CATALOG
        .iter()
        .find(|e| e.id == upstream)
        .map(|e| e.default_model)
}

/// Distinct `model_family` first, then distinct hosts of already-seen
/// families (groq+nvidia both serving gpt-oss is still real diversity).
fn family_ranked(
    entries: Vec<&clawde_api::providers::free::FreeUpstream>,
) -> Vec<&clawde_api::providers::free::FreeUpstream> {
    let mut primary = Vec::new();
    let mut secondary = Vec::new();
    let mut seen_families: HashSet<&str> = HashSet::new();
    for entry in entries {
        if seen_families.insert(entry.model_family) {
            primary.push(entry);
        } else {
            secondary.push(entry);
        }
    }
    primary.extend(secondary);
    primary
}

/// The upstream ids with at least one usable key in the auth store (the same
/// `>= 8 chars after trim` validity rule the chain applies). The ladder
/// derives from these so it never pins a dead upstream (spec §6.3).
fn keyed_upstream_ids() -> HashSet<String> {
    let store = clawde_core::AuthStore::load();
    clawde_api::providers::free::FREE_CATALOG
        .iter()
        .filter(|e| clawde_api::providers::free::first_free_upstream_key(&store, e.id).is_some())
        .map(|e| e.id.to_string())
        .collect()
}

/// Record one attempt outcome on the card (under the lock) so the matrix is
/// visible even while later rungs still run — and survives a crashed runner.
fn record_attempt(project: &str, card_id: &str, outcome: AttemptOutcome) {
    let Ok(_guard) = BoardLock::acquire(project) else {
        return;
    };
    if let Ok(Some(mut board)) = board::load_board(project) {
        if let Some(card) = board.cards.iter_mut().find(|c| c.id == card_id) {
            card.attempts.push(outcome);
            card.updated_at = crate::time::now_secs();
            let _ = board::save_board(&board, project);
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
            card.updated_at = crate::time::now_secs();
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
/// otherwise an isolated scratch dir under our root), run the attempts:N
/// ladder, finalize.
///
/// Ladder semantics (spec §4): with `board.attempts = N > 1`, each rung is
/// pinned to a different free-catalog upstream (distinct model families
/// first). After every rung the verification gate runs in that rung's tree;
/// the FIRST gate pass is promoted immediately (the card finalizes on that
/// tree and the remaining worktrees are torn down). With the gate off, the
/// first non-empty completion wins (no selector, first-wins degradation).
/// Pin honesty: a rung whose attribution does not match its pin never counts
/// as a pass. All rungs' outcomes are recorded on the card.
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
                Some(FailureKind::Worktree),
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
                Some(FailureKind::Worktree),
            );
            return;
        }
    }

    // The board's knobs once per card: gates/toggles plus the ladder shape.
    let (verify_on, auto_review_on, attempts, attempt_upstreams) = board::load_board(project)
        .ok()
        .flatten()
        .map(|b| {
            (
                b.verify,
                b.auto_review,
                b.attempts,
                b.attempt_upstreams.clone(),
            )
        })
        .unwrap_or((true, false, 1, Vec::new()));
    let pins = ladder_pins(
        attempts,
        &attempt_upstreams,
        &keyed_upstream_ids(),
        &clawde_api::providers::free::cooling_free_upstreams(),
    );

    // The ladder. `winner` = (index, structured output) of the promoted rung.
    let mut winner: Option<(usize, AttemptOutput)> = None;
    let mut last_failure: Option<FailureKind> = None;
    for (idx, pin) in pins.iter().enumerate() {
        let started = std::time::Instant::now();
        let (mut outcome, output) =
            run_attempt(&card_id, work_dir, prompt, executor, Some(pin)).await;
        outcome.elapsed_ms = Some(started.elapsed().as_millis() as u64);

        // The empty-completion guard runs BEFORE the gate: a no-op must never
        // pass pass-as-shipped checks (spec §4.6).
        if output.is_empty_completion() {
            outcome.error = Some("empty completion".to_string());
            record_attempt(project, &card_id, outcome.clone());
            continue;
        }
        // Pin honesty: attribution must name the pinned upstream (spec §4.5).
        // Route::Pinned falls through silently; a fallthrough rung is excluded
        // even when its verify happened to pass — its identity is wrong for
        // the diversity policy. No attribution event at all (a non-free
        // provider answered) keeps the rung eligible.
        if let Some(served) = &output.served_upstream {
            if served != &pin.upstream {
                outcome.error = Some(format!("pin fell through: served by {served}"));
                outcome.verify_passed = None;
                record_attempt(project, &card_id, outcome.clone());
                continue;
            }
        }

        // The gate decides (never the agent). The worktree now holds THIS
        // rung's tree.
        let gate = crate::verify::run_gate(work_dir, verify_on).await;
        if !gate.passed {
            outcome.verify_passed = Some(false);
            outcome.error = Some(gate.detail.clone());
            record_attempt(project, &card_id, outcome.clone());
            last_failure = Some(FailureKind::Verification);
            continue;
        }
        outcome.verify_passed = Some(true);
        if gate.skipped {
            // Gate off / no checks / skipped install: no selector exists, so
            // this rung wins on being a real (non-empty) completion — the
            // documented first-wins degradation (spec §9). The skip reason is
            // surfaced on the rung, never silent.
            outcome.error = Some(format!("gate skipped: {}", gate.detail));
        }
        record_attempt(project, &card_id, outcome.clone());
        record_winner(project, &card_id, idx);
        winner = Some((idx, output));
        break;
    }

    // Single-attempt default (no ladder): today's behavior exactly — one run,
    // no pin, gate decides.
    let (final_failed, final_note, final_failure) = if pins.is_empty() && winner.is_none() {
        let started = std::time::Instant::now();
        let (mut outcome, output) = run_attempt(&card_id, work_dir, prompt, executor, None).await;
        outcome.elapsed_ms = Some(started.elapsed().as_millis() as u64);
        if output.is_empty_completion() {
            outcome.error = Some("empty completion".to_string());
            record_attempt(project, &card_id, outcome);
            (
                true,
                Some("empty completion".to_string()),
                Some(FailureKind::Agent),
            )
        } else {
            let gate = crate::verify::run_gate(work_dir, verify_on).await;
            if !gate.passed {
                outcome.verify_passed = Some(false);
                outcome.error = Some(gate.detail.clone());
                record_attempt(project, &card_id, outcome);
                (true, Some(gate.detail), Some(FailureKind::Verification))
            } else {
                outcome.verify_passed = Some(true);
                let mut note = output.digest.clone();
                if gate.skipped {
                    // The old "gate skipped" suffix, preserved verbatim so
                    // existing result consumers keep parsing it.
                    let base = if note.is_empty() {
                        String::new()
                    } else {
                        format!("{note} · ")
                    };
                    note = format!("{base}gate skipped: {}", gate.detail);
                    outcome.error = Some(gate.detail.clone());
                }
                record_attempt(project, &card_id, outcome);
                (false, Some(note), None)
            }
        }
    } else if let Some((_, output)) = winner {
        // A ladder rung won: its digest is the card's result, gate-skip
        // surfacing included (already recorded on the winning outcome).
        (false, Some(output.digest.clone()), None)
    } else {
        // All rungs failed: a composed result naming each rung's error; the
        // failure kind is verification when a gate fail was the last signal,
        // else agent (spec §4.4). auto_retry applies unchanged from here.
        let summary = attempt_errors_summary(project, &card_id);
        (
            true,
            Some(summary),
            last_failure.or(Some(FailureKind::Agent)),
        )
    };

    // Auto-review (option 2) runs once, on the winning attempt's diff only
    // (spec §9). The worktree is still present at this point.
    if !final_failed && auto_review_on {
        let diff = git::diff_clamped(work_dir);
        if diff.trim().is_empty() {
            // Nothing changed (e.g. a no-op follow-up): there is no diff to
            // review — skip the pass instead of spawning a reviewer to look
            // at an empty diff (and potentially attach noise comments).
            tracing::info!(project, card = %card_id, "auto-review skipped: empty diff");
        } else {
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

    finalize(
        project,
        &card_id,
        work_dir,
        final_failed,
        final_note.as_deref(),
        final_failure,
    );
}

/// Run ONE rung of the ladder (or the whole single-attempt run when `pin` is
/// `None`): the agent subprocess in `work_dir` plus its bookkeeping. The gate
/// is NOT run here — the caller gates in ladder order. Returns the outcome
/// (sans elapsed time, which the caller stamps) and the structured output.
async fn run_attempt(
    _card_id: &str,
    work_dir: &Path,
    prompt: &str,
    executor: &Arc<dyn CardExecutor>,
    pin: Option<&AttemptPin>,
) -> (AttemptOutcome, AttemptOutput) {
    let mut outcome = AttemptOutcome {
        upstream: pin.map(|p| p.upstream.clone()).unwrap_or_default(),
        model: pin.map(|p| p.model.clone()).unwrap_or_default(),
        ..AttemptOutcome::default()
    };
    let route = pin.map(|p| p.route_string().to_string());
    // `execute` waits on a subprocess; run it on the blocking pool so a long
    // agent run never stalls the async runtime (works on every runtime flavor).
    let result = {
        let executor = executor.clone();
        let work_dir = work_dir.to_path_buf();
        let prompt = prompt.to_string();
        let route = route.clone();
        tokio::task::spawn_blocking(move || executor.execute(&work_dir, &prompt, route.as_deref()))
            .await
            .unwrap_or_else(|e| Err(format!("attempt task panicked: {e}")))
    };
    match result {
        Ok(output) => {
            outcome.served_upstream = output.served_upstream.clone();
            if outcome.model.is_empty() {
                outcome.model = output.model.clone().unwrap_or_default();
            }
            (outcome, output)
        }
        Err(first_error) => {
            // The one bounded retry (spec §4.5): a rate-limited rung waits a
            // short pause and tries once more, then the ladder moves on.
            if is_rate_limited(&first_error) {
                tokio::time::sleep(RATE_LIMIT_RETRY_PAUSE).await;
                let retry = {
                    let executor = executor.clone();
                    let work_dir = work_dir.to_path_buf();
                    let prompt = prompt.to_string();
                    tokio::task::spawn_blocking(move || {
                        executor.execute(&work_dir, &prompt, route.as_deref())
                    })
                    .await
                    .unwrap_or_else(|e| Err(format!("attempt task panicked: {e}")))
                };
                match retry {
                    Ok(output) => {
                        outcome.served_upstream = output.served_upstream.clone();
                        if outcome.model.is_empty() {
                            outcome.model = output.model.clone().unwrap_or_default();
                        }
                        return (outcome, output);
                    }
                    Err(retry_error) => {
                        outcome.error = Some(retry_error.clone());
                        return (
                            outcome,
                            AttemptOutput {
                                stream_error: Some(retry_error),
                                ..AttemptOutput::default()
                            },
                        );
                    }
                }
            }
            outcome.error = Some(first_error.clone());
            (
                outcome,
                AttemptOutput {
                    stream_error: Some(first_error),
                    ..AttemptOutput::default()
                },
            )
        }
    }
}

/// Persist the winning rung index on the card (under the lock). Called once,
/// immediately after the winning gate pass, so an early promotion is visible
/// on the record even if the process dies before finalize.
fn record_winner(project: &str, card_id: &str, index: usize) {
    let Ok(_guard) = BoardLock::acquire(project) else {
        return;
    };
    if let Ok(Some(mut board)) = board::load_board(project) {
        if let Some(card) = board.cards.iter_mut().find(|c| c.id == card_id) {
            card.picked_attempt = Some(index);
            card.updated_at = crate::time::now_secs();
            let _ = board::save_board(&board, project);
        }
    }
}

/// Compose the all-rungs-failed result from the card's recorded matrix:
/// `1: groq (free/groq/gpt-oss-120b) err (rate limit) · 2: nvidia …`.
fn attempt_errors_summary(project: &str, card_id: &str) -> String {
    board::load_board(project)
        .ok()
        .flatten()
        .and_then(|b| b.card(card_id).cloned())
        .filter(|card| !card.attempts.is_empty())
        .map(|card| card.attempts_summary())
        .unwrap_or_else(|| "all attempts failed".to_string())
}

/// Persist a card's final state after its agent exits. Only transitions a card
/// still `running`; if the admin moved it meanwhile, their edit is preserved.
/// The card's worktree is ALWAYS removed afterwards (best-effort), on every
/// path — an early return (card moved / lock held / card gone) must not leak
/// the checkout and its git registration, or they accumulate forever.
fn finalize(
    project: &str,
    card_id: &str,
    work_dir: &Path,
    failed: bool,
    note: Option<&str>,
    failure_kind: Option<FailureKind>,
) {
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
                card.failure_kind = failure_kind;
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
                                        "could not pin card commit"
                                    );
                                    card.failure_kind = Some(FailureKind::Commit);
                                    card.status = CardStatus::Failed;
                                }
                            }
                        }
                    }
                }
                // A review follow-up (feedback was pending when this run started)
                // is now complete: consume the feedback so a later manual
                // requeue doesn't re-append stale instructions, and acknowledge
                // exactly the comments included in this follow-up. Failed runs
                // leave both fields intact so retry can resend the feedback.
                if card.followup_feedback.is_some() {
                    card.review_ack = card.reviews.len();
                    card.followup_feedback = None;
                }
                card.failure_kind = None;
                card.status = CardStatus::Review;
            }
            card.result = Some(note);
            if !diff.is_empty() {
                card.diff_summary = Some(crate::git::diff_summary(&diff));
                card.diff = Some(diff);
            }
            // The checkout is about to be torn down; drop the stale path.
            card.work_dir = None;
            card.updated_at = crate::time::now_secs();
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
        fn execute(
            &self,
            _work_dir: &Path,
            _prompt: &str,
            _model: Option<&str>,
        ) -> Result<AttemptOutput, String> {
            if self.outcomes.is_empty() {
                return Ok(AttemptOutput {
                    digest: "done".into(),
                    text_chars: 4,
                    ..AttemptOutput::default()
                });
            }
            match self.outcomes.last().copied().unwrap_or(true) {
                true => Ok(AttemptOutput {
                    digest: "done".into(),
                    text_chars: 4,
                    ..AttemptOutput::default()
                }),
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
                finalize(
                    "default",
                    id,
                    &wt,
                    true,
                    Some("boom"),
                    Some(FailureKind::Agent),
                );
            };

            // Attempt 1 fails -> retries=1, still within budget (1 <= cap 1).
            run_and_fail(&a);
            let b = board::load_board("default").unwrap().unwrap();
            let card = b.card(&a).unwrap();
            assert_eq!(card.status, CardStatus::Failed);
            assert_eq!(card.failure_kind, Some(FailureKind::Agent));
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
            finalize("default", &a, &wt, false, Some("all tests pass"), None);
            let b = board::load_board("default").unwrap().unwrap();
            let card = b.card(&a).unwrap();
            assert_eq!(card.status, CardStatus::Review);
            assert_eq!(card.failure_kind, None);
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
            finalize(
                "default",
                &a,
                &wt,
                true,
                Some("boom"),
                Some(FailureKind::Agent),
            );
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

            finalize("default", &a, &wt, false, Some("done feature"), None);
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
            finalize("default", &a, &wt, false, Some("first run"), None);

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
            finalize("default", &a, &wt, false, Some("second run"), None);

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
            assert_eq!(card.review_ack, card.reviews.len());
        });
    }

    #[test]
    fn failed_follow_up_keeps_feedback_for_retry() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let mut board = Board::new();
            let id = board.add_card("fix it");
            board.set_status(&id, CardStatus::Review);
            board::save_board(&board, "default").unwrap();
            board::add_review("default", &id, None, "please fix the failure").unwrap();
            board::send_feedback_to_agent("default", &id).unwrap();

            let queued = board::load_board("default").unwrap().unwrap();
            let card = queued.card(&id).unwrap();
            assert_eq!(card.review_ack, 0);
            assert!(card.followup_feedback.is_some());

            let mut failed = queued;
            failed.set_status(&id, CardStatus::Running);
            board::save_board(&failed, "default").unwrap();
            let wt = git::card_worktree_dir("default", &id);
            std::fs::create_dir_all(&wt).unwrap();
            finalize(
                "default",
                &id,
                &wt,
                true,
                Some("agent failed"),
                Some(FailureKind::Agent),
            );

            let retry = board::load_board("default").unwrap().unwrap();
            let card = retry.card(&id).unwrap();
            assert_eq!(card.status, CardStatus::Failed);
            assert_eq!(card.review_ack, 0);
            assert!(card.followup_feedback.is_some());
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
            finalize("default", &a, &wt, false, Some("first run"), None);
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
            finalize("default", &a, &wt, false, Some("second run"), None);

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
    async fn auto_review_skips_empty_diff() {
        // A run whose agent changes nothing has an empty diff: the auto-review
        // pass must not spawn a reviewer (nothing to review) and must not
        // attach `[auto-review]` noise comments to a change-less card.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        {
            let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var("CLAWDE_HOME").ok();
            std::env::set_var("CLAWDE_HOME", tmp.path());
            crate::projects::set_repo_root("default", repo.path()).unwrap();
            let mut board = Board::new();
            let a = board.add_card("add a feature");
            board::save_board(&board, "default").unwrap();
            // `run_one_card` creates the worktree itself (the card is marked
            // running first), so we only reserve the slot.
            let wt = git::card_worktree_dir("default", &a);
            let mut b = board::load_board("default").unwrap().unwrap();
            b.set_status(&a, CardStatus::Running);
            b.cards.iter_mut().find(|c| c.id == a).unwrap().branch = Some(format!("katban/{a}"));
            b.cards.iter_mut().find(|c| c.id == a).unwrap().work_dir =
                Some(wt.to_string_lossy().into_owned());
            board::save_board(&b, "default").unwrap();

            struct Noop;
            impl CardExecutor for Noop {
                fn execute(
                    &self,
                    _work_dir: &Path,
                    _prompt: &str,
                    _model: Option<&str>,
                ) -> Result<AttemptOutput, String> {
                    Ok(AttemptOutput {
                        digest: "done".into(),
                        text_chars: 4,
                        ..AttemptOutput::default()
                    })
                }
            }
            let executor: Arc<dyn CardExecutor> = Arc::new(Noop);
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
            assert_eq!(card.status, CardStatus::Review);
            assert!(
                card.reviews.is_empty(),
                "empty-diff run must not attach auto-review comments"
            );
            match previous {
                Some(value) => std::env::set_var("CLAWDE_HOME", value),
                None => std::env::remove_var("CLAWDE_HOME"),
            }
        }
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
                fn execute(
                    &self,
                    work_dir: &Path,
                    _prompt: &str,
                    _model: Option<&str>,
                ) -> Result<AttemptOutput, String> {
                    // The agent "writes" a JS project whose test command fails.
                    std::fs::write(
                        work_dir.join("package.json"),
                        r#"{"scripts":{"test":"node -e \"process.exit(1)\""}}"#,
                    )
                    .unwrap();
                    Ok(AttemptOutput {
                        digest: "done".into(),
                        text_chars: 4,
                        ..AttemptOutput::default()
                    })
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

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn board_verify_off_skips_gate_and_reaches_review() {
        // #7 — `board verify off` is the per-board master switch: a card whose
        // project checks would fail must still reach Review (the gate skip is
        // surfaced on the result, not silent).
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
            board.verify = false;
            board.auto_review = false; // never spawn a reviewer in tests
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
                fn execute(
                    &self,
                    work_dir: &Path,
                    _prompt: &str,
                    _model: Option<&str>,
                ) -> Result<AttemptOutput, String> {
                    std::fs::write(
                        work_dir.join("package.json"),
                        r#"{"scripts":{"test":"node -e \"process.exit(1)\""}}"#,
                    )
                    .unwrap();
                    Ok(AttemptOutput {
                        digest: "done".into(),
                        text_chars: 4,
                        ..AttemptOutput::default()
                    })
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
            assert_eq!(card.status, CardStatus::Review);
            let result = card.result.as_deref().unwrap();
            assert!(result.contains("board verify off"), "result: {result}");
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

            finalize("default", &a, &wt, false, Some("done feature"), None);

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

            finalize(
                "default",
                &a,
                &wt,
                true,
                Some("boom"),
                Some(FailureKind::Agent),
            );
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
            finalize(
                "default",
                &a,
                &wt,
                true,
                Some("boom"),
                Some(FailureKind::Agent),
            );
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
                fn execute(
                    &self,
                    work_dir: &Path,
                    _prompt: &str,
                    _model: Option<&str>,
                ) -> Result<AttemptOutput, String> {
                    // work_dir is <root>/<project-encoding>/<card>; recover the
                    // project encoding from the parent's file name.
                    if let Some(proj) = work_dir.parent().and_then(|p| p.file_name()) {
                        let mut set = self.0.lock().unwrap_or_else(|e| e.into_inner());
                        set.insert(proj.to_string_lossy().into_owned());
                    }
                    Ok(AttemptOutput {
                        digest: "done".into(),
                        text_chars: 4,
                        ..AttemptOutput::default()
                    })
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

    // ---- attempts:N ladder (spec §4) -----------------------------------

    /// A scripted executor whose Nth call (0-based) returns the given
    /// served-upstream/output; unscripted calls serve the pinned upstream
    /// parsed from the route string (`free/<upstream>/...`) — what the real
    /// chain reports when a pin holds.
    struct Scripted {
        calls: Arc<std::sync::atomic::AtomicUsize>,
        /// (call_index, served_upstream, empty_completion)
        script: Vec<(usize, Option<String>, bool)>,
        /// Fail with this error on these call indexes.
        fail_on: Vec<usize>,
    }
    impl CardExecutor for Scripted {
        fn execute(
            &self,
            _work_dir: &Path,
            _prompt: &str,
            model: Option<&str>,
        ) -> Result<AttemptOutput, String> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail_on.contains(&n) {
                return Err("rate limit exceeded, retry after 1s".into());
            }
            // The pin holds: attribution names the route's upstream.
            let pinned = model
                .and_then(|route| route.strip_prefix("free/"))
                .and_then(|rest| rest.split('/').next())
                .map(str::to_string);
            let entry = self.script.iter().find(|(i, _, _)| *i == n);
            let (served, empty) = match entry {
                Some((_, s, e)) => (s.clone().or(pinned), *e),
                None => (pinned, false),
            };
            Ok(if empty {
                AttemptOutput::default()
            } else {
                AttemptOutput {
                    digest: "did the work".into(),
                    served_upstream: served,
                    model: Some("test-model".into()),
                    output_tokens: 32,
                    text_chars: 12,
                    tool_calls: 1,
                    ..AttemptOutput::default()
                }
            })
        }
    }

    fn seeded_board(board: &Board) {
        board::save_board(board, "default").unwrap();
    }

    #[test]
    fn ladder_pins_distinct_families_then_hosts() {
        // Auto-derivation (spec §6): keyed upstreams in catalog order,
        // distinct model_family first, then distinct hosts of the same
        // family. nvidia/cerebras/groq all host gpt-oss-120b — groq must not
        // take a rung until families are exhausted.
        let keyed: HashSet<String> = ["nvidia", "cerebras", "groq", "zai"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let pins = ladder_pins(3, &[], &keyed, &HashSet::new());
        assert_eq!(pins.len(), 3, "N rungs from N available");
        let families: Vec<&str> = pins
            .iter()
            .map(|p| {
                clawde_api::providers::free::FREE_CATALOG
                    .iter()
                    .find(|e| e.id == p.upstream)
                    .map(|e| e.model_family)
                    .unwrap_or("")
            })
            .collect();
        let mut uniq = families.clone();
        uniq.sort_unstable();
        uniq.dedup();
        // Primary rungs are distinct families; with only 2 families across 4
        // keyed upstreams and N=3, the 3rd rung reuses a family on a distinct
        // host (spec §6.2).
        assert_eq!(families.len(), 3);
        assert_eq!(uniq.len(), 2, "families: {families:?}");
        let ups: Vec<&str> = pins.iter().map(|p| p.upstream.as_str()).collect();
        assert_eq!(
            ups.len(),
            ups.iter().collect::<HashSet<_>>().len(),
            "no upstream reuse within the ladder: {ups:?}"
        );
        // 4 rungs: the 4th may reuse a family but never the same upstream.
        let pins4 = ladder_pins(4, &[], &keyed, &HashSet::new());
        let ups: Vec<&str> = pins4.iter().map(|p| p.upstream.as_str()).collect();
        assert_eq!(
            ups.len(),
            ups.iter().collect::<HashSet<_>>().len(),
            "no upstream reuse: {ups:?}"
        );
        // Pin routes are exact `free/<upstream>/<default_model>` strings.
        for pin in &pins {
            let def = catalog_entry_default(&pin.upstream).unwrap();
            assert_eq!(pin.model, format!("free/{}/{}", pin.upstream, def));
        }
    }

    #[test]
    fn ladder_pins_admin_list_cycles_verbatim() {
        // An admin-set attempt_upstreams wins and cycles to N rungs (§4.2);
        // unknown ids would have been refused at the CLI surface.
        let list = vec!["zai".to_string(), "nvidia".to_string()];
        let pins = ladder_pins(5, &list, &HashSet::new(), &HashSet::new());
        let ups: Vec<&str> = pins.iter().map(|p| p.upstream.as_str()).collect();
        assert_eq!(ups, vec!["zai", "nvidia", "zai", "nvidia", "zai"]);
    }

    #[test]
    fn ladder_pins_cooling_upstreams_ranked_last() {
        // Phase 2: an upstream the free chain put into cooldown (5xx breaker
        // or empty-completion track) only fills rungs healthy upstreams
        // can't — in both the admin list and auto-derive paths (spec §6.3).
        let keyed: HashSet<String> = ["zai", "nvidia", "groq"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let cooling: HashSet<String> = ["nvidia"].into_iter().map(str::to_string).collect();
        let pins = ladder_pins(3, &[], &keyed, &cooling);
        let ups: Vec<&str> = pins.iter().map(|p| p.upstream.as_str()).collect();
        assert_eq!(ups.last(), Some(&"nvidia"), "cooling demoted: {ups:?}");
        assert_eq!(
            ups.iter().position(|u| *u == "nvidia"),
            Some(2),
            "cooling upstream fills only the last rung: {ups:?}"
        );
        // Admin list: the cooling entry cycles to the tail.
        let list = vec!["nvidia".to_string(), "zai".to_string()];
        let pins = ladder_pins(2, &list, &HashSet::new(), &cooling);
        let ups: Vec<&str> = pins.iter().map(|p| p.upstream.as_str()).collect();
        assert_eq!(
            ups,
            vec!["zai", "nvidia"],
            "admin list cooling demoted: {ups:?}"
        );
        // All-cooling: run the list as-is rather than producing no ladder.
        let all_cooling: HashSet<String> =
            ["zai", "nvidia"].into_iter().map(str::to_string).collect();
        let pins = ladder_pins(2, &list, &HashSet::new(), &all_cooling);
        let ups: Vec<&str> = pins.iter().map(|p| p.upstream.as_str()).collect();
        assert_eq!(
            ups,
            vec!["nvidia", "zai"],
            "all-cooling runs verbatim: {ups:?}"
        );
    }

    #[test]
    fn ladder_empty_without_keys_or_attempts_one() {
        // No keyed upstreams -> no pins (the ladder degrades to the default
        // single run rather than pinning dead upstreams, spec §6.3).
        assert!(ladder_pins(3, &[], &HashSet::new(), &HashSet::new()).is_empty());
        // attempts = 1 is today's behavior: no pins at all.
        let keyed: HashSet<String> = ["zai"].into_iter().map(str::to_string).collect();
        assert!(ladder_pins(1, &[], &keyed, &HashSet::new()).is_empty());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn ladder_first_gate_pass_wins_and_records_matrix() {
        // Rung 1 passes the gate -> rung 2 never runs (early promotion,
        // spec §4.3); the matrix records the picked rung.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        {
            let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var("CLAWDE_HOME").ok();
            std::env::set_var("CLAWDE_HOME", tmp.path());
            std::fs::write(
                tmp.path().join("settings.json"),
                r#"{"config":{"verify":{"enabled":false}}}"#,
            )
            .unwrap();
            crate::projects::set_repo_root("default", repo.path()).unwrap();
            let mut board = Board::new();
            board.attempts = 3;
            board.attempt_upstreams =
                vec!["zai".to_string(), "nvidia".to_string(), "groq".to_string()];
            board.auto_review = false;
            let a = board.add_card("add a feature");
            seeded_board(&board);
            let wt = git::card_worktree_dir("default", &a);
            let mut b = board::load_board("default").unwrap().unwrap();
            b.set_status(&a, CardStatus::Running);
            b.cards.iter_mut().find(|c| c.id == a).unwrap().branch = Some(format!("katban/{a}"));
            b.cards.iter_mut().find(|c| c.id == a).unwrap().work_dir =
                Some(wt.to_string_lossy().into_owned());
            board::save_board(&b, "default").unwrap();

            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let executor: Arc<dyn CardExecutor> = Arc::new(Scripted {
                calls,
                script: vec![],
                fail_on: vec![],
            });
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
            assert_eq!(card.status, CardStatus::Review);
            assert_eq!(card.attempts.len(), 1, "only rung 1 ran");
            assert_eq!(card.picked_attempt, Some(0));
            assert_eq!(card.attempts[0].upstream, "zai");
            assert_eq!(card.attempts[0].verify_passed, Some(true));
            match previous {
                Some(value) => std::env::set_var("CLAWDE_HOME", value),
                None => std::env::remove_var("CLAWDE_HOME"),
            }
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn ladder_pin_fallthrough_is_excluded_and_ladder_advances() {
        // Rung 1's attribution names a different upstream than its pin
        // (Route::Pinned fell through): it must not win even with the gate
        // off; rung 2 wins instead (spec §4.5).
        let tmp = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        {
            let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var("CLAWDE_HOME").ok();
            std::env::set_var("CLAWDE_HOME", tmp.path());
            std::fs::write(
                tmp.path().join("settings.json"),
                r#"{"config":{"verify":{"enabled":false}}}"#,
            )
            .unwrap();
            crate::projects::set_repo_root("default", repo.path()).unwrap();
            let mut board = Board::new();
            board.attempts = 2;
            board.attempt_upstreams = vec!["nvidia".to_string(), "zai".to_string()];
            board.auto_review = false;
            let a = board.add_card("add a feature");
            seeded_board(&board);
            let wt = git::card_worktree_dir("default", &a);
            let mut b = board::load_board("default").unwrap().unwrap();
            b.set_status(&a, CardStatus::Running);
            b.cards.iter_mut().find(|c| c.id == a).unwrap().branch = Some(format!("katban/{a}"));
            b.cards.iter_mut().find(|c| c.id == a).unwrap().work_dir =
                Some(wt.to_string_lossy().into_owned());
            board::save_board(&b, "default").unwrap();

            // Call 0 serves groq while pinned to nvidia -> fallthrough.
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let executor: Arc<dyn CardExecutor> = Arc::new(Scripted {
                calls,
                script: vec![(0, Some("groq".into()), false)],
                fail_on: vec![],
            });
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
            assert_eq!(card.status, CardStatus::Review);
            assert_eq!(card.picked_attempt, Some(1), "rung 2 won");
            assert_eq!(card.attempts.len(), 2);
            assert_eq!(
                card.attempts[0].verify_passed, None,
                "fallthrough never reached the gate"
            );
            assert!(
                card.attempts[0]
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("pin fell through"),
                "error: {:?}",
                card.attempts[0].error
            );
            assert_eq!(card.attempts[1].upstream, "zai");
            assert_eq!(card.attempts[1].verify_passed, Some(true));
            match previous {
                Some(value) => std::env::set_var("CLAWDE_HOME", value),
                None => std::env::remove_var("CLAWDE_HOME"),
            }
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn ladder_empty_completion_never_wins() {
        // Rung 1 is an empty completion (thinking-only flake): it must not
        // reach the gate and must not win; rung 2 wins (spec §4.6).
        let tmp = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        {
            let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var("CLAWDE_HOME").ok();
            std::env::set_var("CLAWDE_HOME", tmp.path());
            std::fs::write(
                tmp.path().join("settings.json"),
                r#"{"config":{"verify":{"enabled":false}}}"#,
            )
            .unwrap();
            crate::projects::set_repo_root("default", repo.path()).unwrap();
            let mut board = Board::new();
            board.attempts = 2;
            board.attempt_upstreams = vec!["groq".to_string(), "zai".to_string()];
            board.auto_review = false;
            let a = board.add_card("add a feature");
            seeded_board(&board);
            let wt = git::card_worktree_dir("default", &a);
            let mut b = board::load_board("default").unwrap().unwrap();
            b.set_status(&a, CardStatus::Running);
            b.cards.iter_mut().find(|c| c.id == a).unwrap().branch = Some(format!("katban/{a}"));
            b.cards.iter_mut().find(|c| c.id == a).unwrap().work_dir =
                Some(wt.to_string_lossy().into_owned());
            board::save_board(&b, "default").unwrap();

            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let executor: Arc<dyn CardExecutor> = Arc::new(Scripted {
                calls,
                script: vec![(0, Some("groq".into()), true)],
                fail_on: vec![],
            });
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
            assert_eq!(card.status, CardStatus::Review);
            assert_eq!(card.picked_attempt, Some(1));
            assert_eq!(card.attempts[0].verify_passed, None);
            assert_eq!(card.attempts[0].error.as_deref(), Some("empty completion"));
            match previous {
                Some(value) => std::env::set_var("CLAWDE_HOME", value),
                None => std::env::remove_var("CLAWDE_HOME"),
            }
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn ladder_all_rungs_fail_fails_card_with_matrix() {
        // Every rung fails: the card fails with the composed matrix in the
        // result (spec §4.4) and no commit is pinned.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        {
            let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var("CLAWDE_HOME").ok();
            std::env::set_var("CLAWDE_HOME", tmp.path());
            std::fs::write(
                tmp.path().join("settings.json"),
                r#"{"config":{"verify":{"enabled":false}}}"#,
            )
            .unwrap();
            crate::projects::set_repo_root("default", repo.path()).unwrap();
            let mut board = Board::new();
            board.attempts = 2;
            board.attempt_upstreams = vec!["groq".to_string(), "zai".to_string()];
            board.auto_review = false;
            let a = board.add_card("add a feature");
            seeded_board(&board);
            let wt = git::card_worktree_dir("default", &a);
            let mut b = board::load_board("default").unwrap().unwrap();
            b.set_status(&a, CardStatus::Running);
            b.cards.iter_mut().find(|c| c.id == a).unwrap().branch = Some(format!("katban/{a}"));
            b.cards.iter_mut().find(|c| c.id == a).unwrap().work_dir =
                Some(wt.to_string_lossy().into_owned());
            board::save_board(&b, "default").unwrap();

            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let executor: Arc<dyn CardExecutor> = Arc::new(Scripted {
                calls,
                // Every call fails (call 2/3 are the retries of rungs 0/1),
                // so both rungs exhaust including their bounded retry.
                script: vec![],
                fail_on: vec![0, 1, 2, 3],
            });
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
            assert_eq!(card.attempts.len(), 2, "both rungs recorded");
            assert!(card.picked_attempt.is_none());
            let result = card.result.as_deref().unwrap();
            assert!(
                result.contains("groq") && result.contains("zai"),
                "matrix in result: {result}"
            );
            match previous {
                Some(value) => std::env::set_var("CLAWDE_HOME", value),
                None => std::env::remove_var("CLAWDE_HOME"),
            }
        }
    }

    #[test]
    fn parse_attempt_stream_reads_attribution_and_usage() {
        // The stream shapes the headless CLI emits (cli main.rs): attribution,
        // text deltas, tool starts, and the final result with usage.
        let stream = concat!(
            r#"{"type":"provider_attribution","provider_id":"free","upstream_id":"groq","model":"gpt-oss-120b","context_tokens_est":41,"retries":0,"fallback_used":false}"#,
            "\n",
            r#"{"type":"text_delta","text":"hello "}"#,
            "\n",
            r#"{"type":"tool_start","tool":"Edit"}"#,
            "\n",
            r#"{"type":"text_delta","text":"world"}"#,
            "\n",
            r#"{"type":"result","usage":{"input_tokens":10,"output_tokens":32},"cost_usd":0.0,"provider":"free","upstream":"groq","model":"gpt-oss-120b","retries":0,"fallback_used":false}"#,
            "\n",
        );
        let out = parse_attempt_stream(stream);
        assert_eq!(out.served_upstream.as_deref(), Some("groq"));
        assert_eq!(out.model.as_deref(), Some("gpt-oss-120b"));
        assert_eq!(out.output_tokens, 32);
        assert_eq!(out.tool_calls, 1);
        assert_eq!(out.text_chars, "hello world".len());
        assert!(!out.is_empty_completion());
        assert_eq!(out.digest, "hello world");
    }

    #[test]
    fn empty_completion_guard_matches_eval_semantics() {
        // Zero text + zero tool calls + zero output tokens = provider flake
        // (the eval's is_empty_completion), never a gate candidate.
        assert!(AttemptOutput::default().is_empty_completion());
        assert!(!AttemptOutput {
            output_tokens: 1,
            ..AttemptOutput::default()
        }
        .is_empty_completion());
        assert!(!AttemptOutput {
            tool_calls: 2,
            ..AttemptOutput::default()
        }
        .is_empty_completion());
        assert!(!AttemptOutput {
            text_chars: 5,
            ..AttemptOutput::default()
        }
        .is_empty_completion());
    }

    #[test]
    fn rate_limit_classification_covers_the_observed_wording() {
        // The exact error strings the free chain produced in the eval runs.
        for message in [
            "Rate limit exceeded. Please retry after 1s.",
            "http 429 too many requests",
            "upstream returned RATE_LIMIT",
            "quota exhausted for today",
            "retry-after: 30",
        ] {
            assert!(is_rate_limited(message), "{message}");
        }
        assert!(!is_rate_limited("could not start clawde"));
        assert!(!is_rate_limited("test: npm test: exit 1"));
    }

    #[test]
    fn attempts_clamped_to_max() {
        let mut board = Board::new();
        assert_eq!(board.set_attempts(0), 1);
        assert_eq!(board.set_attempts(3), 3);
        assert_eq!(board.set_attempts(50), MAX_ATTEMPTS);
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
