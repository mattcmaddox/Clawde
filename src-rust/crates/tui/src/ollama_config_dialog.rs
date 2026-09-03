// ollama_config_dialog.rs — Modal dialog for configuring Ollama connection.
//
// Two-phase UX:
//   1. Default view: shows current host + model, Enter = connect (fast path)
//   2. Edit mode: j/k navigates fields, Enter = edit, tab = switch field
//   3. Model picker: pings server, shows available models
//
// Health dot (●) next to host: green = reachable, red = unreachable, dim = untested.
// Follows the free_mode_dialog health dot convention.

use ratatui::layout::Rect;
use ratatui::prelude::Stylize;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::overlays::{centered_rect, render_dark_overlay, render_dialog_bg, CLAWDE_PANEL_BG};
use crate::vim_search::VimSearch;
use std::cell::Cell;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Which field is selected for editing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OllamaConfigField {
    Host,
    Model,
    Mode,
    /// Common request options editor (num_ctx / num_predict / keep_alive /
    /// temperature / top_p), cycled as one row.
    Options,
}

/// Phase of the dialog flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OllamaConfigPhase {
    /// Default view: show current config, Enter = connect.
    Default,
    /// Editing a field (host or model).
    EditField(OllamaConfigField),
    /// Pinging the server to verify connectivity.
    Pinging,
    /// Ping failed, showing error.
    PingFailed(String),
    /// The server responded successfully but has no installed models.
    NoModels,
    /// Ping succeeded, showing model list.
    SelectModel,
}

/// Health status of the Ollama server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    /// Not yet tested.
    Untested,
    /// Server is reachable.
    Healthy,
    /// Server is unreachable.
    Unhealthy,
}

/// A model returned by Ollama's `/api/tags` endpoint.
/// Re-export from the query crate where `QueryEvent` lives.
pub use clawde_query::OllamaPingModel as OllamaModel;

const MODEL_PICKER_VISIBLE_ROWS: usize = 10;

/// Extension trait for display helpers.
pub trait OllamaModelExt {
    fn size_display(&self) -> String;
}

