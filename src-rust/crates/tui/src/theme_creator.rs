// theme_creator.rs — Interactive 256-color ANSI theme creator + CRUD manager.
//
// Opened by /theme create (the bare /theme command keeps the quick-pick
// popup). Two modes:
//   - List: browse built-in + custom themes (scrollable with a scrollbar),
//     apply, create (n), edit (e), delete (d, with confirm).
//   - Editor: name the theme, then pick each of the 17 palette slots from the
//     full ANSI 256-color grid. Ctrl+S saves to ~/.clawde/themes/<name>.json.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::overlays::{
    begin_modal_frame, modal_header_line_area, render_modal_title_frame, render_scrollbar,
};
use crate::theme_colors::{
    current_palette, delete_theme, list_custom_themes, save_theme, valid_theme_name, ColorPalette,
};
use crate::theme_screen::ThemeOption;
use unicode_width::UnicodeWidthStr;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The 17 palette slots a theme defines, in editor display order.
pub const SLOT_NAMES: &[&str] = &[
    "error",
    "success",
    "warning",
    "info",
    "action",
    "disabled",
    "accent",
    "secondary_accent",
    "panel_bg",
    "text_light",
    "text_dark",
    "border",
    "model_name",
    "hint",
    "effort",
    "routing",
    "vim_hint",
];

/// Built-in theme names that cannot be overwritten by a custom save.
const BUILTIN_THEME_NAMES: &[&str] = &[
    "default",
    "dark",
    "light",
    "solarized",
    "nord",
    "dracula",
    "monokai",
    "catppuccin",
    "deuteranopia",
];

/// A single theme row shown in list mode.
#[derive(Debug, Clone)]
pub struct ThemeEntry {
    pub name: String,
    pub label: String,
    pub description: String,
    pub custom: bool,
    /// A few representative colours used for the swatch preview.
    pub swatch: [Color; 4],
}

/// Editor focus target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorFocus {
    /// The theme-name text field.
    Name,
    /// The list of 17 palette slots.
    Slots,
    /// The 16x16 ANSI 256-color grid.
    Grid,
}

/// Editor state while creating / editing a theme.
#[derive(Debug, Clone)]
pub struct EditorState {
    pub name: String,
    pub palette: ColorPalette,
    pub focus: EditorFocus,
    /// Selected palette slot index (into `SLOT_NAMES`).
    pub slot: usize,
    /// Selected 256-color grid cell (0..255).
    pub grid: usize,
    /// Set when editing an existing custom theme (used for rename cleanup).
    pub original_name: Option<String>,
    /// The slot most recently assigned from the grid (highlighted in the slot
    /// list and the palette preview strip).
    pub last_assigned: Option<usize>,
    /// Palette states for undo (`u`), most recent first, capped at
    /// `UNDO_DEPTH`.
    pub palette_history: Vec<ColorPalette>,
}

/// Full creator state.
#[derive(Debug)]
pub struct ThemeCreator {
    pub visible: bool,
    /// List mode entries (built-in + custom).
    pub themes: Vec<ThemeEntry>,
    pub selected: usize,
    /// First theme index visible in the list viewport (for the scrollbar).
    pub scroll_offset: usize,
    /// Editor mode state (None = list mode).
    pub editor: Option<EditorState>,
    /// Transient status / error line shown in the footer.
    pub notice: Option<String>,
    /// Delete confirmation pending for the selected custom theme.
    pub confirm_delete: bool,
    /// Editor position (grid cell + highlighted slot) remembered from the
    /// last edit session, so the next theme edit resumes where it left off.
    pub last_grid: usize,
    pub last_slot: usize,
    /// Name typed in the last edit session — pre-filled into the next new
    /// theme so variants are quick to create.
    pub last_name: String,
    /// Palette-sequence state: plain `r` picks a fresh random start offset;
    /// Shift+r rolls to the next offset so each palette is guaranteed to
    /// differ from the previous one.
    pub random_start: Option<usize>,
}

/// Maximum number of theme rows shown in the list before it scrolls.
const LIST_VIEWPORT: usize = 12;

/// Step size for Shift-accelerated list navigation (half the viewport).
const LIST_FAST_STEP: usize = LIST_VIEWPORT / 2;

impl ThemeCreator {
    pub fn new() -> Self {
        let mut s = Self {
            visible: false,
            themes: Vec::new(),
            selected: 0,
            scroll_offset: 0,
            editor: None,
            notice: None,
            confirm_delete: false,
            // First hue-ordered cell (dark red), not the buried gray ramp.
            last_grid: hue_order()[0] as usize,
            last_slot: 0,
            last_name: String::new(),
            random_start: None,
        };
        s.refresh_themes();
        s
    }

    pub fn open(&mut self, current_theme: &str) {
        self.visible = true;
        self.editor = None;
        self.confirm_delete = false;
        self.notice = None;
        self.scroll_offset = 0;
        self.refresh_themes();
        if let Some(idx) = self.themes.iter().position(|t| t.name == current_theme) {
            self.selected = idx;
            self.ensure_visible();
        } else {
            self.selected = 0;
        }
    }

    pub fn close(&mut self) {
        self.visible = false;
        self.editor = None;
        self.confirm_delete = false;
    }

    /// Rebuild the list of themes from built-ins + custom files on disk.
    pub fn refresh_themes(&mut self) {
        self.themes = build_theme_entries();
        if self.selected >= self.themes.len() {
            self.selected = self.themes.len().saturating_sub(1);
        }
        self.scroll_offset = self
            .scroll_offset
            .min(self.themes.len().saturating_sub(LIST_VIEWPORT));
    }

    /// Keep the selected row within the scrolling viewport.
    fn ensure_visible(&mut self) {
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + LIST_VIEWPORT {
            self.scroll_offset = self.selected - LIST_VIEWPORT + 1;
        }
    }

    fn select_prev(&mut self) {
        self.select_by(-1);
    }

    fn select_next(&mut self) {
        self.select_by(1);
    }

    /// Skip `LIST_FAST_STEP` items up (Shift+k/h), wrapping around the list.
    fn select_prev_fast(&mut self) {
        self.select_by(-(LIST_FAST_STEP as isize));
    }

    /// Skip `LIST_FAST_STEP` items down (Shift+j/l), wrapping around the list.
    fn select_next_fast(&mut self) {
        self.select_by(LIST_FAST_STEP as isize);
    }

    /// Move the selection by `delta` items, wrapping around the list.
    fn select_by(&mut self, delta: isize) {
        let count = self.themes.len();
        if count == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(count as isize) as usize;
        self.ensure_visible();
    }

    /// Name of the currently selected theme in list mode.
    fn selected_name(&self) -> Option<String> {
        self.themes.get(self.selected).map(|t| t.name.clone())
    }

    fn start_new_theme(&mut self) {
        self.confirm_delete = false;
        self.editor = Some(EditorState {
            // Pre-fill the last-used name and resume the grid/slot position so
            // a new theme is a fast variant of the previous one. Focus always
            // starts on the name field — it is a one-shot entry point (n/e
            // open on it) and Tab/Enter leave it for good.
            name: self.last_name.clone(),
            palette: current_palette(),
            focus: EditorFocus::Name,
            slot: self.last_slot,
            grid: self.last_grid,
            original_name: None,
            last_assigned: None,
            palette_history: Vec::new(),
        });
        self.notice = None;
    }

    /// Open the creator directly in the new-theme editor mode (used by the
    /// quick-pick's `n` action via `ThemePickAction::Create`).
    pub fn open_new_theme(&mut self) {
        self.visible = true;
        self.editor = None;
        self.confirm_delete = false;
        self.notice = None;
        self.refresh_themes();
        self.start_new_theme();
    }

    /// The palette being edited, if the creator is in editor mode. Lets the
    /// renderer theme the whole UI (creator modal included) with the
    /// work-in-progress palette so colour assignments preview live.
    pub fn editor_palette(&self) -> Option<ColorPalette> {
        self.editor.as_ref().map(|e| e.palette)
    }

    fn start_edit_theme(&mut self, name: &str) {
        self.confirm_delete = false;
        let palette = ColorPalette::for_theme(name);
        self.editor = Some(EditorState {
            name: name.to_string(),
            palette,
            // Renaming is the first action when editing: focus starts on the
            // name field (a one-shot entry point, never revisited once left),
            // while the grid cell and slot resume from the last session.
            focus: EditorFocus::Name,
            slot: self.last_slot,
            grid: self.last_grid,
            original_name: Some(name.to_string()),
            last_assigned: None,
            palette_history: Vec::new(),
        });
        self.notice = None;
    }
}

impl Default for ThemeCreator {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Theme list construction
// ---------------------------------------------------------------------------

fn build_theme_entries() -> Vec<ThemeEntry> {
    let mut entries: Vec<ThemeEntry> = crate::theme_screen::builtin_themes()
        .into_iter()
        .map(theme_option_to_entry)
        .collect();
    for name in list_custom_themes() {
        // Skip custom names that collide with a built-in so a hand-placed
        // file (e.g. ~/.clawde/themes/dark.json) isn't listed twice.
        if entries.iter().any(|t| t.name == name) {
            continue;
        }
        let pal = ColorPalette::for_theme(&name);
        entries.push(ThemeEntry {
            name: name.clone(),
            label: name.clone(),
            description: format!("Custom theme — ~/.clawde/themes/{}.json", name),
            custom: true,
            swatch: [pal.panel_bg, pal.accent, pal.success, pal.text_light],
        });
    }
    entries
}

fn theme_option_to_entry(opt: ThemeOption) -> ThemeEntry {
    ThemeEntry {
        name: opt.name,
        label: opt.label,
        description: opt.description,
        custom: opt.custom,
        swatch: opt.swatch,
    }
}

/// Look up the colour for slot `idx` in a palette.
pub fn slot_color(pal: &ColorPalette, idx: usize) -> Color {
    match idx {
        0 => pal.error,
        1 => pal.success,
        2 => pal.warning,
        3 => pal.info,
        4 => pal.action,
        5 => pal.disabled,
        6 => pal.accent,
        7 => pal.secondary_accent,
        8 => pal.panel_bg,
        9 => pal.text_light,
        10 => pal.text_dark,
        11 => pal.border,
        12 => pal.model_name,
        13 => pal.hint,
        14 => pal.effort,
        15 => pal.routing,
        16 => pal.vim_hint,
        _ => pal.vim_hint,
    }
}

/// Assign `c` to slot `idx` in a palette.
fn set_slot_color(pal: &mut ColorPalette, idx: usize, c: Color) {
    match idx {
        0 => pal.error = c,
        1 => pal.success = c,
        2 => pal.warning = c,
        3 => pal.info = c,
        4 => pal.action = c,
        5 => pal.disabled = c,
        6 => pal.accent = c,
        7 => pal.secondary_accent = c,
        8 => pal.panel_bg = c,
        9 => pal.text_light = c,
        10 => pal.text_dark = c,
        11 => pal.border = c,
        12 => pal.model_name = c,
        13 => pal.hint = c,
        14 => pal.effort = c,
        15 => pal.routing = c,
        16 => pal.vim_hint = c,
        _ => pal.vim_hint = c,
    }
}

/// Map an ANSI 256 color index to its RGB value.
fn ansi256_to_rgb(idx: u8) -> (u8, u8, u8) {
    match idx {
        0 => (0, 0, 0),
        1 => (128, 0, 0),
        2 => (0, 128, 0),
        3 => (128, 128, 0),
        4 => (0, 0, 128),
        5 => (128, 0, 128),
        6 => (0, 128, 128),
        7 => (192, 192, 192),
        8 => (128, 128, 128),
        9 => (255, 0, 0),
        10 => (0, 255, 0),
        11 => (255, 255, 0),
        12 => (0, 0, 255),
        13 => (255, 0, 255),
        14 => (0, 255, 255),
        15 => (255, 255, 255),
        16..=231 => {
            let v = idx - 16;
            let r = v / 36;
            let g = (v / 6) % 6;
            let b = v % 6;
            let f = |c: u8| if c == 0 { 0 } else { 55 + c * 40 };
            (f(r), f(g), f(b))
        }
        232..=255 => {
            let v = 8 + (idx - 232) * 10;
            (v, v, v)
        }
    }
}

fn hex_label(idx: u8) -> String {
    let (r, g, b) = ansi256_to_rgb(idx);
    format!("#{:02X}{:02X}{:02X}", r, g, b)
}

/// The 256 ANSI indices laid out in hue order for the 16x16 grid
/// (grid position -> ANSI index). Chromatic colours are sorted by
/// (hue, saturation, lightness) so the grid reads like a rainbow;
/// achromatic colours sort to the end as a grayscale ramp.
static HUE_ORDER: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();

fn hue_order() -> &'static [u8] {
    HUE_ORDER.get_or_init(|| {
        let mut idxs: Vec<u8> = (0..=255).collect();
        idxs.sort_by(|&a, &b| hue_sort_key(a).partial_cmp(&hue_sort_key(b)).unwrap());
        idxs
    })
}

