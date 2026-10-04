// models_menu.rs — the `/models` menu.
//
// One configurable surface for the routing targets: an "Auto" row (route
// across every enabled provider) followed by one row per free-catalog
// upstream. Each upstream row carries a global on/off toggle — "don't use
// Groq" applies everywhere — and its stored-key count. Selecting Auto returns
// to routing across all enabled providers; selecting an upstream pins it.
//
// The Alt+J/K free-model popup (`free_model_popup.rs`) stays the one-keystroke
// model quick pick; this menu owns the provider on/off choice and the
// Auto-vs-pinned target. `/model <name>` keeps its own picker for now.
//
// Layout:
//   ┌─ /models — providers ──────────────────────────────── esc ┐
//   │  ▸ auto   route across 3 enabled providers                 │
//   │    [x] Groq              2 keys                            │
//   │    [ ] Cerebras          0 keys                            │
//   │    ...                                                     │
//   │  ↑/↓ j/k provider · space toggle · enter select · esc close│
//   └────────────────────────────────────────────────────────────┘
//
// Like the other static dialogs this never animates, so it is deliberately
// NOT part of `App::needs_fast_repaint()`.

use ratatui::layout::Rect;
use ratatui::prelude::Stylize;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use std::cell::Cell;

use crate::overlays::{
    centered_rect, render_dark_overlay, render_dialog_bg, CLAWDE_ACCENT, CLAWDE_PANEL_BG,
};

/// Number of rows shown at once before scrolling.
pub const VISIBLE_ROWS: usize = 12;

/// One row in the `/models` menu.
#[derive(Debug, Clone)]
pub struct ModelsRow {
    /// Provider id, or `"free"` for the Auto row.
    pub id: String,
    /// Display name.
    pub title: String,
    /// Whether the provider is enabled in the free chain (Auto is always on).
    pub enabled: bool,
    /// The Auto row (always first, never toggled).
    pub is_auto: bool,
    /// Stored keys for this upstream (0 for Auto).
    pub key_count: usize,
}

/// State for the `/models` menu overlay.
#[derive(Debug)]
pub struct ModelsMenuState {
    pub visible: bool,
    /// Area used by the overlay in the last render (click-outside detection).
    pub last_rect: Cell<Rect>,
    pub rows: Vec<ModelsRow>,
    pub active_idx: usize,
    pub scroll_offset: usize,
}

impl Default for ModelsMenuState {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelsMenuState {
    pub fn new() -> Self {
        Self {
            visible: false,
            last_rect: Cell::new(Rect::default()),
            rows: Vec::new(),
            active_idx: 0,
            scroll_offset: 0,
        }
    }

    /// Open the menu with a freshly built row list, cursor on the first row.
    pub fn open(&mut self, rows: Vec<ModelsRow>) {
        self.rows = rows;
        self.active_idx = 0;
        self.scroll_offset = 0;
        self.visible = true;
    }

    /// Replace the rows after a toggle, preserving the cursor position.
    pub fn set_rows(&mut self, rows: Vec<ModelsRow>) {
        self.rows = rows;
        if self.active_idx >= self.rows.len() {
            self.active_idx = self.rows.len().saturating_sub(1);
        }
        self.clamp_scroll();
    }

    pub fn close(&mut self) {
        self.visible = false;
    }

    /// The row under the cursor.
    pub fn current(&self) -> Option<&ModelsRow> {
        self.rows.get(self.active_idx)
    }

    pub fn select_next(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        self.active_idx = (self.active_idx + 1) % self.rows.len();
        self.clamp_scroll();
    }

    pub fn select_prev(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        self.active_idx = (self.active_idx + self.rows.len() - 1) % self.rows.len();
        self.clamp_scroll();
    }

    /// Flip the cursor row's on/off, returning its id (Auto is never toggled).
    /// The caller persists the change and calls `set_rows` with rebuilt rows.
    pub fn toggle_current(&mut self) -> Option<String> {
        let row = self.rows.get_mut(self.active_idx)?;
        if row.is_auto {
            return None;
        }
        row.enabled = !row.enabled;
        Some(row.id.clone())
    }

