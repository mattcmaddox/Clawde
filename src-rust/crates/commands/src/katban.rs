// `/katban` command — control the Kanban/agent-board surface from inside the
// TUI: boards (cards, statuses, dependencies), the board -> git project
// registry, hosted sites, and the status overview. The TUI's Katban controls
// dialog (Alt+G, `openKatbanControls`) builds its scrollable menu from the
// same stores this command operates on, so both surfaces stay in sync.
//
// The guest chat (Cat Chat) is a separate surface: `/chat` (popup +
// links/create/revoke/password/unblock/status) and the `clawde catchat` CLI.
// Subcommands mirror the `clawde katban` CLI 1:1:
//
//   /katban                         — status overview
//   /katban status                  — status overview
//   /katban board list              — list cards on a board
//   /katban project list            — board -> git repo registry
//   /katban site list               — hosted sites

use super::*;
use async_trait::async_trait;
use clawde_katban::guest::{self, GuestStore};

pub struct KatbanCommand;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Read the live guest store from disk (empty store when absent).
fn load_store() -> GuestStore {
    guest::load().unwrap_or_default()
}

/// Pull `--project NAME` (or `--project=NAME`) out of the arg list, returning
/// the project plus the remaining positional args. Mirrors the CLI's
/// `parse_project_flag` so `/katban board ... --project X` behaves exactly
/// like `clawde katban board ... --project X`.
fn extract_project<'a>(args: &[&'a str]) -> (Option<&'a str>, Vec<&'a str>) {
    let mut project = None;
    let mut rest = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index] {
            "--project" => {
                if let Some(value) = args.get(index + 1) {
                    project = Some(*value);
                    index += 1;
                }
            }
            flag if flag.starts_with("--project=") => {
                project = Some(&flag["--project=".len()..]);
            }
            flag => rest.push(flag),
        }
        index += 1;
    }
    (project, rest)
}

