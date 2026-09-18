//! Session browser overlay (/session, /resume, /rename, /export).
//! Mirrors TS session management in REPL.tsx

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::overlays::{centered_rect, modal_search_line_with_insert, CLAWDE_MUTED};
use crate::vim_search::VimSearch;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The interaction mode of the session browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionBrowserMode {
    /// Default: list sessions, navigate with arrow keys.
    Browse,
    /// User is typing a new name for the selected session.
    Rename,
    /// Waiting for the user to confirm a destructive action (delete / export).
    Confirm,
}

/// A single session entry shown in the browser list.
#[derive(Debug, Clone)]
pub struct SessionEntry {
    pub id: String,
    pub title: String,
    /// Concatenated user/assistant text used by the browser's full-text search.
    /// Kept out of rendering so message content does not widen the table.
    pub searchable_text: String,
    /// Human-readable relative time, e.g. "2 hours ago".
    pub last_updated: String,
    pub message_count: usize,
    /// Estimated USD cost for the session.
    pub cost_usd: f64,
    /// Synopsis of what the session was (first) about. Empty when the
    /// session predates the synopsizer (heuristic extraction also failed).
    pub synopsis_about: String,
    /// Synopsis of where the session left off (last known working point).
    pub synopsis_left_off: String,
    /// Transcript path for the tail preview popup; empty when unavailable.
    pub transcript_path: std::path::PathBuf,
}

/// State for the session browser overlay.
pub struct SessionBrowserState {
    pub visible: bool,
    pub selected_idx: usize,
    pub sessions: Vec<SessionEntry>,
    pub mode: SessionBrowserMode,
    /// Input buffer used while in `Rename` mode.
    pub rename_input: String,
    /// Live search/filter query for session titles and message content.
    pub search_query: String,
    /// The actual session ID captured when entering rename mode (to avoid
    /// filtered-index mismatch when confirming the rename).
    rename_session_id: String,
    /// Vim-modal insert-mode state for the search bar (only used when vim is enabled).
    pub vim_search: VimSearch,
    /// First visible row of the list viewport (so the selection stays in view).
    pub list_offset: usize,
    /// Tail preview popup state: (is_user, text) rows for the focused session.
    pub tail_preview: Option<Vec<(bool, String)>>,
    /// Session id the current `tail_preview` was loaded for.
    pub tail_preview_for: String,
    /// Scroll offset (rows from the top) inside the tail preview popup.
    pub tail_scroll: usize,
    /// True while the background tail load for the focused session is in flight.
    pub tail_loading: bool,
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

impl SessionBrowserState {
    /// Create a new, hidden browser with an empty session list.
    pub fn new() -> Self {
        Self {
            visible: false,
            selected_idx: 0,
            sessions: Vec::new(),
            mode: SessionBrowserMode::Browse,
            rename_input: String::new(),
            search_query: String::new(),
            rename_session_id: String::new(),
            vim_search: VimSearch::new(),
            list_offset: 0,
            tail_preview: None,
            tail_preview_for: String::new(),
            tail_scroll: 0,
            tail_loading: false,
        }
    }

    /// Open the browser with the provided session list.
    pub fn open(&mut self, sessions: Vec<SessionEntry>) {
        self.sessions = sessions;
        self.selected_idx = 0;
        self.mode = SessionBrowserMode::Browse;
        self.rename_input.clear();
        self.search_query.clear();
        self.rename_session_id.clear();
        self.vim_search.reset();
        self.visible = true;
    }

    /// Append a character to the search filter.
    pub fn push_search_char(&mut self, c: char) {
        self.search_query.push(c);
        self.selected_idx = 0;
    }

    /// Remove the last character from the search filter.
    pub fn pop_search_char(&mut self) {
        self.search_query.pop();
        self.selected_idx = 0;
    }

    /// Invalidate the tail preview when the selection moves; the app layer
    /// reloads it for the newly focused session.
    pub fn invalidate_tail(&mut self) {
        self.tail_preview = None;
        self.tail_preview_for.clear();
        self.tail_scroll = 0;
    }

