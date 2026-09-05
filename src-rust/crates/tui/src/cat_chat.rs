// cat_chat.rs — the Cat Chat guest-link manager popup.
//
// Opened by `/chat` in the TUI: a compact popup for managing the guest chat
// ("Cat Chat") links without leaving the conversation. Lists every guest link
// from the live store (`~/.clawde/katban/links.json`) with its state, expiry,
// and device count, plus fixed rows to generate a new link (which prints a
// fresh password once) and to list full details.
//
// Selecting a row seeds the prompt with the matching `/chat ...` command;
// complete rows (revoke/rotate with a specific link id) submit immediately on
// Enter, while rows needing more input (a new link's name) just seed the
// prompt and let the user finish.
//
// This is the chat-focused sibling of the Alt+G Katban controls menu: that
// menu stays Kanban-centric, this one owns the chat links.

use clawde_katban::guest;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Widget};
use ratatui::Frame;

use crate::overlays::{
    CLAWDE_ACCENT, CLAWDE_MUTED, CLAWDE_PANEL_BG, CLAWDE_PANEL_BORDER, CLAWDE_TEXT,
};

/// One row in the Cat Chat popup.
#[derive(Debug, Clone)]
pub struct CatChatItem {
    /// Primary label (e.g. "Revoke — friends").
    pub title: String,
    /// Secondary, dimmer line (e.g. "active · never expires · 2 devices").
    pub subtitle: String,
    /// The `/chat ...` command this row runs.
    pub command: String,
    /// True when `command` is complete and should submit on Enter; false when
    /// the row only seeds the prompt for the user to finish (e.g. a name).
    pub complete: bool,
}

/// State for the Cat Chat popup.
#[derive(Default)]
pub struct CatChatState {
    pub visible: bool,
    pub items: Vec<CatChatItem>,
    pub selected: usize,
    /// First visible item index (scroll position).
    pub scroll: usize,
}

/// Max popup height (rows + chrome); longer lists scroll.
const MAX_HEIGHT: u16 = 22;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Section header helper — a row that is never selectable.
fn section(title: &str) -> CatChatItem {
    CatChatItem {
        title: format!("▸ {title}"),
        subtitle: String::new(),
        command: String::new(),
        complete: false,
    }
}

/// Build the popup rows from the live guest store. Always includes the fixed
/// action rows so the popup is never empty, even on a cold store.
pub fn build_cat_chat_items() -> Vec<CatChatItem> {
    let store = guest::load().unwrap_or_default();
    let now = now_secs();
    let mut items = vec![
        section("Guest links"),
        CatChatItem {
            title: "Generate a new link + password".into(),
            subtitle: "prints the password once — /chat create".into(),
            command: "/chat create ".into(),
            complete: false,
        },
        CatChatItem {
            title: "List all links".into(),
            subtitle: "ids, names, states, expiry".into(),
            command: "/chat links".into(),
            complete: true,
        },
    ];
    // Every link (including revoked/expired ones) gets a detail row; only
    // live links get the destructive rotate/revoke rows — rotating or
    // revoking a dead link succeeds silently and is a footgun. Dead links
    // remain visible here so they can still be inspected.
    for link in &store.links {
        let state = if link.revoked {
            "revoked"
        } else if link.expires_at.is_some_and(|expiry| expiry <= now) {
            "expired"
        } else {
            "active"
        };
        let expiry = match link.expires_at {
            Some(unix) => format!("expires in {}d", unix.saturating_sub(now) / 86400),
            None => "never expires".to_string(),
        };
        let devices = store.devices.get(&link.id).map(|d| d.len()).unwrap_or(0);
        let live = guest::link_active(link, now);
        items.push(CatChatItem {
            title: format!("{} — {}", link.name, link.id),
            subtitle: format!("{state} · {expiry} · {devices} devices"),
            command: format!("/chat show {}", link.id),
            complete: true,
        });
        if live {
            items.push(CatChatItem {
                title: format!("New random password — {}", link.name),
                subtitle: "strong 12-char, printed once · old password stops working".into(),
                command: format!("/chat password {}", link.id),
                complete: true,
            });
            items.push(CatChatItem {
                title: format!("Set your own password — {}", link.name),
                subtitle: "the word friends must remember — type it after this".into(),
                command: format!("/chat password {} --set ", link.id),
                complete: false,
            });
            items.push(CatChatItem {
                title: format!("Delete link — {}", link.name),
                subtitle: "revoke: all its devices stop chatting immediately".into(),
                command: format!("/chat revoke {}", link.id),
                complete: true,
            });
        }
    }
    items
}

impl CatChatState {
    /// Open the popup, rebuilding rows from the live store.
    pub fn open(&mut self) {
        self.items = build_cat_chat_items();
        self.selected = 1; // first action row, skipping the section header
        self.scroll = 0;
        self.visible = true;
    }

    pub fn close(&mut self) {
        self.visible = false;
    }

    pub fn selected_item(&self) -> Option<&CatChatItem> {
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
        const SCROLL_WINDOW: usize = 16;
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

/// Render the Cat Chat popup centered over the chat area.
pub fn render_cat_chat(frame: &mut Frame, state: &CatChatState) {
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
            " 🐾 Cat Chat — guest links ",
            Style::default()
                .fg(CLAWDE_ACCENT)
                .add_modifier(Modifier::BOLD),
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
