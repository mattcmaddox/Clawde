// katban_controls.rs — scrollable Katban controls menu.
//
// Opened by Alt+G (`openKatbanControls`, see app.rs): a compact popup listing
// the Kanban/agent-board operations the user most often needs without leaving
// the TUI — status overview, Kanban board control (list / ready / add /
// per-card advance or merge / review feedback), the board's own settings
// (verify gate, auto-review, attempts ladder, card runtime), the independent
// Katban admin credential and its locked buckets, and the project/site
// listings. Guest-chat link management is NOT here: that surface is the
// chat-focused Cat Chat popup, opened by `/chat` (see cat_chat.rs).
//
// The menu is built live from the Katban admin + board stores each time it
// opens, so it always reflects the real state: one row per card (with its
// status), one row per live locked/blocked bucket, and toggles whose titles
// state the value they will flip. Enter writes the row's `/katban ...` command
// into the prompt and closes the menu, so the user always sees exactly what is
// about to run: a complete row (`complete`, e.g. a card id or bucket already in
// the row, or a toggle with its target state) is ready to run as-is, while an
// incomplete one (a card prompt, an attempt count) is a prefix to finish.
//
// The run itself belongs to the caller: the interactive loop only submits a
// prompt when no modal is open (`crates/cli/src/main.rs`), and the menu is a
// modal while it is up, so a complete row runs on the next Enter rather than
// the one that picked it.
//
// Navigation resolves through `KeyContext::KatbanControls` (see
// `App::handle_katban_controls_navigation`), so the menu's keys are listed in
// the `/keybindings` reference and rebindable like every other overlay's.
//
// This is the "living" surface: as Katban gains a *setting* or a board/status
// operation, add a menu row that maps to the new `/katban` subcommand. Card
// editing (edit / remove / comment / scope) stays on `/katban` and the web
// board, where there is room to read what a destructive row would do.

use clawde_katban::admin_auth;
use clawde_katban::board::CardStatus;
use clawde_katban::http_security::describe_lockouts;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Widget};
use ratatui::Frame;

use crate::overlays::{
    centered_rect, render_dark_overlay, render_dialog_bg, CLAWDE_ACCENT, CLAWDE_MUTED,
    CLAWDE_PANEL_BG, CLAWDE_PANEL_BORDER, CLAWDE_TEXT,
};

/// The board the menu reads its live state from (the slash/CLI default).
const BOARD: &str = "default";

/// One row in the controls menu.
#[derive(Debug, Clone)]
pub struct KatbanControlItem {
    /// Primary label (e.g. "Rotate password — friends").
    pub title: String,
    /// Secondary, dimmer line (e.g. "expires never · 2 devices").
    pub subtitle: String,
    /// The `/katban ...` command this row runs.
    pub command: String,
    /// True when `command` is a complete command that can run as soon as it is
    /// submitted; false when the row only seeds a prefix for the user to finish
    /// (e.g. a card prompt, an attempt count).
    pub complete: bool,
}

/// State for the Katban controls menu.
#[derive(Default)]
pub struct KatbanControlsState {
    pub visible: bool,
    pub items: Vec<KatbanControlItem>,
    pub selected: usize,
    /// First visible item index (scroll position).
    pub scroll: usize,
}

/// Max popup width (rows + chrome); narrower terminals shrink it.
const MAX_WIDTH: u16 = 64;
/// Max popup height (rows + chrome); longer menus scroll.
const MAX_HEIGHT: u16 = 24;
/// How many rows the selection keeps in view.
const SCROLL_WINDOW: usize = 18;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn board_status_name(status: CardStatus) -> &'static str {
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

/// Section header helper — a row that is never selectable.
fn section(title: &str) -> KatbanControlItem {
    KatbanControlItem {
        title: format!("▸ {title}"),
        subtitle: String::new(),
        command: String::new(),
        complete: false,
    }
}

