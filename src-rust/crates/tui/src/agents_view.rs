//! Agent / coordinator progress views for the TUI.
//! Mirrors src/components/agents/ (13 files).

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};
use std::path::{Path, PathBuf};
use unicode_width::UnicodeWidthStr;

use crate::overlays::{
    begin_modal_buf, modal_header_line_area, render_modal_title_buf, CLAWDE_ACCENT, CLAWDE_MUTED,
    CLAWDE_PANEL_BG, CLAWDE_TEXT,
};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// The role of an agent in the manager-executor architecture.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum AgentRole {
    #[default]
    Normal,
    Manager,
    Executor {
        parent_id: String,
    },
}

/// The current status of a sub-agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Idle,
    Running,
    WaitingForTool,
    Complete,
    Failed,
}

impl AgentStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::WaitingForTool => "waiting",
            Self::Complete => "done",
            Self::Failed => "failed",
        }
    }

    pub fn color(&self) -> Color {
        match self {
            Self::Idle => Color::DarkGray,
            Self::Running => Color::Green,
            Self::WaitingForTool => Color::Yellow,
            Self::Complete => Color::Cyan,
            Self::Failed => Color::Red,
        }
    }
}

/// A sub-agent or coordinator instance.
#[derive(Debug, Clone)]
pub struct AgentInfo {
    /// Unique agent ID.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Current status.
    pub status: AgentStatus,
    /// Current tool being executed (if any).
    pub current_tool: Option<String>,
    /// Number of turns completed.
    pub turns_completed: u32,
    /// Is this the coordinator?
    pub is_coordinator: bool,
    /// Brief description or last output snippet.
    pub last_output: Option<String>,
    /// Role in the managed agent architecture.
    #[allow(dead_code)]
    pub agent_role: AgentRole,
    /// Model name used by this agent.
    pub model_name: Option<String>,
    /// Cost in USD accumulated by this agent.
    pub cost_usd: f64,
}
/// One agent definition, from any of the three stores the popup manages: the
/// built-in defaults, the `agents` map in `settings.json`, or a markdown file
/// under `<config dir>/agents/` or `<project>/.clawde/agents/`.
///
/// [`load_agent_definitions`] merges all three and resolves name collisions by
/// precedence. A row with `shadowed_by` set is still listed and still editable
/// (editing it rewrites its own file), but activation resolves to the winner.
#[derive(Debug, Clone)]
pub struct AgentDefinition {
    /// Backing markdown file. `None` for built-in and settings-declared agents.
    pub file_path: Option<PathBuf>,
    /// Agent name.
    pub name: String,
    /// Where the definition came from: `"project"` | `"user"` | `"settings"` |
    /// `"builtin"`. Precedence follows that order, highest first.
    pub source: String,
    /// Model name.
    pub model: Option<String>,
    /// Memory scope.
    pub memory_scope: Option<String>,
    /// Description.
    pub description: String,
    /// Tool list (empty = all tools).
    pub tools: Vec<String>,
    /// If a higher-precedence definition overrides this one.
    pub shadowed_by: Option<String>,
    /// Markdown body / instructions.
    pub instructions: String,
    /// Access tier. Only built-in and settings-declared agents carry one; a
    /// markdown agent runs with full access.
    pub access: Option<String>,
    /// Per-agent turn cap (built-in / settings only).
    pub max_turns: Option<u32>,
    /// Per-agent temperature (built-in / settings only).
    pub temperature: Option<f64>,
    /// Accent colour (built-in / settings only).
    pub color: Option<String>,
    /// Settings-declared agents may opt out of agent listings. Defaults to
    /// `true` for every other source.
    pub visible: bool,
}

impl AgentDefinition {
    /// Name of the source, as the popup and `/agent` print it.
    pub fn source_label(&self) -> &'static str {
        match self.source.as_str() {
            "builtin" => "built-in",
            "settings" => "settings.json",
            "project" => ".clawde/agents",
            "user" => "~/.clawde/agents",
            _ => "agent",
        }
    }

    /// Access tier used when this agent is activated. Markdown agents run with
    /// full access; built-in and settings agents carry an explicit tier.
    pub fn access_tier(&self) -> &str {
        self.access.as_deref().unwrap_or("full")
    }

    /// Precedence rank. Higher wins a name collision.
    fn rank(&self) -> u8 {
        match self.source.as_str() {
            "project" => 3,
            "user" => 2,
            "settings" => 1,
            _ => 0,
        }
    }

    /// The runtime definition activation installs. A markdown agent has no
    /// access tier or turn cap, so it becomes a full-access agent whose body is
    /// the system-prompt prefix.
    pub fn to_core(&self) -> clawde_core::AgentDefinition {
        clawde_core::AgentDefinition {
            description: (!self.description.is_empty()).then(|| self.description.clone()),
            model: self.model.clone(),
            temperature: self.temperature,
            prompt: (!self.instructions.is_empty()).then(|| self.instructions.clone()),
            access: self.access_tier().to_string(),
            visible: self.visible,
            max_turns: self.max_turns,
            color: self.color.clone(),
        }
    }
}

