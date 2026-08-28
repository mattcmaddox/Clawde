//! Configurable keyboard shortcuts system

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{Duration, Instant};
use tracing::warn;

/// Named keybinding presets.
///
/// A preset swaps in an alternative baseline on top of [`default_bindings`]:
/// Vim and Emacs flavours adjust navigation chords and editing keys so users
/// of either editor get muscle-memory-friendly defaults without hand-editing
/// `keybindings.json`.  `Default` is the built-in table.  The chosen preset is
/// stored on [`UserKeybindings::preset`] and applied when the resolver is
/// built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum KeybindingPreset {
    /// The stock binding table (`default_bindings()`).
    #[default]
    Default,
    /// Vim-flavoured bindings: `hjkl` navigation in list contexts
    /// and vim-mode prompt editing enabled by default.
    Vim,
    /// Emacs-flavoured bindings: readline-style `Ctrl+B`/`Ctrl+F` char
    /// movement, `Ctrl+P`/`Ctrl+N` history, `Ctrl+K` kill-to-end, `Ctrl+Y`
    /// yank, and `Alt+B`/`Alt+F` word movement.
    Emacs,
}

impl KeybindingPreset {
    /// Resolve a preset from its name (case-insensitive).
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "default" => Some(Self::Default),
            "vim" => Some(Self::Vim),
            "emacs" => Some(Self::Emacs),
            _ => None,
        }
    }

    /// Human-readable label for the current preset.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Vim => "vim",
            Self::Emacs => "emacs",
        }
    }
}

/// All keybinding contexts
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum KeyContext {
    Global,
    Chat,
    Autocomplete,
    Confirmation,
    Help,
    Transcript,
    HistorySearch,
    Task,
    ThemePicker,
    Settings,
    Tabs,
    Attachments,
    Footer,
    MessageSelector,
    DiffDialog,
    ModelPicker,
    Select,
    Plugin,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedKeystroke {
    pub key: String, // normalized key name
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
}

pub type Chord = Vec<ParsedKeystroke>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedBinding {
    pub chord: Chord,
    pub action: Option<String>, // None = unbound
    pub context: KeyContext,
}

/// Parse a keystroke string like "ctrl+shift+enter" into ParsedKeystroke
pub fn parse_keystroke(s: &str) -> Option<ParsedKeystroke> {
    let s = s.trim().to_lowercase();
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut meta = false;
    let mut key_parts: Vec<&str> = Vec::new();

    for part in s.split('+') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part {
            "ctrl" | "control" => ctrl = true,
            "alt" | "opt" | "option" => alt = true,
            "shift" => shift = true,
            "meta" | "cmd" | "command" | "super" | "win" => meta = true,
            _ => key_parts.push(part),
        }
    }

    if key_parts.is_empty() {
        return None;
    }

    let key = normalize_key(key_parts.join("+").as_str());
    Some(ParsedKeystroke {
        key,
        ctrl,
        alt,
        shift,
        meta,
    })
}

fn format_chord_string(chord: &Chord) -> String {
    chord
        .iter()
        .map(|ks| {
            let mut parts = Vec::new();
            if ks.ctrl {
                parts.push("ctrl");
            }
            if ks.alt {
                parts.push("alt");
            }
            if ks.shift {
                parts.push("shift");
            }
            if ks.meta {
                parts.push("meta");
            }
            parts.push(&ks.key);
            parts.join("+")
        })
        .collect::<Vec<_>>()
        .join(" ")
}
fn normalize_key(k: &str) -> String {
    match k {
        "esc" | "escape" => "escape".to_string(),
        "return" | "enter" => "enter".to_string(),
        "del" | "delete" => "delete".to_string(),
        "backspace" | "bs" => "backspace".to_string(),
        "space" | " " => "space".to_string(),
        "up" => "up".to_string(),
        "down" => "down".to_string(),
        "left" => "left".to_string(),
        "right" => "right".to_string(),
        "pageup" | "pgup" => "pageup".to_string(),
        "pagedown" | "pgdn" | "pgdown" => "pagedown".to_string(),
        "home" => "home".to_string(),
        "end" => "end".to_string(),
        "tab" => "tab".to_string(),
        k => k.to_string(),
    }
}

/// Parse a chord (space-separated keystrokes like "ctrl+k ctrl+d")
pub fn parse_chord(s: &str) -> Option<Chord> {
    let keystrokes: Vec<ParsedKeystroke> =
        s.split_whitespace().filter_map(parse_keystroke).collect();
    if keystrokes.is_empty() {
        None
    } else {
        Some(keystrokes)
    }
}

/// Keys that cannot be rebound
pub const NON_REBINDABLE: &[&str] = &["ctrl+c", "ctrl+d", "ctrl+m"];