/// Build the menu from Katban's admin and board stores. Cat Chat links and
/// guest lockouts belong exclusively to the `/chat` popup.
pub fn build_control_items() -> Vec<KatbanControlItem> {
    let admin = admin_auth::load().unwrap_or_default();
    let now = now_secs();
    let board = clawde_katban::board::load_board(BOARD)
        .ok()
        .flatten()
        .unwrap_or_default();

    let mut items = vec![
        section("Status"),
        KatbanControlItem {
            title: "Katban overview".into(),
            subtitle: "boards, projects, dev sites, admin access".into(),
            command: "/katban status".into(),
            complete: true,
        },
        section("Boards"),
        KatbanControlItem {
            title: "List cards".into(),
            subtitle: format!("{BOARD} board"),
            command: "/katban board list".into(),
            complete: true,
        },
        KatbanControlItem {
            title: "Cards ready to run".into(),
            subtitle: format!("respects the parallel cap ({})", board.parallel_cap),
            command: "/katban board ready".into(),
            complete: true,
        },
        KatbanControlItem {
            title: "Add a card".into(),
            subtitle: "prompt is the card's task".into(),
            command: "/katban board card add ".into(),
            complete: false,
        },
        KatbanControlItem {
            title: "Link cards".into(),
            subtitle: "one card waits on another (cycle-checked)".into(),
            command: "/katban board link ".into(),
            complete: false,
        },
    ];

    // ---- One row per open card --------------------------------------------
    for card in &board.cards {
        if card.status == CardStatus::Done {
            continue;
        }
        let preview: String = card.prompt.chars().take(32).collect();
        // A reviewed card's `next()` status is `done`, and `done` means
        // *discard*: `board card set <id> done` deletes the card's pinned
        // branch. These rows submit on a single Enter, so offering that here
        // was a silent-discard footgun — the board web UI removed its review
        // "advance" row for exactly this reason. A reviewed card offers the
        // safe exit instead, merge, and the row seeds the command rather than
        // submitting it so the user reads what will run first.
        if card.status == CardStatus::Review {
            items.push(KatbanControlItem {
                title: format!("Merge — {preview}"),
                subtitle: "lands the reviewed commit and closes the card".into(),
                command: format!("/katban board card merge {}", card.id),
                complete: false,
            });
            items.push(KatbanControlItem {
                title: format!("Send review feedback — {preview}"),
                subtitle: "hands the card's diff comments to its agent".into(),
                command: format!("/katban board card feedback {}", card.id),
                complete: false,
            });
            items.push(KatbanControlItem {
                title: format!("Inspect — {preview}"),
                subtitle: "status, result, commit and diff".into(),
                command: format!("/katban board card show {}", card.id),
                complete: true,
            });
            continue;
        }
        let status = board_status_name(card.status);
        let next = card.status.next().map(board_status_name).unwrap_or(status);
        items.push(KatbanControlItem {
            title: format!("Advance — {preview}"),
            subtitle: format!("{status} → {next}"),
            command: format!("/katban board card set {} {next}", card.id),
            complete: true,
        });
    }

    // ---- Board settings, read live so each row says what it will flip ------
    items.push(section("Board settings"));
    items.push(KatbanControlItem {
        title: format!(
            "Verification gate — {}",
            if board.verify { "on" } else { "off" }
        ),
        subtitle: "runs the project's checks before a card reaches review".into(),
        command: format!(
            "/katban board verify {}",
            if board.verify { "off" } else { "on" }
        ),
        complete: true,
    });
    items.push(KatbanControlItem {
        title: format!(
            "Auto-review pass — {}",
            if board.auto_review { "on" } else { "off" }
        ),
        subtitle: "a second agent reviews each card's diff".into(),
        command: format!(
            "/katban board auto-review {}",
            if board.auto_review { "off" } else { "on" }
        ),
        complete: true,
    });
    items.push(KatbanControlItem {
        title: format!("Attempts — {}", board.attempts),
        subtitle: format!(
            "rungs per card (1-{}); the first verify pass wins",
            clawde_katban::board::MAX_ATTEMPTS
        ),
        command: "/katban board attempts ".into(),
        complete: false,
    });
    items.push(KatbanControlItem {
        title: format!(
            "Attempt upstreams — {}",
            if board.attempt_upstreams.is_empty() {
                "auto".to_string()
            } else {
                board.attempt_upstreams.join(",")
            }
        ),
        subtitle: "pin the rotation; empty derives it from the free catalog".into(),
        command: "/katban board attempts-upstreams ".into(),
        complete: false,
    });
    items.push(KatbanControlItem {
        title: format!("Card runtime — {}", board.runtime.as_str()),
        subtitle: "host worktree, or a Docker container per attempt".into(),
        command: format!(
            "/katban board runtime {}",
            if board.runtime == clawde_katban::board::ContainerRuntime::Docker {
                "host"
            } else {
                "docker"
            }
        ),
        complete: true,
    });

    // ---- Admin credential + the buckets it locks --------------------------
    items.push(section("Admin"));
    items.push(KatbanControlItem {
        title: "Set or rotate admin password".into(),
        subtitle: if admin.is_configured() {
            "strong admin credential · resets admin lockouts".into()
        } else {
            "required for writes to an exposed board".into()
        },
        command: "/katban board password ".into(),
        complete: false,
    });

    // `active_lockouts` is the same source `/katban status` reads, and
    // `describe_lockouts` names the bucket an operator must pass to `unblock`:
    // a loopback client with no `X-Forwarded-For` is filed under `loopback`,
    // which is not an address and cannot be guessed from one.
    let active = admin.active_lockouts(now);
    if !active.is_empty() {
        items.push(section("Locked buckets"));
        for (bucket, blocked, until) in &active {
            items.push(KatbanControlItem {
                title: format!("Unblock {bucket}"),
                subtitle: describe_lockouts(&[(bucket.clone(), *blocked, *until)], now),
                command: format!("/katban board unblock {bucket}"),
                complete: true,
            });
        }
    }

    items.push(section("Projects & sites"));
    items.push(KatbanControlItem {
        title: "List projects".into(),
        subtitle: "board → git repo registry".into(),
        command: "/katban project list".into(),
        complete: true,
    });
    items.push(KatbanControlItem {
        title: "List sites".into(),
        subtitle: "hosted dev sites".into(),
        command: "/katban site list".into(),
        complete: true,
    });

    items
}