impl OllamaModelExt for OllamaModel {
    /// Human-readable size (e.g., "1.8GB").
    fn size_display(&self) -> String {
        let gb = self.size as f64 / (1024.0 * 1024.0 * 1024.0);
        if gb >= 1.0 {
            format!("{:.1}GB", gb)
        } else {
            let mb = self.size as f64 / (1024.0 * 1024.0);
            format!("{:.0}MB", mb)
        }
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

pub struct OllamaConfigDialogState {
    pub visible: bool,
    /// The area used by this dialog in the last render (for click-outside detection).
    pub last_rect: Cell<Rect>,
    pub host_url_input: String,
    pub model_input: String,
    /// Connectivity mode for the session: Online (network tools allowed) or
    /// Isolated (network tools blocked). Applied immediately on change.
    pub mode_isolated: bool,
    /// Canonical Ollama request options, preset-label keyed (see
    /// `clawde_api::providers::ollama_options`). Empty string = unset
    /// ("Ollama/model default").
    pub num_ctx_label: String,
    pub num_predict_label: String,
    pub keep_alive_label: String,
    pub temperature_label: String,
    pub top_p_label: String,
    /// Which common-option row is focused while the Options field is active
    /// (index into `OPTION_KEYS_ORDER`).
    pub option_key_idx: usize,
    /// Cursor position within the active field (byte index).
    pub cursor_pos: usize,
    pub active_field: OllamaConfigField,
    pub phase: OllamaConfigPhase,
    pub models: Vec<OllamaModel>,
    pub selected_model_idx: usize,
    pub model_scroll_offset: usize,
    /// Exact model names currently loaded in the server's VRAM (from the
    /// periodic `/api/ps` poll). Drives the loaded-state markers in the
    /// model picker; kept outside `models` so it survives refreshes.
    pub loaded_model_names: Vec<String>,
    pub health: HealthStatus,
    /// Vim-modal insert state (only used when vim is enabled).
    pub vim_search: VimSearch,
}

impl Default for OllamaConfigDialogState {
    fn default() -> Self {
        Self::new()
    }
}

impl OllamaConfigDialogState {
    pub fn new() -> Self {
        Self {
            visible: false,
            last_rect: Cell::new(Rect::default()),
            host_url_input: String::new(),
            model_input: String::new(),
            mode_isolated: false,
            num_ctx_label: String::new(),
            num_predict_label: String::new(),
            keep_alive_label: String::new(),
            temperature_label: String::new(),
            top_p_label: String::new(),
            option_key_idx: 0,
            cursor_pos: 0,
            active_field: OllamaConfigField::Host,
            phase: OllamaConfigPhase::Default,
            models: Vec::new(),
            selected_model_idx: 0,
            model_scroll_offset: 0,
            loaded_model_names: Vec::new(),
            health: HealthStatus::Untested,
            vim_search: VimSearch::new(),
        }
    }

    /// Replace the loaded-in-VRAM snapshot (exact model names from
    /// `/api/ps`). Called on screen open and when a poll updates.
    pub fn set_loaded_model_names(&mut self, names: Vec<String>) {
        self.loaded_model_names = names;
    }

    /// Seed the mode + option rows from persisted settings. Called on open;
    /// uses the centralized preset tables so the screen can never drift from
    /// the request pipeline.
    pub fn set_mode_and_options(
        &mut self,
        mode_isolated: bool,
        options: &serde_json::Map<String, serde_json::Value>,
    ) {
        use clawde_api::providers::ollama_options as oo;
        self.mode_isolated = mode_isolated;
        self.num_ctx_label = options
            .get("num_ctx")
            .and_then(|v| v.as_u64())
            .filter(|n| *n > 0)
            .map(oo::num_ctx_to_label)
            .unwrap_or_default();
        self.num_predict_label = options
            .get("num_predict")
            .and_then(|v| v.as_u64())
            .filter(|n| *n > 0)
            .map(oo::num_predict_to_label)
            .unwrap_or_default();
        self.keep_alive_label = options
            .get("keep_alive")
            .and_then(|v| v.as_i64())
            .map(oo::keep_alive_to_label)
            .unwrap_or_default();
        self.temperature_label = options
            .get("temperature")
            .and_then(|v| v.as_f64())
            .map(oo::temperature_to_label)
            .unwrap_or_default();
        self.top_p_label = options
            .get("top_p")
            .and_then(|v| v.as_f64())
            .map(oo::top_p_to_label)
            .unwrap_or_default();
    }

    /// The common-option rows in display order (key, current label). The
    /// value `""` means unset — "Ollama/model default" in the UI.
    pub const OPTION_KEYS_ORDER: [&str; 5] = [
        "num_ctx",
        "num_predict",
        "keep_alive",
        "temperature",
        "top_p",
    ];

    fn option_label(&self, key: &str) -> &str {
        match key {
            "num_ctx" => &self.num_ctx_label,
            "num_predict" => &self.num_predict_label,
            "keep_alive" => &self.keep_alive_label,
            "temperature" => &self.temperature_label,
            "top_p" => &self.top_p_label,
            _ => "",
        }
    }

    fn set_option_label(&mut self, key: &str, label: String) {
        match key {
            "num_ctx" => self.num_ctx_label = label,
            "num_predict" => self.num_predict_label = label,
            "keep_alive" => self.keep_alive_label = label,
            "temperature" => self.temperature_label = label,
            "top_p" => self.top_p_label = label,
            _ => {}
        }
    }

    /// Move the option sub-cursor (j/k while the Options field is active).
    pub fn move_option_key(&mut self, delta: i32) {
        let len = Self::OPTION_KEYS_ORDER.len() as i32;
        let next = (self.option_key_idx as i32 + delta).rem_euclid(len);
        self.option_key_idx = next as usize;
    }

    /// Cycle the focused option's value through its preset list, wrapping
    /// through unset. Left/Right while the Options field is active.
    pub fn cycle_option_value(&mut self, direction: i32) {
        use clawde_api::providers::ollama_options as oo;
        let key = Self::OPTION_KEYS_ORDER[self.option_key_idx];
        let presets: Vec<String> = match key {
            "num_ctx" => oo::OLLAMA_CTX_PRESETS
                .iter()
                .map(|(l, _)| l.to_string())
                .collect(),
            "num_predict" => oo::OLLAMA_PREDICT_PRESETS
                .iter()
                .map(|(l, _)| l.to_string())
                .collect(),
            "keep_alive" => oo::OLLAMA_KEEP_ALIVE_PRESETS
                .iter()
                .map(|(l, _)| l.to_string())
                .collect(),
            "temperature" => oo::OLLAMA_TEMPERATURE_PRESETS
                .iter()
                .map(|(l, _)| l.to_string())
                .collect(),
            "top_p" => oo::OLLAMA_TOP_P_PRESETS
                .iter()
                .map(|(l, _)| l.to_string())
                .collect(),
            _ => return,
        };
        // "" (unset) first, then presets, wrapping both directions.
        let mut all = vec![String::new()];
        all.extend(presets);
        let current = self.option_label(key).to_string();
        let idx = all.iter().position(|l| *l == current).unwrap_or(0);
        let next = if direction >= 0 {
            (idx + 1) % all.len()
        } else {
            (idx + all.len() - 1) % all.len()
        };
        self.set_option_label(key, all[next].clone());
    }

    /// Toggle the connectivity mode row (Online ↔ Isolated). The caller
    /// applies it immediately via the shared mode helper.
    pub fn toggle_mode(&mut self) {
        self.mode_isolated = !self.mode_isolated;
    }

    /// The effective-options preview rows (label, applied status) from
    /// the centralized helper — spec §Option defaults and UI priorities.
    pub fn effective_preview_rows(&self) -> Vec<(String, String)> {
        use clawde_api::providers::ollama_options as oo;
        let mut raw = serde_json::Map::new();
        if let Some(n) = oo::label_to_num_ctx(&self.num_ctx_label) {
            raw.insert("num_ctx".to_string(), serde_json::json!(n));
        }
        if let Some(n) = oo::label_to_num_predict(&self.num_predict_label) {
            raw.insert("num_predict".to_string(), serde_json::json!(n));
        }
        if let Some(n) = oo::label_to_keep_alive(&self.keep_alive_label) {
            raw.insert("keep_alive".to_string(), serde_json::json!(n));
        }
        if let Some(t) = oo::label_to_temperature(&self.temperature_label) {
            raw.insert("temperature".to_string(), serde_json::json!(t));
        }
        if let Some(t) = oo::label_to_top_p(&self.top_p_label) {
            raw.insert("top_p".to_string(), serde_json::json!(t));
        }
        oo::effective_preview(&raw)
    }

    /// Whether an exact model tag is currently loaded in VRAM. Ollama treats
    /// a bare tag as `:latest`, so `foo` and `foo:latest` are the same model;
    /// any other explicit tag (`foo:7b`) is distinct.
    pub fn is_model_loaded(&self, name: &str) -> bool {
        let canonical = |tag: &str| {
            tag.strip_suffix(":latest")
                .map(|bare| bare.to_string())
                .unwrap_or_else(|| tag.to_string())
        };
        let wanted = canonical(name);
        self.loaded_model_names
            .iter()
            .any(|loaded| canonical(loaded) == wanted)
    }

    /// Open the dialog with optional current values.
    pub fn open(&mut self, current_url: Option<String>, current_model: Option<String>) {
        self.visible = true;
        self.host_url_input = current_url.unwrap_or_default();
        self.model_input = current_model.unwrap_or_default();
        self.cursor_pos = 0;
        self.option_key_idx = 0;
        self.active_field = OllamaConfigField::Host;
        self.phase = OllamaConfigPhase::Default;
        self.models.clear();
        self.selected_model_idx = 0;
        self.model_scroll_offset = 0;
        self.health = HealthStatus::Untested;
        self.vim_search.reset();
        // NOTE: mode/option labels are NOT cleared here — the caller seeds
        // them via `set_mode_and_options` right after `open`.
    }

    /// Close and clear the dialog.
    pub fn close(&mut self) {
        self.visible = false;
        self.host_url_input.clear();
        self.model_input.clear();
        self.phase = OllamaConfigPhase::Default;
        self.models.clear();
        self.selected_model_idx = 0;
        self.model_scroll_offset = 0;
        self.health = HealthStatus::Untested;
        self.vim_search.reset();
    }

    /// Enter edit mode for the active field.
    pub fn start_edit(&mut self) {
        // Only free-text fields get a cursor; Mode/Options are value rows.
        match self.active_field {
            OllamaConfigField::Host | OllamaConfigField::Model => {}
            OllamaConfigField::Mode | OllamaConfigField::Options => return,
        }
        self.phase = OllamaConfigPhase::EditField(self.active_field);
        // Set cursor to end of current text
        self.cursor_pos = match self.active_field {
            OllamaConfigField::Host => self.host_url_input.len(),
            OllamaConfigField::Model => self.model_input.len(),
            OllamaConfigField::Mode | OllamaConfigField::Options => 0,
        };
        self.vim_search.enter_insert();
    }

    /// Return to default view from edit mode.
    pub fn cancel_edit(&mut self) {
        self.phase = OllamaConfigPhase::Default;
        self.vim_search.reset();
    }

    /// Navigate to the next field (j or Down).
    pub fn move_next_field(&mut self) {
        self.active_field = match self.active_field {
            OllamaConfigField::Host => OllamaConfigField::Model,
            OllamaConfigField::Model => OllamaConfigField::Mode,
            OllamaConfigField::Mode => OllamaConfigField::Options,
            OllamaConfigField::Options => OllamaConfigField::Host,
        };
    }

    /// Navigate to the previous field (k or Up).
    pub fn move_prev_field(&mut self) {
        self.active_field = match self.active_field {
            OllamaConfigField::Host => OllamaConfigField::Options,
            OllamaConfigField::Model => OllamaConfigField::Host,
            OllamaConfigField::Mode => OllamaConfigField::Model,
            OllamaConfigField::Options => OllamaConfigField::Mode,
        };
    }

    /// Insert a character at the cursor position (edit mode only).
    pub fn insert_char(&mut self, c: char) {
        if let OllamaConfigPhase::EditField(field) = self.phase {
            match field {
                OllamaConfigField::Host => {
                    self.host_url_input.insert(self.cursor_pos, c);
                    self.cursor_pos += c.len_utf8();
                    // Reset health when host is edited
                    self.health = HealthStatus::Untested;
                }
                OllamaConfigField::Model => {
                    self.model_input.insert(self.cursor_pos, c);
                    self.cursor_pos += c.len_utf8();
                }
                // Mode/Options rows have no text cursor.
                OllamaConfigField::Mode | OllamaConfigField::Options => {}
            }
        }
    }

    /// Delete the character before the cursor (edit mode only).
    pub fn backspace(&mut self) {
        if self.cursor_pos == 0 {
            return;
        }
        if let OllamaConfigPhase::EditField(field) = self.phase {
            match field {
                OllamaConfigField::Host => {
                    // Find the previous character boundary
                    let prev_char_start = self.host_url_input[..self.cursor_pos]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    self.host_url_input.drain(prev_char_start..self.cursor_pos);
                    self.cursor_pos = prev_char_start;
                    // Reset health when host is edited
                    self.health = HealthStatus::Untested;
                }
                OllamaConfigField::Model => {
                    let prev_char_start = self.model_input[..self.cursor_pos]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    self.model_input.drain(prev_char_start..self.cursor_pos);
                    self.cursor_pos = prev_char_start;
                }
                // Mode/Options rows have no text cursor.
                OllamaConfigField::Mode | OllamaConfigField::Options => {}
            }
        }
    }

    /// Move cursor left (edit mode only).
    pub fn move_cursor_left(&mut self) {
        if self.cursor_pos == 0 {
            return;
        }
        let text = match self.phase {
            OllamaConfigPhase::EditField(OllamaConfigField::Host) => &self.host_url_input,
            OllamaConfigPhase::EditField(OllamaConfigField::Model) => &self.model_input,
            _ => return,
        };
        let prev_char_start = text[..self.cursor_pos]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.cursor_pos = prev_char_start;
    }

    /// Move cursor right (edit mode only).
    pub fn move_cursor_right(&mut self) {
        let text = match self.phase {
            OllamaConfigPhase::EditField(OllamaConfigField::Host) => &self.host_url_input,
            OllamaConfigPhase::EditField(OllamaConfigField::Model) => &self.model_input,
            _ => return,
        };
        if self.cursor_pos >= text.len() {
            return;
        }
        let next_char_start = text[self.cursor_pos..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| self.cursor_pos + i)
            .unwrap_or(text.len());
        self.cursor_pos = next_char_start;
    }

    /// Validate the host URL format.
    /// Returns Ok(normalized_url) or Err(error_message).
    pub fn validate_host_url(&self) -> Result<String, String> {
        let url = self.host_url_input.trim();
        if url.is_empty() {
            return Err("Host URL is required".to_string());
        }
        // Try to normalize the URL
        if url.contains("[::1]") || url.contains("::1") {
            return Err("Ollama must run on another computer's GPU".to_string());
        }
        let normalized = clawde_core::config::normalize_ollama_host(url).ok_or_else(|| {
            if !url.starts_with("http://") && !url.starts_with("https://") {
                "URL must start with http:// or https://".to_string()
            } else {
                "Ollama must run on another computer's GPU or use a remote hostname".to_string()
            }
        })?;
        Ok(normalized)
    }

    /// Validate the model name format.
    /// Returns Ok(model_name) or Err(error_message).
    pub fn validate_model_name(&self) -> Result<String, String> {
        let model = self.model_input.trim();
        if model.is_empty() {
            return Err("Model name is required".to_string());
        }
        // Basic validation: no spaces, no special characters that would break things
        if model.contains(' ') {
            return Err("Model name cannot contain spaces".to_string());
        }
        Ok(model.to_string())
    }

    /// Return `true` when the host URL is non-empty and ready to connect.
    pub fn can_connect(&self) -> bool {
        !self.host_url_input.trim().is_empty()
    }

    /// Transition to the pinging phase.
    pub fn start_ping(&mut self) {
        self.phase = OllamaConfigPhase::Pinging;
    }

    /// Handle a successful ping: store models and transition to selection.
    /// Returns `Some(removed_model)` when the previously selected model no
    /// longer exists on the server (spec §Model/server behavior: choose the
    /// first available and notify).
    pub fn ping_success(&mut self, models: Vec<OllamaModel>) -> Option<String> {
        self.models = models;
        self.selected_model_idx = 0;
        if self.models.is_empty() {
            self.health = HealthStatus::Healthy;
            self.phase = OllamaConfigPhase::NoModels;
            return None;
        }
        let mut removed: Option<String> = None;
        if !self.model_input.is_empty() {
            match self.models.iter().position(|m| m.name == self.model_input) {
                Some(idx) => self.selected_model_idx = idx,
                None => {
                    // The saved model disappeared — pick the first available
                    // so Enter always lands on a real tag, and report the
                    // swap so the caller can notify.
                    removed = Some(self.model_input.clone());
                    let first = self.models[0].name.clone();
                    self.model_input = first;
                }
            }
        }
        self.ensure_model_visible();
        self.health = HealthStatus::Healthy;
        self.phase = OllamaConfigPhase::SelectModel;
        removed
    }

    /// Handle a failed ping: show error.
    pub fn ping_failed(&mut self, error: String) {
        self.health = HealthStatus::Unhealthy;
        self.phase = OllamaConfigPhase::PingFailed(error);
    }

    /// Record a background health-check success without opening the model picker.
    pub fn health_check_succeeded(&mut self) {
        self.health = HealthStatus::Healthy;
    }

    /// Record a background health-check failure without changing the dialog phase.
    pub fn health_check_failed(&mut self) {
        self.health = HealthStatus::Unhealthy;
    }

    fn ensure_model_visible(&mut self) {
        if self.selected_model_idx < self.model_scroll_offset {
            self.model_scroll_offset = self.selected_model_idx;
        } else if self.selected_model_idx >= self.model_scroll_offset + MODEL_PICKER_VISIBLE_ROWS {
            self.model_scroll_offset = self
                .selected_model_idx
                .saturating_sub(MODEL_PICKER_VISIBLE_ROWS - 1);
        }
    }

    /// Navigate to the previous model in the list.
    pub fn move_model_up(&mut self) {
        if self.selected_model_idx > 0 {
            self.selected_model_idx -= 1;
            self.ensure_model_visible();
        }
    }

    /// Navigate to the next model in the list.
    pub fn move_model_down(&mut self) {
        if self.selected_model_idx + 1 < self.models.len() {
            self.selected_model_idx += 1;
            self.ensure_model_visible();
        }
    }

    /// Return the currently selected model, if any.
    pub fn selected_model(&self) -> Option<&OllamaModel> {
        self.models.get(self.selected_model_idx)
    }

    /// Consume the dialog and return `(host_url, model_name)`.
    pub fn take_values(&mut self) -> (String, String) {
        let host = self.host_url_input.trim().to_string();
        let model = self.model_input.clone();
        self.close();
        (host, model)
    }

    /// Go back from SelectModel or NoModels to Default view (without closing
    /// the dialog).
    pub fn back_to_default(&mut self) {
        self.phase = OllamaConfigPhase::Default;
    }

    /// Check if we're in a modal sub-state (editing, pinging, model picker).
    pub fn is_modal(&self) -> bool {
        matches!(
            self.phase,
            OllamaConfigPhase::EditField(_)
                | OllamaConfigPhase::Pinging
                | OllamaConfigPhase::PingFailed(_)
                | OllamaConfigPhase::NoModels
                | OllamaConfigPhase::SelectModel
        )
    }
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

pub fn render_ollama_config_dialog(
    frame: &mut Frame,
    state: &OllamaConfigDialogState,
    vim_enabled: bool,
    area: Rect,
) {
    if !state.visible {
        return;
    }

    match &state.phase {
        OllamaConfigPhase::Default => render_default_view(frame, state, vim_enabled, area),
        OllamaConfigPhase::EditField(field) => {
            render_edit_mode(frame, state, *field, vim_enabled, area)
        }
        OllamaConfigPhase::Pinging => render_pinging(frame, state, area),
        OllamaConfigPhase::PingFailed(err) => render_ping_failed(frame, state, err, area),
        OllamaConfigPhase::NoModels => render_no_models(frame, state, area),
        OllamaConfigPhase::SelectModel => render_model_picker(frame, state, area),
    }
}

fn render_default_view(
    frame: &mut Frame,
    state: &OllamaConfigDialogState,
    _vim_enabled: bool,
    area: Rect,
) {
    let pink = Color::Rgb(233, 30, 99);
    let dim = Color::Rgb(90, 90, 90);
    let muted = Color::Rgb(180, 180, 180);
    let dialog_bg = CLAWDE_PANEL_BG;

    render_dark_overlay(frame, area);

    let width = 62u16.min(area.width.saturating_sub(4));
    let height = 19u16;
    let dialog_area = centered_rect(width, height, area);
    state.last_rect.set(dialog_area);
    render_dialog_bg(frame, dialog_area);

    let inner = Rect {
        x: dialog_area.x + 1,
        y: dialog_area.y + 1,
        width: dialog_area.width.saturating_sub(2),
        height: dialog_area.height.saturating_sub(2),
    };

    // Health dot
    let (health_dot, health_color) = match state.health {
        HealthStatus::Healthy => ("●", Color::Rgb(76, 175, 80)), // green
        HealthStatus::Unhealthy => ("●", Color::Rgb(220, 50, 50)), // red
        HealthStatus::Untested => ("●", dim),
    };

    // Host display (truncate if too long)
    let host_display = if state.host_url_input.is_empty() {
        "(not configured)".to_string()
    } else {
        state.host_url_input.chars().take(35).collect::<String>()
    };

    let model_display = if state.model_input.is_empty() {
        "(not set)".to_string()
    } else {
        state.model_input.clone()
    };

    let is_host_selected = state.active_field == OllamaConfigField::Host;
    let is_model_selected = state.active_field == OllamaConfigField::Model;
    let is_mode_selected = state.active_field == OllamaConfigField::Mode;
    let is_options_selected = state.active_field == OllamaConfigField::Options;

    let selected_row_style = |selected: bool| {
        if selected {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        }
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            " Connect Ollama",
            Style::default().fg(pink).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "{:>width$}",
                "esc ",
                width = inner.width.saturating_sub(16) as usize
            ),
            Style::default().fg(dim),
        ),
    ]));
    lines.push(Line::from(""));

    // Host row with health dot
    let host_indicator = if is_host_selected { "▸" } else { " " };
    let host_style = selected_row_style(is_host_selected);
    lines.push(Line::from(vec![
        Span::styled(format!(" {} Host:  ", host_indicator), host_style),
        Span::styled(health_dot, Style::default().fg(health_color)),
        Span::styled(format!(" {}", host_display), host_style),
    ]));

    // Model row
    let model_indicator = if is_model_selected { "▸" } else { " " };
    let model_style = selected_row_style(is_model_selected);
    lines.push(Line::from(vec![
        Span::styled(format!(" {} Model: ", model_indicator), model_style),
        Span::styled(model_display, model_style),
    ]));

    // Mode row (spec §TUI layout: run mode with tool-access explanation)
    let mode_indicator = if is_mode_selected { "▸" } else { " " };
    let (mode_label, mode_detail) = if state.mode_isolated {
        ("Isolated", "— network tools blocked")
    } else {
        ("Online", "— network tools allowed")
    };
    lines.push(Line::from(vec![
        Span::styled(
            format!(" {} Mode:  ", mode_indicator),
            selected_row_style(is_mode_selected),
        ),
        Span::styled(mode_label.to_string(), selected_row_style(is_mode_selected)),
        Span::styled(mode_detail, Style::default().fg(dim)),
    ]));

    // Common options rows (spec §Common controls: expanded by default,
    // frequency-ordered). j/k over rows, ←/→ cycles the value.
    let options_indicator = if is_options_selected { "▸" } else { " " };
    for (row_idx, key) in OllamaConfigDialogState::OPTION_KEYS_ORDER
        .iter()
        .enumerate()
    {
        let label = state.option_label(key);
        let display = if label.is_empty() {
            "Ollama/model default".to_string()
        } else {
            label.to_string()
        };
        let focused = is_options_selected && row_idx == state.option_key_idx;
        let prefix = if focused { "›" } else { " " };
        let value_style = if focused {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else if label.is_empty() {
            Style::default().fg(dim)
        } else {
            Style::default().fg(muted)
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("   {} ", prefix),
                selected_row_style(is_options_selected),
            ),
            Span::styled(
                format!("{:<12}", format!("{}:", key)),
                selected_row_style(is_options_selected),
            ),
            Span::styled(display, value_style),
        ]));
    }
    let _ = options_indicator;

    lines.push(Line::from(""));

    // Effective-options preview (spec §Option defaults and UI priorities:
    // overrides vs remote defaults; the native transport applies every
    // request-shaping option).
    let preview = state.effective_preview_rows();
    if preview.is_empty() {
        lines.push(Line::from(Span::styled(
            "   All options at Ollama/model default",
            Style::default().fg(dim),
        )));
    } else {
        for (label, status) in preview.iter().take(2) {
            let color = if status == "applied" {
                Color::Rgb(76, 175, 80)
            } else {
                dim
            };
            lines.push(Line::from(vec![
                Span::styled("   ● ".to_string(), Style::default().fg(color)),
                Span::styled(label.clone(), Style::default().fg(muted)),
                Span::styled(format!(" ({status})"), Style::default().fg(dim)),
            ]));
        }
        if preview.len() > 2 {
            lines.push(Line::from(Span::styled(
                format!("   +{} more option(s)", preview.len() - 2),
                Style::default().fg(dim),
            )));
        }
    }

    lines.push(Line::from(""));
    // Loaded-models summary (spec §Model/server behavior: use /api/ps to
    // mark loaded models). Surfaced even in the fast-path view so the user
    // sees what the server is holding before connecting.
    let loaded_count = state.loaded_model_names.len();
    let loaded_line = if loaded_count == 0 {
        Line::from(Span::styled(
            "   No models loaded in VRAM",
            Style::default().fg(dim),
        ))
    } else {
        let preview = state
            .loaded_model_names
            .iter()
            .take(2)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let more = loaded_count.saturating_sub(2);
        let more_str = if more > 0 {
            format!(" +{} more", more)
        } else {
            String::new()
        };
        Line::from(vec![
            Span::styled("   ● ", Style::default().fg(Color::Rgb(76, 175, 80))),
            Span::styled(
                format!("{loaded_count} loaded in VRAM: {preview}{more_str}"),
                Style::default().fg(muted),
            ),
        ])
    };
    lines.push(loaded_line);
    let mut hint_spans = vec![
        Span::styled("enter", Style::default().fg(dim)),
        Span::styled(" connect  ", Style::default().fg(dim)),
        Span::styled("j/k", Style::default().fg(dim)),
        Span::styled(" navigate  ", Style::default().fg(dim)),
        Span::styled("e", Style::default().fg(dim)),
        Span::styled(" edit  ", Style::default().fg(dim)),
        Span::styled("t", Style::default().fg(dim)),
        Span::styled(" test  ", Style::default().fg(dim)),
        Span::styled("r", Style::default().fg(dim)),
        Span::styled(" refresh", Style::default().fg(dim)),
    ];
    if _vim_enabled {
        hint_spans.push(Span::styled("   -- NORMAL --", Style::default().fg(dim)));
    }
    lines.push(Line::from(hint_spans));

    let para = Paragraph::new(lines).bg(dialog_bg);
    frame.render_widget(para, inner);
}