#[async_trait]
impl SlashCommand for KatbanCommand {
    fn name(&self) -> &str {
        "katban"
    }
    fn description(&self) -> &str {
        "Control Katban: boards, projects, sites, status"
    }
    fn help(&self) -> &str {
        "Usage: /katban [subcommand [args...]]\n\n\
         /katban                         — status overview\n\
         /katban status                  — status overview\n\
         /katban board list              — list cards on a board\n\
         /katban board ready             — cards that can start now\n\
         /katban board card add <prompt> — add a card\n\
         /katban board card set <id> <s> — set a card's status\n\
         /katban board card merge <id>   — merge a review card into the project\n\
         /katban board card remove <id>  — remove a card (discards its branch)\n\
         /katban board card comment <id> [--line N] <text> — note a diff-review comment\n\
         /katban board card feedback <id> — send the card's review comments to its agent\n\
         /katban board auto-review on|off — toggle the auto-review pass\n\
         /katban board verify on|off    — toggle the verification gate\n\
         /katban board attempts <N>      — run each card up to N times (1-5); first verify pass wins\n\
         /katban board attempts-upstreams [ids...] — pin the attempts rotation (empty = auto)\n\
         /katban board card show <id>    — show a card's status, result, commit, diff\n\
         /katban board link <a> <b>      — make <a> wait for <b> (cycle-checked)\n\
         /katban board unlink <a> <b>    — remove that dependency\n\
         /katban project list            — board -> git repo registry\n\
         /katban site list               — hosted sites\n\n\
         Board commands take an optional `--project NAME` (default: 'default'),\n\
         matching the CLI: e.g. /katban board list --project my-repo\n\
         Guest chat management is the /chat command (Cat Chat), not /katban.\n\
         Everything this command can do is also reachable from the Katban\n\
         controls menu (Alt+G in the TUI)."
    }
    fn arg_completions(&self, partial: &str) -> Vec<ArgCompletion> {
        let mut out = Vec::new();
        let args: Vec<&str> = partial.split_whitespace().collect();
        // Values must include the already-typed path so `get_arg_completions`'s
        // prefix filter (which compares against the full partial) keeps them
        // (the /keys convention). `strip_typed_path` trims the prefix for
        // display in the popup.
        //
        // `--project` is board-only; when present, every board completion
        // value reproduces it (via `typed`) so the prefix filter keeps them.
        let (project, rest) = extract_project(&args);

        // `--project=<partial>` typed: complete the project name.
        if let Some(flag_idx) = args.iter().position(|a| a.starts_with("--project=")) {
            let partial_proj = &args[flag_idx]["--project=".len()..];
            let prefix = args[..flag_idx].join(" ");
            for name in clawde_katban::board::existing_projects() {
                if name.starts_with(partial_proj) {
                    out.push(ArgCompletion {
                        value: format!("{prefix} --project={name}"),
                        description: "board project".into(),
                        available: true,
                    });
                }
            }
            return out;
        }
        // Bare `--project ` typed: complete the project name.
        if args.last() == Some(&"--project") {
            let prefix = args[..args.len() - 1].join(" ");
            for name in clawde_katban::board::existing_projects() {
                out.push(ArgCompletion {
                    value: format!("{prefix} --project {name}"),
                    description: "board project".into(),
                    available: true,
                });
            }
            return out;
        }

        // Values reconstruct the typed path: `rest` (positionals, project
        // stripped) + the `--project NAME` segment in its original place +
        // the completion tail. `proj_seg` is empty when no project was typed,
        // so the no-project values are byte-identical to the pre-project ones.
        let proj_seg = project
            .map(|p| format!(" --project {p}"))
            .unwrap_or_default();
        match rest.as_slice() {
            // Root: subcommand names (only when nothing but the root is typed).
            [] | ["status"] => {
                for (value, description) in [
                    ("status", "Katban status overview"),
                    ("board", "Kanban boards (cards, statuses)"),
                    ("project", "Board -> git repo registry"),
                    ("site", "Hosted sites"),
                ] {
                    out.push(ArgCompletion {
                        value: value.into(),
                        description: description.into(),
                        available: true,
                    });
                }
            }
            ["board"] => {
                for (tail, description) in [
                    ("list", "List cards on a board"),
                    ("card", "Card actions (add / set / remove)"),
                    ("link", "Make one card wait on another"),
                    ("ready", "Cards that can start now"),
                ] {
                    out.push(ArgCompletion {
                        value: format!("board{proj_seg} {tail}"),
                        description: description.into(),
                        available: true,
                    });
                }
            }
            ["board", "card"] => {
                for (tail, description) in [
                    ("add", "Add a card"),
                    ("set", "Set a card's status"),
                    ("merge", "Merge a review card into the project"),
                    ("remove", "Remove a card"),
                    ("comment", "Note a diff-review comment"),
                    ("feedback", "Send the card's review comments to its agent"),
                    ("show", "Show a card's status, result, commit, diff"),
                ] {
                    out.push(ArgCompletion {
                        value: format!("board card{proj_seg} {tail}"),
                        description: description.into(),
                        available: true,
                    });
                }
            }
            ["board", "card", "set"] => {
                for card in load_board_cards(project) {
                    out.push(ArgCompletion {
                        value: format!("board card set{proj_seg} {}", card.0),
                        description: card.1,
                        available: true,
                    });
                }
            }
            ["board", "card", "set", _id] => {
                for (value, description) in [
                    ("backlog", "Backlog"),
                    ("queued", "Queued"),
                    ("running", "Running"),
                    ("review", "Review"),
                    ("done", "Done"),
                    ("blocked", "Blocked"),
                    ("failed", "Failed"),
                ] {
                    out.push(ArgCompletion {
                        value: format!("board card set {}{proj_seg} {value}", rest[3]),
                        description: description.into(),
                        available: true,
                    });
                }
            }
            ["board", "card", "remove"]
            | ["board", "card", "show"]
            | ["board", "card", "merge"]
            | ["board", "card", "feedback"]
            | ["board", "card", "comment"] => {
                for card in load_board_cards(project) {
                    out.push(ArgCompletion {
                        value: format!("board card {}{proj_seg} {}", rest[2], card.0),
                        description: card.1,
                        available: true,
                    });
                }
            }
            ["board", "link"] | ["board", "unlink"] => {
                // First card id; the second id completes after it is typed.
                for card in load_board_cards(project) {
                    out.push(ArgCompletion {
                        value: format!("board {}{proj_seg} {}", rest[1], card.0),
                        description: card.1,
                        available: true,
                    });
                }
            }
            ["board", "link", _a] | ["board", "unlink", _a] => {
                for card in load_board_cards(project) {
                    out.push(ArgCompletion {
                        value: format!("board {} {}{proj_seg} {}", rest[1], rest[2], card.0),
                        description: card.1,
                        available: true,
                    });
                }
            }
            ["site"] => {
                out.push(ArgCompletion {
                    value: "site list".into(),
                    description: "List hosted sites".into(),
                    available: true,
                });
            }
            ["project"] => {
                out.push(ArgCompletion {
                    value: "project list".into(),
                    description: "List board -> git repo registry".into(),
                    available: true,
                });
            }
            _ => {}
        }
        out
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let parts: Vec<&str> = args.split_whitespace().collect();
        let (project, parts) = extract_project(&parts);
        match parts.as_slice() {
        [] | ["status"] => CommandResult::Message(status_text()),
        ["project", "list"] => CommandResult::Message(project_list_text()),
            ["site", "list"] => CommandResult::Message(site_list_text()),
            ["board", "list"] => CommandResult::Message(board_list_text(project)),
            ["board", "ready"] => CommandResult::Message(board_ready_text(project)),
            ["board", "card", "add", prompt @ ..] => {
                let prompt = prompt.join(" ");
                if prompt.trim().is_empty() {
                    return CommandResult::Error(
                        "board card add needs a prompt: /katban board card add <PROMPT>".to_string(),
                    );
                }
                match board_add_card(project, &prompt) {
                    Ok(text) => CommandResult::Message(text),
                    Err(message) => CommandResult::Error(message),
                }
            }
            ["board", "card", "set", id, status] => {
                match board_set_status(project, id, status) {
                    Ok(text) => CommandResult::Message(text),
                    Err(message) => CommandResult::Error(message),
                }
            }
            ["board", "auto-review", state] => match board_set_auto_review(project, state) {
                Ok(text) => CommandResult::Message(text),
                Err(message) => CommandResult::Error(message),
            },
            ["board", "verify", state] => match board_set_verify(project, state) {
                Ok(text) => CommandResult::Message(text),
                Err(message) => CommandResult::Error(message),
            },
            ["board", "attempts", n] => match board_set_attempts(project, n) {
                Ok(text) => CommandResult::Message(text),
                Err(message) => CommandResult::Error(message),
            },
            ["board", "runtime", token] => match board_set_runtime(project, token) {
                Ok(text) => CommandResult::Message(text),
                Err(message) => CommandResult::Error(message),
            },
            ["board", "card", "scope", id] => match board_set_card_scope(project, id, &[]) {
                Ok(text) => CommandResult::Message(text),
                Err(message) => CommandResult::Error(message),
            },
            ["board", "card", "scope", id, paths @ ..] => {
                match board_set_card_scope(project, id, paths) {
                    Ok(text) => CommandResult::Message(text),
                    Err(message) => CommandResult::Error(message),
                }
            }
            ["board", "attempts-upstreams"] => match board_set_attempt_upstreams(project, &[]) {
                Ok(text) => CommandResult::Message(text),
                Err(message) => CommandResult::Error(message),
            },
            ["board", "attempts-upstreams", ids @ ..] => {
                match board_set_attempt_upstreams(project, ids) {
                    Ok(text) => CommandResult::Message(text),
                    Err(message) => CommandResult::Error(message),
                }
            }
            ["board", "card", "merge", id] => {
                // Option B — pin-commit flow: merge the review card's branch
                // into the project and close it (dependents then unblock).
                match clawde_katban::commit::merge_card(project_name(project), id) {
                    Ok(()) => CommandResult::Message(format!("'{id}' merged into the project")),
                    Err(message) => CommandResult::Error(message),
                }
            }
            ["board", "card", "remove", id] => match board_remove_card(project, id) {
                Ok(text) => CommandResult::Message(text),
                Err(message) => CommandResult::Error(message),
            },
            ["board", "card", "comment", rest @ ..] => {
                // /katban board card comment <ID> [--line N] <TEXT> — note a diff-review
                // comment (spec §16a E5) to feed back to the card's agent.
                if rest.is_empty() {
                    return CommandResult::Error(
                        "board card comment needs an id and text: /katban board card comment <ID> [--line N] <TEXT>".to_string(),
                    );
                }
                let id = rest[0];
                let mut index = 1;
                let mut location = None;
                let mut text_parts: Vec<&str> = Vec::new();
                while index < rest.len() {
                    if rest[index] == "--line" {
                        location = rest.get(index + 1).copied();
                        index += 2;
                    } else if let Some(value) = rest[index].strip_prefix("--line=") {
                        location = Some(value);
                        index += 1;
                    } else {
                        text_parts.push(rest[index]);
                        index += 1;
                    }
                }
                let text = text_parts.join(" ");
                if text.trim().is_empty() {
                    return CommandResult::Error("board card comment needs comment text".to_string());
                }
                match board_add_comment(project, id, location, &text) {
                    Ok(text) => CommandResult::Message(text),
                    Err(message) => CommandResult::Error(message),
                }
            }
            ["board", "card", "feedback", id] => match board_send_feedback(project, id) {
                Ok(text) => CommandResult::Message(text),
                Err(message) => CommandResult::Error(message),
            },
            ["board", "card", "show", id] => match board_show_card(project, id) {
                Ok(text) => CommandResult::Message(text),
                Err(message) => CommandResult::Error(message),
            },
            ["board", "link", from, to] => match board_link(project, from, to) {
                Ok(text) => CommandResult::Message(text),
                Err(message) => CommandResult::Error(message),
            },
            ["board", "unlink", from, to] => match board_unlink(project, from, to) {
                Ok(text) => CommandResult::Message(text),
                Err(message) => CommandResult::Error(message),
            },
            ["board", "link"] | ["board", "unlink"] => CommandResult::Error(
                "board link needs two card ids: /katban board link <A> <B> (B must finish before A starts)".to_string(),
            ),
            ["board"] => {
                CommandResult::Message(
                    "Usage: /katban board list | board ready | board card add <PROMPT> | board card set <ID> <status> | board card merge <ID> | board card remove <ID> | board card comment <ID> [--line N] <TEXT> | board card feedback <ID> | board link <A> <B> | board unlink <A> <B> — add --project NAME to target another board".to_string(),
                )
            }
            ["site"] => {
                CommandResult::Message("Usage: /katban site list".to_string())
            }
            ["link", ..] | ["guest", ..] => CommandResult::Error(
                "guest links moved to Cat Chat: /chat, /chat links, /chat create <NAME>, /chat unblock <IP>".to_string(),
            ),
            _ => CommandResult::Error(
                "Unknown /katban subcommand. Try /katban, /katban board list, or /katban help."
                    .to_string(),
            ),
        }
    }
}