impl KatbanControlsState {
    /// Open the menu, rebuilding rows from the live store.
    pub fn open(&mut self) {
        self.items = build_control_items();
        self.selected = 1; // first action row, skipping the "Status" header
        self.scroll = 0;
        self.visible = true;
    }

    pub fn close(&mut self) {
        self.visible = false;
    }

    pub fn selected_item(&self) -> Option<&KatbanControlItem> {
        self.items.get(self.selected)
    }

    /// Move the selection up (wrapping). Skips section headers (empty
    /// command rows).
    pub fn select_prev(&mut self) {
        self.move_selection(-1);
    }

    /// Move the selection down (wrapping). Skips section headers.
    pub fn select_next(&mut self) {
        self.move_selection(1);
    }

    fn move_selection(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let n = self.items.len();
        for _ in 0..n {
            let next = (self.selected as isize + delta).rem_euclid(n as isize) as usize;
            self.selected = next;
            if !self.items[next].command.is_empty() {
                break;
            }
        }
        self.clamp_scroll();
    }

    /// Select the first actionable row and scroll to the top (Home).
    pub fn scroll_to_top(&mut self) {
        if let Some(index) = self.items.iter().position(|item| !item.command.is_empty()) {
            self.selected = index;
        }
        self.scroll = 0;
    }

    /// Select the last actionable row and scroll it into view (End).
    pub fn scroll_to_bottom(&mut self) {
        if let Some(index) = self.items.iter().rposition(|item| !item.command.is_empty()) {
            self.selected = index;
        }
        self.clamp_scroll();
    }

    /// Keep the selection inside a fixed scroll window so it stays
    /// visible in any viewport at least SCROLL_WINDOW rows tall.
    fn clamp_scroll(&mut self) {
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + SCROLL_WINDOW {
            self.scroll = self.selected + 1 - SCROLL_WINDOW;
        }
    }

    pub fn page_up(&mut self) {
        for _ in 0..10 {
            self.select_prev();
        }
    }

    pub fn page_down(&mut self) {
        for _ in 0..10 {
            self.select_next();
        }
    }
}