/// Default keybindings with comprehensive coverage of text editing, navigation, vim, and TUI actions
///
/// # Standard Keybindings (Phase 1 Implementation)
/// - **Ctrl+L**: Clear current input line (like bash) [Chat context only due to conflict]
/// - **Ctrl+Shift+A**: Open the model picker
/// - **Ctrl+K**: Open the command palette
/// - **Ctrl+U**: Kill input from cursor to start of line (Emacs-style)
/// - **Alt+←/Alt+→**: Navigate to previous/next message in transcript
/// - **Alt+.**: Jump to previous error/issue in messages
/// - **Alt+N**: Jump to next error/issue in messages
/// - **Shift+Tab**: Reverse indent/unindent in input (cycle permission mode)
/// - **Ctrl+H**: Delete character before cursor (Chat context, Emacs-style)
/// - **Alt+/**: Open help (alternative to F1)
/// - **Alt+R**: History search
/// - **Alt+D**: Delete word forward
/// - **Ctrl+V**: Paste from clipboard
pub fn default_bindings() -> Vec<ParsedBinding> {
    let defaults: &[(&str, &str, KeyContext)] = &[
        // ========== GLOBAL CONTROL ==========
        // ("ctrl+c", "interrupt", KeyContext::Global), // Handled directly in handle_key_event for two-press confirmation
        // ("ctrl+d", "exit", KeyContext::Global), // Handled directly in handle_key_event for two-press confirmation
        ("ctrl+l", "redraw", KeyContext::Global),
        ("alt+r", "historySearch", KeyContext::Global),
        ("alt+f", "toggleFollowupHistory", KeyContext::Chat),
        ("alt+shift+h", "clearFollowupHistory", KeyContext::Global),
        ("alt+shift+u", "clearFollowupUsage", KeyContext::Global),
        ("alt+b", "createBranch", KeyContext::Global),
        ("alt+/", "openHelp", KeyContext::Global),
        ("alt+c", "compact", KeyContext::Global),
        ("ctrl+/", "showKeybindings", KeyContext::Global),
        ("alt+s", "showSources", KeyContext::Global),
        // ========== CHAT / INPUT CONTEXT ==========
        // Message submission
        ("enter", "submit", KeyContext::Chat),
        // Newline insertion (Shift+Enter / Ctrl+J for multi-line composing)
        ("shift+enter", "newline", KeyContext::Chat),
        // Fallback for terminals that do not support the kitty keyboard protocol
        // (e.g. Terminal.app, older iTerm2, Windows Terminal, or SSH sessions).
        // Without the protocol, Shift+Enter is sent as a raw newline byte (0x0A,
        // LF); crossterm reports that as KeyCode::Char('j') with CONTROL because
        // Ctrl+J == 0x0A in ASCII. When the protocol is enabled (see
        // PushKeyboardEnhancementFlags in tui/src/lib.rs), terminals like Ghostty
        // send a proper CSI-u sequence with the Shift modifier instead, so this
        // fallback is not needed there. Keep it as a compatibility belt-and-braces
        // for terminals that do not support the protocol.
        ("ctrl+j", "newline", KeyContext::Chat),
        // Alt+Enter is the other conventional newline escape (many terminals,
        // e.g. legacy iTerm2 / xterm modifyOtherKeys, send Alt+Enter as
        // \x1b\r). Kept as an additional fallback alongside Shift+Enter/Ctrl+J.
        ("alt+enter", "newline", KeyContext::Chat),
        // Line start/end navigation
        ("home", "goLineStart", KeyContext::Chat),
        ("cmd+left", "goLineStart", KeyContext::Chat),
        ("ctrl+a", "goLineStart", KeyContext::Chat),
        ("end", "goLineEnd", KeyContext::Chat),
        ("cmd+right", "goLineEnd", KeyContext::Chat),
        ("ctrl+e", "goLineEnd", KeyContext::Chat),
        // Word navigation
        ("ctrl+left", "moveWordBackward", KeyContext::Chat),
        ("ctrl+right", "moveWordForward", KeyContext::Chat),
        ("alt+b", "moveWordBackward", KeyContext::Chat),
        ("alt+f", "moveWordForward", KeyContext::Chat),
        // Word deletion
        ("ctrl+w", "killWord", KeyContext::Chat),
        ("alt+backspace", "killWord", KeyContext::Chat),
        ("alt+d", "deleteWord", KeyContext::Chat),
        // Character/line deletion
        ("ctrl+h", "deleteCharBefore", KeyContext::Chat),
        ("ctrl+u", "killToStart", KeyContext::Chat),
        ("ctrl+l", "clearLine", KeyContext::Chat),
        // History navigation
        ("up", "historyPrev", KeyContext::Chat),
        // Shift+J/K are case-insensitive vertical-navigation aliases. The TUI
        // resolves these semantic actions to real Up/Down events so every
        // widget that already handles arrow navigation gets them consistently.
        ("shift+k", "verticalPrev", KeyContext::Chat),
        ("shift+j", "verticalNext", KeyContext::Chat),
        // Ctrl+O expands/collapses all thinking blocks (mirrors the spec's
        // "ctrl+o to expand" convention); history navigation stays on Up.
        ("ctrl+o", "toggleThinkingExpand", KeyContext::Chat),
        ("down", "historyNext", KeyContext::Chat),
        ("ctrl+i", "historyNext", KeyContext::Chat),
        // Message navigation
        ("alt+left", "previousMessage", KeyContext::Chat),
        ("alt+right", "nextMessage", KeyContext::Chat),
        // Error/issue navigation
        ("alt+n", "jumpToNextError", KeyContext::Chat),
        ("alt+.", "jumpToPreviousError", KeyContext::Chat),
        // None of the aspirational search/navigation bindings for which
        // no handler arms exist are kept in the default table — a binding
        // with no backend silently swallows the key, which is worse than
        // having no binding at all.
        //
        // Removed: findInMessage (ctrl+f), globalSearch (ctrl+shift+f),
        // findNext (f3/ctrl+]), findPrev (shift+f3/ctrl+[), goToLine (ctrl+g).
        // Indentation
        ("tab", "indent", KeyContext::Chat),
        ("shift+tab", "reverseIndent", KeyContext::Chat),
        // Paste placeholders — expand `[Pasted text #N ...]` back into the
        // full pasted body (clicking the placeholder does the same).
        ("alt+p", "expandPaste", KeyContext::Chat),
        // Standalone clipboard image paste — reads the system clipboard
        // and attaches any image found, without requiring Ctrl+V (which
        // Windows Terminal via SSH intercepts).
        ("alt+i", "pasteImage", KeyContext::Chat),
        // Scrolling
        ("pageup", "scrollUp", KeyContext::Chat),
        ("pagedown", "scrollDown", KeyContext::Chat),
        // App shortcuts
        ("alt+m", "openModelPicker", KeyContext::Chat),
        ("alt+shift+m", "openModePicker", KeyContext::Chat),
        ("ctrl+,", "openSettings", KeyContext::Chat),
        ("ctrl+k", "openCommandPalette", KeyContext::Chat),
        // ========== FREE MODE UPSTREAM CYCLE ==========
        // Alt+J/K open the free-model dropdown (auto + every configured free
        // upstream); Enter pins the selection. Alt+U kept as a forward-cycle
        // alias for muscle memory.
        ("alt+j", "openFreeModelPopup", KeyContext::Chat),
        ("alt+k", "openFreeModelPopup", KeyContext::Chat),
        ("alt+u", "cycleFreeUpstream", KeyContext::Chat),
        ("alt+t", "cycleFreeTask", KeyContext::Chat),
        // ========== OLLAMA MODE TOGGLE ==========
        ("alt+o", "toggleOllama", KeyContext::Chat),
        // ========== EFFORT ==========
        // Alt+H/L step reasoning up/down along the model's supported ladder
        // (clamped — never wraps). Alt+E opens the visual effort picker.
        // Tab+H/L are chord-prefix aliases: press Tab then H/L within the
        // chord window to step effort without releasing to Alt.
        ("alt+h", "effortDecrease", KeyContext::Chat),
        ("alt+l", "effortIncrease", KeyContext::Chat),
        ("alt+e", "openEffort", KeyContext::Chat),
        ("tab h", "effortDecrease", KeyContext::Chat),
        ("tab l", "effortIncrease", KeyContext::Chat),
        // ========== CONFIRMATION DIALOGS ==========
        ("y", "yes", KeyContext::Confirmation),
        ("enter", "yes", KeyContext::Confirmation),
        ("n", "no", KeyContext::Confirmation),
        ("escape", "no", KeyContext::Confirmation),
        ("up", "prevOption", KeyContext::Confirmation),
        ("down", "nextOption", KeyContext::Confirmation),
        ("shift+k", "verticalPrev", KeyContext::Confirmation),
        ("shift+j", "verticalNext", KeyContext::Confirmation),
        // ========== HELP OVERLAY ==========
        ("escape", "close", KeyContext::Help),
        ("q", "close", KeyContext::Help),
        ("up", "scrollUp", KeyContext::Help),
        ("down", "scrollDown", KeyContext::Help),
        ("shift+k", "verticalPrev", KeyContext::Help),
        ("shift+j", "verticalNext", KeyContext::Help),
        ("k", "scrollUp", KeyContext::Help),
        ("j", "scrollDown", KeyContext::Help),
        ("pageup", "pageUp", KeyContext::Help),
        ("pagedown", "pageDown", KeyContext::Help),
        // ========== HISTORY SEARCH ==========
        ("up", "prevResult", KeyContext::HistorySearch),
        ("down", "nextResult", KeyContext::HistorySearch),
        ("shift+k", "verticalPrev", KeyContext::HistorySearch),
        ("shift+j", "verticalNext", KeyContext::HistorySearch),
        ("k", "prevResult", KeyContext::HistorySearch),
        ("j", "nextResult", KeyContext::HistorySearch),
        ("enter", "select", KeyContext::HistorySearch),
        ("escape", "cancel", KeyContext::HistorySearch),
        ("tab", "togglePreview", KeyContext::HistorySearch),
        // ========== MESSAGE SELECTOR OVERLAY ==========
        ("up", "prevMessage", KeyContext::MessageSelector),
        ("down", "nextMessage", KeyContext::MessageSelector),
        ("shift+k", "verticalPrev", KeyContext::MessageSelector),
        ("shift+j", "verticalNext", KeyContext::MessageSelector),
        ("k", "prevMessage", KeyContext::MessageSelector),
        ("j", "nextMessage", KeyContext::MessageSelector),
        ("enter", "select", KeyContext::MessageSelector),
        ("escape", "cancel", KeyContext::MessageSelector),
        // ========== THEME & MODEL PICKERS ==========
        ("up", "prev", KeyContext::ThemePicker),
        ("down", "next", KeyContext::ThemePicker),
        ("shift+k", "verticalPrev", KeyContext::ThemePicker),
        ("shift+j", "verticalNext", KeyContext::ThemePicker),
        ("k", "prev", KeyContext::ThemePicker),
        ("j", "next", KeyContext::ThemePicker),
        ("pageup", "pageUp", KeyContext::ThemePicker),
        ("pagedown", "pageDown", KeyContext::ThemePicker),
        ("enter", "select", KeyContext::ThemePicker),
        ("escape", "cancel", KeyContext::ThemePicker),
        // ========== TASK LIST ==========
        ("up", "prevTask", KeyContext::Task),
        ("down", "nextTask", KeyContext::Task),
        ("shift+k", "verticalPrev", KeyContext::Task),
        ("shift+j", "verticalNext", KeyContext::Task),
        ("k", "prevTask", KeyContext::Task),
        ("j", "nextTask", KeyContext::Task),
        ("enter", "selectTask", KeyContext::Task),
        ("escape", "closeTask", KeyContext::Task),
        ("x", "toggleDone", KeyContext::Task),
        // ========== DIFF DIALOG ==========
        ("up", "prevDiff", KeyContext::DiffDialog),
        ("down", "nextDiff", KeyContext::DiffDialog),
        ("shift+k", "verticalPrev", KeyContext::DiffDialog),
        ("shift+j", "verticalNext", KeyContext::DiffDialog),
        ("k", "prevDiff", KeyContext::DiffDialog),
        ("j", "nextDiff", KeyContext::DiffDialog),
        ("a", "acceptDiff", KeyContext::DiffDialog),
        ("enter", "acceptDiff", KeyContext::DiffDialog),
        ("r", "rejectDiff", KeyContext::DiffDialog),
        ("escape", "rejectDiff", KeyContext::DiffDialog),
        ("pageup", "pageUp", KeyContext::DiffDialog),
        ("pagedown", "pageDown", KeyContext::DiffDialog),
        // ========== MODAL SELECT (Generic) ==========
        ("up", "prev", KeyContext::Select),
        ("down", "next", KeyContext::Select),
        ("shift+k", "verticalPrev", KeyContext::Select),
        ("shift+j", "verticalNext", KeyContext::Select),
        ("k", "prev", KeyContext::Select),
        ("j", "next", KeyContext::Select),
        ("pageup", "pageUp", KeyContext::Select),
        ("pagedown", "pageDown", KeyContext::Select),
        ("shift+k", "verticalPrev", KeyContext::Settings),
        ("shift+j", "verticalNext", KeyContext::Settings),
        ("shift+k", "verticalPrev", KeyContext::ModelPicker),
        ("shift+j", "verticalNext", KeyContext::ModelPicker),
        ("enter", "select", KeyContext::Select),
        ("escape", "cancel", KeyContext::Select),
        ("/", "search", KeyContext::Select),
        // ========== PLUGIN & ATTACHMENTS ==========
        ("up", "prev", KeyContext::Plugin),
        ("down", "next", KeyContext::Plugin),
        ("shift+k", "verticalPrev", KeyContext::Plugin),
        ("shift+j", "verticalNext", KeyContext::Plugin),
        ("enter", "select", KeyContext::Plugin),
        ("escape", "cancel", KeyContext::Plugin),
        ("space", "toggle", KeyContext::Attachments),
        ("a", "addAttachment", KeyContext::Attachments),
        ("r", "removeAttachment", KeyContext::Attachments),
    ];

    defaults
        .iter()
        .filter_map(|(chord_str, action, context)| {
            parse_chord(chord_str).map(|chord| ParsedBinding {
                chord,
                action: Some(action.to_string()),
                context: context.clone(),
            })
        })
        .collect()
}