/// The metadata rows shared by the popup's detail screen and `/agent <name>`'s
/// text output, so the two projections cannot describe an agent differently.
pub fn agent_detail_fields(def: &AgentDefinition) -> Vec<(&'static str, String)> {
    let mut rows = vec![
        ("Name", def.name.clone()),
        ("Source", def.source_label().to_string()),
        (
            "Model",
            def.model.clone().unwrap_or_else(|| "default".to_string()),
        ),
        ("Access", def.access_tier().to_string()),
    ];
    if let Some(turns) = def.max_turns {
        rows.push(("Max turns", turns.to_string()));
    }
    if let Some(memory) = &def.memory_scope {
        rows.push(("Memory", memory.clone()));
    }
    rows.push((
        "Tools",
        if def.tools.is_empty() {
            "All tools".to_string()
        } else {
            def.tools.join(", ")
        },
    ));
    rows
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentEditorField {
    Name,
    Model,
    Memory,
    Tools,
    Description,
    Prompt,
}

impl AgentEditorField {
    pub fn next(self) -> Self {
        match self {
            Self::Name => Self::Model,
            Self::Model => Self::Memory,
            Self::Memory => Self::Tools,
            Self::Tools => Self::Description,
            Self::Description => Self::Prompt,
            Self::Prompt => Self::Name,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::Name => Self::Prompt,
            Self::Model => Self::Name,
            Self::Memory => Self::Model,
            Self::Tools => Self::Memory,
            Self::Description => Self::Tools,
            Self::Prompt => Self::Description,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentEditorState {
    pub original_index: Option<usize>,
    pub name: String,
    pub model: String,
    pub memory_scope: String,
    pub tools: String,
    pub description: String,
    pub prompt: String,
    pub selected_field: AgentEditorField,
    pub error: Option<String>,
    pub saved_message: Option<String>,
}

impl Default for AgentEditorState {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentEditorState {
    pub fn new() -> Self {
        Self {
            original_index: None,
            name: String::new(),
            model: "claude-sonnet-4-6".to_string(),
            memory_scope: String::new(),
            tools: String::new(),
            description: String::new(),
            prompt: String::new(),
            selected_field: AgentEditorField::Name,
            error: None,
            saved_message: None,
        }
    }

    pub fn from_definition(def: Option<(usize, &AgentDefinition)>) -> Self {
        match def {
            Some((idx, def)) => Self {
                original_index: Some(idx),
                name: def.name.clone(),
                model: def
                    .model
                    .clone()
                    .unwrap_or_else(|| "claude-sonnet-4-6".to_string()),
                memory_scope: def.memory_scope.clone().unwrap_or_default(),
                tools: def.tools.join(", "),
                description: def.description.clone(),
                prompt: def.instructions.clone(),
                selected_field: AgentEditorField::Name,
                error: None,
                saved_message: None,
            },
            None => Self::new(),
        }
    }

    fn selected_text_mut(&mut self) -> &mut String {
        match self.selected_field {
            AgentEditorField::Name => &mut self.name,
            AgentEditorField::Model => &mut self.model,
            AgentEditorField::Memory => &mut self.memory_scope,
            AgentEditorField::Tools => &mut self.tools,
            AgentEditorField::Description => &mut self.description,
            AgentEditorField::Prompt => &mut self.prompt,
        }
    }
}

// ---------------------------------------------------------------------------
// Screen routes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentsRoute {
    List,
    Detail(usize),         // index into definitions
    Editor(Option<usize>), // None = create new
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Full state for the agents menu overlay.
#[derive(Debug, Clone)]
pub struct AgentsMenuState {
    pub visible: bool,
    pub route: AgentsRoute,
    pub definitions: Vec<AgentDefinition>,
    pub active_agents: Vec<AgentInfo>,
    pub selected_row: usize,
    pub project_root: Option<PathBuf>,
    pub editor: AgentEditorState,
    /// The `agents` map from `settings.json`, kept so a save or delete can
    /// reload the full merged list without the caller re-supplying it.
    pub settings_agents: std::collections::HashMap<String, clawde_core::AgentDefinition>,
    /// Index of the definition awaiting delete confirmation, if any.
    pub pending_delete: Option<usize>,
}

impl AgentsMenuState {
    pub fn new() -> Self {
        Self {
            visible: false,
            route: AgentsRoute::List,
            definitions: Vec::new(),
            active_agents: Vec::new(),
            selected_row: 0,
            project_root: None,
            editor: AgentEditorState::new(),
            settings_agents: std::collections::HashMap::new(),
            pending_delete: None,
        }
    }

    pub fn open(
        &mut self,
        project_root: &std::path::Path,
        settings_agents: &std::collections::HashMap<String, clawde_core::AgentDefinition>,
    ) {
        self.project_root = Some(project_root.to_path_buf());
        self.settings_agents = settings_agents.clone();
        self.reload();
        self.selected_row = 0;
        self.route = AgentsRoute::List;
        self.pending_delete = None;
        self.visible = true;
    }

    /// Re-read every agent store. Used after a save or delete so the merged
    /// list (and its shadowing) reflects what just hit the filesystem.
    pub fn reload(&mut self) {
        self.definitions = match &self.project_root {
            Some(root) => load_agent_definitions(root, &self.settings_agents),
            None => Vec::new(),
        };
    }

    /// Select the definition for `name` and open its detail screen. Returns
    /// `false` when no definition carries that name (shadowed rows included).
    pub fn select_by_name(&mut self, name: &str) -> bool {
        match self.definitions.iter().position(|def| def.name == name) {
            Some(idx) => {
                self.selected_row = idx + 1;
                self.route = AgentsRoute::Detail(idx);
                true
            }
            None => false,
        }
    }

    pub fn close(&mut self) {
        self.visible = false;
        self.pending_delete = None;
    }

    pub fn select_prev(&mut self) {
        let row_count = self.definitions.len() + 1;
        if row_count == 0 {
            return;
        }
        if self.selected_row == 0 {
            self.selected_row = row_count - 1;
        } else {
            self.selected_row -= 1;
        }
    }

    pub fn select_next(&mut self) {
        let row_count = self.definitions.len() + 1;
        if row_count == 0 {
            return;
        }
        self.selected_row = (self.selected_row + 1) % row_count;
    }

    pub fn confirm_selection(&mut self) {
        match self.route {
            AgentsRoute::List => {
                if self.selected_row == 0 {
                    self.open_editor(None);
                } else {
                    let idx = self.selected_row - 1;
                    if idx < self.definitions.len() {
                        self.route = AgentsRoute::Detail(idx);
                    }
                }
            }
            AgentsRoute::Detail(idx) => self.open_editor(Some(idx)),
            AgentsRoute::Editor(_) => {}
        }
    }

    pub fn go_back(&mut self) {
        match &self.route {
            AgentsRoute::Detail(_) | AgentsRoute::Editor(_) => {
                self.route = AgentsRoute::List;
            }
            AgentsRoute::List => {
                self.close();
            }
        }
    }

    pub fn open_editor(&mut self, idx: Option<usize>) {
        self.editor = AgentEditorState::from_definition(
            idx.and_then(|index| self.definitions.get(index).map(|def| (index, def))),
        );
        self.route = AgentsRoute::Editor(idx);
    }

    pub fn editor_insert_char(&mut self, ch: char) {
        let field = self.editor.selected_text_mut();
        field.push(ch);
        self.editor.error = None;
        self.editor.saved_message = None;
    }

    pub fn editor_backspace(&mut self) {
        self.editor.selected_text_mut().pop();
    }

    pub fn editor_insert_newline(&mut self) {
        match self.editor.selected_field {
            AgentEditorField::Description | AgentEditorField::Prompt => {
                self.editor.selected_text_mut().push('\n');
            }
            _ => self.editor.selected_field = self.editor.selected_field.next(),
        }
    }

    pub fn editor_next_field(&mut self) {
        self.editor.selected_field = self.editor.selected_field.next();
    }

    pub fn editor_prev_field(&mut self) {
        self.editor.selected_field = self.editor.selected_field.prev();
    }

    /// Index of the definition under the cursor, if the route has one. Row 0 of
    /// the list is the "Create new agent" entry and has no definition.
    pub fn selected_index(&self) -> Option<usize> {
        match self.route {
            AgentsRoute::Detail(idx) => (idx < self.definitions.len()).then_some(idx),
            AgentsRoute::List => self
                .selected_row
                .checked_sub(1)
                .filter(|idx| *idx < self.definitions.len()),
            AgentsRoute::Editor(_) => None,
        }
    }

    /// The definition under the cursor, if the route has one.
    pub fn selected_definition(&self) -> Option<&AgentDefinition> {
        self.definitions.get(self.selected_index()?)
    }

    /// The definition activation should install: the winner for the selected
    /// name, so activating a shadowed row runs the definition that outranks it.
    pub fn activation_target(&self) -> Option<&AgentDefinition> {
        let name = &self.selected_definition()?.name;
        self.definitions
            .iter()
            .find(|def| def.name == *name && def.shadowed_by.is_none())
    }

    /// Ask for confirmation before deleting the agent on screen.
    ///
    /// Only a markdown-backed definition can be deleted: a built-in lives in
    /// the binary and a settings-declared agent lives in `settings.json`, so
    /// both need a different editing surface.
    pub fn request_delete(&mut self) -> Result<(), String> {
        let Some(def) = self.selected_definition() else {
            return Err("Select an agent before deleting it.".to_string());
        };
        if def.file_path.is_none() {
            return Err(match def.source.as_str() {
                "builtin" => format!(
                    "@{} is built in — save a project override instead of deleting it.",
                    def.name
                ),
                _ => format!(
                    "@{} is declared in settings.json — remove it there, not here.",
                    def.name
                ),
            });
        }
        self.pending_delete = Some(match self.route {
            AgentsRoute::Detail(idx) => idx,
            _ => return Err("Open an agent before deleting it.".to_string()),
        });
        Ok(())
    }

    pub fn cancel_delete(&mut self) {
        self.pending_delete = None;
    }

    pub fn confirm_delete(&mut self) -> Result<String, String> {
        let idx = self
            .pending_delete
            .take()
            .ok_or_else(|| "Nothing to delete.".to_string())?;
        let Some(def) = self.definitions.get(idx) else {
            return Err("Agent is no longer listed.".to_string());
        };
        let Some(path) = def.file_path.clone() else {
            return Err("That agent has no file to delete.".to_string());
        };
        let name = def.name.clone();
        std::fs::remove_file(&path)
            .map_err(|err| format!("Failed to delete {}: {}", path.display(), err))?;
        self.reload();
        self.route = AgentsRoute::List;
        // Keep the cursor near the removed row, clamped to the new list.
        self.selected_row = (idx + 1).min(self.definitions.len());
        Ok(format!("Deleted @{} ({})", name, path.display()))
    }

    pub fn save_editor(&mut self) -> Result<String, String> {
        validate_editor(&self.editor)?;
        let root = self
            .project_root
            .clone()
            .ok_or_else(|| "Project root is unavailable.".to_string())?;
        // A built-in or settings-declared agent has no file of its own: saving
        // writes a project override, which then outranks the definition it was
        // copied from (see `load_agent_definitions`).
        let file_path = self
            .editor
            .original_index
            .and_then(|idx| self.definitions.get(idx))
            .and_then(|def| def.file_path.clone())
            .unwrap_or_else(|| {
                root.join(".clawde")
                    .join("agents")
                    .join(format!("{}.md", slugify_agent_name(&self.editor.name)))
            });

        write_editor_to_disk(&file_path, &self.editor)?;
        self.reload();

        let saved_idx = self
            .definitions
            .iter()
            .position(|def| def.file_path.as_deref() == Some(file_path.as_path()))
            .unwrap_or(0);
        self.selected_row = saved_idx + 1;
        self.route = AgentsRoute::Detail(saved_idx);
        let msg = format!("Saved agent to {}", file_path.display());
        self.editor.saved_message = Some(msg.clone());
        self.editor.error = None;
        Ok(msg)
    }
}

impl Default for AgentsMenuState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Data loading
// ---------------------------------------------------------------------------

/// Load every agent definition, merged across the three stores and ordered by
/// name.
///
/// Precedence, highest first: project markdown (`<root>/.clawde/agents`), user
/// markdown (`<config dir>/agents`), the `agents` map in `settings.json`, and
/// the built-in defaults. A definition that loses a name collision keeps
/// `shadowed_by` set to the winner, so the popup lists it as overridden instead
/// of hiding it.
pub fn load_agent_definitions(
    project_root: &std::path::Path,
    settings_agents: &std::collections::HashMap<String, clawde_core::AgentDefinition>,
) -> Vec<AgentDefinition> {
    let mut defs: Vec<AgentDefinition> = Vec::new();

    for (name, def) in clawde_core::default_agents() {
        defs.push(AgentDefinition {
            file_path: None,
            name,
            source: "builtin".to_string(),
            model: def.model,
            memory_scope: None,
            description: def.description.unwrap_or_default(),
            tools: Vec::new(),
            shadowed_by: None,
            instructions: def.prompt.unwrap_or_default(),
            access: Some(def.access),
            max_turns: def.max_turns,
            temperature: def.temperature,
            color: def.color,
            visible: def.visible,
        });
    }

    let mut settings_names: Vec<&String> = settings_agents.keys().collect();
    settings_names.sort();
    for name in settings_names {
        let def = &settings_agents[name];
        defs.push(AgentDefinition {
            file_path: None,
            name: name.clone(),
            source: "settings".to_string(),
            model: def.model.clone(),
            memory_scope: None,
            description: def.description.clone().unwrap_or_default(),
            tools: Vec::new(),
            shadowed_by: None,
            instructions: def.prompt.clone().unwrap_or_default(),
            access: Some(def.access.clone()),
            max_turns: def.max_turns,
            temperature: def.temperature,
            color: def.color.clone(),
            visible: def.visible,
        });
    }

    // Global agents first, then project ones, so a same-named project file is
    // the definition the collision pass keeps.
    for (dir, source) in [
        (
            clawde_core::config::Settings::config_dir().join("agents"),
            "user",
        ),
        (project_root.join(".clawde").join("agents"), "project"),
    ] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.extension().is_some_and(|ext| ext == "md") {
                continue;
            }
            if let Some(def) = parse_agent_def(&path, source) {
                defs.push(def);
            }
        }
    }

    mark_shadowed(&mut defs);
    defs.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| b.rank().cmp(&a.rank())));
    defs
}

/// Flag every definition whose name is claimed by a higher-precedence one.
fn mark_shadowed(defs: &mut [AgentDefinition]) {
    let mut winners: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (idx, def) in defs.iter().enumerate() {
        match winners.get(&def.name).copied() {
            Some(current) if defs[current].rank() >= def.rank() => {}
            _ => {
                winners.insert(def.name.clone(), idx);
            }
        }
    }
    let shadowed: Vec<Option<String>> = (0..defs.len())
        .map(|idx| {
            let winner = winners[&defs[idx].name];
            (winner != idx)
                .then(|| format!("{} ({})", defs[winner].name, defs[winner].source_label()))
        })
        .collect();
    for (def, shadow) in defs.iter_mut().zip(shadowed) {
        def.shadowed_by = shadow;
    }
}

/// Resolve the definition activation installs for `name`.
///
/// `--agent`, the mode-cycle arm, `tools_for_agent_mode` and the popup's
/// activate action all resolve through here, so a markdown agent becomes
/// activatable exactly when the popup lists it as the winner.
pub fn resolve_agent(
    project_root: &std::path::Path,
    settings_agents: &std::collections::HashMap<String, clawde_core::AgentDefinition>,
    name: &str,
) -> Option<clawde_core::AgentDefinition> {
    load_agent_definitions(project_root, settings_agents)
        .into_iter()
        .find(|def| def.name == name && def.shadowed_by.is_none())
        .map(|def| def.to_core())
}

fn parse_agent_def(path: &std::path::Path, source: &str) -> Option<AgentDefinition> {
    let content = std::fs::read_to_string(path).ok()?;
    let stem = path.file_stem()?.to_string_lossy().to_string();

    let (name, model, memory, description, tools, instructions) =
        if let Some(after) = content.strip_prefix("---") {
            let end = after.find("\n---")?;
            let front = &after[..end];
            let body = after[end + 4..].trim().to_string();
            let name = extract_yaml_str(front, "name").unwrap_or_else(|| stem.clone());
            let model = extract_yaml_str(front, "model");
            let memory = extract_yaml_str(front, "memory_scope")
                .or_else(|| extract_yaml_str(front, "memory"));
            let desc = extract_yaml_str(front, "description").unwrap_or_default();
            let tools = extract_yaml_list(front, "tools");
            (name, model, memory, desc, tools, body)
        } else {
            (
                stem,
                None,
                None,
                content.lines().next().unwrap_or("").to_string(),
                vec![],
                content.trim().to_string(),
            )
        };

    Some(AgentDefinition {
        file_path: Some(path.to_path_buf()),
        name,
        source: source.to_string(),
        model,
        memory_scope: memory,
        description,
        tools,
        shadowed_by: None,
        instructions,
        // A markdown agent declares no runtime policy of its own: it runs
        // with full access and inherits the session's turn cap and model
        // unless the frontmatter names one.
        access: None,
        max_turns: None,
        temperature: None,
        color: None,
        visible: true,
    })
}

fn extract_yaml_str(front: &str, key: &str) -> Option<String> {
    for line in front.lines() {
        if let Some(rest) = line.strip_prefix(&format!("{key}:")) {
            return Some(rest.trim().trim_matches('"').trim_matches('\'').to_string());
        }
    }
    None
}

fn extract_yaml_list(front: &str, key: &str) -> Vec<String> {
    for line in front.lines() {
        if let Some(rest) = line.strip_prefix(&format!("{key}:")) {
            let rest = rest.trim().trim_matches('[').trim_matches(']');
            return rest
                .split(',')
                .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
    }
    Vec::new()
}

fn slugify_agent_name(name: &str) -> String {
    let mut slug = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if matches!(ch, ' ' | '-' | '_' | '.') && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_string()
}

fn validate_editor(editor: &AgentEditorState) -> Result<(), String> {
    let name = editor.name.trim();
    if name.is_empty() {
        return Err("Agent name is required.".to_string());
    }
    if slugify_agent_name(name).is_empty() {
        return Err("Agent name must contain letters or numbers.".to_string());
    }
    if editor.model.trim().is_empty() {
        return Err("Model is required.".to_string());
    }
    if editor.description.trim().is_empty() {
        return Err("Description is required.".to_string());
    }
    if editor.prompt.trim().is_empty() {
        return Err("Prompt body is required.".to_string());
    }
    Ok(())
}

fn serialize_editor(editor: &AgentEditorState) -> String {
    let tools = editor
        .tools
        .split(',')
        .map(|tool| tool.trim())
        .filter(|tool| !tool.is_empty())
        .map(|tool| format!("\"{}\"", tool))
        .collect::<Vec<_>>()
        .join(", ");

    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("name: {}\n", editor.name.trim()));
    out.push_str(&format!("model: {}\n", editor.model.trim()));
    if !editor.memory_scope.trim().is_empty() {
        out.push_str(&format!("memory_scope: {}\n", editor.memory_scope.trim()));
    }
    out.push_str(&format!("description: {}\n", editor.description.trim()));
    if !tools.is_empty() {
        out.push_str(&format!("tools: [{}]\n", tools));
    }
    out.push_str("---\n\n");
    out.push_str(editor.prompt.trim());
    out.push('\n');
    out
}

fn write_editor_to_disk(path: &Path, editor: &AgentEditorState) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Failed to create {}: {}", parent.display(), err))?;
    }
    std::fs::write(path, serialize_editor(editor))
        .map_err(|err| format!("Failed to write {}: {}", path.display(), err))
}