/// Render the Katban controls popup centered over the chat area.
pub fn render_katban_controls(frame: &mut Frame, state: &KatbanControlsState) {
    if !state.visible {
        return;
    }
    let area = frame.area();
    // The shared geometry helpers, so the popup's frame matches every other
    // modal (the dialog_select picker is the closest sibling).
    render_dark_overlay(frame, area);
    let width = MAX_WIDTH.min(area.width.saturating_sub(4));
    let height = (state.items.len() as u16 + 4)
        .min(MAX_HEIGHT)
        .min(area.height.saturating_sub(2));
    let rect = centered_rect(width, height, area);
    render_dialog_bg(frame, rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(CLAWDE_PANEL_BORDER))
        .title(Span::styled(
            " Katban controls ",
            Style::default().fg(CLAWDE_ACCENT),
        ));
    let buf = frame.buffer_mut();
    block.render(rect, buf);

    let inner = Rect {
        x: rect.x + 1,
        y: rect.y + 1,
        width: rect.width.saturating_sub(2),
        height: rect.height.saturating_sub(2),
    };

    // Footer hint. The keys themselves resolve through the keybinding table
    // (`KeyContext::KatbanControls`), so this is a reminder of the defaults.
    if inner.height > 0 {
        let hint = Line::from(Span::styled(
            "↑↓ move · Enter run · Esc close · PgUp/PgDn scroll",
            Style::default().fg(CLAWDE_MUTED),
        ));
        buf.set_line(inner.x, inner.y, &hint, inner.width);
    }
    let visible = inner.height.saturating_sub(1) as usize;
    let bottom = rect.y + rect.height - 1;
    let mut row_y = inner.y + 1;
    for idx in state.scroll..state.items.len() {
        if row_y >= bottom || row_y.saturating_sub(inner.y) >= visible as u16 {
            break;
        }
        let item = &state.items[idx];
        if item.command.is_empty() {
            // Section header.
            let line = Line::from(Span::styled(
                format!(" {} ", item.title),
                Style::default()
                    .fg(CLAWDE_MUTED)
                    .bg(CLAWDE_PANEL_BG)
                    .add_modifier(Modifier::BOLD),
            ));
            buf.set_line(inner.x, row_y, &line, inner.width);
            row_y += 1;
            continue;
        }
        let selected = idx == state.selected;
        let bg = if selected {
            CLAWDE_ACCENT
        } else {
            CLAWDE_PANEL_BG
        };
        let fg = if selected {
            CLAWDE_PANEL_BG
        } else {
            CLAWDE_TEXT
        };
        let title = format!(
            "{} {:<width$}",
            if selected { "▸" } else { " " },
            item.title,
            width = inner.width.saturating_sub(2) as usize
        );
        let line = Line::from(Span::styled(title, Style::default().fg(fg).bg(bg)));
        buf.set_line(inner.x, row_y, &line, inner.width);
        row_y += 1;
        if selected && !item.subtitle.is_empty() && row_y < bottom {
            let sub = Line::from(Span::styled(
                format!("  {}", item.subtitle),
                Style::default().fg(CLAWDE_MUTED).bg(bg),
            ));
            buf.set_line(inner.x, row_y, &sub, inner.width);
            row_y += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawde_katban::board::{save_board, Board, CardStatus, ContainerRuntime};

    /// Build the menu rows against a scratch home, with a board and an admin
    /// store the caller sets up. Serializes on the crate's test env lock: the
    /// builder reads `CLAWDE_HOME`-rooted stores.
    fn items_with(
        setup: impl FnOnce(&mut Board),
        admin: impl FnOnce(&mut admin_auth::AdminStore),
    ) -> Vec<KatbanControlItem> {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("CLAWDE_HOME").ok();
        std::env::set_var("CLAWDE_HOME", tmp.path());
        let mut board = Board::new();
        setup(&mut board);
        save_board(&board, "default").unwrap();
        let mut store = admin_auth::AdminStore::default();
        admin(&mut store);
        admin_auth::save(&store).unwrap();
        let items = build_control_items();
        match previous {
            Some(value) => std::env::set_var("CLAWDE_HOME", value),
            None => std::env::remove_var("CLAWDE_HOME"),
        }
        items
    }

    fn command_of<'a>(items: &'a [KatbanControlItem], title_prefix: &str) -> &'a str {
        items
            .iter()
            .find(|item| item.title.starts_with(title_prefix))
            .unwrap_or_else(|| panic!("no row starting with {title_prefix:?}"))
            .command
            .as_str()
    }

    /// Regression: a reviewed card's `next()` status is `done`, and `done`
    /// means *discard* — `board card set <id> done` deletes the card's pinned
    /// branch. The menu rows submit on a single Enter, so the old generic
    /// advance row threw the agent's committed work away with no confirmation
    /// (the board web UI had already removed the same control). A reviewed
    /// card must offer merge instead, seeded so the user reads it first.
    #[test]
    fn reviewed_card_offers_merge_not_a_one_enter_discard() {
        let items = items_with(
            |board| {
                let id = board.add_card("ship the widget");
                board.set_status(&id, CardStatus::Review);
            },
            |_| {},
        );

        assert!(
            !items.iter().any(|item| item.command.ends_with(" done")),
            "no row may discard a reviewed card on one Enter: {:?}",
            items.iter().map(|item| &item.command).collect::<Vec<_>>()
        );
        assert!(
            !items
                .iter()
                .any(|item| item.title.starts_with("Advance — ")),
            "a reviewed card has no generic advance"
        );

        let merge = items
            .iter()
            .find(|item| item.title.starts_with("Merge — "))
            .expect("a reviewed card offers a merge row");
        assert!(
            merge.command.starts_with("/katban board card merge "),
            "command: {}",
            merge.command
        );
        assert!(
            !merge.complete,
            "merge seeds the command so the user can read it before it runs"
        );
    }

    /// The guard above must not have disabled advancing ordinary cards.
    #[test]
    fn non_review_cards_still_advance_with_one_enter() {
        let items = items_with(
            |board| {
                board.add_card("build the landing page");
            },
            |_| {},
        );
        let advance = items
            .iter()
            .find(|item| item.title.starts_with("Advance — "))
            .expect("a backlog card offers an advance row");
        assert!(advance.command.ends_with(" queued"), "{}", advance.command);
        assert!(advance.complete);
    }

    /// A review card's review loop is reachable: feedback and inspect rows ride
    /// alongside merge, and both name the card id so they are complete.
    #[test]
    fn review_cards_offer_feedback_and_inspect_rows() {
        let items = items_with(
            |board| {
                let id = board.add_card("polish the widget");
                board.set_status(&id, CardStatus::Review);
            },
            |_| {},
        );
        assert!(command_of(&items, "Send review feedback — ")
            .starts_with("/katban board card feedback "));
        assert!(command_of(&items, "Inspect — ").starts_with("/katban board card show "));
    }

    /// The board settings rows are read live: a row's title states the value
    /// the board holds now, and its command flips exactly that value.
    #[test]
    fn board_setting_rows_state_live_values_and_toggle_them() {
        let items = items_with(
            |board| {
                board.verify = false;
                board.auto_review = false;
                board.attempts = 3;
                board.attempt_upstreams = vec!["nvidia".into(), "zai".into()];
                board.runtime = ContainerRuntime::Docker;
            },
            |_| {},
        );

        assert_eq!(
            command_of(&items, "Verification gate"),
            "/katban board verify on"
        );
        assert_eq!(
            command_of(&items, "Auto-review pass"),
            "/katban board auto-review on"
        );
        assert_eq!(command_of(&items, "Attempts — "), "/katban board attempts ");
        assert_eq!(
            command_of(&items, "Attempt upstreams — "),
            "/katban board attempts-upstreams "
        );
        assert_eq!(
            command_of(&items, "Card runtime"),
            "/katban board runtime host"
        );
        assert!(
            items.iter().any(|item| item.title == "Attempts — 3"),
            "the attempts row names the board's current rung count"
        );
        assert!(
            items.iter().any(|item| item.title.contains("nvidia,zai")),
            "the upstreams row names the pinned rotation"
        );
    }

    /// A stored lockout is offered under the bucket name the `unblock` command
    /// takes. A loopback client is filed under `loopback` — not an address —
    /// so showing a raw key and letting `describe_lockouts` name it keeps the
    /// menu, `status`, and the unblock miss message in one vocabulary.
    #[test]
    fn locked_buckets_offer_the_bucket_name_unblock_takes() {
        let items = items_with(
            |_| {},
            |store| {
                store.failed_attempts.insert(
                    "loopback".to_string(),
                    admin_auth::AdminFailedAttempt {
                        count: 0,
                        locked_until: None,
                        strikes: 3,
                        blocked_until: Some(now_secs() + 3600),
                    },
                );
            },
        );

        let row = items
            .iter()
            .find(|item| item.title.starts_with("Unblock "))
            .expect("a live lockout offers an unblock row");
        assert_eq!(row.command, "/katban board unblock loopback");
        assert!(
            row.subtitle.contains("loopback") && row.subtitle.contains("24h block"),
            "subtitle names the bucket and the block: {}",
            row.subtitle
        );
    }

    /// The project/site listings the `/katban` help promises are in the menu.
    #[test]
    fn project_and_site_listings_are_present() {
        let items = items_with(|_| {}, |_| {});
        assert_eq!(command_of(&items, "List projects"), "/katban project list");
        assert_eq!(command_of(&items, "List sites"), "/katban site list");
    }

    /// Home/End move to the first/last actionable row and never select a
    /// section header (whose empty command would do nothing on Enter).
    #[test]
    fn scroll_to_top_and_bottom_land_on_actionable_rows() {
        let mut state = KatbanControlsState {
            visible: true,
            items: vec![
                section("One"),
                KatbanControlItem {
                    title: "first".into(),
                    subtitle: String::new(),
                    command: "/a".into(),
                    complete: true,
                },
                section("Two"),
                KatbanControlItem {
                    title: "last".into(),
                    subtitle: String::new(),
                    command: "/b".into(),
                    complete: true,
                },
            ],
            selected: 0,
            scroll: 0,
        };
        state.scroll_to_bottom();
        assert_eq!(state.selected, 3);
        state.scroll_to_top();
        assert_eq!(state.selected, 1);
        state.scroll = 99;
        state.scroll_to_top();
        assert_eq!(state.scroll, 0);
    }
}
