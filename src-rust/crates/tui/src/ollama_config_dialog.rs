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
    /// Cursor position within the active field (byte index).
    pub cursor_pos: usize,
    pub active_field: OllamaConfigField,
    pub phase: OllamaConfigPhase,
    pub models: Vec<OllamaModel>,
    pub selected_model_idx: usize,
    pub model_scroll_offset: usize,
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
            cursor_pos: 0,
            active_field: OllamaConfigField::Host,
            phase: OllamaConfigPhase::Default,
            models: Vec::new(),
            selected_model_idx: 0,
            model_scroll_offset: 0,
            health: HealthStatus::Untested,
            vim_search: VimSearch::new(),
        }
    }

    /// Open the dialog with optional current values.
    pub fn open(&mut self, current_url: Option<String>, current_model: Option<String>) {
        self.visible = true;
        self.host_url_input = current_url.unwrap_or_default();
        self.model_input = current_model.unwrap_or_default();
        self.cursor_pos = 0;
        self.active_field = OllamaConfigField::Host;
        self.phase = OllamaConfigPhase::Default;
        self.models.clear();
        self.selected_model_idx = 0;
        self.model_scroll_offset = 0;
        self.health = HealthStatus::Untested;
        self.vim_search.reset();
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
        self.phase = OllamaConfigPhase::EditField(self.active_field);
        // Set cursor to end of current text
        self.cursor_pos = match self.active_field {
            OllamaConfigField::Host => self.host_url_input.len(),
            OllamaConfigField::Model => self.model_input.len(),
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
            OllamaConfigField::Model => OllamaConfigField::Host,
        };
    }

    /// Navigate to the previous field (k or Up).
    pub fn move_prev_field(&mut self) {
        self.active_field = match self.active_field {
            OllamaConfigField::Host => OllamaConfigField::Model,
            OllamaConfigField::Model => OllamaConfigField::Host,
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
        let normalized = clawde_core::config::normalize_ollama_host(url).ok_or_else(|| {
            if !url.starts_with("http://") && !url.starts_with("https://") {
                "URL must start with http:// or https://".to_string()
            } else {
                "Invalid host URL".to_string()
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
    pub fn ping_success(&mut self, models: Vec<OllamaModel>) {
        self.models = models;
        self.selected_model_idx = 0;
        if self.models.is_empty() {
            self.health = HealthStatus::Healthy;
            self.phase = OllamaConfigPhase::NoModels;
            return;
        }
        // Pre-select the current model if it's in the list
        if !self.model_input.is_empty() {
            if let Some(idx) = self.models.iter().position(|m| m.name == self.model_input) {
                self.selected_model_idx = idx;
            }
        }
        self.ensure_model_visible();
        self.health = HealthStatus::Healthy;
        self.phase = OllamaConfigPhase::SelectModel;
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
    let dialog_bg = CLAWDE_PANEL_BG;

    render_dark_overlay(frame, area);

    let width = 56u16.min(area.width.saturating_sub(4));
    let height = 11u16;
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
    let host_style = if is_host_selected {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    lines.push(Line::from(vec![
        Span::styled(format!(" {} Host:  ", host_indicator), host_style),
        Span::styled(health_dot, Style::default().fg(health_color)),
        Span::styled(format!(" {}", host_display), host_style),
    ]));

    // Model row
    let model_indicator = if is_model_selected { "▸" } else { " " };
    let model_style = if is_model_selected {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    lines.push(Line::from(vec![
        Span::styled(format!(" {} Model: ", model_indicator), model_style),
        Span::styled(model_display, model_style),
    ]));

    lines.push(Line::from(""));
    let mut hint_spans = vec![
        Span::styled("enter", Style::default().fg(dim)),
        Span::styled(" connect  ", Style::default().fg(dim)),
        Span::styled("j/k", Style::default().fg(dim)),
        Span::styled(" navigate  ", Style::default().fg(dim)),
        Span::styled("e", Style::default().fg(dim)),
        Span::styled(" edit", Style::default().fg(dim)),
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
    lines.push(Line::from(vec![
        Span::styled("j/k", Style::default().fg(dim)),
        Span::styled(" select  ", Style::default().fg(dim)),
        Span::styled("enter", Style::default().fg(dim)),
        Span::styled(" confirm  ", Style::default().fg(dim)),
        Span::styled("esc", Style::default().fg(dim)),
        Span::styled(" back", Style::default().fg(dim)),
    ]));

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
        assert_eq!(state.active_field, OllamaConfigField::Host);

        state.move_prev_field();
        assert_eq!(state.active_field, OllamaConfigField::Model);
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