// ---------------------------------------------------------------------------
// Rendering: Agents Menu overlay
// ---------------------------------------------------------------------------

/// Render the agents menu overlay.
pub fn render_agents_menu(state: &AgentsMenuState, area: Rect, buf: &mut Buffer) {
    if !state.visible {
        return;
    }

    let layout = begin_modal_buf(buf, area, 92, 30, 2, 1);
    let deleting = state.pending_delete.is_some();
    let (title, subtitle, footer) = match &state.route {
        AgentsRoute::List => (
            "Agents".to_string(),
            format!(
                " {} active  ·  {} definitions",
                state.active_agents.len(),
                state.definitions.len()
            ),
            " enter open  ·  a activate  ·  esc close".to_string(),
        ),
        AgentsRoute::Detail(idx) => (
            state
                .definitions
                .get(*idx)
                .map(|def| def.name.clone())
                .unwrap_or_else(|| "Agent".to_string()),
            " Review configuration and prompt details.".to_string(),
            if deleting {
                " Delete this agent?  y confirm  ·  n cancel".to_string()
            } else {
                " enter edit  ·  a activate  ·  d delete  ·  esc back".to_string()
            },
        ),
        AgentsRoute::Editor(Some(_)) => (
            "Edit agent".to_string(),
            " Update metadata, tools, and prompt instructions.".to_string(),
            " tab move  ·  ctrl+s save  ·  esc back".to_string(),
        ),
        AgentsRoute::Editor(None) => (
            "Create agent".to_string(),
            " Define a new reusable agent for this workspace.".to_string(),
            " tab move  ·  ctrl+s save  ·  esc back".to_string(),
        ),
    };
    render_modal_title_buf(buf, layout.header_area, &title, "esc");
    if let Some(subtitle_area) = modal_header_line_area(layout.header_area, 1) {
        Paragraph::new(Line::from(vec![Span::styled(
            subtitle,
            Style::default().fg(CLAWDE_MUTED),
        )]))
        .render(subtitle_area, buf);
    }

    match &state.route {
        AgentsRoute::List => render_agents_list(state, layout.body_area, buf),
        AgentsRoute::Detail(idx) => {
            if let Some(def) = state.definitions.get(*idx) {
                render_agent_detail(def, layout.body_area, buf);
            }
        }
        AgentsRoute::Editor(Some(_idx)) => {
            render_agent_editor(state, layout.body_area, buf);
        }
        AgentsRoute::Editor(None) => {
            render_agent_editor(state, layout.body_area, buf);
        }
    }
    let footer_style = if deleting {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(CLAWDE_MUTED)
            .add_modifier(Modifier::ITALIC)
    };
    Paragraph::new(Line::from(vec![Span::styled(footer, footer_style)]))
        .render(layout.footer_area, buf);
}