/// The full binding table for a [`KeybindingPreset`]: the stock defaults plus
/// preset-specific additions/overrides appended after them.  The resolver
/// matches the *last* exact binding for a given chord+context, so anything
/// appended here wins over the default for the same keys.
pub fn preset_bindings(preset: &KeybindingPreset) -> Vec<ParsedBinding> {
    let mut bindings = default_bindings();
    let extras: &[(&str, &str, KeyContext)] = match preset {
        KeybindingPreset::Default => &[],
        KeybindingPreset::Vim => VIM_PRESET_EXTRAS,
        KeybindingPreset::Emacs => EMACS_PRESET_EXTRAS,
    };
    bindings.extend(extras.iter().filter_map(|(chord_str, action, context)| {
        parse_chord(chord_str).map(|chord| ParsedBinding {
            chord,
            action: Some(action.to_string()),
            context: context.clone(),
        })
    }));
    bindings
}

/// Vim-flavoured additions on top of the defaults.
///
/// The stock table already navigates list contexts with `j`/`k`; the Vim
/// preset completes the `hjkl` set (adding `h`/`l` as prev/next).  These only
/// touch list/dialog contexts, never Chat — plain letters must keep typing
/// into the prompt.
///
/// NOTE: `gg`/`G` jump-to-top/bottom is deliberately NOT included — the
/// transcript context is never the active resolver context (see
/// `App::current_key_context`), so such bindings would be dead config.
const VIM_PRESET_EXTRAS: &[(&str, &str, KeyContext)] = &[
    ("h", "prev", KeyContext::Select),
    ("l", "next", KeyContext::Select),
    ("h", "prev", KeyContext::ThemePicker),
    ("l", "next", KeyContext::ThemePicker),
    ("h", "prevResult", KeyContext::HistorySearch),
    ("l", "nextResult", KeyContext::HistorySearch),
    ("h", "prevMessage", KeyContext::MessageSelector),
    ("l", "nextMessage", KeyContext::MessageSelector),
    ("h", "prevTask", KeyContext::Task),
    ("l", "nextTask", KeyContext::Task),
    ("h", "prevDiff", KeyContext::DiffDialog),
    ("l", "nextDiff", KeyContext::DiffDialog),
];

/// Emacs (readline-style) additions on top of the defaults.
///
/// The stock Chat table already provides `Ctrl+A`/`Ctrl+E` line edges,
/// `Ctrl+W` kill-word, `Ctrl+U` kill-to-start, `Ctrl+H` delete-char and
/// `Alt+D` delete-word.  This preset adds the remaining readline chords:
/// `Ctrl+B`/`Ctrl+F` char movement, `Ctrl+P`/`Ctrl+N` history, `Ctrl+K`
/// kill-to-end, `Ctrl+Y` yank, `Alt+B`/`Alt+F` word movement, and moves the
/// command palette to `Ctrl+Shift+P` (classic emacs `M-x` style) so `Ctrl+K`
/// stays free for kill-line.
const EMACS_PRESET_EXTRAS: &[(&str, &str, KeyContext)] = &[
    ("ctrl+b", "moveCharBackward", KeyContext::Chat),
    ("ctrl+f", "moveCharForward", KeyContext::Chat),
    ("ctrl+p", "historyPrev", KeyContext::Chat),
    ("ctrl+n", "historyNext", KeyContext::Chat),
    ("ctrl+k", "killToEnd", KeyContext::Chat),
    ("ctrl+y", "yank", KeyContext::Chat),
    ("alt+b", "moveWordBackward", KeyContext::Chat),
    ("alt+f", "moveWordForward", KeyContext::Chat),
    ("ctrl+shift+p", "openCommandPalette", KeyContext::Chat),
];

/// Current schema version for keybindings
pub const KEYBINDINGS_SCHEMA_VERSION: u32 = 1;
/// User keybindings loaded from ~/.clawde/keybindings.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserKeybindings {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Active keybinding preset ("default", "vim" or "emacs").  Stored so the
    /// resolver can layer the preset baseline on top of user overrides.
    #[serde(default)]
    pub preset: KeybindingPreset,
    pub bindings: Vec<UserBinding>,
}

fn default_schema_version() -> u32 {
    KEYBINDINGS_SCHEMA_VERSION
}

