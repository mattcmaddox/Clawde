// free_mode_dialog.rs — Setup dialog for the composite "Free" provider.
//
// Walks the user through the multi-provider free-mode caveats and collects
// API keys from any subset of the supported upstreams. The chain stacks
// many free tiers (Groq, Cerebras, Google, Mistral, SambaNova, NVIDIA,
// Cohere, OpenRouter, OpenCode Zen, Z.AI, Zhipu) behind one synthetic
// `free/auto` model — the more keys the user pastes in, the more
// providers the router can fall back to. Minimum 1 key to enable; more
// is better.
//
// Layout:
//   ┌─ Connect Free (multi-provider — 3 keys) ────────────── esc ┐
//   │  Stack the free tiers from many providers behind           │
//   │  one endpoint. ⚠ context management is worse than          │
//   │  paid models; long sessions truncate aggressively.         │
//   │  TIP More keys = better availability and higher caps.      │
//   │                                                            │
//   │  ▸ Groq  (●) ●      console.groq.com/keys                 │
//   │     ••••••••AbCd_                                          │
//   │    Cerebras  ●        cloud.cerebras.ai                   │
//   │     paste new API key here...                             │
//   │    Google Gemini  ● ●  aistudio.google.com                │
//   │     enter to reveal key                                   │
//   │    …8 more — tab/↑↓ to scroll                             │
//   │                                                            │
//   │  ↑/↓ j/k provider  ←/→ h/l key  enter reveal/append  del  │
//   │  tab show all  ctrl+d on/off  ctrl+enter connect (3 keys) │
//   └────────────────────────────────────────────────────────────┘
//
// Stored keys are NEVER shown by default — each usable key is a health
// dot next to the provider name (● green = valid, ● red = invalid,
// dim = untested). Dots are selectable nodes (←/→ or h/l); Enter on a
// dot reveals that key inline; the blank line under each provider
// accepts a new key which Enter appends as an additional dot. Delete
// while a key is revealed asks for confirmation, showing that key's
// health dot. Ctrl+Enter commits everything and connects Free mode;
// Esc closes without applying changes.

use ratatui::layout::Rect;
use ratatui::prelude::Stylize;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use clawde_api::{FreeUpstream, FREE_CATALOG};

use crate::overlays::{centered_rect, render_dark_overlay, render_dialog_bg, CLAURST_PANEL_BG};
use crate::vim_search::VimSearch;
use std::cell::Cell;

/// One background key-validation ping result: `(field_idx, key_idx, result)`.
/// Named so the receiver type stays readable in `start_auto_pings`,
/// `start_validate`, and the `App` struct (keeps clippy::type_complexity off).
pub type ValidationPing = (usize, usize, Result<(), String>);

/// Horizontal cursor position within a provider row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodePos {
    /// The blank new-key input line under the provider name.
    NewKey,
    /// An existing key dot at this index (into `FreeModeField::keys`).
    Key(usize),
}

/// State for the "delete this key?" confirmation popup.
#[derive(Debug, Clone, Copy)]
pub struct DeleteConfirm {
    pub field_idx: usize,
    pub key_idx: usize,
}

/// One row in the dialog — one provider's name, URL, its stored keys
/// (rendered as health dots), and the blank line for new keys.
#[derive(Debug, Clone)]
pub struct FreeModeField {
    pub upstream: &'static FreeUpstream,
    /// Multi-key storage — each entry is one usable key (one dot).
    pub keys: Vec<String>,
    /// Parallel to `keys`: per-key validation status. `None` = not yet
    /// tested (dim dot), `Some(Ok(()))` = valid (green), `Some(Err(_))` =
    /// invalid (red).
    pub key_status: Vec<Option<Result<(), String>>>,
    /// New-key input buffer — the blank line that accepts new keys.
    pub pending: String,
    /// Two-step composite-key flow (cloudflare): when `Some`, the API token
    /// was captured on the first Enter and the row now awaits the account ID.
    /// The second Enter joins them into the stored `ACCOUNT_ID:API_TOKEN`.
    pub pending_token: Option<String>,
    /// Index of the key currently revealed inline (view-only). `None` =
    /// masked.
    pub revealed: Option<usize>,
    /// When `true`, this upstream is hidden behind the "show all" toggle.
    pub collapsed: bool,
    /// Whether this upstream is enabled in the free provider chain.
    /// Disabled upstreams are skipped by `store_updates()` even if they have keys.
    pub enabled: bool,
    /// When `true`, the keys came from environment variables and are
    /// read-only in this dialog (cannot be edited, appended to, or deleted).
    pub from_env: bool,
}

pub struct FreeModeDialogState {
    pub visible: bool,
    /// The area used by this dialog in the last render (for click-outside detection).
    pub last_rect: Cell<Rect>,
    pub fields: Vec<FreeModeField>,
    /// Active provider row.
    pub active_idx: usize,
    /// Active node within the active row (new-key line or a key dot).
    pub active_node: NodePos,
    /// When set, the delete-confirmation popup is open and captures input.
    pub delete_confirm: Option<DeleteConfirm>,
    /// First visible field index (for scrolling when fields > viewport).
    pub scroll_offset: usize,
    /// When `true`, all upstreams are shown (none collapsed).
    pub show_all: bool,
    /// When `true`, a key validation is in progress (prevents rapid Ctrl+V).
    pub is_validating: bool,
    /// Vim-modal insert state (only used when vim is enabled). The dialog is
    /// a key-entry form, so it opens in insert; `Esc` exits insert before
    /// the unreveal → clear → close cascade runs.
    pub vim_search: VimSearch,
}

impl Default for FreeModeDialogState {
    fn default() -> Self {
        Self::new()
    }
}

impl FreeModeDialogState {
    pub fn new() -> Self {
        let fields = FREE_CATALOG
            .iter()
            .map(|upstream| FreeModeField {
                upstream,
                keys: Vec::new(),
                key_status: Vec::new(),
                pending: String::new(),
                pending_token: None,
                revealed: None,
                collapsed: true,
                enabled: true,
                from_env: false,
            })
            .collect();
        Self {
            visible: false,
            fields,
            active_idx: 0,
            active_node: NodePos::NewKey,
            delete_confirm: None,
            scroll_offset: 0,
            show_all: false,
            is_validating: false,
            last_rect: Cell::new(Rect::default()),
            vim_search: VimSearch::new(),
        }
    }

    /// Mark upstreams whose keys came from environment variables.
    /// These are shown as read-only in the dialog — the user can see them
    /// but they must edit the env var in their shell profile to change.
    pub fn set_env_var_keys(&mut self, env_var_keys: &[(&str, String)]) {
        for (id, _key) in env_var_keys {
            if let Some(field) = self.fields.iter_mut().find(|f| f.upstream.id == *id) {
                field.from_env = true;
            }
        }
    }