fn render_agents_list(state: &AgentsMenuState, area: Rect, buf: &mut Buffer) {
    let mut lines = Vec::new();
    if !state.active_agents.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            " Active now",
            Style::default()
                .fg(CLAWDE_ACCENT)
                .add_modifier(Modifier::BOLD),
        )]));
        for agent in state.active_agents.iter().take(3) {
            lines.push(Line::from(vec![
                Span::styled(" ", Style::default().fg(CLAWDE_MUTED)),
                Span::styled(agent.name.clone(), Style::default().fg(CLAWDE_TEXT)),
                Span::styled(
                    format!("  {}", agent.status.label()),
                    Style::default().fg(agent.status.color()),
                ),
            ]));
        }
        lines.push(Line::from(""));
    }

    let create_selected = state.selected_row == 0;
    lines.push(agent_list_row(
        "[+ Create new agent]".to_string(),
        "Create a reusable workspace agent".to_string(),
        create_selected,
        area.width,
    ));
    lines.push(Line::from(""));

    // Keep the highlighted definition inside the viewport. `selected_row` is
    // offset by one because row 0 is the "Create new agent" entry, which is
    // always rendered above the scrollable definition list.
    let max_visible = (area.height as usize)
        .saturating_sub(lines.len() + 1)
        .max(1);
    let selected_def = state.selected_row.saturating_sub(1);
    let start = if selected_def < max_visible {
        0
    } else {
        selected_def + 1 - max_visible
    };
    let start = start.min(state.definitions.len().saturating_sub(max_visible));

    for (i, def) in state.definitions[start..].iter().enumerate() {
        if i >= max_visible {
            break;
        }
        let abs_idx = start + i;
        let selected = state.selected_row == abs_idx + 1;
        let model_str = def.model.as_deref().unwrap_or("default");
        let mut meta = format!("{}  ·  {}", model_str, def.source_label());
        if !def.visible {
            meta.push_str("  ·  hidden");
        }
        if def.shadowed_by.is_some() {
            meta.push_str("  ⚠");
        }
        lines.push(agent_list_row(def.name.clone(), meta, selected, area.width));
    }
    Paragraph::new(lines)
        .style(Style::default().bg(CLAWDE_PANEL_BG))
        .render(area, buf);
}

