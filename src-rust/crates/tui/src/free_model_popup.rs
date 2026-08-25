// free_model_popup.rs — compact dropdown of available free models.
//
// Opened by Alt+J/K (see `openFreeModelPicker` in app.rs): a small popup
// anchored just above the prompt input area listing "auto" plus every
// currently free model (model-first, grouped by family sections). Enter pins
// the selected entry via `set_model` (real switch, not a display label); Esc
// closes without changing anything.
//
// The item list is rebuilt each time the popup opens from
// `app.free_model_lists` / `app.free_model_defaults`, so it always reflects
// the live free chain (upstreams that actually have keys) and the discovered
// per-provider free model lists.
//
// Long lists scroll: selection stays within the visible viewport, and
// section-header rows are non-selectable.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Widget};
use ratatui::Frame;

use crate::overlays::{
    CLAWDE_ACCENT, CLAWDE_MUTED, CLAWDE_PANEL_BG, CLAWDE_PANEL_BORDER, CLAWDE_TEXT,
};

/// One selectable row (or, when `header`, a non-selectable section title).
pub struct FreeModelItem {
    /// Model id to set on Enter, e.g. `free/auto`, `free/family/<slug>` or
    /// `free/<provider>/<model>`. Empty for section headers.
    pub id: String,
    /// Primary row label (e.g. the model slug or section title).
    pub title: String,
    /// Secondary, dimmer line (e.g. hosting providers).
    pub subtitle: String,
    /// Section header row — rendered dimmed, skipped by selection.
    pub header: bool,
}

/// State for the free-model dropdown.
#[derive(Default)]
pub struct FreeModelPopupState {
    pub visible: bool,
    pub items: Vec<FreeModelItem>,
    pub selected: usize,
    /// Index of the first item visible in the viewport (scroll position).
    pub scroll: usize,
}

/// Max popup height (rows of items + chrome). Longer lists scroll.
const MAX_HEIGHT: u16 = 22;

impl FreeModelPopupState {
    /// Open the popup with `items`, selecting the entry whose id matches
    /// `current` (falls back to the first row, which is always "auto").
    /// Selection never lands on a section header.
    pub fn open(&mut self, items: Vec<FreeModelItem>, current: &str) {
        let mut selected = items.iter().position(|i| i.id == current).unwrap_or(0);
        while selected < items.len() && items[selected].header {
            selected += 1;
        }
        self.selected = selected.min(items.len().saturating_sub(1));
        // Leave a little context above the selection when opening on a deep
        // row.
        self.scroll = self.selected.saturating_sub(4);
        self.items = items;
        self.visible = true;
    }

    pub fn close(&mut self) {
        self.visible = false;
    }

    /// Move the selection up, wrapping around (row order is short, so wrap is
    /// harmless and matches every other picker in the TUI). Skips section
    /// headers.
    pub fn select_prev(&mut self) {
        self.move_selection(-1);
    }

    /// Move the selection down, wrapping around. Skips section headers.
    pub fn select_next(&mut self) {
        self.move_selection(1);
    }

    /// Max rows between `scroll` and the selection; the renderer's viewport
    /// is at least this tall for any usable terminal, so the selection stays
    /// visible without the renderer needing mutable access to clamp it.
    const SCROLL_WINDOW: usize = 14;

    fn move_selection(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let n = self.items.len();
        for _ in 0..n {
            let next = (self.selected as isize + delta).rem_euclid(n as isize) as usize;
            self.selected = next;
            if !self.items[next].header {
                break;
            }
        }
        // Keep the selection inside a fixed window of the scroll position so
        // it stays visible in any viewport at least SCROLL_WINDOW rows tall.
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + Self::SCROLL_WINDOW {
            self.scroll = self.selected + 1 - Self::SCROLL_WINDOW;
        }
    }

    /// The currently selected item, if any.
    pub fn selected(&self) -> Option<&FreeModelItem> {
        self.items.get(self.selected).filter(|i| !i.header)
    }
}

