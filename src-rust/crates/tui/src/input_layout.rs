// input_layout.rs — Shared wrapped-input layout for the TUI's typed-input dialogs.
//
// A single-line input that never wraps clips long values (base URLs, API keys,
// free-text elicitation answers). This helper wraps a value to a column width,
// preferring to break at spaces and hard-breaking an over-long word, and maps a
// byte cursor into the resulting (row, column) so a renderer can draw an
// insertion cursor on the right wrapped row.
//
// Used by `ask_user_dialog` (write-in row), `custom_provider_dialog`,
// `key_input_dialog`, and `elicitation_dialog`.

/// A text field laid out for display: the text wrapped to `width` columns
/// (breaking preferentially at spaces, hard-breaking an over-long word), plus
/// the cursor's row and display column in that layout.
///
/// `cursor` is a byte offset into `text`; the returned `cursor_row`/`cursor_col`
/// locate it inside `lines` so the renderer can draw the block cursor in the
/// right row of the wrapped text.
pub(crate) struct WrappedInput {
    pub(crate) lines: Vec<String>,
    pub(crate) cursor_row: usize,
    pub(crate) cursor_col: usize,
}

impl WrappedInput {
    pub(crate) fn layout(text: &str, cursor: usize, width: usize) -> Self {
        use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
        let width = width.max(1);
        let cursor = cursor.min(text.len());
        let chars: Vec<(usize, char)> = text.char_indices().collect();
        if chars.is_empty() {
            return Self {
                lines: vec![String::new()],
                cursor_row: 0,
                cursor_col: 0,
            };
        }

        let mut lines: Vec<String> = Vec::new();
        let mut starts: Vec<usize> = Vec::new();
        let mut i = 0usize;
        while i < chars.len() {
            let start_i = i;
            let start_byte = chars[i].0;
            let mut w = 0usize;
            let mut last_space: Option<usize> = None;
            while i < chars.len() {
                let (_, c) = chars[i];
                let cw = UnicodeWidthChar::width(c).unwrap_or(0);
                if w > 0 && w + cw > width {
                    break;
                }
                if c == ' ' {
                    last_space = Some(i);
                }
                w += cw;
                i += 1;
            }
            // Prefer a word break: if we ran out of room mid-text and a space
            // is available on this line, end the line just after that space so
            // the following word starts on the next row.
            if i < chars.len() {
                if let Some(sp) = last_space {
                    if sp >= start_i {
                        i = sp + 1;
                    }
                }
            }
            let end_byte = if i < chars.len() {
                chars[i].0
            } else {
                text.len()
            };
            starts.push(start_byte);
            lines.push(text[start_byte..end_byte].to_string());
            if i == start_i {
                // Defensive: never loop forever if a zero-width layout slips
                // through (no character consumed and no space to break on).
                break;
            }
        }

        // Map the byte cursor into (row, column) in the wrapped layout.
        let mut cursor_row = lines.len() - 1;
        let mut cursor_col = UnicodeWidthStr::width(lines.last().map(String::as_str).unwrap_or(""));
        for (r, start) in starts.iter().enumerate() {
            let end = starts.get(r + 1).copied().unwrap_or(text.len());
            if cursor >= *start && cursor < end {
                cursor_row = r;
                let rel = cursor - start; // char boundary within this line
                cursor_col = UnicodeWidthStr::width(&lines[r][..rel]);
                break;
            }
        }

        Self {
            lines,
            cursor_row,
            cursor_col,
        }
    }

    pub(crate) fn height(&self) -> u16 {
        self.lines.len() as u16
    }
}

/// Split `text` at display column `col`, returning `(before, after)`. Used to
/// splice the block cursor into a wrapped line.
pub(crate) fn split_at_display_col(text: &str, col: usize) -> (String, String) {
    use unicode_width::UnicodeWidthChar;
    let mut width = 0usize;
    for (idx, ch) in text.char_indices() {
        if width >= col {
            return (text[..idx].to_string(), text[idx..].to_string());
        }
        width += UnicodeWidthChar::width(ch).unwrap_or(0);
    }
    (text.to_string(), String::new())
}

/// The byte offset of the `n`-th character in `text` (clamped to `text.len()`).
/// Lets a dialog map a char-indexed cursor (used by masked display) back to a
/// byte offset the layout helper understands.
pub(crate) fn byte_offset_of_char(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map(|(i, _)| i)
        .unwrap_or(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_and_maps_cursor() {
        let layout = WrappedInput::layout("hello world foo", 12, 6);
        assert!(layout.lines.len() > 1);
        for line in &layout.lines {
            assert!(line.chars().count() <= 6);
        }
        // Byte 12 is the start of "foo" on the last line.
        assert_eq!(layout.cursor_row, layout.lines.len() - 1);
        assert_eq!(layout.cursor_col, 0);
    }

    #[test]
    fn empty_text_has_one_line() {
        let layout = WrappedInput::layout("", 0, 10);
        assert_eq!(layout.height(), 1);
        assert_eq!(layout.cursor_col, 0);
    }

    #[test]
    fn split_maps_display_column() {
        let (before, after) = split_at_display_col("abcdef", 3);
        assert_eq!(before, "abc");
        assert_eq!(after, "def");
    }

    #[test]
    fn byte_offset_of_char_clamps() {
        assert_eq!(byte_offset_of_char("abc", 1), 1);
        assert_eq!(byte_offset_of_char("abc", 99), 3);
        assert_eq!(byte_offset_of_char("é", 1), 2);
    }
}
