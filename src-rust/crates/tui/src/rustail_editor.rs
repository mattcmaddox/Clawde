// rustail_editor.rs — In-TUI pixel editor for the Rustail mascot animation.
//
// Opened with `/rustail`. Edits the animation frames on their 8×29 grid with a
// keyboard glyph palette, then writes `FRAMES`, `FRAME_DURATIONS_MS` and
// `CYCLE_MS` back into `rustail.rs` so a rebuild picks up the changes.
//
// Keys: hjkl / arrows move · space paint · backspace erase · qwertasdfgzxcv
// select glyph · tab cycle · u undo · = add frame · - remove · 1-9 go to
// frame · shift+1-9 set duration in seconds · ctrl+d duplicate · enter save
// & close · esc close.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::preset_store;
use crate::rustail;
use crate::theme_colors::current_palette;

/// The mascot canvas is fixed at 8 rows × 29 cols (matches the renderer).
pub const GRID_ROWS: u16 = 8;
pub const GRID_COLS: u16 = 29;

/// The 14 documented block glyphs, in the studio's documented order.
pub const GLYPHS: [char; 14] = [
    '▄', '▛', '▜', '▟', '▙', '█', '▔', '▌', '▗', '▖', '▐', '▝', '▘', '▚',
];

/// Keyboard letters bound to each glyph: q w e r t a s d f g z x c v.
pub const GLYPH_KEYS: [char; 14] = [
    'q', 'w', 'e', 'r', 't', 'a', 's', 'd', 'f', 'g', 'z', 'x', 'c', 'v',
];

/// One editable animation frame.
#[derive(Debug, Clone)]
pub struct EditableFrame {
    /// GRID_ROWS × GRID_COLS of chars, `' '` for empty cells.
    pub rows: Vec<Vec<char>>,
    pub dur_ms: u64,
}

impl EditableFrame {
    fn blank() -> Self {
        Self {
            rows: vec![vec![' '; GRID_COLS as usize]; GRID_ROWS as usize],
            dur_ms: 1000,
        }
    }

    fn from_owned((strings, dur_ms): (Vec<String>, u64)) -> Self {
        let rows = strings
            .into_iter()
            .map(|s| s.chars().take(GRID_COLS as usize).collect())
            .collect();
        Self { rows, dur_ms }
    }

    /// Rows back into the 29-char string form rustail.rs expects.
    fn to_rows(&self) -> Vec<String> {
        self.rows
            .iter()
            .map(|row| row.iter().collect::<String>())
            .collect()
    }
}

/// Undo entry: a full snapshot of one frame's grid.
#[derive(Debug, Clone)]
struct UndoEntry {
    frame: usize,
    rows: Vec<Vec<char>>,
}

/// Actions the editor can request from the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RustailEditAction {
    /// The frames were written back to rustail.rs.
    Saved,
}

pub struct RustailEditor {
    pub visible: bool,
    pub frames: Vec<EditableFrame>,
    pub selected: usize,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub palette: usize,
    pub notice: Option<String>,
    pub dirty: bool,
    pub cursor_visible: bool,
    /// Esc-press tracking for discarding unsaved changes.
    pub confirm_discard: bool,
    /// Minus-press tracking for removing the current frame.
    pub confirm_remove: bool,
    /// Confirmation flag for preset deletion (separate from frame removal).
    pub confirm_delete_preset: bool,
    /// Named animation preset currently being edited.
    pub preset_name: String,
    undo_stack: Vec<UndoEntry>,
    /// Blink-phase counter so the grid cursor pulses at ~1 Hz.
    blink_frame: u32,
}

impl RustailEditor {
    pub fn new() -> Self {
        Self {
            visible: false,
            frames: rustail::rustail_frames_owned()
                .into_iter()
                .map(EditableFrame::from_owned)
                .collect(),
            selected: 0,
            cursor_row: 0,
            cursor_col: 0,
            palette: 0,
            notice: None,
            dirty: false,
            confirm_discard: false,
            confirm_remove: false,
            confirm_delete_preset: false,
            preset_name: "default".into(),
            undo_stack: Vec::new(),
            cursor_visible: true,
            blink_frame: 0,
        }
    }

    /// Reload the frames from the active preset and show the editor.
    pub fn open(&mut self) {
        preset_store::ensure_seed();
        let name = preset_store::active_preset();
        self.load_preset(&name);
        self.visible = true;
    }

    /// Load a named preset into the editor (without changing the active
    /// marker — that only happens on save).
    fn load_preset(&mut self, name: &str) {
        self.frames = load_preset_frames(name);
        self.preset_name = name.to_string();
        self.selected = 0;
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.palette = 0;
        self.notice = None;
        self.dirty = false;
        self.confirm_discard = false;
        self.confirm_remove = false;
        self.confirm_delete_preset = false;
        self.undo_stack.clear();
        self.cursor_visible = true;
        self.blink_frame = 0;
    }

    pub fn close(&mut self) {
        self.visible = false;
    }

    fn push_undo(&mut self) {
        if let Some(f) = self.frames.get(self.selected) {
            self.undo_stack.push(UndoEntry {
                frame: self.selected,
                rows: f.rows.clone(),
            });
            if self.undo_stack.len() > 60 {
                self.undo_stack.remove(0);
            }
        }
    }

    fn undo(&mut self) {
        if let Some(entry) = self.undo_stack.pop() {
            if let Some(f) = self.frames.get_mut(entry.frame) {
                f.rows = entry.rows;
            }
            self.dirty = true;
        } else {
            self.notice = Some("Nothing to undo.".into());
        }
    }

    fn move_cursor(&mut self, dr: i16, dc: i16) {
        let r = self.cursor_row as i16 + dr;
        let c = self.cursor_col as i16 + dc;
        self.cursor_row = r.clamp(0, GRID_ROWS as i16 - 1) as u16;
        self.cursor_col = c.clamp(0, GRID_COLS as i16 - 1) as u16;
    }

