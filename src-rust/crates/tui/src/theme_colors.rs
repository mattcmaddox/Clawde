// theme_colors.rs — Color palette management for accessibility-friendly themes.
//
// Provides color definitions for different themes, with special support for
// Deuteranopia (red-green color blindness) using blue, yellow, and gray palettes.

use ratatui::style::Color;
use std::cell::RefCell;

/// Color palette for a specific theme.
#[derive(Debug, Clone, Copy)]
pub struct ColorPalette {
    /// Error messages and alerts (normally red, but color-blind friendly)
    pub error: Color,
    /// Success indicators (normally green, but color-blind friendly)
    pub success: Color,
    /// Warning/caution messages
    pub warning: Color,
    /// Information messages
    pub info: Color,
    /// Action buttons and interactive elements
    pub action: Color,
    /// Disabled or dimmed states
    pub disabled: Color,
    /// Primary accent color
    pub accent: Color,
    /// Secondary accent
    pub secondary_accent: Color,
    /// The main panel/dialog background colour.
    pub panel_bg: Color,
    /// Text on dark backgrounds
    pub text_light: Color,
    /// Text on light backgrounds
    pub text_dark: Color,
    /// Borders and dividers
    pub border: Color,
    /// The active model name shown in the prompt status line.
    pub model_name: Color,
    /// Muted hint / shortcut text (e.g. "? shortcuts · Ctrl+/ keys").
    pub hint: Color,
    /// Effort-level indicator in the status line (thinking level).
    pub effort: Color,
    /// Routing-strategy badge (free provider) in the status line.
    pub routing: Color,
    /// Vim-mode navigation hint in the status line.
    pub vim_hint: Color,
}

impl ColorPalette {
    /// Get the color palette for a given theme name.
    pub fn for_theme(theme_name: &str) -> Self {
        match theme_name {
            "deuteranopia" => Self::deuteranopia(),
            "dark" => Self::dark(),
            "light" => Self::light(),
            "solarized" => Self::solarized(),
            "nord" => Self::nord(),
            "dracula" => Self::dracula(),
            "monokai" => Self::monokai(),
            "catppuccin" => Self::catppuccin(),
            unknown => {
                // Try loading a custom theme from ~/.clawde/themes/<name>.json
                if let Ok(custom) = Self::from_json_file(unknown) {
                    custom
                } else {
                    Self::default_theme()
                }
            }
        }
    }

    /// Default Clawde theme
    fn default_theme() -> Self {
        Self {
            error: Color::Rgb(255, 87, 51),   // Bright red-orange
            success: Color::Rgb(76, 175, 80), // Green
            warning: Color::Rgb(255, 152, 0), // Orange
            info: Color::Cyan,
            action: Color::Cyan,
            disabled: Color::Rgb(110, 110, 118),
            accent: Color::Rgb(233, 30, 99),
            secondary_accent: Color::Rgb(233, 30, 99),
            panel_bg: Color::Rgb(20, 20, 28),
            text_light: Color::Rgb(235, 235, 240),
            text_dark: Color::Black,
            border: Color::DarkGray,
            model_name: Color::White,
            hint: Color::Rgb(110, 110, 124),
            effort: Color::Rgb(180, 140, 255),
            routing: Color::Rgb(100, 180, 255),
            vim_hint: Color::Rgb(140, 200, 140),
        }
    }

    /// Dark theme
    fn dark() -> Self {
        Self {
            error: Color::Rgb(239, 83, 80),     // Light red
            success: Color::Rgb(129, 199, 132), // Light green
            warning: Color::Rgb(255, 171, 64),  // Light orange
            info: Color::Rgb(100, 181, 246),    // Light blue
            action: Color::Rgb(100, 181, 246),
            disabled: Color::Rgb(97, 97, 97),
            accent: Color::Rgb(100, 181, 246),
            secondary_accent: Color::Rgb(229, 57, 53),
            panel_bg: Color::Rgb(33, 33, 33),
            text_light: Color::Rgb(229, 229, 229),
            text_dark: Color::Rgb(33, 33, 33),
            border: Color::Rgb(66, 66, 66),
            model_name: Color::White,
            hint: Color::Rgb(150, 150, 165),
            effort: Color::Rgb(180, 140, 255),
            routing: Color::Rgb(100, 180, 255),
            vim_hint: Color::Rgb(140, 200, 140),
        }
    }