fn status_text() -> String {
    let status = clawde_katban::status::status();
    let store = load_store();
    let now = now_secs();
    let active_links = store
        .links
        .iter()
        .filter(|link| guest::link_active(link, now))
        .count();
    let blocked: Vec<&str> = store
        .failed_attempts
        .iter()
        .filter(|(_, attempt)| {
            attempt.blocked_until.is_some_and(|u| u > now)
                || attempt.locked_until.is_some_and(|u| u > now)
        })
        .map(|(ip, _)| ip.as_str())
        .collect();
    let mut out = String::new();
    out.push_str(&format!("data dir:    {}\n", status.data_dir.display()));
    out.push_str(&format!(
        "sites:       {} ({} exposed)\n",
        status.site_count, status.exposed_count
    ));
    out.push_str(&format!(
        "boards:      {}\n",
        if status.board_projects.is_empty() {
            "none".to_string()
        } else {
            status.board_projects.join(", ")
        }
    ));
    out.push_str(&format!(
        "runnable:    {}\n",
        if status.runnable_projects.is_empty() {
            "none (register a repo: clawde katban project set <NAME> <DIR>)".to_string()
        } else {
            status.runnable_projects.join(", ")
        }
    ));
    out.push_str(&format!(
        "guest links: {active_links} active (manage with /chat — Cat Chat)\n"
    ));
    out.push_str(&format!(
        "locked IPs:  {}\n",
        if blocked.is_empty() {
            "none".to_string()
        } else {
            blocked.join(", ")
        }
    ));
    out.push_str(&format!(
        "caddy:       {} ({})\n",
        status.managed_caddy_path.display(),
        if status.managed_caddy_exists {
            "in place"
        } else {
            "missing — run an expose command to write it"
        }
    ));
    out.push_str("\nTry /katban board list, /katban site list, or Alt+G for the controls menu.");
    out
}