    fn clamp_scroll(&mut self) {
        if self.active_idx < self.scroll_offset {
            self.scroll_offset = self.active_idx;
        } else if self.active_idx >= self.scroll_offset + VISIBLE_ROWS {
            self.scroll_offset = self.active_idx + 1 - VISIBLE_ROWS;
        }
    }
}

/// Render the `/models` menu overlay.
pub fn render_models_menu(frame: &mut Frame, state: &ModelsMenuState, area: Rect) {
    if !state.visible {
        return;
    }
    let pink = CLAWDE_ACCENT;
    let dim = Color::Rgb(90, 90, 90);
    let muted = Color::Rgb(180, 180, 180);
    let dialog_bg = CLAWDE_PANEL_BG;

    render_dark_overlay(frame, area);

    let width = 72u16.min(area.width.saturating_sub(4));
    let height = (state.rows.len().min(VISIBLE_ROWS) as u16 + 6).min(area.height.saturating_sub(2));
    let dialog_area = centered_rect(width, height, area);
    state.last_rect.set(dialog_area);
    render_dialog_bg(frame, dialog_area);

    let inner = Rect {
        x: dialog_area.x + 1,
        y: dialog_area.y + 1,
        width: dialog_area.width.saturating_sub(2),
        height: dialog_area.height.saturating_sub(2),
    };

    let enabled = state
        .rows
        .iter()
        .filter(|r| !r.is_auto && r.enabled)
        .count();
    let total = state.rows.iter().filter(|r| !r.is_auto).count();

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            " /models \u{2014} providers",
            Style::default().fg(pink).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("   {}/{} enabled", enabled, total),
            Style::default().fg(dim),
        ),
    ]));
    lines.push(Line::styled(
        "  space toggles a provider on/off; enter selects the routing target",
        Style::default().fg(dim),
    ));
    lines.push(Line::styled("", Style::default()));

    for (i, row) in state
        .rows
        .iter()
        .enumerate()
        .skip(state.scroll_offset)
        .take(VISIBLE_ROWS)
    {
        let active = i == state.active_idx;
        let marker = if active { "\u{25b8}" } else { " " };
        let name_style = if active {
            Style::default().fg(pink).add_modifier(Modifier::BOLD)
        } else if row.enabled || row.is_auto {
            Style::default().fg(muted)
        } else {
            Style::default().fg(dim)
        };
        let toggle = if row.is_auto {
            "auto".to_string()
        } else if row.enabled {
            "[x]".to_string()
        } else {
            "[ ]".to_string()
        };
        let toggle_style = if row.is_auto {
            Style::default().fg(pink)
        } else if row.enabled {
            Style::default().fg(Color::Rgb(120, 210, 150))
        } else {
            Style::default().fg(dim)
        };
        let tail = if row.is_auto {
            format!(
                "route across {} enabled provider{}",
                enabled,
                if enabled == 1 { "" } else { "s" }
            )
        } else {
            format!(
                "{} key{}",
                row.key_count,
                if row.key_count == 1 { "" } else { "s" }
            )
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {} ", marker),
                Style::default().fg(if active { pink } else { dim }),
            ),
            Span::styled(format!("{:<5}", toggle), toggle_style),
            Span::styled(format!("{:<22}", row.title), name_style),
            Span::styled(tail, Style::default().fg(dim)),
        ]));
    }

    lines.push(Line::styled("", Style::default()));
    lines.push(Line::styled(
        "  \u{2191}/\u{2193} j/k provider \u{00b7} space toggle \u{00b7} enter select \u{00b7} esc close",
        Style::default().fg(dim),
    ));

    let para = Paragraph::new(lines).bg(dialog_bg);
    frame.render_widget(para, inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<ModelsRow> {
        vec![
            ModelsRow {
                id: "free".into(),
                title: "auto".into(),
                enabled: true,
                is_auto: true,
                key_count: 0,
            },
            ModelsRow {
                id: "groq".into(),
                title: "Groq".into(),
                enabled: true,
                is_auto: false,
                key_count: 2,
            },
            ModelsRow {
                id: "cerebras".into(),
                title: "Cerebras".into(),
                enabled: false,
                is_auto: false,
                key_count: 0,
            },
        ]
    }

    #[test]
    fn defaults_hidden_with_no_rows() {
        let s = ModelsMenuState::new();
        assert!(!s.visible);
        assert!(s.rows.is_empty());
    }

    #[test]
    fn open_shows_rows_and_puts_cursor_on_auto() {
        let mut s = ModelsMenuState::new();
        s.open(rows());
        assert!(s.visible);
        assert_eq!(s.active_idx, 0);
        assert_eq!(s.current().map(|r| r.id.as_str()), Some("free"));
    }

    #[test]
    fn navigation_wraps() {
        let mut s = ModelsMenuState::new();
        s.open(rows());
        s.select_next();
        assert_eq!(s.current().map(|r| r.id.as_str()), Some("groq"));
        s.select_prev();
        assert_eq!(s.current().map(|r| r.id.as_str()), Some("free"));
        s.select_prev();
        assert_eq!(
            s.current().map(|r| r.id.as_str()),
            Some("cerebras"),
            "wraps"
        );
    }

    #[test]
    fn toggle_flips_a_provider_but_never_auto() {
        let mut s = ModelsMenuState::new();
        s.open(rows());
        assert_eq!(s.toggle_current(), None, "Auto is not toggleable");
        assert!(s.rows[0].enabled, "Auto stays enabled");
        s.select_next();
        assert_eq!(s.toggle_current().as_deref(), Some("groq"));
        assert!(!s.rows[1].enabled);
        s.select_next();
        assert_eq!(s.toggle_current().as_deref(), Some("cerebras"));
        assert!(s.rows[2].enabled);
    }

    #[test]
    fn set_rows_preserves_the_cursor() {
        let mut s = ModelsMenuState::new();
        s.open(rows());
        s.select_next();
        s.select_next();
        s.set_rows(rows());
        assert_eq!(s.current().map(|r| r.id.as_str()), Some("cerebras"));
    }

    #[test]
    fn render_does_not_panic() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        let mut s = ModelsMenuState::new();
        s.open(rows());
        terminal
            .draw(|f| render_models_menu(f, &s, f.area()))
            .unwrap();
    }
}