    /// Light theme
    fn light() -> Self {
        Self {
            error: Color::Rgb(211, 47, 47),    // Dark red
            success: Color::Rgb(27, 94, 32),   // Dark green
            warning: Color::Rgb(230, 124, 13), // Dark orange
            info: Color::Rgb(13, 71, 161),     // Dark blue
            action: Color::Blue,
            disabled: Color::Rgb(189, 189, 189),
            accent: Color::Blue,
            secondary_accent: Color::Rgb(194, 24, 91),
            panel_bg: Color::White,
            text_light: Color::White,
            text_dark: Color::Black,
            border: Color::Rgb(189, 189, 189),
            model_name: Color::Rgb(33, 33, 33),
            hint: Color::Rgb(90, 90, 100),
            effort: Color::Rgb(120, 80, 190),
            routing: Color::Rgb(50, 130, 220),
            vim_hint: Color::Rgb(80, 150, 90),
        }
    }

    /// Solarized Dark theme
    fn solarized() -> Self {
        Self {
            error: Color::Rgb(220, 50, 47),   // Solarized red
            success: Color::Rgb(133, 153, 0), // Solarized green
            warning: Color::Rgb(181, 137, 0), // Solarized yellow
            info: Color::Rgb(38, 139, 210),   // Solarized blue
            action: Color::Rgb(38, 139, 210),
            disabled: Color::Rgb(88, 110, 117),
            accent: Color::Rgb(38, 139, 210),
            secondary_accent: Color::Rgb(108, 113, 196),
            panel_bg: Color::Rgb(0, 43, 54),
            text_light: Color::Rgb(131, 148, 150),
            text_dark: Color::Rgb(0, 43, 54),
            border: Color::Rgb(7, 54, 66),
            model_name: Color::Rgb(238, 232, 213),
            hint: Color::Rgb(88, 110, 117),
            effort: Color::Rgb(108, 113, 196),
            routing: Color::Rgb(38, 139, 210),
            vim_hint: Color::Rgb(133, 153, 0),
        }
    }

    /// Nord theme
    fn nord() -> Self {
        Self {
            error: Color::Rgb(191, 97, 106),    // Nord red
            success: Color::Rgb(163, 190, 140), // Nord green
            warning: Color::Rgb(235, 203, 139), // Nord yellow
            info: Color::Rgb(136, 192, 208),    // Nord blue
            action: Color::Rgb(136, 192, 208),
            disabled: Color::Rgb(76, 86, 106),
            accent: Color::Rgb(136, 192, 208),
            secondary_accent: Color::Rgb(191, 97, 106),
            panel_bg: Color::Rgb(46, 52, 64),
            text_light: Color::Rgb(236, 239, 244),
            text_dark: Color::Rgb(46, 52, 64),
            border: Color::Rgb(67, 76, 94),
            model_name: Color::Rgb(236, 239, 244),
            hint: Color::Rgb(94, 105, 127),
            effort: Color::Rgb(180, 160, 220),
            routing: Color::Rgb(136, 192, 208),
            vim_hint: Color::Rgb(163, 190, 140),
        }
    }

    /// Dracula theme
    fn dracula() -> Self {
        Self {
            error: Color::Rgb(255, 85, 85),     // Dracula red
            success: Color::Rgb(80, 250, 123),  // Dracula green
            warning: Color::Rgb(241, 250, 140), // Dracula yellow
            info: Color::Rgb(139, 233, 253),    // Dracula blue
            action: Color::Rgb(139, 233, 253),
            disabled: Color::Rgb(98, 114, 164),
            accent: Color::Rgb(139, 233, 253),
            secondary_accent: Color::Rgb(189, 147, 249),
            panel_bg: Color::Rgb(40, 42, 54),
            text_light: Color::Rgb(248, 248, 242),
            text_dark: Color::Rgb(40, 42, 54),
            border: Color::Rgb(68, 71, 90),
            model_name: Color::Rgb(248, 248, 242),
            hint: Color::Rgb(98, 114, 164),
            effort: Color::Rgb(189, 147, 249),
            routing: Color::Rgb(139, 233, 253),
            vim_hint: Color::Rgb(80, 250, 123),
        }
    }