    /// The filtered list index of the first visible row given a viewport of
    /// `rows` entries, keeping the selection in view.
    pub fn clamped_list_offset(&self, rows: usize) -> usize {
        let count = self.filtered_sessions().len();
        if rows == 0 || count <= rows {
            return 0;
        }
        let max_off = count - rows;
        if self.selected_idx < rows {
            0
        } else if self.selected_idx >= count {
            max_off
        } else {
            self.selected_idx.saturating_sub(rows - 1).min(max_off)
        }
    }

    /// Return sessions whose titles contain the search query (case-insensitive).
    /// When query is empty, returns all sessions.
    pub fn filtered_sessions(&self) -> Vec<&SessionEntry> {
        if self.search_query.is_empty() {
            return self.sessions.iter().collect();
        }
        let q = self.search_query.to_lowercase();
        self.sessions
            .iter()
            .filter(|s| {
                s.title.to_lowercase().contains(&q) || s.searchable_text.to_lowercase().contains(&q)
            })
            .collect()
    }

    /// Close the browser entirely.
    pub fn close(&mut self) {
        self.visible = false;
        self.mode = SessionBrowserMode::Browse;
        self.rename_input.clear();
        self.search_query.clear();
        self.rename_session_id.clear();
        self.vim_search.reset();
        self.list_offset = 0;
        self.tail_preview = None;
        self.tail_preview_for.clear();
        self.tail_scroll = 0;
        self.tail_loading = false;
    }

    /// Move selection up one row, wrapping to the end.
    pub fn select_prev(&mut self) {
        let count = self.filtered_sessions().len();
        if count == 0 {
            self.selected_idx = 0;
            return;
        }
        if self.selected_idx == 0 {
            self.selected_idx = count - 1;
        } else {
            self.selected_idx -= 1;
        }
    }

    /// Move selection down one row, wrapping to the start.
    pub fn select_next(&mut self) {
        let count = self.filtered_sessions().len();
        if count == 0 {
            self.selected_idx = 0;
            return;
        }
        self.selected_idx = (self.selected_idx + 1) % count;
    }

    /// Return a reference to the currently selected session, if any.
    /// Respects the active search filter — uses `filtered_sessions()` so
    /// `selected_idx` always maps to the correct session.
    pub fn selected_session(&self) -> Option<&SessionEntry> {
        self.filtered_sessions().get(self.selected_idx).copied()
    }

    /// Switch to rename mode, pre-populating the input with the current title.
    /// Captures the session ID so `confirm_rename()` can find the right session
    /// even when a search filter is active (filtered-index mismatch guard).
    pub fn start_rename(&mut self) {
        if let Some(session) = self.filtered_sessions().get(self.selected_idx).copied() {
            let title = session.title.clone();
            let sid = session.id.clone();
            self.rename_input = title;
            self.rename_session_id = sid;
            self.mode = SessionBrowserMode::Rename;
        }
    }

    /// Append a character to the rename input buffer.
    pub fn push_rename_char(&mut self, c: char) {
        if self.mode == SessionBrowserMode::Rename {
            self.rename_input.push(c);
        }
    }

    /// Remove the last character from the rename input buffer.
    pub fn pop_rename_char(&mut self) {
        if self.mode == SessionBrowserMode::Rename {
            self.rename_input.pop();
        }
    }

    /// Confirm the rename. Returns `(session_id, new_name)` when in rename mode
    /// with a non-empty name and a valid selection. Resets to browse mode.
    /// Uses the captured `rename_session_id` to find the right session even
    /// when a search filter was active when renaming started.
    pub fn confirm_rename(&mut self) -> Option<(String, String)> {
        if self.mode != SessionBrowserMode::Rename {
            return None;
        }
        let new_name = self.rename_input.trim().to_string();
        if new_name.is_empty() || self.rename_session_id.is_empty() {
            return None;
        }
        let session_id = self.rename_session_id.clone();
        // Apply the rename in the local list immediately for UI consistency.
        if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            session.title = new_name.clone();
        }
        self.mode = SessionBrowserMode::Browse;
        self.rename_input.clear();
        self.rename_session_id.clear();
        Some((session_id, new_name))
    }