fn render_edit_mode(
    frame: &mut Frame,
    state: &OllamaConfigDialogState,
    field: OllamaConfigField,
    _vim_enabled: bool,
    area: Rect,
) {
    let pink = Color::Rgb(233, 30, 99);
    let dim = Color::Rgb(90, 90, 90);
    let muted = Color::Rgb(180, 180, 180);
    let dialog_bg = CLAWDE_PANEL_BG;

    render_dark_overlay(frame, area);

    let width = 60u16.min(area.width.saturating_sub(4));
    let height = 13u16;
    let dialog_area = centered_rect(width, height, area);
    state.last_rect.set(dialog_area);
    render_dialog_bg(frame, dialog_area);

    let inner = Rect {
        x: dialog_area.x + 1,
        y: dialog_area.y + 1,
        width: dialog_area.width.saturating_sub(2),
        height: dialog_area.height.saturating_sub(2),
    };

    let host_style = if field == OllamaConfigField::Host {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    let model_style = if field == OllamaConfigField::Model {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };

    let url_text = if state.host_url_input.is_empty() {
        "http://your-ollama-server:11434".to_string()
    } else {
        state.host_url_input.clone()
    };
    let model_text = if state.model_input.is_empty() {
        "qwen2.5-coder:3b".to_string()
    } else {
        state.model_input.clone()
    };

    // Build text with cursor at the correct position
    let cursor_char = if _vim_enabled && state.vim_search.insert {
        '_'
    } else {
        '|'
    };

    let url_spans = if field == OllamaConfigField::Host {
        let before = &url_text[..state.cursor_pos.min(url_text.len())];
        let after = &url_text[state.cursor_pos.min(url_text.len())..];
        vec![
            Span::styled(format!(" {}", before), host_style),
            Span::styled(
                cursor_char.to_string(),
                Style::default().fg(pink).add_modifier(Modifier::BOLD),
            ),
            Span::styled(after.to_string(), host_style),
        ]
    } else {
        vec![Span::styled(format!(" {}", url_text), host_style)]
    };

    let model_spans = if field == OllamaConfigField::Model {
        let before = &model_text[..state.cursor_pos.min(model_text.len())];
        let after = &model_text[state.cursor_pos.min(model_text.len())..];
        vec![
            Span::styled(format!(" {}", before), model_style),
            Span::styled(
                cursor_char.to_string(),
                Style::default().fg(pink).add_modifier(Modifier::BOLD),
            ),
            Span::styled(after.to_string(), model_style),
        ]
    } else {
        vec![Span::styled(format!(" {}", model_text), model_style)]
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            " Connect Ollama",
            Style::default().fg(pink).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "{:>width$}",
                "esc ",
                width = inner.width.saturating_sub(16) as usize
            ),
            Style::default().fg(dim),
        ),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        " Host URL:",
        Style::default().fg(muted),
    )]));
    lines.push(Line::from(url_spans));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        " Model:",
        Style::default().fg(muted),
    )]));
    lines.push(Line::from(model_spans));
    lines.push(Line::from(""));
    let mut hint_spans = vec![
        Span::styled("tab", Style::default().fg(dim)),
        Span::styled(" switch field  ", Style::default().fg(dim)),
        Span::styled("enter", Style::default().fg(dim)),
        Span::styled(" confirm  ", Style::default().fg(dim)),
        Span::styled("ctrl-p", Style::default().fg(dim)),
        Span::styled(" ping", Style::default().fg(dim)),
    ];
    if _vim_enabled && state.vim_search.insert {
        hint_spans.push(Span::styled(
            "   -- INSERT --",
            Style::default().fg(dim).add_modifier(Modifier::BOLD),
        ));
    }
    lines.push(Line::from(hint_spans));

    let para = Paragraph::new(lines).bg(dialog_bg);
    frame.render_widget(para, inner);
}