fn render_agent_detail(def: &AgentDefinition, area: Rect, buf: &mut Buffer) {
    let mut lines = Vec::new();
    // One field list for the popup and `/agent <name>`; see
    // `agent_detail_fields`.
    for (label, value) in agent_detail_fields(def) {
        let bold = label == "Name";
        let value_style = if bold {
            Style::default()
                .fg(CLAWDE_TEXT)
                .add_modifier(Modifier::BOLD)
        } else if label == "Tools" && value == "All tools" {
            Style::default().fg(CLAWDE_MUTED)
        } else {
            Style::default().fg(CLAWDE_TEXT)
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {label:<10} "), Style::default().fg(CLAWDE_MUTED)),
            Span::styled(value, value_style),
        ]));
    }
    lines.push(Line::default());
    lines.push(Line::from(vec![Span::styled(
        " Description",
        Style::default()
            .fg(CLAWDE_ACCENT)
            .add_modifier(Modifier::BOLD),
    )]));
    for line in def.description.lines() {
        lines.push(Line::from(vec![Span::raw(format!(" {}", line))]));
    }
    lines.push(Line::default());
    lines.push(Line::from(vec![Span::styled(
        " Prompt",
        Style::default()
            .fg(CLAWDE_ACCENT)
            .add_modifier(Modifier::BOLD),
    )]));
    for line in def.instructions.lines().take(8) {
        lines.push(Line::from(vec![Span::styled(
            format!(" {}", line),
            Style::default().fg(CLAWDE_TEXT),
        )]));
    }

    if let Some(shadow) = &def.shadowed_by {
        lines.push(Line::default());
        lines.push(Line::from(vec![Span::styled(
            format!("⚠ Shadowed by: {}", shadow),
            Style::default().fg(Color::Yellow),
        )]));
    }

    Paragraph::new(lines)
        .wrap(ratatui::widgets::Wrap { trim: false })
        .style(Style::default().bg(CLAWDE_PANEL_BG))
        .render(area, buf);
}