/// Convert an RGB colour to HSL: hue (0..360), saturation, lightness.
/// Achromatic colours get an infinite hue so callers can treat them as a
/// distinct group (e.g. the grayscale ramp at the end of the grid).
fn rgb_to_hsl(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let (r, g, b) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let d = max - min;
    if d == 0.0 {
        (f32::INFINITY, 0.0, l)
    } else {
        let s = d / (1.0 - (2.0 * l - 1.0).abs());
        let h = if max == r {
            (g - b) / d + if g < b { 6.0 } else { 0.0 }
        } else if max == g {
            (b - r) / d + 2.0
        } else {
            (r - g) / d + 4.0
        };
        (h * 60.0, s, l)
    }
}

/// HSL sort key: hue, saturation, lightness. Achromatic colours get an
/// infinite hue so they form a grayscale ramp at the end of the grid.
fn hue_sort_key(idx: u8) -> (f32, f32, f32) {
    let (r, g, b) = ansi256_to_rgb(idx);
    rgb_to_hsl(r, g, b)
}

/// Position (0..255) of ANSI index `idx` in the hue-ordered grid.
fn grid_position_of(idx: usize) -> usize {
    hue_order()
        .iter()
        .position(|&c| c as usize == idx)
        .unwrap_or(idx)
}

/// Move the grid cursor `delta` cells within the hue-ordered 16x16 layout,
/// wrapping around. The returned value is an ANSI index.
fn grid_move(current: usize, delta: isize) -> usize {
    let pos = grid_position_of(current);
    let new_pos = (pos as isize + delta).rem_euclid(256) as usize;
    hue_order()[new_pos] as usize
}

/// Grid cursor delta for a navigation key. Plain hjkl / arrows move one cell
/// (rows are 16 wide); holding Shift moves four cells per press. Uppercase
/// letters arrive from Shift+hjkl on terminals with the kitty keyboard
/// protocol (which report the shifted character).
fn grid_nav_delta(key: &crossterm::event::KeyEvent) -> Option<isize> {
    use crossterm::event::{KeyCode, KeyModifiers};
    // Kitty-protocol terminals report Shift+hjkl as the shifted uppercase
    // char (with or without the SHIFT flag), so treat uppercase as fast too.
    let fast = key.modifiers.contains(KeyModifiers::SHIFT)
        || matches!(key.code, KeyCode::Char('H' | 'J' | 'K' | 'L'));
    let mul: isize = if fast { 4 } else { 1 };
    match key.code {
        KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('H') => Some(-mul),
        KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('L') => Some(mul),
        KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('K') => Some(-16 * mul),
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('J') => Some(16 * mul),
        _ => None,
    }
}

/// List navigation step for a key: `None` for non-navigation keys, otherwise
/// a signed step (negative = up). Plain hjkl / arrows step by 1; holding
/// Shift (or the uppercase char from kitty-protocol terminals) skips
/// `LIST_FAST_STEP` items at a time.
fn list_nav_step(key: &crossterm::event::KeyEvent) -> Option<isize> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let fast = key.modifiers.contains(KeyModifiers::SHIFT)
        || matches!(key.code, KeyCode::Char('H' | 'J' | 'K' | 'L'));
    let step: isize = if fast { LIST_FAST_STEP as isize } else { 1 };
    match key.code {
        KeyCode::Up
        | KeyCode::Char('k')
        | KeyCode::Char('h')
        | KeyCode::Char('K')
        | KeyCode::Char('H') => Some(-step),
        KeyCode::Down
        | KeyCode::Char('j')
        | KeyCode::Char('l')
        | KeyCode::Char('J')
        | KeyCode::Char('L') => Some(step),
        _ => None,
    }
}

/// Format a `Color` for a compact display label.
fn color_label(c: Color) -> String {
    match c {
        Color::Indexed(n) => format!("ANSI {} {}", n, hex_label(n)),
        Color::Rgb(r, g, b) => format!("#{:02X}{:02X}{:02X}", r, g, b),
        other => format!("{:?}", other),
    }
}

/// Compact `#RRGGBB` label for a colour (indexed colours resolved to RGB).
/// Used where space is tight and the ANSI number would clip the row.
fn color_hex(c: Color) -> String {
    match c {
        Color::Indexed(n) => hex_label(n),
        Color::Rgb(r, g, b) => format!("#{:02X}{:02X}{:02X}", r, g, b),
        other => format!("{:?}", other),
    }
}

/// Approximate human-readable name for an ANSI 256 colour index.
/// The base 16 colours get their classic names; everything else falls back to
/// a hue/lightness-based name (e.g. `#5F0000` → "dark red").
fn color_name(idx: u8) -> String {
    const BASE: [&str; 16] = [
        "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white", "gray", "red",
        "green", "yellow", "blue", "magenta", "cyan", "white",
    ];
    if idx < 16 {
        return BASE[idx as usize].to_string();
    }
    let (r, g, b) = ansi256_to_rgb(idx);
    // Grayscale ramp 232..=255: 8 → 238 steps.
    if idx >= 232 {
        let v = 8 + (idx - 232) * 10;
        return if v <= 48 {
            "black".to_string()
        } else if v <= 108 {
            "dark gray".to_string()
        } else if v <= 168 {
            "gray".to_string()
        } else if v <= 218 {
            "light gray".to_string()
        } else {
            "white".to_string()
        };
    }
    // Hidden grays inside the 6x6x6 cube (0, 95, 135, 175, 215, 255).
    if r == g && g == b {
        return match r {
            0 => "black".to_string(),
            95 => "dark gray".to_string(),
            135 | 175 => "gray".to_string(),
            215 => "light gray".to_string(),
            _ => "white".to_string(),
        };
    }
    rgb_hue_name(r, g, b)
}

/// Hue/lightness-based name for an arbitrary RGB colour.
fn rgb_hue_name(r: u8, g: u8, b: u8) -> String {
    let (h, _s, l) = rgb_to_hsl(r, g, b);
    if h.is_infinite() {
        return "gray".to_string();
    }
    let hue = match h {
        _ if !(15.0..345.0).contains(&h) => "red",
        _ if h < 45.0 => "orange",
        _ if h < 70.0 => "yellow",
        _ if h < 160.0 => "green",
        _ if h < 200.0 => "cyan",
        _ if h < 260.0 => "blue",
        _ if h < 290.0 => "purple",
        _ => "magenta",
    };
    let prefix = if l < 0.35 { "dark " } else { "" };
    format!("{}{}", prefix, hue)
}

/// Approximate name for any palette `Color` (Indexed / Rgb; Debug fallback).
fn color_display_name(c: Color) -> String {
    match c {
        Color::Indexed(n) => color_name(n),
        Color::Rgb(r, g, b) => rgb_hue_name(r, g, b),
        other => format!("{:?}", other),
    }
}

/// Assign the grid cursor colour to the highlighted slot and advance to the
/// next slot. Returns the notice text for the footer. Shared by the Slots and
/// Grid editor focuses so Enter/Space behave the same in both.
/// Assign the grid cursor colour to the highlighted slot and remember it as
/// `last_assigned`, WITHOUT advancing the slot. Used by the `o` key so a
/// colour can be overwritten or auditioned in place. Returns the footer
/// notice text.
fn assign_grid_color_stay(editor: &mut EditorState) -> String {
    let c = Color::Indexed(editor.grid as u8);
    let slot_name = SLOT_NAMES[editor.slot];
    // Record the pre-change state so `u` can step back.
    push_palette_history(editor);
    set_slot_color(&mut editor.palette, editor.slot, c);
    editor.last_assigned = Some(editor.slot);
    format!(
        "{} ← {} ({}) · staying",
        slot_name,
        color_label(c),
        color_display_name(c)
    )
}

/// Assign the grid cursor colour to the highlighted slot and advance to the
/// next slot. Returns the notice text for the footer. Shared by the Slots and
/// Grid editor focuses so Enter/Space behave the same in both.
fn assign_grid_color(editor: &mut EditorState) -> String {
    let notice = assign_grid_color_stay(editor);
    editor.slot = (editor.slot + 1).min(SLOT_NAMES.len() - 1);
    notice
}

/// Resolve a palette `Color` to its nearest ANSI 256 index: indexed colours
/// map directly, RGB colours map to the closest 256-colour cube cell, and
/// other colours (Reset etc.) have no grid cell and return `None`.
fn ansi_index_for_color(c: Color) -> Option<u8> {
    match c {
        Color::Indexed(n) => Some(n),
        Color::Rgb(r, g, b) => Some(nearest_ansi256(r, g, b)),
        _ => None,
    }
}

/// Find the ANSI 256 index whose RGB value is closest to `(r, g, b)`.
fn nearest_ansi256(r: u8, g: u8, b: u8) -> u8 {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    let mut best = 0u8;
    let mut best_dist = i32::MAX;
    for i in 0..=255u8 {
        let (ir, ig, ib) = ansi256_to_rgb(i);
        let d = (ir as i32 - r).pow(2) + (ig as i32 - g).pow(2) + (ib as i32 - b).pow(2);
        if d < best_dist {
            best_dist = d;
            best = i;
        }
    }
    best
}

/// A tiny xorshift64 PRNG so palette randomization needs no external crate.
/// Not for cryptography — just enough entropy to scatter colours.
struct SmallRng(u64);