/// Render the popup anchored just above `input_area`, left-aligned with the
/// prompt text (matching the input's own 2-column indent).
pub fn render_free_model_popup(
    frame: &mut Frame,
    state: &FreeModelPopupState,
    input_area: Rect,
    inspector: Option<&clawde_api::providers::effort_shaping::ThinkingInspection>,
) {
    if !state.visible || state.items.is_empty() {
        return;
    }

    let width = 72u16.min(input_area.width.saturating_sub(4));
    // Title row + blank + header line + footer + one row per item, clamped
    // to a reasonable max so long model-first lists scroll instead of
    // covering the whole screen.
    let height = (state.items.len() as u16 + 5)
        .min(MAX_HEIGHT)
        .min(input_area.y.max(4));
    let x = input_area.x + 2;
    let y = input_area.y.saturating_sub(height);
    let area = Rect {
        x,
        y,
        width,
        height,
    };

    let buf = frame.buffer_mut();

    // Popup background + border.
    for row in area.y..area.y + area.height {
        for col in area.x..area.x + area.width {
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
            " Free models ",
            Style::default().fg(CLAWDE_ACCENT),
        ));
    block.render(area, buf);

    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };

    // Header line.
    if inner.height > 0 {
        let header = Line::from(vec![
            Span::styled("Model", Style::default().fg(CLAWDE_MUTED)),
            Span::styled(
                format!(
                    "{:>width$}",
                    "↑↓ move · Enter select · Esc close",
                    width = inner.width.saturating_sub(6) as usize
                ),
                Style::default().fg(CLAWDE_MUTED),
            ),
        ]);
        buf.set_line(inner.x, inner.y, &header, inner.width);
    }

    // Viewport window: render the slice of items starting at `scroll`.
    let visible = inner.height.saturating_sub(1) as usize;
    let row_start = inner.y + 1;
    for (offset, idx) in (state.scroll..state.items.len()).enumerate() {
        if offset >= visible {
            break;
        }
        let row_y = row_start + offset as u16;
        if row_y >= area.y + area.height - 1 {
            break;
        }
        let item = &state.items[idx];
        if item.header {
            let line = Line::from(Span::styled(
                format!(" {} ", item.title),
                Style::default()
                    .fg(CLAWDE_MUTED)
                    .bg(CLAWDE_PANEL_BG)
                    .add_modifier(ratatui::style::Modifier::BOLD),
            ));
            buf.set_line(inner.x, row_y, &line, inner.width);
            continue;
        }
        let selected = idx == state.selected;
        let bg = if selected {
            CLAWDE_ACCENT
        } else {
            CLAWDE_PANEL_BG
        };
        let fg = if selected {
            Color::Rgb(255, 255, 255)
        } else {
            CLAWDE_TEXT
        };
        let sub_fg = if selected {
            Color::Rgb(255, 200, 215)
        } else {
            CLAWDE_MUTED
        };
        let line = Line::from(vec![
            Span::styled(
                if selected { "▸ " } else { "  " },
                Style::default().fg(fg).bg(bg),
            ),
            Span::styled(item.title.clone(), Style::default().fg(fg).bg(bg)),
            Span::styled(
                format!("  {}", item.subtitle),
                Style::default().fg(sub_fg).bg(bg),
            ),
        ]);
        buf.set_line(inner.x, row_y, &line, inner.width);
    }

    // Inspector footer: one line showing the thinking wire param for the
    // selected model, or a hint when the selection is a section header.
    let footer_y = area.y + area.height.saturating_sub(2);
    if footer_y >= area.y && inner.height > 1 {
        let footer_text = if let Some(insp) = inspector {
            if let Some(ref wp) = insp.wire_param {
                format!(" → {wp}")
            } else {
                match insp.mode {
                    clawde_api::providers::effort_shaping::ThinkingMode::NotSupported => {
                        " → (no thinking knob)".to_string()
                    }
                    _ => String::new(),
                }
            }
        } else {
            " ↓ select to inspect".to_string()
        };
        let footer_fg = inspector
            .filter(|i| !i.warnings.is_empty())
            .map(|_| CLAWDE_ACCENT)
            .unwrap_or(CLAWDE_MUTED);
        let footer_line = Line::from(vec![Span::styled(
            footer_text,
            Style::default().fg(footer_fg),
        )]);
        buf.set_line(inner.x, footer_y, &footer_line, inner.width);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, title: &str) -> FreeModelItem {
        FreeModelItem {
            id: id.to_string(),
            title: title.to_string(),
            subtitle: String::new(),
            header: false,
        }
    }

    fn header(title: &str) -> FreeModelItem {
        FreeModelItem {
            id: String::new(),
            title: title.to_string(),
            subtitle: String::new(),
            header: true,
        }
    }

    #[test]
    fn open_selects_matching_current() {
        let mut state = FreeModelPopupState::default();
        let items = vec![item("free/auto", "Auto"), item("groq/gpt-oss-120b", "Groq")];
        state.open(items, "groq/gpt-oss-120b");
        assert!(state.visible);
        assert_eq!(
            state.selected().map(|i| i.id.as_str()),
            Some("groq/gpt-oss-120b")
        );
    }

    #[test]
    fn open_never_lands_on_section_header() {
        let mut state = FreeModelPopupState::default();
        // Current matches nothing → falls back to the first row, which is a
        // header; selection must skip it and land on the first model row.
        let items = vec![
            header("gpt-oss-120b"),
            item("free/groq/gpt-oss-120b", "gpt-oss-120b"),
        ];
        state.open(items, "does-not-exist");
        assert_eq!(state.selected, 1);
        assert_eq!(
            state.selected().map(|i| i.id.as_str()),
            Some("free/groq/gpt-oss-120b")
        );
    }

    #[test]
    fn selection_skips_section_headers() {
        let mut state = FreeModelPopupState::default();
        let items = vec![
            item("free/auto", "Auto"),
            header("gpt-oss-120b"),
            item("free/groq/gpt-oss-120b", "gpt-oss-120b"),
            header("other"),
            item("free/groq/deepseek-v4-flash", "deepseek-v4-flash"),
        ];
        state.open(items, "free/auto");
        // Auto → next lands on gpt-oss-120b (skips its header), then
        // deepseek-v4-flash (skips its header).
        state.select_next();
        assert_eq!(state.selected, 2);
        state.select_next();
        assert_eq!(state.selected, 4);
        // Wrap: from the last row back to auto (skipping both headers).
        state.select_next();
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn scrolling_keeps_selection_visible() {
        let mut state = FreeModelPopupState::default();
        let items: Vec<FreeModelItem> = (0..40)
            .map(|i| item(&format!("free/groq/model-{}", i), &format!("model-{}", i)))
            .collect();
        state.open(items, "free/groq/model-0");
        // Jump far down: scroll must follow so the selection is in view.
        for _ in 0..35 {
            state.select_next();
        }
        assert_eq!(state.selected, 35);
        assert!(state.selected >= state.scroll);
        assert!(state.selected - state.scroll < 20);
        // Jump back up: scroll follows up too.
        for _ in 0..30 {
            state.select_prev();
        }
        assert_eq!(state.selected, 5);
        assert!(state.selected >= state.scroll);
    }

    #[test]
    fn open_positions_scroll_with_context_above_selected() {
        let mut state = FreeModelPopupState::default();
        let items: Vec<FreeModelItem> = (0..30)
            .map(|i| item(&format!("free/groq/model-{}", i), &format!("model-{}", i)))
            .collect();
        state.open(items, "free/groq/model-25");
        assert_eq!(state.selected, 25);
        // A few rows of context above the deep selection, and the selection
        // stays within the scroll window.
        assert_eq!(state.scroll, 21);
        assert!(state.selected - state.scroll < FreeModelPopupState::SCROLL_WINDOW);
    }
}
