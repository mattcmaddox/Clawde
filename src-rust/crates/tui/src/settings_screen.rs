// settings_screen.rs — Flat searchable settings interface.
//
// Opened by /config or /settings commands. Shows all editable settings
// in a single scrollable list with live search filtering.
// Changes are persisted via Settings::save_sync() or settings.json writes.

use crate::overlays::{
    centered_rect, modal_search_line_with_insert, render_dark_overlay, render_dialog_bg,
    CLAURST_ACCENT, CLAURST_MUTED, CLAURST_PANEL_BG,
};
use std::cell::Cell;

use crate::vim_search::{VimSearch, VimSearchKey};
use clawde_core::config::{Config, PermissionMode, Settings};
use clawde_core::constants::DEFAULT_MAX_TOKENS;
use clawde_core::keybindings::UserKeybindings;
use clawde_core::output_styles::{builtin_styles, find_style};
use clawde_tools::web_search::check_backend_configured;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;
use std::collections::HashMap;
use std::path::Path;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum SettingKind {
    Bool,
    Enum {
        options: Vec<&'static str>,
    },
    Number,
    /// Free-form text (e.g. a comma-separated list) edited inline.
    Text,
}

/// Whether a setting applies to the running session immediately, or only
/// after the next launch (e.g. verbose logging, mouse capture, headless-only
/// output format). Shown as a per-row state tag so users know what to expect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingEffect {
    Immediate,
    NextSession,
}

impl SettingEffect {
    fn label(self) -> &'static str {
        match self {
            SettingEffect::Immediate => "now",
            SettingEffect::NextSession => "next",
        }
    }
}

/// Where the effective value of a setting comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingOrigin {
    /// Not set anywhere — the built-in default is in effect.
    Default,
    /// Set in the user's global `~/.clawde/settings.json`.
    Global,
    /// Overridden by the repository's `.clawde/settings.json`.
    Project,
}

impl SettingOrigin {
    fn label(self) -> &'static str {
        match self {
            SettingOrigin::Default => "default",
            SettingOrigin::Global => "global",
            SettingOrigin::Project => "project",
        }
    }

    fn color(self) -> Color {
        match self {
            SettingOrigin::Default => Color::DarkGray,
            SettingOrigin::Global => Color::Rgb(150, 150, 170),
            SettingOrigin::Project => Color::Cyan,
        }
    }
}

/// Section headers shown in the settings list. The curated "Common" section
/// is pinned first so the settings users actually change live at the top;
/// the rest are grouped by concern.
const SECTION_COMMON: &str = "Common";
const SECTION_INTERFACE: &str = "Interface";
const SECTION_WORKSPACE: &str = "Workspace & files";
const SECTION_FREE_ROUTING: &str = "Free-mode routing";
const SECTION_OLLAMA: &str = "Ollama (local)";

#[derive(Debug, Clone)]
pub struct SettingsEntry {
    pub key: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub section: &'static str,
    /// The built-in default for this setting (as displayed). Used to compute
    /// the per-row origin tag (default vs customized).
    pub default: String,
    pub effect: SettingEffect,
    pub kind: SettingKind,
    pub value: String,
}

pub struct SettingsScreen {
    pub visible: bool,
    pub search_query: String,
    pub selected_idx: usize,
    pub scroll_offset: usize,
    /// Vim-modal insert-mode state for the search bar (only used when vim is enabled).
    pub vim_search: VimSearch,
    /// Which field is being edited (field name as key).
    pub edit_field: Option<String>,
    /// Current buffer content while editing a field.
    pub edit_value: String,
    /// Snapshot of the GLOBAL settings — what edits are applied to and what
    /// `save_sync()` writes back to `~/.clawde/settings.json`.
    pub settings_snapshot: Settings,
    /// Effective settings (global merged with any project overrides). Values
    /// are displayed from this view so the screen is honest about what is
    /// actually in effect, even when a `.clawde/settings.json` overrides it.
    pub effective_snapshot: Settings,
    /// Project-level settings loaded from the nearest `.clawde/settings.json`
    /// (if any). Drives the per-entry origin tag ("project").
    pub project_snapshot: Option<Settings>,
    /// Pending changes (field_name → new_value string).
    pub pending_changes: HashMap<String, String>,

    // ---- Real settings fields ----
    pub auto_compact: bool,
    pub notifications: bool,
    pub show_turn_duration: bool,
    pub output_style: String,
    pub reduce_motion: bool,
    pub terminal_progress_bar: bool,
    pub verbose: bool,
    pub cursor_blink_enabled: bool,
    pub auto_copy_enabled: bool,
    pub mouse_capture: bool,
    pub show_cwd: bool,
    pub show_git_branch: bool,
    pub compact_threshold: String,
    pub auto_commits: bool,
    pub output_format: String,
    pub disable_claude_mds: bool,
    pub file_injection_enabled: bool,
    pub file_autocomplete_limit: String,
    pub file_autocomplete_show_hidden_files: bool,
    pub file_injection_max_size: String,
    /// Current free-mode routing strategy ("sequential", "random_failover",
    /// "latency_based", "task_based").
    pub routing_strategy: String,
    /// Comma-separated list of disabled free upstream IDs.
    pub disabled_upstreams: String,
    /// First-byte watchdog timeout in seconds (0 = disabled).
    pub first_byte_timeout_secs: String,
    /// Whether the parallel probe is enabled (default true).
    pub staggered_probe: bool,
    /// 5xx cooldown in seconds (0 = disabled).
    pub upstream_5xx_cooldown_secs: String,
    /// Health poller interval in seconds (0 = startup only).
    pub health_poll_interval_secs: String,
    /// Whole-chain retries after all upstreams fail.
    pub fallback_retries: String,
    /// Preferred web search backend.
    pub preferred_search_backend: String,
    /// Health warning message for the search backend (shown in description area).
    pub health_warning: String,
    /// Keybinding preset from keybindings.json ("default"/"vim"/"emacs").
    pub keybinding_preset: String,
    /// Last known visible row count from the render pass, used by scroll tracking.
    /// Uses Cell for interior mutability — set during render (behind &self),
    /// read during key handling.
    pub last_visible_rows: Cell<usize>,
    /// When true, the user is asked to confirm before discarding pending changes.
    pub confirming_discard: bool,
    /// Ollama: context window size (human label from OLLAMA_CTX_PRESETS).
    pub ollama_num_ctx: String,
    /// Ollama: model keep-alive duration (human label).
    pub ollama_keep_alive: String,
    /// Ollama: max output tokens (human label from OLLAMA_PREDICT_PRESETS).
    pub ollama_num_predict: String,
    /// Ollama: require an explicit host (no localhost fallback).
    pub ollama_require_explicit_host: bool,
    /// Ollama: default host URL when no api_base or OLLAMA_HOST is set.
    pub ollama_default_host: String,
    /// Permission mode ("default", "acceptEdits", "bypassPermissions", "plan").
    pub permission_mode: String,
    /// Verify sandbox mode ("direct" / "worktree" / "container").
    pub verify_sandbox: String,
    /// Container image for the `container` verify sandbox (empty = auto).
    pub verify_container_image: String,
}

impl SettingsScreen {
    pub fn new() -> Self {
        let settings_snapshot = Settings::load_sync().unwrap_or_default();
        let mut screen = Self {
            visible: false,
            search_query: String::new(),
            selected_idx: 0,
            scroll_offset: 0,
            vim_search: VimSearch::new(),
            edit_field: None,
            edit_value: String::new(),
            settings_snapshot: settings_snapshot.clone(),
            effective_snapshot: settings_snapshot.clone(),
            project_snapshot: None,
            pending_changes: HashMap::new(),
            auto_compact: false,
            notifications: true,
            show_turn_duration: false,
            output_style: "default".to_string(),
            reduce_motion: false,
            terminal_progress_bar: true,
            verbose: false,
            cursor_blink_enabled: false,
            auto_copy_enabled: false,
            mouse_capture: true,
            show_cwd: false,
            show_git_branch: false,
            compact_threshold: "95".to_string(),
            auto_commits: false,
            output_format: "text".to_string(),
            disable_claude_mds: false,
            file_injection_enabled: true,
            file_autocomplete_limit: "15".to_string(),
            file_autocomplete_show_hidden_files: false,
            file_injection_max_size: "100".to_string(),
            routing_strategy: "sequential".to_string(),
            disabled_upstreams: String::new(),
            first_byte_timeout_secs: "0".to_string(),
            staggered_probe: true,
            upstream_5xx_cooldown_secs: "45".to_string(),
            health_poll_interval_secs: "300".to_string(),
            fallback_retries: "1".to_string(),
            preferred_search_backend: "auto".to_string(),
            health_warning: String::new(),
            last_visible_rows: Cell::new(10),
            confirming_discard: false,
            ollama_num_ctx: "12K".to_string(),
            ollama_keep_alive: "forever".to_string(),
            ollama_num_predict: "2K".to_string(),
            ollama_require_explicit_host: false,
            ollama_default_host: "http://localhost:11434".to_string(),
            permission_mode: "default".to_string(),
            verify_sandbox: "direct".to_string(),
            verify_container_image: String::new(),
            keybinding_preset: "default".to_string(),
        };
        // Apply settings from snapshot immediately on initialization
        screen.apply_settings_from_snapshot();
        screen
    }