    fn paint(&mut self) {
        self.push_undo();
        let (r, c) = (self.cursor_row as usize, self.cursor_col as usize);
        let glyph = GLYPHS[self.palette];
        if let Some(f) = self.current_frame_mut() {
            f.rows[r][c] = glyph;
        }
        self.dirty = true;
    }

    fn erase(&mut self) {
        self.push_undo();
        let (r, c) = (self.cursor_row as usize, self.cursor_col as usize);
        if let Some(f) = self.current_frame_mut() {
            f.rows[r][c] = ' ';
        }
        self.dirty = true;
    }

    fn current_frame_mut(&mut self) -> Option<&mut EditableFrame> {
        self.frames.get_mut(self.selected)
    }

    fn select_glyph(&mut self, idx: usize) {
        self.palette = idx % GLYPHS.len();
    }

    fn cycle_glyph(&mut self, dir: i8) {
        let len = GLYPHS.len() as i16;
        self.palette = ((self.palette as i16 + dir as i16 + len) % len) as usize;
    }

    fn add_frame(&mut self) {
        let idx = self.selected + 1;
        self.frames.insert(idx, EditableFrame::blank());
        self.selected = idx;
        self.dirty = true;
        self.confirm_discard = false;
        self.confirm_remove = false;
        self.confirm_delete_preset = false;
        self.undo_stack.clear();
    }

    fn duplicate_frame(&mut self) {
        let idx = self.selected + 1;
        if let Some(template) = self.frames.get(self.selected) {
            self.frames.insert(idx, template.clone());
            self.selected = idx;
            self.dirty = true;
            self.confirm_discard = false;
            self.confirm_remove = false;
            self.confirm_delete_preset = false;
            self.undo_stack.clear();
        }
    }

    fn remove_frame(&mut self) {
        if self.frames.len() <= 1 {
            self.notice = Some("Can't remove the last frame.".into());
            self.confirm_remove = false;
            self.confirm_delete_preset = false;
            return;
        }
        // Blank frames (no glyphs) are safe to remove instantly.
        let blank = self
            .frames
            .get(self.selected)
            .map(|f| f.rows.iter().all(|row| row.iter().all(|&c| c == ' ')))
            .unwrap_or(true);
        if !blank && !self.confirm_remove {
            self.confirm_remove = true;
            let idx = self.selected + 1;
            self.notice = Some(format!(
                "Remove frame {idx}? Press - again to confirm, any other key to cancel."
            ));
            return;
        }
        self.confirm_remove = false;
        self.confirm_delete_preset = false;
        self.frames.remove(self.selected);
        if self.selected >= self.frames.len() {
            self.selected = self.frames.len() - 1;
        }
        self.cursor_row = self.cursor_row.min(GRID_ROWS - 1);
        self.cursor_col = self.cursor_col.min(GRID_COLS - 1);
        self.dirty = true;
        self.confirm_discard = false;
        self.undo_stack.clear();
    }

    fn goto_frame(&mut self, idx: usize) {
        if idx < self.frames.len() {
            self.selected = idx;
        }
    }

    fn set_duration_seconds(&mut self, secs: u64) {
        let ms = if secs == 0 {
            10_000
        } else {
            (secs * 1000).clamp(100, 10_000)
        };
        if let Some(f) = self.current_frame_mut() {
            f.dur_ms = ms;
        }
        self.dirty = true;
        self.confirm_discard = false;
    }

    fn current_dur_seconds(&self) -> f64 {
        self.frames
            .get(self.selected)
            .map(|f| f.dur_ms as f64 / 1000.0)
            .unwrap_or(0.0)
    }

    /// Advance the blink-phase counter (call every repaint frame while
    /// visible so the grid cursor pulses at ~1 Hz).
    pub fn tick_blink(&mut self) {
        self.blink_frame = self.blink_frame.wrapping_add(1);
        self.cursor_visible = (self.blink_frame / 30).is_multiple_of(2);
    }

    /// Write the edited frames back into rustail.rs AND the preset store.
    pub fn save(&mut self) -> Result<(), String> {
        // Persist to the preset store first.
        let raw: Vec<(Vec<String>, u64)> = self
            .frames
            .iter()
            .map(|f| (f.to_rows(), f.dur_ms))
            .collect();
        preset_store::save_preset(&self.preset_name, &raw)?;
        preset_store::set_active(&self.preset_name);
        // Then write consts into rustail.rs.
        let path = rustail_source_path();
        let source = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
        let frames_text = build_frames_const(&self.frames);
        let dur_line = build_durations_line(&self.frames);
        let cycle_block = build_cycle_block(self.frames.len());
        let updated = replace_consts(&source, &frames_text, &dur_line, &cycle_block)?;
        std::fs::write(&path, updated)
            .map_err(|e| format!("cannot write {}: {}", path.display(), e))?;
        // Keep the Rustail Animation Studio (tools/logo-editor.html) in sync so
        // the `durations_match_rustail_studio` guard test stays green and the
        // studio preview matches the TUI animation.
        if let Err(e) = sync_studio_html(&self.frames) {
            // Best-effort: the studio is a dev tool, so a failed sync must not
            // fail the save (and lose the user's edits).
            eprintln!("rustail_editor: studio sync failed: {e}");
        }
        self.dirty = false;
        Ok(())
    }

    // ---- preset CRUD ----------------------------------------------------

    fn new_preset(&mut self) {
        // Fork a numbered preset name.
        let existing = preset_store::list_presets();
        let mut n = 1u32;
        loop {
            let candidate = format!("custom-{n}");
            if !existing.contains(&candidate) {
                self.preset_name = candidate;
                break;
            }
            n += 1;
        }
        self.confirm_discard = false;
        self.notice = Some(format!(
            "New preset \"{}\" — save to keep it.",
            self.preset_name
        ));
    }

