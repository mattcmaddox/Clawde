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

use crate::overlays::{
    centered_rect, render_dark_overlay, render_dialog_bg, CLAWDE_ACCENT, CLAWDE_PANEL_BG,
};
use crate::vim_search::VimSearch;
use std::cell::Cell;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Which field is selected for editing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OllamaConfigField {
    Host,
    /// LAN discovery state + picker entry: the in-menu surface for found
    /// servers. Enter scans when nothing is cached, otherwise opens the
    /// SelectHost picker — same convention as the Model row.
    Servers,
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
    /// LAN scan found servers, showing host picker.
    SelectHost,
}

/// A LAN-discovered Ollama server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredHost {
    pub host_url: String,
    pub latency_ms: u128,
    pub model_count: usize,
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

/// Human-readable VRAM size for the model picker (e.g. "18.2GiB"). Uses GiB
/// to match the footer's VRAM-in-use badge; drops to MiB below 1 GiB.
pub fn format_vram_display(bytes: u64) -> String {
    let gib = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    if gib >= 1.0 {
        format!("{gib:.1}GiB")
    } else {
        let mib = bytes as f64 / (1024.0 * 1024.0);
        format!("{mib:.0}MiB")
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
    /// LAN-discovered candidate servers (from the auto/manual scan). Shown
    /// in the SelectHost picker; the configured host is never silently
    /// replaced.
    pub discovered_hosts: Vec<DiscoveredHost>,
    pub selected_host_idx: usize,
    pub host_scroll_offset: usize,
    /// Whether a LAN scan is in flight (drives the Servers row to
    /// "scanning…"). Mirrored from the App-side pending flag.
    pub discovery_scanning: bool,
    /// Whether at least one scan has completed this dialog session — the
    /// Servers row distinguishes "not scanned yet" from "scanned, none
    /// found".
    pub discovery_checked: bool,
    /// Set once the host picker auto-opened for the current host-less
    /// dialog session, so the 60s background rescan cannot yank the user
    /// back into the picker after they left it.
    pub auto_host_prompted: bool,
    /// Models currently loaded in the server's VRAM (from the periodic
    /// `/api/ps` poll), including the per-model `size_vram` Ollama reports.
    /// Drives the loaded markers and the VRAM column in the model picker;
    /// kept outside `models` so it survives refreshes.
    pub loaded_models: Vec<clawde_core::OllamaLoadedModel>,
    /// What the server reported about itself during the last ping: version
    /// plus the effective request parameters for the selected model
    /// (modelfile parameters via `/api/show`). `None` until a ping
    /// succeeds — the screen then shows a server-reported block below the
    /// options rows.
    pub server_info: Option<clawde_query::OllamaServerInfo>,
    /// Footer VRAM state: the sample behind the footer pill plus the probe
    /// outcome, so this screen can report *why* a number is present, absent,
    /// or `--`. Without it a typo'd `vram_probe_cmd` looks exactly like no
    /// command at all. Kept in sync from the poll and seeded on open.
    pub vram: clawde_core::config::OllamaVramStatus,
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
            discovered_hosts: Vec::new(),
            selected_host_idx: 0,
            host_scroll_offset: 0,
            discovery_scanning: false,
            discovery_checked: false,
            auto_host_prompted: false,
            loaded_models: Vec::new(),
            server_info: None,
            vram: clawde_core::config::OllamaVramStatus::default(),
            health: HealthStatus::Untested,
            vim_search: VimSearch::new(),
        }
    }

    /// Replace the loaded-in-VRAM snapshot (exact models from `/api/ps`,
    /// including per-model VRAM sizes). Called on screen open and when a
    /// poll updates.
    pub fn set_loaded_models(&mut self, models: Vec<clawde_core::OllamaLoadedModel>) {
        self.loaded_models = models;
    }

    /// Replace the server-reported info snapshot (version + effective
    /// request parameters). Called when a ping result passes the App-side
    /// staleness guards, and by the continuous poll while the screen is
    /// open.
    pub fn set_server_info(&mut self, info: clawde_query::OllamaServerInfo) {
        self.server_info = Some(info);
    }

    /// Drop the server-reported snapshot — used when a continuous-poll
    /// probe fails so a dead server does not keep displaying stale
    /// parameters.
    pub fn clear_server_info(&mut self) {
        self.server_info = None;
    }

    /// Replace the VRAM/probe snapshot. Called on screen open and whenever the
    /// footer poll assembles a new status.
    pub fn set_vram_status(&mut self, vram: clawde_core::config::OllamaVramStatus) {
        self.vram = vram;
    }

    /// One line explaining the footer's VRAM pill: where the number came from
    /// and — when a probe is configured — whether it answered.
    ///
    /// Always rendered, including with nothing configured, because this is the
    /// only surface that names the `vram_probe_cmd` / `vram_total_mb` knobs.
    pub fn vram_probe_line(&self) -> Line<'static> {
        use clawde_core::config::{OllamaVramProbe, OllamaVramSample};

        let dim = Style::default().fg(Color::Rgb(90, 90, 90));
        let muted = Style::default().fg(Color::Rgb(180, 180, 180));
        let warn = Style::default().fg(Color::Rgb(220, 50, 50));

        // "6.3GiB of 8.0GiB", or "-- of 8.0GiB" when the host never answered.
        let reading = |sample: &OllamaVramSample| match sample.used_bytes {
            Some(used) => format!(
                "{} of {}",
                format_vram_display(used),
                format_vram_display(sample.total_bytes)
            ),
            None => format!("-- of {}", format_vram_display(sample.total_bytes)),
        };

        let (text, style) = match (&self.vram.probe, self.vram.sample.as_ref()) {
            (
                OllamaVramProbe::Reported {
                    used_bytes,
                    total_bytes,
                },
                _,
            ) => (
                format!(
                    "VRAM probe: {} of {} (whole-GPU)",
                    format_vram_display(*used_bytes),
                    format_vram_display(*total_bytes)
                ),
                muted,
            ),
            (OllamaVramProbe::Failed, Some(sample)) => (
                format!("VRAM probe failed; Ollama-reported {}", reading(sample)),
                warn,
            ),
            (OllamaVramProbe::Failed, None) => (
                "VRAM probe failed; set vram_total_mb for a capacity".to_string(),
                warn,
            ),
            (OllamaVramProbe::NotConfigured, Some(sample)) => (
                format!("VRAM {} (declared capacity)", reading(sample)),
                muted,
            ),
            (OllamaVramProbe::NotConfigured, None) => (
                "VRAM not measured: set vram_probe_cmd or vram_total_mb".to_string(),
                dim,
            ),
        };
        Line::from(vec![Span::styled("   · ", dim), Span::styled(text, style)])
    }

    /// The parameters the server reported for the selected model, as
    /// `(name, value)` display rows. Used by the config screen to show what
    /// Ollama will actually apply (modelfile values) next to what the user
    /// configured (request overrides).
    pub fn server_param_rows(&self) -> Vec<(String, String)> {
        let Some(info) = self.server_info.as_ref() else {
            return Vec::new();
        };
        let mut rows: Vec<(String, String)> = info
            .params
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        // Surface the model's context window even when num_ctx is not in
        // the modelfile: `model_info.<family>.context_length` is the
        // architecture limit the request-level num_ctx clamps into.
        if let Some(ctx) = info.context_length {
            if !rows.iter().any(|(name, _)| name == "num_ctx") {
                rows.push(("context_length".to_string(), ctx.to_string()));
            }
        }
        rows
    }

    /// Seed the mode + option rows from persisted settings. Called on open;
    /// uses the centralized preset tables so the screen can never drift from
    /// the request pipeline.
    ///
    /// Values are canonicalized first (numeric strings over strings,
    /// keep-alive duration strings like "5m" resolved to seconds), so a
    /// hand-edited settings value shows its real value instead of rendering
    /// as unset — which would delete it on the next save.
    pub fn set_mode_and_options(
        &mut self,
        mode_isolated: bool,
        options: &serde_json::Map<String, serde_json::Value>,
    ) {
        use clawde_api::providers::ollama_options as oo;
        self.mode_isolated = mode_isolated;
        let seed = |key: &str| -> String {
            options
                .get(key)
                .and_then(|v| oo::normalize_common_option(key, v))
                .map(|v| oo::common_option_label(key, &v))
                .unwrap_or_default()
        };
        self.num_ctx_label = seed("num_ctx");
        self.num_predict_label = seed("num_predict");
        self.keep_alive_label = seed("keep_alive");
        self.temperature_label = seed("temperature");
        self.top_p_label = seed("top_p");
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
        // "" (unset) first, then presets, wrapping both directions. A custom
        // value (raw set, label not a preset) occupies its own slot right
        // after unset so cycling away is explicit: forward lands on the first
        // preset, backward on unset — it never silently vanishes by wrapping.
        let current = self.option_label(key).to_string();
        let is_preset = presets.contains(&current);
        let mut all = vec![String::new()];
        if !current.is_empty() && !is_preset {
            all.push(current.clone());
        }
        all.extend(presets);
        let idx = all.iter().position(|l| l == &current).unwrap_or(0);
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

    /// The canonical raw option map for the five common rows: the display
    /// labels parsed back through the centralized parsers. Empty string
    /// labels mean unset and are omitted, matching the omit-unless-set
    /// persistence rule. This is the single conversion point the preview and
    /// the settings writer both consume, so they can never disagree.
    pub fn common_options_map(&self) -> serde_json::Map<String, serde_json::Value> {
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
        raw
    }

    /// The effective-options preview rows (label, applied status) from
    /// the centralized helper — spec §Option defaults and UI priorities.
    pub fn effective_preview_rows(&self) -> Vec<(String, String)> {
        use clawde_api::providers::ollama_options as oo;
        oo::effective_preview(&self.common_options_map())
    }

    /// The loaded entry matching `name`. Ollama treats a bare tag as
    /// `:latest`, so `foo` and `foo:latest` are the same model; any other
    /// explicit tag (`foo:7b`) is distinct.
    fn loaded_entry(&self, name: &str) -> Option<&clawde_core::OllamaLoadedModel> {
        let canonical = |tag: &str| {
            tag.strip_suffix(":latest")
                .map(|bare| bare.to_string())
                .unwrap_or_else(|| tag.to_string())
        };
        let wanted = canonical(name);
        self.loaded_models
            .iter()
            .find(|loaded| canonical(&loaded.name) == wanted)
    }

    /// Whether an exact model tag is currently loaded in VRAM.
    pub fn is_model_loaded(&self, name: &str) -> bool {
        self.loaded_entry(name).is_some()
    }

    /// Bytes of VRAM Ollama reports as resident for a loaded model. `None`
    /// when the model is not loaded, or when the server reported no size
    /// (older Ollama) — the picker row then shows the marker alone rather
    /// than a bogus zero.
    pub fn loaded_vram(&self, name: &str) -> Option<u64> {
        self.loaded_entry(name).and_then(|loaded| loaded.size_vram)
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
        self.discovered_hosts.clear();
        self.selected_host_idx = 0;
        self.host_scroll_offset = 0;
        self.discovery_scanning = false;
        self.discovery_checked = false;
        self.auto_host_prompted = false;
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
        self.discovered_hosts.clear();
        self.selected_host_idx = 0;
        self.host_scroll_offset = 0;
        self.discovery_scanning = false;
        self.discovery_checked = false;
        self.auto_host_prompted = false;
        self.health = HealthStatus::Untested;
        self.vim_search.reset();
    }

    /// Enter edit mode for the active field.
    pub fn start_edit(&mut self) {
        // Only free-text fields get a cursor; Mode/Options/Servers are
        // value rows.
        match self.active_field {
            OllamaConfigField::Host | OllamaConfigField::Model => {}
            OllamaConfigField::Mode | OllamaConfigField::Options | OllamaConfigField::Servers => {
                return
            }
        }
        self.phase = OllamaConfigPhase::EditField(self.active_field);
        // Set cursor to end of current text
        self.cursor_pos = match self.active_field {
            OllamaConfigField::Host => self.host_url_input.len(),
            OllamaConfigField::Model => self.model_input.len(),
            OllamaConfigField::Mode | OllamaConfigField::Options | OllamaConfigField::Servers => 0,
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
            OllamaConfigField::Host => OllamaConfigField::Servers,
            OllamaConfigField::Servers => OllamaConfigField::Model,
            OllamaConfigField::Model => OllamaConfigField::Mode,
            OllamaConfigField::Mode => OllamaConfigField::Options,
            OllamaConfigField::Options => OllamaConfigField::Host,
        };
    }

    /// Navigate to the previous field (k or Up).
    pub fn move_prev_field(&mut self) {
        self.active_field = match self.active_field {
            OllamaConfigField::Host => OllamaConfigField::Options,
            OllamaConfigField::Servers => OllamaConfigField::Host,
            OllamaConfigField::Model => OllamaConfigField::Servers,
            OllamaConfigField::Mode => OllamaConfigField::Model,
            OllamaConfigField::Options => OllamaConfigField::Mode,
        };
    }

    /// Value text for the Servers row, reflecting the current discovery
    /// state inside the menu.
    pub fn servers_row_value(&self) -> String {
        if self.discovery_scanning {
            return "scanning the LAN…".to_string();
        }
        if let Some(best) = self.discovered_hosts.first() {
            if self.discovered_hosts.len() == 1 {
                if best.model_count > 0 {
                    return format!(
                        "{} — {} model(s), {}ms",
                        best.host_url, best.model_count, best.latency_ms
                    );
                }
                return format!("{} — {}ms", best.host_url, best.latency_ms);
            }
            return format!(
                "{} servers found — enter to pick",
                self.discovered_hosts.len()
            );
        }
        if self.discovery_checked {
            "no Ollama servers answered on the LAN".to_string()
        } else {
            "not scanned — enter scans the LAN".to_string()
        }
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
                // Mode/Options/Servers rows have no text cursor.
                OllamaConfigField::Mode
                | OllamaConfigField::Options
                | OllamaConfigField::Servers => {}
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
                // Mode/Options/Servers rows have no text cursor.
                OllamaConfigField::Mode
                | OllamaConfigField::Options
                | OllamaConfigField::Servers => {}
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

    /// Refresh the installed-model list from the background poll (delivered
    /// every poll cycle while a host is configured) without yanking the
    /// phase or losing the selection: the selected model is preserved by
    /// name (falling back to the first row when it vanished), and a server
    /// that earlier reported no models but now has some opens the picker.
    pub fn update_models_auto(&mut self, models: Vec<OllamaModel>) {
        self.health = HealthStatus::Healthy;
        if models.is_empty() {
            // An authoritative empty list means every model was removed —
            // the picker has nothing left to show.
            if self.phase == OllamaConfigPhase::SelectModel {
                self.models.clear();
                self.phase = OllamaConfigPhase::NoModels;
            }
            return;
        }
        let keep = self.selected_model().map(|m| m.name.clone());
        self.models = models;
        self.selected_model_idx = keep
            .and_then(|name| self.models.iter().position(|m| m.name == name))
            .unwrap_or(0);
        self.ensure_model_visible();
        if self.phase == OllamaConfigPhase::NoModels {
            self.phase = OllamaConfigPhase::SelectModel;
        }
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

    /// Replace the LAN-discovered host list (from the auto/manual scan).
    /// Keeps the current selection when the host survives a refresh.
    pub fn set_discovered_hosts(&mut self, hosts: Vec<DiscoveredHost>) {
        let keep = self.selected_host().map(|h| h.host_url.clone());
        self.discovered_hosts = hosts;
        self.selected_host_idx = keep
            .and_then(|url| self.discovered_hosts.iter().position(|h| h.host_url == url))
            .unwrap_or(0);
        self.ensure_host_visible();
    }

    fn ensure_host_visible(&mut self) {
        if self.selected_host_idx < self.host_scroll_offset {
            self.host_scroll_offset = self.selected_host_idx;
        } else if self.selected_host_idx >= self.host_scroll_offset + MODEL_PICKER_VISIBLE_ROWS {
            self.host_scroll_offset = self
                .selected_host_idx
                .saturating_sub(MODEL_PICKER_VISIBLE_ROWS - 1);
        }
    }

    /// Navigate to the previous discovered host.
    pub fn move_host_up(&mut self) {
        if self.selected_host_idx > 0 {
            self.selected_host_idx -= 1;
            self.ensure_host_visible();
        }
    }

    /// Navigate to the next discovered host.
    pub fn move_host_down(&mut self) {
        if self.selected_host_idx + 1 < self.discovered_hosts.len() {
            self.selected_host_idx += 1;
            self.ensure_host_visible();
        }
    }

    /// Return the currently selected discovered host, if any.
    pub fn selected_host(&self) -> Option<&DiscoveredHost> {
        self.discovered_hosts.get(self.selected_host_idx)
    }

    /// Whether the SelectHost picker has anything to show.
    pub fn has_discovered_hosts(&self) -> bool {
        !self.discovered_hosts.is_empty()
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

    /// Check if we're in a modal sub-state (editing, pinging, pickers).
    pub fn is_modal(&self) -> bool {
        matches!(
            self.phase,
            OllamaConfigPhase::EditField(_)
                | OllamaConfigPhase::Pinging
                | OllamaConfigPhase::PingFailed(_)
                | OllamaConfigPhase::NoModels
                | OllamaConfigPhase::SelectModel
                | OllamaConfigPhase::SelectHost
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
        OllamaConfigPhase::SelectHost => render_host_picker(frame, state, area),
    }
}

fn render_default_view(
    frame: &mut Frame,
    state: &OllamaConfigDialogState,
    _vim_enabled: bool,
    area: Rect,
) {
    let pink = CLAWDE_ACCENT;
    let dim = Color::Rgb(90, 90, 90);
    let muted = Color::Rgb(180, 180, 180);
    let dialog_bg = CLAWDE_PANEL_BG;

    render_dark_overlay(frame, area);

    let width = 62u16.min(area.width.saturating_sub(4));
    // Room for the Servers row, the server-reported block (version + up to 3
    // params), and the VRAM-probe line.
    let height = 27u16;
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
    let is_servers_selected = state.active_field == OllamaConfigField::Servers;
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

    // Servers row: the in-menu surface for LAN discovery. Enter opens the
    // host picker (or starts a scan); the value text reflects the current
    // discovery state so found servers are announced inside the menu, not
    // through an out-of-menu status toast.
    let servers_indicator = if is_servers_selected { "▸" } else { " " };
    let servers_style = selected_row_style(is_servers_selected);
    let servers_value = state.servers_row_value();
    let servers_value = servers_value.chars().take(34).collect::<String>();
    lines.push(Line::from(vec![
        Span::styled(format!(" {} Servers:", servers_indicator), servers_style),
        Span::styled(
            if servers_value.is_empty() {
                "-".to_string()
            } else {
                format!(" {servers_value}")
            },
            if is_servers_selected {
                servers_style
            } else if state.discovered_hosts.is_empty() && !state.discovery_scanning {
                Style::default().fg(dim)
            } else {
                Style::default().fg(muted)
            },
        ),
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
    let loaded_count = state.loaded_models.len();
    let loaded_line = if loaded_count == 0 {
        Line::from(Span::styled(
            "   No models loaded in VRAM",
            Style::default().fg(dim),
        ))
    } else {
        let preview = state
            .loaded_models
            .iter()
            .take(2)
            .map(|model| model.name.clone())
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

    // Footer-pill provenance: what the VRAM number means, and whether the
    // optional probe answered. Rendered unconditionally — it is the only
    // surface that names the knobs.
    lines.push(state.vram_probe_line());

    // Server-reported block (spec §Model/server behavior): version and the
    // effective request parameters Ollama reports for the selected model
    // (`/api/show` modelfile parameters). Only drawn once a ping has
    // succeeded — before that there is nothing to show.
    if let Some(info) = state.server_info.as_ref() {
        let version_text = info.version.as_deref().unwrap_or("unknown");
        lines.push(Line::from(vec![
            Span::styled("   ", Style::default()),
            Span::styled(format!("server {version_text}"), Style::default().fg(dim)),
        ]));
        let param_rows = state.server_param_rows();
        if param_rows.is_empty() {
            lines.push(Line::from(Span::styled(
                "   Server model defaults in effect (no modelfile overrides)",
                Style::default().fg(dim),
            )));
        } else {
            for (name, value) in param_rows.iter().take(3) {
                lines.push(Line::from(vec![
                    Span::styled("   · ", Style::default().fg(dim)),
                    Span::styled(
                        format!("{:<14}", format!("{name}:")),
                        Style::default().fg(muted),
                    ),
                    Span::styled(value.clone(), Style::default().fg(muted)),
                ]));
            }
            let more = param_rows.len().saturating_sub(3);
            if more > 0 {
                lines.push(Line::from(Span::styled(
                    format!("   +{more} more server parameter(s)"),
                    Style::default().fg(dim),
                )));
            }
        }
    }

    let mut hint_spans = vec![
        Span::styled("enter", Style::default().fg(dim)),
        Span::styled(" open row  ", Style::default().fg(dim)),
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
    let pink = CLAWDE_ACCENT;
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
    let pink = CLAWDE_ACCENT;
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
    let pink = CLAWDE_ACCENT;
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
    let pink = CLAWDE_ACCENT;
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
    let pink = CLAWDE_ACCENT;
    let dim = Color::Rgb(90, 90, 90);
    let muted = Color::Rgb(180, 180, 180);
    let highlight_bg = CLAWDE_ACCENT;
    let highlight_fg = Color::White;
    let dialog_bg = CLAWDE_PANEL_BG;

    render_dark_overlay(frame, area);

    let width = 78u16.min(area.width.saturating_sub(4));
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

    // Row layout: " ▸ ●" (4) + name (30) + size/quant/params (24) + the VRAM
    // column (13). The VRAM column is dropped on narrow terminals so a
    // number is never truncated mid-value.
    const PICKER_ROW_META_WIDTH: u16 = 58;
    const PICKER_VRAM_WIDTH: u16 = 13;
    let show_vram = inner.width >= PICKER_ROW_META_WIDTH + PICKER_VRAM_WIDTH;

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

            // Per-model VRAM from `/api/ps`, shown for loaded models only —
            // this is the metric Ollama actually reports (it exposes no
            // free/total GPU memory).
            let vram_span = show_vram
                .then(|| state.loaded_vram(&model.name))
                .flatten()
                .map(|bytes| {
                    Span::styled(
                        format!(
                            "{:>width$}",
                            format!("{} VRAM", format_vram_display(bytes)),
                            width = PICKER_VRAM_WIDTH as usize
                        ),
                        if is_selected {
                            Style::default().bg(highlight_bg).fg(marker_color)
                        } else {
                            Style::default().fg(marker_color)
                        },
                    )
                });

            let mut spans = vec![
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
            ];
            spans.extend(vram_span);

            lines.push(Line::from(spans));
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

fn render_host_picker(frame: &mut Frame, state: &OllamaConfigDialogState, area: Rect) {
    let pink = CLAWDE_ACCENT;
    let dim = Color::Rgb(90, 90, 90);
    let muted = Color::Rgb(180, 180, 180);
    let highlight_bg = CLAWDE_ACCENT;
    let highlight_fg = Color::White;
    let dialog_bg = CLAWDE_PANEL_BG;

    render_dark_overlay(frame, area);

    let width = 68u16.min(area.width.saturating_sub(4));
    let host_rows = state.discovered_hosts.len().min(MODEL_PICKER_VISIBLE_ROWS) as u16;
    let height = (5 + host_rows + 2).max(9);
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
            " Select Host",
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
        " Ollama servers on the LAN (port 11434)",
        Style::default().fg(muted),
    )]));
    lines.push(Line::from(""));

    if state.discovered_hosts.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            " No servers answered. Run /ollama discover to rescan.",
            Style::default().fg(muted),
        )]));
    } else {
        for (i, host) in state
            .discovered_hosts
            .iter()
            .enumerate()
            .skip(state.host_scroll_offset)
            .take(MODEL_PICKER_VISIBLE_ROWS)
        {
            let is_selected = i == state.selected_host_idx;
            let indicator = if is_selected { "▸" } else { " " };
            let row_style = if is_selected {
                Style::default()
                    .bg(highlight_bg)
                    .fg(highlight_fg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let detail = if host.model_count > 0 {
                format!("{} model(s) · {}ms", host.model_count, host.latency_ms)
            } else {
                format!("{}ms", host.latency_ms)
            };
            lines.push(Line::from(vec![
                Span::styled(format!(" {} ", indicator), row_style),
                Span::styled(format!("{:<40}", host.host_url), row_style),
                Span::styled(
                    detail,
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
    let hint_spans = vec![
        Span::styled("j/k", Style::default().fg(dim)),
        Span::styled(" select  ", Style::default().fg(dim)),
        Span::styled("enter", Style::default().fg(dim)),
        Span::styled(" use host  ", Style::default().fg(dim)),
        Span::styled("r", Style::default().fg(dim)),
        Span::styled(" rescan  ", Style::default().fg(dim)),
        Span::styled("esc", Style::default().fg(dim)),
        Span::styled(" back", Style::default().fg(dim)),
    ];
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

    /// A loaded-model snapshot entry as `/api/ps` reports it.
    fn loaded_model(name: &str, size_vram: Option<u64>) -> clawde_core::OllamaLoadedModel {
        clawde_core::OllamaLoadedModel {
            name: name.to_string(),
            size: size_vram,
            size_vram,
            expires_at: None,
            context_length: None,
        }
    }

    /// Render the dialog on a test backend and return the visible text.
    fn render_screen_for_test(state: &OllamaConfigDialogState) -> String {
        use ratatui::{backend::TestBackend, Terminal};
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        terminal
            .draw(|f| render_ollama_config_dialog(f, state, false, f.area()))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
            out.push('\n');
        }
        out
    }

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
        assert_eq!(state.active_field, OllamaConfigField::Servers);

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

        state.move_prev_field();
        assert_eq!(state.active_field, OllamaConfigField::Model);

        state.move_prev_field();
        assert_eq!(state.active_field, OllamaConfigField::Servers);

        state.move_prev_field();
        assert_eq!(state.active_field, OllamaConfigField::Host);
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
    fn server_param_rows_surface_context_length_and_params() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);
        // Nothing reported yet.
        assert!(state.server_param_rows().is_empty());

        state.set_server_info(clawde_query::OllamaServerInfo {
            version: Some("0.12.6".to_string()),
            params: vec![
                ("temperature".to_string(), "0.7".to_string()),
                ("num_ctx".to_string(), "32768".to_string()),
            ],
            context_length: Some(131_072),
        });
        let rows = state.server_param_rows();
        assert!(rows.contains(&("temperature".to_string(), "0.7".to_string())));
        // num_ctx from the modelfile wins; no synthetic context_length row.
        assert!(rows.contains(&("num_ctx".to_string(), "32768".to_string())));
        assert!(!rows.iter().any(|(name, _)| name == "context_length"));

        // Without a modelfile num_ctx, the architecture context window is
        // surfaced as its own row.
        state.set_server_info(clawde_query::OllamaServerInfo {
            version: None,
            params: vec![("temperature".to_string(), "0.8".to_string())],
            context_length: Some(131_072),
        });
        let rows = state.server_param_rows();
        assert!(rows.contains(&("context_length".to_string(), "131072".to_string())));
    }

    #[test]
    fn server_info_block_renders_version_and_params() {
        let mut state = OllamaConfigDialogState::new();
        state.open(Some("http://gpu.example.test:11434".to_string()), None);
        state.set_server_info(clawde_query::OllamaServerInfo {
            version: Some("0.12.6".to_string()),
            params: vec![
                ("temperature".to_string(), "0.7".to_string()),
                ("top_p".to_string(), "0.9".to_string()),
            ],
            context_length: Some(131_072),
        });
        let out = render_screen_for_test(&state);
        // The Default view should surface the server-reported block.
        assert!(
            out.contains("server 0.12.6"),
            "version should render. Output: {out:?}"
        );
        assert!(
            out.contains("temperature:") && out.contains("0.7"),
            "temperature row should render. Output: {out:?}"
        );
        assert!(
            out.contains("context_length:") && out.contains("131072"),
            "context_length row should render. Output: {out:?}"
        );
    }

    #[test]
    fn server_info_block_absent_before_first_ping() {
        let mut state = OllamaConfigDialogState::new();
        state.open(Some("http://gpu.example.test:11434".to_string()), None);
        let out = render_screen_for_test(&state);
        assert!(
            !out.contains("server "),
            "no server line before a ping succeeded. Output: {out:?}"
        );
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

    /// Flatten a rendered line's spans into plain text.
    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn vram_sample(
        used_bytes: Option<u64>,
        total_bytes: u64,
    ) -> clawde_core::config::OllamaVramSample {
        clawde_core::config::OllamaVramSample {
            used_bytes,
            total_bytes,
        }
    }

    /// Every state the probe can be in must be distinguishable on-screen: an
    /// unreported typo and no command at all cannot look the same, and an
    /// unknown numerator cannot look like an empty card.
    #[test]
    fn vram_probe_line_reports_each_state() {
        use clawde_core::config::{OllamaVramProbe, OllamaVramStatus};
        const GIB: u64 = 1024 * 1024 * 1024;

        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);

        // Nothing configured: the line names both knobs, which is the only
        // place they are discoverable from the TUI.
        let text = line_text(&state.vram_probe_line());
        assert!(text.contains("vram_probe_cmd"), "{text:?}");
        assert!(text.contains("vram_total_mb"), "{text:?}");

        // A working probe: whole-GPU used/total, and it does not claim to be
        // the declared-capacity fallback.
        state.set_vram_status(OllamaVramStatus {
            sample: Some(vram_sample(Some(24 * GIB), 50 * GIB)),
            probe: OllamaVramProbe::Reported {
                used_bytes: 24 * GIB,
                total_bytes: 50 * GIB,
            },
        });
        let text = line_text(&state.vram_probe_line());
        assert!(text.contains("whole-GPU"), "{text:?}");
        assert!(!text.contains("declared capacity"), "{text:?}");

        // A configured-but-failing probe is called out rather than silently
        // rendering as "no command".
        state.set_vram_status(OllamaVramStatus {
            sample: Some(vram_sample(Some(4 * GIB), 8 * GIB)),
            probe: OllamaVramProbe::Failed,
        });
        let text = line_text(&state.vram_probe_line());
        assert!(text.contains("probe failed"), "{text:?}");

        // Declared capacity only.
        state.set_vram_status(OllamaVramStatus {
            sample: Some(vram_sample(Some(4 * GIB), 8 * GIB)),
            probe: OllamaVramProbe::NotConfigured,
        });
        let text = line_text(&state.vram_probe_line());
        assert!(text.contains("declared capacity"), "{text:?}");

        // Declared capacity with an unreachable host: the reading is `--`, not
        // a zero that would read as an empty card.
        state.set_vram_status(OllamaVramStatus {
            sample: Some(vram_sample(None, 8 * GIB)),
            probe: OllamaVramProbe::NotConfigured,
        });
        let text = line_text(&state.vram_probe_line());
        assert!(text.contains("-- of 8.0GiB"), "{text:?}");
    }

    #[test]
    fn default_view_renders_the_vram_probe_line() {
        use clawde_core::config::{OllamaVramProbe, OllamaVramStatus};
        const GIB: u64 = 1024 * 1024 * 1024;

        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);
        state.set_vram_status(OllamaVramStatus {
            sample: Some(vram_sample(Some(4 * GIB), 8 * GIB)),
            probe: OllamaVramProbe::NotConfigured,
        });

        let out = render_screen_for_test(&state);
        assert!(
            out.contains("declared capacity"),
            "the probe line must reach the screen. Output: {out:?}"
        );
    }

    #[test]
    fn test_loaded_model_markers() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);
        state.set_loaded_models(vec![loaded_model("qwen2.5-coder:7b", Some(4_500_000_000))]);

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
        state.set_loaded_models(vec![loaded_model("llama3:latest", Some(2_000_000_000))]);
        assert!(state.is_model_loaded("llama3"));
        assert!(state.is_model_loaded("llama3:latest"));
        // ...but a versioned tag is distinct from any other tag.
        assert!(!state.is_model_loaded("llama3:8b"));
        assert!(!state.is_model_loaded("qwen2.5-coder:7b"));

        // The snapshot survives a refresh cycle (models replaced, loaded
        // names kept) — this is why it lives outside `models`.
        state.set_loaded_models(vec![]);
        assert!(!state.is_model_loaded("qwen2.5-coder:7b"));
    }

    #[test]
    fn test_loaded_vram_lookup() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);
        state.set_loaded_models(vec![
            loaded_model("qwen3:32b", Some(19_300_000_000)),
            loaded_model("llama3:latest", Some(2_000_000_000)),
            // Older servers omit `size_vram`; the marker still shows.
            loaded_model("gemma3:4b", None),
        ]);

        assert_eq!(state.loaded_vram("qwen3:32b"), Some(19_300_000_000));
        // Bare tag and `:latest` resolve to the same loaded entry.
        assert_eq!(state.loaded_vram("llama3"), Some(2_000_000_000));
        // Loaded but with no reported size: marker yes, size no.
        assert!(state.is_model_loaded("gemma3:4b"));
        assert_eq!(state.loaded_vram("gemma3:4b"), None);
        // Not loaded at all.
        assert_eq!(state.loaded_vram("not-installed:1b"), None);
    }

    #[test]
    fn test_model_picker_shows_per_model_vram() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);
        state.set_loaded_models(vec![
            loaded_model("qwen3:32b", Some(19_300_000_000)),
            loaded_model("gemma3:4b", None),
        ]);
        state.ping_success(vec![
            OllamaModel {
                name: "qwen3:32b".to_string(),
                size: 19_800_000_000,
                quantization: "Q4_K_M".to_string(),
                parameter_size: "32B".to_string(),
            },
            OllamaModel {
                name: "gemma3:4b".to_string(),
                size: 3_300_000_000,
                quantization: "Q4_0".to_string(),
                parameter_size: "4B".to_string(),
            },
            OllamaModel {
                name: "llama3:8b".to_string(),
                size: 4_000_000_000,
                quantization: "Q4_0".to_string(),
                parameter_size: "8B".to_string(),
            },
        ]);
        state.phase = OllamaConfigPhase::SelectModel;

        let out = render_screen_for_test(&state);
        // Loaded model row carries its VRAM figure...
        assert!(
            out.contains("18.0GiB VRAM"),
            "expected per-model VRAM in picker rows, got:\n{out}"
        );
        // ...the installed-only row does not, and no bogus zero is drawn.
        assert!(!out.contains("0.0GiB"), "no bogus zero VRAM: \n{out}");
        // Loaded without a reported size: marker only, no VRAM text.
        let gemma_row = out
            .lines()
            .find(|line| line.contains("gemma3:4b"))
            .expect("gemma row rendered");
        assert!(
            !gemma_row.contains("VRAM"),
            "gemma row should have no VRAM column, got: {gemma_row}"
        );
    }

    #[test]
    fn test_vram_display_format() {
        assert_eq!(format_vram_display(19_327_352_832), "18.0GiB");
        assert_eq!(format_vram_display(1_073_741_824), "1.0GiB");
        // Below 1 GiB drops to MiB rather than showing "0.3GiB".
        assert_eq!(format_vram_display(287_000_000), "274MiB");
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
    fn test_update_models_auto_preserves_selection_and_phase() {
        let model = |name: &str| OllamaModel {
            name: name.to_string(),
            size: 1,
            quantization: "Q4".to_string(),
            parameter_size: "1B".to_string(),
        };
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);
        state.ping_success(vec![model("a"), model("b"), model("c")]);
        state.move_model_down(); // select b
        assert_eq!(state.phase, OllamaConfigPhase::SelectModel);
        assert_eq!(state.selected_model().unwrap().name, "b");

        // Auto-poll with the selected model still present: list refreshes
        // in place, phase and selection survive.
        state.update_models_auto(vec![model("a"), model("b"), model("c"), model("d")]);
        assert_eq!(state.models.len(), 4);
        assert_eq!(state.phase, OllamaConfigPhase::SelectModel);
        assert_eq!(state.selected_model().unwrap().name, "b");

        // Selected model vanished: fall back to the first row, stay in the
        // picker (no phase yank).
        state.update_models_auto(vec![model("a"), model("c")]);
        assert_eq!(state.phase, OllamaConfigPhase::SelectModel);
        assert_eq!(state.selected_model().unwrap().name, "a");

        // Authoritative empty list (everything deleted): picker reports no
        // models instead of showing a stale list.
        state.update_models_auto(vec![]);
        assert_eq!(state.phase, OllamaConfigPhase::NoModels);
        assert!(state.models.is_empty());

        // A later poll with models recovers back to the picker.
        state.update_models_auto(vec![model("x")]);
        assert_eq!(state.phase, OllamaConfigPhase::SelectModel);
        assert_eq!(state.models.len(), 1);
    }

    #[test]
    fn test_servers_row_value_states() {
        let mut state = OllamaConfigDialogState::new();
        // Not scanned yet.
        assert_eq!(
            state.servers_row_value(),
            "not scanned — enter scans the LAN"
        );
        // Scanning in flight.
        state.discovery_scanning = true;
        assert_eq!(state.servers_row_value(), "scanning the LAN…");
        // Scanned, nothing found.
        state.discovery_scanning = false;
        state.discovery_checked = true;
        assert_eq!(
            state.servers_row_value(),
            "no Ollama servers answered on the LAN"
        );
        // One host found.
        state.discovered_hosts = vec![DiscoveredHost {
            host_url: "http://192.168.1.45:11434".to_string(),
            latency_ms: 213,
            model_count: 2,
        }];
        assert_eq!(
            state.servers_row_value(),
            "http://192.168.1.45:11434 — 2 model(s), 213ms"
        );
        // Multiple hosts found.
        state.discovered_hosts.push(DiscoveredHost {
            host_url: "http://192.168.1.99:11434".to_string(),
            latency_ms: 5,
            model_count: 1,
        });
        assert_eq!(state.servers_row_value(), "2 servers found — enter to pick");
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
    fn custom_option_values_survive_seed_and_save() {
        // Regression: a hand-set non-preset value used to render as a lossy
        // label that failed to parse back, so opening the screen and pressing
        // Enter (connect) deleted the setting. The full seed -> map path must
        // preserve it exactly.
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);
        let options = serde_json::json!({
            "num_ctx": 49_152u64,
            "num_predict": 3_000u64,
            "keep_alive": 90i64,
            "temperature": 0.15,
            "top_p": 0.93,
        });
        state.set_mode_and_options(false, options.as_object().unwrap());
        assert_eq!(state.num_ctx_label, "49152 (custom)");
        assert_eq!(state.num_predict_label, "3000 (custom)");
        assert_eq!(state.keep_alive_label, "90 (custom)");
        assert_eq!(state.temperature_label, "0.15 (custom)");
        assert_eq!(state.top_p_label, "0.93 (custom)");
        assert_eq!(state.common_options_map(), *options.as_object().unwrap());
    }

    #[test]
    fn string_typed_settings_values_canonicalize_on_seed() {
        // Ollama's wire format accepts keep-alive duration strings and
        // numeric strings; settings may hold either. They must not render as
        // unset (which would delete them on save).
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);
        let options = serde_json::json!({
            "num_ctx": "65536",
            "keep_alive": "5m",
            "temperature": "0.15",
        });
        state.set_mode_and_options(false, options.as_object().unwrap());
        assert_eq!(state.num_ctx_label, "64K");
        assert_eq!(state.keep_alive_label, "5 min");
        assert_eq!(state.temperature_label, "0.15 (custom)");
        let map = state.common_options_map();
        assert_eq!(map.get("num_ctx"), Some(&serde_json::json!(65_536)));
        assert_eq!(map.get("keep_alive"), Some(&serde_json::json!(300)));
        assert_eq!(map.get("temperature"), Some(&serde_json::json!(0.15)));
    }

    #[test]
    fn cycling_from_custom_value_lands_on_neighbor_slots() {
        let mut state = OllamaConfigDialogState::new();
        state.open(None, None);
        state.active_field = OllamaConfigField::Options;
        state.num_ctx_label = "49152 (custom)".to_string();

        // Custom slot sits between unset and the first preset: forward
        // reaches 2K, backward reaches unset.
        state.cycle_option_value(1);
        assert_eq!(state.num_ctx_label, "2K");
        state.num_ctx_label = "49152 (custom)".to_string();
        state.cycle_option_value(-1);
        assert_eq!(state.num_ctx_label, "");

        // Leaving the custom slot is explicit: the custom slot only exists
        // while it is the current value. After cycling away, the list is
        // unset + presets (9 slots), and a full wrap returns to 2K.
        state.cycle_option_value(1); // custom -> 2K
        for _ in 0..(clawde_api::providers::ollama_options::OLLAMA_CTX_PRESETS.len() + 1) {
            state.cycle_option_value(1);
        }
        assert_eq!(state.num_ctx_label, "2K");
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