impl SmallRng {
    fn new() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        Self::with_seed(seed)
    }

    /// Deterministic seed (used by `new()` and by tests).
    fn with_seed(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

/// The chromatic (non-gray) ANSI 256 cells in hue order, used as the
/// candidate pool for palette randomization. Deterministic, so a given
/// start offset always yields the same palette.
fn chromatic_candidates() -> Vec<u8> {
    hue_order()
        .iter()
        .copied()
        .filter(|&i| i < 232)
        .filter(|&i| {
            let (r, g, b) = ansi256_to_rgb(i);
            !(r == g && g == b) && (r >= 95 || g >= 95 || b >= 95)
        })
        .collect()
}

/// Scatter a fresh set of pleasing colours across all 17 palette slots.
/// Slots are picked from the chromatic region of the hue-ordered grid using
/// golden-angle stepping: each successive slot lands ~0.382 of the way
/// around the hue wheel from the previous one, so adjacent slots always get
/// distinct hues. `start` is the candidate index for slot 0 — the same
/// start always yields the same palette, which is what lets Shift+r roll
/// deterministically through the sequence.
fn randomize_palette(editor: &mut EditorState, start: usize) {
    let candidates = chromatic_candidates();
    let n = candidates.len();
    if n == 0 {
        return;
    }
    let start = start % n;
    // Record the pre-change state so `u` can step back.
    push_palette_history(editor);
    // The golden angle: 137.5°, i.e. 0.381966 of a full turn. The step is
    // far wider than any single hue bucket (~10 cells), so consecutive
    // slots never share a hue.
    let golden = 0.381_966_f64;
    for slot in 0..SLOT_NAMES.len() {
        let pos = (start as f64 + slot as f64 * golden * n as f64) as usize % n;
        set_slot_color(&mut editor.palette, slot, Color::Indexed(candidates[pos]));
    }
    editor.last_assigned = None;
    // Restart colouring from the first slot so refinement is predictable.
    editor.slot = 0;
}

/// Apply the `r` / `Shift+r` randomization action. Plain `r` starts a fresh
/// random palette; Shift+r (uppercase R on kitty-protocol terminals) rolls
/// to the next palette in the deterministic sequence — each roll advances
/// the start offset, so every palette is guaranteed to differ. Returns the
/// notice text for the footer.
fn randomize_action(
    start_slot: &mut Option<usize>,
    editor: &mut EditorState,
    shifted: bool,
) -> String {
    let n = chromatic_candidates().len().max(1);
    if shifted {
        let start = start_slot.unwrap_or(0).wrapping_add(1) % n;
        *start_slot = Some(start);
        randomize_palette(editor, start);
        "Next random palette — shift+r to roll again".to_string()
    } else {
        let start = SmallRng::new().below(n);
        *start_slot = Some(start);
        randomize_palette(editor, start);
        "Randomized a fresh palette — shift+r rolls to the next".to_string()
    }
}

/// Remember the editor's grid cell, highlighted slot and name so the next
/// edit session (new theme or editing another custom theme) resumes where
/// this one left off. Focus always starts on the name field.
fn remember_editor_state(creator: &mut ThemeCreator) {
    if let Some(ed) = &creator.editor {
        creator.last_grid = ed.grid;
        creator.last_slot = ed.slot;
        creator.last_name = ed.name.clone();
    }
}

/// Maximum number of palette states kept for undo.
const UNDO_DEPTH: usize = 8;

/// Record the editor's current palette on the undo stack (capped at
/// [`UNDO_DEPTH`] states), so `u` can step back through palette changes.
/// Called before every palette mutation (assignments and randomizations).
fn push_palette_history(editor: &mut EditorState) {
    editor.palette_history.push(editor.palette);
    if editor.palette_history.len() > UNDO_DEPTH {
        editor.palette_history.remove(0);
    }
}

/// Restore the previous palette from the undo stack. Returns `true` when a
/// state was restored, `false` when the history is empty. Clears the
/// last-assigned marker since it may point at a slot whose colour was
/// reverted.
fn undo_palette(editor: &mut EditorState) -> bool {
    if let Some(prev) = editor.palette_history.pop() {
        editor.palette = prev;
        editor.last_assigned = None;
        true
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// Key handling (called from app.rs)
// ---------------------------------------------------------------------------

/// Handle a key event for the creator. Returns `Some(name)` when a theme
/// should be applied live / confirmed (list navigation, Enter, or save).
pub fn handle_theme_creator_key(
    creator: &mut ThemeCreator,
    key: crossterm::event::KeyEvent,
) -> Option<String> {
    use crossterm::event::KeyCode;

    if !creator.visible {
        return None;
    }

    if creator.editor.is_some() {
        return handle_editor_key(creator, key);
    }

    // ---- List mode -------------------------------------------------------
    match key.code {
        KeyCode::Esc => {
            if creator.confirm_delete {
                creator.confirm_delete = false;
                creator.notice = None;
            } else {
                creator.close();
            }
            None
        }
        KeyCode::Enter => {
            let name = creator.selected_name();
            // Custom themes in the list come from disk, so they exist;
            // built-ins always resolve. Apply and close.
            creator.close();
            name
        }
        KeyCode::Char('n') => {
            creator.start_new_theme();
            None
        }
        KeyCode::Char('e') => {
            if let Some(name) = creator.selected_name() {
                let is_custom = creator
                    .themes
                    .get(creator.selected)
                    .is_some_and(|t| t.custom);
                if is_custom {
                    creator.start_edit_theme(&name);
                } else {
                    creator.notice = Some(
                        "Built-in themes can't be edited — press n to create a new one".into(),
                    );
                }
            }
            None
        }
        KeyCode::Char('d') | KeyCode::Char('y') if creator.confirm_delete => {
            creator.confirm_delete = false;
            if let Some(name) = creator.selected_name() {
                let _ = delete_theme(&name);
                creator.notice = Some(format!("Deleted '{}'.", name));
                creator.refresh_themes();
            }
            None
        }
        KeyCode::Char('d') => {
            let is_custom = creator
                .themes
                .get(creator.selected)
                .is_some_and(|t| t.custom);
            if is_custom {
                creator.confirm_delete = true;
                creator.notice = Some("Press d again to confirm deletion.".to_string());
            } else {
                creator.notice = Some("Only custom themes can be deleted.".into());
            }
            None
        }
        _ => {
            // hjkl + arrows navigate; holding Shift (or the uppercase char
            // from kitty-protocol terminals) skips LIST_FAST_STEP items.
            if let Some(step) = list_nav_step(&key) {
                match step {
                    -1 => creator.select_prev(),
                    1 => creator.select_next(),
                    s if s < 0 => creator.select_prev_fast(),
                    _ => creator.select_next_fast(),
                }
                // Navigation resets any pending delete confirmation so the
                // next 'd' targets the newly selected theme, not the old one.
                creator.confirm_delete = false;
                creator.notice = None;
                creator.selected_name()
            } else {
                None
            }
        }
    }
}

fn handle_editor_key(
    creator: &mut ThemeCreator,
    key: crossterm::event::KeyEvent,
) -> Option<String> {
    use crossterm::event::KeyCode;

    // Ctrl+S saves from anywhere in the editor. Handled before taking the
    // editor borrow because `save_editor` needs `&mut creator`.
    if key.code == KeyCode::Char('s')
        && key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
    {
        return save_editor(creator);
    }

    // Esc exits the editor back to the theme list from any focus.
    if key.code == KeyCode::Esc {
        remember_editor_state(creator);
        creator.editor = None;
        creator.notice = None;
        return None;
    }

    let editor = creator.editor.as_mut()?;

    match editor.focus {
        EditorFocus::Name => match key.code {
            KeyCode::Enter | KeyCode::Tab => {
                editor.focus = EditorFocus::Slots;
                None
            }
            KeyCode::Backspace => {
                editor.name.pop();
                None
            }
            KeyCode::Char(c) => {
                if editor.name.len() < 40 {
                    editor.name.push(c);
                }
                None
            }
            _ => None,
        },
        EditorFocus::Slots => match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                editor.slot = editor.slot.saturating_sub(1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                editor.slot = (editor.slot + 1).min(SLOT_NAMES.len() - 1);
                None
            }
            // Enter/Space assign the grid cursor colour to the highlighted
            // slot (the right column's `current:` line updates immediately),
            // then advance to the next slot for rapid colouring.
            KeyCode::Enter | KeyCode::Char(' ') => {
                creator.notice = Some(assign_grid_color(editor));
                None
            }
            // `o` assigns the colour without advancing, so a slot can be
            // overwritten or auditioned in place before moving on.
            KeyCode::Char('o') => {
                creator.notice = Some(assign_grid_color_stay(editor));
                None
            }
            // `r` scatters a fresh random palette; Shift+r (uppercase R on
            // kitty-protocol terminals) rolls to the next palette in the
            // deterministic sequence.
            KeyCode::Char('r') | KeyCode::Char('R') => {
                let shifted = key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::SHIFT)
                    || key.code == KeyCode::Char('R');
                let notice = randomize_action(&mut creator.random_start, editor, shifted);
                creator.notice = Some(notice);
                None
            }
            // `u` steps back through palette changes (assignments and
            // randomizations), restoring the previous state.
            KeyCode::Char('u') => {
                creator.notice = Some(if undo_palette(editor) {
                    "Undid the last palette change".into()
                } else {
                    "Nothing to undo yet".into()
                });
                None
            }
            KeyCode::Tab | KeyCode::Right => {
                // Sync the grid cursor to the highlighted slot's current
                // colour so editing picks up where the slot left off.
                if let Some(idx) = ansi_index_for_color(slot_color(&editor.palette, editor.slot)) {
                    editor.grid = idx as usize;
                }
                editor.focus = EditorFocus::Grid;
                None
            }
            _ => None,
        },
        EditorFocus::Grid => match key.code {
            // Tab / Shift+Tab cycle between Slots and Grid only — the name
            // field is a one-shot entry point (n/e open on it) and is never
            // revisited once left.
            KeyCode::Tab | KeyCode::BackTab => {
                editor.focus = EditorFocus::Slots;
                None
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                creator.notice = Some(assign_grid_color(editor));
                None
            }
            // `o` assigns without advancing to the next slot.
            KeyCode::Char('o') => {
                creator.notice = Some(assign_grid_color_stay(editor));
                None
            }
            // `r` scatters a fresh random palette; Shift+r (uppercase R on
            // kitty-protocol terminals) rolls to the next palette in the
            // deterministic sequence.
            KeyCode::Char('r') | KeyCode::Char('R') => {
                let shifted = key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::SHIFT)
                    || key.code == KeyCode::Char('R');
                let notice = randomize_action(&mut creator.random_start, editor, shifted);
                creator.notice = Some(notice);
                None
            }
            // `u` steps back through palette changes (assignments and
            // randomizations), restoring the previous state.
            KeyCode::Char('u') => {
                creator.notice = Some(if undo_palette(editor) {
                    "Undid the last palette change".into()
                } else {
                    "Nothing to undo yet".into()
                });
                None
            }
            _ => {
                // Vim-style hjkl + arrows move one cell; holding Shift moves
                // four cells at a time (see grid_nav_delta).
                if let Some(delta) = grid_nav_delta(&key) {
                    editor.grid = grid_move(editor.grid, delta);
                }
                None
            }
        },
    }
}

