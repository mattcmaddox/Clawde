// ask_user_dialog.rs — TUI overlay for model-initiated questions.
//
// Rendered when the model calls the `AskUserQuestion` tool.  The dialog
// shows the question text, an optional list of predefined choices that the
// user can navigate with arrow keys or number shortcuts, and a free-text
// input line for a custom answer.
//
// Layout:
//   ┌─ Question ──────────────────────────────────────┐
//   │                                                 │
//   │  How should the tests be run?                   │
//   │                                                 │
//   │  ▶ 1  cargo test --workspace                    │
//   │    2  cargo test -p clawde-api                 │
//   │    3  cargo test --features dev_full            │
//   │                                                 │
//   │  ❯ _                              (custom)      │
//   │                                                 │
//   │  Tab/↑↓: navigate   Enter: confirm   Esc: skip  │
//   └─────────────────────────────────────────────────┘

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use std::cell::Cell;

use crate::input_layout::{split_at_display_col, WrappedInput};
use crate::overlays::{centered_rect, CLAWDE_PANEL_BG};

const BORDER_FG: Color = Color::Rgb(120, 120, 170);
const TITLE_FG: Color = Color::Rgb(200, 160, 255);
const QUESTION_FG: Color = Color::Rgb(230, 230, 230);
const OPTION_FG: Color = Color::Rgb(190, 190, 210);
const SELECTED_FG: Color = Color::Rgb(255, 255, 255);
const SELECTED_BG: Color = Color::Rgb(55, 55, 90);
const HINT_FG: Color = Color::Rgb(100, 100, 130);
const INPUT_FG: Color = Color::Rgb(200, 255, 200);
const NUMBER_FG: Color = Color::Rgb(150, 150, 200);

/// State for the ask-user question dialog overlay.
#[derive(Default)]
pub struct AskUserDialogState {
    /// Whether the dialog is currently visible.
    pub visible: bool,
    /// The area used by this dialog in the last render (for click-outside detection).
    pub last_rect: Cell<Rect>,
    /// The question text from the model.
    pub question: String,
    /// Optional predefined choices.
    pub options: Option<Vec<String>>,
    /// Index of the currently highlighted option (0 = custom-text row when
    /// options is None, or indices into options vec, with the custom row last).
    pub selected_idx: usize,
    /// Scroll offset for the options list (for viewport when options exceed height).
    pub scroll_offset: usize,
    /// Custom text the user is typing (if they choose not to pick an option).
    pub custom_text: String,
    /// Insert-point byte cursor inside `custom_text` (supports mid-string edits).
    pub custom_cursor: usize,
    /// Whether cursor is in the custom-text input row.
    pub in_custom_input: bool,
    /// Pending reply channel sender — set when the dialog opens, consumed on submit.
    pub(crate) reply_tx: Option<tokio::sync::oneshot::Sender<String>>,
}