    fn cycle_preset(&mut self) {
        let names = preset_store::list_presets();
        if names.len() < 2 {
            self.notice = Some("Only one preset exists. Use ctrl+n to create another.".into());
            return;
        }
        if self.dirty && !self.confirm_discard {
            self.confirm_discard = true;
            self.notice =
                Some("Unsaved changes — press ctrl+o again to discard and switch.".into());
            return;
        }
        self.confirm_discard = false;
        let cur_pos = names
            .iter()
            .position(|n| n == &self.preset_name)
            .unwrap_or(0);
        let next = (cur_pos + 1) % names.len();
        self.load_preset(&names[next]);
    }

    fn delete_preset(&mut self) {
        let names = preset_store::list_presets();
        if names.len() <= 1 {
            self.notice = Some("Can't delete the only preset.".into());
            self.confirm_delete_preset = false;
            return;
        }
        if !self.confirm_delete_preset {
            self.confirm_delete_preset = true;
            self.notice = Some(format!(
                "Delete preset \"{}\"? Press ctrl+shift+d again to confirm.\n(any other key cancels)",
                self.preset_name
            ));
            return;
        }
        self.confirm_delete_preset = false;
        let deleted = self.preset_name.clone();
        preset_store::delete_preset(&deleted);
        // Switch to the first remaining preset.
        let names = preset_store::list_presets();
        self.load_preset(&names[0]);
        self.notice = Some(format!(
            "Deleted \"{deleted}\". Now editing \"{}\".",
            self.preset_name
        ));
    }
}

impl Default for RustailEditor {
    fn default() -> Self {
        Self::new()
    }
}