fn site_list_text() -> String {
    let config = clawde_katban::config::load().unwrap_or_default();
    if config.sites.is_empty() {
        return "no hosted sites — add one with: clawde katban site add <DIR>".to_string();
    }
    let mut out = format!("{:<16} {:<8} {:<28} PORT\n", "NAME", "STATE", "PUBLIC");
    for site in &config.sites {
        out.push_str(&format!(
            "{:<16} {:<8} {:<28} {}\n",
            site.name,
            if site.locked { "locked" } else { "live" },
            site.public_subdomain.as_deref().unwrap_or("(local only)"),
            site.port,
        ));
    }
    out
}

fn project_list_text() -> String {
    let registry = clawde_katban::projects::load().unwrap_or_default();
    if registry.projects.is_empty() {
        return "no projects registered to a git repo — register one with: clawde katban project set <NAME> <DIR> (or /katban can't run cards)".to_string();
    }
    let mut out = format!("{:<20} REPO ROOT\n", "PROJECT");
    for (name, root) in &registry.projects {
        out.push_str(&format!("{name:<20} {root}\n"));
    }
    out.trim_end().to_string()
}

// ---- Board helpers ---------------------------------------------------------

fn board(project: Option<&str>) -> clawde_katban::board::Board {
    let project = project.unwrap_or(clawde_katban::board::DEFAULT_PROJECT);
    clawde_katban::board::load_board(project)
        .ok()
        .flatten()
        .unwrap_or_default()
}

fn save_board(board: &clawde_katban::board::Board, project: Option<&str>) -> Result<(), String> {
    let project = project.unwrap_or(clawde_katban::board::DEFAULT_PROJECT);
    clawde_katban::board::save_board(board, project).map_err(|e| format!("could not save: {e:#}"))
}

/// `(id, prompt)` for every card on the board — used by the completion popup.
fn load_board_cards(project: Option<&str>) -> Vec<(String, String)> {
    board(project)
        .cards
        .into_iter()
        .map(|card| (card.id, card.prompt))
        .collect()
}

fn board_list_text(project: Option<&str>) -> String {
    let board = board(project);
    let project = project.unwrap_or(clawde_katban::board::DEFAULT_PROJECT);
    if board.cards.is_empty() {
        return format!(
            "board '{project}' has no cards — add one with /katban board card add <PROMPT>"
        );
    }
    let mut out = format!("board '{project}':\n");
    for card in &board.cards {
        out.push_str(&format!(
            "{:<12} {:>8}  {}\n",
            card.id,
            board_status_name(card.status),
            card.prompt
        ));
    }
    out
}

fn board_ready_text(project: Option<&str>) -> String {
    let board = board(project);
    let running: std::collections::HashSet<String> = board
        .cards
        .iter()
        .filter(|card| card.status == clawde_katban::board::CardStatus::Running)
        .map(|card| card.id.clone())
        .collect();
    let ready = board.queued_ids(&running, board.parallel_cap);
    if ready.is_empty() {
        return "nothing ready to run".to_string();
    }
    let mut out = String::new();
    for id in &ready {
        let prompt = board.card(id).map(|c| c.prompt.as_str()).unwrap_or("?");
        out.push_str(&format!("{id}  {prompt}\n"));
    }
    out
}

/// The project name, defaulted like the CLI does.
fn project_name(project: Option<&str>) -> &str {
    project.unwrap_or(clawde_katban::board::DEFAULT_PROJECT)
}

/// Hold the project's board lock across a mutation's read-modify-write so a
/// concurrent writer (CLI / TUI / future runner) can't silently drop either
/// change. `f` runs with the freshly-loaded board and must save it.
fn with_board_lock<T>(
    project: Option<&str>,
    f: impl FnOnce(&mut clawde_katban::board::Board) -> T,
) -> Result<T, String> {
    let project = project_name(project);
    let _guard = clawde_katban::board::BoardLock::acquire(project)
        .map_err(|e| format!("could not lock board: {e:#}"))?;
    let mut board = clawde_katban::board::load_board(project)
        .map_err(|e| format!("could not load board: {e:#}"))?
        .unwrap_or_default();
    let out = f(&mut board);
    save_board(&board, Some(project)).map_err(|e| format!("could not save board: {e:#}"))?;
    Ok(out)
}

fn board_add_card(project: Option<&str>, prompt: &str) -> Result<String, String> {
    let out = with_board_lock(project, |board| {
        let id = board.add_card(prompt);
        format!("{id}  {prompt}")
    })?;
    Ok(out)
}

fn board_set_status(project: Option<&str>, id: &str, status: &str) -> Result<String, String> {
    let parsed = clawde_katban::board::CardStatus::parse(status)
        .ok_or_else(|| format!("unknown status '{status}' — try backlog, queued, running, review, done, blocked, failed"))?;
    // A bare "done" on a card with a pinned commit is an explicit discard: the
    // `katban/<id>` branch must be cleaned up, not leaked forever. `discard_card`
    // locks and deletes the branch (if any) + marks the card done.
    if parsed == clawde_katban::board::CardStatus::Done {
        clawde_katban::commit::discard_card(project_name(project), id)?;
        return Ok(format!("'{id}' -> done (branch cleaned up)"));
    }
    if !with_board_lock(project, |board| board.set_status(id, parsed))? {
        return Err(format!("no card with id '{id}'"));
    }
    Ok(format!("'{id}' -> {}", board_status_name(parsed)))
}