    /// Monokai theme
    fn monokai() -> Self {
        Self {
            error: Color::Rgb(249, 38, 114), // Monokai magenta (used for errors)
            success: Color::Rgb(166, 226, 46), // Monokai green
            warning: Color::Rgb(253, 151, 31), // Monokai orange
            info: Color::Rgb(102, 217, 239), // Monokai cyan
            action: Color::Rgb(102, 217, 239),
            disabled: Color::Rgb(117, 113, 94),
            accent: Color::Rgb(102, 217, 239),
            secondary_accent: Color::Rgb(249, 38, 114),
            panel_bg: Color::Rgb(39, 40, 34),
            text_light: Color::Rgb(248, 248, 242),
            text_dark: Color::Rgb(39, 40, 34),
            border: Color::Rgb(75, 75, 75),
            model_name: Color::Rgb(248, 248, 242),
            hint: Color::Rgb(117, 113, 94),
            effort: Color::Rgb(180, 140, 255),
            routing: Color::Rgb(102, 217, 239),
            vim_hint: Color::Rgb(166, 226, 46),
        }
    }
    /// Deuteranopia (red-green color blind) theme
    /// Uses blue, yellow, and gray to avoid red/green distinction
    fn deuteranopia() -> Self {
        Self {
            error: Color::Rgb(255, 140, 0),   // Orange (not red)
            success: Color::Rgb(0, 150, 200), // Blue (not green)
            warning: Color::Rgb(255, 180, 0), // Gold/Yellow
            info: Color::Cyan,
            action: Color::Rgb(0, 150, 200), // Blue action buttons
            disabled: Color::Rgb(120, 120, 120), // Neutral gray
            accent: Color::Rgb(0, 150, 200), // Blue accent
            secondary_accent: Color::Rgb(180, 140, 255), // Purple accent
            panel_bg: Color::Rgb(18, 18, 18),
            text_light: Color::Rgb(220, 220, 220),
            text_dark: Color::Rgb(40, 40, 40),
            border: Color::Rgb(100, 100, 100),
            model_name: Color::Rgb(220, 220, 220),
            hint: Color::Rgb(120, 120, 120),
            effort: Color::Rgb(180, 140, 255),
            routing: Color::Rgb(0, 150, 200),
            vim_hint: Color::Rgb(200, 170, 60),
        }
    }

    /// Catppuccin Mocha theme (warm, popular dark theme)
    fn catppuccin() -> Self {
        Self {
            error: Color::Rgb(243, 139, 168),   // Catppuccin red/mauve
            success: Color::Rgb(166, 227, 161), // Catppuccin green
            warning: Color::Rgb(249, 226, 175), // Catppuccin yellow
            info: Color::Rgb(137, 180, 250),    // Catppuccin blue
            action: Color::Rgb(137, 180, 250),
            disabled: Color::Rgb(108, 112, 134),
            accent: Color::Rgb(203, 166, 247), // Catppuccin mauve
            secondary_accent: Color::Rgb(245, 194, 231), // Catppuccin pink
            panel_bg: Color::Rgb(30, 30, 46),
            text_light: Color::Rgb(205, 214, 244),
            text_dark: Color::Rgb(30, 30, 46),
            border: Color::Rgb(69, 71, 90),
            model_name: Color::Rgb(205, 214, 244),
            hint: Color::Rgb(147, 153, 178),
            effort: Color::Rgb(203, 166, 247),
            routing: Color::Rgb(137, 180, 250),
            vim_hint: Color::Rgb(166, 227, 161),
        }
    }

    /// Load a custom theme from a JSON file in ~/.clawde/themes/<name>.json.
    fn from_json_file(name: &str) -> Result<Self, ()> {
        // Sanitize: only allow alphanumeric and underscore for theme names
        if !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            return Err(());
        }
        let dir = clawde_core::Settings::config_dir().join("themes");
        let file = dir.join(format!("{}.json", name));
        if !file.exists() {
            return Err(());
        }
        let data = std::fs::read_to_string(&file).map_err(|_| ())?;
        let v: serde_json::Value = serde_json::from_str(&data).map_err(|_| ())?;

        let color = |key: &str| -> Result<Color, ()> {
            let Some(v) = v.get(key) else { return Err(()) };
            color_from_json(v).ok_or(())
        };