    /// Open the dialog, pre-populating each row from `existing[upstream.id]`
    /// when present. Each string is one stored key (rendered as a dot).
    /// Fields with keys are expanded; empty fields are collapsed.
    /// Also reads `disabled_upstreams` from settings to set the enabled state.
    pub fn open(&mut self, existing: &[(&str, Vec<String>)]) {
        self.visible = true;
        self.show_all = false;
        self.active_node = NodePos::NewKey;
        self.delete_confirm = None;
        self.vim_search.enter_insert();
        // Reset every field; the dialog is re-seeded from the store each
        // time it opens so discarded edits never leak back in.
        for field in &mut self.fields {
            field.keys.clear();
            field.key_status.clear();
            field.pending.clear();
            field.pending_token = None;
            field.revealed = None;
        }
        for (id, keys) in existing {
            if let Some(field) = self.fields.iter_mut().find(|f| f.upstream.id == *id) {
                // Don't overwrite env-var keys with auth_store keys (env var wins).
                if !field.from_env {
                    field.keys = keys
                        .iter()
                        .filter(|k| !k.trim().is_empty())
                        .cloned()
                        .collect();
                    field.key_status = vec![None; field.keys.len()];
                }
            }
        }
        // Read disabled upstreams from settings so toggle state persists.
        let disabled_upstreams: Vec<String> = clawde_core::config::Settings::load_sync()
            .map(|s| s.effective_config())
            .unwrap_or_default()
            .provider_configs
            .get("free")
            .and_then(|pc| pc.options.get("routing"))
            .and_then(|v| v.get("disabled_upstreams"))
            .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
            .unwrap_or_default();

        // Collapse empty fields, expand fields with keys.
        for field in &mut self.fields {
            field.collapsed = field.keys.is_empty();
            field.enabled = !disabled_upstreams.contains(&field.upstream.id.to_string());
        }
        // Start on the first empty (non-collapsed visible) field, or the first
        // field if none are empty.
        let visible = self.visible_field_indices();
        self.active_idx = visible
            .iter()
            .find(|&&i| self.fields[i].keys.is_empty())
            .copied()
            .unwrap_or(*visible.first().unwrap_or(&0));
        self.scroll_offset = 0;
        self.ensure_active_visible();
    }