/// Validate and persist the editor palette, then switch back to list mode.
/// Returns `Some(name)` when the theme was saved so the app can apply it.
fn save_editor(creator: &mut ThemeCreator) -> Option<String> {
    // Extract values in a scoped block so the borrow on `creator.editor` ends
    // before we mutate `creator` below.
    let (name, palette, original) = {
        let ed = creator.editor.as_ref()?;
        (
            ed.name.trim().to_string(),
            ed.palette,
            ed.original_name.clone(),
        )
    };

    if name.is_empty() {
        creator.notice = Some("Theme name can't be empty.".into());
        return None;
    }
    if !valid_theme_name(&name) {
        creator.notice = Some("Name may contain only letters, numbers, and underscores.".into());
        return None;
    }
    if BUILTIN_THEME_NAMES.contains(&name.as_str()) {
        creator.notice = Some("That's a built-in theme name — pick something else.".into());
        return None;
    }

    if let Err(e) = save_theme(&name, &palette) {
        creator.notice = Some(format!("Save failed: {}", e));
        return None;
    }

    // Rename cleanup: an old file under the previous name is removed.
    if let Some(orig) = &original {
        if orig != &name {
            let _ = delete_theme(orig);
        }
    }

    remember_editor_state(creator);
    creator.editor = None;
    creator.refresh_themes();
    if let Some(idx) = creator.themes.iter().position(|t| t.name == name) {
        creator.selected = idx;
    }
    creator.notice = Some(format!("Saved '{}' to ~/.clawde/themes/.", name));
    Some(name)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

pub fn render_theme_creator(frame: &mut Frame, creator: &ThemeCreator, area: Rect) {
    if !creator.visible {
        return;
    }
    if creator.editor.is_some() {
        render_editor(frame, creator, area);
    } else {
        render_list(frame, creator, area);
    }
}

fn render_list(frame: &mut Frame, creator: &ThemeCreator, area: Rect) {
    let p = current_palette();
    // Cap the dialog to the scrolling viewport; each entry renders as two
    // lines (row + blank), plus header/footer margins.
    let shown = creator.themes.len().min(LIST_VIEWPORT);
    let rows = ((shown as u16 * 2) + 2).min(area.height.saturating_sub(6));
    let layout = begin_modal_frame(frame, area, 76, rows + 6, 2, 1);
    render_modal_title_frame(frame, layout.header_area, "Theme Creator", "esc");
    if let Some(subtitle_area) = modal_header_line_area(layout.header_area, 1) {
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                format!(
                    " j/k/h/l navigate · enter apply · n new · e edit · d delete{}",
                    if creator.themes.len() > LIST_VIEWPORT {
                        format!(" · {} themes (scrollable)", creator.themes.len())
                    } else {
                        String::new()
                    }
                ),
                Style::default().fg(p.disabled),
            )])),
            subtitle_area,
        );
    }

    let mut lines: Vec<Line> = Vec::new();
    let start = creator.scroll_offset;
    let end = (start + LIST_VIEWPORT).min(creator.themes.len());
    for (i, theme) in creator.themes[start..end].iter().enumerate() {
        let real_i = start + i;
        let is_selected = real_i == creator.selected;
        let bg = if is_selected { p.accent } else { p.panel_bg };
        let fg = if is_selected {
            Color::White
        } else {
            p.text_light
        };
        let desc_fg = if is_selected {
            Color::Rgb(248, 220, 236)
        } else {
            p.disabled
        };

        let swatch_spans: Vec<Span> = theme
            .swatch
            .iter()
            .map(|&c| Span::styled("  ", Style::default().bg(c)))
            .collect();

        let badge = if theme.custom { " (custom)" } else { "" };
        let mut row_spans: Vec<Span> = Vec::new();
        row_spans.push(Span::styled(
            if is_selected { "▸" } else { " " },
            Style::default().fg(fg).bg(bg),
        ));
        row_spans.extend(swatch_spans);
        row_spans.push(Span::styled(
            format!(" {:<14}{}", theme.label, badge),
            Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
        ));
        row_spans.push(Span::styled(
            theme.description.clone(),
            Style::default().fg(desc_fg).bg(bg),
        ));
        // Reserve one column for the scrollbar when the list overflows.
        let bar_col = if creator.themes.len() > LIST_VIEWPORT {
            1
        } else {
            0
        };
        let used: usize = row_spans.iter().map(|s| s.content.len()).sum();
        let pad = layout
            .body_area
            .width
            .saturating_sub(used as u16)
            .saturating_sub(bar_col as u16) as usize;
        if pad > 0 {
            row_spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
        }
        lines.push(Line::from(row_spans));
        lines.push(Line::from(""));
    }
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(p.panel_bg)),
        layout.body_area,
    );

    // Scrollbar on the right edge when the list overflows the viewport.
    if creator.themes.len() > LIST_VIEWPORT {
        render_scrollbar(
            frame,
            &p,
            layout.body_area,
            creator.scroll_offset,
            creator.themes.len(),
            LIST_VIEWPORT,
        );
    }

    // The title frame already shows "esc" in the top-right corner, so the
    // footer omits it — 72 display columns fits the 74-wide footer area.
    let footer_text = creator.notice.clone().unwrap_or_else(|| {
        " ↑↓/hjkl navigate (shift=fast) · enter apply · n new · e edit · d delete".to_string()
    });
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            footer_text,
            Style::default()
                .fg(p.disabled)
                .add_modifier(Modifier::ITALIC),
        )])),
        layout.footer_area,
    );
}

fn render_editor(frame: &mut Frame, creator: &ThemeCreator, area: Rect) {
    let Some(editor) = creator.editor.as_ref() else {
        return;
    };
    let p = current_palette();

    // Sized for the slot list, the hue-ordered grid, and the full-width live
    // preview — narrower and shorter than before.
    // Compact: 98 wide x 28 tall with a 2-row header and 1-row footer, so
    // the body is 23 rows — top section 18 (grid title+blank+16 rows) and
    // the live preview 5 (info + prompt bar + chips + sample).
    let layout = begin_modal_frame(frame, area, 98, 28, 2, 1);
    render_modal_title_frame(
        frame,
        layout.header_area,
        "Theme Creator — Editor",
        "esc back · ctrl+s save",
    );

    // Name input row.
    if let Some(name_area) = modal_header_line_area(layout.header_area, 1) {
        let name_style = if editor.focus == EditorFocus::Name {
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.text_light)
        };
        let cursor = if editor.focus == EditorFocus::Name {
            "▌"
        } else {
            " "
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" Name: ", Style::default().fg(p.disabled)),
                Span::styled(
                    if editor.name.is_empty() {
                        "(untitled)".to_string()
                    } else {
                        editor.name.clone()
                    },
                    name_style,
                ),
                Span::styled(cursor, Style::default().fg(p.accent)),
                Span::styled(
                    format!(
                        "    focus: {}",
                        match editor.focus {
                            EditorFocus::Name => "name",
                            EditorFocus::Slots => "slots",
                            EditorFocus::Grid => "grid",
                        }
                    ),
                    Style::default().fg(p.disabled),
                ),
            ])),
            name_area,
        );
    }

    // Body: columns on top (slots | divider | grid) with a full-width live
    // preview below. The preview mirrors the main TUI so palette changes are
    // visible immediately (the whole frame is themed by the editor palette).
    let body_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(5)])
        .split(layout.body_area);

    let col_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(31),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(body_chunks[0]);

    // ---- Left: slot list -------------------------------------------------
    let mut left_lines: Vec<Line> = Vec::new();
    left_lines.push(Line::from(Span::styled(
        " Palette slots",
        Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
    )));

    for (i, name) in SLOT_NAMES.iter().enumerate() {
        let is_active = editor.focus == EditorFocus::Slots && i == editor.slot;
        let color = slot_color(&editor.palette, i);
        let bg = if is_active { p.accent } else { p.panel_bg };
        let fg = if is_active {
            Color::White
        } else {
            p.text_light
        };
        let mut spans: Vec<Span> = Vec::new();
        spans.push(Span::styled(
            if is_active { "▸" } else { " " },
            Style::default().fg(fg).bg(bg),
        ));
        spans.push(Span::styled(
            format!(" {:<17}", name),
            Style::default().fg(fg).bg(bg).add_modifier(if is_active {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
        ));
        spans.push(Span::styled("  ", Style::default().bg(color)));
        // A checkmark marks the slot most recently assigned so the left
        // column visibly confirms every Enter assignment.
        if Some(i) == editor.last_assigned {
            spans.push(Span::styled(
                "✓",
                Style::default()
                    .fg(Color::White)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        // Compact: hex only — no ANSI number, so rows never clip.
        spans.push(Span::styled(
            format!(" {}", color_hex(color)),
            Style::default().fg(if is_active {
                Color::Rgb(248, 220, 236)
            } else {
                p.disabled
            }),
        ));
        left_lines.push(Line::from(spans));
    }

    frame.render_widget(
        Paragraph::new(left_lines).style(Style::default().bg(p.panel_bg)),
        col_chunks[0],
    );

    // ---- Divider ----------------------------------------------------------
    let divider_lines: Vec<Line> = (0..col_chunks[1].height)
        .map(|_| Line::from(Span::styled("│", Style::default().fg(p.disabled))))
        .collect();
    frame.render_widget(Paragraph::new(divider_lines), col_chunks[1]);

    // ---- Right: 256-color grid -------------------------------------------
    let mut grid_lines: Vec<Line> = Vec::new();
    grid_lines.push(Line::from(Span::styled(
        " ANSI 256-color palette",
        Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
    )));
    grid_lines.push(Line::from(""));

    // The grid is laid out in hue order (see `hue_order`): chromatic colours
    // read as a rainbow from top-left to bottom-right, with the grayscale
    // ramp at the very end. `editor.grid` stays the ANSI index.
    let order = hue_order();
    for row in 0..16u16 {
        let mut spans: Vec<Span> = Vec::new();
        for col in 0..16u16 {
            let pos = (row * 16 + col) as usize;
            let idx = order[pos];
            let is_sel = editor.focus == EditorFocus::Grid && editor.grid as u8 == idx;
            let cell = if is_sel { "██" } else { "  " };
            let style = if is_sel {
                Style::default().fg(Color::White).bg(Color::Indexed(idx))
            } else {
                Style::default().bg(Color::Indexed(idx))
            };
            spans.push(Span::styled(cell, style));
        }
        grid_lines.push(Line::from(spans));
    }

    frame.render_widget(
        Paragraph::new(grid_lines).style(Style::default().bg(p.panel_bg)),
        col_chunks[2],
    );

    // ---- Live preview (full width, mirrors the main TUI) ------------------
    let mut preview_lines: Vec<Line> = Vec::new();
    // The pick/current info line leads the preview: the grid cursor (the
    // colour you're about to pick) on the left, and the highlighted slot's
    // *current* colour on the right. Tying `current` to the slot means the
    // hex updates while navigating the slot list with j/k, not just while
    // moving the grid cursor. Each hex carries a colour name (e.g. "red").
    // Use the short slot name so the line fits one row.
    let slot_name = match SLOT_NAMES[editor.slot] {
        "secondary_accent" => "sec",
        other => other,
    };
    let slot_now = slot_color(&editor.palette, editor.slot);
    preview_lines.push(Line::from(vec![
        Span::styled(
            format!(
                " pick: ANSI {} · {} ({})",
                editor.grid,
                hex_label(editor.grid as u8),
                color_name(editor.grid as u8)
            ),
            Style::default()
                .fg(p.text_light)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" → {} slot", slot_name),
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "    current: {} ({})",
                color_hex(slot_now),
                color_display_name(slot_now)
            ),
            Style::default().fg(p.disabled),
        ),
    ]));
    // Mock prompt bar, like the real input area at the bottom of the TUI
    // (token counter right-aligned, mirroring the real status row).
    let mut prompt_spans: Vec<Span> = vec![
        Span::styled(
            " ❯ ",
            Style::default()
                .fg(editor.palette.accent)
                .bg(editor.palette.panel_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "design a theme for me",
            Style::default()
                .fg(editor.palette.text_light)
                .bg(editor.palette.panel_bg),
        ),
        Span::styled(
            "▌",
            Style::default()
                .fg(editor.palette.accent)
                .bg(editor.palette.panel_bg),
        ),
    ];
    let counter = "  12.4k/200k tok";
    // Display width, not byte length: the ❯ / ▌ glyphs are multi-byte.
    let used: usize = prompt_spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let pad = body_chunks[1]
        .width
        .saturating_sub((used + counter.len()) as u16) as usize;
    if pad > 0 {
        prompt_spans.push(Span::styled(
            " ".repeat(pad),
            Style::default().bg(editor.palette.panel_bg),
        ));
    }
    prompt_spans.push(Span::styled(
        counter,
        Style::default()
            .fg(editor.palette.disabled)
            .bg(editor.palette.panel_bg),
    ));
    preview_lines.push(Line::from(prompt_spans));
    // Two rows of labelled colour chips.
    let half = SLOT_NAMES.len() / 2;
    preview_lines.push(preview_chip_line(
        &editor.palette,
        &SLOT_NAMES[..half],
        editor.palette.panel_bg,
    ));
    preview_lines.push(preview_chip_line(
        &editor.palette,
        &SLOT_NAMES[half..],
        editor.palette.panel_bg,
    ));
    // Sample sentence in the theme's text colours.
    preview_lines.push(preview_sample_line(&editor.palette));

    frame.render_widget(
        Paragraph::new(preview_lines).style(Style::default().bg(p.panel_bg)),
        body_chunks[1],
    );

    // ---- Footer -----------------------------------------------------------
    // 87 display columns fits even when the dialog clamps on narrow
    // terminals. Save/esc live in the title's right hint; tab-switch is
    // implied by the focus indicator in the name row.
    let footer_text = creator.notice.clone().unwrap_or_else(|| {
        " ↑↓/jk slots · hjkl grid (shift=fast) · enter/space pick · o stay · r/R random · u undo"
            .to_string()
    });
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            footer_text,
            Style::default()
                .fg(p.disabled)
                .add_modifier(Modifier::ITALIC),
        )])),
        layout.footer_area,
    );
}

