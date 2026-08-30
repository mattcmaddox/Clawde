// katban_controls.rs — scrollable Katban controls menu.
//
// Opened by Alt+G (`openKatbanControls`, see app.rs): a compact popup listing
// the admin operations the user most often needs without leaving the TUI —
// status overview, Kanban board control (list / ready / add / per-card
// advance), guest-link management (list / create / rotate password /
// revoke), and unblocking locked or permanently-blocked IPs.
//
// The menu is built live from the guest store (`~/.clawde/katban/links.json`)
// each time it opens, so it always reflects the real state: one row per link
// (with its state and expiry), one row per locked/blocked IP, plus the
// fixed actions. Selecting a row seeds the prompt with the matching
// `/katban ...` command; rows whose command is complete (e.g. a specific
// link id or IP) are submitted immediately on Enter, while rows that need
// more input (e.g. a link name) just seed the prompt and let the user finish.
//
// This is the "living" surface: as Katban gains features, add a menu row that
// maps to the new `/katban` subcommand and it shows up here automatically.

use clawde_katban::guest;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Widget};
use ratatui::Frame;

use crate::overlays::{
    CLAWDE_ACCENT, CLAWDE_MUTED, CLAWDE_PANEL_BG, CLAWDE_PANEL_BORDER, CLAWDE_TEXT,
};

/// One row in the controls menu.
#[derive(Debug, Clone)]
pub struct KatbanControlItem {
    /// Primary label (e.g. "Rotate password — friends").
    pub title: String,
    /// Secondary, dimmer line (e.g. "expires never · 2 devices").
    pub subtitle: String,
    /// The `/katban ...` command this row runs.
    pub command: String,
    /// True when `command` is complete and should submit on Enter; false when
    /// the row only seeds the prompt for the user to finish (e.g. a name).
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

/// Max popup height (rows + chrome); longer menus scroll.
const MAX_HEIGHT: u16 = 24;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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

/// Section header helper — a row that is never selectable.
fn section(title: &str) -> KatbanControlItem {
    KatbanControlItem {
        title: format!("▸ {title}"),
        subtitle: String::new(),
        command: String::new(),
        complete: false,
    }
}

/// Build the menu from the live guest store. Empty (but still visible) when
/// the store cannot be read, so the dialog degrades gracefully.
pub fn build_control_items() -> Vec<KatbanControlItem> {
    let store = guest::load().unwrap_or_default();
    let now = now_secs();
    let mut items = vec![
        section("Status"),
        KatbanControlItem {
            title: "Katban overview".into(),
            subtitle: "sites, boards, guest links, caddy".into(),
            command: "/katban status".into(),
            complete: true,
        },
        section("Guest links"),
        KatbanControlItem {
            title: "List guest links".into(),
            subtitle: "ids, names, states, expiry".into(),
            command: "/katban link list".into(),
            complete: true,
        },
        KatbanControlItem {
            title: "Create a guest link".into(),
            subtitle: "prints a fresh password once".into(),
            command: "/katban link create ".into(),
            complete: false,
        },
    ];
    // Only live links get management rows: rotating the password of a
    // revoked/expired link succeeds silently and revoking it again is a
    // no-op, so offering those rows is a footgun. Dead links remain visible
    // via `/katban link list`.
    for link in store.links.iter().filter(|l| guest::link_active(l, now)) {
        let expiry = link
            .expires_at
            .map(|unix| format!("expires in {}d", unix.saturating_sub(now) / 86400))
            .unwrap_or_else(|| "never expires".to_string());
        let subtitle = format!("active · {expiry}");
        items.push(KatbanControlItem {
            title: format!("Rotate password — {}", link.name),
            subtitle: subtitle.clone(),
            command: format!("/katban link password {}", link.id),
            complete: true,
        });
        items.push(KatbanControlItem {
            title: format!("Revoke — {}", link.name),
            subtitle,
            command: format!("/katban link revoke {}", link.id),
            complete: true,
        });
    }

    // ---- Boards (cards + statuses) ------------------------------------------
    let board = clawde_katban::board::load_board("default")
        .ok()
        .flatten()
        .unwrap_or_default();
    items.push(section("Boards"));
    items.push(KatbanControlItem {
        title: "List cards".into(),
        subtitle: "default board".into(),
        command: "/katban board list".into(),
        complete: true,
    });
    items.push(KatbanControlItem {
        title: "Cards ready to run".into(),
        subtitle: "respects the parallel cap".into(),
        command: "/katban board ready".into(),
        complete: true,
    });
    items.push(KatbanControlItem {
        title: "Add a card".into(),
        subtitle: "prompt is the card's task".into(),
        command: "/katban board card add ".into(),
        complete: false,
    });
    items.push(KatbanControlItem {
        title: "Link cards".into(),
        subtitle: "one card waits on another (cycle-checked)".into(),
        command: "/katban board link ".into(),
        complete: false,
    });
    for card in &board.cards {
        if card.status == clawde_katban::board::CardStatus::Done {
            continue;
        }
        let status = board_status_name(card.status);
        let next = card.status.next().map(board_status_name).unwrap_or(status);
        let preview: String = card.prompt.chars().take(32).collect();
        items.push(KatbanControlItem {
            title: format!("Advance — {preview}"),
            subtitle: format!("{status} → {next}"),
            command: format!("/katban board card set {} {next}", card.id),
            complete: true,
        });
    }

    let blocked: Vec<(&String, &guest::FailedAttempt)> = store
        .failed_attempts
        .iter()
        .filter(|(_, attempt)| {
            attempt.permanently_blocked || attempt.locked_until.is_some_and(|u| u > now)
        })
        .collect();
    if !blocked.is_empty() {
        items.push(section("Locked IPs"));
        for (ip, attempt) in blocked {
            let subtitle = if attempt.permanently_blocked {
                "permanently blocked".to_string()
            } else {
                format!(
                    "locked {}s",
                    attempt.locked_until.unwrap_or(now).saturating_sub(now)
                )
            };
            items.push(KatbanControlItem {
                title: format!("Unblock {ip}"),
                subtitle,
                command: format!("/katban guest unblock {ip}"),
                complete: true,
            });
        }
    }

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
        // Keep the selection inside a fixed scroll window so it stays
        // visible in any viewport at least SCROLL_WINDOW rows tall.
        const SCROLL_WINDOW: usize = 18;
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
    let width = 64u16.min(area.width.saturating_sub(4));
    let height = (state.items.len() as u16 + 4)
        .min(MAX_HEIGHT)
        .min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let rect = Rect {
        x,
        y,
        width,
        height,
    };

    let buf = frame.buffer_mut();
    // Popup background + border.
    for row in rect.y..rect.y + rect.height {
        for col in rect.x..rect.x + rect.width {
            if let Some(cell) = buf.cell_mut((col, row)) {
                cell.set_char(' ');
                cell.set_bg(CLAWDE_PANEL_BG);
                cell.set_fg(CLAWDE_TEXT);
            }
        }
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(CLAWDE_PANEL_BORDER))
        .title(Span::styled(
            " Katban controls ",
            Style::default().fg(CLAWDE_ACCENT),
        ));
    block.render(rect, buf);

    let inner = Rect {
        x: rect.x + 1,
        y: rect.y + 1,
        width: rect.width.saturating_sub(2),
        height: rect.height.saturating_sub(2),
    };

    // Footer hint.
    if inner.height > 0 {
        let hint = Line::from(Span::styled(
            "↑↓ move · Enter run · Esc close",
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