fn render_pinging(frame: &mut Frame, state: &OllamaConfigDialogState, area: Rect) {
    let pink = Color::Rgb(233, 30, 99);
    let dim = Color::Rgb(90, 90, 90);
    let muted = Color::Rgb(180, 180, 180);
    let dialog_bg = CLAWDE_PANEL_BG;

    render_dark_overlay(frame, area);

    let width = 50u16.min(area.width.saturating_sub(4));
    let height = 7u16;
    let dialog_area = centered_rect(width, height, area);
    state.last_rect.set(dialog_area);
    render_dialog_bg(frame, dialog_area);

    let inner = Rect {
        x: dialog_area.x + 1,
        y: dialog_area.y + 1,
        width: dialog_area.width.saturating_sub(2),
        height: dialog_area.height.saturating_sub(2),
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            " Connect Ollama",
            Style::default().fg(pink).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "{:>width$}",
                "esc ",
                width = inner.width.saturating_sub(16) as usize
            ),
            Style::default().fg(dim),
        ),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        format!(" Pinging {}...", state.host_url_input),
        Style::default().fg(muted),
    )]));

    let para = Paragraph::new(lines).bg(dialog_bg);
    frame.render_widget(para, inner);
}

fn render_ping_failed(frame: &mut Frame, state: &OllamaConfigDialogState, error: &str, area: Rect) {
    let pink = Color::Rgb(233, 30, 99);
    let dim = Color::Rgb(90, 90, 90);
    let red = Color::Rgb(220, 50, 50);
    let muted = Color::Rgb(180, 180, 180);
    let dialog_bg = CLAWDE_PANEL_BG;

    render_dark_overlay(frame, area);

    let width = 60u16.min(area.width.saturating_sub(4));
    let height = 9u16;
    let dialog_area = centered_rect(width, height, area);
    state.last_rect.set(dialog_area);
    render_dialog_bg(frame, dialog_area);

    let inner = Rect {
        x: dialog_area.x + 1,
        y: dialog_area.y + 1,
        width: dialog_area.width.saturating_sub(2),
        height: dialog_area.height.saturating_sub(2),
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            " Connect Ollama",
            Style::default().fg(pink).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "{:>width$}",
                "esc ",
                width = inner.width.saturating_sub(16) as usize
            ),
            Style::default().fg(dim),
        ),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        " Connection failed:",
        Style::default().fg(red),
    )]));
    lines.push(Line::from(vec![Span::styled(
        format!(" {}", error),
        Style::default().fg(muted),
    )]));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("enter", Style::default().fg(dim)),
        Span::styled(" retry  ", Style::default().fg(dim)),
        Span::styled("esc", Style::default().fg(dim)),
        Span::styled(" back", Style::default().fg(dim)),
    ]));

    let para = Paragraph::new(lines).bg(dialog_bg);
    frame.render_widget(para, inner);
}