/// Load frames from the preset store, falling back to rustail.rs built-ins.
fn load_preset_frames(name: &str) -> Vec<EditableFrame> {
    if let Some(frames) = preset_store::load_preset(name) {
        frames.into_iter().map(EditableFrame::from_owned).collect()
    } else {
        rustail::rustail_frames_owned()
            .into_iter()
            .map(EditableFrame::from_owned)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// rustail.rs const regeneration (pure + unit-testable)
// ---------------------------------------------------------------------------

/// The path to the crate's own `rustail.rs` — the editor rewrites its source.
fn rustail_source_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("rustail.rs")
}

/// The path to the Rustail Animation Studio HTML — the editor keeps its
/// `DEFAULT_FRAMES` durations in sync so the `durations_match_rustail_studio`
/// guard test (which `include_str!`s the studio) stays green.
fn studio_html_path() -> PathBuf {
    // crates/tui/src → ../../../../tools/logo-editor.html (repo root).
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../../tools/logo-editor.html")
}

/// Generate the full `const FRAMES: [[&str; 8]; N] = [...]` block.
fn build_frames_const(frames: &[EditableFrame]) -> String {
    let n = frames.len();
    let mut out = format!("const FRAMES: [[&str; 8]; {n}] = [");
    for (i, f) in frames.iter().enumerate() {
        out.push_str(&format!("\n    // Frame {i}\n    ["));
        for row in f.to_rows() {
            out.push_str(&format!("\n        \"{row}\","));
        }
        out.push_str("\n    ],");
    }
    out.push_str("\n];");
    out
}

/// Generate the `const FRAME_DURATIONS_MS` declaration line.
fn build_durations_line(frames: &[EditableFrame]) -> String {
    let vals: Vec<String> = frames.iter().map(|f| f.dur_ms.to_string()).collect();
    format!(
        "const FRAME_DURATIONS_MS: [u64; FRAMES.len()] = [{}];",
        vals.join(", ")
    )
}

/// Generate the `const CYCLE_MS` block for `n` frames (hand-written sum).
fn build_cycle_block(n: usize) -> String {
    let mut out = String::from("const CYCLE_MS: u64 = FRAME_DURATIONS_MS[0]");
    for i in 1..n {
        out.push_str(&format!("\n    + FRAME_DURATIONS_MS[{i}]"));
    }
    out.push(';');
    out
}

/// Replace the three consts inside the current rustail.rs source text.
fn replace_consts(
    source: &str,
    frames_text: &str,
    dur_line: &str,
    cycle_block: &str,
) -> Result<String, String> {
    let fr_start = source
        .find("const FRAMES:")
        .ok_or_else(|| "could not find the FRAMES const in rustail.rs".to_string())?;
    let fr_end = source[fr_start..]
        .find("\n];")
        .map(|i| fr_start + i + 3) // include the closing `;` so it isn't doubled
        .ok_or_else(|| "could not find the end of the FRAMES const".to_string())?;

    let dur_start = source
        .find("const FRAME_DURATIONS_MS:")
        .ok_or_else(|| "could not find FRAME_DURATIONS_MS in rustail.rs".to_string())?;
    let dur_end = source[dur_start..]
        .find('\n')
        .map(|i| dur_start + i)
        .unwrap_or(source.len());

    let cyc_start = source
        .find("const CYCLE_MS: u64 = FRAME_DURATIONS_MS[0]")
        .ok_or_else(|| "could not find the CYCLE_MS const in rustail.rs".to_string())?;
    let cyc_end = source[cyc_start..]
        .find(';')
        .map(|i| cyc_start + i + 1)
        .ok_or_else(|| "could not find the end of CYCLE_MS".to_string())?;

    // Replace from last block to first so length changes in earlier
    // (lexically-later) replacements never shift the start/end offsets of
    // subsequent (lexically-earlier) replacements.
    //
    //   CYCLE_MS (last)  →  FRAME_DURATIONS_MS (middle)  →  FRAMES (first)
    //
    // This relies on the three consts appearing in exactly this order in
    // rustail.rs.  All three markers are structural (`const NAME:` and
    // trailing `;`), so comments between blocks do not affect matching.
    let mut out = source.to_string();
    for (start, end, repl) in [
        (cyc_start, cyc_end, cycle_block),
        (dur_start, dur_end, dur_line),
        (fr_start, fr_end, frames_text),
    ] {
        out.replace_range(start..end, repl);
    }
    Ok(out)
}

/// Update the per-frame `dur:` values in the Rustail Animation Studio HTML to
/// match `frames`, and bump its `STORAGE_VERSION` so the studio discards any
/// stale localStorage copy of DEFAULT_FRAMES.
///
/// The studio marks each frame's duration with a
/// `dur: N, // TUI FRAME_DURATIONS_MS[i]` line; the `durations_match_rustail_studio`
/// test in rustail.rs parses those markers, so this function is what keeps the
/// two sides in sync after a TUI editor save.  Pure string surgery — returns
/// the updated HTML.
pub fn sync_studio_html(frames: &[EditableFrame]) -> Result<(), String> {
    let path = studio_html_path();
    let source = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    // `None` means either no markers (malformed file) or no change (no-op).
    // Only the first case is an error; a no-op sync is silently OK.
    let Some(updated) = rewrite_studio_durations(&source, frames) else {
        // No markers at all — the studio HTML is malformed or has been
        // manually edited.  Still not a hard error (the studio is a dev
        // tool, not a runtime dependency).
        return Ok(());
    };
    std::fs::write(&path, updated)
        .map_err(|e| format!("cannot write {}: {}", path.display(), e))?;
    Ok(())
}

/// Rewrite every `dur: N, // TUI FRAME_DURATIONS_MS[i]` marker line in the
/// studio HTML to the matching duration from `frames`, and bump the
/// `STORAGE_VERSION` constant (defaults to a +1 bump when present).  Returns
/// `None` when the HTML has no marker lines at all (so callers can distinguish
/// "nothing to sync" from a malformed file).
fn rewrite_studio_durations(html: &str, frames: &[EditableFrame]) -> Option<String> {
    let mut out_lines: Vec<String> = Vec::new();
    let mut frame_idx = 0usize;
    let mut changed = false;

    for line in html.lines() {
        if line.contains("TUI FRAME_DURATIONS_MS") {
            let dur = frames
                .get(frame_idx)
                .map(|f| f.dur_ms.to_string())
                .unwrap_or_else(|| line.trim().to_string());
            // `dur: N, // ...` — preserve the leading whitespace and the
            // marker comment, replacing only the numeric value.
            let trimmed = line.trim_start();
            let indent = &line[..line.len() - trimmed.len()];
            if let Some(rest) = trimmed.strip_prefix("dur:") {
                if let Some(comment_idx) = rest.find("//") {
                    let comment = &rest[comment_idx..];
                    // Only count as a change if the value actually differs
                    // (a no-op save must not bump the studio's version and
                    // nuke a user's localStorage snapshot).
                    let new_line = format!("{indent}dur: {dur}, {comment}");
                    if new_line != line {
                        changed = true;
                    }
                    out_lines.push(new_line);
                } else {
                    out_lines.push(line.to_string());
                }
            } else {
                out_lines.push(line.to_string());
            }
            frame_idx += 1;
        } else if line.contains("STORAGE_VERSION =") {
            if changed {
                // Durations moved → bump the studio's STORAGE_VERSION so
                // browsers discard their localStorage snapshot of the old
                // DEFAULT_FRAMES.  Only bumped when a real change happened.
                match bump_storage_version(line) {
                    Some(bumped) => {
                        out_lines.push(bumped);
                    }
                    None => out_lines.push(line.to_string()),
                }
            } else {
                out_lines.push(line.to_string());
            }
        } else {
            out_lines.push(line.to_string());
        }
    }

    if !changed {
        return None;
    }
    let mut out = out_lines.join("\n");
    // .lines() drops the trailing newline; restore it if the source had one.
    if html.ends_with('\n') && !out.ends_with('\n') {
        out.push('\n');
    }
    Some(out)
}

/// Given a `const STORAGE_VERSION = N; ...` line, return it with N incremented
/// and everything after the digits preserved (e.g. the `// bump` comment).
fn bump_storage_version(line: &str) -> Option<String> {
    let idx = line.find("STORAGE_VERSION =")?;
    let rest = &line[idx + "STORAGE_VERSION =".len()..];
    // Allow optional whitespace between `=` and the number.
    let after_eq = rest.trim_start();
    let digits_end = after_eq.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits_end == 0 {
        return None;
    }
    let version: u64 = after_eq[..digits_end].parse().ok()?;
    let suffix = &after_eq[digits_end..];
    Some(format!(
        "{}STORAGE_VERSION = {}{}",
        &line[..idx],
        version + 1,
        suffix
    ))
}

// ---------------------------------------------------------------------------
// Key handling (called from app.rs)
// ---------------------------------------------------------------------------

/// Resolve the numeric value of a digit key, tolerating terminals that report
/// the shifted symbol (`Shift+3` → `'#'`) and those that report the plain
/// digit with a SHIFT modifier.
fn digit_from_key(key: &KeyEvent) -> Option<u8> {
    let ch = match key.code {
        KeyCode::Char(c) => c,
        _ => return None,
    };
    match ch {
        '0'..='9' => Some(ch as u8 - b'0'),
        '!' => Some(1),
        '@' => Some(2),
        '#' => Some(3),
        '$' => Some(4),
        '%' => Some(5),
        '^' => Some(6),
        '&' => Some(7),
        '*' => Some(8),
        '(' => Some(9),
        ')' => Some(0),
        _ => None,
    }
}

/// Returns `Some(RustailEditAction)` when the editor wants the app to react.
pub fn handle_rustail_editor_key(
    screen: &mut RustailEditor,
    key: KeyEvent,
) -> Option<RustailEditAction> {
    if !screen.visible {
        return None;
    }

    // Any non-esc key cancels a pending discard confirmation.
    if key.code != KeyCode::Esc {
        screen.confirm_discard = false;
    }
    // Any non-minus key cancels a pending remove confirmation.
    if !matches!(key.code, KeyCode::Char('-' | '_')) {
        screen.confirm_remove = false;
    }
    // Clear preset-delete confirm on any non-ctrl+shift+d key.
    if key.code != KeyCode::Char('D')
        || !key.modifiers.contains(KeyModifiers::CONTROL)
        || !key.modifiers.contains(KeyModifiers::SHIFT)
    {
        screen.confirm_delete_preset = false;
    }

    match key.code {
        KeyCode::Esc => {
            if screen.dirty && !screen.confirm_discard {
                screen.confirm_discard = true;
                screen.notice =
                    Some("Unsaved changes — press esc again to discard, enter to save.".into());
            } else {
                screen.close();
            }
            None
        }
        KeyCode::Enter => match screen.save() {
            Ok(()) => {
                screen.close();
                Some(RustailEditAction::Saved)
            }
            Err(e) => {
                screen.notice = Some(format!("Save failed: {e}"));
                None
            }
        },
        KeyCode::Left | KeyCode::Char('h') => {
            screen.move_cursor(0, -1);
            None
        }
        KeyCode::Right | KeyCode::Char('l') => {
            screen.move_cursor(0, 1);
            None
        }
        KeyCode::Up | KeyCode::Char('k') => {
            screen.move_cursor(-1, 0);
            None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            screen.move_cursor(1, 0);
            None
        }
        KeyCode::Char(' ') => {
            screen.paint();
            None
        }
        KeyCode::Backspace | KeyCode::Delete => {
            screen.erase();
            None
        }
        KeyCode::Tab => {
            screen.cycle_glyph(1);
            None
        }
        KeyCode::BackTab => {
            screen.cycle_glyph(-1);
            None
        }
        KeyCode::Char(c) => {
            // Ctrl+D duplicates frames — check before glyph mapping since
            // 'd' is also a glyph key.  Exclude SHIFT so Ctrl+Shift+D hits
            // the preset-delete path instead.
            if c == 'd'
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::SHIFT)
            {
                screen.duplicate_frame();
                return None;
            }
            // Preset CRUD binds (ctrl+letter; before glyph mapping).
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                match c {
                    'n' => {
                        screen.new_preset();
                        return None;
                    }
                    'o' => {
                        screen.cycle_preset();
                        return None;
                    }
                    _ => {}
                }
                if c == 'D' && key.modifiers.contains(KeyModifiers::SHIFT) {
                    screen.delete_preset();
                    return None;
                }
            }
            if let Some(gi) = GLYPH_KEYS.iter().position(|&g| g == c) {
                screen.select_glyph(gi);
                return None;
            }
            match c {
                '0'..='9' => {
                    let d = c as u8 - b'0';
                    if key.modifiers.contains(KeyModifiers::SHIFT) {
                        screen.set_duration_seconds(d as u64);
                    } else {
                        // Plain digits select a frame (1-based; 0 → frame 10).
                        let idx = if d == 0 { 9 } else { (d - 1) as usize };
                        screen.goto_frame(idx);
                    }
                }
                // Shifted-digit symbols set the duration in seconds.
                '!' | '@' | '#' | '$' | '%' | '^' | '&' | '*' | '(' | ')' => {
                    let d = digit_from_key(&key).unwrap_or(0);
                    screen.set_duration_seconds(d as u64);
                }
                'u' => screen.undo(),
                '=' | '+' => screen.add_frame(),
                '-' | '_' => screen.remove_frame(),
                _ => {}
            }
            None
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

pub fn render_rustail_editor(frame: &mut Frame, screen: &RustailEditor, area: Rect) {
    if !screen.visible {
        return;
    }
    let p = current_palette();
    let glyph_style = Style::default().fg(p.accent);
    let muted = Style::default().fg(p.disabled);

    // Column width: the 29-col grid + border needs 31; the palette line needs
    // ~42 — use 44 and center the grid inside. Key hints span the full width.
    let panel_w = 44u16;
    let rows: [u16; 5] = [1, 10, 1, 5, 1]; // title, grid block, palette, hints, notice
    let total_h: u16 = rows.iter().sum::<u16>() + 2;
    let x = area.x + area.width.saturating_sub(panel_w) / 2;
    let mut y = area.y + area.height.saturating_sub(total_h) / 2;

    // Title — preset name, frame index and duration stay visible but unobtrusive.
    let title = format!(
        " {} · Frame {}/{} · {:.1}s{} ",
        screen.preset_name,
        screen.selected + 1,
        screen.frames.len(),
        screen.current_dur_seconds(),
        if screen.dirty { " · unsaved" } else { "" },
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            title,
            Style::default()
                .fg(p.text_light)
                .add_modifier(Modifier::BOLD),
        )])),
        Rect::new(x, y, panel_w, 1),
    );
    y += rows[0] + 1;

    // Grid block.
    let grid_area = Rect::new(x, y, panel_w, rows[1]);
    let block = Block::default()
        .borders(Borders::ALL)
        .style(Style::default().fg(p.text_light));
    let inner = block.inner(grid_area);
    frame.render_widget(block, grid_area);
    let grid_x = inner.x + inner.width.saturating_sub(GRID_COLS) / 2;
    let mut lines: Vec<Line> = Vec::new();
    for r in 0..GRID_ROWS as usize {
        let mut spans = Vec::new();
        for c in 0..GRID_COLS as usize {
            let ch = screen.frames[screen.selected].rows[r][c];
            let is_cursor = screen.cursor_visible
                && r == screen.cursor_row as usize
                && c == screen.cursor_col as usize;
            let style = if is_cursor {
                Style::default()
                    .fg(Color::Black)
                    .bg(p.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                glyph_style
            };
            spans.push(Span::styled(ch.to_string(), style));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(grid_x, inner.y, GRID_COLS, GRID_ROWS),
    );
    y += rows[1] + 1;

    // Glyph palette with letter bindings.
    let mut pal_spans: Vec<Span> = Vec::new();
    for (i, glyph) in GLYPHS.iter().enumerate() {
        let active = i == screen.palette;
        let style = if active {
            Style::default()
                .fg(Color::Black)
                .bg(p.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            muted
        };
        pal_spans.push(Span::styled(format!("{}{} ", GLYPH_KEYS[i], glyph), style));
    }
    frame.render_widget(
        Paragraph::new(Line::from(pal_spans)),
        Rect::new(x, y, panel_w, 1),
    );
    y += rows[2] + 1;

    // Key hints (span the full terminal width, centred).
    let hints = [
        " hjkl/arrows move · space paint · backspace erase · tab cycle · u undo",
        " = add frame · - remove frame · ctrl+d duplicate · 1-9 go to frame",
        " shift+1-9 set seconds · enter save & close · esc close (esc esc discards)",
        " safety: 1 - for blanks, - - for art · esc esc to discard unsaved",
        " ctrl+n new preset · ctrl+o switch · ctrl+shift+d delete preset",
    ];
    let hint_lines: Vec<Line> = hints
        .iter()
        .map(|h| Line::from(Span::styled(*h, muted)))
        .collect();
    frame.render_widget(
        Paragraph::new(hint_lines).alignment(ratatui::layout::Alignment::Center),
        Rect::new(area.x, y, area.width, 5),
    );
    y += rows[3] + 1;

    // Transient notice (errors, discard confirm).
    if let Some(notice) = &screen.notice {
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                notice,
                Style::default().fg(p.warning),
            )])),
            Rect::new(x, y, panel_w, 1),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn editor_loads_frames_from_rustail() {
        let editor = RustailEditor::new();
        assert_eq!(editor.frames.len(), 6);
        assert_eq!(editor.frames[0].rows.len(), 8);
        assert_eq!(editor.frames[0].rows[0].len(), 29);
        let durs: Vec<u64> = editor.frames.iter().map(|f| f.dur_ms).collect();
        assert_eq!(durs, [3000, 1500, 2000, 2000, 1500, 1500]);
    }

    #[test]
    fn paint_erase_and_undo() {
        let mut editor = RustailEditor::new();
        editor.visible = true;
        // Move to (1,1) then paint the current glyph.
        handle_rustail_editor_key(&mut editor, key(KeyCode::Char('j')));
        handle_rustail_editor_key(&mut editor, key(KeyCode::Char('l')));
        handle_rustail_editor_key(&mut editor, key(KeyCode::Char(' ')));
        let painted = editor.frames[0].rows[1][1];
        assert_eq!(painted, GLYPHS[0]);
        assert!(editor.dirty);

        // Undo restores the original cell.
        handle_rustail_editor_key(&mut editor, key(KeyCode::Char('u')));
        assert_eq!(editor.frames[0].rows[1][1], ' ');

        // Erase works too.
        handle_rustail_editor_key(&mut editor, key(KeyCode::Char(' ')));
        handle_rustail_editor_key(&mut editor, key(KeyCode::Backspace));
        assert_eq!(editor.frames[0].rows[1][1], ' ');
    }

    #[test]
    fn glyph_keys_select_palette() {
        let mut editor = RustailEditor::new();
        editor.visible = true;
        for (i, letter) in GLYPH_KEYS.iter().enumerate() {
            handle_rustail_editor_key(&mut editor, key(KeyCode::Char(*letter)));
            assert_eq!(editor.palette, i, "letter {letter} should pick glyph {i}");
        }
    }

    #[test]
    fn digits_goto_frame_and_set_duration() {
        let mut editor = RustailEditor::new();
        editor.visible = true;
        // '3' → frame index 2.
        handle_rustail_editor_key(&mut editor, key(KeyCode::Char('3')));
        assert_eq!(editor.selected, 2);
        // Shift+3 as a symbol '#' → 3 seconds.
        handle_rustail_editor_key(&mut editor, key(KeyCode::Char('#')));
        assert_eq!(editor.frames[2].dur_ms, 3000);
        // Shift+3 reported as digit + SHIFT modifier → 3 seconds too.
        handle_rustail_editor_key(
            &mut editor,
            KeyEvent::new(KeyCode::Char('4'), KeyModifiers::SHIFT),
        );
        assert_eq!(editor.frames[2].dur_ms, 4000);
    }

    #[test]
    fn add_and_remove_frame() {
        let mut editor = RustailEditor::new();
        editor.visible = true;
        let before = editor.frames.len();
        handle_rustail_editor_key(&mut editor, key(KeyCode::Char('=')));
        assert_eq!(editor.frames.len(), before + 1);
        // The new frame is inserted after the current one and selected.
        assert_eq!(editor.selected, 1);
        // Blank frame — removes instantly (no confirm needed).
        handle_rustail_editor_key(&mut editor, key(KeyCode::Char('-')));
        assert!(!editor.confirm_remove);
        assert_eq!(editor.frames.len(), before);
        assert_eq!(editor.selected, 1);
    }

    #[test]
    fn remove_confirmation_cancelled_by_other_key() {
        let mut editor = RustailEditor::new();
        editor.visible = true;
        // Frame 0 has artwork → needs confirmation.
        handle_rustail_editor_key(&mut editor, key(KeyCode::Char('-')));
        assert!(editor.confirm_remove);
        // Any other key cancels the confirmation.
        handle_rustail_editor_key(&mut editor, key(KeyCode::Char('l')));
        assert!(!editor.confirm_remove);
        // Now - starts a new confirmation (still warns, doesn't remove).
        let before = editor.frames.len();
        handle_rustail_editor_key(&mut editor, key(KeyCode::Char('-')));
        assert!(editor.confirm_remove);
        assert_eq!(editor.frames.len(), before);
    }

    #[test]
    fn blank_frame_removes_instantly() {
        let mut editor = RustailEditor::new();
        editor.visible = true;
        // Add a blank frame then switch to it.
        handle_rustail_editor_key(&mut editor, key(KeyCode::Char('=')));
        // It's blank — single - removes it instantly, no confirm flag.
        let before = editor.frames.len();
        handle_rustail_editor_key(&mut editor, key(KeyCode::Char('-')));
        assert!(!editor.confirm_remove);
        assert_eq!(editor.frames.len(), before - 1);
    }

    #[test]
    fn cannot_remove_last_frame() {
        let mut editor = RustailEditor::new();
        editor.visible = true;
        // Remove 5 frames (each needs 2 presses: confirm + execute).
        for _ in 0..5 {
            handle_rustail_editor_key(&mut editor, key(KeyCode::Char('-'))); // warn
            handle_rustail_editor_key(&mut editor, key(KeyCode::Char('-'))); // remove
        }
        assert_eq!(editor.frames.len(), 1);
        // First - warns, second - tries to remove last frame, gets blocked.
        handle_rustail_editor_key(&mut editor, key(KeyCode::Char('-')));
        assert_eq!(editor.frames.len(), 1, "first - on last frame warns");
        handle_rustail_editor_key(&mut editor, key(KeyCode::Char('-')));
        assert_eq!(
            editor.frames.len(),
            1,
            "can't remove last frame even after confirm"
        );
        assert!(editor.notice.is_some());
    }

    #[test]
    fn duplicate_frame_copies_grid_and_duration() {
        let mut editor = RustailEditor::new();
        editor.visible = true;
        // Paint something unique so we can recognise the copy.
        handle_rustail_editor_key(&mut editor, key(KeyCode::Char('v'))); // pick ▚ glyph
        handle_rustail_editor_key(&mut editor, key(KeyCode::Char(' '))); // paint at (0,0)
        editor.frames[0].dur_ms = 4200;

        let orig_len = editor.frames.len();
        let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        handle_rustail_editor_key(&mut editor, ctrl_d);

        assert_eq!(editor.frames.len(), orig_len + 1);
        assert_eq!(editor.selected, 1);
        // The copy must match the source frame.
        assert_eq!(editor.frames[1].rows, editor.frames[0].rows);
        assert_eq!(editor.frames[1].dur_ms, 4200);
        // Plain 'd' without ctrl should NOT trigger duplicate.
        let prev_len = editor.frames.len();
        handle_rustail_editor_key(&mut editor, key(KeyCode::Char('d')));
        assert_eq!(editor.frames.len(), prev_len);
    }

    #[test]
    fn esc_requires_confirmation_when_dirty() {
        let mut editor = RustailEditor::new();
        editor.visible = true;
        handle_rustail_editor_key(&mut editor, key(KeyCode::Char(' '))); // make dirty
        assert!(editor.visible);
        handle_rustail_editor_key(&mut editor, key(KeyCode::Esc));
        assert!(editor.visible, "first esc only warns");
        assert!(editor.confirm_discard);
        handle_rustail_editor_key(&mut editor, key(KeyCode::Esc));
        assert!(!editor.visible, "second esc discards and closes");
    }

    #[test]
    fn esc_closes_when_clean() {
        let mut editor = RustailEditor::new();
        editor.visible = true;
        handle_rustail_editor_key(&mut editor, key(KeyCode::Esc));
        assert!(!editor.visible);
    }

    #[test]
    fn frames_const_builder_round_trips() {
        let frames = vec![
            EditableFrame {
                rows: vec![
                    vec!['x'; GRID_COLS as usize],
                    vec![' '; GRID_COLS as usize],
                    vec![' '; GRID_COLS as usize],
                    vec![' '; GRID_COLS as usize],
                    vec![' '; GRID_COLS as usize],
                    vec![' '; GRID_COLS as usize],
                    vec![' '; GRID_COLS as usize],
                    vec![' '; GRID_COLS as usize],
                ],
                dur_ms: 2500,
            },
            EditableFrame::blank(),
        ];
        let text = build_frames_const(&frames);
        assert!(text.starts_with("const FRAMES: [[&str; 8]; 2] = ["));
        assert!(text.contains("\"xxxxxxxxxxxxxxxxxxxxxxxxxxxxx\","));
        assert!(text.ends_with("];"));
        assert_eq!(
            build_durations_line(&frames),
            "const FRAME_DURATIONS_MS: [u64; FRAMES.len()] = [2500, 1000];"
        );
        assert_eq!(
            build_cycle_block(2),
            "const CYCLE_MS: u64 = FRAME_DURATIONS_MS[0]\n    + FRAME_DURATIONS_MS[1];"
        );
    }

    #[test]
    fn replace_consts_swaps_all_three_blocks() {
        let source = "\
const FRAMES: [[&str; 8]; 1] = [
    // Frame 0
    [
        \"                             \",
    ],
];
const FRAME_DURATIONS_MS: [u64; FRAMES.len()] = [1000];
const CYCLE_MS: u64 = FRAME_DURATIONS_MS[0];
";
        let frames_text = "const FRAMES: [[&str; 8]; 2] = [\n    // Frame 0\n    [\n        \"a\",\n    ],\n    // Frame 1\n    [\n        \"b\",\n    ],\n];";
        let dur_line = "const FRAME_DURATIONS_MS: [u64; FRAMES.len()] = [2000, 3000];";
        let cycle_block =
            "const CYCLE_MS: u64 = FRAME_DURATIONS_MS[0]\n    + FRAME_DURATIONS_MS[1];";
        let updated = replace_consts(source, frames_text, dur_line, cycle_block).unwrap();
        assert!(updated.contains(frames_text));
        assert!(updated.contains(dur_line));
        assert!(updated.contains(cycle_block));
        assert!(!updated.contains("const FRAME_DURATIONS_MS: [u64; FRAMES.len()] = [1000];"));
        // The closing brace must not be doubled (regression: `];` → `];;`).
        assert!(
            !updated.contains(";;"),
            "closing semicolon was doubled:\n{updated}"
        );
    }

    #[test]
    fn replace_consts_does_not_double_closing_semicolon() {
        // Source shaped like the real rustail.rs: frames const immediately
        // followed by the durations and cycle consts.
        let source = "\
// header
const FRAMES: [[&str; 8]; 2] = [
    [
        \"a\",
        \"b\",
    ],
    [
        \"c\",
        \"d\",
    ],
];
const FRAME_DURATIONS_MS: [u64; FRAMES.len()] = [1000, 1000];
const CYCLE_MS: u64 = FRAME_DURATIONS_MS[0]
    + FRAME_DURATIONS_MS[1];
// tail
";
        let frames_text = build_frames_const(&[EditableFrame::blank(), EditableFrame::blank()]);
        let dur_line = "const FRAME_DURATIONS_MS: [u64; FRAMES.len()] = [500, 700];";
        let cycle_block = build_cycle_block(2);
        let updated = replace_consts(source, &frames_text, dur_line, &cycle_block).unwrap();
        assert!(
            !updated.contains(";;"),
            "no double semicolon expected:\n{updated}"
        );
        assert!(updated.contains("\n// tail"));
        assert!(updated.contains("= [500, 700];"));
    }

    #[test]
    fn cursor_blink_toggles() {
        let mut editor = RustailEditor::new();
        assert!(editor.cursor_visible);
        // Advance 30 frames → phase 1 (hidden).
        for _ in 0..30 {
            editor.tick_blink();
        }
        assert!(!editor.cursor_visible);
        // Advance 30 more → phase 2 (visible again).
        for _ in 0..30 {
            editor.tick_blink();
        }
        assert!(editor.cursor_visible);
    }

    fn render_editor(screen: &RustailEditor) -> String {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_rustail_editor(frame, screen, frame.area()))
            .unwrap();
        let rendered = terminal.backend().buffer();
        rendered
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    #[test]
    fn editor_renders_baseline_ui() {
        let mut screen = RustailEditor::new();
        screen.open();
        let content = render_editor(&screen);
        assert!(content.contains("default"), "title shows preset name");
        assert!(
            content.contains("Frame 1/6"),
            "title shows frame and duration"
        );
        assert!(content.contains(GLYPHS[0]));
        assert!(content.contains("enter save"));
        assert!(content.contains("safety:"));
    }

    #[test]
    fn editor_renders_confirm_on_non_blank_frame() {
        let mut screen = RustailEditor::new();
        screen.open();
        // Frame 0 has artwork — pressing - shows a confirmation notice.
        handle_rustail_editor_key(&mut screen, key(KeyCode::Char('-')));
        assert!(screen.confirm_remove);
        let content = render_editor(&screen);
        assert!(
            content.contains("Remove frame 1?"),
            "confirm notice must appear for non-blank frames"
        );
        assert!(content.contains("again to confirm"));
    }

    #[test]
    fn editor_renders_no_confirm_on_blank_frame() {
        let mut screen = RustailEditor::new();
        screen.open();
        // Add a blank frame and switch to it.
        handle_rustail_editor_key(&mut screen, key(KeyCode::Char('=')));
        // Press - — blank frame removes instantly, no confirm notice.
        let before = screen.frames.len();
        handle_rustail_editor_key(&mut screen, key(KeyCode::Char('-')));
        assert!(!screen.confirm_remove);
        assert_eq!(screen.frames.len(), before - 1);
        let content = render_editor(&screen);
        assert!(
            !content.contains("Remove frame"),
            "blank frame must not show confirm notice"
        );
        assert!(content.contains("Frame 2/6"), "title updates after removal");
    }

    #[test]
    fn rewrite_studio_durations_updates_markers_and_bumps_version() {
        let html = [
            "const DEFAULT_FRAMES = [",
            "  { // Frame 1",
            "    dur: 3000, // TUI FRAME_DURATIONS_MS[0]",
            "  },",
            "  { // Frame 2",
            "    dur: 1500, // TUI FRAME_DURATIONS_MS[1]",
            "  },",
            "];",
            "const STORAGE_VERSION = 5;  // bump when DEFAULT_FRAMES changes",
        ]
        .join("\n");
        let frames = vec![
            EditableFrame {
                rows: vec![],
                dur_ms: 2500,
            },
            EditableFrame {
                rows: vec![],
                dur_ms: 700,
            },
        ];
        let out = rewrite_studio_durations(&html, &frames).expect("markers present");
        assert!(out.contains("dur: 2500, // TUI FRAME_DURATIONS_MS[0]"));
        assert!(out.contains("dur: 700, // TUI FRAME_DURATIONS_MS[1]"));
        // The studio must discard stale localStorage after a DEFAULT_FRAMES change.
        assert!(
            out.contains("const STORAGE_VERSION = 6;"),
            "STORAGE_VERSION must be bumped so the studio drops stale frames"
        );
        // No marker lines → nothing to sync.
        assert!(rewrite_studio_durations("<html></html>", &frames).is_none());
    }

    #[test]
    fn bump_storage_version_increments_digits() {
        let line = "const STORAGE_VERSION = 5;  // bump when DEFAULT_FRAMES changes";
        let out = bump_storage_version(line).expect("version present");
        assert!(out.contains("const STORAGE_VERSION = 6;"), "got: {out}");
        assert!(
            out.contains("// bump when DEFAULT_FRAMES changes"),
            "trailing comment must survive: {out}"
        );
        assert!(bump_storage_version("no version here").is_none());
    }
}