        Ok(Self {
            error: color("error")?,
            success: color("success")?,
            warning: color("warning")?,
            info: color("info")?,
            action: color("action")?,
            disabled: color("disabled")?,
            accent: color("accent")?,
            secondary_accent: color("secondary_accent")?,
            panel_bg: color("panel_bg")
                .or_else(|_| color("text_dark"))
                .unwrap_or(Color::Rgb(20, 20, 28)),
            text_light: color("text_light")?,
            text_dark: color("text_dark")?,
            border: color("border")?,
            // Newer keys are optional so older custom theme files still
            // load; they fall back to the nearest existing colour.
            model_name: color("model_name")
                .or_else(|_| color("text_light"))
                .unwrap_or(Color::White),
            hint: color("hint")
                .or_else(|_| color("disabled"))
                .unwrap_or(Color::Rgb(110, 110, 124)),
            effort: color("effort")
                .or_else(|_| color("secondary_accent"))
                .unwrap_or(Color::Rgb(180, 140, 255)),
            routing: color("routing")
                .or_else(|_| color("action"))
                .unwrap_or(Color::Rgb(100, 180, 255)),
            vim_hint: color("vim_hint")
                .or_else(|_| color("success"))
                .unwrap_or(Color::Rgb(140, 200, 140)),
        })
    }

    /// Serialize this palette to the JSON object used by
    /// `~/.clawde/themes/<name>.json`. Indexed (ANSI 256) colours are written
    /// as plain integers (0-255); RGB colours as `[r,g,b]` arrays; named
    /// colours are converted to their closest RGB triple.
    pub fn to_json_value(&self) -> serde_json::Value {
        serde_json::json!({
            "error": color_to_json(self.error),
            "success": color_to_json(self.success),
            "warning": color_to_json(self.warning),
            "info": color_to_json(self.info),
            "action": color_to_json(self.action),
            "disabled": color_to_json(self.disabled),
            "accent": color_to_json(self.accent),
            "secondary_accent": color_to_json(self.secondary_accent),
            "panel_bg": color_to_json(self.panel_bg),
            "text_light": color_to_json(self.text_light),
            "text_dark": color_to_json(self.text_dark),
            "border": color_to_json(self.border),
            "model_name": color_to_json(self.model_name),
            "hint": color_to_json(self.hint),
            "effort": color_to_json(self.effort),
            "routing": color_to_json(self.routing),
            "vim_hint": color_to_json(self.vim_hint),
        })
    }
}

// ---------------------------------------------------------------------------
// Custom theme file CRUD (~/.clawde/themes/<name>.json)
// ---------------------------------------------------------------------------

/// Return the directory holding custom theme JSON files.
pub fn themes_dir() -> std::path::PathBuf {
    clawde_core::Settings::config_dir().join("themes")
}

/// Whether `name` is a legal custom-theme filename (alphanumeric + underscore).
pub fn valid_theme_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// Serialize a single `Color` to JSON: indexed → integer, RGB → array, named → RGB triple.
pub fn color_to_json(c: Color) -> serde_json::Value {
    match c {
        Color::Indexed(n) => serde_json::json!(n),
        Color::Rgb(r, g, b) => serde_json::json!([r, g, b]),
        other => {
            let (r, g, b) = named_color_rgb(other);
            serde_json::json!([r, g, b])
        }
    }
}

/// Parse a JSON value into a `Color`: integer 0-255 → indexed, `[r,g,b]` → RGB.
pub fn color_from_json(v: &serde_json::Value) -> Option<Color> {
    if let Some(arr) = v.as_array() {
        if arr.len() == 3 {
            let r = arr[0].as_u64().unwrap_or(0) as u8;
            let g = arr[1].as_u64().unwrap_or(0) as u8;
            let b = arr[2].as_u64().unwrap_or(0) as u8;
            return Some(Color::Rgb(r, g, b));
        }
        return None;
    }
    if let Some(n) = v.as_u64() {
        if n <= 255 {
            return Some(Color::Indexed(n as u8));
        }
    }
    None
}

/// Map ratatui's named colours to the ANSI RGB values they render as.
fn named_color_rgb(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Black => (0, 0, 0),
        Color::Red => (128, 0, 0),
        Color::Green => (0, 128, 0),
        Color::Yellow => (128, 128, 0),
        Color::Blue => (0, 0, 128),
        Color::Magenta => (128, 0, 128),
        Color::Cyan => (0, 128, 128),
        Color::Gray => (128, 128, 128),
        Color::DarkGray => (64, 64, 64),
        Color::LightRed => (255, 0, 0),
        Color::LightGreen => (0, 255, 0),
        Color::LightYellow => (255, 255, 0),
        Color::LightBlue => (0, 0, 255),
        Color::LightMagenta => (255, 0, 255),
        Color::LightCyan => (0, 255, 255),
        Color::White => (255, 255, 255),
        _ => (0, 0, 0),
    }
}

/// Persist `palette` as `~/.clawde/themes/<name>.json`.
pub fn save_theme(name: &str, palette: &ColorPalette) -> anyhow::Result<()> {
    if !valid_theme_name(name) {
        anyhow::bail!("Theme name must contain only letters, numbers, and underscores");
    }
    let dir = themes_dir();
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(format!("{}.json", name));
    let data = serde_json::to_string_pretty(&palette.to_json_value())?;
    std::fs::write(file, data)?;
    Ok(())
}