    /// Cancel the current mode:
    /// - In `Rename` or `Confirm` mode: return to `Browse`.
    /// - In `Browse` mode: close the overlay.
    pub fn cancel(&mut self) {
        match self.mode {
            SessionBrowserMode::Browse => self.close(),
            SessionBrowserMode::Rename | SessionBrowserMode::Confirm => {
                self.mode = SessionBrowserMode::Browse;
                self.rename_input.clear();
            }
        }
    }
}

impl Default for SessionBrowserState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

/// Format a cost as a dollar string with 4 decimal places. Free sessions
/// price at $0.00, so zero renders as an empty cell instead of "$0.0000".
fn fmt_cost(usd: f64) -> String {
    if usd < 0.0001 {
        String::new()
    } else {
        format!("${:.4}", usd)
    }
}

/// Truncate `s` to fit within `max_width` display columns, appending `…` if cut.
fn truncate_display(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_string();
    }
    if max_width <= 1 {
        return "…".to_string();
    }
    let mut out = String::new();
    for ch in s.chars() {
        if out.width() + ch.len_utf8() + 1 > max_width {
            break;
        }
        out.push(ch);
    }
    format!("{}…", out)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render the session browser overlay directly into `buf`.
///
/// Draws a centred modal (≈70 wide × ≈22 tall) with:
/// - A scrollable list of sessions; each entry is a title row plus two
///   synopsis rows (what it was about / where it left off)
/// - Selection highlight on the focused entry
/// - Mode-sensitive hint bar at the bottom
/// - A rename input field shown when in `Rename` mode
///
/// A second popup under the browser shows the tail of the focused session.
pub fn render_session_browser(state: &SessionBrowserState, area: Rect, buf: &mut Buffer) {
    if !state.visible {
        return;
    }

    const MODAL_W: u16 = 70;
    const MODAL_H: u16 = 22;

    let dialog_area = centered_rect(
        MODAL_W.min(area.width.saturating_sub(2)),
        MODAL_H.min(area.height.saturating_sub(2)),
        area,
    );

    // --- Clear background -------------------------------------------------
    for y in dialog_area.y..dialog_area.y + dialog_area.height {
        for x in dialog_area.x..dialog_area.x + dialog_area.width {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.reset();
            }
        }
    }

    let inner_w = dialog_area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();

    // --- Search line (always shown in Browse mode) -------------------------
    if state.mode == SessionBrowserMode::Browse {
        let search_line = modal_search_line_with_insert(
            &state.search_query,
            "Type to filter sessions...",
            CLAWDE_MUTED,
            Color::Cyan,
            state.vim_search.insert,
        );
        lines.push(search_line);
        lines.push(Line::from(""));
    }

    // --- Session list -----------------------------------------------------
    let filtered = state.filtered_sessions();

    if filtered.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            if state.search_query.is_empty() {
                "  No sessions found."
            } else {
                "  No sessions match your search."
            },
            Style::default().fg(Color::DarkGray),
        )]));
    } else {
        // Column widths (approximate):
        //   title: ~40 chars  |  date: ~14 chars  |  msgs: 5  |  cost: 9
        let date_w: usize = 14;
        // Drop the Msgs column when no visible session has a nonzero count
        // (e.g. project-scoped JSONL store doesn't track costs).
        let any_msgs = filtered.iter().any(|s| s.message_count > 0);
        let msgs_w: usize = if any_msgs { 5 } else { 0 };
        let msgs_header = if any_msgs { "Msgs" } else { "" };
        // Drop the Cost column when no visible session has a nonzero cost.
        let any_cost = filtered.iter().any(|s| s.cost_usd >= 0.0001);
        let cost_w: usize = if any_cost { 9 } else { 0 };
        let cost_header = if any_cost { "Cost" } else { "" };
        let fixed = date_w + msgs_w + cost_w + 6; // separators & padding
        let title_w = inner_w.saturating_sub(fixed).max(10);

        // Header row
        lines.push(Line::from(vec![Span::styled(
            format!(
                "  {:<title_w$}  {:<date_w$}  {:>msgs_w$}  {:>cost_w$}",
                "Title",
                "Last Updated",
                msgs_header,
                cost_header,
                title_w = title_w,
                date_w = date_w,
                msgs_w = msgs_w,
                cost_w = cost_w
            ),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::UNDERLINED),
        )]));
        lines.push(Line::from(""));

        // List viewport: entries are 3 rows tall (title + 2 synopsis lines),
        // so the window shows as many whole entries as the modal body allows.
        // The bottom hint bar takes 2 rows inside the body.
        let body_rows = dialog_area.height.saturating_sub(2) as usize; // minus borders
        let chrome_rows = if state.mode == SessionBrowserMode::Browse {
            3
        } else {
            1
        }; // search line + spacer, or spacer
        let chrome_rows = chrome_rows + 2; // hint bar + spacer above it
        let list_rows = body_rows.saturating_sub(chrome_rows);
        const ROWS_PER_ENTRY: usize = 3;
        let visible_entries = (list_rows / ROWS_PER_ENTRY).max(1);
        let list_offset = state.clamped_list_offset(visible_entries);
        let last_shown = list_offset + visible_entries;

        for (i, session) in filtered.iter().enumerate() {
            if i < list_offset {
                continue;
            }
            if i >= last_shown {
                break;
            }
            let is_selected = i == state.selected_idx;
            let title_cell = truncate_display(&session.title, title_w);
            let date_cell = truncate_display(&session.last_updated, date_w);
            let msgs_cell = format!("{:>msgs_w$}", session.message_count, msgs_w = msgs_w);
            let cost_cell = format!("{:>cost_w$}", fmt_cost(session.cost_usd), cost_w = cost_w);

            let row_bg = if is_selected {
                Color::Rgb(40, 60, 80)
            } else {
                // transparent — ratatui uses reset/default for "no background"
                Color::Reset
            };

            let title_style = if is_selected {
                Style::default()
                    .fg(Color::Cyan)
                    .bg(row_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            let meta_style = if is_selected {
                Style::default().fg(Color::Rgb(180, 200, 220)).bg(row_bg)
            } else {
                Style::default().fg(Color::DarkGray)
            };

            let prefix_style = Style::default().bg(row_bg);

            lines.push(Line::from(vec![
                Span::styled("  ", prefix_style),
                Span::styled(
                    format!("{:<title_w$}", title_cell, title_w = title_w),
                    title_style,
                ),
                Span::styled("  ", meta_style),
                Span::styled(
                    format!("{:<date_w$}", date_cell, date_w = date_w),
                    meta_style,
                ),
                Span::styled("  ", meta_style),
                Span::styled(msgs_cell, meta_style),
                Span::styled("  ", meta_style),
                Span::styled(cost_cell, meta_style),
            ]));

            // Synopsis rows: what the session was about, then where it left
            // off. Indented under the title; muted so they read as secondary.
            let syn_style = if is_selected {
                Style::default().fg(Color::Rgb(120, 160, 190)).bg(row_bg)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let syn_w = inner_w.saturating_sub(6);
            if !session.synopsis_about.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("    {}", truncate_display(&session.synopsis_about, syn_w)),
                    syn_style,
                )));
            } else {
                lines.push(Line::from(Span::styled("    \u{2026}", syn_style)));
            }
            if !session.synopsis_left_off.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!(
                        "    \u{21b3} {}",
                        truncate_display(&session.synopsis_left_off, syn_w.saturating_sub(2))
                    ),
                    syn_style,
                )));
            }
        }

        // Overflow hint when the viewport hides entries.
        let hidden_below = filtered.len().saturating_sub(last_shown);
        if list_offset > 0 || hidden_below > 0 {
            let mut hint = String::new();
            if list_offset > 0 {
                hint.push_str(&format!("\u{2191} {} above", list_offset));
            }
            if hidden_below > 0 {
                if !hint.is_empty() {
                    hint.push_str("  ");
                }
                hint.push_str(&format!("\u{2193} {} below", hidden_below));
            }
            lines.push(Line::from(Span::styled(
                format!("  {}", hint),
                Style::default().fg(Color::Rgb(90, 90, 110)),
            )));
        }
    }

    lines.push(Line::from(""));

    // --- Mode-sensitive bottom section -----------------------------------
    match &state.mode {
        SessionBrowserMode::Browse => {
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    "\u{2191}\u{2193}",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" navigate  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "Enter",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("=resume  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "r",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("=rename  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "Esc",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("=close", Style::default().fg(Color::DarkGray)),
            ]));
        }
        SessionBrowserMode::Rename => {
            // Show rename input field.
            let label = "  Rename: ";
            let cursor = "\u{2588}"; // block cursor
            let input_display = format!("{}{}", state.rename_input, cursor);
            lines.push(Line::from(vec![
                Span::styled(
                    label,
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    input_display,
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    "Enter",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("=confirm  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "Esc",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("=cancel", Style::default().fg(Color::DarkGray)),
            ]));
        }
        SessionBrowserMode::Confirm => {
            lines.push(Line::from(vec![
                Span::styled(
                    "  Confirm? ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "Enter",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("=yes  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "Esc",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::styled("=no", Style::default().fg(Color::DarkGray)),
            ]));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Sessions ")
        .title_alignment(Alignment::Center)
        .border_style(Style::default().fg(Color::Cyan));

    let para = Paragraph::new(lines)
        .block(block)
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: false });

    use ratatui::widgets::Widget;
    para.render(dialog_area, buf);

    render_tail_preview(state, dialog_area, buf);
}

/// Render one single-row paragraph into `buf` (Widget trait import lives at
/// the call site in `render_session_browser`).
fn render_tail_row(line: Line<'_>, area: Rect, buf: &mut Buffer) {
    use ratatui::widgets::Widget;
    Paragraph::new(line).render(area, buf);
}

/// Render the tail preview: a second popup under the browser showing the last
/// messages of the focused session, so past sessions can be browsed at a
/// glance without resuming them.
fn render_tail_preview(state: &SessionBrowserState, browser_area: Rect, buf: &mut Buffer) {
    let Some(selected) = state.selected_session() else {
        return;
    };

    const TAIL_W: u16 = 70;
    const TAIL_H: u16 = 9;

    // Anchor below the browser modal, clamped to the screen.
    let width = TAIL_W.min(browser_area.width);
    let height = TAIL_H.min(browser_area.bottom().saturating_sub(browser_area.y + 1));
    if width < 10 || height < 4 {
        return;
    }
    let x = browser_area.x;
    let y = (browser_area.bottom()).min(buf.area.height.saturating_sub(height));
    let tail_area = Rect::new(x, y, width, height);

    // Clear + border.
    for iy in tail_area.y..tail_area.bottom() {
        for ix in tail_area.x..tail_area.right() {
            if let Some(cell) = buf.cell_mut((ix, iy)) {
                cell.reset();
            }
        }
    }
    let border_style = Style::default().fg(Color::Rgb(70, 110, 140));
    for iy in tail_area.y..tail_area.bottom() {
        for ix in tail_area.x..tail_area.right() {
            let is_top = iy == tail_area.y;
            let is_bot = iy == tail_area.bottom() - 1;
            let is_left = ix == tail_area.x;
            let is_right = ix == tail_area.right() - 1;
            if let Some(cell) = buf.cell_mut((ix, iy)) {
                let ch = match (is_top, is_bot, is_left, is_right) {
                    (true, _, true, _) => '╭',
                    (true, _, _, true) => '╮',
                    (_, true, true, _) => '╰',
                    (_, true, _, true) => '╯',
                    (true, _, _, _) | (_, true, _, _) => '─',
                    (_, _, true, _) | (_, _, _, true) => '│',
                    _ => continue,
                };
                cell.set_char(ch);
                cell.set_style(border_style);
            }
        }
    }

    // Title: session label.
    let title = format!(
        " {} ",
        truncate_display(&selected.title, width.saturating_sub(8) as usize)
    );
    for (i, ch) in title.chars().enumerate() {
        if let Some(cell) = buf.cell_mut((tail_area.x + 2 + i as u16, tail_area.y)) {
            cell.set_char(ch);
            cell.set_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
        }
    }

    let inner = Rect::new(
        tail_area.x + 2,
        tail_area.y + 1,
        width.saturating_sub(4),
        height.saturating_sub(2),
    );

    let Some(rows) = state.tail_preview.as_ref() else {
        let note = if state.tail_loading {
            " loading transcript…"
        } else {
            " (no transcript preview)"
        };
        let note_line = Line::from(Span::styled(note, Style::default().fg(Color::DarkGray)));
        render_tail_row(note_line, Rect::new(inner.x, inner.y, inner.width, 1), buf);
        return;
    };

    if rows.is_empty() {
        let note_line = Line::from(Span::styled(
            " (no messages found)",
            Style::default().fg(Color::DarkGray),
        ));
        render_tail_row(note_line, Rect::new(inner.x, inner.y, inner.width, 1), buf);
        return;
    }

    // Scroll clamp: content rows minus viewport rows.
    let max_scroll = rows.len().saturating_sub(inner.height as usize);
    let scroll = state.tail_scroll.min(max_scroll);

    // Status right-aligned in the title row: row position.
    let pos = format!("{}/{} ", (scroll + 1).min(rows.len()), rows.len());
    let pos_x = tail_area.right().saturating_sub(pos.len() as u16 + 2);
    for (i, ch) in pos.chars().enumerate() {
        if let Some(cell) = buf.cell_mut((pos_x + i as u16, tail_area.y)) {
            cell.set_char(ch);
            cell.set_style(Style::default().fg(Color::DarkGray));
        }
    }

    for (i, (is_user, text)) in rows
        .iter()
        .skip(scroll)
        .take(inner.height as usize)
        .enumerate()
    {
        let prefix = if *is_user { "❯ " } else { "  " };
        let style = if *is_user {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::Rgb(170, 170, 180))
        };
        let display = truncate_display(text, inner.width.saturating_sub(3) as usize);
        let line = Line::from(Span::styled(format!("{}{}", prefix, display), style));
        render_tail_row(
            line,
            Rect::new(inner.x, inner.y + i as u16, inner.width, 1),
            buf,
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_sessions() -> Vec<SessionEntry> {
        vec![
            SessionEntry {
                id: "sess-001".to_string(),
                title: "Refactor auth module".to_string(),
                searchable_text: "rotate authentication tokens".to_string(),
                last_updated: "2 hours ago".to_string(),
                message_count: 34,
                cost_usd: 0.0124,
                synopsis_about: "Fixing OAuth token rotation".to_string(),
                synopsis_left_off: "Mid-refactor of the refresh flow".to_string(),
                transcript_path: std::path::PathBuf::new(),
            },
            SessionEntry {
                id: "sess-002".to_string(),
                title: "Write unit tests".to_string(),
                searchable_text: "coverage report".to_string(),
                last_updated: "yesterday".to_string(),
                message_count: 12,
                cost_usd: 0.0045,
                synopsis_about: String::new(),
                synopsis_left_off: String::new(),
                transcript_path: std::path::PathBuf::new(),
            },
            SessionEntry {
                id: "sess-003".to_string(),
                title: "Debug memory leak".to_string(),
                searchable_text: "heap profile".to_string(),
                last_updated: "3 days ago".to_string(),
                message_count: 57,
                cost_usd: 0.0289,
                synopsis_about: "Tracking a leak in the renderer".to_string(),
                synopsis_left_off: String::new(),
                transcript_path: std::path::PathBuf::new(),
            },
        ]
    }

    // 1. new() starts hidden with no sessions.
    #[test]
    fn new_starts_hidden() {
        let s = SessionBrowserState::new();
        assert!(!s.visible);
        assert!(s.sessions.is_empty());
        assert_eq!(s.mode, SessionBrowserMode::Browse);
    }

    // 2. open() populates sessions and becomes visible.
    #[test]
    fn open_populates_and_shows() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        assert!(s.visible);
        assert_eq!(s.sessions.len(), 3);
        assert_eq!(s.selected_idx, 0);
        assert_eq!(s.mode, SessionBrowserMode::Browse);
    }

    // 3. select_next() advances selection and wraps to the start.
    #[test]
    fn select_next_wraps_to_start() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.select_next();
        assert_eq!(s.selected_idx, 1);
        s.select_next();
        assert_eq!(s.selected_idx, 2);
        s.select_next();
        assert_eq!(s.selected_idx, 0);
    }

    // 4. select_prev() decrements and wraps to the end.
    #[test]
    fn select_prev_wraps_to_end() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.select_prev();
        assert_eq!(s.selected_idx, 2);
    }

    // 5. Content search matches message text and keeps selection in range.
    #[test]
    fn content_search_matches_message_text() {
        let mut state = SessionBrowserState::new();
        state.open(sample_sessions());
        state.push_search_char('t');
        state.push_search_char('o');
        state.push_search_char('k');
        let matches = state.filtered_sessions();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].id, "sess-001");
        state.select_next();
        assert_eq!(
            state.selected_session().map(|session| session.id.as_str()),
            Some("sess-001")
        );

        state.search_query = "does-not-exist".to_string();
        state.select_next();
        assert!(state.selected_session().is_none());
        assert_eq!(state.selected_idx, 0);
    }

    // 6. selected_session() returns correct entry.
    #[test]
    fn selected_session_correct() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.selected_idx = 1;
        let sess = s.selected_session().unwrap();
        assert_eq!(sess.id, "sess-002");
    }

    // 6. start_rename() switches mode and pre-fills input.
    #[test]
    fn start_rename_prefills_title() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.selected_idx = 0;
        s.start_rename();
        assert_eq!(s.mode, SessionBrowserMode::Rename);
        assert_eq!(s.rename_input, "Refactor auth module");
    }

    // 7. push_rename_char / pop_rename_char edit the input buffer.
    #[test]
    fn rename_char_editing() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.start_rename();
        s.rename_input.clear(); // clear prefill for clean test
        s.push_rename_char('H');
        s.push_rename_char('i');
        assert_eq!(s.rename_input, "Hi");
        s.pop_rename_char();
        assert_eq!(s.rename_input, "H");
    }

    // 8. confirm_rename() returns (id, new_name) and resets mode.
    #[test]
    fn confirm_rename_returns_pair() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.selected_idx = 0;
        s.start_rename();
        s.rename_input = "  New Title  ".to_string(); // intentional whitespace
        let result = s.confirm_rename();
        assert_eq!(
            result,
            Some(("sess-001".to_string(), "New Title".to_string()))
        );
        assert_eq!(s.mode, SessionBrowserMode::Browse);
        assert!(s.rename_input.is_empty());
        // Also check local title was updated
        assert_eq!(s.sessions[0].title, "New Title");
    }

    // 9. confirm_rename() with empty input returns None.
    #[test]
    fn confirm_rename_empty_returns_none() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.start_rename();
        s.rename_input = "   ".to_string(); // whitespace only
        let result = s.confirm_rename();
        assert!(result.is_none());
    }

    // 10. cancel() in Rename mode returns to Browse without closing.
    #[test]
    fn cancel_rename_goes_to_browse() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.start_rename();
        s.cancel();
        assert_eq!(s.mode, SessionBrowserMode::Browse);
        assert!(
            s.visible,
            "overlay should remain visible after cancel-from-rename"
        );
    }

    // 11. cancel() in Browse mode closes the overlay.
    #[test]
    fn cancel_browse_closes() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        assert_eq!(s.mode, SessionBrowserMode::Browse);
        s.cancel();
        assert!(!s.visible);
    }

    // 12. render_session_browser does not panic.
    #[test]
    fn render_does_not_panic() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        let area = Rect::new(0, 0, 120, 40);
        let mut buf = Buffer::empty(area);
        render_session_browser(&s, area, &mut buf);
    }

    #[test]
    fn render_shows_synopsis_rows_and_tail_popup() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        let area = Rect::new(0, 0, 120, 40);
        let mut buf = Buffer::empty(area);
        // Tail preview rows for the focused session.
        s.tail_preview = Some(vec![
            (true, "how does the refresh flow work?".to_string()),
            (false, "it rotates tokens via the auth module".to_string()),
        ]);
        s.tail_preview_for = "sess-001".to_string();
        render_session_browser(&s, area, &mut buf);

        let text: String = buf.content.iter().map(|c| c.symbol().to_string()).collect();
        // Synopsis rows for the selected entry.
        assert!(
            text.contains("Fixing OAuth token rotation"),
            "about row missing"
        );
        assert!(
            text.contains("Mid-refactor of the refresh flow"),
            "left-off row missing"
        );
        // Tail popup content and user prefix.
        assert!(
            text.contains("how does the refresh flow work?"),
            "tail user row missing"
        );
        assert!(
            text.contains("it rotates tokens via the auth module"),
            "tail assistant row missing"
        );
    }

    #[test]
    fn render_synopsis_absent_shows_ellipsis() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.selected_idx = 1; // sess-002 has no synopsis
        let area = Rect::new(0, 0, 120, 40);
        let mut buf = Buffer::empty(area);
        render_session_browser(&s, area, &mut buf);
        let text: String = buf.content.iter().map(|c| c.symbol().to_string()).collect();
        assert!(text.contains("Write unit tests"));
    }

    #[test]
    fn list_offset_keeps_selection_visible() {
        let mut s = SessionBrowserState::new();
        let mut many = Vec::new();
        for i in 0..40 {
            many.push(SessionEntry {
                id: format!("s{i:03}"),
                title: format!("session {i}"),
                searchable_text: String::new(),
                last_updated: "now".to_string(),
                message_count: 1,
                cost_usd: 0.0,
                synopsis_about: String::new(),
                synopsis_left_off: String::new(),
                transcript_path: std::path::PathBuf::new(),
            });
        }
        s.open(many);
        s.selected_idx = 30;
        let off = s.clamped_list_offset(6);
        assert!(off <= 30 && 30 < off + 6, "off={off}");
        s.selected_idx = 0;
        assert_eq!(s.clamped_list_offset(6), 0);
    }

    // 13. render is a no-op when hidden.
    #[test]
    fn render_noop_when_hidden() {
        let s = SessionBrowserState::new(); // visible = false
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        render_session_browser(&s, area, &mut buf);
        for cell in buf.content() {
            assert_eq!(
                cell.symbol(),
                " ",
                "buffer should be empty when browser is hidden"
            );
        }
    }

    // 14. fmt_cost formats correctly — zero renders as an empty cell so free
    //     sessions don't fill the table with "$0.0000" readouts.
    #[test]
    fn fmt_cost_formats() {
        assert_eq!(fmt_cost(0.0), "");
        assert_eq!(fmt_cost(0.00005), "");
        assert_eq!(fmt_cost(0.0124), "$0.0124");
        assert_eq!(fmt_cost(1.5), "$1.5000");
    }

    // 15. truncate_display trims long strings.
    #[test]
    fn truncate_display_trims() {
        let long = "abcdefghij"; // 10 chars
        let result = truncate_display(long, 5);
        assert!(
            result.width() <= 6,
            "truncated string should fit within budget"
        );
        assert!(result.ends_with('…'));
    }
}