fn render_agent_editor(state: &AgentsMenuState, area: Rect, buf: &mut Buffer) {
    let editor = &state.editor;
    let selected_style = Style::default()
        .fg(Color::White)
        .bg(CLAWDE_ACCENT)
        .add_modifier(Modifier::BOLD);
    let normal_style = Style::default().fg(CLAWDE_TEXT);

    let field_style = |field: AgentEditorField| {
        if editor.selected_field == field {
            selected_style
        } else {
            normal_style
        }
    };

    let header = vec![
        render_editor_field("Name", &editor.name, field_style(AgentEditorField::Name)),
        render_editor_field("Model", &editor.model, field_style(AgentEditorField::Model)),
        render_editor_field(
            "Memory",
            &editor.memory_scope,
            field_style(AgentEditorField::Memory),
        ),
        render_editor_field("Tools", &editor.tools, field_style(AgentEditorField::Tools)),
        render_editor_field(
            "Description",
            &editor.description,
            field_style(AgentEditorField::Description),
        ),
        Line::default(),
        Line::from(vec![Span::styled(
            " Prompt",
            Style::default()
                .fg(CLAWDE_ACCENT)
                .add_modifier(Modifier::BOLD),
        )]),
    ];

    let prompt_style = field_style(AgentEditorField::Prompt);
    let prompt_lines: Vec<Line> = if editor.prompt.is_empty() {
        vec![Line::from(vec![Span::styled(
            "(empty)",
            prompt_style.add_modifier(Modifier::ITALIC),
        )])]
    } else {
        let width = area.width as usize;
        let mut out = Vec::new();
        for logical in editor.prompt.split('\n') {
            for chunk in wrap_preserving(logical, width) {
                out.push(Line::from(vec![Span::styled(chunk, prompt_style)]));
            }
        }
        out
    };

    let mut footer: Vec<Line> = vec![Line::default()];
    if let Some(msg) = editor.saved_message.as_ref() {
        footer.push(Line::from(vec![Span::styled(
            msg.clone(),
            Style::default().fg(Color::Green),
        )]));
    }
    if let Some(err) = editor.error.as_ref() {
        footer.push(Line::from(vec![Span::styled(
            err.clone(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )]));
    }

    // Pin the field header and the message footer; the prompt body is the only
    // part that grows, so it is the only part that scrolls. Showing the tail
    // keeps the text just typed in view.
    let avail = (area.height as usize).saturating_sub(header.len() + footer.len());
    let start = prompt_lines.len().saturating_sub(avail);
    let mut lines = header;
    lines.extend(prompt_lines.into_iter().skip(start));
    lines.extend(footer);

    Paragraph::new(lines)
        .style(Style::default().bg(CLAWDE_PANEL_BG))
        .render(area, buf);
}

/// Wrap one logical line to `width` display columns, preserving leading
/// whitespace (unlike `dialogs::word_wrap`, which collapses runs of spaces).
/// Used for the agent prompt, where indentation is meaningful.
fn wrap_preserving(line: &str, width: usize) -> Vec<String> {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if width == 0 || UnicodeWidthStr::width(line) <= width {
        return vec![line.to_string()];
    }
    let mut out = Vec::new();
    let mut rest = line;
    while UnicodeWidthStr::width(rest) > width {
        let mut w = 0usize;
        let mut cut = 0usize;
        let mut last_space: Option<usize> = None;
        for (idx, ch) in rest.char_indices() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if w + cw > width {
                break;
            }
            w += cw;
            cut = idx + ch.len_utf8();
            if ch == ' ' {
                last_space = Some(cut);
            }
        }
        if cut == 0 {
            // A single character wider than `width`: take it whole so the loop
            // always makes progress.
            cut = rest.chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        }
        let take = last_space.filter(|&s| s <= cut && s > 0).unwrap_or(cut);
        out.push(rest[..take].to_string());
        rest = &rest[take..];
    }
    if !rest.is_empty() {
        out.push(rest.to_string());
    }
    out
}