fn render_no_models(frame: &mut Frame, state: &OllamaConfigDialogState, area: Rect) {
    let pink = Color::Rgb(233, 30, 99);
    let dim = Color::Rgb(90, 90, 90);
    let muted = Color::Rgb(180, 180, 180);
    let dialog_bg = CLAWDE_PANEL_BG;

    render_dark_overlay(frame, area);

    let width = 62u16.min(area.width.saturating_sub(4));
    let height = 9u16;
    let dialog_area = centered_rect(width, height, area);
    state.last_rect.set(dialog_area);
    render_dialog_bg(frame, dialog_area);

    let inner = Rect {
        x: dialog_area.x + 1,
        y: dialog_area.y + 1,
        width: dialog_area.width.saturating_sub(2),
        height: dialog_area.height.saturating_sub(2),
    };

    let lines = vec![
        Line::from(vec![
            Span::styled(
                " Ollama Connected",
                Style::default().fg(pink).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "{:>width$}",
                    "esc ",
                    width = inner.width.saturating_sub(14) as usize
                ),
                Style::default().fg(dim),
            ),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled(
            " No models are installed on this server.",
            Style::default().fg(muted),
        )]),
        Line::from(vec![Span::styled(
            " Pull one with: ollama pull <model>",
            Style::default().fg(muted),
        )]),
        Line::from(""),
        Line::from(vec![
            Span::styled("enter", Style::default().fg(dim)),
            Span::styled(" retry  ", Style::default().fg(dim)),
            Span::styled("esc", Style::default().fg(dim)),
            Span::styled(" back", Style::default().fg(dim)),
        ]),
    ];

    frame.render_widget(Paragraph::new(lines).bg(dialog_bg), inner);
}