impl AskUserDialogState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open the dialog with a question and optional choices.
    pub fn open(
        &mut self,
        question: String,
        options: Option<Vec<String>>,
        reply_tx: tokio::sync::oneshot::Sender<String>,
    ) {
        self.question = question;
        self.options = options;
        self.selected_idx = 0;
        self.scroll_offset = 0;
        self.custom_text.clear();
        self.custom_cursor = 0;
        self.in_custom_input = self.options.is_none();
        self.reply_tx = Some(reply_tx);
        self.visible = true;
    }

    /// Max visible option rows in the dialog viewport.
    const VISIBLE_OPTION_ROWS: usize = 8;

    /// Navigate selection up.
    pub fn select_prev(&mut self) {
        let n = self.option_count();
        if n == 0 {
            return;
        }
        if self.selected_idx == 0 {
            self.selected_idx = n; // wrap to custom row
            self.focus_custom_row();
        } else {
            self.selected_idx -= 1;
            self.in_custom_input = self.selected_idx >= self.options_len();
            if self.in_custom_input {
                self.custom_cursor = self.custom_text.len();
            }
        }
        self.ensure_visible();
    }

    /// Navigate selection down.
    pub fn select_next(&mut self) {
        let n = self.option_count();
        if n == 0 {
            return;
        }
        if self.selected_idx >= n {
            self.selected_idx = 0;
            self.in_custom_input = false;
        } else {
            self.selected_idx += 1;
            self.in_custom_input = self.selected_idx >= self.options_len();
            if self.in_custom_input {
                self.custom_cursor = self.custom_text.len();
            }
        }
        self.ensure_visible();
    }

    /// Adjust scroll_offset so the selected option (non-custom) is visible.
    fn ensure_visible(&mut self) {
        let opts_len = self.options_len();
        if opts_len == 0 || self.in_custom_input {
            return;
        }
        if self.selected_idx < self.scroll_offset {
            self.scroll_offset = self.selected_idx;
        } else if self.selected_idx >= self.scroll_offset + Self::VISIBLE_OPTION_ROWS {
            self.scroll_offset = self.selected_idx + 1 - Self::VISIBLE_OPTION_ROWS;
        }
    }

    /// Select an option directly by 1-based number key.
    pub fn select_by_number(&mut self, n: usize) {
        if let Some(ref opts) = self.options {
            if n >= 1 && n <= opts.len() {
                self.selected_idx = n - 1;
                self.in_custom_input = false;
                self.ensure_visible();
            }
        }
    }

    /// Focus the custom write-in row, placing the cursor at the end of any text
    /// already typed there.
    fn focus_custom_row(&mut self) {
        self.in_custom_input = true;
        self.custom_cursor = self.custom_text.len();
    }

    /// Insert a character at the cursor in the custom-text input.
    ///
    /// Any printable character auto-switches to the custom row regardless of
    /// where the selection currently is — so the user can just start typing
    /// without having to navigate down with Tab/↓ first.
    pub fn push_char(&mut self, c: char) {
        self.custom_text.insert(self.custom_cursor, c);
        self.custom_cursor += c.len_utf8();
        self.in_custom_input = true;
        self.selected_idx = self.options_len();
    }

    /// Backspace in the custom-text input (delete the character before the
    /// cursor).
    pub fn pop_char(&mut self) {
        if (self.in_custom_input || self.options.is_none()) && self.custom_cursor > 0 {
            let prev = self.custom_text[..self.custom_cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.custom_text.remove(prev);
            self.custom_cursor = prev;
        }
    }

    /// Delete the character at the cursor (forward delete).
    pub fn delete_char(&mut self) {
        if self.custom_cursor < self.custom_text.len() {
            self.custom_text.remove(self.custom_cursor);
        }
    }

    /// Move the cursor one character to the left.
    pub fn move_cursor_left(&mut self) {
        if self.custom_cursor > 0 {
            self.custom_cursor = self.custom_text[..self.custom_cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
    }

    /// Move the cursor one character to the right.
    pub fn move_cursor_right(&mut self) {
        if self.custom_cursor < self.custom_text.len() {
            self.custom_cursor = self.custom_text[self.custom_cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.custom_cursor + i)
                .unwrap_or(self.custom_text.len());
        }
    }

    /// Move the cursor to the start of the custom-text input.
    pub fn move_cursor_home(&mut self) {
        self.custom_cursor = 0;
    }

    /// Move the cursor to the end of the custom-text input.
    pub fn move_cursor_end(&mut self) {
        self.custom_cursor = self.custom_text.len();
    }

    /// Confirm the current selection and send the answer.
    ///
    /// Returns `true` if the dialog was successfully submitted (i.e. a reply
    /// channel was present).
    pub fn confirm(&mut self) -> bool {
        let answer = if self.in_custom_input || self.options.is_none() {
            self.custom_text.clone()
        } else if let Some(ref opts) = self.options {
            opts.get(self.selected_idx).cloned().unwrap_or_default()
        } else {
            self.custom_text.clone()
        };

        self.send_reply(answer)
    }

    /// Dismiss without answering (sends an empty string so the tool result
    /// signals "user dismissed").
    pub fn close(&mut self) {
        self.dismiss();
    }

    pub fn dismiss(&mut self) -> bool {
        self.send_reply(String::new())
    }

    fn send_reply(&mut self, answer: String) -> bool {
        self.visible = false;
        if let Some(tx) = self.reply_tx.take() {
            let _ = tx.send(answer);
            true
        } else {
            false
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn options_len(&self) -> usize {
        self.options.as_ref().map(|v| v.len()).unwrap_or(0)
    }

    /// Total number of selectable rows: options + custom-text row.
    fn option_count(&self) -> usize {
        self.options_len() + 1
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render the ask-user question dialog into the terminal buffer.
///
/// Call this only when `state.visible` is true; typically from `render_app`.
pub fn render_ask_user_dialog(state: &AskUserDialogState, area: Rect, buf: &mut Buffer) {
    if !state.visible {
        return;
    }

    // ---- size estimate ----
    // The write-in row wraps and grows a line per overflow, so its layout is
    // needed for both the height estimate and the render below. The text sits
    // inside the border (2 cols) after the two-column selection prefix.
    let width = 58u16.min(area.width.saturating_sub(4));
    let custom_layout = WrappedInput::layout(
        &state.custom_text,
        state.custom_cursor,
        width.saturating_sub(4) as usize,
    );
    let custom_lines = custom_layout.height();
    let question_lines = word_wrap(&state.question, 52).len() as u16;
    let options_count = state.options.as_ref().map(|v| v.len() as u16).unwrap_or(0);
    // Cap visible options to viewport; if options exceed it, show scroll indicators.
    let visible_options = options_count.min(AskUserDialogState::VISIBLE_OPTION_ROWS as u16);
    let has_scroll_up = state.scroll_offset > 0;
    let has_scroll_down =
        state.options_len() > state.scroll_offset + AskUserDialogState::VISIBLE_OPTION_ROWS;
    let scroll_indicators = (has_scroll_up as u16) + (has_scroll_down as u16);
    let options_lines = visible_options + scroll_indicators + 1; // +1 for spacer after options
    let height = (5 + question_lines + options_lines + custom_lines + 2)
        .min(area.height.saturating_sub(2))
        .max(10);
    let modal_area = centered_rect(width, height, area);
    state.last_rect.set(modal_area);

    // ---- background ----
    for y in modal_area.top()..modal_area.bottom() {
        for x in modal_area.left()..modal_area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_char(' ');
                cell.set_bg(CLAWDE_PANEL_BG);
            }
        }
    }

    // ---- border ----
    let border_style = Style::default().fg(BORDER_FG).bg(CLAWDE_PANEL_BG);
    let inner_w = modal_area.width.saturating_sub(2) as usize;
    for y in modal_area.top()..modal_area.bottom() {
        let is_top = y == modal_area.top();
        let is_bot = y == modal_area.bottom() - 1;
        for x in modal_area.left()..modal_area.right() {
            let is_left = x == modal_area.left();
            let is_right = x == modal_area.right() - 1;
            if let Some(cell) = buf.cell_mut((x, y)) {
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

    // ---- title ----
    let title = " Question ";
    let title_x = modal_area.left() + 2;
    let title_style = Style::default()
        .fg(TITLE_FG)
        .bg(CLAWDE_PANEL_BG)
        .add_modifier(Modifier::BOLD);
    for (i, ch) in title.chars().enumerate() {
        let x = title_x + i as u16;
        if x < modal_area.right() - 1 {
            if let Some(cell) = buf.cell_mut((x, modal_area.top())) {
                cell.set_char(ch);
                cell.set_style(title_style);
            }
        }
    }

    // ---- inner content area ----
    let inner = Rect {
        x: modal_area.x + 1,
        y: modal_area.y + 1,
        width: modal_area.width.saturating_sub(2),
        height: modal_area.height.saturating_sub(2),
    };

    let mut row = inner.y;

    macro_rules! write_line {
        ($row:expr, $line:expr) => {{
            if $row < inner.y + inner.height {
                let r = Rect {
                    x: inner.x,
                    y: $row,
                    width: inner.width,
                    height: 1,
                };
                Paragraph::new($line).render(r, buf);
            }
        }};
    }

    // Question text — clipped, never allowed to evict the option viewport,
    // custom-answer row, or hint (a long question in a short terminal used to
    // `return` here and leave the dialog unanswerable). The reservation
    // mirrors the layout below: spacer + option viewport (+ scroll indicators)
    // + spacer before custom + custom row (its guard wants one spare row
    // below) + spacer + hint = indicators + visible options + 5.
    let opts_len0 = state.options.as_ref().map(|v| v.len()).unwrap_or(0);
    let ind0 = (state.scroll_offset > 0) as usize
        + (opts_len0 > state.scroll_offset + AskUserDialogState::VISIBLE_OPTION_ROWS) as usize;
    let vis0 = opts_len0.min(AskUserDialogState::VISIBLE_OPTION_ROWS);
    let question_cap =
        (inner.y + inner.height).saturating_sub((ind0 + vis0 + custom_lines as usize + 4) as u16);

    row += 1; // top padding
    let mut clipped = false;
    for wrap_line in word_wrap(&state.question, inner_w) {
        // Keep one row in reserve for the ellipsis marker.
        if row + 1 >= question_cap {
            clipped = true;
            break;
        }
        write_line!(
            row,
            Line::from(Span::styled(
                wrap_line,
                Style::default().fg(QUESTION_FG).bg(CLAWDE_PANEL_BG)
            ))
        );
        row += 1;
    }
    // Ellipsis only when there is actually room for it; with zero question
    // rows available the tail rows take precedence.
    if clipped && row < question_cap {
        write_line!(
            row,
            Line::from(Span::styled(
                "  \u{2026}",
                Style::default().fg(HINT_FG).bg(CLAWDE_PANEL_BG)
            ))
        );
        row += 1;
    }

    // Spacer
    row += 1;

    // Option rows (scrollable viewport)
    if let Some(ref opts) = state.options {
        let opts_len = opts.len();
        let end = (state.scroll_offset + AskUserDialogState::VISIBLE_OPTION_ROWS).min(opts_len);

        // Scroll-up indicator
        if state.scroll_offset > 0 {
            write_line!(
                row,
                Line::from(Span::styled(
                    format!("   \u{2191} {} more above", state.scroll_offset),
                    Style::default()
                        .fg(Color::Rgb(90, 90, 110))
                        .bg(CLAWDE_PANEL_BG),
                ))
            );
            row += 1;
        }

        for (i, opt) in opts
            .iter()
            .enumerate()
            .skip(state.scroll_offset)
            .take(end - state.scroll_offset)
        {
            if row >= inner.y + inner.height - 2 {
                break;
            }
            let is_sel = !state.in_custom_input && state.selected_idx == i;
            let prefix = if is_sel { "▶ " } else { "  " };
            let num_str = format!("{}", i + 1);
            let label = format!(" {}", opt);
            let style_bg = if is_sel { SELECTED_BG } else { CLAWDE_PANEL_BG };
            write_line!(
                row,
                Line::from(vec![
                    Span::styled(
                        prefix,
                        Style::default()
                            .fg(if is_sel { SELECTED_FG } else { HINT_FG })
                            .bg(style_bg)
                    ),
                    Span::styled(num_str, Style::default().fg(NUMBER_FG).bg(style_bg)),
                    Span::styled(
                        label,
                        Style::default()
                            .fg(if is_sel { SELECTED_FG } else { OPTION_FG })
                            .bg(style_bg)
                            .add_modifier(if is_sel {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            })
                    ),
                ])
            );
            row += 1;
        }

        // Scroll-down indicator
        let remaining = opts_len.saturating_sub(end);
        if remaining > 0 {
            write_line!(
                row,
                Line::from(Span::styled(
                    format!("   \u{2193} {} more below", remaining),
                    Style::default()
                        .fg(Color::Rgb(90, 90, 110))
                        .bg(CLAWDE_PANEL_BG),
                ))
            );
            row += 1;
        }

        row += 1; // spacer before custom row
    }

    // Custom input row — word-wrapped across as many rows as the answer needs;
    // the dialog grows to fit (see the height estimate above).
    let is_sel = state.in_custom_input || state.options.is_none();
    let style_bg = if is_sel { SELECTED_BG } else { CLAWDE_PANEL_BG };
    let prefix = if is_sel { "❯ " } else { "  " };
    let show_placeholder = state.custom_text.is_empty() && !is_sel && state.options.is_some();
    if show_placeholder {
        if row < inner.y + inner.height - 1 {
            write_line!(
                row,
                Line::from(vec![
                    Span::styled(prefix, Style::default().fg(HINT_FG).bg(style_bg),),
                    Span::styled(
                        "type to fill custom answer…",
                        Style::default().fg(HINT_FG).bg(style_bg),
                    ),
                ])
            );
            row += 1;
        }
    } else {
        for (i, line_text) in custom_layout.lines.iter().enumerate() {
            if row >= inner.y + inner.height - 1 {
                break;
            }
            let lead = if i == 0 { prefix } else { "  " };
            let mut spans = vec![Span::styled(
                lead,
                Style::default()
                    .fg(if is_sel { SELECTED_FG } else { HINT_FG })
                    .bg(style_bg),
            )];
            if is_sel && i == custom_layout.cursor_row {
                // Split the line at the cursor so the insertion point is drawn
                // as a solid block within the wrapped text.
                let (before, after) = split_at_display_col(line_text, custom_layout.cursor_col);
                if !before.is_empty() {
                    spans.push(Span::styled(
                        before,
                        Style::default().fg(INPUT_FG).bg(style_bg),
                    ));
                }
                spans.push(Span::styled(
                    "█",
                    Style::default().fg(INPUT_FG).bg(style_bg),
                ));
                if !after.is_empty() {
                    spans.push(Span::styled(
                        after,
                        Style::default().fg(INPUT_FG).bg(style_bg),
                    ));
                }
            } else {
                spans.push(Span::styled(
                    line_text.clone(),
                    Style::default().fg(INPUT_FG).bg(style_bg),
                ));
            }
            write_line!(row, Line::from(spans));
            row += 1;
        }
    }

    // Hint row
    row += 1;
    if row < inner.y + inner.height {
        let hint = if state.options.is_some() {
            "  type: custom   ↑↓/Tab: options   Enter: confirm   Esc: skip"
        } else {
            "  Type answer, then Enter to confirm   Esc: skip"
        };
        write_line!(
            row,
            Line::from(Span::styled(
                hint,
                Style::default().fg(HINT_FG).bg(CLAWDE_PANEL_BG)
            ))
        );
    }

    let _ = row;
}

// ---------------------------------------------------------------------------
// Word-wrap helper
// ---------------------------------------------------------------------------

fn word_wrap(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in paragraph.split_whitespace() {
            if current.is_empty() {
                current.push_str(word);
            } else if current.len() + 1 + word.len() <= max_width {
                current.push(' ');
                current.push_str(word);
            } else {
                lines.push(current.clone());
                current = word.to_string();
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
    }
    lines
}

#[cfg(test)]
mod overflow_tests {
    use super::*;

    /// Regression: a long question in a short terminal used to `return` out of
    /// the question loop, so options and the custom-answer row never rendered
    /// and the dialog was unanswerable. The question must now clip with an
    /// ellipsis while the tail rows survive.
    #[test]
    fn long_question_never_evicts_options_and_custom_row() {
        let mut state = AskUserDialogState::new();
        state.visible = true;
        state.question = "Explain in exhaustive detail ".repeat(30); // far beyond viewport
        state.options = Some(vec![
            "first option".to_string(),
            "second option".to_string(),
            "third option".to_string(),
        ]);

        let area = Rect::new(0, 0, 58, 12); // small terminal: question cannot fit
        let mut buf = ratatui::buffer::Buffer::empty(area);
        render_ask_user_dialog(&state, area, &mut buf);

        let text: String = buf.content.iter().map(|c| c.symbol().to_string()).collect();
        for opt in state.options.as_ref().unwrap().iter() {
            assert!(
                text.contains(opt.as_str()),
                "option '{}' evicted by long question",
                opt
            );
        }
        // The custom-answer prompt must also survive.
        assert!(
            text.contains("type to fill custom answer"),
            "custom-answer row evicted by long question"
        );
        // The question is clipped with an ellipsis marker, not silently cut.
        assert!(text.contains('\u{2026}'), "clipped question lacks ellipsis");
    }

    fn render_rows(state: &AskUserDialogState, area: Rect) -> Vec<String> {
        let mut buf = ratatui::buffer::Buffer::empty(area);
        render_ask_user_dialog(state, area, &mut buf);
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn write_in_wraps_and_grows_the_dialog() {
        let mut state = AskUserDialogState::new();
        state.visible = true;
        state.question = "Pick one or type your own".to_string();
        state.options = Some(vec!["one".to_string(), "two".to_string()]);
        state.custom_text =
            "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron"
                .to_string();
        state.custom_cursor = state.custom_text.len();
        state.in_custom_input = true;

        let area = Rect::new(0, 0, 60, 40);
        let rows = render_rows(&state, area);
        let first = rows
            .iter()
            .position(|r| r.contains("alpha"))
            .expect("first word on screen");
        let last = rows
            .iter()
            .position(|r| r.contains("omicron"))
            .expect("wrapped tail on screen");
        assert!(
            last > first,
            "a long write-in must wrap onto a later row, not clip"
        );
        assert!(
            rows[last].contains('\u{2588}'),
            "the block cursor should sit on the final wrapped row"
        );
    }

    #[test]
    fn wrapped_input_maps_cursor_into_wrapped_layout() {
        let text = "alpha beta gamma delta epsilon zeta";
        let start = WrappedInput::layout(text, 0, 12);
        assert!(start.lines.len() > 1, "narrow width must wrap");
        assert_eq!((start.cursor_row, start.cursor_col), (0, 0));

        let end = WrappedInput::layout(text, text.len(), 12);
        assert_eq!(end.cursor_row, end.lines.len() - 1);
        assert_eq!(
            end.cursor_col,
            end.lines.last().unwrap().chars().count(),
            "cursor at end maps to the last row's full width"
        );
    }

    #[test]
    fn write_in_cursor_edits_at_insertion_point() {
        let mut s = AskUserDialogState::new();
        s.visible = true;
        s.options = None;
        for c in "helo".chars() {
            s.push_char(c);
        }
        s.move_cursor_left(); // between 'l' and 'o'
        s.push_char('l'); // -> "hello"
        assert_eq!(s.custom_text, "hello");
        assert_eq!(s.custom_cursor, 4);

        s.move_cursor_home();
        s.push_char('>'); // -> ">hello"
        assert_eq!(s.custom_text, ">hello");

        s.move_cursor_end();
        s.delete_char(); // at the end: no-op
        assert_eq!(s.custom_text, ">hello");

        s.move_cursor_home();
        s.delete_char(); // delete the leading '>'
        assert_eq!(s.custom_text, "hello");

        s.move_cursor_end();
        s.pop_char();
        assert_eq!(s.custom_text, "hell");
    }
}