fn render_editor_field(label: &str, value: &str, value_style: Style) -> Line<'static> {
    let display = if value.is_empty() {
        "(empty)".to_string()
    } else {
        value.to_string()
    };
    Line::from(vec![
        Span::styled(format!(" {label:<10} "), Style::default().fg(CLAWDE_MUTED)),
        Span::styled(display, value_style),
    ])
}

fn agent_list_row(title: String, meta: String, selected: bool, width: u16) -> Line<'static> {
    let bg = if selected {
        CLAWDE_ACCENT
    } else {
        CLAWDE_PANEL_BG
    };
    let title_style = if selected {
        Style::default()
            .fg(Color::White)
            .bg(bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(CLAWDE_TEXT).bg(bg)
    };
    let meta_style = if selected {
        Style::default().fg(Color::Rgb(248, 220, 236)).bg(bg)
    } else {
        Style::default().fg(CLAWDE_MUTED).bg(bg)
    };
    let mut spans = vec![
        Span::styled(" ", Style::default().bg(bg)),
        Span::styled(title, title_style),
        Span::styled(format!("  {}", meta), meta_style),
    ];
    let used: usize = spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    let pad = width.saturating_sub(used as u16) as usize;
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
    }
    Line::from(spans)
}

// ---------------------------------------------------------------------------
// Rendering: Coordinator status inline widget
// ---------------------------------------------------------------------------