fn board_remove_card(project: Option<&str>, id: &str) -> Result<String, String> {
    // Discard: archive the card AND delete its pinned branch (if any).
    clawde_katban::commit::discard_card(project_name(project), id)?;
    Ok(format!("'{id}' archived (branch cleaned up)"))
}

/// Toggle the board's auto-review pass (`/katban board auto-review on|off`).
/// The pass is enabled by default; disabling it skips the second headless
/// review agent for future cards (the verification gate still runs).
fn board_set_auto_review(project: Option<&str>, state: &str) -> Result<String, String> {
    let enabled = match state {
        "on" => true,
        "off" => false,
        other => return Err(format!("auto-review needs on or off, got '{other}'")),
    };
    with_board_lock(project, |board| board.auto_review = enabled)?;
    Ok(format!(
        "auto-review {} for board '{}'",
        if enabled { "enabled" } else { "disabled" },
        project_name(project)
    ))
}

/// Set the board's per-card attempts:N ladder count
/// (`/katban board attempts <N>`, spec §7).
fn board_set_attempts(project: Option<&str>, n: &str) -> Result<String, String> {
    let value: u32 = n.parse().map_err(|_| {
        format!(
            "attempts must be a number 1-{}, got '{n}'",
            clawde_katban::board::MAX_ATTEMPTS
        )
    })?;
    if !(1..=clawde_katban::board::MAX_ATTEMPTS).contains(&value) {
        return Err(format!(
            "attempts must be 1-{}, got {value}",
            clawde_katban::board::MAX_ATTEMPTS
        ));
    }
    let stored = with_board_lock(project, |board| board.set_attempts(value))?;
    Ok(format!(
        "attempts {stored} for board '{}'",
        project_name(project)
    ))
}

/// Set where card attempts execute (`/katban board runtime incus|host`,
/// spec §8). Container attempts need the `incus` CLI; `host` is the default.
fn board_set_runtime(project: Option<&str>, token: &str) -> Result<String, String> {
    let Some(runtime) = clawde_katban::board::ContainerRuntime::parse(token) else {
        return Err(format!("runtime needs 'host' or 'incus', got '{token}'"));
    };
    if runtime == clawde_katban::board::ContainerRuntime::Incus
        && !clawde_katban::container::available()
    {
        return Err(
            "incus is not reachable (`incus list` failed) — is incusd running?".to_string(),
        );
    }
    let stored = with_board_lock(project, |board| board.set_runtime(runtime))?;
    Ok(format!(
        "runtime {} for board '{}'",
        stored.as_str(),
        project_name(project)
    ))
}

/// Set a card's scope allowlist (`/katban board card scope <ID> [PATH...]`);
/// empty = no scope opinion. Enforced by the FS-manifest scope gate on the
/// container tier.
fn board_set_card_scope(project: Option<&str>, id: &str, paths: &[&str]) -> Result<String, String> {
    let list: Vec<String> = paths.iter().map(|s| s.to_string()).collect();
    let stored = with_board_lock(project, |board| {
        if !board.set_card_scope(id, list) {
            return Err(format!("no such card: {id}"));
        }
        Ok::<_, String>(
            board
                .card(id)
                .map(|c| c.scope_paths.clone())
                .unwrap_or_default(),
        )
    })??;
    if stored.is_empty() {
        Ok(format!("scope cleared for card {id} (no scope opinion)"))
    } else {
        Ok(format!("scope for card {id}: {}", stored.join(", ")))
    }
}

/// Pin the attempts rotation (`/katban board attempts-upstreams [ID...]`);
/// empty = auto-derive from the keyed free catalog (spec §6).
fn board_set_attempt_upstreams(project: Option<&str>, ids: &[&str]) -> Result<String, String> {
    for id in ids {
        if clawde_api::providers::free::catalog_entry(id).is_none() {
            return Err(format!("unknown upstream '{id}' — not in the free catalog"));
        }
    }
    let names: Vec<String> = with_board_lock(project, |board| {
        board.attempt_upstreams = ids.iter().map(|s| s.to_string()).collect();
        board.attempt_upstreams.clone()
    })?;
    let project = project_name(project);
    if names.is_empty() {
        Ok(format!(
            "attempts-upstreams auto (derived from the free catalog) for board '{project}'"
        ))
    } else {
        Ok(format!(
            "attempts-upstreams {} for board '{project}'",
            names.join(", ")
        ))
    }
}

/// Toggle the board's verification gate (`/katban board verify on|off`).
/// Enabled by default; disabling it lets cards reach review without running
/// the project's checks in the worktree (the global `config.verify.enabled`
/// setting must also be on for the gate to run).
fn board_set_verify(project: Option<&str>, state: &str) -> Result<String, String> {
    let enabled = match state {
        "on" => true,
        "off" => false,
        other => return Err(format!("verify needs on or off, got '{other}'")),
    };
    with_board_lock(project, |board| board.verify = enabled)?;
    Ok(format!(
        "verify {} for board '{}'",
        if enabled { "enabled" } else { "disabled" },
        project_name(project)
    ))
}