    /// Spawn background validation pings for every stored key on every
    /// enabled upstream. Returns a receiver the caller drains in the main
    /// loop. Each received `(field_idx, key_idx, result)` should be passed
    /// to `set_validation_result()`.
    pub fn start_auto_pings(&mut self) -> Option<std::sync::mpsc::Receiver<ValidationPing>> {
        let targets: Vec<(usize, usize, String, String)> = self
            .fields
            .iter()
            .enumerate()
            .flat_map(|(fi, f)| {
                if !f.enabled {
                    return Vec::new();
                }
                f.keys
                    .iter()
                    .enumerate()
                    .filter(|(_, k)| !k.trim().is_empty())
                    .map(|(ki, k)| (fi, ki, f.upstream.id.to_string(), k.trim().to_string()))
                    .collect()
            })
            .collect();

        if targets.is_empty() {
            return None;
        }

        let (tx, rx) = std::sync::mpsc::channel();
        for (fi, ki, upstream_id, key) in targets {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let result = clawde_api::providers::free::validate_upstream_key(&upstream_id, &key);
                let _ = tx.send((fi, ki, result));
            });
        }
        drop(tx);

        Some(rx)
    }

    pub fn close(&mut self) {
        self.visible = false;
        // Clear all transient + seeded state: Esc discards changes and the
        // next open() re-seeds from the auth store.
        self.active_idx = 0;
        self.active_node = NodePos::NewKey;
        self.scroll_offset = 0;
        self.show_all = false;
        self.is_validating = false;
        self.delete_confirm = None;
        self.vim_search.reset();
        for field in &mut self.fields {
            field.keys.clear();
            field.key_status.clear();
            field.pending.clear();
            field.pending_token = None;
            field.revealed = None;
            field.collapsed = false;
        }
    }

    /// Number of rows shown at once in the scrolling viewport.
    pub const VISIBLE_ROWS: usize = 4;

    /// Return indices of fields that are currently visible (non-collapsed or
    /// show_all is active).
    pub fn visible_field_indices(&self) -> Vec<usize> {
        self.fields
            .iter()
            .enumerate()
            .filter(|(_, f)| self.show_all || !f.collapsed)
            .map(|(i, _)| i)
            .collect()
    }

    /// Toggle whether the active upstream is enabled/disabled.
    /// Disabled upstreams are skipped by `store_updates()` even if they have keys.
    /// The disabled list is persisted to settings.json immediately, preserving
    /// any existing routing strategy.
    pub fn toggle_enabled(&mut self) {
        if let Some(field) = self.fields.get_mut(self.active_idx) {
            field.enabled = !field.enabled;
            // Persist the disabled upstreams to settings.json.
            let disabled: Vec<String> = self
                .fields
                .iter()
                .filter(|f| !f.enabled)
                .map(|f| f.upstream.id.to_string())
                .collect();
            if let Ok(mut settings) = clawde_core::config::Settings::load_sync() {
                // Preserve existing routing configuration (strategy, etc.)
                let mut cfg = settings
                    .config
                    .provider_configs
                    .get("free")
                    .and_then(|pc| pc.options.get("routing"))
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({"strategy": "sequential"}));
                if let Some(obj) = cfg.as_object_mut() {
                    obj.insert(
                        "disabled_upstreams".to_string(),
                        serde_json::json!(disabled),
                    );
                }
                settings
                    .config
                    .provider_configs
                    .entry("free".to_string())
                    .or_default()
                    .options
                    .insert("routing".to_string(), cfg);
                let _ = settings.save_sync();
            }
        }
    }

    /// Toggle between showing only non-collapsed fields and all fields.
    pub fn toggle_show_all(&mut self) {
        self.show_all = !self.show_all;
        // If hiding collapsed fields, ensure active_idx is still on a visible one.
        if !self.show_all {
            let visible = self.visible_field_indices();
            if !visible.contains(&self.active_idx) {
                self.active_idx = visible.first().copied().unwrap_or(0);
            }
        }
        self.scroll_offset = 0;
        self.ensure_active_visible();
    }

    /// Collapsed count (unconfigured upstreams currently hidden).
    pub fn collapsed_count(&self) -> usize {
        self.fields.iter().filter(|f| f.collapsed).count()
    }

    /// Move to the next provider row, discarding transient state
    /// (revealed key, un-committed new-key text, captured composite token)
    /// of the row being left.
    pub fn move_next(&mut self) {
        if let Some(field) = self.fields.get_mut(self.active_idx) {
            field.revealed = None;
            field.pending.clear();
            field.pending_token = None;
        }
        let visible = self.visible_field_indices();
        if visible.is_empty() {
            return;
        }
        let pos = visible.iter().position(|i| *i == self.active_idx);
        match pos {
            Some(p) if p + 1 < visible.len() => self.active_idx = visible[p + 1],
            _ => self.active_idx = visible[0],
        }
        self.active_node = NodePos::NewKey;
        self.ensure_active_visible();
    }

    /// Move to the previous provider row, discarding transient state.
    pub fn move_prev(&mut self) {
        if let Some(field) = self.fields.get_mut(self.active_idx) {
            field.revealed = None;
            field.pending.clear();
            field.pending_token = None;
        }
        let visible = self.visible_field_indices();
        if visible.is_empty() {
            return;
        }
        let pos = visible.iter().position(|i| *i == self.active_idx);
        match pos {
            Some(p) if p > 0 => self.active_idx = visible[p - 1],
            _ => self.active_idx = *visible.last().unwrap(),
        }
        self.active_node = NodePos::NewKey;
        self.ensure_active_visible();
    }

    /// Move the horizontal cursor to the next node in the active row
    /// (new-key line → key dots → wraps). Revealed keys are re-masked.
    pub fn move_node_next(&mut self) {
        self.unreveal_active();
        let Some(field) = self.fields.get(self.active_idx) else {
            return;
        };
        let count = field.keys.len() + 1;
        if count <= 1 {
            return;
        }
        let cur = node_index(self.active_node);
        self.active_node = node_at((cur + 1) % count);
    }

    /// Move the horizontal cursor to the previous node in the active row.
    pub fn move_node_prev(&mut self) {
        self.unreveal_active();
        let Some(field) = self.fields.get(self.active_idx) else {
            return;
        };
        let count = field.keys.len() + 1;
        if count <= 1 {
            return;
        }
        let cur = node_index(self.active_node);
        self.active_node = node_at((cur + count - 1) % count);
    }

    fn ensure_active_visible(&mut self) {
        let visible = self.visible_field_indices();
        if visible.is_empty() {
            return;
        }
        // Convert absolute active_idx to its position within visible fields.
        let pos = visible
            .iter()
            .position(|i| *i == self.active_idx)
            .unwrap_or(0);
        if pos < self.scroll_offset {
            self.scroll_offset = pos;
        } else if pos >= self.scroll_offset + Self::VISIBLE_ROWS {
            self.scroll_offset = pos + 1 - Self::VISIBLE_ROWS;
        }
    }

    /// Enter on the active node: appends a typed new key as a dot, reveals
    /// a selected dot, or advances to the next node when neither applies.
    pub fn enter_active(&mut self) {
        if self.append_pending() {
            return;
        }
        if let Some(field) = self.fields.get_mut(self.active_idx) {
            match self.active_node {
                NodePos::Key(i) if i < field.keys.len() => {
                    field.revealed = Some(i);
                }
                _ => self.move_node_next(),
            }
        }
    }

    /// Re-mask the revealed key of the active row. Returns `true` if a key
    /// was revealed (and is now hidden again).
    pub fn unreveal_active(&mut self) -> bool {
        if let Some(field) = self.fields.get_mut(self.active_idx) {
            if field.revealed.is_some() {
                field.revealed = None;
                return true;
            }
        }
        false
    }

    /// Commit the typed new-key buffer as an additional stored key (a new
    /// dot). Returns `true` when a key was appended — or, for the Cloudflare
    /// two-step composite flow, when the API token was captured and the row
    /// is now awaiting the account ID.
    ///
    /// Cloudflare keys are stored as `ACCOUNT_ID:API_TOKEN`. The first Enter
    /// captures the token into `pending_token` and switches the row to an
    /// account-ID prompt; the second Enter joins them into the stored key.
    pub fn append_pending(&mut self) -> bool {
        if let Some(field) = self.fields.get_mut(self.active_idx) {
            if field.from_env {
                return false;
            }
            let key = field.pending.trim().to_string();
            if key.is_empty() {
                return false;
            }
            // Two-step composite flow: first Enter captures the API token.
            if field.upstream.id == "cloudflare" && field.pending_token.is_none() {
                field.pending_token = Some(key);
                field.pending.clear();
                return true;
            }
            // Second Enter: join the typed account ID with the captured token.
            if let Some(token) = field.pending_token.take() {
                let composite = format!("{}:{}", key, token);
                field.keys.push(composite);
                field.key_status.push(None);
                field.pending.clear();
                field.collapsed = false;
                return true;
            }
            field.keys.push(key);
            field.key_status.push(None);
            field.pending.clear();
            field.collapsed = false;
            return true;
        }
        false
    }

    /// Whether the active row's new-key buffer is empty. Gates vim-style
    /// h/j/k/l navigation — once the user starts typing a key, those
    /// letters belong to the key, not the cursor.
    pub fn pending_is_empty(&self) -> bool {
        self.fields
            .get(self.active_idx)
            .map(|f| f.pending.is_empty())
            .unwrap_or(true)
    }

    /// Discard the active row's typed new-key text. Returns `true` if there
    /// was anything to clear (Esc cascade: reveal → clear → close).
    ///
    /// For the Cloudflare two-step flow, an Esc with an empty ID buffer
    /// cancels the captured token (restores it to the new-key line so it can
    /// be re-entered or re-Entered), so a second Esc proceeds to close.
    pub fn clear_pending(&mut self) -> bool {
        if let Some(field) = self.fields.get_mut(self.active_idx) {
            if !field.pending.is_empty() {
                field.pending.clear();
                return true;
            }
            if let Some(token) = field.pending_token.take() {
                field.pending = token;
                return true;
            }
        }
        false
    }

    pub fn insert_char(&mut self, c: char) {
        if let Some(field) = self.fields.get_mut(self.active_idx) {
            // View-only while a key is revealed — never risk corrupting a
            // stored key with an accidental keystroke. Env keys are read-only.
            if field.revealed.is_some() || field.from_env {
                return;
            }
            field.pending.push(c);
            // Auto-expand collapsed field when user starts typing.
            if field.collapsed {
                field.collapsed = false;
            }
        }
    }

    pub fn backspace(&mut self) {
        if let Some(field) = self.fields.get_mut(self.active_idx) {
            if field.revealed.is_some() || field.from_env {
                return;
            }
            field.pending.pop();
        }
    }

    /// Open the delete-confirmation popup for the currently revealed key.
    /// Only possible when the active node is a key dot AND that key is
    /// revealed inline. Returns `false` (and lets the caller backspace) when
    /// no key is revealed or the field is read-only.
    pub fn try_open_delete_confirm(&mut self) -> bool {
        let Some(field) = self.fields.get(self.active_idx) else {
            return false;
        };
        if field.from_env {
            return false;
        }
        if let NodePos::Key(i) = self.active_node {
            if field.revealed == Some(i) && i < field.keys.len() {
                self.delete_confirm = Some(DeleteConfirm {
                    field_idx: self.active_idx,
                    key_idx: i,
                });
                return true;
            }
        }
        false
    }

    /// Confirm the pending delete: remove the key (and its dot) locally.
    /// Changes are applied to the auth store on commit (Ctrl+Enter / Ctrl+S).
    pub fn confirm_delete(&mut self) {
        let Some(dc) = self.delete_confirm.take() else {
            return;
        };
        if let Some(field) = self.fields.get_mut(dc.field_idx) {
            if dc.key_idx < field.keys.len() {
                field.keys.remove(dc.key_idx);
                field.key_status.remove(dc.key_idx);
                field.revealed = None;
            }
        }
        // Re-clamp the node cursor to the (possibly shorter) dot list.
        let node_count = self
            .fields
            .get(self.active_idx)
            .map(|f| f.keys.len() + 1)
            .unwrap_or(1);
        if let NodePos::Key(i) = self.active_node {
            if i + 1 >= node_count {
                self.active_node = node_at(node_count.saturating_sub(1));
            }
        }
    }

    /// Cancel the delete popup (key is kept).
    pub fn cancel_delete(&mut self) {
        self.delete_confirm = None;
    }

    /// Start validating the active field's stored keys in the background.
    /// Returns a `Receiver` that the caller (App) must drain in the main loop.
    /// Only one validation runs at a time.
    pub fn start_validate(&mut self) -> Option<std::sync::mpsc::Receiver<ValidationPing>> {
        if self.is_validating {
            return None;
        }
        let field_idx = self.active_idx;
        let field = self.fields.get(field_idx)?;
        let targets: Vec<(usize, String, String)> = field
            .keys
            .iter()
            .enumerate()
            .filter(|(_, k)| !k.trim().is_empty())
            .map(|(ki, k)| (ki, field.upstream.id.to_string(), k.trim().to_string()))
            .collect();
        if targets.is_empty() {
            return None;
        }

        let (tx, rx) = std::sync::mpsc::channel();
        self.is_validating = true;

        for (ki, upstream_id, key) in targets {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let result = clawde_api::providers::free::validate_upstream_key(&upstream_id, &key);
                // Best-effort send; silently fails if the dialog was closed.
                let _ = tx.send((field_idx, ki, result));
            });
        }

        Some(rx)
    }

    /// Set the validation result for a given key of a given field.
    /// Called from the main loop when a validation result arrives.
    pub fn set_validation_result(
        &mut self,
        field_idx: usize,
        key_idx: usize,
        result: Result<(), String>,
    ) {
        self.is_validating = false;
        if let Some(field) = self.fields.get_mut(field_idx) {
            if let Some(slot) = field.key_status.get_mut(key_idx) {
                *slot = Some(result);
            }
        }
    }

    /// Re-probe the active upstream's stored keys through the health-poller
    /// probe path (`probe_sync_for` — the same probe `/health <upstream>`
    /// runs). Returns a `Receiver` the caller drains in the main loop; each
    /// message is `(field_idx, outcome)` — the field the probe was started
    /// for, captured here so results land on the right provider even if the
    /// user moves the cursor while the probe runs.
    ///
    /// Unlike `start_validate` (which probes the dialog's in-memory keys via
    /// `validate_upstream_key`), this probes the keys on disk exactly like
    /// `/health` would, including the auth-lax chat confirmation for
    /// nvidia/huggingface/openrouter/sambanova/cline.
    pub fn start_reprobe(
        &mut self,
    ) -> Option<std::sync::mpsc::Receiver<(usize, clawde_api::health_poller::ProbeOutcome)>> {
        if self.is_validating {
            return None;
        }
        let field_idx = self.active_idx;
        let upstream_id = self.fields.get(field_idx)?.upstream.id.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        self.is_validating = true;
        std::thread::spawn(move || {
            let outcome = clawde_api::health_poller::probe_sync_for(&upstream_id);
            // Best-effort send; silently fails if the dialog was closed.
            let _ = tx.send((field_idx, outcome));
        });
        Some(rx)
    }

    /// Apply a health-poller [`ProbeOutcome`] to the field it was started
    /// for. Called from the main loop when a re-probe completes. Uses the
    /// captured `field_idx` (not the current cursor) so a mid-probe cursor
    /// move never misdirects or drops the results.
    pub fn apply_probe_outcome(
        &mut self,
        field_idx: usize,
        outcome: &clawde_api::health_poller::ProbeOutcome,
    ) {
        self.is_validating = false;
        let Some(field) = self.fields.get_mut(field_idx) else {
            return;
        };
        for result in &outcome.results {
            // Only apply results for the re-probed upstream, and only to
            // dot slots that still exist (the list may have changed while
            // the probe ran).
            if result.upstream != field.upstream.id {
                continue;
            }
            if let Some(slot) = field.key_status.get_mut(result.key_idx) {
                *slot = if result.ok {
                    Some(Ok(()))
                } else {
                    Some(Err(result.err.clone().unwrap_or_default()))
                };
            }
        }
    }

    /// Enabling Free mode requires at least one usable key on an enabled
    /// upstream (a typed-but-uncommitted new key counts too — Ctrl+Enter
    /// appends it first). More is better. Env-var keys count.
    pub fn can_submit(&self) -> bool {
        self.fields
            .iter()
            .any(|f| f.enabled && (!f.keys.is_empty() || !f.pending.trim().is_empty()))
    }

    /// Total number of stored (usable) keys across all fields.
    pub fn filled_count(&self) -> usize {
        self.fields
            .iter()
            .map(|f| f.keys.iter().filter(|k| !k.trim().is_empty()).count())
            .sum()
    }

    pub fn env_var_count(&self) -> usize {
        self.fields.iter().filter(|f| f.from_env).count()
    }

    /// Compute the auth-store mutations the dialog wants applied:
    /// `(writes, credential_removals)`.
    ///
    /// `writes` is every enabled, non-env field's `(provider_id, keys)` —
    /// these become the rotation pool via `AuthStore::set_keys`.
    /// `credential_removals` is every non-env field's provider id: a stale
    /// single-key credential would win over the `keys` pool in
    /// `api_key_for` (credentials-first) and resurrect keys deleted in the
    /// dialog. Env-var fields are left untouched — env vars remain the
    /// source of truth.
    pub fn store_updates(&self) -> (Vec<(&'static str, Vec<String>)>, Vec<&'static str>) {
        let writes: Vec<(&'static str, Vec<String>)> = self
            .fields
            .iter()
            .filter_map(|f| {
                if !f.enabled || f.from_env {
                    return None;
                }
                let keys: Vec<String> = f
                    .keys
                    .iter()
                    .filter(|k| !k.trim().is_empty())
                    .map(|k| k.trim().to_string())
                    .collect();
                if keys.is_empty() {
                    None
                } else {
                    Some((f.upstream.id, keys))
                }
            })
            .collect();
        // Only remove stale single-key credentials for fields the dialog
        // actively manages. Disabled upstreams are already excluded from the
        // free chain via the persisted `disabled_upstreams` setting — their
        // credentials must survive so a standalone (non-free) provider keeps
        // working when the user only meant to disable free-mode routing,
        // not delete the key.
        let credential_removals: Vec<&'static str> = self
            .fields
            .iter()
            .filter(|f| !f.from_env && f.enabled)
            .map(|f| f.upstream.id)
            .collect();
        (writes, credential_removals)
    }

    /// Consume the dialog state, returning every enabled, non-env field's
    /// `(provider_id, keys)` pairs the user configured. Does NOT close the
    /// dialog — the caller closes it explicitly.
    pub fn take_values(&mut self) -> Vec<(&'static str, Vec<String>)> {
        self.store_updates().0
    }

    /// Apply the current values to the auth store without closing the dialog.
    /// This lets users add keys incrementally: type a key, press Ctrl+S to save
    /// it, then move to the next field and repeat.
    /// Returns the number of keys saved.
    pub fn apply_values(&mut self) -> usize {
        let (writes, removals) = self.store_updates();
        let count: usize = writes.iter().map(|(_, ks)| ks.len()).sum();
        let mut auth_store = clawde_core::AuthStore::load();
        for provider_id in removals {
            auth_store.credentials.remove(provider_id);
        }
        for (provider_id, keys) in writes {
            auth_store.set_keys(provider_id, keys);
        }
        auth_store.save();
        count
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn mask_key(input: &str) -> String {
    if input.is_empty() {
        "paste new API key here...".to_string()
    } else {
        let chars: Vec<char> = input.chars().collect();
        if chars.len() <= 4 {
            input.to_string()
        } else {
            let tail: String = chars[chars.len() - 4..].iter().collect();
            format!("{}{}", "\u{2022}".repeat(chars.len() - 4), tail)
        }
    }
}

/// Placeholder shown on a provider's blank new-key line.
///
/// During the Cloudflare two-step composite flow (token captured, awaiting
/// the account ID) the prompt is explicit about which half is being asked
/// for, instead of the generic "paste new API key here...".
fn new_key_placeholder(field: &FreeModeField) -> String {
    if field.pending_token.is_some() {
        "Paste your Cloudflare ID now...".to_string()
    } else {
        mask_key("")
    }
}

/// Health color for a key dot: green = valid, red = invalid, dim = untested.
fn key_dot_color(status: &Option<Result<(), String>>) -> Color {
    match status {
        Some(Ok(())) => Color::Green,
        Some(Err(_)) => Color::Red,
        None => Color::Rgb(110, 110, 110),
    }
}

fn node_index(node: NodePos) -> usize {
    match node {
        NodePos::NewKey => 0,
        NodePos::Key(i) => i + 1,
    }
}

fn node_at(idx: usize) -> NodePos {
    if idx == 0 {
        NodePos::NewKey
    } else {
        NodePos::Key(idx - 1)
    }
}

pub fn render_free_mode_dialog(
    frame: &mut Frame,
    state: &FreeModeDialogState,
    vim_enabled: bool,
    area: Rect,
) {
    if !state.visible {
        return;
    }

    let pink = Color::Rgb(233, 30, 99);
    let dim = Color::Rgb(90, 90, 90);
    let muted = Color::Rgb(180, 180, 180);
    let tip = Color::Rgb(120, 210, 150);
    let dialog_bg = CLAURST_PANEL_BG;

    render_dark_overlay(frame, area);

    let width = 84u16.min(area.width.saturating_sub(4));
    let height = 26u16.min(area.height.saturating_sub(2));
    let dialog_area = centered_rect(width, height, area);
    state.last_rect.set(dialog_area);
    render_dialog_bg(frame, dialog_area);

    let inner = Rect {
        x: dialog_area.x + 1,
        y: dialog_area.y + 1,
        width: dialog_area.width.saturating_sub(2),
        height: dialog_area.height.saturating_sub(2),
    };

    let total_keys = state.filled_count();
    let env_count = state.env_var_count();
    let title_text = format!(
        "Connect Free (multi-provider \u{2014} {} key{})",
        total_keys,
        if total_keys == 1 { "" } else { "s" }
    );
    let title_pad = inner
        .width
        .saturating_sub(title_text.chars().count() as u16 + 5) as usize;

    let confirm_hint = if state.can_submit() {
        format!(
            " ctrl+enter connect ({} key{} \u{2014} more = better)",
            total_keys,
            if total_keys == 1 { "" } else { "s" }
        )
    } else {
        " paste at least 1 key \u{2014} as many as you can add is better".to_string()
    };

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Title row
    lines.push(Line::from(vec![
        Span::styled(
            format!(" {}", title_text),
            Style::default().fg(pink).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:>width$}", "esc ", width = title_pad),
            Style::default().fg(dim),
        ),
    ]));
    lines.push(Line::from(""));

    // Description (one tight line) + tip.
    lines.push(Line::from(vec![Span::styled(
        " Stack free tiers behind one endpoint.",
        Style::default().fg(muted),
    )]));
    lines.push(Line::from(vec![
        Span::styled(
            " TIP ",
            Style::default().fg(tip).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "More keys = better availability and higher caps.",
            Style::default().fg(tip),
        ),
    ]));
    // Show env-var key hint when any are detected.
    if env_count > 0 {
        lines.push(Line::from(vec![
            Span::styled(
                " \u{1f512} ",
                Style::default().fg(Color::Rgb(180, 160, 80)),
            ),
            Span::styled(
                format!(
                    "{} provider{} use env keys \u{2014} read-only here; edit in your shell profile to change.",
                    env_count,
                    if env_count == 1 { "" } else { "s" }
                ),
                Style::default()
                    .fg(Color::Rgb(180, 160, 80))
                    .add_modifier(Modifier::ITALIC),
            ),
        ]));
    }
    lines.push(Line::from(""));

    // Determine which fields are visible
    let visible_indices = state.visible_field_indices();
    let visible_count = visible_indices.len();

    // Show collapse hint when there are collapsed fields and we're not showing all
    if !state.show_all {
        let collapsed = state.collapsed_count();
        if collapsed > 0 {
            lines.push(Line::from(vec![
                Span::styled(
                    format!(
                        "   \u{2192} {} upstream{} collapsed",
                        collapsed,
                        if collapsed == 1 { "" } else { "s" }
                    ),
                    Style::default().fg(dim).add_modifier(Modifier::ITALIC),
                ),
                Span::styled(
                    "  [tab to show all]",
                    Style::default()
                        .fg(Color::Rgb(120, 120, 140))
                        .add_modifier(Modifier::DIM),
                ),
            ]));
        }
    }

    // Key health summary bar (aggregated over every stored key).
    let valid_count = state
        .fields
        .iter()
        .flat_map(|f| f.key_status.iter())
        .filter(|s| matches!(s, Some(Ok(()))))
        .count();
    let invalid_count = state
        .fields
        .iter()
        .flat_map(|f| f.key_status.iter())
        .filter(|s| matches!(s, Some(Err(_))))
        .count();
    let untested_count = state
        .fields
        .iter()
        .flat_map(|f| f.keys.iter().zip(f.key_status.iter()))
        .filter(|(k, s)| !k.trim().is_empty() && s.is_none())
        .count();
    if valid_count > 0 || invalid_count > 0 || untested_count > 0 {
        let health_text = if valid_count > 0 && invalid_count == 0 && untested_count == 0 {
            format!(
                "   \u{2713} {} key{} valid",
                valid_count,
                if valid_count == 1 { "" } else { "s" }
            )
        } else {
            let mut parts: Vec<String> = Vec::new();
            if valid_count > 0 {
                parts.push(format!("\u{2713} {} ok", valid_count));
            }
            if invalid_count > 0 {
                parts.push(format!("\u{2717} {} bad", invalid_count));
            }
            if untested_count > 0 {
                parts.push(format!("\u{231b} {} pending", untested_count));
            }
            format!("   {}", parts.join("  "))
        };
        lines.push(Line::from(vec![Span::styled(
            health_text,
            Style::default().fg(if invalid_count > 0 {
                Color::Yellow
            } else {
                tip
            }),
        )]));
    }

    // Field viewport: use visible indices only
    let start = state.scroll_offset.min(visible_count.saturating_sub(1));
    let end = (start + FreeModeDialogState::VISIBLE_ROWS).min(visible_count);
    if start > 0 {
        lines.push(Line::from(vec![Span::styled(
            format!("   \u{2191} {} above", start),
            Style::default().fg(dim),
        )]));
    }

    let row_label_width: usize = state
        .fields
        .iter()
        .map(|f| f.upstream.title.chars().count())
        .max()
        .unwrap_or(0)
        .max(8);

    for &idx in visible_indices.iter().skip(start).take(end - start) {
        let field = &state.fields[idx];
        let active = idx == state.active_idx;
        let marker = if active { "\u{25b8}" } else { " " };
        let label_style = if active {
            if field.enabled {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(Color::Rgb(140, 80, 80))
                    .add_modifier(Modifier::BOLD)
            }
        } else if field.enabled {
            Style::default().fg(muted)
        } else {
            Style::default().fg(dim)
        };
        let url_style = Style::default().fg(dim);

        let label_padded = format!("{:<width$}", field.upstream.title, width = row_label_width);
        let mut name_spans: Vec<Span<'static>> = vec![
            Span::styled(format!(" {} ", marker), Style::default().fg(pink)),
            Span::styled(label_padded, label_style),
        ];

        // Health dots — one per usable key, colored by validation status.
        // The selected dot is a bracketed, focusable node: (●)
        for (ki, status) in field.key_status.iter().enumerate() {
            let selected = active && state.active_node == NodePos::Key(ki);
            let color = key_dot_color(status);
            let dot_style = Style::default().fg(color);
            if selected {
                name_spans.push(Span::styled("(", dot_style.add_modifier(Modifier::BOLD)));
                name_spans.push(Span::styled(
                    "\u{25cf}",
                    dot_style.add_modifier(Modifier::BOLD),
                ));
                name_spans.push(Span::styled(")", dot_style.add_modifier(Modifier::BOLD)));
            } else {
                name_spans.push(Span::styled("\u{25cf}", dot_style));
            }
            name_spans.push(Span::raw(" "));
        }

        name_spans.push(Span::styled("  ", Style::default()));
        name_spans.push(Span::styled(field.upstream.key_url.to_string(), url_style));
        if field.from_env {
            name_spans.push(Span::styled(
                "  [env]",
                Style::default()
                    .fg(Color::Rgb(160, 140, 60))
                    .add_modifier(Modifier::DIM),
            ));
        }
        lines.push(Line::from(name_spans));

        // Second line — revealed key > typed new key > node hints > blank.
        let mut input_line: Vec<Span<'static>> = vec![Span::styled("     ", Style::default())];
        if let Some(ri) = field.revealed {
            if let Some(key) = field.keys.get(ri) {
                input_line.push(Span::styled(key.clone(), Style::default().fg(Color::Cyan)));
                input_line.push(Span::styled(
                    "  (revealed \u{2014} del deletes, esc hides)",
                    Style::default().fg(dim),
                ));
            }
        } else if !field.pending.is_empty() {
            let masked = mask_key(&field.pending);
            let input_style = if active {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            input_line.push(Span::styled(masked, input_style));
            input_line.push(Span::styled("_", Style::default().fg(pink)));
        } else if active && state.active_node == NodePos::NewKey {
            let input_style = if field.from_env {
                Style::default()
                    .fg(Color::Rgb(180, 160, 80))
                    .add_modifier(Modifier::ITALIC)
            } else {
                Style::default().fg(dim)
            };
            input_line.push(Span::styled(new_key_placeholder(field), input_style));
            if !field.from_env {
                input_line.push(Span::styled("_", Style::default().fg(pink)));
            }
        } else if active {
            // A dot is selected but not revealed — invite reveal.
            input_line.push(Span::styled(
                "enter to reveal key",
                Style::default().fg(dim),
            ));
        }
        if active && field.from_env {
            input_line.push(Span::styled(
                "  [env \u{2014} edit in shell profile]",
                Style::default()
                    .fg(Color::Rgb(160, 140, 60))
                    .add_modifier(Modifier::DIM),
            ));
        }
        lines.push(Line::from(input_line));

        // Active row's catalog note — per-upstream limits and key-format
        // hints (e.g. Cloudflare's ACCOUNT_ID:API_TOKEN composite) shown
        // right where the user is about to paste.
        if active && !field.upstream.note.is_empty() {
            lines.push(Line::from(vec![Span::styled(
                format!("  \u{2139} {}", field.upstream.note),
                Style::default().fg(dim),
            )]));
        }
    }

    if end < visible_count {
        lines.push(Line::from(vec![Span::styled(
            format!("   \u{2193} {} more", visible_count - end),
            Style::default().fg(dim),
        )]));
    }

    // Show-all / collapse toggle when there are collapsed upstreams
    if !state.show_all && state.collapsed_count() > 0 {
        lines.push(Line::from(vec![Span::styled(
            "   [tab] show all upstreams",
            Style::default()
                .fg(Color::Rgb(100, 100, 140))
                .add_modifier(Modifier::DIM),
        )]));
    } else if state.show_all {
        lines.push(Line::from(vec![Span::styled(
            "   [tab] show configured only",
            Style::default()
                .fg(Color::Rgb(100, 100, 140))
                .add_modifier(Modifier::DIM),
        )]));
    }

    lines.push(Line::from(""));

    // Footer
    let mut footer_spans = vec![
        Span::styled(" \u{2191}/\u{2193} j/k", Style::default().fg(dim)),
        Span::styled(" provider   ", Style::default().fg(dim)),
        Span::styled("\u{2190}/\u{2192} h/l", Style::default().fg(dim)),
        Span::styled(" key   ", Style::default().fg(dim)),
        Span::styled("enter", Style::default().fg(Color::Rgb(140, 140, 160))),
        Span::styled(" reveal/append   ", Style::default().fg(dim)),
        Span::styled("del", Style::default().fg(Color::Rgb(140, 140, 160))),
        Span::styled(" delete key", Style::default().fg(dim)),
    ];
    if vim_enabled && state.vim_search.insert {
        footer_spans.push(Span::styled(
            "  -- INSERT --",
            Style::default().fg(dim).add_modifier(Modifier::BOLD),
        ));
    }
    lines.push(Line::from(footer_spans));
    lines.push(Line::from(vec![
        Span::styled(" tab", Style::default().fg(Color::Rgb(140, 140, 160))),
        Span::styled(" show all   ", Style::default().fg(dim)),
        Span::styled("ctrl+d", Style::default().fg(Color::Rgb(140, 140, 160))),
        Span::styled(" on/off   ", Style::default().fg(dim)),
        Span::styled("ctrl+r", Style::default().fg(Color::Rgb(140, 140, 160))),
        Span::styled(" re-probe   ", Style::default().fg(dim)),
        Span::styled(confirm_hint, Style::default().fg(dim)),
    ]));

    let para = Paragraph::new(lines).bg(dialog_bg);
    frame.render_widget(para, inner);

    render_delete_confirm(frame, state, area);
}

/// Nested "Delete this key? (●)" confirmation popup over the dialog.
fn render_delete_confirm(frame: &mut Frame, state: &FreeModeDialogState, area: Rect) {
    let Some(dc) = state.delete_confirm else {
        return;
    };
    let Some(field) = state.fields.get(dc.field_idx) else {
        return;
    };
    let Some(key) = field.keys.get(dc.key_idx) else {
        return;
    };
    let status = field.key_status.get(dc.key_idx).and_then(|s| s.as_ref());
    let dot_color = match status {
        Some(Ok(())) => Color::Green,
        Some(Err(_)) => Color::Red,
        None => Color::Rgb(110, 110, 110),
    };
    let question = if matches!(status, Some(Err(_))) {
        "Delete this not working key?"
    } else {
        "Delete this key?"
    };

    let pink = Color::Rgb(233, 30, 99);
    let dim = Color::Rgb(90, 90, 90);

    let width = 56u16.min(area.width.saturating_sub(4));
    let height = 8u16;
    let popup = centered_rect(width, height, area);

    let block = Block::bordered()
        .title(Line::from(Span::styled(
            " Delete key ",
            Style::default().fg(pink).add_modifier(Modifier::BOLD),
        )))
        .border_style(Style::default().fg(Color::Rgb(140, 60, 90)));
    frame.render_widget(block, popup);

    let inner = Rect {
        x: popup.x + 1,
        y: popup.y + 1,
        width: popup.width.saturating_sub(2),
        height: popup.height.saturating_sub(2),
    };

    let lines: Vec<Line<'static>> = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!(" {}  ", question),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                "\u{25cf}",
                Style::default().fg(dot_color).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![Span::styled(
            format!("  {}  \u{2190} key {}", mask_key(key), dc.key_idx + 1),
            Style::default().fg(dim),
        )]),
        Line::from(""),
        Line::from(vec![
            Span::styled(" y", Style::default().fg(Color::Rgb(140, 140, 160))),
            Span::styled(" yes    ", Style::default().fg(dim)),
            Span::styled("n", Style::default().fg(Color::Rgb(140, 140, 160))),
            Span::styled(" no    ", Style::default().fg(dim)),
            Span::styled("esc", Style::default().fg(Color::Rgb(140, 140, 160))),
            Span::styled(" cancel", Style::default().fg(dim)),
        ]),
    ];

    let para = Paragraph::new(lines).bg(CLAURST_PANEL_BG);
    frame.render_widget(para, inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn defaults_hidden() {
        let s = FreeModeDialogState::new();
        assert!(!s.visible);
        assert_eq!(s.fields.len(), FREE_CATALOG.len());
    }

    #[test]
    fn open_starts_on_first_empty_field() {
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        assert!(s.visible);
        // All fields are empty and collapsed; open() falls back to index 0.
        assert_eq!(s.active_idx, 0);
        assert_eq!(s.active_node, NodePos::NewKey);
    }

    #[test]
    fn open_seeds_existing_keys_and_shows_only_configured() {
        let mut s = FreeModeDialogState::new();
        s.open(&[(FREE_CATALOG[0].id, vec!["existing-key".to_string()])]);
        assert_eq!(s.fields[0].keys, vec!["existing-key"]);
        assert_eq!(s.fields[0].key_status.len(), 1);
        // Field 0 has a key (not collapsed). Other fields are collapsed.
        // visible = [0]; no empty visible fields, so active_idx = visible[0] = 0.
        assert_eq!(s.active_idx, 0);
        assert!(
            !s.fields[0].collapsed,
            "configured field should be expanded"
        );
        assert!(s.fields[1].collapsed, "empty field should be collapsed");
    }

    #[test]
    fn open_seeds_multiple_keys_per_upstream() {
        let mut s = FreeModeDialogState::new();
        s.open(&[(
            FREE_CATALOG[0].id,
            vec!["k1".into(), "k2".into(), "k3".into()],
        )]);
        assert_eq!(s.fields[0].keys.len(), 3);
        assert_eq!(s.fields[0].key_status.len(), 3);
    }

    #[test]
    fn open_with_show_all_shows_all_fields() {
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        s.toggle_show_all();
        assert!(s.show_all);
        assert_eq!(s.visible_field_indices().len(), s.fields.len());
    }

    #[test]
    fn move_next_wraps_within_visible() {
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        s.toggle_show_all(); // All fields visible
        let n = s.fields.len();
        s.active_idx = n - 1;
        s.move_next();
        assert_eq!(s.active_idx, 0, "should wrap to first field");
    }

    #[test]
    fn move_prev_wraps_within_visible() {
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        s.toggle_show_all(); // All fields visible
        s.active_idx = 0;
        s.move_prev();
        assert_eq!(
            s.active_idx,
            s.fields.len() - 1,
            "should wrap to last field"
        );
    }

    #[test]
    fn move_next_skips_collapsed_fields() {
        let mut s = FreeModeDialogState::new();
        s.open(&[
            (FREE_CATALOG[0].id, vec!["k1".into()]),
            (FREE_CATALOG[2].id, vec!["k3".into()]),
        ]);
        // Only fields 0 and 2 are expanded (have keys).
        let visible = s.visible_field_indices();
        assert_eq!(
            visible,
            vec![0, 2],
            "only configured fields should be visible"
        );
        // active_idx = first visible field without keys → none → first visible = 0
        assert_eq!(s.active_idx, 0);
        s.move_next();
        assert_eq!(s.active_idx, 2, "should skip to field 2 (next visible)");
        s.move_next();
        assert_eq!(s.active_idx, 0, "should wrap to first visible");
    }

    #[test]
    fn toggle_show_all_expands_all_fields() {
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        // Initially all collapsed.
        assert_eq!(s.visible_field_indices().len(), 0);
        s.toggle_show_all();
        assert_eq!(s.visible_field_indices().len(), s.fields.len());
    }

    #[test]
    fn collapsed_count_reflects_empty_fields() {
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        assert_eq!(s.collapsed_count(), s.fields.len());
        s.open(&[(FREE_CATALOG[0].id, vec!["k1".into()])]);
        assert_eq!(s.collapsed_count(), s.fields.len() - 1);
    }

    #[test]
    fn insert_and_backspace_target_pending() {
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        s.insert_char('a');
        s.insert_char('b');
        assert_eq!(s.fields[0].pending, "ab");
        s.backspace();
        assert_eq!(s.fields[0].pending, "a");
    }

    #[test]
    fn insert_ignored_while_key_revealed() {
        let mut s = FreeModeDialogState::new();
        s.open(&[(FREE_CATALOG[0].id, vec!["secret-key".into()])]);
        s.move_node_next(); // → Key(0)
        s.enter_active(); // reveal
        assert_eq!(s.fields[0].revealed, Some(0));
        s.insert_char('x');
        assert_eq!(s.fields[0].pending, "", "stored keys must not be editable");
        assert_eq!(s.fields[0].keys[0], "secret-key");
    }

    #[test]
    fn can_submit_requires_at_least_one_key() {
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        assert!(!s.can_submit());
        s.insert_char('k');
        assert!(s.can_submit(), "uncommitted pending key should count");
    }

    #[test]
    fn node_navigation_cycles_newkey_and_dots() {
        let mut s = FreeModeDialogState::new();
        s.open(&[(FREE_CATALOG[0].id, vec!["k1".into(), "k2".into()])]);
        assert_eq!(s.active_node, NodePos::NewKey);
        s.move_node_next();
        assert_eq!(s.active_node, NodePos::Key(0));
        s.move_node_next();
        assert_eq!(s.active_node, NodePos::Key(1));
        s.move_node_next();
        assert_eq!(s.active_node, NodePos::NewKey, "should wrap");
        s.move_node_prev();
        assert_eq!(s.active_node, NodePos::Key(1));
    }

    #[test]
    fn enter_appends_pending_then_reveals() {
        let mut s = FreeModeDialogState::new();
        s.open(&[(FREE_CATALOG[0].id, vec!["k1".into()])]);
        s.insert_char('n');
        s.insert_char('e');
        s.insert_char('w');
        assert_eq!(s.fields[0].pending, "new");
        s.enter_active();
        assert_eq!(
            s.fields[0].keys,
            vec!["k1", "new"],
            "Enter appends the typed key"
        );
        assert_eq!(s.fields[0].pending, "");
        assert_eq!(s.fields[0].key_status.len(), 2);
        assert_eq!(s.active_node, NodePos::NewKey, "stay on the new-key line");

        s.move_node_next(); // → Key(0)
        s.enter_active();
        assert_eq!(
            s.fields[0].revealed,
            Some(0),
            "Enter reveals a selected dot"
        );
        assert!(s.unreveal_active());
        assert_eq!(s.fields[0].revealed, None);
    }

    #[test]
    fn append_pending_trims_and_skips_empty() {
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        s.insert_char(' ');
        s.insert_char('a');
        s.insert_char(' ');
        assert!(s.append_pending());
        assert_eq!(s.fields[0].keys, vec!["a"], "appended key is trimmed");
        s.insert_char(' ');
        assert!(
            !s.append_pending(),
            "whitespace-only pending must not append"
        );
        assert!(s.fields[0].keys.len() == 1);
    }

    #[test]
    fn cloudflare_append_pending_is_two_step_token_then_id() {
        // Find the cloudflare field (FREE_CATALOG order: huggingface, nvidia,
        // cerebras, google, cloudflare, groq, ...).
        let cf_idx = FREE_CATALOG
            .iter()
            .position(|u| u.id == "cloudflare")
            .expect("cloudflare in catalog");
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        s.active_idx = cf_idx;
        assert_eq!(s.fields[cf_idx].pending_token, None);
        assert_eq!(
            new_key_placeholder(&s.fields[cf_idx]),
            "paste new API key here...",
            "before token capture the row is a normal key prompt"
        );

        // First Enter captures the API token — no key appended yet.
        for c in "tok-123456789".chars() {
            s.insert_char(c);
        }
        assert!(s.append_pending());
        assert_eq!(
            s.fields[cf_idx].pending_token.as_deref(),
            Some("tok-123456789"),
            "token captured on first Enter"
        );
        assert!(
            s.fields[cf_idx].keys.is_empty(),
            "no key until the ID is given"
        );
        assert_eq!(
            new_key_placeholder(&s.fields[cf_idx]),
            "Paste your Cloudflare ID now...",
            "row switches to the ID prompt"
        );

        // Second Enter joins the account ID with the captured token.
        for c in "acct-987654321".chars() {
            s.insert_char(c);
        }
        assert!(s.append_pending());
        assert_eq!(
            s.fields[cf_idx].keys,
            vec!["acct-987654321:tok-123456789"],
            "stored key is the composite ACCOUNT_ID:API_TOKEN"
        );
        assert_eq!(
            s.fields[cf_idx].pending_token, None,
            "two-step flow completes"
        );
        assert!(s.fields[cf_idx].pending.is_empty());
    }

    #[test]
    fn cloudflare_clear_pending_cancels_captured_token() {
        let cf_idx = FREE_CATALOG
            .iter()
            .position(|u| u.id == "cloudflare")
            .expect("cloudflare in catalog");
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        s.active_idx = cf_idx;
        for c in "tok-123456789".chars() {
            s.insert_char(c);
        }
        assert!(s.append_pending());
        assert!(s.fields[cf_idx].pending_token.is_some());

        // Esc with an empty ID buffer restores the token to the new-key line
        // so it can be re-entered (or re-Entered); the next Esc closes.
        assert!(s.clear_pending());
        assert_eq!(
            s.fields[cf_idx].pending_token, None,
            "captured token cancelled"
        );
        assert_eq!(
            s.fields[cf_idx].pending, "tok-123456789",
            "token restored to the new-key line"
        );
    }

    #[test]
    fn cloudflare_multi_key_requires_two_enters_per_key() {
        let cf_idx = FREE_CATALOG
            .iter()
            .position(|u| u.id == "cloudflare")
            .expect("cloudflare in catalog");
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        s.active_idx = cf_idx;

        for c in "tok1-aaaa".chars() {
            s.insert_char(c);
        }
        s.append_pending(); // capture token 1
        for c in "acct1".chars() {
            s.insert_char(c);
        }
        s.append_pending(); // key 1 = acct1:tok1-aaaa
        assert_eq!(s.fields[cf_idx].keys, vec!["acct1:tok1-aaaa"]);

        for c in "tok2-bbbb".chars() {
            s.insert_char(c);
        }
        s.append_pending(); // capture token 2
        for c in "acct2".chars() {
            s.insert_char(c);
        }
        s.append_pending(); // key 2 = acct2:tok2-bbbb
        assert_eq!(
            s.fields[cf_idx].keys,
            vec!["acct1:tok1-aaaa", "acct2:tok2-bbbb"],
            "each key goes through the two-step flow"
        );
    }

    #[test]
    fn clear_pending_cascade() {
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        s.insert_char('x');
        assert!(s.clear_pending());
        assert!(s.fields[0].pending.is_empty());
        assert!(!s.clear_pending(), "second Esc proceeds to close");
    }

    #[test]
    fn delete_requires_reveal_and_env_guard() {
        // Not revealed → no popup.
        let mut s = FreeModeDialogState::new();
        s.open(&[(FREE_CATALOG[0].id, vec!["k1".into()])]);
        s.move_node_next();
        assert!(!s.try_open_delete_confirm(), "must be revealed first");

        // Revealed → popup opens, confirm removes the key and clamps to NewKey.
        s.enter_active();
        assert!(s.try_open_delete_confirm());
        s.confirm_delete();
        assert!(s.delete_confirm.is_none());
        assert!(s.fields[0].keys.is_empty());
        assert_eq!(s.active_node, NodePos::NewKey);

        // Env-var keys can never be deleted here.
        let mut s = FreeModeDialogState::new();
        s.open(&[(FREE_CATALOG[0].id, vec!["env-key".into()])]);
        s.fields[0].from_env = true;
        s.move_node_next();
        s.enter_active();
        assert!(!s.try_open_delete_confirm(), "env keys are read-only");
    }

    #[test]
    fn delete_clamps_node_to_last_dot() {
        let mut s = FreeModeDialogState::new();
        s.open(&[(
            FREE_CATALOG[0].id,
            vec!["k1".into(), "k2".into(), "k3".into()],
        )]);
        // Select the last dot (index 2), reveal, delete.
        s.move_node_next();
        s.move_node_next();
        s.move_node_next();
        assert_eq!(s.active_node, NodePos::Key(2));
        s.enter_active();
        assert!(s.try_open_delete_confirm());
        s.confirm_delete();
        assert_eq!(s.fields[0].keys.len(), 2);
        assert_eq!(
            s.active_node,
            NodePos::Key(1),
            "cursor clamps to the new last dot"
        );
    }

    #[test]
    fn take_values_returns_enabled_non_env_keys_and_does_not_close() {
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        s.toggle_show_all(); // Show all fields so move_next works
        s.insert_char('a');
        s.enter_active(); // append "a" to field 0
        s.move_next();
        s.insert_char('b');
        s.enter_active(); // append "b" to field 1
        let values = s.take_values();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0], (FREE_CATALOG[0].id, vec!["a".to_string()]));
        assert_eq!(values[1], (FREE_CATALOG[1].id, vec!["b".to_string()]));
        // take_values no longer closes — caller is responsible.
        assert!(s.visible);
        s.close();
        assert!(!s.visible);
    }

    #[test]
    fn store_updates_excludes_env_fields_and_disabled() {
        let mut s = FreeModeDialogState::new();
        s.open(&[
            (FREE_CATALOG[0].id, vec!["k1".into(), "k2".into()]),
            (FREE_CATALOG[1].id, vec!["env-key".into()]),
            (FREE_CATALOG[2].id, vec!["disabled-key".into()]),
        ]);
        s.fields[1].from_env = true;
        s.fields[2].enabled = false;

        let (writes, removals) = s.store_updates();
        assert_eq!(writes.len(), 1);
        assert_eq!(
            writes[0],
            (FREE_CATALOG[0].id, vec!["k1".into(), "k2".into()])
        );
        // Env fields are excluded from writes AND credential removals.
        assert!(!removals.contains(&FREE_CATALOG[1].id));
        assert!(removals.contains(&FREE_CATALOG[0].id));
        assert!(
            !removals.contains(&FREE_CATALOG[2].id),
            "disabled fields' credentials must survive"
        );
    }

    #[test]
    fn filled_count_sums_all_keys() {
        let mut s = FreeModeDialogState::new();
        s.open(&[
            (FREE_CATALOG[0].id, vec!["k1".into(), "k2".into()]),
            (FREE_CATALOG[1].id, vec!["k3".into()]),
        ]);
        assert_eq!(s.filled_count(), 3);
    }

    #[test]
    fn close_discards_edits() {
        let mut s = FreeModeDialogState::new();
        s.open(&[(FREE_CATALOG[0].id, vec!["k1".into()])]);
        s.insert_char('x');
        s.close();
        assert!(s.fields[0].keys.is_empty());
        assert!(s.fields[0].pending.is_empty());
        assert_eq!(s.active_node, NodePos::NewKey);
    }

    #[test]
    fn mask_key_hides_all_but_last_four() {
        assert_eq!(mask_key(""), "paste new API key here...");
        assert_eq!(mask_key("abc"), "abc");
        assert_eq!(mask_key("abcdefgh"), "\u{2022}\u{2022}\u{2022}\u{2022}efgh");
    }

    #[test]
    fn render_free_mode_dialog_with_dots_and_popup_does_not_panic() {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut state = FreeModeDialogState::new();
        state.open(&[
            (FREE_CATALOG[0].id, vec!["k1".into(), "k2".into()]),
            (FREE_CATALOG[1].id, vec!["k3".into()]),
        ]);
        state.fields[0].key_status[0] = Some(Ok(()));
        state.fields[0].key_status[1] = Some(Err("401".into()));
        state.move_node_next();
        state.enter_active(); // reveal k1
        terminal
            .draw(|frame| render_free_mode_dialog(frame, &state, false, frame.area()))
            .unwrap();

        // Open the delete popup and render again.
        assert!(state.try_open_delete_confirm());
        terminal
            .draw(|frame| render_free_mode_dialog(frame, &state, false, frame.area()))
            .unwrap();
        state.confirm_delete();
        terminal
            .draw(|frame| render_free_mode_dialog(frame, &state, false, frame.area()))
            .unwrap();
    }
}