impl Default for UserKeybindings {
    fn default() -> Self {
        Self {
            schema_version: KEYBINDINGS_SCHEMA_VERSION,
            preset: KeybindingPreset::Default,
            bindings: Vec::new(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct JsonKeybindingConfig {
    #[serde(default)]
    bindings: Vec<JsonKeybindingBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JsonKeybindingBlock {
    context: String,
    bindings: IndexMap<String, Option<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserBinding {
    pub chord: String,          // e.g. "ctrl+k ctrl+d"
    pub action: Option<String>, // None = unbound
    pub context: Option<String>,
}

impl UserKeybindings {
    pub fn from_json_str(content: &str) -> Self {
        let mut kb = serde_json::from_str(content)
            .or_else(|_| Self::from_block_config(content))
            .unwrap_or_default();

        // Warn about and filter out non-rebindable keys
        let original_len = kb.bindings.len();
        kb.bindings.retain(|binding| {
            let normalized = binding.chord.to_lowercase();
            if NON_REBINDABLE
                .iter()
                .any(|protected| normalized == *protected)
            {
                warn!(
                    "Cannot rebind protected key '{}' in keybindings.json",
                    binding.chord
                );
                return false;
            }
            true
        });

        if kb.bindings.len() < original_len {
            let filtered_count = original_len - kb.bindings.len();
            warn!(
                "Filtered out {} protected keybinding(s). Protected keys: {}",
                filtered_count,
                NON_REBINDABLE.join(", ")
            );
        }

        kb
    }

    pub fn load(config_dir: &Path) -> Self {
        let path = config_dir.join("keybindings.json");
        if let Ok(content) = std::fs::read_to_string(&path) {
            let mut kb = Self::from_json_str(&content);
            let old_version = kb.schema_version;
            kb.smart_merge_with_defaults();

            // Save back if schema was updated
            if kb.schema_version > old_version {
                if let Err(e) = kb.save(config_dir) {
                    warn!("Failed to save updated keybindings: {}", e);
                }
            }

            kb
        } else {
            Self::default()
        }
    }

    pub fn save(&self, config_dir: &Path) -> anyhow::Result<()> {
        let path = config_dir.join("keybindings.json");
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    fn from_block_config(content: &str) -> Result<Self, serde_json::Error> {
        let config: JsonKeybindingConfig = serde_json::from_str(content)?;
        let bindings = config
            .bindings
            .into_iter()
            .flat_map(|block| {
                let context = block.context;
                block
                    .bindings
                    .into_iter()
                    .map(move |(chord, action)| UserBinding {
                        chord,
                        action,
                        context: Some(context.clone()),
                    })
            })
            .collect();
        Ok(Self {
            schema_version: 0,
            preset: KeybindingPreset::Default,
            bindings,
        })
    }

    /// Smart merge: preserve user customizations while adding new defaults
    pub fn smart_merge_with_defaults(&mut self) {
        if self.schema_version >= KEYBINDINGS_SCHEMA_VERSION {
            return; // Already up to date
        }

        let old_version = self.schema_version;
        self.schema_version = KEYBINDINGS_SCHEMA_VERSION;

        // Build a set of user-customized bindings (those that differ from old defaults)
        // and bindings user explicitly unbound
        let mut user_customizations: std::collections::HashMap<String, Option<String>> =
            std::collections::HashMap::new();
        for binding in &self.bindings {
            // Migration: remove old bindings that have changed in defaults
            // This distinguishes between "user customized" and "old default that changed"

            // Old: ctrl+a -> openModelPicker (moved to ctrl+shift+a)
            if binding.chord == "ctrl+a" && binding.action.as_deref() == Some("openModelPicker") {
                continue;
            }

            // Old: tab -> togglePreview in Chat context (changed to indent)
            if binding.chord == "tab"
                && binding.context.as_deref() == Some("Chat")
                && binding.action.as_deref() == Some("togglePreview")
            {
                continue;
            }

            user_customizations.insert(binding.chord.clone(), binding.action.clone());
        }

        // Get current defaults and integrate customizations
        let mut merged_bindings = Vec::new();
        for default in default_bindings() {
            let chord_str = format_chord_string(&default.chord);
            let context_str = format!("{:?}", default.context);

            if let Some(custom_action) = user_customizations.get(&chord_str) {
                // User has customized this binding, use their version
                merged_bindings.push(UserBinding {
                    chord: chord_str.clone(),
                    action: custom_action.clone(),
                    context: Some(context_str),
                });
                user_customizations.remove(&chord_str);
            } else {
                // Use the default
                merged_bindings.push(UserBinding {
                    chord: chord_str,
                    action: default.action.clone(),
                    context: Some(context_str),
                });
            }
        }

        // Add any remaining user customizations that aren't in current defaults
        for (chord, action) in user_customizations {
            merged_bindings.push(UserBinding {
                chord,
                action,
                context: None,
            });
        }

        self.bindings = merged_bindings;
        warn!(
            "Keybindings schema upgraded from v{} to v{}. User customizations preserved.",
            old_version, KEYBINDINGS_SCHEMA_VERSION
        );
    }
}

/// Timeout before a single-key chord prefix fires its standalone action.
/// If the user presses Tab and then H within this window, the Tab action
/// is suppressed and the chord fires instead.
const CHORD_TIMEOUT_MS: u64 = 200;

/// Resolved keybindings (defaults merged with user overrides)
pub struct KeybindingResolver {
    bindings: Vec<ParsedBinding>,
    pending_chord: Vec<ParsedKeystroke>,
    /// When a `PendingSingle` was returned, this records when it was emitted.
    /// `check_timeout()` fires the held action once this exceeds
    /// [`CHORD_TIMEOUT_MS`].
    pending_single_action: Option<String>,
    pending_single_started: Option<Instant>,
}

impl KeybindingResolver {
    pub fn new(user: &UserKeybindings) -> Self {
        let mut bindings = preset_bindings(&user.preset);

        // Apply user overrides (user bindings win, last match wins)
        for user_binding in &user.bindings {
            if let Some(chord) = parse_chord(&user_binding.chord) {
                let context = user_binding
                    .context
                    .as_deref()
                    .and_then(|c| serde_json::from_str(&format!("\"{}\"", c)).ok())
                    .unwrap_or(KeyContext::Global);

                bindings.push(ParsedBinding {
                    chord,
                    action: user_binding.action.clone(),
                    context,
                });
            }
        }

        Self {
            bindings,
            pending_chord: Vec::new(),
            pending_single_action: None,
            pending_single_started: None,
        }
    }

    /// Process a keystroke, returns action if binding matches
    pub fn process(
        &mut self,
        keystroke: ParsedKeystroke,
        context: &KeyContext,
    ) -> KeybindingResult {
        // If we have a pending single-key action (from a previous keystroke
        // that matched both a single-key binding and a chord prefix), check
        // whether this keystroke completes the chord.
        if let Some(held_action) = self.pending_single_action.take() {
            self.pending_single_started = None;
            // The pending chord already contains the first keystroke.
            self.pending_chord.push(keystroke);

            let matches: Vec<&ParsedBinding> = self
                .bindings
                .iter()
                .filter(|b| &b.context == context || b.context == KeyContext::Global)
                .filter(|b| b.chord.starts_with(self.pending_chord.as_slice()))
                .collect();

            if !matches.is_empty() {
                let exact: Vec<&ParsedBinding> = matches
                    .iter()
                    .copied()
                    .filter(|b| b.chord.len() == self.pending_chord.len())
                    .collect();

                if !exact.is_empty() {
                    // Chord completed — fire the chord action, discard held.
                    let binding = exact.last().unwrap();
                    self.pending_chord.clear();
                    return match &binding.action {
                        Some(action) => KeybindingResult::Action(action.clone()),
                        None => KeybindingResult::Unbound,
                    };
                }
                // Longer chord in progress
                return KeybindingResult::Pending;
            }

            // Keystroke did not complete any chord — fire the held single-key
            // action.  The chord timeout expired or the second key was
            // unrelated; treat the original key as a standalone press.
            self.pending_chord.clear();
            return KeybindingResult::Action(held_action);
        }

        self.process_fresh(keystroke, context)
    }

    /// Inner dispatch that handles a single keystroke without pending-single
    /// state.
    fn process_fresh(
        &mut self,
        keystroke: ParsedKeystroke,
        context: &KeyContext,
    ) -> KeybindingResult {
        self.pending_chord.push(keystroke);

        // Find matching bindings in current context + Global
        let matches: Vec<&ParsedBinding> = self
            .bindings
            .iter()
            .filter(|b| &b.context == context || b.context == KeyContext::Global)
            .filter(|b| b.chord.starts_with(self.pending_chord.as_slice()))
            .collect();

        if matches.is_empty() {
            self.pending_chord.clear();
            return KeybindingResult::NoMatch;
        }

        let exact: Vec<&ParsedBinding> = matches
            .iter()
            .copied()
            .filter(|b| b.chord.len() == self.pending_chord.len())
            .collect();

        if !exact.is_empty() {
            // Check if this exact match is ALSO a chord prefix (i.e. there
            // are longer bindings that start with this chord).  If so, hold
            // the action so the next keystroke can complete the chord.
            let is_chord_prefix = matches
                .iter()
                .any(|b| b.chord.len() > self.pending_chord.len());
            if is_chord_prefix {
                let binding = exact.last().unwrap();
                if let Some(action) = &binding.action {
                    self.pending_single_action = Some(action.clone());
                    self.pending_single_started = Some(Instant::now());
                    // Don't clear pending_chord — keep it for the next keystroke.
                    return KeybindingResult::PendingSingle(action.clone());
                }
            }

            // Last match wins (user overrides)
            let binding = exact.last().unwrap();
            self.pending_chord.clear();
            return match &binding.action {
                Some(action) => KeybindingResult::Action(action.clone()),
                None => KeybindingResult::Unbound,
            };
        }

        // Chord in progress
        KeybindingResult::Pending
    }

    /// Check whether a held single-key action has timed out.
    ///
    /// Call this after each keystroke that returned [`KeybindingResult::Pending`]
    /// or [`KeybindingResult::PendingSingle`].  Returns `Some(action)` when the
    /// chord window has expired and the single-key action should fire.
    pub fn check_timeout(&mut self) -> Option<String> {
        if let (Some(action), Some(started)) =
            (&self.pending_single_action, self.pending_single_started)
        {
            if started.elapsed() >= Duration::from_millis(CHORD_TIMEOUT_MS) {
                let action = action.clone();
                self.pending_single_action = None;
                self.pending_single_started = None;
                self.pending_chord.clear();
                return Some(action);
            }
        }
        None
    }

    /// Resolve an exact single-key binding without changing pending chord state.
    ///
    /// This is used for semantic aliases that must be translated before a
    /// widget's legacy arrow-key handler runs. Explicit user unbindings are
    /// returned as [`KeybindingResult::Unbound`] so they suppress defaults.
    pub fn resolve_single(
        &self,
        keystroke: &ParsedKeystroke,
        context: &KeyContext,
    ) -> Option<KeybindingResult> {
        self.bindings
            .iter()
            .filter(|binding| {
                (binding.context == *context || binding.context == KeyContext::Global)
                    && binding.chord.len() == 1
                    && binding.chord[0] == *keystroke
            })
            .next_back()
            .map(|binding| match &binding.action {
                Some(action) => KeybindingResult::Action(action.clone()),
                None => KeybindingResult::Unbound,
            })
    }

    pub fn cancel_chord(&mut self) {
        self.pending_chord.clear();
        // A held single-key action is part of the same wait; cancelling the
        // chord must not leave it behind to fire on a later keystroke.
        self.pending_single_action = None;
        self.pending_single_started = None;
    }

    /// Discard a held single-key action without firing it.
    pub fn cancel_pending_single(&mut self) {
        self.pending_single_action = None;
        self.pending_single_started = None;
    }

    pub fn has_pending_chord(&self) -> bool {
        !self.pending_chord.is_empty()
    }
}

impl PartialEq for ParsedKeystroke {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
            && self.ctrl == other.ctrl
            && self.alt == other.alt
            && self.shift == other.shift
            && self.meta == other.meta
    }
}

#[derive(Debug, Clone)]
pub enum KeybindingResult {
    Action(String),
    Unbound,
    Pending,
    /// A single-key action fired, but the key is also a chord prefix.
    /// The caller should start a short timeout; if the next keystroke
    /// completes the chord within the window, discard this action.
    PendingSingle(String),
    NoMatch,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_keystroke_simple() {
        let ks = parse_keystroke("enter").unwrap();
        assert_eq!(ks.key, "enter");
        assert!(!ks.ctrl);
        assert!(!ks.alt);
        assert!(!ks.shift);
        assert!(!ks.meta);
    }

    #[test]
    fn test_parse_keystroke_ctrl_c() {
        let ks = parse_keystroke("ctrl+c").unwrap();
        assert_eq!(ks.key, "c");
        assert!(ks.ctrl);
        assert!(!ks.alt);
    }

    #[test]
    fn test_parse_keystroke_ctrl_shift_enter() {
        let ks = parse_keystroke("ctrl+shift+enter").unwrap();
        assert_eq!(ks.key, "enter");
        assert!(ks.ctrl);
        assert!(ks.shift);
        assert!(!ks.alt);
    }

    #[test]
    fn test_parse_keystroke_normalizes_esc() {
        let ks = parse_keystroke("esc").unwrap();
        assert_eq!(ks.key, "escape");
    }

    #[test]
    fn test_parse_keystroke_normalizes_return() {
        let ks = parse_keystroke("return").unwrap();
        assert_eq!(ks.key, "enter");
    }

    #[test]
    fn test_parse_keystroke_empty_returns_none() {
        assert!(parse_keystroke("ctrl+").is_none());
        assert!(parse_keystroke("").is_none());
    }

    #[test]
    fn test_parse_chord_single() {
        let chord = parse_chord("ctrl+c").unwrap();
        assert_eq!(chord.len(), 1);
        assert_eq!(chord[0].key, "c");
        assert!(chord[0].ctrl);
    }

    #[test]
    fn test_parse_chord_multi() {
        let chord = parse_chord("ctrl+k ctrl+d").unwrap();
        assert_eq!(chord.len(), 2);
        assert_eq!(chord[0].key, "k");
        assert_eq!(chord[1].key, "d");
        assert!(chord[0].ctrl);
        assert!(chord[1].ctrl);
    }

    #[test]
    fn test_parse_chord_empty_returns_none() {
        assert!(parse_chord("").is_none());
    }

    #[test]
    fn test_default_bindings_not_empty() {
        let bindings = default_bindings();
        assert!(!bindings.is_empty());
    }

    #[test]
    fn test_default_bindings_omit_ctrl_c_and_ctrl_d() {
        // Ctrl+C and Ctrl+D are intentionally NOT in the resolver table: since
        // commit 270947d they are handled directly in handle_key_event for the
        // alternating two-press exit/interrupt confirmation. The resolver must
        // not shadow that special handling, so the table must omit them.
        let bindings = default_bindings();
        let has = |key: &str| {
            bindings.iter().any(|b| {
                b.chord.len() == 1
                    && b.chord[0].ctrl
                    && b.chord[0].key == key
                    && b.context == KeyContext::Global
            })
        };
        assert!(
            !has("c"),
            "ctrl+c must be handled directly, not via the resolver"
        );
        assert!(
            !has("d"),
            "ctrl+d must be handled directly, not via the resolver"
        );
    }

    #[test]
    fn test_default_bindings_map_alt_m_and_alt_k_to_app_shortcuts() {
        let bindings = default_bindings();

        let alt_m = bindings.iter().find(|b| {
            b.chord.len() == 1
                && b.chord[0].alt
                && b.chord[0].key == "m"
                && b.context == KeyContext::Chat
        });
        let ctrl_k = bindings.iter().find(|b| {
            b.chord.len() == 1
                && b.chord[0].ctrl
                && b.chord[0].key == "k"
                && b.context == KeyContext::Chat
        });

        assert_eq!(
            alt_m.and_then(|b| b.action.as_deref()),
            Some("openModelPicker")
        );
        assert_eq!(
            ctrl_k.and_then(|b| b.action.as_deref()),
            Some("openCommandPalette")
        );
    }

    #[test]
    fn test_free_upstream_bindings_on_alt_j_k_and_u() {
        let bindings = default_bindings();
        let find = |key: &str| {
            bindings.iter().find(|b| {
                b.chord.len() == 1
                    && b.chord[0].alt
                    && b.chord[0].key == key
                    && b.context == KeyContext::Chat
            })
        };
        // Alt+J/K open the free-model dropdown; Alt+U kept as forward-cycle
        // alias.
        assert_eq!(
            find("j").and_then(|b| b.action.as_deref()),
            Some("openFreeModelPopup")
        );
        assert_eq!(
            find("k").and_then(|b| b.action.as_deref()),
            Some("openFreeModelPopup")
        );
        assert_eq!(
            find("u").and_then(|b| b.action.as_deref()),
            Some("cycleFreeUpstream")
        );
    }

    #[test]
    fn test_mode_picker_binding_on_alt_shift_m() {
        // Alt+Shift+M opens the mode quick-pick (Alt+M is the model picker,
        // so the mode picker is the shifted twin).
        let bindings = default_bindings();
        let binding = bindings.iter().find(|b| {
            b.chord.len() == 1
                && b.chord[0].alt
                && b.chord[0].shift
                && b.chord[0].key == "m"
                && b.context == KeyContext::Chat
        });
        assert_eq!(
            binding.and_then(|b| b.action.as_deref()),
            Some("openModePicker")
        );
    }

    #[test]
    fn test_effort_step_bindings_on_alt_h_and_alt_l() {
        let bindings = default_bindings();
        let find = |key: &str| {
            bindings.iter().find(|b| {
                b.chord.len() == 1
                    && b.chord[0].alt
                    && b.chord[0].key == key
                    && b.context == KeyContext::Chat
            })
        };
        // Alt+H steps reasoning down, Alt+L steps it up (clamped, never wraps).
        assert_eq!(
            find("h").and_then(|b| b.action.as_deref()),
            Some("effortDecrease")
        );
        assert_eq!(
            find("l").and_then(|b| b.action.as_deref()),
            Some("effortIncrease")
        );
    }

    #[test]
    fn test_shifted_vertical_navigation_aliases_are_registered() {
        let bindings = default_bindings();
        for context in [
            KeyContext::Chat,
            KeyContext::Help,
            KeyContext::HistorySearch,
            KeyContext::Confirmation,
            KeyContext::MessageSelector,
            KeyContext::ThemePicker,
            KeyContext::Task,
            KeyContext::DiffDialog,
            KeyContext::Select,
            KeyContext::Settings,
        ] {
            assert!(bindings.iter().any(|binding| {
                binding.context == context
                    && binding.chord == parse_chord("shift+k").unwrap()
                    && binding.action.as_deref() == Some("verticalPrev")
            }));
            assert!(bindings.iter().any(|binding| {
                binding.context == context
                    && binding.chord == parse_chord("shift+j").unwrap()
                    && binding.action.as_deref() == Some("verticalNext")
            }));
        }
    }

    #[test]
    fn test_resolve_single_preserves_user_unbinding() {
        let user = UserKeybindings {
            bindings: vec![UserBinding {
                chord: "shift+j".to_string(),
                action: None,
                context: Some("Chat".to_string()),
            }],
            ..UserKeybindings::default()
        };
        let resolver = KeybindingResolver::new(&user);
        let key = parse_keystroke("shift+j").unwrap();
        assert!(matches!(
            resolver.resolve_single(&key, &KeyContext::Chat),
            Some(KeybindingResult::Unbound)
        ));
    }

    #[test]
    fn test_resolver_simple_action() {
        let user = UserKeybindings::default();
        let mut resolver = KeybindingResolver::new(&user);
        // ctrl+l is a single-chord Global binding ("redraw"); ctrl+c is no
        // longer resolver-routed (see test_default_bindings_omit_ctrl_c_and_ctrl_d).
        let ks = parse_keystroke("ctrl+l").unwrap();
        let result = resolver.process(ks, &KeyContext::Global);
        assert!(matches!(result, KeybindingResult::Action(ref a) if a == "redraw"));
    }

    #[test]
    fn test_resolver_no_match() {
        let user = UserKeybindings::default();
        let mut resolver = KeybindingResolver::new(&user);
        // ctrl+z has no default binding
        let ks = parse_keystroke("ctrl+z").unwrap();
        let result = resolver.process(ks, &KeyContext::Chat);
        assert!(matches!(result, KeybindingResult::NoMatch));
    }

    #[test]
    fn test_resolver_context_match_global_from_chat() {
        let user = UserKeybindings::default();
        let mut resolver = KeybindingResolver::new(&user);
        // ctrl+l in Chat context maps to "clearLine" (newly added Phase 1 keybinding)
        // Global context is checked after context-specific bindings
        let ks = parse_keystroke("ctrl+l").unwrap();
        let result = resolver.process(ks, &KeyContext::Chat);
        assert!(matches!(result, KeybindingResult::Action(ref a) if a == "clearLine"));
    }

    #[test]
    fn test_keystroke_equality() {
        let ks1 = parse_keystroke("ctrl+enter").unwrap();
        let ks2 = parse_keystroke("ctrl+enter").unwrap();
        let ks3 = parse_keystroke("shift+enter").unwrap();
        assert_eq!(ks1, ks2);
        assert_ne!(ks1, ks3);
    }

    #[test]
    fn test_user_keybindings_default_empty() {
        let user = UserKeybindings::default();
        assert!(user.bindings.is_empty());
    }

    #[test]
    fn test_user_keybindings_supports_ts_block_format() {
        let user = UserKeybindings::from_json_str(
            r#"{
  "bindings": [
    {
      "context": "Chat",
      "bindings": {
        "ctrl+g": "chat:externalEditor",
        "space": null
      }
    }
  ]
}"#,
        );

        assert_eq!(user.bindings.len(), 2);
        assert_eq!(user.bindings[0].context.as_deref(), Some("Chat"));
        assert_eq!(user.bindings[0].chord, "ctrl+g");
        assert_eq!(
            user.bindings[0].action.as_deref(),
            Some("chat:externalEditor")
        );
        assert_eq!(user.bindings[1].chord, "space");
        assert_eq!(user.bindings[1].action, None);
    }

    #[test]
    fn test_ctrl_j_maps_to_newline() {
        let bindings = default_bindings();
        let ctrl_j = bindings.iter().find(|b| {
            b.chord.len() == 1
                && b.chord[0].ctrl
                && b.chord[0].key == "j"
                && b.context == KeyContext::Chat
        });
        assert!(ctrl_j.is_some(), "ctrl+j binding not found");
        assert_eq!(ctrl_j.unwrap().action.as_deref(), Some("newline"));
    }

    #[test]
    fn test_shift_enter_and_alt_enter_map_to_newline() {
        // The multi-line composing fallbacks (#224): Shift+Enter (kitty),
        // plus Alt+Enter and Ctrl+J for terminals that can't distinguish
        // Shift+Enter from a bare Enter.
        let bindings = default_bindings();
        let find = |ctrl: bool, alt: bool, shift: bool, key: &str| {
            bindings
                .iter()
                .find(|b| {
                    b.chord.len() == 1
                        && b.chord[0].ctrl == ctrl
                        && b.chord[0].alt == alt
                        && b.chord[0].shift == shift
                        && b.chord[0].key == key
                        && b.context == KeyContext::Chat
                })
                .and_then(|b| b.action.as_deref())
        };
        assert_eq!(
            find(false, false, true, "enter"),
            Some("newline"),
            "shift+enter"
        );
        assert_eq!(
            find(false, true, false, "enter"),
            Some("newline"),
            "alt+enter"
        );
        // And a bare Enter still submits.
        assert_eq!(find(false, false, false, "enter"), Some("submit"), "enter");
    }

    #[test]
    fn test_new_phase1_keybindings_registered() {
        // Verify that all Phase 1 keybindings are registered
        let bindings = default_bindings();

        // Build list of keybinding actions
        let actions: Vec<String> = bindings.iter().filter_map(|b| b.action.clone()).collect();

        // Check Phase 1 keybinding actions exist
        assert!(
            actions.contains(&"clearLine".to_string()),
            "clearLine action not found"
        );
        assert!(
            actions.contains(&"submit".to_string()),
            "submit action not found"
        );
        assert!(
            actions.contains(&"jumpToNextError".to_string()),
            "jumpToNextError action not found"
        );
        assert!(
            actions.contains(&"jumpToPreviousError".to_string()),
            "jumpToPreviousError action not found"
        );
        assert!(
            actions.contains(&"previousMessage".to_string()),
            "previousMessage action not found"
        );
        assert!(
            actions.contains(&"nextMessage".to_string()),
            "nextMessage action not found"
        );
        assert!(
            actions.contains(&"openHelp".to_string()),
            "openHelp action not found"
        );
        assert!(
            actions.contains(&"deleteCharBefore".to_string()),
            "deleteCharBefore action not found"
        );
        assert!(
            actions.contains(&"reverseIndent".to_string()),
            "reverseIndent action not found"
        );

        // Verify we have at least 10 new keybindings (Phase 1 requirement)
        assert!(
            actions.len() >= 40,
            "Expected at least 40 keybindings, found {}",
            actions.len()
        );
    }

    #[test]
    fn test_old_format_keybindings_get_upgraded() {
        let old_format_json = r#"{
            "bindings": [
                {
                    "context": "Chat",
                    "bindings": {
                        "ctrl+shift+a": "openModelPicker",
                        "ctrl+e": "goLineEnd"
                    }
                }
            ]
        }"#;

        let mut kb = UserKeybindings::from_json_str(old_format_json);
        assert_eq!(kb.schema_version, 0, "Old format should start at version 0");

        kb.smart_merge_with_defaults();

        assert_eq!(kb.schema_version, 1, "Should be upgraded to version 1");
        assert!(
            kb.bindings.iter().any(|b| b.chord == "meta+left"),
            "meta+left (cmd+left) should be added from defaults after merge"
        );
        assert!(
            kb.bindings.iter().any(
                |b| b.chord == "ctrl+shift+a" && b.action.as_deref() == Some("openModelPicker")
            ),
            "User customization (ctrl+shift+a -> openModelPicker) should be preserved"
        );
    }

    #[test]
    fn test_preset_default_matches_stock_table() {
        // The Default preset must be byte-for-byte the stock table so existing
        // users see no behaviour change.
        let stock = default_bindings();
        let preset = preset_bindings(&KeybindingPreset::Default);
        assert_eq!(stock.len(), preset.len());
        for (a, b) in stock.iter().zip(preset.iter()) {
            assert_eq!(a.chord, b.chord);
            assert_eq!(a.action, b.action);
            assert_eq!(a.context, b.context);
        }
    }

    #[test]
    fn test_vim_preset_adds_hjkl_navigation() {
        let bindings = preset_bindings(&KeybindingPreset::Vim);
        // h/l prev/next in Select context
        let h = bindings.iter().find(|b| {
            b.context == KeyContext::Select && b.chord.len() == 1 && b.chord[0].key == "h"
        });
        let l = bindings.iter().find(|b| {
            b.context == KeyContext::Select && b.chord.len() == 1 && b.chord[0].key == "l"
        });
        assert_eq!(h.and_then(|b| b.action.as_deref()), Some("prev"));
        assert_eq!(l.and_then(|b| b.action.as_deref()), Some("next"));
        // Vim extras must only add single-key chords: `gg`/`G` would be
        // dead config because the transcript context is never the active
        // resolver context.  (Default bindings may include multi-key chords
        // like `tab h` / `tab l` for effort stepping.)
        for (chord_str, _action, _ctx) in VIM_PRESET_EXTRAS {
            let chord = parse_chord(chord_str).expect("vim extra must parse");
            assert_eq!(
                chord.len(),
                1,
                "vim preset extra '{}' must be a single-key chord",
                chord_str
            );
        }
        // No vim bindings may bind a bare letter in Chat context (letters must
        // keep typing into the prompt).  Stock defaults may bind unmodified
        // non-letter keys (up/down/home/enter/tab) in Chat, so only flag
        // single lowercase-letter chords.
        let bare_letter_in_chat = |b: &ParsedBinding| {
            b.context == KeyContext::Chat
                && b.chord.len() == 1
                && !b.chord[0].ctrl
                && !b.chord[0].alt
                && !b.chord[0].meta
                && !b.chord[0].shift
                && b.chord[0].key.len() == 1
                && b.chord[0]
                    .key
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_lowercase())
        };
        assert!(
            !bindings.iter().any(bare_letter_in_chat),
            "vim preset must not bind bare letters in Chat context"
        );
    }

    #[test]
    fn test_emacs_preset_adds_readline_chords() {
        let bindings = preset_bindings(&KeybindingPreset::Emacs);
        // Last match wins (the resolver uses exact.last()), so take the LAST
        // Chat-context binding for each chord — that is what actually fires.
        let find = |key: &str, ctrl: bool, alt: bool| {
            bindings
                .iter()
                .filter(|b| {
                    b.context == KeyContext::Chat
                        && b.chord.len() == 1
                        && b.chord[0].key == key
                        && b.chord[0].ctrl == ctrl
                        && b.chord[0].alt == alt
                        && !b.chord[0].shift
                })
                .next_back()
                .and_then(|b| b.action.as_deref())
        };
        assert_eq!(find("b", true, false), Some("moveCharBackward"));
        assert_eq!(find("f", true, false), Some("moveCharForward"));
        assert_eq!(find("p", true, false), Some("historyPrev"));
        assert_eq!(find("n", true, false), Some("historyNext"));
        assert_eq!(find("k", true, false), Some("killToEnd"));
        assert_eq!(find("y", true, false), Some("yank"));
        assert_eq!(find("b", false, true), Some("moveWordBackward"));
        assert_eq!(find("f", false, true), Some("moveWordForward"));
        // Ctrl+K kill-line must override the stock openCommandPalette.
        let ctrl_k = bindings
            .iter()
            .filter(|b| {
                b.context == KeyContext::Chat
                    && b.chord.len() == 1
                    && b.chord[0].key == "k"
                    && b.chord[0].ctrl
            })
            .next_back()
            .expect("emacs ctrl+k")
            .action
            .as_deref()
            .unwrap();
        assert_eq!(ctrl_k, "killToEnd");
        // Command palette stays reachable via ctrl+shift+p.
        let ctrl_shift_p = bindings
            .iter()
            .filter(|b| {
                b.context == KeyContext::Chat
                    && b.chord.len() == 1
                    && b.chord[0].key == "p"
                    && b.chord[0].ctrl
                    && b.chord[0].shift
            })
            .next_back()
            .expect("emacs ctrl+shift+p")
            .action
            .as_deref()
            .unwrap();
        assert_eq!(ctrl_shift_p, "openCommandPalette");
    }

    #[test]
    fn test_resolver_respects_user_preset() {
        let user = UserKeybindings {
            schema_version: KEYBINDINGS_SCHEMA_VERSION,
            preset: KeybindingPreset::Emacs,
            bindings: Vec::new(),
        };
        let mut resolver = KeybindingResolver::new(&user);
        let ks = parse_keystroke("ctrl+b").unwrap();
        let result = resolver.process(ks.clone(), &KeyContext::Chat);
        assert!(matches!(result, KeybindingResult::Action(ref a) if a == "moveCharBackward"));

        // With the Default preset, Alt+B resolves to the Chat-context
        // binding (moveWordBackward), not the Global one (createBranch).
        let default_user = UserKeybindings::default();
        let mut default_resolver = KeybindingResolver::new(&default_user);
        let alt_b_ks = parse_keystroke("alt+b").unwrap();
        let result = default_resolver.process(alt_b_ks, &KeyContext::Chat);
        assert!(matches!(result, KeybindingResult::Action(ref a) if a == "moveWordBackward"));
    }

    #[test]
    fn test_preset_serialisation_round_trip() {
        let user = UserKeybindings {
            schema_version: KEYBINDINGS_SCHEMA_VERSION,
            preset: KeybindingPreset::Vim,
            bindings: Vec::new(),
        };
        let json = serde_json::to_string(&user).unwrap();
        let parsed: UserKeybindings = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.preset, KeybindingPreset::Vim);
    }

    #[test]
    fn test_preset_from_name() {
        assert_eq!(
            KeybindingPreset::from_name("vim"),
            Some(KeybindingPreset::Vim)
        );
        assert_eq!(
            KeybindingPreset::from_name("Vim"),
            Some(KeybindingPreset::Vim)
        );
        assert_eq!(
            KeybindingPreset::from_name("emacs"),
            Some(KeybindingPreset::Emacs)
        );
        assert_eq!(
            KeybindingPreset::from_name("default"),
            Some(KeybindingPreset::Default)
        );
        assert_eq!(KeybindingPreset::from_name("nope"), None);
    }

    #[test]
    fn test_old_format_keybindings_get_upgraded_with_default_preset() {
        let old_format_json = r#"{
            "bindings": [
                {
                    "context": "Chat",
                    "bindings": {
                        "ctrl+shift+a": "openModelPicker",
                        "ctrl+e": "goLineEnd"
                    }
                }
            ]
        }"#;
        let mut kb = UserKeybindings::from_json_str(old_format_json);
        assert_eq!(kb.preset, KeybindingPreset::Default);
        kb.smart_merge_with_defaults();
        assert_eq!(kb.preset, KeybindingPreset::Default);
    }

    #[test]
    fn test_cmd_left_resolves_to_go_line_start() {
        let user = UserKeybindings::default();
        let mut resolver = KeybindingResolver::new(&user);

        // Create a keystroke for CMD+Left (SUPER modifier + left arrow)
        let keystroke = ParsedKeystroke {
            key: "left".to_string(),
            ctrl: false,
            alt: false,
            shift: false,
            meta: true,
        };

        let result = resolver.process(keystroke, &KeyContext::Chat);
        match result {
            KeybindingResult::Action(action) => {
                assert_eq!(action, "goLineStart", "CMD+Left should map to goLineStart");
            }
            other => panic!("Expected Action(\"goLineStart\"), got {:?}", other),
        }
    }

    // ---- Tab+H / Tab+L chord tests ----

    fn ks(key: &str) -> ParsedKeystroke {
        ParsedKeystroke {
            key: key.to_string(),
            ctrl: false,
            alt: false,
            shift: false,
            meta: false,
        }
    }

    #[test]
    fn test_tab_h_chord_resolves_to_effort_decrease() {
        let user = UserKeybindings::default();
        let mut resolver = KeybindingResolver::new(&user);
        // Tab pressed → PendingSingle("indent")
        match resolver.process(ks("tab"), &KeyContext::Chat) {
            KeybindingResult::PendingSingle(action) => assert_eq!(action, "indent"),
            other => panic!("Expected PendingSingle, got {:?}", other),
        }
        // H completes the chord → effortDecrease
        match resolver.process(ks("h"), &KeyContext::Chat) {
            KeybindingResult::Action(action) => assert_eq!(action, "effortDecrease"),
            other => panic!("Expected Action(effortDecrease), got {:?}", other),
        }
    }

    #[test]
    fn test_tab_l_chord_resolves_to_effort_increase() {
        let user = UserKeybindings::default();
        let mut resolver = KeybindingResolver::new(&user);
        match resolver.process(ks("tab"), &KeyContext::Chat) {
            KeybindingResult::PendingSingle(_) => {}
            other => panic!("Expected PendingSingle, got {:?}", other),
        }
        match resolver.process(ks("l"), &KeyContext::Chat) {
            KeybindingResult::Action(action) => assert_eq!(action, "effortIncrease"),
            other => panic!("Expected Action(effortIncrease), got {:?}", other),
        }
    }

    #[test]
    fn test_tab_followed_by_unrelated_key_fires_indent() {
        let user = UserKeybindings::default();
        let mut resolver = KeybindingResolver::new(&user);
        match resolver.process(ks("tab"), &KeyContext::Chat) {
            KeybindingResult::PendingSingle(_) => {}
            other => panic!("Expected PendingSingle, got {:?}", other),
        }
        // Pressing 'a' doesn't complete any chord → fires held indent action
        match resolver.process(ks("a"), &KeyContext::Chat) {
            KeybindingResult::Action(action) => assert_eq!(action, "indent"),
            other => panic!("Expected Action(indent), got {:?}", other),
        }
    }

    #[test]
    fn test_tab_timeout_fires_indent() {
        let user = UserKeybindings::default();
        let mut resolver = KeybindingResolver::new(&user);
        match resolver.process(ks("tab"), &KeyContext::Chat) {
            KeybindingResult::PendingSingle(_) => {}
            other => panic!("Expected PendingSingle, got {:?}", other),
        }
        // Simulate timeout by manually advancing the started time
        resolver.pending_single_started =
            Some(Instant::now() - Duration::from_millis(CHORD_TIMEOUT_MS + 1));
        let action = resolver.check_timeout().expect("timeout should fire");
        assert_eq!(action, "indent");
    }

    #[test]
    fn test_cancel_chord_clears_held_single_action() {
        let user = UserKeybindings::default();
        let mut resolver = KeybindingResolver::new(&user);
        match resolver.process(ks("tab"), &KeyContext::Chat) {
            KeybindingResult::PendingSingle(_) => {}
            other => panic!("Expected PendingSingle, got {:?}", other),
        }
        // Cancelling the chord (e.g. the Tab-with-suggestions bypass) must
        // also drop the held action so it cannot fire on a later keystroke.
        resolver.cancel_chord();
        assert!(resolver.pending_single_action.is_none());
        assert!(resolver.pending_single_started.is_none());
        assert!(!resolver.has_pending_chord());
        assert!(resolver.check_timeout().is_none());
        // The next keystroke is processed fresh and never fires the held
        // indent action.
        let result = resolver.process(ks("h"), &KeyContext::Chat);
        assert!(!matches!(result, KeybindingResult::Action(action) if action == "indent"));
    }

    #[test]
    fn test_alt_h_still_works_as_effort_decrease() {
        let user = UserKeybindings::default();
        let mut resolver = KeybindingResolver::new(&user);
        let ks = ParsedKeystroke {
            key: "h".to_string(),
            ctrl: false,
            alt: true,
            shift: false,
            meta: false,
        };
        match resolver.process(ks, &KeyContext::Chat) {
            KeybindingResult::Action(action) => assert_eq!(action, "effortDecrease"),
            other => panic!("Expected Action(effortDecrease), got {:?}", other),
        }
    }
}