/// One line of labelled colour chips: each slot name in its colour followed
/// by a solid swatch, on the theme's panel background.
fn preview_chip_line(pal: &ColorPalette, names: &[&str], panel_bg: Color) -> Line<'static> {
    let mut spans: Vec<Span> = Vec::new();
    for &name in names {
        let idx = SLOT_NAMES.iter().position(|n| *n == name).unwrap_or(0);
        let c = slot_color(pal, idx);
        let label = match name {
            "secondary_accent" => "sec",
            "panel_bg" => "panel",
            "text_light" => "light",
            "text_dark" => "dark",
            "model_name" => "model",
            "routing" => "route",
            "vim_hint" => "vim",
            other => other,
        };
        spans.push(Span::styled(
            format!(" {label} "),
            Style::default()
                .fg(c)
                .bg(panel_bg)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled("▮▮ ", Style::default().bg(c)));
    }
    Line::from(spans)
}

/// Sample sentence in the theme's text colours.
fn preview_sample_line(pal: &ColorPalette) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            "  The quick brown fox jumps over the lazy dog 1234567890",
            Style::default().fg(pal.text_light).bg(pal.panel_bg),
        ),
        Span::styled(" error ", Style::default().fg(pal.error).bg(pal.panel_bg)),
        Span::styled(
            " success ",
            Style::default().fg(pal.success).bg(pal.panel_bg),
        ),
        Span::styled(" warn ", Style::default().fg(pal.warning).bg(pal.panel_bg)),
        Span::styled(" info ", Style::default().fg(pal.info).bg(pal.panel_bg)),
        Span::styled(" accent ", Style::default().fg(pal.accent).bg(pal.panel_bg)),
    ])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn list_includes_builtin_and_custom_sections() {
        let mut c = ThemeCreator::new();
        c.open("dark");
        assert!(c.visible);
        assert!(!c.themes.is_empty());
        assert!(c.themes.iter().any(|t| t.name == "dark"));
        assert!(!c.themes.iter().any(|t| t.name == "default" && t.custom));
        // Custom themes only come from disk; the refresh shouldn't crash.
        c.refresh_themes();
        assert!(!c.themes.is_empty());
    }

    #[test]
    fn list_navigation_wraps_and_live_previews() {
        let mut c = ThemeCreator::new();
        c.open("default");
        let first = c.selected_name();
        let up = handle_theme_creator_key(&mut c, key(KeyCode::Char('k')));
        assert!(up.is_some(), "k should live-preview a theme");
        assert_ne!(up, first);
        // Wrapping: `len` total presses returns to the starting selection.
        // One press happened above, so loop `len - 1` more times.
        let mut last = None;
        for _ in 0..c.themes.len().saturating_sub(1) {
            last = handle_theme_creator_key(&mut c, key(KeyCode::Char('k')));
        }
        assert_eq!(last, first);
        assert!(c.visible, "navigation keeps the creator open");
    }

    #[test]
    fn enter_applies_and_closes() {
        let mut c = ThemeCreator::new();
        c.open("dark");
        c.selected = c.themes.iter().position(|t| t.name == "dark").unwrap();
        let applied = handle_theme_creator_key(&mut c, key(KeyCode::Enter));
        assert_eq!(applied.as_deref(), Some("dark"));
        assert!(!c.visible);
    }

    #[test]
    fn new_theme_opens_editor_and_types_name() {
        let mut c = ThemeCreator::new();
        c.open("default");
        handle_theme_creator_key(&mut c, key(KeyCode::Char('n')));
        assert!(c.editor.is_some());
        for ch in "mytheme".chars() {
            handle_theme_creator_key(&mut c, key(KeyCode::Char(ch)));
        }
        assert_eq!(c.editor.as_ref().unwrap().name, "mytheme");
        // Backspace removes a single char.
        handle_theme_creator_key(&mut c, key(KeyCode::Backspace));
        assert_eq!(c.editor.as_ref().unwrap().name, "mythem");
        // Enter moves to slots.
        handle_theme_creator_key(&mut c, key(KeyCode::Enter));
        assert_eq!(c.editor.as_ref().unwrap().focus, EditorFocus::Slots);
    }

    #[test]
    fn n_opens_editor_on_the_name_field() {
        let mut c = ThemeCreator::new();
        c.open("default");
        handle_theme_creator_key(&mut c, key(KeyCode::Char('n')));
        assert_eq!(
            c.editor.as_ref().unwrap().focus,
            EditorFocus::Name,
            "n starts on the name field for its one-shot rename"
        );
    }

    #[test]
    fn edit_opens_editor_on_the_name_field() {
        let mut c = ThemeCreator::new();
        // open() marks the creator visible (handle_theme_creator_key early-
        // returns otherwise) and refreshes the list, so push after it.
        c.open("default");
        c.themes.push(ThemeEntry {
            name: "mytheme".into(),
            label: "mytheme".into(),
            description: "Custom theme".into(),
            custom: true,
            swatch: [Color::Reset; 4],
        });
        c.selected = c.themes.len() - 1;
        handle_theme_creator_key(&mut c, key(KeyCode::Char('e')));
        let ed = c.editor.as_ref().unwrap();
        assert_eq!(ed.focus, EditorFocus::Name, "e starts on the name field");
        assert_eq!(ed.name, "mytheme");
        assert_eq!(ed.original_name.as_deref(), Some("mytheme"));
    }

    #[test]
    fn tab_cycle_never_returns_to_the_name_field() {
        let mut c = ThemeCreator::new();
        c.open("default");
        handle_theme_creator_key(&mut c, key(KeyCode::Char('n')));
        assert_eq!(c.editor.as_ref().unwrap().focus, EditorFocus::Name);
        // Leave the name field once — Tab/Enter must never bring it back.
        handle_theme_creator_key(&mut c, key(KeyCode::Enter));
        assert_eq!(c.editor.as_ref().unwrap().focus, EditorFocus::Slots);
        for _ in 0..6 {
            handle_theme_creator_key(&mut c, key(KeyCode::Tab));
            let f = c.editor.as_ref().unwrap().focus;
            assert_ne!(
                f,
                EditorFocus::Name,
                "Tab must never cycle back to the name field"
            );
            assert!(matches!(f, EditorFocus::Slots | EditorFocus::Grid));
        }
        // Shift+Tab from the grid also returns to slots, not the name.
        handle_theme_creator_key(&mut c, key(KeyCode::Tab));
        assert_eq!(c.editor.as_ref().unwrap().focus, EditorFocus::Grid);
        handle_theme_creator_key(&mut c, key(KeyCode::BackTab));
        assert_eq!(c.editor.as_ref().unwrap().focus, EditorFocus::Slots);
    }

    #[test]
    fn grid_picks_color_into_slot_and_advances() {
        let mut c = ThemeCreator::new();
        c.open("default");
        handle_theme_creator_key(&mut c, key(KeyCode::Char('n')));
        let ed = c.editor.as_mut().unwrap();
        ed.focus = EditorFocus::Grid;
        ed.grid = 196;
        let slot_before = ed.slot;
        handle_theme_creator_key(&mut c, key(KeyCode::Enter));
        let ed = c.editor.as_ref().unwrap();
        assert_eq!(slot_color(&ed.palette, slot_before), Color::Indexed(196));
        assert_eq!(ed.slot, (slot_before + 1).min(SLOT_NAMES.len() - 1));
        assert_eq!(ed.last_assigned, Some(slot_before));
    }

    #[test]
    fn slots_enter_assigns_grid_color_and_advances() {
        let mut c = ThemeCreator::new();
        c.open("default");
        handle_theme_creator_key(&mut c, key(KeyCode::Char('n')));
        let ed = c.editor.as_mut().unwrap();
        ed.focus = EditorFocus::Slots;
        ed.grid = 196;
        let slot_before = ed.slot;
        handle_theme_creator_key(&mut c, key(KeyCode::Enter));
        let ed = c.editor.as_ref().unwrap();
        assert_eq!(slot_color(&ed.palette, slot_before), Color::Indexed(196));
        assert_eq!(ed.slot, (slot_before + 1).min(SLOT_NAMES.len() - 1));
        assert_eq!(ed.last_assigned, Some(slot_before));
        assert_eq!(ed.focus, EditorFocus::Slots, "Enter keeps focus on slots");
        assert!(
            c.notice.as_deref().unwrap().contains("←"),
            "footer notice confirms the assignment"
        );
    }

    #[test]
    fn o_picks_color_without_advancing_slot() {
        let mut c = ThemeCreator::new();
        c.open("default");
        handle_theme_creator_key(&mut c, key(KeyCode::Char('n')));
        let ed = c.editor.as_mut().unwrap();
        ed.focus = EditorFocus::Slots;
        ed.grid = 196;
        let slot_before = ed.slot;
        // `o` assigns and STAYS on the slot.
        handle_theme_creator_key(&mut c, key(KeyCode::Char('o')));
        let ed = c.editor.as_ref().unwrap();
        assert_eq!(slot_color(&ed.palette, slot_before), Color::Indexed(196));
        assert_eq!(ed.slot, slot_before, "o must not advance the slot");
        assert_eq!(ed.last_assigned, Some(slot_before));
        assert_eq!(ed.focus, EditorFocus::Slots, "o keeps focus on slots");
        assert!(
            c.notice.as_deref().unwrap().contains("staying"),
            "notice marks the stay"
        );

        // Enter then advances from the same slot.
        handle_theme_creator_key(&mut c, key(KeyCode::Enter));
        let ed = c.editor.as_ref().unwrap();
        assert_eq!(ed.slot, (slot_before + 1).min(SLOT_NAMES.len() - 1));
    }

    #[test]
    fn o_works_from_grid_focus_too() {
        let mut c = ThemeCreator::new();
        c.open("default");
        handle_theme_creator_key(&mut c, key(KeyCode::Char('n')));
        let ed = c.editor.as_mut().unwrap();
        ed.focus = EditorFocus::Grid;
        ed.grid = 52;
        let slot_before = ed.slot;
        handle_theme_creator_key(&mut c, key(KeyCode::Char('o')));
        let ed = c.editor.as_ref().unwrap();
        assert_eq!(slot_color(&ed.palette, slot_before), Color::Indexed(52));
        assert_eq!(ed.slot, slot_before, "o must not advance the slot");
        assert_eq!(ed.focus, EditorFocus::Grid, "o keeps focus on grid");
    }

    #[test]
    fn nearest_ansi256_maps_rgb_to_closest_cell() {
        // Exact cube/base matches (unique cells).
        assert_eq!(nearest_ansi256(135, 95, 95), 95);
        assert_eq!(nearest_ansi256(0, 0, 0), 0);
        assert_eq!(nearest_ansi256(128, 128, 128), 8);
        // Exact ties resolve to the lowest index: pure red matches both base
        // 9 and cube 196 (#FF0000), so the earlier index wins.
        assert_eq!(nearest_ansi256(255, 0, 0), 9);
        assert_eq!(nearest_ansi256(250, 3, 2), 9);
        // Off-cube values round to the nearest cell: (200,100,50) →
        // (215,95,95) = index 167.
        assert_eq!(nearest_ansi256(200, 100, 50), 167);
    }

    #[test]
    fn tab_into_grid_syncs_cursor_to_slot_color() {
        let mut c = ThemeCreator::new();
        c.open("default");
        handle_theme_creator_key(&mut c, key(KeyCode::Char('n')));
        let ed = c.editor.as_mut().unwrap();
        ed.focus = EditorFocus::Slots;
        // The highlighted slot holds a known ANSI colour; Tab should land the
        // grid cursor on exactly that cell.
        set_slot_color(&mut ed.palette, ed.slot, Color::Indexed(196));
        handle_theme_creator_key(&mut c, key(KeyCode::Tab));
        let ed = c.editor.as_ref().unwrap();
        assert_eq!(ed.focus, EditorFocus::Grid);
        assert_eq!(
            ed.grid, 196,
            "grid cursor syncs to the slot's indexed colour"
        );

        // An RGB slot colour maps to the nearest ANSI cell. Pure red ties
        // between base 9 and cube 196 (#FF0000); the lowest index wins.
        let ed = c.editor.as_mut().unwrap();
        ed.focus = EditorFocus::Slots;
        set_slot_color(&mut ed.palette, ed.slot, Color::Rgb(255, 0, 0));
        handle_theme_creator_key(&mut c, key(KeyCode::Right));
        assert_eq!(c.editor.as_ref().unwrap().grid, 9);
    }

    #[test]
    fn randomize_palette_scatters_vivid_colors() {
        let mut c = ThemeCreator::new();
        c.open("default");
        handle_theme_creator_key(&mut c, key(KeyCode::Char('n')));
        let ed = c.editor.as_mut().unwrap();
        ed.focus = EditorFocus::Grid;
        ed.slot = 4;
        randomize_palette(ed, 42);
        let ed = c.editor.as_ref().unwrap();
        assert_eq!(ed.slot, 0, "randomize restarts colouring from slot 0");
        assert_eq!(ed.last_assigned, None);
        for slot in 0..SLOT_NAMES.len() {
            match slot_color(&ed.palette, slot) {
                Color::Indexed(i) => {
                    assert!(i < 232, "no grayscale-ramp colours");
                    let (r, g, b) = ansi256_to_rgb(i);
                    assert!(!(r == g && g == b), "no gray cells in the palette");
                }
                other => panic!("slot {} should be Indexed, got {:?}", slot, other),
            }
        }
    }

    #[test]
    fn r_key_randomizes_and_resets_slot() {
        let mut c = ThemeCreator::new();
        c.open("default");
        handle_theme_creator_key(&mut c, key(KeyCode::Char('n')));
        let ed = c.editor.as_mut().unwrap();
        ed.focus = EditorFocus::Grid;
        ed.slot = 4;
        handle_theme_creator_key(&mut c, key(KeyCode::Char('r')));
        let ed = c.editor.as_ref().unwrap();
        assert_eq!(ed.slot, 0, "r resets the slot pointer");
        assert_eq!(ed.focus, EditorFocus::Grid, "r keeps focus on the grid");
        assert!(c.notice.as_deref().unwrap().contains("Randomized"));
    }

    #[test]
    fn randomize_uses_golden_angle_so_adjacent_hues_differ() {
        let mut c = ThemeCreator::new();
        c.open("default");
        handle_theme_creator_key(&mut c, key(KeyCode::Char('n')));
        let ed = c.editor.as_mut().unwrap();
        ed.focus = EditorFocus::Grid;
        randomize_palette(ed, 7);
        let ed = c.editor.as_ref().unwrap();
        for slot in 0..SLOT_NAMES.len() - 1 {
            let (ra, ga, ba) = match slot_color(&ed.palette, slot) {
                Color::Indexed(i) => ansi256_to_rgb(i),
                other => panic!("slot {} should be Indexed, got {:?}", slot, other),
            };
            let (rb, gb, bb) = match slot_color(&ed.palette, slot + 1) {
                Color::Indexed(i) => ansi256_to_rgb(i),
                other => panic!("slot {} should be Indexed, got {:?}", slot + 1, other),
            };
            let (ha, _, _) = rgb_to_hsl(ra, ga, ba);
            let (hb, _, _) = rgb_to_hsl(rb, gb, bb);
            assert_ne!(
                ha,
                hb,
                "adjacent slots {} and {} share hue {}",
                slot,
                slot + 1,
                ha
            );
        }
    }

    #[test]
    fn same_start_yields_same_palette() {
        let mut c = ThemeCreator::new();
        c.open("default");
        handle_theme_creator_key(&mut c, key(KeyCode::Char('n')));
        let ed = c.editor.as_mut().unwrap();
        ed.focus = EditorFocus::Grid;
        randomize_palette(ed, 11);
        let first = c.editor.as_ref().unwrap().palette;
        let ed = c.editor.as_mut().unwrap();
        ed.focus = EditorFocus::Grid;
        randomize_palette(ed, 11);
        let second = c.editor.as_ref().unwrap().palette;
        for slot in 0..SLOT_NAMES.len() {
            assert_eq!(
                slot_color(&first, slot),
                slot_color(&second, slot),
                "slot {} differs across identical starts",
                slot
            );
        }
    }

    #[test]
    fn shift_r_rolls_to_next_palette_in_sequence() {
        let mut c = ThemeCreator::new();
        c.open("default");
        handle_theme_creator_key(&mut c, key(KeyCode::Char('n')));
        let ed = c.editor.as_mut().unwrap();
        ed.focus = EditorFocus::Grid;
        // Deterministic start: 7 → 8 → 9 as shift+r is pressed.
        c.random_start = Some(7);
        let shifted_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::SHIFT);
        handle_theme_creator_key(&mut c, shifted_r);
        let first0 = slot_color(&c.editor.as_ref().unwrap().palette, 0);
        assert_eq!(c.random_start, Some(8));
        handle_theme_creator_key(&mut c, shifted_r);
        let second0 = slot_color(&c.editor.as_ref().unwrap().palette, 0);
        assert_eq!(c.random_start, Some(9));
        assert_ne!(
            first0, second0,
            "each shift+r must roll to a different palette"
        );
        // Uppercase R (kitty protocol) behaves like shift+r.
        handle_theme_creator_key(&mut c, key(KeyCode::Char('R')));
        assert_eq!(c.random_start, Some(10));
        assert!(c.notice.as_deref().unwrap().contains("Next random palette"));
    }

    #[test]
    fn u_undoes_from_slots_focus() {
        let mut c = ThemeCreator::new();
        c.open("default");
        handle_theme_creator_key(&mut c, key(KeyCode::Char('n')));
        let ed = c.editor.as_mut().unwrap();
        ed.focus = EditorFocus::Slots;
        ed.grid = 196;
        set_slot_color(&mut ed.palette, 0, Color::Indexed(52));
        // Enter in Slots focus assigns slot 0 -> 196 (pushing the baseline).
        handle_theme_creator_key(&mut c, key(KeyCode::Enter));
        assert_eq!(
            slot_color(&c.editor.as_ref().unwrap().palette, 0),
            Color::Indexed(196)
        );
        // u in Slots focus restores it and clears the checkmark.
        handle_theme_creator_key(&mut c, key(KeyCode::Char('u')));
        let ed = c.editor.as_ref().unwrap();
        assert_eq!(slot_color(&ed.palette, 0), Color::Indexed(52));
        assert_eq!(ed.focus, EditorFocus::Slots, "u keeps focus on slots");
        assert_eq!(ed.last_assigned, None, "undo clears the checkmark");
    }

    #[test]
    fn u_undoes_last_assignment() {
        let mut c = ThemeCreator::new();
        c.open("default");
        handle_theme_creator_key(&mut c, key(KeyCode::Char('n')));
        let ed = c.editor.as_mut().unwrap();
        ed.focus = EditorFocus::Grid;
        ed.grid = 196;
        // Baseline: give slot 0 a known colour (outside the undo stack).
        set_slot_color(&mut ed.palette, 0, Color::Indexed(52));
        // Enter assigns slot 0 -> 196 and pushes the baseline onto the stack.
        handle_theme_creator_key(&mut c, key(KeyCode::Enter));
        assert_eq!(
            slot_color(&c.editor.as_ref().unwrap().palette, 0),
            Color::Indexed(196)
        );
        // u restores the baseline.
        handle_theme_creator_key(&mut c, key(KeyCode::Char('u')));
        let ed = c.editor.as_ref().unwrap();
        assert_eq!(slot_color(&ed.palette, 0), Color::Indexed(52));
        assert!(c.notice.as_deref().unwrap().contains("Undid"));
        // A second u has nothing left.
        handle_theme_creator_key(&mut c, key(KeyCode::Char('u')));
        assert_eq!(
            slot_color(&c.editor.as_ref().unwrap().palette, 0),
            Color::Indexed(52)
        );
        assert!(c.notice.as_deref().unwrap().contains("Nothing to undo"));
    }

    #[test]
    fn u_undoes_randomize() {
        let mut c = ThemeCreator::new();
        c.open("default");
        handle_theme_creator_key(&mut c, key(KeyCode::Char('n')));
        let ed = c.editor.as_mut().unwrap();
        ed.focus = EditorFocus::Grid;
        set_slot_color(&mut ed.palette, 0, Color::Indexed(52));
        // 'r' randomizes (pushing the baseline); u restores it.
        handle_theme_creator_key(&mut c, key(KeyCode::Char('r')));
        handle_theme_creator_key(&mut c, key(KeyCode::Char('u')));
        assert_eq!(
            slot_color(&c.editor.as_ref().unwrap().palette, 0),
            Color::Indexed(52),
            "u restores the pre-randomize palette"
        );
    }

    #[test]
    fn u_steps_back_multiple_assignments() {
        let mut c = ThemeCreator::new();
        c.open("default");
        handle_theme_creator_key(&mut c, key(KeyCode::Char('n')));
        let ed = c.editor.as_mut().unwrap();
        ed.focus = EditorFocus::Grid;
        // Baseline slots 0-2.
        set_slot_color(&mut ed.palette, 0, Color::Indexed(0));
        set_slot_color(&mut ed.palette, 1, Color::Indexed(1));
        set_slot_color(&mut ed.palette, 2, Color::Indexed(2));
        // Assign three slots in sequence.
        let grid = [196, 46, 21];
        for g in grid {
            let ed = c.editor.as_mut().unwrap();
            ed.grid = g;
            handle_theme_creator_key(&mut c, key(KeyCode::Enter));
        }
        // Undo three times returns to the baseline.
        handle_theme_creator_key(&mut c, key(KeyCode::Char('u')));
        assert_eq!(
            slot_color(&c.editor.as_ref().unwrap().palette, 2),
            Color::Indexed(2)
        );
        handle_theme_creator_key(&mut c, key(KeyCode::Char('u')));
        assert_eq!(
            slot_color(&c.editor.as_ref().unwrap().palette, 1),
            Color::Indexed(1)
        );
        handle_theme_creator_key(&mut c, key(KeyCode::Char('u')));
        assert_eq!(
            slot_color(&c.editor.as_ref().unwrap().palette, 0),
            Color::Indexed(0)
        );
        // Nothing left to undo.
        handle_theme_creator_key(&mut c, key(KeyCode::Char('u')));
        assert!(c.notice.as_deref().unwrap().contains("Nothing to undo"));
    }

    #[test]
    fn undo_history_is_capped() {
        let mut c = ThemeCreator::new();
        c.open("default");
        handle_theme_creator_key(&mut c, key(KeyCode::Char('n')));
        let ed = c.editor.as_mut().unwrap();
        ed.focus = EditorFocus::Grid;
        // Baseline slot 0, distinct from any assignment below.
        set_slot_color(&mut ed.palette, 0, Color::Indexed(0));
        // Make UNDO_DEPTH + 2 assignments.
        for k in 1..=(UNDO_DEPTH + 2) {
            let ed = c.editor.as_mut().unwrap();
            ed.grid = 16 + k;
            handle_theme_creator_key(&mut c, key(KeyCode::Enter));
        }
        // Exactly UNDO_DEPTH undos are available; the oldest state was
        // evicted, so the baseline is unreachable.
        for _ in 0..UNDO_DEPTH {
            handle_theme_creator_key(&mut c, key(KeyCode::Char('u')));
        }
        assert!(c.notice.as_deref().unwrap().contains("Undid"));
        handle_theme_creator_key(&mut c, key(KeyCode::Char('u')));
        assert!(c.notice.as_deref().unwrap().contains("Nothing to undo"));
        assert_ne!(
            slot_color(&c.editor.as_ref().unwrap().palette, 0),
            Color::Indexed(0),
            "the oldest (baseline) state was evicted by the cap"
        );
    }

    #[test]
    fn editor_state_persists_across_edit_sessions() {
        let mut c = ThemeCreator::new();
        c.open("default");
        handle_theme_creator_key(&mut c, key(KeyCode::Char('n')));
        let ed = c.editor.as_mut().unwrap();
        ed.focus = EditorFocus::Grid;
        ed.grid = 196;
        ed.slot = 5;
        ed.name = "palmtree".into();
        // Esc back to the list remembers position and name.
        handle_theme_creator_key(&mut c, key(KeyCode::Esc));
        assert!(c.editor.is_none());
        // The next edit session resumes where the last one left off.
        handle_theme_creator_key(&mut c, key(KeyCode::Char('n')));
        let ed = c.editor.as_ref().unwrap();
        assert_eq!(ed.grid, 196);
        assert_eq!(ed.slot, 5);
        assert_eq!(
            ed.focus,
            EditorFocus::Name,
            "n always starts on the name field (one-shot entry)"
        );
        assert_eq!(ed.name, "palmtree", "name carries over for quick variants");
    }

    #[test]
    fn slots_tab_moves_to_grid() {
        let mut c = ThemeCreator::new();
        c.open("default");
        handle_theme_creator_key(&mut c, key(KeyCode::Char('n')));
        let ed = c.editor.as_mut().unwrap();
        ed.focus = EditorFocus::Slots;
        handle_theme_creator_key(&mut c, key(KeyCode::Tab));
        assert_eq!(c.editor.as_ref().unwrap().focus, EditorFocus::Grid);
    }

    #[test]
    fn grid_movement_wraps_within_hue_order() {
        let mut c = ThemeCreator::new();
        c.open("default");
        handle_theme_creator_key(&mut c, key(KeyCode::Char('n')));
        let ed = c.editor.as_mut().unwrap();
        ed.focus = EditorFocus::Grid;
        ed.grid = hue_order()[0] as usize; // first cell in hue order
                                           // Left wraps to the last cell of the 16x16 grid.
        handle_theme_creator_key(&mut c, key(KeyCode::Left));
        assert_eq!(c.editor.as_ref().unwrap().grid, hue_order()[255] as usize);
        // Down from the last row (pos 255) wraps to the top of the same
        // column: (255 + 16) mod 256 = 15.
        handle_theme_creator_key(&mut c, key(KeyCode::Down));
        assert_eq!(c.editor.as_ref().unwrap().grid, hue_order()[15] as usize);
    }

    #[test]
    fn grid_navigates_with_hjkl() {
        let mut c = ThemeCreator::new();
        c.open("default");
        handle_theme_creator_key(&mut c, key(KeyCode::Char('n')));
        let ed = c.editor.as_mut().unwrap();
        ed.focus = EditorFocus::Grid;
        ed.grid = hue_order()[0] as usize;
        // l moves right one hue-ordered cell.
        handle_theme_creator_key(&mut c, key(KeyCode::Char('l')));
        assert_eq!(c.editor.as_ref().unwrap().grid, hue_order()[1] as usize);
        // h moves back left.
        handle_theme_creator_key(&mut c, key(KeyCode::Char('h')));
        assert_eq!(c.editor.as_ref().unwrap().grid, hue_order()[0] as usize);
        // j moves down one row (16 cells).
        handle_theme_creator_key(&mut c, key(KeyCode::Char('j')));
        assert_eq!(c.editor.as_ref().unwrap().grid, hue_order()[16] as usize);
        // k moves back up.
        handle_theme_creator_key(&mut c, key(KeyCode::Char('k')));
        assert_eq!(c.editor.as_ref().unwrap().grid, hue_order()[0] as usize);
    }

    #[test]
    fn grid_nav_delta_fast_with_shift() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let k = |code, mods| KeyEvent::new(code, mods);
        // Plain hjkl: one cell (a row is 16 cells).
        assert_eq!(
            grid_nav_delta(&k(KeyCode::Char('h'), KeyModifiers::NONE)),
            Some(-1)
        );
        assert_eq!(
            grid_nav_delta(&k(KeyCode::Char('l'), KeyModifiers::NONE)),
            Some(1)
        );
        assert_eq!(
            grid_nav_delta(&k(KeyCode::Char('j'), KeyModifiers::NONE)),
            Some(16)
        );
        assert_eq!(
            grid_nav_delta(&k(KeyCode::Char('k'), KeyModifiers::NONE)),
            Some(-16)
        );
        // Shift+hjkl (lowercase + SHIFT flag): four cells.
        assert_eq!(
            grid_nav_delta(&k(KeyCode::Char('h'), KeyModifiers::SHIFT)),
            Some(-4)
        );
        assert_eq!(
            grid_nav_delta(&k(KeyCode::Char('l'), KeyModifiers::SHIFT)),
            Some(4)
        );
        assert_eq!(
            grid_nav_delta(&k(KeyCode::Char('j'), KeyModifiers::SHIFT)),
            Some(64)
        );
        assert_eq!(
            grid_nav_delta(&k(KeyCode::Char('k'), KeyModifiers::SHIFT)),
            Some(-64)
        );
        // Kitty protocol sends the shifted uppercase char.
        assert_eq!(
            grid_nav_delta(&k(KeyCode::Char('H'), KeyModifiers::NONE)),
            Some(-4)
        );
        assert_eq!(
            grid_nav_delta(&k(KeyCode::Char('J'), KeyModifiers::NONE)),
            Some(64)
        );
        // Shift+arrows also jump.
        assert_eq!(
            grid_nav_delta(&k(KeyCode::Left, KeyModifiers::SHIFT)),
            Some(-4)
        );
        assert_eq!(
            grid_nav_delta(&k(KeyCode::Down, KeyModifiers::SHIFT)),
            Some(64)
        );
        // Plain arrows still move one cell.
        assert_eq!(
            grid_nav_delta(&k(KeyCode::Left, KeyModifiers::NONE)),
            Some(-1)
        );
        // Non-navigation keys are ignored.
        assert_eq!(
            grid_nav_delta(&k(KeyCode::Char('x'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(grid_nav_delta(&k(KeyCode::Esc, KeyModifiers::NONE)), None);
    }

    #[test]
    fn grid_shift_jumps_four_cells_in_editor() {
        let mut c = ThemeCreator::new();
        c.open("default");
        handle_theme_creator_key(&mut c, key(KeyCode::Char('n')));
        let ed = c.editor.as_mut().unwrap();
        ed.focus = EditorFocus::Grid;
        ed.grid = hue_order()[0] as usize;
        let shift_down = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::SHIFT);
        handle_theme_creator_key(&mut c, shift_down);
        assert_eq!(c.editor.as_ref().unwrap().grid, hue_order()[64] as usize);
    }

    #[test]
    fn list_nav_step_matches_hjkl_and_shift() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let k = |code, mods| KeyEvent::new(code, mods);
        assert_eq!(
            list_nav_step(&k(KeyCode::Char('h'), KeyModifiers::NONE)),
            Some(-1)
        );
        assert_eq!(
            list_nav_step(&k(KeyCode::Char('l'), KeyModifiers::NONE)),
            Some(1)
        );
        assert_eq!(
            list_nav_step(&k(KeyCode::Char('k'), KeyModifiers::NONE)),
            Some(-1)
        );
        assert_eq!(
            list_nav_step(&k(KeyCode::Char('j'), KeyModifiers::NONE)),
            Some(1)
        );
        assert_eq!(list_nav_step(&k(KeyCode::Up, KeyModifiers::NONE)), Some(-1));
        assert_eq!(
            list_nav_step(&k(KeyCode::Down, KeyModifiers::NONE)),
            Some(1)
        );
        let fast = LIST_FAST_STEP as isize;
        assert_eq!(
            list_nav_step(&k(KeyCode::Char('h'), KeyModifiers::SHIFT)),
            Some(-fast)
        );
        assert_eq!(
            list_nav_step(&k(KeyCode::Char('l'), KeyModifiers::SHIFT)),
            Some(fast)
        );
        assert_eq!(
            list_nav_step(&k(KeyCode::Char('H'), KeyModifiers::NONE)),
            Some(-fast)
        );
        assert_eq!(
            list_nav_step(&k(KeyCode::Char('J'), KeyModifiers::NONE)),
            Some(fast)
        );
        assert_eq!(
            list_nav_step(&k(KeyCode::Up, KeyModifiers::SHIFT)),
            Some(-fast)
        );
        assert_eq!(list_nav_step(&k(KeyCode::Enter, KeyModifiers::NONE)), None);
        assert_eq!(
            list_nav_step(&k(KeyCode::Char('d'), KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn list_navigates_with_hl_and_fast_shift() {
        let mut c = ThemeCreator::new();
        c.open("default");
        let first = c.selected_name();
        // 'h' moves up (wraps to the last theme), 'l' moves back down.
        let up = handle_theme_creator_key(&mut c, key(KeyCode::Char('h')));
        assert!(up.is_some());
        assert_ne!(up, first);
        let down = handle_theme_creator_key(&mut c, key(KeyCode::Char('l')));
        assert_eq!(down, first);
        assert!(c.visible, "navigation keeps the creator open");

        // Shift+k skips LIST_FAST_STEP items up, wrapping around the list.
        let len = c.themes.len();
        let shift_k = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::SHIFT);
        handle_theme_creator_key(&mut c, shift_k);
        let expected = (0isize - LIST_FAST_STEP as isize).rem_euclid(len as isize) as usize;
        assert_eq!(c.selected, expected);
        // Uppercase 'J' (kitty protocol) is a fast jump back down.
        let upper_j = KeyEvent::new(KeyCode::Char('J'), KeyModifiers::NONE);
        handle_theme_creator_key(&mut c, upper_j);
        assert_eq!(c.selected, (expected + LIST_FAST_STEP) % len);
    }

    #[test]
    fn hue_order_is_rainbow_then_grayscale() {
        let order = hue_order();
        assert_eq!(order.len(), 256);
        // Chromatic colours come first. The sort key is (hue, saturation,
        // lightness), so the grid opens with the hue-0 (red) group ordered by
        // saturation — ANSI 95 = (135,95,95) = #875F5F leads, not the fully
        // saturated reds.
        let (r0, g0, b0) = ansi256_to_rgb(order[0]);
        assert!(r0 > g0 && g0 == b0, "first cell should be a hue-0 red");
        assert_eq!(hex_label(order[0]), "#875F5F");
        // Achromatic (grayscale) colours sort to the very end: black first,
        // white last. Count them: base grays 0/7/8/15, cube grays
        // 16/59/102/145/188/231, and the 232..=255 ramp.
        let achromatic: Vec<u8> = [0, 7, 8, 15, 16, 59, 102, 145, 188, 231]
            .into_iter()
            .chain(232..=255)
            .collect();
        let start = order.len() - achromatic.len();
        assert_eq!(order[start], 0, "black is the first grayscale cell");
        assert_eq!(order[255], 231, "white is the last grayscale cell");
        assert!(
            order[..start].iter().all(|i| !achromatic.contains(i)),
            "no grayscale colour appears before the ramp"
        );
        // Every ANSI index appears exactly once.
        let mut sorted = order.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..=255).collect::<Vec<u8>>());
    }

    #[test]
    fn save_rejects_empty_and_builtin_names() {
        let mut c = ThemeCreator::new();
        c.open("default");
        handle_theme_creator_key(&mut c, key(KeyCode::Char('n')));
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        // Empty name rejected.
        let r = handle_theme_creator_key(&mut c, ctrl_s);
        assert!(r.is_none());
        assert!(c.editor.is_some());
        // Built-in name rejected.
        c.editor.as_mut().unwrap().name = "dark".into();
        let r = handle_theme_creator_key(&mut c, ctrl_s);
        assert!(r.is_none());
        assert!(c.editor.is_some());
        // Invalid characters rejected.
        c.editor.as_mut().unwrap().name = "bad name!".into();
        let r = handle_theme_creator_key(&mut c, ctrl_s);
        assert!(r.is_none());
    }

    #[test]
    fn delete_requires_confirm_then_removes() {
        // A custom entry must be selectable to confirm; simulate by adding one
        // that points at an existing file path (deletion of a missing file is
        // a silent no-op via delete_theme).
        let mut c = ThemeCreator::new();
        c.open("default");
        c.themes.push(ThemeEntry {
            name: "does_not_exist_custom".into(),
            label: "does_not_exist_custom".into(),
            description: "custom".into(),
            custom: true,
            swatch: [Color::Reset; 4],
        });
        c.selected = c.themes.len() - 1;
        handle_theme_creator_key(&mut c, key(KeyCode::Char('d')));
        assert!(c.confirm_delete);
        assert!(c.visible);
        handle_theme_creator_key(&mut c, key(KeyCode::Char('d')));
        assert!(!c.confirm_delete);
        // No panic; the entry is gone from the refreshed list.
        assert!(!c.themes.iter().any(|t| t.name == "does_not_exist_custom"));
    }

    #[test]
    fn list_scrolls_when_themes_overflow_viewport() {
        let mut c = ThemeCreator::new();
        c.open("default");
        // Simulate many custom themes on disk.
        for i in 0..(LIST_VIEWPORT + 10) {
            c.themes.push(ThemeEntry {
                name: format!("custom_{}", i),
                label: format!("custom_{}", i),
                description: "custom".into(),
                custom: true,
                swatch: [Color::Reset; 4],
            });
        }
        c.selected = 0;
        c.scroll_offset = 0;
        // Scrolling down moves the viewport once the selection leaves it.
        for _ in 0..LIST_VIEWPORT {
            handle_theme_creator_key(&mut c, key(KeyCode::Char('j')));
        }
        assert_eq!(c.scroll_offset, 1);
        assert!(c.selected < c.scroll_offset + LIST_VIEWPORT);
        // Scrolling up moves the viewport back.
        for _ in 0..LIST_VIEWPORT {
            handle_theme_creator_key(&mut c, key(KeyCode::Char('k')));
        }
        assert_eq!(c.scroll_offset, 0);
    }

    #[test]
    fn ensure_visible_clamps_selection_into_viewport() {
        let mut c = ThemeCreator::new();
        c.open("default");
        for i in 0..(LIST_VIEWPORT + 5) {
            c.themes.push(ThemeEntry {
                name: format!("custom_{}", i),
                label: format!("custom_{}", i),
                description: "custom".into(),
                custom: true,
                swatch: [Color::Reset; 4],
            });
        }
        // Selecting far down scrolls the viewport down.
        c.selected = c.themes.len() - 1;
        c.scroll_offset = 0;
        c.ensure_visible();
        assert_eq!(c.scroll_offset, c.themes.len() - LIST_VIEWPORT);
        assert!(c.selected < c.scroll_offset + LIST_VIEWPORT);
        // Selecting far up scrolls the viewport back to the top.
        c.selected = 0;
        c.scroll_offset = c.themes.len() - LIST_VIEWPORT;
        c.ensure_visible();
        assert_eq!(c.scroll_offset, 0);
    }

    #[test]
    fn ansi256_hex_mapping() {
        assert_eq!(hex_label(196), "#FF0000");
        assert_eq!(hex_label(0), "#000000");
        assert_eq!(hex_label(231), "#FFFFFF");
        assert_eq!(hex_label(232), "#080808");
        assert_eq!(hex_label(255), "#EEEEEE");
    }

    #[test]
    fn color_names_are_hue_based() {
        // Base 16 classic names.
        assert_eq!(color_name(0), "black");
        assert_eq!(color_name(15), "white");
        // Fully saturated red leads the hue group.
        assert_eq!(color_name(196), "red");
        // Deep shade gets a "dark" prefix.
        assert_eq!(color_name(52), "dark red");
        // A 6x6x6 yellow (215, 215, 95).
        assert_eq!(color_name(185), "yellow");
        // Hidden grays in the cube and the ramp.
        assert_eq!(color_name(231), "white");
        assert_eq!(color_name(232), "black");
        assert_eq!(color_name(255), "white");
        // Arbitrary RGB (e.g. Rgb palette slots).
        assert_eq!(rgb_hue_name(255, 0, 0), "red");
        assert_eq!(rgb_hue_name(255, 165, 0), "orange");
        assert_eq!(rgb_hue_name(0, 0, 255), "blue");
        assert_eq!(rgb_hue_name(128, 128, 128), "gray");
    }

    #[test]
    fn color_hex_labels() {
        assert_eq!(color_hex(Color::Indexed(196)), "#FF0000");
        assert_eq!(color_hex(Color::Rgb(255, 0, 0)), "#FF0000");
        assert_eq!(color_hex(Color::Indexed(0)), "#000000");
        assert_eq!(color_hex(Color::Rgb(13, 71, 161)), "#0D47A1");
    }

    #[test]
    fn new_slots_map_to_model_name_and_hint() {
        let mut pal = ColorPalette::for_theme("default");
        // Index 12 = model_name, 13 = hint, 14 = effort, 15 = routing,
        // 16 = vim_hint (see SLOT_NAMES).
        set_slot_color(&mut pal, 12, Color::Indexed(196));
        set_slot_color(&mut pal, 13, Color::Indexed(21));
        set_slot_color(&mut pal, 14, Color::Indexed(51));
        set_slot_color(&mut pal, 15, Color::Indexed(33));
        set_slot_color(&mut pal, 16, Color::Indexed(114));
        assert_eq!(pal.model_name, Color::Indexed(196));
        assert_eq!(pal.hint, Color::Indexed(21));
        assert_eq!(pal.effort, Color::Indexed(51));
        assert_eq!(pal.routing, Color::Indexed(33));
        assert_eq!(pal.vim_hint, Color::Indexed(114));
        assert_eq!(slot_color(&pal, 12), Color::Indexed(196));
        assert_eq!(slot_color(&pal, 13), Color::Indexed(21));
        assert_eq!(slot_color(&pal, 14), Color::Indexed(51));
        assert_eq!(slot_color(&pal, 15), Color::Indexed(33));
        assert_eq!(slot_color(&pal, 16), Color::Indexed(114));
        // The slot count drives the editor loop; all new names present.
        assert_eq!(SLOT_NAMES.len(), 17);
        assert_eq!(SLOT_NAMES[12], "model_name");
        assert_eq!(SLOT_NAMES[13], "hint");
        assert_eq!(SLOT_NAMES[14], "effort");
        assert_eq!(SLOT_NAMES[15], "routing");
        assert_eq!(SLOT_NAMES[16], "vim_hint");
    }

    #[test]
    fn editor_palette_tracks_assignments() {
        let mut c = ThemeCreator::new();
        c.open("default");
        handle_theme_creator_key(&mut c, key(KeyCode::Char('n')));
        assert!(c.editor_palette().is_some());
        let ed = c.editor.as_mut().unwrap();
        ed.focus = EditorFocus::Grid;
        ed.grid = 196;
        let slot_before = ed.slot;
        handle_theme_creator_key(&mut c, key(KeyCode::Enter));
        let live = c.editor_palette().unwrap();
        assert_eq!(slot_color(&live, slot_before), Color::Indexed(196));
        // Leaving the editor clears the live palette.
        handle_theme_creator_key(&mut c, key(KeyCode::Esc));
        assert!(c.editor_palette().is_none());
    }
}