/// Render an inline coordinator + sub-agent status widget.
///
/// Shows: coordinator status, then each sub-agent with its current tool.
/// Suitable for embedding in the main TUI layout (e.g., below the message list).
pub fn render_coordinator_status(agents: &[AgentInfo], area: Rect, buf: &mut Buffer) {
    if agents.is_empty() {
        return;
    }

    Block::default()
        .title(" Active Agents ")
        .borders(Borders::TOP)
        .style(Style::default().fg(Color::DarkGray))
        .render(area, buf);

    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(1),
    };

    for (i, agent) in agents.iter().enumerate() {
        if i as u16 >= inner.height {
            break;
        }
        let y = inner.y + i as u16;
        let row_area = Rect {
            x: inner.x,
            y,
            width: inner.width,
            height: 1,
        };

        let (prefix, role_badge, role_color, indent) = match &agent.agent_role {
            AgentRole::Manager => ("● ", "[MGR]", Color::Magenta, ""),
            AgentRole::Executor { .. } => ("  ○ ", "[EXE]", Color::Cyan, "  "),
            AgentRole::Normal => {
                if agent.is_coordinator {
                    ("● ", "", Color::Green, "")
                } else {
                    ("  ○ ", "", Color::DarkGray, "  ")
                }
            }
        };
        let tool_str = agent
            .current_tool
            .as_deref()
            .map(|t| format!(" → {}", t))
            .unwrap_or_default();
        let model_str = agent
            .model_name
            .as_deref()
            .map(|m| format!(" ({})", m))
            .unwrap_or_default();
        let cost_str = if agent.cost_usd > 0.0 {
            format!("  ${:.4}", agent.cost_usd)
        } else {
            String::new()
        };

        let mut spans = vec![
            Span::styled(indent.to_string(), Style::default()),
            Span::styled(prefix, Style::default().fg(agent.status.color())),
        ];
        if !role_badge.is_empty() {
            spans.push(Span::styled(
                format!("{} ", role_badge),
                Style::default().fg(role_color).add_modifier(Modifier::BOLD),
            ));
        }
        spans.extend(vec![
            Span::styled(agent.name.clone(), Style::default().fg(Color::White)),
            Span::styled(model_str, Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!(" [{}]", agent.status.label()),
                Style::default().fg(agent.status.color()),
            ),
            Span::styled(
                format!(" {} turns", agent.turns_completed),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(tool_str, Style::default().fg(Color::Yellow)),
            Span::styled(cost_str, Style::default().fg(Color::DarkGray)),
        ]);

        let line = Line::from(spans);
        Paragraph::new(line).render(row_area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn make_definition(name: &str) -> AgentDefinition {
        AgentDefinition {
            file_path: Some(std::path::PathBuf::from(format!("/tmp/{name}.md"))),
            name: name.to_string(),
            source: "user".to_string(),
            model: Some("claude-sonnet-4-6".to_string()),
            memory_scope: None,
            description: "A test agent".to_string(),
            tools: Vec::new(),
            shadowed_by: None,
            instructions: "Do the thing.".to_string(),
            access: None,
            max_turns: None,
            temperature: None,
            color: None,
            visible: true,
        }
    }

    fn write_agent(root: &std::path::Path, name: &str, model: &str) -> std::path::PathBuf {
        let dir = root.join(".clawde").join("agents");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.md"));
        std::fs::write(
            &path,
            format!(
                "---\nname: {name}\nmodel: {model}\ndescription: probe agent\n---\n\nBody for {name}.\n"
            ),
        )
        .unwrap();
        path
    }

    /// Every store is merged, and a shared name is ordered project → settings →
    /// built-in so the winner is the first row.
    #[test]
    fn loader_orders_project_files_above_settings_and_builtins() {
        let temp = tempfile::tempdir().unwrap();
        let mut settings = std::collections::HashMap::new();
        settings.insert(
            "build".to_string(),
            clawde_core::AgentDefinition {
                description: Some("settings build".to_string()),
                access: "read-only".to_string(),
                ..Default::default()
            },
        );
        write_agent(temp.path(), "build", "probe/model");

        let defs = load_agent_definitions(temp.path(), &settings);
        let build: Vec<&AgentDefinition> = defs.iter().filter(|def| def.name == "build").collect();
        assert_eq!(build.len(), 3, "builtin + settings + project: {build:?}");
        assert_eq!(build[0].source, "project");
        assert!(build[0].shadowed_by.is_none());
        assert_eq!(build[0].model.as_deref(), Some("probe/model"));
        assert_eq!(build[0].access_tier(), "full");
        assert_eq!(build[1].source, "settings");
        assert_eq!(build[1].access_tier(), "read-only");
        assert_eq!(build[2].source, "builtin");
        assert!(
            build[1].shadowed_by.is_some() && build[2].shadowed_by.is_some(),
            "both losers must be flagged: {build:?}"
        );
    }

    #[test]
    fn saving_a_builtin_writes_a_project_override_that_shadows_it() {
        let temp = tempfile::tempdir().unwrap();
        let mut state = AgentsMenuState::new();
        state.open(temp.path(), &std::collections::HashMap::new());
        let idx = state
            .definitions
            .iter()
            .position(|def| def.name == "plan" && def.source == "builtin")
            .expect("built-in plan agent");
        state.route = AgentsRoute::Detail(idx);
        state.open_editor(Some(idx));
        state.editor.prompt = "Project plan body.".to_string();

        let msg = state.save_editor().expect("save override");
        assert!(msg.contains("plan.md"), "{msg}");

        let plan: Vec<&AgentDefinition> = state
            .definitions
            .iter()
            .filter(|def| def.name == "plan")
            .collect();
        assert_eq!(plan[0].source, "project");
        assert_eq!(plan[0].instructions, "Project plan body.");
        assert!(
            plan[1].shadowed_by.is_some(),
            "the built-in must be flagged as shadowed: {plan:?}"
        );
    }

    #[test]
    fn delete_refuses_builtin_and_settings_definitions() {
        let temp = tempfile::tempdir().unwrap();
        let mut settings = std::collections::HashMap::new();
        settings.insert(
            "settings-probe".to_string(),
            clawde_core::AgentDefinition::default(),
        );
        let mut state = AgentsMenuState::new();
        state.open(temp.path(), &settings);

        let builtin = state
            .definitions
            .iter()
            .position(|def| def.name == "explore")
            .expect("built-in explore agent");
        state.route = AgentsRoute::Detail(builtin);
        let err = state
            .request_delete()
            .expect_err("built-ins are not deletable");
        assert!(err.contains("built in"), "{err}");

        let from_settings = state
            .definitions
            .iter()
            .position(|def| def.name == "settings-probe")
            .expect("settings agent");
        state.route = AgentsRoute::Detail(from_settings);
        let err = state
            .request_delete()
            .expect_err("settings agents are not deletable");
        assert!(err.contains("settings.json"), "{err}");
        assert!(state.pending_delete.is_none());
    }

    #[test]
    fn delete_needs_confirmation_and_removes_the_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = write_agent(temp.path(), "delete-probe", "probe/model");
        let mut state = AgentsMenuState::new();
        state.open(temp.path(), &std::collections::HashMap::new());
        let idx = state
            .definitions
            .iter()
            .position(|def| def.name == "delete-probe")
            .expect("file agent");
        state.route = AgentsRoute::Detail(idx);

        state.request_delete().expect("arm the confirmation");
        assert_eq!(state.pending_delete, Some(idx));
        assert!(path.exists(), "arming must not delete anything");

        let msg = state.confirm_delete().expect("delete");
        assert!(msg.contains("delete-probe"), "{msg}");
        assert!(!path.exists());
        assert!(state
            .definitions
            .iter()
            .all(|def| def.name != "delete-probe"));
        assert_eq!(state.route, AgentsRoute::List);
    }

    #[test]
    fn resolve_agent_returns_the_winning_definition() {
        let temp = tempfile::tempdir().unwrap();
        write_agent(temp.path(), "resolver-probe", "probe/model");

        let def = resolve_agent(
            temp.path(),
            &std::collections::HashMap::new(),
            "resolver-probe",
        )
        .expect("file agent resolves");
        assert_eq!(def.access, "full");
        assert_eq!(def.model.as_deref(), Some("probe/model"));
        assert_eq!(def.prompt.as_deref(), Some("Body for resolver-probe."));
        assert!(resolve_agent(
            temp.path(),
            &std::collections::HashMap::new(),
            "no-such-agent"
        )
        .is_none());
    }

    #[test]
    fn agents_list_scrolls_to_selected_definition() {
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        let mut state = AgentsMenuState::new();
        state.visible = true;
        state.definitions = (0..40)
            .map(|i| make_definition(&format!("agent-{i:02}")))
            .collect();
        // The last definition is selected (`selected_row` is offset by the
        // "Create new agent" row).
        state.selected_row = state.definitions.len();
        terminal
            .draw(|frame| render_agents_menu(&state, frame.area(), frame.buffer_mut()))
            .unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .clone()
            .content()
            .iter()
            .map(|c| c.symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(
            content.contains("agent-39"),
            "selected definition must scroll into view: {content:?}"
        );
    }

    #[test]
    fn wrap_preserving_keeps_leading_indentation() {
        let wrapped = wrap_preserving("    indented long line that must wrap here", 20);
        assert!(wrapped.len() > 1, "long line must wrap");
        assert!(
            wrapped[0].starts_with("    "),
            "leading indentation must be preserved: {wrapped:?}"
        );
        for line in &wrapped {
            assert!(line.chars().count() <= 20, "line too wide: {line:?}");
        }
    }

    #[test]
    fn agent_editor_prompt_tail_stays_visible() {
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
        let mut state = AgentsMenuState::new();
        state.visible = true;
        state.open_editor(None);
        state.editor.prompt = format!("{}LONGPROMPTTAIL", "filler line\n".repeat(40));
        terminal
            .draw(|frame| render_agents_menu(&state, frame.area(), frame.buffer_mut()))
            .unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .clone()
            .content()
            .iter()
            .map(|c| c.symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(
            content.contains("LONGPROMPTTAIL"),
            "the editor must scroll to the prompt tail: {content:?}"
        );
    }
}