/// Append a diff-review comment (spec §16a E5). `board::add_review` locks
/// and saves the board itself, so no board lock is held here.
fn board_add_comment(
    project: Option<&str>,
    card_id: &str,
    location: Option<&str>,
    text: &str,
) -> Result<String, String> {
    let location = location
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty());
    let comment_id =
        clawde_katban::board::add_review(project_name(project), card_id, location, text)
            .map_err(|e| format!("could not comment: {e}"))?;
    Ok(format!("comment {comment_id} added to '{card_id}'"))
}

/// Send a review card's comments back to its agent as a follow-up run: requeues
/// the card and appends the composed feedback to its next prompt (spec §16a E5).
fn board_send_feedback(project: Option<&str>, card_id: &str) -> Result<String, String> {
    let count = clawde_katban::board::send_feedback_to_agent(project_name(project), card_id)
        .map_err(|e| format!("could not send feedback: {e}"))?;
    Ok(format!(
        "sent {count} comment(s) back to '{card_id}' — card requeued for a follow-up run"
    ))
}

fn board_show_card(project: Option<&str>, id: &str) -> Result<String, String> {
    let board = board(project);
    let card = board
        .card(id)
        .ok_or_else(|| format!("no card with id '{id}'"))?;
    let mut out = format!(
        "id:      {}\nprompt:  {}\nstatus:  {}",
        card.id,
        card.prompt,
        board_status_name(card.status)
    );
    if let Some(branch) = &card.branch {
        out.push_str(&format!("\nbranch:  {branch}"));
    }
    if let Some(commit) = &card.commit {
        let short = commit.get(..commit.len().min(12)).unwrap_or(commit);
        out.push_str(&format!("\ncommit:  {short}"));
    }
    if card.retries > 0 {
        out.push_str(&format!("\nretries: {}", card.retries));
    }
    if let Some(result) = &card.result {
        out.push_str(&format!("\n\nresult:\n{result}"));
    }
    if let Some(diff) = &card.diff {
        out.push_str(&format!("\n\ndiff ({} ch):\n{diff}", diff.len()));
    }
    if !card.reviews.is_empty() {
        out.push_str("\n\nreviews:");
        for r in &card.reviews {
            match &r.location {
                Some(loc) => out.push_str(&format!("\n[L{loc}] {}", r.text)),
                None => out.push_str(&format!("\n  {}", r.text)),
            }
        }
    }
    if let Some(fb) = &card.followup_feedback {
        out.push_str(&format!(
            "\n\npending feedback (sent to agent on the next run):\n{fb}"
        ));
    }
    Ok(out)
}

fn board_link(project: Option<&str>, from: &str, to: &str) -> Result<String, String> {
    with_board_lock(project, |board| {
        board
            .add_dependency(from, to)
            .map_err(|message| format!("cannot link: {message}"))
    })?
    .map(|_| format!("'{from}' now waits for '{to}'"))
}

fn board_unlink(project: Option<&str>, from: &str, to: &str) -> Result<String, String> {
    let removed = with_board_lock(project, |board| board.remove_dependency(from, to))?;
    if !removed {
        return Err(format!("no such link between '{from}' and '{to}'"));
    }
    Ok(format!("removed link '{from}' -> '{to}'"))
}