/// List the names of all custom themes saved on disk, sorted.
pub fn list_custom_themes() -> Vec<String> {
    let dir = themes_dir();
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    if valid_theme_name(stem) {
                        names.push(stem.to_string());
                    }
                }
            }
        }
    }
    names.sort();
    names
}

/// Delete a custom theme file (no-op when it does not exist).
pub fn delete_theme(name: &str) -> anyhow::Result<()> {
    if !valid_theme_name(name) {
        anyhow::bail!("Theme name must contain only letters, numbers, and underscores");
    }
    let file = themes_dir().join(format!("{}.json", name));
    if file.exists() {
        std::fs::remove_file(file)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Thread-local palette — set once per frame by render_app
// ---------------------------------------------------------------------------

thread_local! {
    pub static CURRENT_PALETTE: RefCell<ColorPalette> = RefCell::new(ColorPalette::default_theme());
}

/// Return a copy of the palette currently active for this render frame.
#[inline]
pub fn current_palette() -> ColorPalette {
    CURRENT_PALETTE.with(|p| *p.borrow())
}

/// Get appropriate color for a given theme based on message type/role.
#[allow(dead_code)]
pub fn get_message_indicator_color(theme_name: &str, role: &str) -> Color {
    let palette = ColorPalette::for_theme(theme_name);
    match role {
        "user" => palette.accent,
        "assistant" => palette.secondary_accent,
        "system" => palette.disabled,
        "tool" => palette.action,
        _ => palette.text_light,
    }
}

/// Get error indicator color for given theme (always prominent, never red in deuteranopia).
#[allow(dead_code)]
pub fn get_error_color(theme_name: &str) -> Color {
    ColorPalette::for_theme(theme_name).error
}

/// Get success indicator color for given theme (blue instead of green in deuteranopia).
#[allow(dead_code)]
pub fn get_success_color(theme_name: &str) -> Color {
    ColorPalette::for_theme(theme_name).success
}

/// Get warning indicator color for given theme (yellow/gold instead of orange in deuteranopia).
#[allow(dead_code)]
pub fn get_warning_color(theme_name: &str) -> Color {
    ColorPalette::for_theme(theme_name).warning
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_builtin_themes_define_new_slots() {
        for name in [
            "default",
            "dark",
            "light",
            "solarized",
            "nord",
            "dracula",
            "monokai",
            "catppuccin",
            "deuteranopia",
        ] {
            let pal = ColorPalette::for_theme(name);
            for (slot, value) in [
                ("model_name", pal.model_name),
                ("hint", pal.hint),
                ("effort", pal.effort),
                ("routing", pal.routing),
                ("vim_hint", pal.vim_hint),
            ] {
                assert_ne!(
                    value,
                    Color::Reset,
                    "theme '{}' must define a {} colour",
                    name,
                    slot
                );
            }
        }
    }

    #[test]
    fn json_round_trip_preserves_new_slots() {
        // Named colours (e.g. Color::White) serialize to an RGB triple and
        // parse back as Color::Rgb, so compare against the serialized form.
        let pal = ColorPalette::default_theme();
        let v = pal.to_json_value();
        for (key, value) in [
            ("model_name", pal.model_name),
            ("hint", pal.hint),
            ("effort", pal.effort),
            ("routing", pal.routing),
            ("vim_hint", pal.vim_hint),
        ] {
            assert_eq!(
                color_from_json(v.get(key).unwrap()),
                color_from_json(&color_to_json(value)),
                "slot '{}' must round-trip through JSON",
                key
            );
        }
    }

    #[test]
    fn indexed_new_slots_survive_json() {
        let mut pal = ColorPalette::for_theme("default");
        pal.model_name = Color::Indexed(196);
        pal.hint = Color::Indexed(21);
        pal.effort = Color::Indexed(51);
        pal.routing = Color::Indexed(33);
        pal.vim_hint = Color::Indexed(114);
        let v = pal.to_json_value();
        assert_eq!(
            color_from_json(v.get("model_name").unwrap()),
            Some(Color::Indexed(196))
        );
        assert_eq!(
            color_from_json(v.get("hint").unwrap()),
            Some(Color::Indexed(21))
        );
        assert_eq!(
            color_from_json(v.get("effort").unwrap()),
            Some(Color::Indexed(51))
        );
        assert_eq!(
            color_from_json(v.get("routing").unwrap()),
            Some(Color::Indexed(33))
        );
        assert_eq!(
            color_from_json(v.get("vim_hint").unwrap()),
            Some(Color::Indexed(114))
        );
    }
}