fn render_model_picker(frame: &mut Frame, state: &OllamaConfigDialogState, area: Rect) {
    let pink = Color::Rgb(233, 30, 99);
    let dim = Color::Rgb(90, 90, 90);
    let muted = Color::Rgb(180, 180, 180);
    let highlight_bg = Color::Rgb(233, 30, 99);
    let highlight_fg = Color::White;
    let dialog_bg = CLAWDE_PANEL_BG;

    render_dark_overlay(frame, area);

    let width = 65u16.min(area.width.saturating_sub(4));
    let model_rows = state.models.len().min(MODEL_PICKER_VISIBLE_ROWS) as u16;
    let height = (5 + model_rows + 2).max(9);
    let dialog_area = centered_rect(width, height, area);
    state.last_rect.set(dialog_area);
    render_dialog_bg(frame, dialog_area);

    let inner = Rect {
        x: dialog_area.x + 1,
        y: dialog_area.y + 1,
        width: dialog_area.width.saturating_sub(2),
        height: dialog_area.height.saturating_sub(2),
    };

    let host_display = state.host_url_input.chars().take(40).collect::<String>();

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            " Select Model",
            Style::default().fg(pink).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "{:>width$}",
                "esc ",
                width = inner.width.saturating_sub(14) as usize
            ),
            Style::default().fg(dim),
        ),
    ]));
    lines.push(Line::from(vec![Span::styled(
        format!(" Available models on {}", host_display),
        Style::default().fg(muted),
    )]));
    lines.push(Line::from(""));

    if state.models.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            " No models found. Pull a model on the server first.",
            Style::default().fg(muted),
        )]));
    } else {
        for (i, model) in state
            .models
            .iter()
            .enumerate()
            .skip(state.model_scroll_offset)
            .take(MODEL_PICKER_VISIBLE_ROWS)
        {
            let is_selected = i == state.selected_model_idx;
            let indicator = if is_selected { "▸" } else { " " };
            // Loaded-state marker (spec §TUI layout): "●" green when the
            // model is resident in VRAM, "○" dim when installed only.
            let (loaded_marker, marker_color) = if state.is_model_loaded(&model.name) {
                ("●", Color::Rgb(76, 175, 80))
            } else {
                ("○", dim)
            };

            let row_style = if is_selected {
                Style::default()
                    .bg(highlight_bg)
                    .fg(highlight_fg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            let size_str = model.size_display();
            let quant_str = &model.quantization;
            let params_str = &model.parameter_size;

            lines.push(Line::from(vec![
                Span::styled(format!(" {} ", indicator), row_style),
                Span::styled(
                    loaded_marker.to_string(),
                    if is_selected {
                        Style::default().bg(highlight_bg).fg(marker_color)
                    } else {
                        Style::default().fg(marker_color)
                    },
                ),
                Span::styled(format!("{:<30}", model.name), row_style),
                Span::styled(
                    format!("{:>6}  {:<8}  {:<6}", size_str, quant_str, params_str),
                    if is_selected {
                        Style::default().bg(highlight_bg).fg(highlight_fg)
                    } else {
                        Style::default().fg(muted)
                    },
                ),
            ]));
        }
    }

    lines.push(Line::from(""));
    let loaded_count = state
        .models
        .iter()
        .filter(|model| state.is_model_loaded(&model.name))
        .count();
    let mut hint_spans = vec![
        Span::styled("j/k", Style::default().fg(dim)),
        Span::styled(" select  ", Style::default().fg(dim)),
        Span::styled("enter", Style::default().fg(dim)),
        Span::styled(" confirm  ", Style::default().fg(dim)),
        Span::styled("r", Style::default().fg(dim)),
        Span::styled(" refresh  ", Style::default().fg(dim)),
        Span::styled("esc", Style::default().fg(dim)),
        Span::styled(" back", Style::default().fg(dim)),
    ];
    if loaded_count > 0 {
        hint_spans.push(Span::styled(
            format!("   ● loaded in VRAM ({loaded_count})"),
            Style::default().fg(Color::Rgb(76, 175, 80)),
        ));
    } else {
        hint_spans.push(Span::styled(
            "   ○ = installed only",
            Style::default().fg(dim),
        ));
    }
    lines.push(Line::from(hint_spans));

    let para = Paragraph::new(lines).bg(dialog_bg);
    frame.render_widget(para, inner);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_open_close() {
        let mut state = OllamaConfigDialogState::new();
        assert!(!state.visible);

        state.open(
            Some("http://gpu-host.example:11434".to_string()),
            Some("qwen2.5-coder:3b".to_string()),
        );
        assert!(state.visible);
        assert_eq!(state.host_url_input, "http://gpu-host.example:11434");
        assert_eq!(state.model_input, "qwen2.5-coder:3b");
        assert_eq!(state.phase, OllamaConfigPhase::Default);

        state.close();
        assert!(!state.visible);
        assert!(state.host_url_input.is_empty());
    }

    #[test]
    fn test_field_navigation() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);
        assert_eq!(state.active_field, OllamaConfigField::Host);

        state.move_next_field();
        assert_eq!(state.active_field, OllamaConfigField::Model);

        state.move_next_field();
        assert_eq!(state.active_field, OllamaConfigField::Mode);

        state.move_next_field();
        assert_eq!(state.active_field, OllamaConfigField::Options);

        // Wraps to Host after the last field.
        state.move_next_field();
        assert_eq!(state.active_field, OllamaConfigField::Host);

        state.move_prev_field();
        assert_eq!(state.active_field, OllamaConfigField::Options);

        state.move_prev_field();
        assert_eq!(state.active_field, OllamaConfigField::Mode);
    }

    #[test]
    fn test_edit_mode() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);
        assert_eq!(state.phase, OllamaConfigPhase::Default);

        state.start_edit();
        assert!(matches!(
            state.phase,
            OllamaConfigPhase::EditField(OllamaConfigField::Host)
        ));

        state.insert_char('h');
        state.insert_char('t');
        state.insert_char('t');
        state.insert_char('p');
        assert_eq!(state.host_url_input, "http");

        state.cancel_edit();
        assert_eq!(state.phase, OllamaConfigPhase::Default);
    }

    #[test]
    fn test_backspace() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);
        state.start_edit();

        state.insert_char('a');
        state.insert_char('b');
        state.insert_char('c');
        assert_eq!(state.host_url_input, "abc");

        state.backspace();
        assert_eq!(state.host_url_input, "ab");

        state.backspace();
        state.backspace();
        assert_eq!(state.host_url_input, "");

        // Backspace on empty should not panic
        state.backspace();
        assert_eq!(state.host_url_input, "");
    }

    #[test]
    fn test_can_connect() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);
        assert!(!state.can_connect());

        state.host_url_input = "http://gpu-host.example:11434".to_string();
        assert!(state.can_connect());
    }

    #[test]
    fn test_model_navigation() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);

        let models = vec![
            OllamaModel {
                name: "model-a".to_string(),
                size: 1_000_000_000,
                quantization: "Q4_K_M".to_string(),
                parameter_size: "7B".to_string(),
            },
            OllamaModel {
                name: "model-b".to_string(),
                size: 2_000_000_000,
                quantization: "Q4_0".to_string(),
                parameter_size: "13B".to_string(),
            },
            OllamaModel {
                name: "model-c".to_string(),
                size: 500_000_000,
                quantization: "Q8_0".to_string(),
                parameter_size: "3B".to_string(),
            },
        ];

        state.ping_success(models);
        assert_eq!(state.phase, OllamaConfigPhase::SelectModel);
        assert_eq!(state.selected_model_idx, 0);
        assert_eq!(state.selected_model().unwrap().name, "model-a");

        state.move_model_down();
        assert_eq!(state.selected_model_idx, 1);
        assert_eq!(state.selected_model().unwrap().name, "model-b");

        state.move_model_down();
        assert_eq!(state.selected_model_idx, 2);

        // Can't go past the end
        state.move_model_down();
        assert_eq!(state.selected_model_idx, 2);

        state.move_model_up();
        assert_eq!(state.selected_model_idx, 1);

        state.move_model_up();
        assert_eq!(state.selected_model_idx, 0);

        // Can't go below 0
        state.move_model_up();
        assert_eq!(state.selected_model_idx, 0);
    }

    #[test]
    fn test_model_navigation_scrolls_large_lists() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);
        let models = (0..15)
            .map(|index| OllamaModel {
                name: format!("model-{index}"),
                size: 1_000_000_000,
                quantization: "Q4_K_M".to_string(),
                parameter_size: "7B".to_string(),
            })
            .collect();

        state.ping_success(models);
        assert_eq!(state.model_scroll_offset, 0);
        for _ in 0..10 {
            state.move_model_down();
        }
        assert_eq!(state.selected_model_idx, 10);
        assert_eq!(state.model_scroll_offset, 1);
        state.move_model_down();
        assert_eq!(state.model_scroll_offset, 2);
        for _ in 0..11 {
            state.move_model_up();
        }
        assert_eq!(state.selected_model_idx, 0);
        assert_eq!(state.model_scroll_offset, 0);
    }

    #[test]
    fn test_background_health_check_does_not_change_phase() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);
        state.health_check_succeeded();
        assert_eq!(state.health, HealthStatus::Healthy);
        assert_eq!(state.phase, OllamaConfigPhase::Default);
        state.health_check_failed();
        assert_eq!(state.health, HealthStatus::Unhealthy);
        assert_eq!(state.phase, OllamaConfigPhase::Default);
    }

    #[test]
    fn test_take_values() {
        let mut state = OllamaConfigDialogState::new();
        state.open(
            Some("http://gpu-host.example:11434".to_string()),
            Some("qwen2.5-coder:3b".to_string()),
        );

        let (host, model) = state.take_values();
        assert_eq!(host, "http://gpu-host.example:11434");
        assert_eq!(model, "qwen2.5-coder:3b");
        assert!(!state.visible);
    }

    #[test]
    fn test_model_size_display() {
        let model = OllamaModel {
            name: "test".to_string(),
            size: 1_800_000_000,
            quantization: "Q4_K_M".to_string(),
            parameter_size: "3B".to_string(),
        };
        assert_eq!(model.size_display(), "1.7GB");

        let small = OllamaModel {
            name: "test".to_string(),
            size: 500_000_000,
            quantization: "Q4_0".to_string(),
            parameter_size: "1B".to_string(),
        };
        assert_eq!(small.size_display(), "477MB");
    }

    #[test]
    fn test_empty_model_list_is_actionable() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);
        state.ping_success(vec![]);
        assert_eq!(state.health, HealthStatus::Healthy);
        assert_eq!(state.phase, OllamaConfigPhase::NoModels);
        assert!(state.selected_model().is_none());
    }

    #[test]
    fn test_health_status() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);
        assert_eq!(state.health, HealthStatus::Untested);

        state.ping_success(vec![]);
        assert_eq!(state.health, HealthStatus::Healthy);

        state.ping_failed("error".to_string());
        assert_eq!(state.health, HealthStatus::Unhealthy);
    }

    #[test]
    fn test_is_modal() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);
        assert!(!state.is_modal());

        state.start_edit();
        assert!(state.is_modal());

        state.cancel_edit();
        assert!(!state.is_modal());

        state.start_ping();
        assert!(state.is_modal());
    }

    #[test]
    fn test_validate_host_url() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);

        // Empty URL
        assert!(state.validate_host_url().is_err());

        // Valid URL
        state.host_url_input = "http://gpu-host.example:11434".to_string();
        assert!(state.validate_host_url().is_ok());

        // URL without scheme
        state.host_url_input = "gpu-host.example:11434".to_string();
        assert!(state.validate_host_url().is_err());

        // URL with /v1 suffix (should be normalized)
        state.host_url_input = "http://gpu-host.example:11434/v1".to_string();
        let result = state.validate_host_url();
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "http://gpu-host.example:11434");
    }

    #[test]
    fn test_validate_host_url_rejects_local_ollama() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);

        for host in [
            "http://localhost:11434",
            "http://127.0.0.1:11434/v1",
            "http://[::1]:11434",
        ] {
            state.host_url_input = host.to_string();
            assert!(
                state.validate_host_url().is_err(),
                "accepted local host {host}"
            );
        }
    }

    #[test]
    fn test_validate_model_name() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);

        // Empty model
        assert!(state.validate_model_name().is_err());

        // Valid model
        state.model_input = "qwen2.5-coder:3b".to_string();
        assert!(state.validate_model_name().is_ok());

        // Model with spaces
        state.model_input = "qwen 2.5".to_string();
        assert!(state.validate_model_name().is_err());
    }

    #[test]
    fn test_health_resets_on_host_edit() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);

        // Set health to healthy
        state.health = HealthStatus::Healthy;
        assert_eq!(state.health, HealthStatus::Healthy);

        // Enter edit mode and modify host
        state.phase = OllamaConfigPhase::EditField(OllamaConfigField::Host);
        state.insert_char('x');
        assert_eq!(state.health, HealthStatus::Untested);
    }

    #[test]
    fn test_loaded_model_markers() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);
        state.set_loaded_model_names(vec!["qwen2.5-coder:7b".to_string()]);

        let models = vec![
            OllamaModel {
                name: "qwen2.5-coder:7b".to_string(),
                size: 4_700_000_000,
                quantization: "Q4_K_M".to_string(),
                parameter_size: "7B".to_string(),
            },
            OllamaModel {
                name: "llama3:8b".to_string(),
                size: 4_000_000_000,
                quantization: "Q4_0".to_string(),
                parameter_size: "8B".to_string(),
            },
        ];
        state.ping_success(models);
        assert!(state.is_model_loaded("qwen2.5-coder:7b"));
        // Bare tag and `:latest` are the same model to Ollama...
        state.set_loaded_model_names(vec!["llama3:latest".to_string()]);
        assert!(state.is_model_loaded("llama3"));
        assert!(state.is_model_loaded("llama3:latest"));
        // ...but a versioned tag is distinct from any other tag.
        assert!(!state.is_model_loaded("llama3:8b"));
        assert!(!state.is_model_loaded("qwen2.5-coder:7b"));

        // The snapshot survives a refresh cycle (models replaced, loaded
        // names kept) — this is why it lives outside `models`.
        state.set_loaded_model_names(vec![]);
        assert!(!state.is_model_loaded("qwen2.5-coder:7b"));
    }

    #[test]
    fn test_option_value_cycling_wraps_through_unset() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);
        state.active_field = OllamaConfigField::Options;

        // Focus the first option row (num_ctx) and cycle forward from unset:
        // unset -> first preset.
        state.cycle_option_value(1);
        assert_eq!(state.num_ctx_label, "2K");
        // Cycle backward twice: 2K -> unset -> last preset (128K).
        state.cycle_option_value(-1);
        assert_eq!(state.num_ctx_label, "");
        state.cycle_option_value(-1);
        assert_eq!(state.num_ctx_label, "128K");

        // Move the sub-cursor to keep_alive (index 2) and cycle.
        state.move_option_key(1); // num_predict
        state.move_option_key(1); // keep_alive
        state.cycle_option_value(1);
        assert_eq!(state.keep_alive_label, "unload after request");
        state.cycle_option_value(-1);
        assert_eq!(state.keep_alive_label, "");
    }

    #[test]
    fn test_ping_success_reports_removed_model() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);
        state.model_input = "deleted-model:7b".to_string();

        let models = vec![OllamaModel {
            name: "qwen2.5-coder:7b".to_string(),
            size: 4_700_000_000,
            quantization: "Q4_K_M".to_string(),
            parameter_size: "7B".to_string(),
        }];
        let removed = state.ping_success(models);
        // Spec §Model/server behavior: the disappeared model is reported and
        // the first available tag takes its place so Enter lands on a real
        // model.
        assert_eq!(removed.as_deref(), Some("deleted-model:7b"));
        assert_eq!(state.model_input, "qwen2.5-coder:7b");
        assert_eq!(state.selected_model_idx, 0);

        // An existing model is kept and nothing is reported.
        state.model_input = "qwen2.5-coder:7b".to_string();
        let models = vec![OllamaModel {
            name: "qwen2.5-coder:7b".to_string(),
            size: 4_700_000_000,
            quantization: "Q4_K_M".to_string(),
            parameter_size: "7B".to_string(),
        }];
        let removed = state.ping_success(models);
        assert!(removed.is_none());
    }

    #[test]
    fn test_effective_preview_rows() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);
        // All unset -> empty preview.
        assert!(state.effective_preview_rows().is_empty());
        state.num_ctx_label = "16K".to_string();
        state.temperature_label = "0.2 (precise)".to_string();
        let rows = state.effective_preview_rows();
        assert_eq!(rows.len(), 2);
        assert!(rows
            .iter()
            .any(|(l, s)| l.contains("num_ctx") && s.contains("applied")));
        assert!(rows
            .iter()
            .any(|(l, s)| l.contains("temperature") && s.contains("applied")));
    }

    #[test]
    fn test_set_mode_and_options_seeds_from_raw_values() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);
        let options = serde_json::json!({
            "num_ctx": 16_384u64,
            "temperature": 0.2,
            "keep_alive": 600i64,
        });
        state.set_mode_and_options(true, options.as_object().unwrap());
        assert!(state.mode_isolated);
        assert_eq!(state.num_ctx_label, "16K");
        assert_eq!(state.temperature_label, "0.2 (precise)");
        assert_eq!(state.keep_alive_label, "10 min");
        assert_eq!(state.num_predict_label, "");
    }

    #[test]
    fn test_back_to_default() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);

        // An empty successful response is an explicit no-models state.
        state.ping_success(vec![]);
        assert_eq!(state.phase, OllamaConfigPhase::NoModels);

        // Go back to default
        state.back_to_default();
        assert_eq!(state.phase, OllamaConfigPhase::Default);
    }

    #[test]
    fn test_cursor_movement() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);
        state.host_url_input = "http://gpu-host.example:11434".to_string();
        state.start_edit();

        // Cursor starts at end (29 chars: http://gpu-host.example:11434)
        assert_eq!(state.cursor_pos, 29);

        // Move left
        state.move_cursor_left();
        assert_eq!(state.cursor_pos, 28);

        // Move left again
        state.move_cursor_left();
        assert_eq!(state.cursor_pos, 27);

        // Move right
        state.move_cursor_right();
        assert_eq!(state.cursor_pos, 28);

        // Move right to end
        state.move_cursor_right();
        assert_eq!(state.cursor_pos, 29);

        // Can't move past end
        state.move_cursor_right();
        assert_eq!(state.cursor_pos, 29);

        // Move to beginning
        state.cursor_pos = 0;
        state.move_cursor_left();
        assert_eq!(state.cursor_pos, 0);
    }

    #[test]
    fn test_insert_at_cursor() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);
        state.host_url_input = "http://example.com".to_string();
        state.start_edit();

        // Move to position 4 (after "http")
        state.cursor_pos = 4;
        state.insert_char('s');
        assert_eq!(state.host_url_input, "https://example.com");
        assert_eq!(state.cursor_pos, 5);
    }

    #[test]
    fn test_backspace_at_cursor() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);
        state.host_url_input = "http://example.com".to_string();
        state.start_edit();

        // Move to position 5 (after "http:")
        state.cursor_pos = 5;
        state.backspace();
        assert_eq!(state.host_url_input, "http//example.com");
        assert_eq!(state.cursor_pos, 4);
    }
}