fn board_status_name(status: clawde_katban::board::CardStatus) -> &'static str {
    match status {
        clawde_katban::board::CardStatus::Backlog => "backlog",
        clawde_katban::board::CardStatus::Queued => "queued",
        clawde_katban::board::CardStatus::Running => "running",
        clawde_katban::board::CardStatus::Blocked => "blocked",
        clawde_katban::board::CardStatus::Review => "review",
        clawde_katban::board::CardStatus::Failed => "failed",
        clawde_katban::board::CardStatus::Done => "done",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn test_context(working_dir: &std::path::Path) -> CommandContext {
        CommandContext {
            config: clawde_core::Config::default(),
            cost_tracker: clawde_core::CostTracker::new(),
            messages: Vec::new(),
            working_dir: working_dir.to_path_buf(),
            session_id: "katban-test".to_string(),
            session_title: None,
            remote_session_url: None,
            mcp_manager: None,
            mcp_auth_runner: None,
            provider_registry: None,
            test_provider: None,
            effort: None,
            tool_use_tracker: None,
            autonomy: None,
            transient_prev_config: None,
        }
    }

    /// Run `f` with `CLAWDE_HOME` pointed at a temp dir. Shares the commands
    /// crate's `CLAWDE_HOME_LOCK` (lib.rs tests) with every other env-mutating
    /// test so these serialize under the parallel runner.
    fn with_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        let _guard = crate::tests::CLAWDE_HOME_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::var("CLAWDE_HOME").ok();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("CLAWDE_HOME", tmp.path());
        let result = f(tmp.path());
        match previous {
            Some(value) => std::env::set_var("CLAWDE_HOME", value),
            None => std::env::remove_var("CLAWDE_HOME"),
        }
        result
    }

    #[test]
    fn arg_completions_offer_subcommands() {
        with_home(|_| {
            // Katban is board/project/site only — guest-link subcommands
            // moved to /chat.
            let completions = crate::get_arg_completions("katban", "");
            let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
            assert!(values.contains(&"status"));
            assert!(values.contains(&"board"));
            assert!(values.contains(&"project"));
            assert!(values.contains(&"site"));
            assert!(!values.contains(&"link"));
            assert!(!values.contains(&"guest"));
        });
    }

    #[test]
    fn katban_link_and_guest_redirect_to_chat() {
        with_home(|tmp| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let mut ctx = test_context(tmp);
            for args in ["link list", "guest unblock 1.2.3.4"] {
                let result = rt.block_on(KatbanCommand.execute(args, &mut ctx));
                let CommandResult::Error(text) = result else {
                    panic!("expected redirect error for '{args}', got {result:?}");
                };
                assert!(
                    text.contains("/chat"),
                    "redirect must point at /chat: {text}"
                );
            }
        });
    }

    #[test]
    fn board_add_list_set_remove_round_trip() {
        with_home(|tmp| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let cmd = KatbanCommand;
            let mut ctx = test_context(tmp);

            let result =
                rt.block_on(cmd.execute("board card add build the landing page", &mut ctx));
            let CommandResult::Message(text) = result else {
                panic!("expected message, got {result:?}");
            };
            let id = text.split_whitespace().next().unwrap().to_string();

            let result =
                rt.block_on(cmd.execute(&format!("board card set {id} running"), &mut ctx));
            assert!(matches!(result, CommandResult::Message(_)));
            let board = clawde_katban::board::load_board("default")
                .unwrap()
                .unwrap();
            assert_eq!(
                board.card(&id).unwrap().status,
                clawde_katban::board::CardStatus::Running
            );

            // Bad status is refused.
            let result = rt.block_on(cmd.execute(&format!("board card set {id} bogus"), &mut ctx));
            assert!(matches!(result, CommandResult::Error(_)));

            let result = rt.block_on(cmd.execute("board list", &mut ctx));
            let CommandResult::Message(text) = result else {
                panic!("expected message, got {result:?}");
            };
            assert!(text.contains("build the landing page"));

            let result = rt.block_on(cmd.execute(&format!("board card remove {id}"), &mut ctx));
            assert!(matches!(result, CommandResult::Message(_)));
            let board = clawde_katban::board::load_board("default")
                .unwrap()
                .unwrap();
            assert_eq!(
                board.card(&id).unwrap().status,
                clawde_katban::board::CardStatus::Done
            );
        });
    }

    #[test]
    fn board_card_merge_rejects_a_card_without_a_pinned_commit() {
        // Dispatch smoke test: `/katban board card merge` is wired and routes a
        // non-mergeable card's rejection back as an Error (Option B guard).
        with_home(|tmp| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let mut ctx = test_context(tmp);
            let cmd = KatbanCommand;

            let result = rt.block_on(cmd.execute("board card add ship the thing", &mut ctx));
            let CommandResult::Message(text) = result else {
                panic!("expected message, got {result:?}");
            };
            let id = text.split_whitespace().next().unwrap().to_string();

            // A backlog card (no review, no commit) cannot be merged.
            let result = rt.block_on(cmd.execute(&format!("board card merge {id}"), &mut ctx));
            let CommandResult::Error(err) = result else {
                panic!("expected error, got {result:?}");
            };
            assert!(err.contains("review"), "err: {err}");

            // `show` now also reports the pinned commit when one exists.
            let result = rt.block_on(cmd.execute(&format!("board card show {id}"), &mut ctx));
            let CommandResult::Message(text) = result else {
                panic!("expected message, got {result:?}");
            };
            assert!(text.contains("backlog"), "text: {text}");
        });
    }

    #[test]
    fn board_completions_offer_card_ids() {
        with_home(|_| {
            let mut board = clawde_katban::board::Board::new();
            let id = board.add_card("ship it");
            clawde_katban::board::save_board(&board, "default").unwrap();

            let completions = crate::get_arg_completions("katban", "board card set ");
            let values: Vec<String> = completions.iter().map(|c| c.value.clone()).collect();
            assert!(
                values.contains(&format!("board card set {id}")),
                "completions: {values:?}"
            );
        });
    }

    #[test]
    fn board_commands_accept_project_flag() {
        with_home(|tmp| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let mut ctx = test_context(tmp);

            // Add to a named project, then read it back from that project's
            // board file — the default board must stay untouched.
            let result = rt.block_on(
                KatbanCommand.execute("board card add wire the db --project my-repo", &mut ctx),
            );
            let CommandResult::Message(text) = result else {
                panic!("expected message, got {result:?}");
            };
            let id = text.split_whitespace().next().unwrap().to_string();

            let board = clawde_katban::board::load_board("my-repo")
                .unwrap()
                .unwrap();
            assert_eq!(board.card(&id).unwrap().prompt, "wire the db");
            assert!(clawde_katban::board::load_board("default")
                .unwrap()
                .is_none());

            // `--project=NAME` spelling and status set round-trip.
            let result = rt.block_on(KatbanCommand.execute(
                &format!("board card set {id} running --project=my-repo"),
                &mut ctx,
            ));
            assert!(matches!(result, CommandResult::Message(_)));
            let board = clawde_katban::board::load_board("my-repo")
                .unwrap()
                .unwrap();
            assert_eq!(
                board.card(&id).unwrap().status,
                clawde_katban::board::CardStatus::Running
            );
        });
    }

    #[test]
    fn board_link_unlink_round_trip_and_cycle_refused() {
        with_home(|tmp| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let cmd = KatbanCommand;
            let mut ctx = test_context(tmp);

            let a = match rt.block_on(cmd.execute("board card add backend", &mut ctx)) {
                CommandResult::Message(text) => text.split_whitespace().next().unwrap().to_string(),
                other => panic!("expected message, got {other:?}"),
            };
            let b = match rt.block_on(cmd.execute("board card add frontend", &mut ctx)) {
                CommandResult::Message(text) => text.split_whitespace().next().unwrap().to_string(),
                other => panic!("expected message, got {other:?}"),
            };

            // Link: frontend waits for backend.
            let result = rt.block_on(cmd.execute(&format!("board link {b} {a}"), &mut ctx));
            assert!(matches!(result, CommandResult::Message(_)));
            let board = clawde_katban::board::load_board("default")
                .unwrap()
                .unwrap();
            assert!(!board.ready_to_run(&b));
            assert!(board.ready_to_run(&a));

            // The reverse link is a cycle and is refused with a clear error.
            let result = rt.block_on(cmd.execute(&format!("board link {a} {b}"), &mut ctx));
            let CommandResult::Error(message) = result else {
                panic!("cycle link should error, got {result:?}");
            };
            assert!(message.contains("loop forever"), "error: {message}");

            // Unlink restores readiness, and a missing link errors.
            let result = rt.block_on(cmd.execute(&format!("board unlink {b} {a}"), &mut ctx));
            assert!(matches!(result, CommandResult::Message(_)));
            let board = clawde_katban::board::load_board("default")
                .unwrap()
                .unwrap();
            assert!(board.ready_to_run(&b));

            let result = rt.block_on(cmd.execute(&format!("board unlink {b} {a}"), &mut ctx));
            assert!(matches!(result, CommandResult::Error(_)));

            // Missing the second id -> helpful error, not "unknown subcommand".
            let result = rt.block_on(cmd.execute("board link", &mut ctx));
            assert!(matches!(result, CommandResult::Error(_)));
        });
    }

    #[test]
    fn board_completions_offer_link_ids() {
        with_home(|_| {
            let mut board = clawde_katban::board::Board::new();
            let a = board.add_card("backend");
            let b = board.add_card("frontend");
            clawde_katban::board::save_board(&board, "default").unwrap();

            let completions = crate::get_arg_completions("katban", "board link ");
            let values: Vec<String> = completions.iter().map(|c| c.value.clone()).collect();
            assert!(values.contains(&format!("board link {a}")), "{values:?}");
            assert!(values.contains(&format!("board link {b}")), "{values:?}");

            // Second id completes after the first is typed.
            let completions = crate::get_arg_completions("katban", &format!("board link {a} "));
            let values: Vec<String> = completions.iter().map(|c| c.value.clone()).collect();
            assert!(
                values.contains(&format!("board link {a} {b}")),
                "{values:?}"
            );
        });
    }

    #[test]
    fn project_list_lists_registered_repos_and_guides_when_empty() {
        with_home(|tmp| {
            let repo = tempfile::tempdir().unwrap();
            use clawde_katban::projects;
            projects::set_repo_root("my-repo", repo.path()).unwrap();

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let mut ctx = test_context(tmp);
            let result = rt.block_on(KatbanCommand.execute("project list", &mut ctx));
            let CommandResult::Message(text) = result else {
                panic!("expected message, got {result:?}");
            };
            assert!(text.contains("my-repo"), "text: {text}");

            // Empty registry -> helpful guidance, not an error. Clear in place
            // (the env guard is already held by with_home, so we must not
            // touch CLAWDE_HOME or re-lock).
            projects::save(&projects::ProjectRegistry::default()).unwrap();
            let result = rt.block_on(KatbanCommand.execute("project list", &mut ctx));
            let CommandResult::Message(text) = result else {
                panic!("expected message, got {result:?}");
            };
            assert!(text.contains("project set"), "text: {text}");
        });
    }

    #[test]
    fn board_card_show_prints_details() {
        with_home(|tmp| {
            let mut board = clawde_katban::board::Board::new();
            let id = board.add_card("fix the bug");
            let b = &mut board.cards[0];
            b.retries = 1;
            b.status = clawde_katban::board::CardStatus::Failed;
            b.result = Some("boom".into());
            b.diff = Some("--- a/x.rs\n+++ b/x.rs\n".into());
            clawde_katban::board::save_board(&board, "default").unwrap();

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let mut ctx = test_context(tmp);
            let result =
                rt.block_on(KatbanCommand.execute(&format!("board card show {id}"), &mut ctx));
            let CommandResult::Message(text) = result else {
                panic!("expected message, got {result:?}");
            };
            assert!(text.contains("fix the bug"), "text: {text}");
            assert!(text.contains("failed"), "text: {text}");
            assert!(text.contains("retries: 1"), "text: {text}");
            assert!(text.contains("result:\nboom"), "text: {text}");
            assert!(text.contains("diff ("), "text: {text}");
            assert!(text.contains("x.rs"), "text: {text}");

            // Missing id -> helpful error.
            let result = rt.block_on(KatbanCommand.execute("board card show nope", &mut ctx));
            assert!(matches!(result, CommandResult::Error(_)));
        });
    }

    #[test]
    fn board_completions_complete_project_names() {
        with_home(|_| {
            let mut board = clawde_katban::board::Board::new();
            let id = board.add_card("x");
            clawde_katban::board::save_board(&board, "my-repo").unwrap();

            // Bare `--project ` completes the project name.
            let completions = crate::get_arg_completions("katban", "board list --project ");
            let values: Vec<String> = completions.iter().map(|c| c.value.clone()).collect();
            assert!(
                values.iter().any(|v| v == "board list --project my-repo"),
                "completions: {values:?}"
            );

            // A project-typed card completion carries the flag through, so the
            // prefix filter keeps it and the card ids come from that project's
            // board, not the default one.
            let completions =
                crate::get_arg_completions("katban", "board card set --project my-repo ");
            let values: Vec<String> = completions.iter().map(|c| c.value.clone()).collect();
            let want = format!("board card set --project my-repo {id}");
            assert!(values.contains(&want), "completions: {values:?}");
        });
    }
}