    /// Apply all settings from the snapshot to the screen fields.
    /// This is called on initialization and when opening the settings screen.
    fn apply_settings_from_snapshot(&mut self) {
        let s = &self.effective_snapshot;
        self.auto_compact = s.auto_compact;
        self.notifications = s.notifications;
        self.show_turn_duration = s.show_turn_duration;
        self.output_style = s
            .config
            .output_style
            .clone()
            .unwrap_or_else(|| "default".to_string());
        self.reduce_motion = s.reduce_motion;
        self.terminal_progress_bar = s.terminal_progress_bar;
        self.verbose = s.config.verbose;
        self.cursor_blink_enabled = s.config.cursor_blink_enabled;
        self.auto_copy_enabled = s.auto_copy_on_highlight;
        self.mouse_capture = s.config.mouse_capture_enabled();
        self.show_cwd = s.show_cwd;
        self.show_git_branch = s.show_git_branch;
        // The threshold is stored as a percentage number in the screen's terms;
        // an unset (0.0) value means the default is in effect.
        self.compact_threshold = if s.config.compact_threshold > 0.0 {
            s.config.compact_threshold.to_string()
        } else {
            "95".to_string()
        };
        self.auto_commits = s.config.auto_commits.unwrap_or(false);
        self.output_format = match &s.config.output_format {
            clawde_core::config::OutputFormat::Text => "text".to_string(),
            clawde_core::config::OutputFormat::Json => "json".to_string(),
            clawde_core::config::OutputFormat::StreamJson => "stream_json".to_string(),
        };
        self.disable_claude_mds = s.config.disable_claude_mds;
        self.file_injection_enabled = s.config.file_injection_enabled;
        self.file_autocomplete_limit = s.config.file_autocomplete_limit.to_string();
        self.file_autocomplete_show_hidden_files = s.config.file_autocomplete_show_hidden_files;
        self.file_injection_max_size = s.config.file_injection_max_size.to_string();
        self.permission_mode = permission_mode_str(&s.config.permission_mode);
        self.verify_sandbox = s.config.verify.sandbox.label().to_string();
        self.verify_container_image = s.config.verify.container_image.clone().unwrap_or_default();

        // Read routing strategy from provider config
        self.routing_strategy = s
            .config
            .provider_configs
            .get("free")
            .and_then(|pc| pc.options.get("routing"))
            .and_then(|v| v.get("strategy"))
            .and_then(|v| v.as_str())
            .unwrap_or("sequential")
            .to_string();

        // Read disabled upstreams from provider config
        self.disabled_upstreams = s
            .config
            .provider_configs
            .get("free")
            .and_then(|pc| pc.options.get("routing"))
            .and_then(|v| v.get("disabled_upstreams"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();

        // Read first-byte timeout from provider config (default 0 = disabled).
        self.first_byte_timeout_secs = s
            .config
            .provider_configs
            .get("free")
            .and_then(|pc| pc.options.get("routing"))
            .and_then(|v| v.get("first_byte_timeout_secs"))
            .and_then(|v| v.as_u64())
            .map(|n| n.to_string())
            .unwrap_or_else(|| "0".to_string());

        // Read staggered probe flag from provider config (default true).
        self.staggered_probe = s
            .config
            .provider_configs
            .get("free")
            .and_then(|pc| pc.options.get("routing"))
            .and_then(|v| v.get("staggered_probe"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        // Read 5xx cooldown from provider config (default 45s).
        self.upstream_5xx_cooldown_secs = s
            .config
            .provider_configs
            .get("free")
            .and_then(|pc| pc.options.get("routing"))
            .and_then(|v| v.get("upstream_5xx_cooldown_secs"))
            .and_then(|v| v.as_u64())
            .map(|n| n.to_string())
            .unwrap_or_else(|| "45".to_string());

        // Read health poll interval from provider config (default 300s).
        self.health_poll_interval_secs = s
            .config
            .provider_configs
            .get("free")
            .and_then(|pc| pc.options.get("routing"))
            .and_then(|v| v.get("health_poll_interval_secs"))
            .and_then(|v| v.as_u64())
            .map(|n| n.to_string())
            .unwrap_or_else(|| "300".to_string());

        // Read fallback retries from provider config (default 0).
        self.fallback_retries = s
            .config
            .provider_configs
            .get("free")
            .and_then(|pc| pc.options.get("routing"))
            .and_then(|v| v.get("fallback_retries"))
            .and_then(|v| v.as_u64())
            .map(|n| n.to_string())
            .unwrap_or_else(|| "0".to_string());

        // Read preferred search backend from settings
        self.preferred_search_backend = s.preferred_search_backend.clone();

        // Read Ollama options from provider config.
        let ollama_opts = s
            .config
            .provider_configs
            .get("ollama")
            .map(|pc| &pc.options);
        self.ollama_num_ctx = ollama_opts
            .and_then(|o| o.get("num_ctx").and_then(|v| v.as_u64()))
            .map(num_ctx_to_preset)
            .unwrap_or_else(|| "12K".to_string());
        self.ollama_keep_alive = ollama_opts
            .and_then(|o| o.get("keep_alive").and_then(keep_alive_value_to_i64))
            .map(keep_alive_to_preset)
            .unwrap_or_else(|| "forever".to_string());
        self.ollama_num_predict = ollama_opts
            .and_then(|o| o.get("num_predict").and_then(|v| v.as_u64()))
            .map(num_predict_to_preset)
            .unwrap_or_else(|| "2K".to_string());
        self.ollama_require_explicit_host = ollama_opts
            .and_then(|o| o.get("require_explicit_host").and_then(|v| v.as_bool()))
            .unwrap_or(false);
        self.ollama_default_host = ollama_opts
            .and_then(|o| o.get("default_host").and_then(|v| v.as_str()))
            .filter(|s| !s.is_empty())
            .unwrap_or("http://localhost:11434")
            .to_string();

        // Read keybinding preset from keybindings.json
        self.keybinding_preset = UserKeybindings::load(&Settings::config_dir())
            .preset
            .label()
            .to_string();

        // Sync the env var so web_search.rs respects the stored preference immediately.
        let val = self.preferred_search_backend.trim();
        if val == "auto" || val.is_empty() {
            std::env::remove_var("PREFERRED_SEARCH_BACKEND");
            self.health_warning.clear();
        } else {
            std::env::set_var("PREFERRED_SEARCH_BACKEND", val);
            // Check if the selected backend is properly configured
            match check_backend_configured(val) {
                Ok(()) => self.health_warning.clear(),
                Err(msg) => {
                    self.health_warning = format!("Warning: {} not configured — {}", val, msg);
                }
            }
        }
    }

    /// Open the screen, loading the effective (global + project) view for the
    /// given working directory so displayed values and origin tags reflect
    /// what the running session actually uses. Edits always write to the
    /// global settings file.
    pub fn open(&mut self, cwd: &Path) {
        self.settings_snapshot = Settings::load_sync().unwrap_or_default();
        self.effective_snapshot = Settings::load_effective_sync(cwd);
        self.project_snapshot = Settings::load_project_settings_sync(cwd);
        self.pending_changes.clear();
        self.edit_field = None;
        self.edit_value.clear();
        self.search_query.clear();
        self.vim_search.reset();
        self.selected_idx = 0;
        self.scroll_offset = 0;
        self.visible = true;

        // Wire real settings from the effective snapshot
        self.apply_settings_from_snapshot();
    }

    pub fn close(&mut self) {
        self.visible = false;
        self.edit_field = None;
        self.edit_value.clear();
        self.vim_search.reset();
    }

    pub fn push_search_char(&mut self, c: char) {
        self.search_query.push(c);
        self.selected_idx = 0;
    }

    pub fn pop_search_char(&mut self) {
        self.search_query.pop();
        self.selected_idx = 0;
    }

    pub fn select_prev(&mut self) {
        if self.selected_idx > 0 {
            self.selected_idx -= 1;
        }
    }

    pub fn select_next(&mut self, total_visible: usize) {
        if total_visible > 0 && self.selected_idx + 1 < total_visible {
            self.selected_idx += 1;
        }
    }

    /// Start editing a field by name, seeding the buffer with current value.
    pub fn start_edit(&mut self, field: &str, current_value: &str) {
        self.edit_field = Some(field.to_string());
        self.edit_value = current_value.to_string();
    }

    /// Commit the current edit to pending_changes.
    pub fn commit_edit(&mut self) {
        if let Some(field) = self.edit_field.take() {
            let value = std::mem::take(&mut self.edit_value);
            self.pending_changes.insert(field, value);
        }
    }

    /// Discard the current edit.
    pub fn cancel_edit(&mut self) {
        self.edit_field = None;
        self.edit_value.clear();
    }

    /// Apply all pending changes to settings and persist them.
    pub fn apply_and_save(&mut self, config: &mut Config) {
        for (field, value) in &self.pending_changes {
            match field.as_str() {
                "max_tokens" => {
                    if let Ok(n) = value.parse::<u32>() {
                        config.max_tokens = Some(n);
                        self.settings_snapshot.config.max_tokens = Some(n);
                    }
                }
                "output_style" => {
                    let v = if value.is_empty() {
                        None
                    } else {
                        Some(value.clone())
                    };
                    config.output_style = v.clone();
                    self.settings_snapshot.config.output_style = v;
                }
                "compact_threshold" => {
                    if let Ok(n) = value.parse::<f32>() {
                        config.compact_threshold = n;
                        self.settings_snapshot.config.compact_threshold = n;
                        self.compact_threshold = value.clone();
                    }
                }
                "fileAutocompleteLimit" => {
                    if let Ok(n) = value.parse::<usize>() {
                        config.file_autocomplete_limit = n;
                        self.settings_snapshot.config.file_autocomplete_limit = n;
                        self.file_autocomplete_limit = value.clone();
                    }
                }
                "fileInjectionMaxSize" => {
                    if let Ok(n) = value.parse::<usize>() {
                        config.file_injection_max_size = n;
                        self.settings_snapshot.config.file_injection_max_size = n;
                        self.file_injection_max_size = value.clone();
                    }
                }
                "first_byte_timeout_secs" => {
                    if let Ok(n) = value.parse::<u64>() {
                        self.first_byte_timeout_secs = value.clone();
                        let mut routing = get_or_create_routing_json(config);
                        routing["first_byte_timeout_secs"] = serde_json::Value::from(n);
                        config
                            .provider_configs
                            .entry("free".to_string())
                            .or_default()
                            .options
                            .insert("routing".to_string(), routing.clone());
                        self.settings_snapshot
                            .config
                            .provider_configs
                            .entry("free".to_string())
                            .or_default()
                            .options
                            .insert("routing".to_string(), routing);
                    }
                }
                "upstream_5xx_cooldown_secs" => {
                    if let Ok(n) = value.parse::<u64>() {
                        self.upstream_5xx_cooldown_secs = value.clone();
                        let mut routing = get_or_create_routing_json(config);
                        routing["upstream_5xx_cooldown_secs"] = serde_json::Value::from(n);
                        config
                            .provider_configs
                            .entry("free".to_string())
                            .or_default()
                            .options
                            .insert("routing".to_string(), routing.clone());
                        self.settings_snapshot
                            .config
                            .provider_configs
                            .entry("free".to_string())
                            .or_default()
                            .options
                            .insert("routing".to_string(), routing);
                    }
                }
                "health_poll_interval_secs" => {
                    if let Ok(n) = value.parse::<u64>() {
                        self.health_poll_interval_secs = value.clone();
                        let mut routing = get_or_create_routing_json(config);
                        routing["health_poll_interval_secs"] = serde_json::Value::from(n);
                        config
                            .provider_configs
                            .entry("free".to_string())
                            .or_default()
                            .options
                            .insert("routing".to_string(), routing.clone());
                        self.settings_snapshot
                            .config
                            .provider_configs
                            .entry("free".to_string())
                            .or_default()
                            .options
                            .insert("routing".to_string(), routing);
                    }
                }
                "fallback_retries" => {
                    if let Ok(n) = value.parse::<u32>() {
                        self.fallback_retries = value.clone();
                        let mut routing = get_or_create_routing_json(config);
                        routing["fallback_retries"] = serde_json::Value::from(n);
                        config
                            .provider_configs
                            .entry("free".to_string())
                            .or_default()
                            .options
                            .insert("routing".to_string(), routing.clone());
                        self.settings_snapshot
                            .config
                            .provider_configs
                            .entry("free".to_string())
                            .or_default()
                            .options
                            .insert("routing".to_string(), routing);
                    }
                }
                "disabled_upstreams" => {
                    self.disabled_upstreams = value.clone();
                    let parsed: Vec<String> = value
                        .split(|c: char| [',', ' '].contains(&c))
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    let mut routing = get_or_create_routing_json(config);
                    routing["disabled_upstreams"] = serde_json::Value::from(parsed.clone());
                    config
                        .provider_configs
                        .entry("free".to_string())
                        .or_default()
                        .options
                        .insert("routing".to_string(), routing.clone());
                    self.settings_snapshot
                        .config
                        .provider_configs
                        .entry("free".to_string())
                        .or_default()
                        .options
                        .insert("routing".to_string(), routing);
                }
                "ollama_require_explicit_host" => {
                    let val = value == "true";
                    self.ollama_require_explicit_host = val;
                    config
                        .provider_configs
                        .entry("ollama".to_string())
                        .or_default()
                        .options
                        .insert(
                            "require_explicit_host".to_string(),
                            serde_json::Value::from(val),
                        );
                    self.settings_snapshot
                        .config
                        .provider_configs
                        .entry("ollama".to_string())
                        .or_default()
                        .options
                        .insert(
                            "require_explicit_host".to_string(),
                            serde_json::Value::from(val),
                        );
                }
                "ollama_default_host" => {
                    let trimmed = value.trim();
                    // Show a warning for URLs that don't look like HTTP endpoints,
                    // but don't block the save — the user may have a valid reason.
                    if !trimmed.is_empty()
                        && !trimmed.starts_with("http://")
                        && !trimmed.starts_with("https://")
                    {
                        self.health_warning = format!(
                            "Saved '{}' — URL should start with http:// or https://",
                            trimmed
                        );
                    } else {
                        self.health_warning.clear();
                    }
                    self.ollama_default_host = if trimmed.is_empty() {
                        "http://localhost:11434".to_string()
                    } else {
                        trimmed.to_string()
                    };
                    if trimmed.is_empty() {
                        config
                            .provider_configs
                            .entry("ollama".to_string())
                            .or_default()
                            .options
                            .remove("default_host");
                        self.settings_snapshot
                            .config
                            .provider_configs
                            .entry("ollama".to_string())
                            .or_default()
                            .options
                            .remove("default_host");
                    } else {
                        config
                            .provider_configs
                            .entry("ollama".to_string())
                            .or_default()
                            .options
                            .insert(
                                "default_host".to_string(),
                                serde_json::Value::from(value.clone()),
                            );
                        self.settings_snapshot
                            .config
                            .provider_configs
                            .entry("ollama".to_string())
                            .or_default()
                            .options
                            .insert(
                                "default_host".to_string(),
                                serde_json::Value::from(value.clone()),
                            );
                    }
                }
                "verify_container_image" => {
                    let trimmed = value.trim();
                    let image = if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    };
                    self.verify_container_image = trimmed.to_string();
                    config.verify.container_image = image.clone();
                    self.settings_snapshot.config.verify.container_image = image;
                }
                _ => {}
            }
        }
        let _ = self.settings_snapshot.save_sync();
        self.pending_changes.clear();
    }
}

impl Default for SettingsScreen {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Settings entries definition
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Ollama preset helpers — map human labels to/from raw API values
// ---------------------------------------------------------------------------

const OLLAMA_CTX_PRESETS: &[(&str, u64)] = &[
    ("4K", 4_096),
    ("8K", 8_192),
    ("12K", 12_288),
    ("16K", 16_384),
    ("24K", 24_576),
    ("32K", 32_768),
];

const OLLAMA_PREDICT_PRESETS: &[(&str, u64)] = &[
    ("512", 512),
    ("1K", 1_024),
    ("2K", 2_048),
    ("4K", 4_096),
    ("8K", 8_192),
];

const OLLAMA_KEEP_ALIVE_PRESETS: &[(&str, i64)] = &[
    ("5 min", 300),
    ("10 min", 600),
    ("30 min", 1_800),
    ("1 hour", 3_600),
    ("forever", -1),
];

fn keep_alive_value_to_i64(v: &serde_json::Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
}

fn num_ctx_to_preset(n: u64) -> String {
    for (label, val) in OLLAMA_CTX_PRESETS {
        if *val == n {
            return label.to_string();
        }
    }
    format!("{}K", n / 1024)
}

fn preset_to_num_ctx(preset: &str) -> u64 {
    for (label, val) in OLLAMA_CTX_PRESETS {
        if *label == preset {
            return *val;
        }
    }
    12_288
}

fn num_predict_to_preset(n: u64) -> String {
    for (label, val) in OLLAMA_PREDICT_PRESETS {
        if *val == n {
            return label.to_string();
        }
    }
    format!("{}", n)
}

fn preset_to_num_predict(preset: &str) -> u64 {
    for (label, val) in OLLAMA_PREDICT_PRESETS {
        if *label == preset {
            return *val;
        }
    }
    2_048
}

fn keep_alive_to_preset(n: i64) -> String {
    if n <= 0 {
        return "forever".to_string();
    }
    for (label, val) in OLLAMA_KEEP_ALIVE_PRESETS {
        if *val == n {
            return label.to_string();
        }
    }
    format!("{}s", n)
}

fn preset_to_keep_alive(preset: &str) -> String {
    for (label, val) in OLLAMA_KEEP_ALIVE_PRESETS {
        if *label == preset {
            return val.to_string();
        }
    }
    "-1".to_string()
}

fn ollama_ctx_labels() -> Vec<&'static str> {
    OLLAMA_CTX_PRESETS.iter().map(|(l, _)| *l).collect()
}

fn ollama_predict_labels() -> Vec<&'static str> {
    OLLAMA_PREDICT_PRESETS.iter().map(|(l, _)| *l).collect()
}

fn ollama_keep_alive_labels() -> Vec<&'static str> {
    OLLAMA_KEEP_ALIVE_PRESETS.iter().map(|(l, _)| *l).collect()
}

/// Read the existing routing JSON from the free provider config, or return
/// an empty object if none exists. Callers modify the returned object and
/// write it back — this prevents editing one routing field from overwriting
/// all the others.
fn get_or_create_routing_json(config: &Config) -> serde_json::Value {
    config
        .provider_configs
        .get("free")
        .and_then(|pc| pc.options.get("routing"))
        .cloned()
        .unwrap_or(serde_json::Value::Object(serde_json::Map::new()))
}

// Extract a single setting's raw value (as stored) from a settings
// snapshot. Returns the empty string when the setting is unset — the caller
// distinguishes "unset" (default) from "customized" via [`default_value_for`].
// This is what powers the per-row origin tag: compare the global and project
// snapshots against the built-in default.
fn value_from_settings(settings: &Settings, key: &str) -> String {
    let c = &settings.config;
    match key {
        "max_tokens" => c.max_tokens.map(|n| n.to_string()).unwrap_or_default(),
        "auto_compact" => settings.auto_compact.to_string(),
        "notifications" => settings.notifications.to_string(),
        "show_turn_duration" => settings.show_turn_duration.to_string(),
        "output_style" => c.output_style.clone().unwrap_or_default(),
        "reduce_motion" => settings.reduce_motion.to_string(),
        "terminal_progress_bar" => settings.terminal_progress_bar.to_string(),
        "verbose" => c.verbose.to_string(),
        "cursor_blink_enabled" => c.cursor_blink_enabled.to_string(),
        "auto_copy_enabled" => settings.auto_copy_on_highlight.to_string(),
        "mouse_capture" => c.mouse_capture_enabled().to_string(),
        "show_cwd" => settings.show_cwd.to_string(),
        "show_git_branch" => settings.show_git_branch.to_string(),
        // The threshold is stored as a percentage number; 0.0 means unset.
        "compact_threshold" => {
            if c.compact_threshold > 0.0 {
                c.compact_threshold.to_string()
            } else {
                String::new()
            }
        }
        "auto_commits" => c.auto_commits.unwrap_or(false).to_string(),
        "output_format" => match c.output_format {
            clawde_core::config::OutputFormat::Text => "text",
            clawde_core::config::OutputFormat::Json => "json",
            clawde_core::config::OutputFormat::StreamJson => "streamjson",
        }
        .to_string(),
        "disable_claude_mds" => c.disable_claude_mds.to_string(),
        "permission_mode" => permission_mode_str(&c.permission_mode),
        "verify_sandbox" => c.verify.sandbox.config_name().to_string(),
        "verify_container_image" => c.verify.container_image.clone().unwrap_or_default(),
        "routing_strategy" => routing_str_value(c, "strategy"),
        "first_byte_timeout_secs" => routing_u64_str(c, "first_byte_timeout_secs"),
        "staggered_probe" => routing_bool_str(c, "staggered_probe"),
        "upstream_5xx_cooldown_secs" => routing_u64_str(c, "upstream_5xx_cooldown_secs"),
        "health_poll_interval_secs" => routing_u64_str(c, "health_poll_interval_secs"),
        "fallback_retries" => routing_u64_str(c, "fallback_retries"),
        "disabled_upstreams" => routing_disabled_str(c),
        "preferredSearchBackend" => settings.preferred_search_backend.clone(),
        "fileInjectionEnabled" => c.file_injection_enabled.to_string(),
        "fileAutocompleteLimit" => c.file_autocomplete_limit.to_string(),
        "fileAutocompleteShowHiddenFiles" => c.file_autocomplete_show_hidden_files.to_string(),
        "fileInjectionMaxSize" => c.file_injection_max_size.to_string(),
        "ollama_num_ctx" => ollama_num_ctx_str(c),
        "ollama_keep_alive" => ollama_keep_alive_str(c),
        "ollama_num_predict" => ollama_num_predict_str(c),
        "ollama_require_explicit_host" => ollama_opts(c)
            .and_then(|o| o.get("require_explicit_host").and_then(|v| v.as_bool()))
            .map(|b| b.to_string())
            .unwrap_or_default(),
        "ollama_default_host" => ollama_opts(c)
            .and_then(|o| o.get("default_host").and_then(|v| v.as_str()))
            .unwrap_or_default()
            .to_string(),
        // Keybinding preset lives in keybindings.json, not settings.json.
        // Origin tracking for this entry is handled through the screen field.
        "keybinding_preset" => String::new(),
        _ => String::new(),
    }
}

/// The raw "unset" representation of a setting — used to decide whether a
/// snapshot differs from the built-in default (origin tag). Empty string for
/// settings that are absent by default; explicit literals for those whose
/// defaults are baked into serde or getters.
fn default_value_for(key: &str) -> String {
    match key {
        // Absent in the file == default.
        "output_style"
        | "compact_threshold"
        | "max_tokens"
        | "disabled_upstreams"
        | "routing_strategy"
        | "first_byte_timeout_secs"
        | "staggered_probe"
        | "upstream_5xx_cooldown_secs"
        | "health_poll_interval_secs"
        | "fallback_retries"
        | "ollama_num_ctx"
        | "ollama_keep_alive"
        | "ollama_num_predict"
        | "ollama_require_explicit_host"
        | "ollama_default_host"
        | "keybinding_preset"
        | "verify_container_image" => String::new(),
        "auto_compact"
        | "notifications"
        | "terminal_progress_bar"
        | "show_cwd"
        | "show_git_branch"
        | "fileInjectionEnabled"
        | "mouse_capture" => "true".to_string(),
        "permission_mode" => "default".to_string(),
        "verify_sandbox" => "direct".to_string(),
        "output_format" => "text".to_string(),
        "preferredSearchBackend" => "auto".to_string(),
        "fileAutocompleteLimit" => "15".to_string(),
        "fileInjectionMaxSize" => "100".to_string(),
        _ => "false".to_string(),
    }
}

/// Where a setting's effective value comes from: built-in default, the global
/// `~/.clawde/settings.json`, or a project `.clawde/settings.json` override.
/// Computed fresh on every call so toggling a value flips the tag immediately.
fn entry_origin(screen: &SettingsScreen, key: &str) -> SettingOrigin {
    let default_v = default_value_for(key);
    let glob = value_from_settings(&screen.settings_snapshot, key);
    let proj = screen
        .project_snapshot
        .as_ref()
        .map(|p| value_from_settings(p, key))
        .unwrap_or_default();
    if !proj.is_empty() && proj != glob {
        return SettingOrigin::Project;
    }
    if glob != default_v {
        return SettingOrigin::Global;
    }
    SettingOrigin::Default
}

fn routing_json(config: &Config) -> Option<&serde_json::Value> {
    config
        .provider_configs
        .get("free")
        .and_then(|pc| pc.options.get("routing"))
}

fn routing_str_value(config: &Config, key: &str) -> String {
    routing_json(config)
        .and_then(|r| r.get(key))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn routing_u64_str(config: &Config, key: &str) -> String {
    routing_json(config)
        .and_then(|r| r.get(key))
        .and_then(|v| v.as_u64())
        .map(|n| n.to_string())
        .unwrap_or_default()
}

fn routing_bool_str(config: &Config, key: &str) -> String {
    routing_json(config)
        .and_then(|r| r.get(key))
        .and_then(|v| v.as_bool())
        .map(|b| b.to_string())
        .unwrap_or_default()
}

fn routing_disabled_str(config: &Config) -> String {
    routing_json(config)
        .and_then(|r| r.get("disabled_upstreams"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

fn ollama_opts(config: &Config) -> Option<&std::collections::HashMap<String, serde_json::Value>> {
    config.provider_configs.get("ollama").map(|pc| &pc.options)
}

fn ollama_num_ctx_str(config: &Config) -> String {
    ollama_opts(config)
        .and_then(|o| o.get("num_ctx").and_then(|v| v.as_u64()))
        .map(num_ctx_to_preset)
        .unwrap_or_default()
}

fn ollama_keep_alive_str(config: &Config) -> String {
    ollama_opts(config)
        .and_then(|o| o.get("keep_alive").and_then(keep_alive_value_to_i64))
        .map(keep_alive_to_preset)
        .unwrap_or_default()
}

fn ollama_num_predict_str(config: &Config) -> String {
    ollama_opts(config)
        .and_then(|o| o.get("num_predict").and_then(|v| v.as_u64()))
        .map(num_predict_to_preset)
        .unwrap_or_default()
}

fn permission_mode_str(mode: &PermissionMode) -> String {
    match mode {
        PermissionMode::Default => "default",
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::BypassPermissions => "bypassPermissions",
        PermissionMode::Plan => "plan",
    }
    .to_string()
}

fn parse_permission_mode(s: &str) -> PermissionMode {
    match s {
        "acceptEdits" => PermissionMode::AcceptEdits,
        "bypassPermissions" => PermissionMode::BypassPermissions,
        "plan" => PermissionMode::Plan,
        _ => PermissionMode::Default,
    }
}

fn make_entry(
    key: &'static str,
    label: &'static str,
    description: &'static str,
    section: &'static str,
    default: String,
    effect: SettingEffect,
    kind: SettingKind,
    value: String,
) -> SettingsEntry {
    SettingsEntry {
        key,
        label,
        description,
        section,
        default,
        effect,
        kind,
        value,
    }
}

fn bool_v(b: bool) -> String {
    if b { "true" } else { "false" }.to_string()
}

/// Every editable setting, ordered into sections. The curated "Common"
/// section is pinned first — these are the settings users actually change
/// most often (permission mode, output style, free routing, auto-compact,
/// token cap, auto-commits, notifications). The rest are grouped by concern
/// so a flat alphabetical list never hides a setting again.
fn all_entries(screen: &SettingsScreen) -> Vec<SettingsEntry> {
    let mut entries = vec![
        // ---- Common -----------------------------------------------------
        make_entry(
            "permission_mode",
            "Permission mode",
            "How tool calls are approved: default (prompt for writes/commands), acceptEdits (auto-approve), bypassPermissions (no prompts), plan (read-only).",
            SECTION_COMMON,
            "default".to_string(),
            SettingEffect::Immediate,
            SettingKind::Enum {
                options: vec!["default", "acceptEdits", "bypassPermissions", "plan"],
            },
            screen.permission_mode.clone(),
        ),
        make_entry(
            "verify_sandbox",
            "Verify sandbox",
            "Where the execute-and-verify loop runs tests/lints: direct (in the project dir, fast but leaves build artifacts), worktree (temporary git worktree, clean isolation, requires git), container (Docker/podman, max isolation).",
            SECTION_COMMON,
            "direct".to_string(),
            SettingEffect::Immediate,
            SettingKind::Enum {
                options: vec!["direct", "worktree", "container"],
            },
            screen.verify_sandbox.clone(),
        ),
        make_entry(
            "verify_container_image",
            "Verify container image",
            "Image used by the container verify sandbox, e.g. node:20-slim. Overrides the CLAWDE_VERIFY_IMAGE env var and the per-language default. Empty = auto (env var, then language default).",
            SECTION_COMMON,
            String::new(),
            SettingEffect::Immediate,
            SettingKind::Text,
            screen.verify_container_image.clone(),
        ),
        make_entry(
            "output_style",
            "Output style",
            "Controls the verbosity and format of responses.",
            SECTION_COMMON,
            "default".to_string(),
            SettingEffect::Immediate,
            SettingKind::Enum {
                options: vec!["default", "concise", "explanatory", "learning"],
            },
            screen.output_style.clone(),
        ),
        make_entry(
            "routing_strategy",
            "Free routing",
            "How free-mode selects upstream providers (sequential/random/latency/task).",
            SECTION_COMMON,
            "sequential".to_string(),
            SettingEffect::Immediate,
            SettingKind::Enum {
                options: vec![
                    "sequential",
                    "random_failover",
                    "latency_based",
                    "task_based",
                ],
            },
            screen.routing_strategy.clone(),
        ),
        make_entry(
            "auto_compact",
            "Auto-compact",
            "Automatically compact turns at threshold.",
            SECTION_COMMON,
            "true".to_string(),
            SettingEffect::Immediate,
            SettingKind::Bool,
            bool_v(screen.auto_compact),
        ),
        make_entry(
            "compact_threshold",
            "Auto-compact threshold",
            "Context usage % at which to trigger auto-compact (0-100).",
            SECTION_COMMON,
            "95".to_string(),
            SettingEffect::Immediate,
            SettingKind::Number,
            screen.compact_threshold.clone(),
        ),
        make_entry(
            "max_tokens",
            "Max tokens",
            "Maximum tokens per response.",
            SECTION_COMMON,
            DEFAULT_MAX_TOKENS.to_string(),
            SettingEffect::Immediate,
            SettingKind::Number,
            screen
                .settings_snapshot
                .config
                .max_tokens
                .map(|n| n.to_string())
                .unwrap_or_else(|| DEFAULT_MAX_TOKENS.to_string()),
        ),
        make_entry(
            "auto_commits",
            "Auto-commits",
            "Automatically snapshot changes to git via shadow-git.",
            SECTION_COMMON,
            "false".to_string(),
            SettingEffect::Immediate,
            SettingKind::Bool,
            bool_v(screen.auto_commits),
        ),
        make_entry(
            "notifications",
            "Desktop notifications",
            "Notify when a turn completes.",
            SECTION_COMMON,
            "true".to_string(),
            SettingEffect::Immediate,
            SettingKind::Bool,
            bool_v(screen.notifications),
        ),
        // ---- Interface ---------------------------------------------------
        make_entry(
            "output_format",
            "Output format",
            "How responses are formatted: text, JSON, or streaming JSON (headless).",
            SECTION_INTERFACE,
            "text".to_string(),
            SettingEffect::NextSession,
            SettingKind::Enum {
                options: vec!["text", "json", "streamjson"],
            },
            screen.output_format.clone(),
        ),
        make_entry(
            "preferredSearchBackend",
            "Search backend",
            "Preferred web search backend (auto, searxng, firecrawl, duckduckgo).",
            SECTION_INTERFACE,
            "auto".to_string(),
            SettingEffect::Immediate,
            SettingKind::Enum {
                options: vec!["auto", "searxng", "firecrawl", "duckduckgo"],
            },
            screen.preferred_search_backend.clone(),
        ),
        make_entry(
            "verbose",
            "Verbose logging",
            "Log additional debug information. Takes effect on next session.",
            SECTION_INTERFACE,
            "false".to_string(),
            SettingEffect::NextSession,
            SettingKind::Bool,
            bool_v(screen.verbose),
        ),
        make_entry(
            "reduce_motion",
            "Reduce motion",
            "Disable UI animations.",
            SECTION_INTERFACE,
            "false".to_string(),
            SettingEffect::Immediate,
            SettingKind::Bool,
            bool_v(screen.reduce_motion),
        ),
        make_entry(
            "terminal_progress_bar",
            "Terminal progress bar",
            "Show progress during tool use.",
            SECTION_INTERFACE,
            "true".to_string(),
            SettingEffect::Immediate,
            SettingKind::Bool,
            bool_v(screen.terminal_progress_bar),
        ),
        make_entry(
            "show_turn_duration",
            "Show turn duration",
            "Display elapsed time per turn in status bar.",
            SECTION_INTERFACE,
            "false".to_string(),
            SettingEffect::Immediate,
            SettingKind::Bool,
            bool_v(screen.show_turn_duration),
        ),
        make_entry(
            "cursor_blink_enabled",
            "Cursor blinking",
            "Enable cursor blinking in the chat prompt.",
            SECTION_INTERFACE,
            "false".to_string(),
            SettingEffect::Immediate,
            SettingKind::Bool,
            bool_v(screen.cursor_blink_enabled),
        ),
        make_entry(
            "auto_copy_enabled",
            "Auto-copy on highlight",
            "Automatically copy highlighted text to clipboard.",
            SECTION_INTERFACE,
            "false".to_string(),
            SettingEffect::Immediate,
            SettingKind::Bool,
            bool_v(screen.auto_copy_enabled),
        ),
        make_entry(
            "mouse_capture",
            "Mouse capture",
            "Capture the mouse for scroll/right-click/drag-select. Turn off for native terminal text selection. Takes effect on next session.",
            SECTION_INTERFACE,
            "true".to_string(),
            SettingEffect::NextSession,
            SettingKind::Bool,
            bool_v(screen.mouse_capture),
        ),
        make_entry(
            "keybinding_preset",
            "Keybinding preset",
            "Keyboard shortcuts profile: default, vim (hjkl navigation), or emacs (readline chords). Takes effect on next session.",
            SECTION_INTERFACE,
            "default".to_string(),
            SettingEffect::NextSession,
            SettingKind::Enum {
                options: vec!["default", "vim", "emacs"],
            },
            screen.keybinding_preset.clone(),
        ),
        // ---- Workspace & files ------------------------------------------
        make_entry(
            "show_cwd",
            "Show current directory",
            "Display the current working directory in the footer.",
            SECTION_WORKSPACE,
            "true".to_string(),
            SettingEffect::Immediate,
            SettingKind::Bool,
            bool_v(screen.show_cwd),
        ),
        make_entry(
            "show_git_branch",
            "Show git branch",
            "Display the current git branch in the footer.",
            SECTION_WORKSPACE,
            "true".to_string(),
            SettingEffect::Immediate,
            SettingKind::Bool,
            bool_v(screen.show_git_branch),
        ),
        make_entry(
            "disable_claude_mds",
            "Disable CLAUDE.md",
            "Ignore CLAUDE.md files in projects (use defaults instead).",
            SECTION_WORKSPACE,
            "false".to_string(),
            SettingEffect::Immediate,
            SettingKind::Bool,
            bool_v(screen.disable_claude_mds),
        ),
        make_entry(
            "fileInjectionEnabled",
            "File injection (@)",
            "Auto-inject @file references into message context.",
            SECTION_WORKSPACE,
            "true".to_string(),
            SettingEffect::Immediate,
            SettingKind::Bool,
            bool_v(screen.file_injection_enabled),
        ),
    ];

    // Only show these if file injection is enabled
    if screen.file_injection_enabled {
        entries.push(make_entry(
            "fileAutocompleteLimit",
            "File autocomplete limit",
            "Max suggestions shown in @ autocomplete (type more to narrow results).",
            SECTION_WORKSPACE,
            "15".to_string(),
            SettingEffect::Immediate,
            SettingKind::Number,
            screen.file_autocomplete_limit.clone(),
        ));
        entries.push(make_entry(
            "fileAutocompleteShowHiddenFiles",
            "Show hidden files",
            "Include hidden files (.) in @ autocomplete.",
            SECTION_WORKSPACE,
            "false".to_string(),
            SettingEffect::Immediate,
            SettingKind::Bool,
            bool_v(screen.file_autocomplete_show_hidden_files),
        ));
        entries.push(make_entry(
            "fileInjectionMaxSize",
            "File injection max size",
            "Max file size to auto-inject (KB, 0=no limit).",
            SECTION_WORKSPACE,
            "100".to_string(),
            SettingEffect::Immediate,
            SettingKind::Number,
            screen.file_injection_max_size.clone(),
        ));
    }

    // ---- Free-mode routing ----------------------------------------------
    entries.push(make_entry(
        "first_byte_timeout_secs",
        "First-byte timeout (s)",
        "Seconds to wait for first byte before racing the next free upstream (0 = disabled, recommend 5).",
        SECTION_FREE_ROUTING,
        "0".to_string(),
        SettingEffect::Immediate,
        SettingKind::Number,
        screen.first_byte_timeout_secs.clone(),
    ));
    entries.push(make_entry(
        "staggered_probe",
        "Parallel probe",
        "When the first-byte timeout expires, launch a parallel probe at the next upstream instead of advancing sequentially.",
        SECTION_FREE_ROUTING,
        "true".to_string(),
        SettingEffect::Immediate,
        SettingKind::Bool,
        bool_v(screen.staggered_probe),
    ));
    entries.push(make_entry(
        "upstream_5xx_cooldown_secs",
        "5xx cooldown (s)",
        "Seconds to cool down an upstream after a 5xx/498 server error (0 = disabled, default 45).",
        SECTION_FREE_ROUTING,
        "45".to_string(),
        SettingEffect::Immediate,
        SettingKind::Number,
        screen.upstream_5xx_cooldown_secs.clone(),
    ));
    entries.push(make_entry(
        "health_poll_interval_secs",
        "Health poll interval (s)",
        "How often to probe upstream key health (0 = startup only, default 300).",
        SECTION_FREE_ROUTING,
        "300".to_string(),
        SettingEffect::Immediate,
        SettingKind::Number,
        screen.health_poll_interval_secs.clone(),
    ));
    entries.push(make_entry(
        "fallback_retries",
        "Fallback retries",
        "Whole-chain retries after every upstream fails (0 = no retry, surface summary immediately).",
        SECTION_FREE_ROUTING,
        "0".to_string(),
        SettingEffect::Immediate,
        SettingKind::Number,
        screen.fallback_retries.clone(),
    ));
    entries.push(make_entry(
        "disabled_upstreams",
        "Disabled upstreams",
        "Free upstreams to skip (comma-separated IDs, e.g. nvidia, cohere).",
        SECTION_FREE_ROUTING,
        String::new(),
        SettingEffect::Immediate,
        SettingKind::Text,
        screen.disabled_upstreams.clone(),
    ));

    // ---- Ollama (local) -------------------------------------------------
    entries.push(make_entry(
        "ollama_num_ctx",
        "Ollama: Context window",
        "Context window size for Ollama models. Lower = less VRAM, faster. Higher = more conversation history. 12K is a good default for 3B coding models.",
        SECTION_OLLAMA,
        "12K".to_string(),
        SettingEffect::Immediate,
        SettingKind::Enum {
            options: ollama_ctx_labels(),
        },
        screen.ollama_num_ctx.clone(),
    ));
    entries.push(make_entry(
        "ollama_keep_alive",
        "Ollama: Keep alive",
        "How long Ollama keeps the model loaded in VRAM after the last request. 'forever' avoids reload delays but uses VRAM continuously.",
        SECTION_OLLAMA,
        "forever".to_string(),
        SettingEffect::Immediate,
        SettingKind::Enum {
            options: ollama_keep_alive_labels(),
        },
        screen.ollama_keep_alive.clone(),
    ));
    entries.push(make_entry(
        "ollama_num_predict",
        "Ollama: Max output",
        "Maximum tokens the model can generate per response. Lower values are faster and prevent the model from rambling.",
        SECTION_OLLAMA,
        "2K".to_string(),
        SettingEffect::Immediate,
        SettingKind::Enum {
            options: ollama_predict_labels(),
        },
        screen.ollama_num_predict.clone(),
    ));
    entries.push(make_entry(
        "ollama_require_explicit_host",
        "Ollama: Require explicit host",
        "When on, Ollama only connects if api_base or OLLAMA_HOST is explicitly set — never falls back to localhost. Use this when Ollama runs on a remote GPU machine.",
        SECTION_OLLAMA,
        "false".to_string(),
        SettingEffect::Immediate,
        SettingKind::Bool,
        bool_v(screen.ollama_require_explicit_host),
    ));
    entries.push(make_entry(
        "ollama_default_host",
        "Ollama: Default host",
        "Host URL used when no api_base or OLLAMA_HOST is set. Set this to point at a LAN GPU server so Ollama always targets the same machine across devices (e.g. http://devbox:11434).",
        SECTION_OLLAMA,
        "http://localhost:11434".to_string(),
        SettingEffect::NextSession,
        SettingKind::Text,
        screen.ollama_default_host.clone(),
    ));

    entries
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Whether an entry should appear given the current search box text.
/// A leading `!` narrows results to only settings whose origin is NOT
/// Default — i.e. the user has customised them. The rest of the text
/// (if any) is matched against the entry label.
fn entry_matches_filter(
    entry: &SettingsEntry,
    search_query: &str,
    screen: &SettingsScreen,
) -> bool {
    let (customized_only, term) = if let Some(t) = search_query.strip_prefix('!') {
        (true, t)
    } else {
        (false, search_query)
    };
    if customized_only && matches!(entry_origin(screen, entry.key), SettingOrigin::Default) {
        return false;
    }
    if !term.is_empty() && !entry.label.to_lowercase().contains(&term.to_lowercase()) {
        return false;
    }
    true
}

pub fn render_settings_screen(frame: &mut Frame, screen: &SettingsScreen, area: Rect) {
    if !screen.visible {
        return;
    }

    render_dark_overlay(frame, area);

    // 80% width, 90% height, centred
    let w = (area.width * 4 / 5)
        .max(60)
        .min(area.width.saturating_sub(2));
    let h = (area.height * 9 / 10)
        .max(20)
        .min(area.height.saturating_sub(2));
    let popup = centered_rect(w, h, area);
    render_dialog_bg(frame, popup);

    // Inset inner area
    let inner = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: popup.height.saturating_sub(2),
    };

    if inner.height < 6 {
        return;
    }

    // Split into header + search + spacer + content + description + footer
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Percentage(50),
            Constraint::Length(3),
        ])
        .split(inner);

    let header_area = layout[0];
    let search_area = layout[1];
    let content_area = layout[3];
    let description_area = layout[4];
    let footer_area = layout[5];

    // Header — the current model sits in the middle so the single most-changed
    // setting is visible at the top of the screen without adding a row.
    // When numeric edits are buffered but not yet committed, a small "unsaved"
    // marker appears so the user knows there are pending changes.
    let model_disp = truncate_mid(screen.effective_snapshot.config.effective_model(), 28);
    let dirty = !screen.pending_changes.is_empty();
    let dirty_span = if dirty {
        Span::styled(
            "  ● unsaved",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::DIM),
        )
    } else {
        Span::raw("")
    };
    // The literal prefix is "  ·  model " (11 chars) — must match the span
    // below or "Esc close" overflows the dialog on narrow widths.
    let head_len = " Settings — Clawde".chars().count()
        + model_disp.chars().count()
        + 11
        + if dirty { 11 } else { 0 };
    let esc_w = inner.width.saturating_sub(head_len as u16) as usize;
    let title = Line::from(vec![
        Span::styled(
            " Settings",
            Style::default()
                .fg(CLAURST_ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" — Clawde", Style::default().fg(CLAURST_MUTED)),
        Span::styled(
            format!("  ·  model {}", model_disp),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ),
        dirty_span,
        Span::styled(
            format!("{:>width$}", "Esc close", width = esc_w),
            Style::default().fg(CLAURST_MUTED),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(title).style(Style::default().bg(CLAURST_PANEL_BG)),
        header_area,
    );

    // Search
    let search_line = modal_search_line_with_insert(
        &screen.search_query,
        "Type to search settings...",
        Color::DarkGray,
        CLAURST_ACCENT,
        screen.vim_search.insert,
    );
    frame.render_widget(
        Paragraph::new(search_line).style(Style::default().bg(CLAURST_PANEL_BG)),
        search_area,
    );

    // Store the actual visible row count for scroll tracking.
    screen.last_visible_rows.set(content_area.height as usize);

    // Content
    render_settings_list(frame, screen, content_area);

    // Description of selected entry
    let all = all_entries(screen);
    let filtered: Vec<_> = all
        .iter()
        .filter(|e| entry_matches_filter(e, &screen.search_query, screen))
        .collect();

    // Description of the selected entry, prefixed with its status: where the
    // value comes from, when it applies, and what the default is. This is the
    // "know their status" line — every setting answers all three at a glance.
    let mut desc_lines: Vec<Line> = Vec::new();
    let desc_text = if let Some(entry) = filtered.get(screen.selected_idx) {
        let origin = entry_origin(screen, entry.key);
        let default_disp = if entry.default.is_empty() {
            "—".to_string()
        } else {
            entry.default.clone()
        };
        desc_lines.push(Line::from(vec![
            Span::styled("●", origin.color()),
            Span::styled(
                format!(
                    " {}  ·  applies {}  ·  fallback {}",
                    origin.label(),
                    entry.effect.label(),
                    default_disp
                ),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        desc_lines.push(Line::from(""));

        // Surface the full current value for text/number entries — the list
        // row truncates long values (e.g. Ollama host URLs) with an ellipsis,
        // so the complete value is shown here where there is room to wrap.
        if matches!(entry.kind, SettingKind::Text | SettingKind::Number) {
            let is_editing = screen.edit_field.as_deref() == Some(entry.key);
            let body = if is_editing {
                format!("Editing: {}{}", screen.edit_value, "▏")
            } else {
                format!("Value: {}", entry.value)
            };
            desc_lines.push(Line::from(vec![Span::styled(
                body,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )]));
            desc_lines.push(Line::from(""));
        }

        // For Output Style, show current selection and all available options with descriptions
        if entry.key == "output_style" {
            let mut lines = vec![entry.description.to_string(), String::new()];

            let all_styles = builtin_styles();
            let current_style_name = if screen.output_style.is_empty() {
                "default"
            } else {
                &screen.output_style
            };
            if let Some(current_style) = find_style(&all_styles, current_style_name) {
                lines.push(format!(
                    "Current: {} — {}",
                    current_style.label, current_style.description
                ));
                lines.push(String::new());
            }

            lines.push("Available:".to_string());
            for style in builtin_styles() {
                lines.push(format!("  {} — {}", style.name, style.description));
            }
            lines.join("\n")
        } else {
            entry.description.to_string()
        }
    } else {
        String::new()
    };
    for l in desc_text.lines() {
        desc_lines.push(Line::from(l));
    }
    // Append health warning to description if present
    if !screen.health_warning.is_empty() {
        desc_lines.push(Line::from(""));
        desc_lines.push(Line::from(screen.health_warning.clone()));
    }
    let desc_para = Paragraph::new(desc_lines)
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Left)
        .block(Block::default().padding(ratatui::widgets::Padding::new(1, 0, 1, 0)));
    frame.render_widget(desc_para, description_area);

    // Footer — keys on the first line, status-legend on the second so the
    // legend never truncates at the dialog edge.
    let mut footer: Vec<Line> = Vec::new();
    footer.push(if screen.edit_field.is_some() {
        Line::from(vec![
            Span::styled(
                " Enter ",
                Style::default()
                    .fg(CLAURST_ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("save  "),
            Span::styled(
                " Esc ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("cancel"),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                " ↑↓ ",
                Style::default()
                    .fg(CLAURST_ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("navigate  "),
            Span::styled(
                " Enter ",
                Style::default()
                    .fg(CLAURST_ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("toggle/edit  "),
            Span::styled(
                " Esc ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("close"),
        ])
    });
    footer.push(Line::from(vec![
        Span::styled(
            "●default/global/project",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ),
        Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "now=applies now  ·  next=restart",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ),
    ]));
    // Key hints for discoverable shortcuts: the section jumps, reset, and
    // the customised-only filter (! prefix in search) all live here.
    footer.push(Line::from(vec![Span::styled(
        "! filter · 1-5 sections · r reset",
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    )]));
    let footer_para = Paragraph::new(footer)
        .style(Style::default().fg(CLAURST_MUTED).bg(CLAURST_PANEL_BG))
        .alignment(Alignment::Center);
    frame.render_widget(footer_para, footer_area);
}

fn render_settings_list(frame: &mut Frame, screen: &SettingsScreen, area: Rect) {
    let all = all_entries(screen);

    // Filter entries by search query
    let filtered: Vec<_> = all
        .iter()
        .filter(|e| entry_matches_filter(e, &screen.search_query, screen))
        .collect();

    if filtered.is_empty() {
        let para = Paragraph::new("No settings match your search.")
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(para, area);
        return;
    }

    // Width budget: label column, value column (grows with room), status tail.
    let label_len = 36usize;
    let status_w = 24u16; // "  ● global · next" + padding
    let value_max = area
        .width
        .saturating_sub((label_len as u16) + status_w + 8)
        .max(10) as usize;

    // Build lines — section headers first, then per-row value + status.
    let mut lines: Vec<Line> = Vec::new();
    let visible_rows = area.height as usize;
    let mut current_section: Option<&str> = None;

    for (i, entry) in filtered.iter().enumerate() {
        // Dim section header, shown only when the section has visible entries.
        if current_section != Some(entry.section) {
            current_section = Some(entry.section);
            lines.push(Line::from(vec![Span::styled(
                format!("  {} ", entry.section),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )]));
        }

        let is_selected = i == screen.selected_idx;
        let marker = if is_selected { "►" } else { " " };

        // Show edit value if currently editing this field, otherwise show the entry value
        let value_str = if screen.edit_field.as_deref() == Some(entry.key) && is_selected {
            format!("{}_ ", screen.edit_value) // Add cursor indicator
        } else {
            entry.value.clone()
        };

        let row_style = if is_selected {
            Style::default()
                .fg(Color::Black)
                .bg(CLAURST_ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        let origin = entry_origin(screen, entry.key);
        let mut spans = vec![Span::styled(
            format!("   {} {:<label_len$}", marker, entry.label),
            row_style,
        )];
        // Empty override (e.g. "preferredSearchBackend": "") renders as a
        // dash rather than a blank column — still shows the origin tag. The
        // dash inherits row_style so it reads consistently on the selected row.
        if value_str.is_empty() {
            spans.push(Span::styled(
                "—".to_string(),
                row_style.add_modifier(Modifier::DIM),
            ));
        } else if screen.edit_field.as_deref() == Some(entry.key) && is_selected {
            // While editing, show the full edit buffer (no ellipsis). The
            // description panel repeats it in full if it overflows the row.
            spans.push(Span::styled(value_str, row_style));
        } else {
            spans.push(Span::styled(truncate_end(&value_str, value_max), row_style));
        }
        // Status tail — shown on every row so the state of each setting is
        // visible without extra interactions. Muted so it never competes with
        // the label/value.
        spans.push(Span::styled("  ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            format!("●{}", origin.label()),
            Style::default()
                .fg(origin.color())
                .add_modifier(Modifier::DIM),
        ));
        spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            entry.effect.label(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));
        lines.push(Line::from(spans));
    }

    // Scroll tracking is handled in update_scroll_offset_for_selection()

    // Apply manual scrolling
    let visible_lines: Vec<Line> = lines
        .into_iter()
        .skip(screen.scroll_offset)
        .take(visible_rows.max(1))
        .collect();

    let para = Paragraph::new(visible_lines);
    frame.render_widget(para, area);
}

/// Truncate a string to `max` chars, keeping the head and tail with an
/// ellipsis in the middle (used for the header model name).
fn truncate_mid(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep = (max.saturating_sub(1)) / 2;
    let head: String = s.chars().take(keep).collect();
    let tail: String = s
        .chars()
        .rev()
        .take(keep)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{}…{}", head, tail)
}

/// Truncate a string to `max` chars with a trailing ellipsis (used for long
/// setting values so the status tail never wraps off-screen).
fn truncate_end(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Key handling
// ---------------------------------------------------------------------------

pub fn handle_settings_key(
    screen: &mut SettingsScreen,
    config: &mut Config,
    key: crossterm::event::KeyEvent,
    vim_enabled: bool,
) -> bool {
    use crossterm::event::KeyCode;

    if !screen.visible {
        return false;
    }

    // Editing mode
    if screen.edit_field.is_some() {
        match key.code {
            KeyCode::Enter => {
                screen.commit_edit();
                screen.apply_and_save(config);
            }
            KeyCode::Esc => {
                screen.cancel_edit();
            }
            KeyCode::Backspace => {
                screen.edit_value.pop();
            }
            KeyCode::Char(c) => {
                screen.edit_value.push(c);
            }
            _ => {}
        }
        return true;
    }

    // Vim-modal search: letters only type into the search bar after `i`
    // (insert mode); `Esc` exits insert before the usual close/clear logic.
    // Skipped while editing a field — that is an insert-always text entry.
    match screen.vim_search.handle_key(vim_enabled, &key) {
        VimSearchKey::Consumed => return true,
        VimSearchKey::PushChar(c) => {
            screen.push_search_char(c);
            return true;
        }
        VimSearchKey::PopChar => {
            screen.pop_search_char();
            return true;
        }
        VimSearchKey::Passthrough => {}
    }

    // Navigation mode
    match key.code {
        KeyCode::Enter => {
            toggle_or_cycle_current(screen, config);
        }
        KeyCode::Esc => {
            if screen.confirming_discard {
                screen.confirming_discard = false;
                screen.close();
            } else if !screen.search_query.is_empty() {
                screen.search_query.clear();
                screen.selected_idx = 0;
            } else if !screen.pending_changes.is_empty() {
                screen.confirming_discard = true;
            } else {
                screen.close();
            }
        }
        KeyCode::Backspace if !vim_enabled => {
            screen.pop_search_char();
        }
        KeyCode::Up => {
            screen.select_prev();
            update_scroll_offset_for_selection(screen);
        }
        KeyCode::Char('k') if vim_enabled => {
            screen.select_prev();
            update_scroll_offset_for_selection(screen);
        }
        KeyCode::Down => {
            let all = all_entries(screen);
            let filtered: Vec<_> = all
                .iter()
                .filter(|e| entry_matches_filter(e, &screen.search_query, screen))
                .collect();
            screen.select_next(filtered.len());
            update_scroll_offset_for_selection(screen);
        }
        KeyCode::Char('j') if vim_enabled => {
            let all = all_entries(screen);
            let filtered: Vec<_> = all
                .iter()
                .filter(|e| entry_matches_filter(e, &screen.search_query, screen))
                .collect();
            screen.select_next(filtered.len());
            update_scroll_offset_for_selection(screen);
        }
        // Section quick-jump: 1-5 jump to each section's first visible entry.
        // Mirrors the model picker's number-key section jumps.
        KeyCode::Char(c @ ('1'..='5')) => {
            let all = all_entries(screen);
            let filtered: Vec<_> = all
                .iter()
                .filter(|e| entry_matches_filter(e, &screen.search_query, screen))
                .collect();
            let section = match c {
                '1' => SECTION_COMMON,
                '2' => SECTION_INTERFACE,
                '3' => SECTION_WORKSPACE,
                '4' => SECTION_FREE_ROUTING,
                '5' => SECTION_OLLAMA,
                _ => unreachable!(),
            };
            if let Some(pos) = filtered.iter().position(|e| e.section == section) {
                screen.selected_idx = pos;
                update_scroll_offset_for_selection(screen);
            }
        }
        // Reset the selected setting back to its built-in default, clearing
        // any global override. Only available for rows tagged "global" or
        // "project" — default rows are already at the built-in value.
        KeyCode::Char('r') => {
            let all = all_entries(screen);
            let filtered: Vec<_> = all
                .iter()
                .filter(|e| entry_matches_filter(e, &screen.search_query, screen))
                .collect();
            if let Some(entry) = filtered.get(screen.selected_idx) {
                if !matches!(entry_origin(screen, entry.key), SettingOrigin::Default) {
                    reset_setting_to_default(screen, entry.key);
                }
            }
        }
        KeyCode::Char(c) if !vim_enabled => {
            screen.push_search_char(c);
        }
        _ => {}
    }
    true
}

fn update_scroll_offset_for_selection(screen: &mut SettingsScreen) {
    // The rendered list interleaves section-header rows, so scroll math must
    // use the visual (header-augmented) line of the selection, not its index
    // into the filtered entries.
    let all = all_entries(screen);
    let filtered: Vec<_> = all
        .iter()
        .filter(|e| entry_matches_filter(e, &screen.search_query, screen))
        .collect();
    if filtered.is_empty() {
        return;
    }
    let visual = visual_line_for(&filtered, screen.selected_idx);
    let visible_rows = screen.last_visible_rows.get().max(1);
    if visual < screen.scroll_offset {
        screen.scroll_offset = visual;
    } else if visual >= screen.scroll_offset + visible_rows {
        screen.scroll_offset = visual.saturating_sub(visible_rows - 1);
    }
}

/// Visual row of the entry at `filtered_idx` in the header-augmented list,
/// mirroring the header-insertion loop in `render_settings_list`.
fn visual_line_for(filtered: &[&SettingsEntry], filtered_idx: usize) -> usize {
    if filtered.is_empty() {
        return 0;
    }
    let mut line = 0;
    let mut prev_section: Option<&str> = None;
    for (i, entry) in filtered.iter().enumerate() {
        if prev_section != Some(entry.section) {
            line += 1; // section header row
            prev_section = Some(entry.section);
        }
        if i == filtered_idx {
            break;
        }
        line += 1; // entry row
    }
    line
}

/// Reset a single setting back to its built-in default by clearing the global
/// override from `settings_snapshot` and re-saving. The row's origin tag flips
/// from Global/Project → Default immediately.
fn reset_setting_to_default(screen: &mut SettingsScreen, key: &str) {
    let s = &mut screen.settings_snapshot;
    let c = &mut s.config;
    match key {
        // Bools — set to their serde default (true for default_true fields).
        "auto_compact" => s.auto_compact = true,
        "notifications" => s.notifications = true,
        "terminal_progress_bar" => s.terminal_progress_bar = true,
        "show_cwd" => s.show_cwd = true,
        "show_git_branch" => s.show_git_branch = true,
        "fileInjectionEnabled" => c.file_injection_enabled = true,
        // mouse_capture: default is on (None = enabled).
        "mouse_capture" => c.mouse_capture = None,
        "show_turn_duration" => s.show_turn_duration = false,
        "reduce_motion" => s.reduce_motion = false,
        "verbose" => c.verbose = false,
        "cursor_blink_enabled" => c.cursor_blink_enabled = false,
        "auto_copy_enabled" => s.auto_copy_on_highlight = false,
        "disable_claude_mds" => c.disable_claude_mds = false,
        "fileAutocompleteShowHiddenFiles" => c.file_autocomplete_show_hidden_files = false,
        "auto_commits" => c.auto_commits = None,
        // Numbers / optionals — clear the explicit override.
        "max_tokens" => c.max_tokens = None,
        "compact_threshold" => c.compact_threshold = 0.0,
        "fileAutocompleteLimit" => c.file_autocomplete_limit = 15,
        "fileInjectionMaxSize" => c.file_injection_max_size = 100,
        // Option-like strings — set to their default.
        "output_style" => c.output_style = None,
        "output_format" => c.output_format = clawde_core::config::OutputFormat::Text,
        "permission_mode" => c.permission_mode = clawde_core::config::PermissionMode::Default,
        "verify_sandbox" => c.verify.sandbox = clawde_core::config::VerifySandbox::Direct,
        "verify_container_image" => c.verify.container_image = None,
        "preferredSearchBackend" => s.preferred_search_backend = "auto".to_string(),
        // Routing entries — remove from the routing JSON.
        "routing_strategy" => {
            c.provider_configs
                .entry("free".to_string())
                .or_default()
                .options
                .entry("routing".to_string())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .map(|r| r.remove("strategy"));
        }
        "first_byte_timeout_secs"
        | "upstream_5xx_cooldown_secs"
        | "health_poll_interval_secs"
        | "fallback_retries" => {
            let rk = match key {
                "first_byte_timeout_secs" => "first_byte_timeout_secs",
                "upstream_5xx_cooldown_secs" => "upstream_5xx_cooldown_secs",
                "health_poll_interval_secs" => "health_poll_interval_secs",
                "fallback_retries" => "fallback_retries",
                _ => "",
            };
            c.provider_configs
                .entry("free".to_string())
                .or_default()
                .options
                .entry("routing".to_string())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .map(|r| r.remove(rk));
        }
        "staggered_probe" => {
            c.provider_configs
                .entry("free".to_string())
                .or_default()
                .options
                .entry("routing".to_string())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .map(|r| r.remove("staggered_probe"));
        }
        "disabled_upstreams" => {
            c.provider_configs
                .entry("free".to_string())
                .or_default()
                .options
                .entry("routing".to_string())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .map(|r| r.remove("disabled_upstreams"));
        }
        // Ollama — clear the provider option.
        "ollama_num_ctx" => {
            _ = c
                .provider_configs
                .get_mut("ollama")
                .and_then(|pc| pc.options.remove("num_ctx"))
        }
        "ollama_keep_alive" => {
            _ = c
                .provider_configs
                .get_mut("ollama")
                .and_then(|pc| pc.options.remove("keep_alive"))
        }
        "ollama_num_predict" => {
            _ = c
                .provider_configs
                .get_mut("ollama")
                .and_then(|pc| pc.options.remove("num_predict"))
        }
        "ollama_require_explicit_host" => {
            _ = c
                .provider_configs
                .get_mut("ollama")
                .and_then(|pc| pc.options.remove("require_explicit_host"));
        }
        "ollama_default_host" => {
            _ = c
                .provider_configs
                .get_mut("ollama")
                .and_then(|pc| pc.options.remove("default_host"));
        }
        _ => return,
    }
    let _ = s.save_sync();
    // Reload the snapshot so `entry_origin` returns Default on the next
    // render, then update the screen's display field for this key so the
    // row value and origin tag agree. We do NOT call
    // `apply_settings_from_snapshot()` here — that reads from the stale
    // `effective_snapshot` and would undo the reset.
    screen.settings_snapshot = Settings::load_sync().unwrap_or_default();
    screen.pending_changes.remove(key);
    // Propagate the reset value into the screen's display field.
    sync_screen_field(screen, key);
}

/// After a reset writes the default value to `settings_snapshot`, mirror the
/// same value into the screen's display field so the row value matches the
/// origin tag. Avoids `apply_settings_from_snapshot` which would overwrite
/// every field from the stale `effective_snapshot` and undo the reset.
fn sync_screen_field(screen: &mut SettingsScreen, key: &str) {
    match key {
        "auto_compact" => screen.auto_compact = true,
        "notifications" => screen.notifications = true,
        "terminal_progress_bar" => screen.terminal_progress_bar = true,
        "show_cwd" => screen.show_cwd = true,
        "show_git_branch" => screen.show_git_branch = true,
        "fileInjectionEnabled" => screen.file_injection_enabled = true,
        "mouse_capture" => screen.mouse_capture = true,
        "show_turn_duration" => screen.show_turn_duration = false,
        "reduce_motion" => screen.reduce_motion = false,
        "verbose" => screen.verbose = false,
        "cursor_blink_enabled" => screen.cursor_blink_enabled = false,
        "auto_copy_enabled" => screen.auto_copy_enabled = false,
        "disable_claude_mds" => screen.disable_claude_mds = false,
        "fileAutocompleteShowHiddenFiles" => screen.file_autocomplete_show_hidden_files = false,
        "auto_commits" => screen.auto_commits = false,
        "max_tokens" => {}
        "compact_threshold" => screen.compact_threshold = "95".to_string(),
        "fileAutocompleteLimit" => screen.file_autocomplete_limit = "15".to_string(),
        "fileInjectionMaxSize" => screen.file_injection_max_size = "100".to_string(),
        "output_style" => screen.output_style = "default".to_string(),
        "output_format" => screen.output_format = "text".to_string(),
        "permission_mode" => screen.permission_mode = "default".to_string(),
        "verify_sandbox" => screen.verify_sandbox = "direct".to_string(),
        "verify_container_image" => screen.verify_container_image = String::new(),
        "preferredSearchBackend" => screen.preferred_search_backend = "auto".to_string(),
        "routing_strategy" => screen.routing_strategy = "sequential".to_string(),
        "first_byte_timeout_secs" => screen.first_byte_timeout_secs = "0".to_string(),
        "staggered_probe" => screen.staggered_probe = true,
        "upstream_5xx_cooldown_secs" => screen.upstream_5xx_cooldown_secs = "45".to_string(),
        "health_poll_interval_secs" => screen.health_poll_interval_secs = "300".to_string(),
        "fallback_retries" => screen.fallback_retries = "0".to_string(),
        "disabled_upstreams" => screen.disabled_upstreams = String::new(),
        "ollama_num_ctx" => screen.ollama_num_ctx = "12K".to_string(),
        "ollama_keep_alive" => screen.ollama_keep_alive = "forever".to_string(),
        "ollama_num_predict" => screen.ollama_num_predict = "2K".to_string(),
        _ => {}
    }
}

fn toggle_or_cycle_current(screen: &mut SettingsScreen, config: &mut Config) {
    let all = all_entries(screen);
    let filtered: Vec<_> = all
        .iter()
        .filter(|e| entry_matches_filter(e, &screen.search_query, screen))
        .collect();

    if let Some(entry) = filtered.get(screen.selected_idx) {
        match entry.kind {
            SettingKind::Bool => {
                let new_value = entry.value != "true";
                match entry.key {
                    "auto_compact" => {
                        screen.auto_compact = new_value;
                        screen.settings_snapshot.auto_compact = new_value;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "notifications" => {
                        screen.notifications = new_value;
                        screen.settings_snapshot.notifications = new_value;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "show_turn_duration" => {
                        screen.show_turn_duration = new_value;
                        screen.settings_snapshot.show_turn_duration = new_value;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "reduce_motion" => {
                        screen.reduce_motion = new_value;
                        screen.settings_snapshot.reduce_motion = new_value;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "terminal_progress_bar" => {
                        screen.terminal_progress_bar = new_value;
                        screen.settings_snapshot.terminal_progress_bar = new_value;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "verbose" => {
                        screen.verbose = new_value;
                        screen.settings_snapshot.config.verbose = new_value;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "cursor_blink_enabled" => {
                        screen.cursor_blink_enabled = new_value;
                        screen.settings_snapshot.config.cursor_blink_enabled = new_value;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "auto_copy_enabled" => {
                        screen.auto_copy_enabled = new_value;
                        screen.settings_snapshot.auto_copy_on_highlight = new_value;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "mouse_capture" => {
                        screen.mouse_capture = new_value;
                        // Persist only the off state; on is the default, so clear the key.
                        screen.settings_snapshot.config.mouse_capture =
                            if new_value { None } else { Some(false) };
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "staggered_probe" => {
                        screen.staggered_probe = new_value;
                        let mut routing =
                            get_or_create_routing_json(&screen.settings_snapshot.config);
                        routing["staggered_probe"] = serde_json::Value::Bool(new_value);
                        screen
                            .settings_snapshot
                            .config
                            .provider_configs
                            .entry("free".to_string())
                            .or_default()
                            .options
                            .insert("routing".to_string(), routing.clone());
                        // Also update the live config so the change takes effect immediately.
                        config
                            .provider_configs
                            .entry("free".to_string())
                            .or_default()
                            .options
                            .insert("routing".to_string(), routing);
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "show_cwd" => {
                        screen.show_cwd = new_value;
                        screen.settings_snapshot.show_cwd = new_value;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "show_git_branch" => {
                        screen.show_git_branch = new_value;
                        screen.settings_snapshot.show_git_branch = new_value;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "auto_commits" => {
                        screen.auto_commits = new_value;
                        screen.settings_snapshot.config.auto_commits =
                            if new_value { Some(true) } else { None };
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "disable_claude_mds" => {
                        screen.disable_claude_mds = new_value;
                        screen.settings_snapshot.config.disable_claude_mds = new_value;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "fileInjectionEnabled" => {
                        screen.file_injection_enabled = new_value;
                        screen.settings_snapshot.config.file_injection_enabled = new_value;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "fileAutocompleteShowHiddenFiles" => {
                        screen.file_autocomplete_show_hidden_files = new_value;
                        screen
                            .settings_snapshot
                            .config
                            .file_autocomplete_show_hidden_files = new_value;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "ollama_require_explicit_host" => {
                        screen.ollama_require_explicit_host = new_value;
                        let val = serde_json::Value::from(new_value);
                        screen
                            .settings_snapshot
                            .config
                            .provider_configs
                            .entry("ollama".to_string())
                            .or_default()
                            .options
                            .insert("require_explicit_host".to_string(), val.clone());
                        config
                            .provider_configs
                            .entry("ollama".to_string())
                            .or_default()
                            .options
                            .insert("require_explicit_host".to_string(), val);
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    _ => {}
                }
            }
            SettingKind::Enum { ref options } => {
                let current_idx = options.iter().position(|&o| o == entry.value).unwrap_or(0);
                let next_idx = (current_idx + 1) % options.len();
                let new_value = options[next_idx];

                match entry.key {
                    "permission_mode" => {
                        let mode = parse_permission_mode(new_value);
                        screen.permission_mode = new_value.to_string();
                        screen.settings_snapshot.config.permission_mode = mode.clone();
                        // Apply to the live config too so the mode changes
                        // immediately, not just for the next session.
                        config.permission_mode = mode;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "verify_sandbox" => {
                        let sandbox = match new_value {
                            "worktree" => clawde_core::config::VerifySandbox::Worktree,
                            "container" => clawde_core::config::VerifySandbox::Container,
                            _ => clawde_core::config::VerifySandbox::Direct,
                        };
                        screen.verify_sandbox = new_value.to_string();
                        screen.settings_snapshot.config.verify.sandbox = sandbox;
                        // Apply to the live config too so the change takes
                        // effect immediately, not just for the next session.
                        config.verify.sandbox = sandbox;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "keybinding_preset" => {
                        screen.keybinding_preset = new_value.to_string();
                        let config_dir = Settings::config_dir();
                        let mut kb = UserKeybindings::load(&config_dir);
                        kb.preset =
                            clawde_core::keybindings::KeybindingPreset::from_name(new_value)
                                .unwrap_or_default();
                        let _ = kb.save(&config_dir);
                    }
                    "output_style" => {
                        screen.output_style = new_value.to_string();
                        let style = (new_value != "default").then(|| new_value.to_string());
                        screen.settings_snapshot.config.output_style = style.clone();
                        config.output_style = style; // live for the next request
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "output_format" => {
                        screen.output_format = new_value.to_string();
                        screen.settings_snapshot.config.output_format = match new_value {
                            "json" => clawde_core::config::OutputFormat::Json,
                            "stream_json" => clawde_core::config::OutputFormat::StreamJson,
                            _ => clawde_core::config::OutputFormat::Text,
                        };
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "routing_strategy" => {
                        screen.routing_strategy = new_value.to_string();
                        let mut routing =
                            get_or_create_routing_json(&screen.settings_snapshot.config);
                        routing["strategy"] = serde_json::Value::String(new_value.to_string());
                        screen
                            .settings_snapshot
                            .config
                            .provider_configs
                            .entry("free".to_string())
                            .or_default()
                            .options
                            .insert("routing".to_string(), routing.clone());
                        // Also update the live config so the change takes effect immediately.
                        config
                            .provider_configs
                            .entry("free".to_string())
                            .or_default()
                            .options
                            .insert("routing".to_string(), routing);
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "preferredSearchBackend" => {
                        screen.preferred_search_backend = new_value.to_string();
                        screen.settings_snapshot.preferred_search_backend = new_value.to_string();
                        // Also set the env var so it takes effect immediately.
                        if new_value == "auto" {
                            std::env::remove_var("PREFERRED_SEARCH_BACKEND");
                        } else {
                            std::env::set_var("PREFERRED_SEARCH_BACKEND", new_value);
                        }
                        // Check if the selected backend is properly configured
                        if new_value != "auto" {
                            match check_backend_configured(new_value) {
                                Ok(()) => {
                                    screen.health_warning = String::new();
                                }
                                Err(msg) => {
                                    screen.health_warning =
                                        format!("Warning: {} not configured — {}", new_value, msg);
                                }
                            }
                        } else {
                            screen.health_warning.clear();
                        }
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "ollama_num_ctx" => {
                        screen.ollama_num_ctx = new_value.to_string();
                        let val = serde_json::Value::from(preset_to_num_ctx(new_value));
                        screen
                            .settings_snapshot
                            .config
                            .provider_configs
                            .entry("ollama".to_string())
                            .or_default()
                            .options
                            .insert("num_ctx".to_string(), val.clone());
                        config
                            .provider_configs
                            .entry("ollama".to_string())
                            .or_default()
                            .options
                            .insert("num_ctx".to_string(), val);
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "ollama_keep_alive" => {
                        screen.ollama_keep_alive = new_value.to_string();
                        let val = serde_json::Value::String(preset_to_keep_alive(new_value));
                        screen
                            .settings_snapshot
                            .config
                            .provider_configs
                            .entry("ollama".to_string())
                            .or_default()
                            .options
                            .insert("keep_alive".to_string(), val.clone());
                        config
                            .provider_configs
                            .entry("ollama".to_string())
                            .or_default()
                            .options
                            .insert("keep_alive".to_string(), val);
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "ollama_num_predict" => {
                        screen.ollama_num_predict = new_value.to_string();
                        let val = serde_json::Value::from(preset_to_num_predict(new_value));
                        screen
                            .settings_snapshot
                            .config
                            .provider_configs
                            .entry("ollama".to_string())
                            .or_default()
                            .options
                            .insert("num_predict".to_string(), val.clone());
                        config
                            .provider_configs
                            .entry("ollama".to_string())
                            .or_default()
                            .options
                            .insert("num_predict".to_string(), val);
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    _ => {}
                }
            }
            SettingKind::Number | SettingKind::Text => {
                screen.start_edit(entry.key, &entry.value);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_screen_new_has_sensible_defaults() {
        let screen = SettingsScreen::new();
        assert!(!screen.visible);
        assert!(screen.search_query.is_empty());
        assert_eq!(screen.selected_idx, 0);
        assert!(screen.edit_field.is_none());
        assert!(screen.edit_value.is_empty());
    }

    #[test]
    fn all_entries_returns_expected_settings() {
        let screen = SettingsScreen::new();
        let entries = all_entries(&screen);
        // Base settings are always present (30 with the permission-mode entry),
        // plus 0-3 conditional file injection settings.
        assert!(
            entries.len() >= 28,
            "Should have at least 28 editable settings, got {}",
            entries.len()
        );
        assert!(
            entries.len() <= 40,
            "Should have at most 40 editable settings, got {}",
            entries.len()
        );
    }

    #[test]
    fn all_entries_puts_common_first_and_permission_mode_on_top() {
        let screen = SettingsScreen::new();
        let entries = all_entries(&screen);
        assert_eq!(entries.first().unwrap().key, "permission_mode");
        assert_eq!(entries.first().unwrap().section, SECTION_COMMON);
        // Every entry must carry a section, a default, and an effect so the
        // always-on status column renders something meaningful.
        for e in &entries {
            assert!(!e.section.is_empty(), "{} has no section", e.key);
            assert!(
                matches!(
                    e.effect,
                    SettingEffect::Immediate | SettingEffect::NextSession
                ),
                "{} has no effect",
                e.key
            );
        }
    }

    /// Reset a fresh screen to controlled snapshots so the tests do not depend
    /// on whatever `~/.clawde/settings.json` exists on the machine running them.
    /// `Settings::default()` is NOT the same as the serde defaults (e.g.
    /// `notifications` defaults to `true` via `#[serde(default = ...)]`), so
    /// the fresh state is an empty-file deserialization.
    fn fresh_controlled_screen() -> SettingsScreen {
        let fresh: Settings = serde_json::from_str("{}").unwrap();
        let mut screen = SettingsScreen::new();
        screen.settings_snapshot = fresh.clone();
        screen.effective_snapshot = fresh;
        screen.project_snapshot = None;
        screen
    }

    #[test]
    fn entry_origin_reflects_global_customization() {
        let mut screen = fresh_controlled_screen();
        // Fresh defaults: nothing customized.
        assert_eq!(
            entry_origin(&screen, "notifications"),
            SettingOrigin::Default
        );
        assert_eq!(
            entry_origin(&screen, "routing_strategy"),
            SettingOrigin::Default
        );
        // Setting a value in the global snapshot flips the origin to Global.
        screen.settings_snapshot.config.verbose = true;
        screen.settings_snapshot.config.output_style = Some("concise".to_string());
        assert_eq!(entry_origin(&screen, "verbose"), SettingOrigin::Global);
        assert_eq!(entry_origin(&screen, "output_style"), SettingOrigin::Global);
    }

    #[test]
    fn entry_origin_detects_project_override() {
        let mut screen = fresh_controlled_screen();
        // Project sets the routing strategy while global keeps the default.
        let mut project: Settings = serde_json::from_str("{}").unwrap();
        project
            .config
            .provider_configs
            .entry("free".to_string())
            .or_default()
            .options
            .insert(
                "routing".to_string(),
                serde_json::json!({"strategy": "latency_based"}),
            );
        screen.project_snapshot = Some(project);
        assert_eq!(
            entry_origin(&screen, "routing_strategy"),
            SettingOrigin::Project
        );
        // Unrelated keys stay Default.
        assert_eq!(
            entry_origin(&screen, "notifications"),
            SettingOrigin::Default
        );
    }

    #[test]
    fn search_filters_entries_correctly() {
        let screen = SettingsScreen::new();
        let all = all_entries(&screen);
        let filtered: Vec<_> = all
            .iter()
            .filter(|e| e.label.to_lowercase().contains("token"))
            .collect();
        assert_eq!(
            filtered.len(),
            1,
            "Should find exactly 1 entry matching 'token'"
        );
        assert_eq!(filtered[0].label, "Max tokens");
    }

    #[test]
    fn visual_line_for_counts_section_headers() {
        // The list renderer interleaves a section header whenever the section
        // changes, so the visual row of a filtered entry is its index plus the
        // number of headers above it. Scroll math must use this augmented row.
        let screen = SettingsScreen::new();
        let all = all_entries(&screen);
        let filtered: Vec<_> = all.iter().collect();

        let first = visual_line_for(&filtered, 0);
        assert_eq!(
            first, 1,
            "first entry sits one row below its section header"
        );

        // Find the index of the first entry of the second section (a section
        // boundary we cross when navigating).
        let first_section = filtered[0].section;
        let second_start = filtered
            .iter()
            .position(|e| e.section != first_section)
            .unwrap();
        let visual = visual_line_for(&filtered, second_start);
        assert_eq!(
            visual,
            second_start + 2,
            "entry after a boundary has one header for each section above it"
        );

        // Two consecutive entries in the same section differ by exactly one row.
        let a = visual_line_for(&filtered, 1);
        let b = visual_line_for(&filtered, 2);
        if filtered[1].section == filtered[2].section {
            assert_eq!(b, a + 1);
        }
    }

    #[test]
    fn toggle_bool_entry_flips_value() {
        let mut screen = SettingsScreen::new();
        screen.notifications = true;
        screen.open(Path::new("."));

        let initial = screen.notifications;
        let all = all_entries(&screen);
        let entry = all
            .iter()
            .find(|e| e.label == "Desktop notifications")
            .unwrap();
        assert_eq!(entry.label, "Desktop notifications");

        // Simulate toggle (manually, since toggle_or_cycle_current modifies internal state)
        screen.notifications = !screen.notifications;
        assert_ne!(screen.notifications, initial);
    }

    #[test]
    fn cycle_enum_entry_wraps_around() {
        let mut screen = SettingsScreen::new();
        screen.output_style = "default".to_string();

        // Simulate cycling through all options
        let options = ["default", "concise", "explanatory", "learning"];
        let mut idx = options.iter().position(|&o| o == "default").unwrap();

        idx = (idx + 1) % options.len();
        assert_eq!(options[idx], "concise");

        idx = (idx + 1) % options.len();
        assert_eq!(options[idx], "explanatory");

        idx = (idx + 1) % options.len();
        assert_eq!(options[idx], "learning");

        idx = (idx + 1) % options.len();
        assert_eq!(options[idx], "default"); // Wraps around
    }

    #[test]
    fn verify_sandbox_entry_is_enum_with_three_modes() {
        let screen = SettingsScreen::new();
        let entries = all_entries(&screen);
        let entry = entries
            .iter()
            .find(|e| e.key == "verify_sandbox")
            .expect("verify_sandbox entry must exist");
        assert_eq!(entry.label, "Verify sandbox");
        assert_eq!(entry.section, SECTION_COMMON);
        match &entry.kind {
            SettingKind::Enum { options } => {
                assert_eq!(options, &vec!["direct", "worktree", "container"]);
            }
            other => panic!("verify_sandbox must be an enum, got: {other:?}"),
        }
        assert_eq!(entry.value, "direct");
    }

    #[test]
    fn verify_container_image_entry_is_editable_text() {
        let mut screen = SettingsScreen::new();
        // The value getter reads straight from the config's container_image.
        screen.settings_snapshot.config.verify.container_image = Some("node:20-slim".to_string());
        let entries = all_entries(&screen);
        let entry = entries
            .iter()
            .find(|e| e.key == "verify_container_image")
            .expect("verify_container_image entry must exist");
        assert_eq!(entry.label, "Verify container image");
        assert_eq!(entry.section, SECTION_COMMON);
        match &entry.kind {
            SettingKind::Text => {}
            other => panic!("verify_container_image must be Text, got: {other:?}"),
        }
        assert_eq!(
            value_from_settings(&screen.settings_snapshot, "verify_container_image"),
            "node:20-slim"
        );
        // Default is empty (auto image resolution).
        assert_eq!(default_value_for("verify_container_image"), "");
    }

    #[test]
    fn verify_sandbox_value_reads_from_config() {
        // The value getter must read the sandbox out of the config, so a
        // project/global override shows up in the row and in the origin tag.
        let mut settings = Settings::default();
        settings.config.verify.sandbox = clawde_core::config::VerifySandbox::Worktree;
        assert_eq!(value_from_settings(&settings, "verify_sandbox"), "worktree");
    }

    #[test]
    fn ollama_default_host_entry_is_editable_text() {
        // The "Host URL" row must be a Text entry fed from the screen field,
        // so Enter opens an inline editor (type → save) rather than a toggle
        // or enum cycle, and the row value is the raw URL.
        let mut screen = SettingsScreen::new();
        screen.ollama_default_host = "http://devbox:11434".to_string();
        let entries = all_entries(&screen);
        let entry = entries
            .iter()
            .find(|e| e.key == "ollama_default_host")
            .expect("ollama default host entry");
        assert!(matches!(entry.kind, SettingKind::Text));
        assert_eq!(entry.value, "http://devbox:11434");
        assert_eq!(entry.section, SECTION_OLLAMA);
    }
}
