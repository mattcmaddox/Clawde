//! Shared hierarchical slash-command metadata.
//!
//! The command implementations remain in `clawde-commands`, but this small
//! route table lives in core so the TUI can offer the same nested paths without
//! depending on the commands crate (which already depends on the TUI).

/// A discoverable nested slash-command path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HierarchicalCommand {
    /// Path without the leading slash, for example `"provider connect"`.
    pub path: &'static str,
    /// Short text shown beside the completion.
    pub description: &'static str,
    /// Existing flat command that ultimately handles this route.
    pub target: &'static str,
}

/// Metadata for a flat slash command exposed by prompt completion and TUI discovery.
///
/// The command implementation remains in `clawde-commands`; keeping this small
/// presentation registry in core lets the TUI and prompt editor share one list
/// without introducing a dependency cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub category: &'static str,
    /// True when the command is handled by the interactive TUI and has no
    /// headless `SlashCommand` implementation.
    pub tui_only: bool,
}

/// Flat slash commands that are intentionally retained as compatibility
/// commands while users migrate to the hierarchical families below.
pub const PROMPT_COMMANDS: &[PromptCommand] = &[
    PromptCommand { name: "advisor", description: "Set or unset the server-side advisor model", category: "Commands", tui_only: false },
    PromptCommand { name: "agent", description: "List available agents or show agent details", category: "Tools", tui_only: false },
    PromptCommand { name: "agents", description: "Browse agent definitions and active agents", category: "Tools", tui_only: false },
    PromptCommand { name: "new-agent", description: "Create a new sub-agent in the editor", category: "Tools", tui_only: false },
    PromptCommand { name: "changes", description: "Inspect changes from the current session", category: "Review & History", tui_only: true },
    PromptCommand { name: "clear", description: "Clear the conversation transcript", category: "Session", tui_only: false },
    PromptCommand { name: "compact", description: "Compact the conversation context", category: "Session", tui_only: false },
    PromptCommand { name: "config", description: "Open settings", category: "Workspace", tui_only: false },
    PromptCommand { name: "connect", description: "Connect an AI provider", category: "Model & Provider", tui_only: false },
    PromptCommand { name: "context", description: "Show context window and rate limit usage", category: "Diagnostics", tui_only: false },
    PromptCommand { name: "compare", description: "Compare free-model upstream performance and health", category: "Model & Provider", tui_only: false },
    PromptCommand { name: "copy", description: "Copy the last assistant response to clipboard", category: "Review & History", tui_only: false },
    PromptCommand { name: "cost", description: "Show cost breakdown", category: "Diagnostics", tui_only: false },
    PromptCommand { name: "diff", description: "Inspect the current git diff", category: "Review & History", tui_only: false },
    PromptCommand { name: "doctor", description: "Run diagnostics", category: "Diagnostics", tui_only: false },
    PromptCommand { name: "effort", description: "Set effort level (low/medium/high/max)", category: "Model & Provider", tui_only: false },
    PromptCommand { name: "exit", description: "Quit Clawde", category: "Session", tui_only: false },
    PromptCommand { name: "export", description: "Export conversation", category: "Review & History", tui_only: false },
    PromptCommand { name: "fast", description: "Toggle fast mode", category: "Model & Provider", tui_only: false },
    PromptCommand { name: "fork", description: "Fork session into a new branch", category: "Session", tui_only: false },
    PromptCommand { name: "goal", description: "Set or view the current session goal", category: "Commands", tui_only: false },
    PromptCommand { name: "heapdump", description: "Show process memory and diagnostic information", category: "Diagnostics", tui_only: false },
    PromptCommand { name: "health", description: "Probe free-mode key health — /health [<upstream>]", category: "Diagnostics", tui_only: false },
    PromptCommand { name: "help", description: "Show help", category: "Commands", tui_only: false },
    PromptCommand { name: "history", description: "Show recent sessions for this project and where history lives", category: "Session", tui_only: false },
    PromptCommand { name: "hooks", description: "Browse configured hooks (read-only)", category: "Workspace", tui_only: false },
    PromptCommand { name: "image", description: "Switch to a vision-capable model for image processing", category: "Commands", tui_only: false },
    PromptCommand { name: "import-config", description: "Import CLAUDE.md and settings.json from ~/.claude", category: "Workspace", tui_only: false },
    PromptCommand { name: "init", description: "Initialize AGENTS.md for this project", category: "Commands", tui_only: false },
    PromptCommand { name: "insights", description: "Generate a session analysis report with conversation statistics", category: "Diagnostics", tui_only: false },
    PromptCommand { name: "keybindings", description: "Show keybinding configuration", category: "Workspace", tui_only: false },
    PromptCommand { name: "links", description: "Open URLs from this session in your browser", category: "Review & History", tui_only: false },
    PromptCommand { name: "login", description: "Log in to Clawde", category: "Commands", tui_only: false },
    PromptCommand { name: "logout", description: "Log out of Clawde", category: "Commands", tui_only: false },
    PromptCommand { name: "managed-agents", description: "Configure manager-executor managed agent system", category: "Commands", tui_only: false },
    PromptCommand { name: "mcp", description: "Browse configured MCP servers", category: "Workspace", tui_only: false },
    PromptCommand { name: "memory", description: "Browse and open AGENTS.md memory files", category: "Tools", tui_only: false },
    PromptCommand { name: "model", description: "Change the AI model", category: "Model & Provider", tui_only: false },
    PromptCommand { name: "models", description: "Browse free upstream models", category: "Model & Provider", tui_only: false },
    PromptCommand { name: "task", description: "Set free-model sort: /task <name> (all/coding/reasoning/creative/fast/multimodal/long-context)", category: "Model & Provider", tui_only: false },
    PromptCommand { name: "move", description: "Re-home this session to another worktree of the same project", category: "Session", tui_only: false },
    PromptCommand { name: "new", description: "Start a fresh session (keeps model, provider & directory)", category: "Session", tui_only: false },
    PromptCommand { name: "output-style", description: "Show or switch the output style / persona", category: "Commands", tui_only: false },
    PromptCommand { name: "plugin", description: "Manage plugins (list/info/enable/disable/reload)", category: "Tools", tui_only: false },
    PromptCommand { name: "providers", description: "List available AI providers and their status", category: "Model & Provider", tui_only: false },
    PromptCommand { name: "caveman", description: "Caveman persona output style — save big token", category: "Commands", tui_only: false },
    PromptCommand { name: "rocky", description: "Rocky persona output style — amaze amaze amaze", category: "Commands", tui_only: false },
    PromptCommand { name: "normal", description: "Reset persona / output style to default", category: "Commands", tui_only: false },
    PromptCommand { name: "quit", description: "Exit Clawde", category: "Session", tui_only: false },
    PromptCommand { name: "refresh", description: "Clear saved provider auth and model caches", category: "Model & Provider", tui_only: false },
    PromptCommand { name: "rename", description: "Rename this session", category: "Session", tui_only: false },
    PromptCommand { name: "resume", description: "Resume a previous session", category: "Session", tui_only: false },
    PromptCommand { name: "review", description: "Review changes (git diff)", category: "Review & History", tui_only: false },
    PromptCommand { name: "rewind", description: "Rewind to an earlier turn", category: "Review & History", tui_only: false },
    PromptCommand { name: "rustle", description: "Edit the Rustle mascot animation frames", category: "Tools", tui_only: true },
    PromptCommand { name: "session", description: "Browse and manage sessions", category: "Session", tui_only: false },
    PromptCommand { name: "settings", description: "Open settings", category: "Workspace", tui_only: false },
    PromptCommand { name: "share", description: "Upload the current session as a secret gist and get a shareable URL", category: "Review & History", tui_only: false },
    PromptCommand { name: "stats", description: "Open token and cost stats", category: "Diagnostics", tui_only: false },
    PromptCommand { name: "survey", description: "Open session feedback survey", category: "Tools", tui_only: true },
    PromptCommand { name: "theme", description: "Open the theme picker", category: "Workspace", tui_only: false },
    PromptCommand { name: "ultrareview", description: "Run an exhaustive multi-dimensional code review", category: "Commands", tui_only: false },
    PromptCommand { name: "update", description: "Check for updates and upgrade to the latest version", category: "Commands", tui_only: false },
    PromptCommand { name: "upgrade", description: "Check for updates and upgrade to the latest version", category: "Commands", tui_only: false },
    PromptCommand { name: "vim", description: "Toggle vim keybindings", category: "Commands", tui_only: false },
    PromptCommand { name: "voice", description: "Toggle voice input mode", category: "Model & Provider", tui_only: false },
    PromptCommand { name: "ollama", description: "Toggle Ollama connectivity mode (auto / isolated)", category: "Model & Provider", tui_only: false },
];

/// Return the flat registry in the shape consumed by prompt typeahead.
///
/// The derived pair list is initialized once because the TUI refreshes prompt
/// suggestions on every keystroke.
pub fn prompt_command_pairs() -> &'static [(&'static str, &'static str)] {
    static PAIRS: std::sync::OnceLock<Vec<(&'static str, &'static str)>> =
        std::sync::OnceLock::new();
    PAIRS
        .get_or_init(|| {
            PROMPT_COMMANDS
                .iter()
                .map(|command| (command.name, command.description))
                .collect()
        })
        .as_slice()
}

/// Return the shared category for a flat compatibility command.
pub fn prompt_command_category(name: &str) -> &'static str {
    PROMPT_COMMANDS
        .iter()
        .find(|command| command.name == name)
        .map(|command| command.category)
        .unwrap_or("Commands")
}

/// The shape of arguments accepted after a hierarchical command leaf.
///
/// This is deliberately metadata-only for now: execution remains in the
/// existing flat command implementation, while help and completion can reason
/// about the route without parsing ad-hoc strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HierarchicalArgument {
    None,
    FreeText,
    Enum(&'static [&'static str]),
}

impl HierarchicalCommand {
    /// Human-facing family/category label used by help and the command palette.
    pub fn category(self) -> &'static str {
        match self.path {
            path if path.starts_with("config ") => "Configuration",
            path if path.starts_with("provider ") => "Providers",
            path if path.starts_with("model ") => "Models & Routing",
            path if path.starts_with("session ") => "Sessions",
            path if path.starts_with("project ") => "Project",
            path if path.starts_with("context ") => "Context",
            path if path.starts_with("system ") => "System",
            path if path.starts_with("integrations ") => "Integrations",
            _ => "Commands",
        }
    }

    /// Typed argument metadata for this route.
    pub fn argument_kind(self) -> HierarchicalArgument {
        match self.path {
            "provider login" | "provider switch" | "provider keys" => {
                HierarchicalArgument::FreeText
            }
            "provider health" | "provider compare" => HierarchicalArgument::FreeText,
            "model use" | "model compare" => HierarchicalArgument::FreeText,
            "model routing" => {
                HierarchicalArgument::Enum(&["auto", "sequential", "random", "latency", "task"])
            }
            "model task" => HierarchicalArgument::Enum(&[
                "all",
                "coding",
                "reasoning",
                "creative",
                "fast",
                "multimodal",
                "long-context",
            ]),
            "context auto-compact" => HierarchicalArgument::Enum(&["on", "off"]),
            _ => HierarchicalArgument::None,
        }
    }
}

/// Validate the route table's structural invariants.
pub fn validate_hierarchy() -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    let mut paths = std::collections::HashSet::new();
    for route in HIERARCHICAL_COMMANDS {
        if route.path.split_whitespace().count() != 2 {
            errors.push(format!(
                "route '{}' must contain exactly two segments",
                route.path
            ));
        }
        if route.target.trim().is_empty() {
            errors.push(format!("route '{}' has an empty target", route.path));
        }
        if !paths.insert(route.path) {
            errors.push(format!("duplicate route '{}'", route.path));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// The first additive command-family migration.
///
/// These paths deliberately point at existing flat commands. Removing or
/// renaming the flat commands is a separate compatibility decision.
pub const HIERARCHICAL_COMMANDS: &[HierarchicalCommand] = &[
    HierarchicalCommand {
        path: "config theme",
        description: "Choose the terminal theme",
        target: "theme",
    },
    HierarchicalCommand {
        path: "config color",
        description: "Choose or edit terminal colors",
        target: "color",
    },
    HierarchicalCommand {
        path: "config editor",
        description: "Toggle the Vim-style editor",
        target: "vim",
    },
    HierarchicalCommand {
        path: "config keybindings",
        description: "View or edit keybindings",
        target: "keybindings",
    },
    HierarchicalCommand {
        path: "config output-style",
        description: "Choose the output style",
        target: "output-style",
    },
    HierarchicalCommand {
        path: "config privacy",
        description: "Open privacy settings",
        target: "privacy-settings",
    },
    HierarchicalCommand {
        path: "provider list",
        description: "List providers and their status",
        target: "providers",
    },
    HierarchicalCommand {
        path: "provider connect",
        description: "Connect a provider",
        target: "connect",
    },
    HierarchicalCommand {
        path: "provider login",
        description: "Log in to a provider",
        target: "login",
    },
    HierarchicalCommand {
        path: "provider logout",
        description: "Log out of the active provider",
        target: "logout",
    },
    HierarchicalCommand {
        path: "provider accounts",
        description: "Manage provider accounts",
        target: "accounts",
    },
    HierarchicalCommand {
        path: "provider switch",
        description: "Switch the active account",
        target: "switch",
    },
    HierarchicalCommand {
        path: "provider keys",
        description: "Manage provider API keys",
        target: "keys",
    },
    HierarchicalCommand {
        path: "provider limits",
        description: "Show provider limits",
        target: "limits",
    },
    HierarchicalCommand {
        path: "provider health",
        description: "Check free-provider health",
        target: "health",
    },
    HierarchicalCommand {
        path: "provider compare",
        description: "Compare free-model upstream performance",
        target: "compare",
    },
    HierarchicalCommand {
        path: "provider refresh",
        description: "Refresh provider state",
        target: "refresh",
    },
    HierarchicalCommand {
        path: "model list",
        description: "Browse available models",
        target: "models",
    },
    HierarchicalCommand {
        path: "model use",
        description: "Select a model",
        target: "model",
    },
    HierarchicalCommand {
        path: "model routing",
        description: "Configure smart routing",
        target: "routing",
    },
    HierarchicalCommand {
        path: "model compare",
        description: "Compare free-model upstream performance",
        target: "compare",
    },
    HierarchicalCommand {
        path: "model task",
        description: "Choose the free-model task lane",
        target: "task",
    },
    HierarchicalCommand {
        path: "session list",
        description: "Browse saved sessions",
        target: "session",
    },
    HierarchicalCommand {
        path: "session resume",
        description: "Resume a saved session",
        target: "resume",
    },
    HierarchicalCommand {
        path: "session rename",
        description: "Rename the current session",
        target: "rename",
    },
    HierarchicalCommand {
        path: "session fork",
        description: "Fork the current session",
        target: "fork",
    },
    HierarchicalCommand {
        path: "session history",
        description: "Search session history",
        target: "history",
    },
    HierarchicalCommand {
        path: "session rewind",
        description: "Rewind to an earlier turn",
        target: "rewind",
    },
    HierarchicalCommand {
        path: "project init",
        description: "Initialize project instructions",
        target: "init",
    },
    HierarchicalCommand {
        path: "project diff",
        description: "Inspect the current diff",
        target: "diff",
    },
    HierarchicalCommand {
        path: "project review",
        description: "Review project changes",
        target: "review",
    },
    HierarchicalCommand {
        path: "project verify",
        description: "Run project verification",
        target: "verify",
    },
    HierarchicalCommand {
        path: "project memory",
        description: "Browse project memory files",
        target: "memory",
    },
    HierarchicalCommand {
        path: "context show",
        description: "Show context usage",
        target: "context",
    },
    HierarchicalCommand {
        path: "context compact",
        description: "Compact conversation context",
        target: "compact",
    },
    HierarchicalCommand {
        path: "context auto-compact",
        description: "Toggle automatic compaction",
        target: "auto-compact",
    },
    HierarchicalCommand {
        path: "context memory",
        description: "Configure project memory",
        target: "memory",
    },
    HierarchicalCommand {
        path: "system status",
        description: "Show system status",
        target: "status",
    },
    HierarchicalCommand {
        path: "system doctor",
        description: "Run diagnostics",
        target: "doctor",
    },
    HierarchicalCommand {
        path: "system version",
        description: "Show version information",
        target: "version",
    },
    HierarchicalCommand {
        path: "system update",
        description: "Check for updates",
        target: "update",
    },
    HierarchicalCommand {
        path: "integrations mcp",
        description: "Manage MCP servers",
        target: "mcp",
    },
    HierarchicalCommand {
        path: "integrations hooks",
        description: "Browse configured hooks",
        target: "hooks",
    },
    HierarchicalCommand {
        path: "integrations ide",
        description: "Manage IDE integration",
        target: "ide",
    },
    HierarchicalCommand {
        path: "integrations chrome",
        description: "Manage browser/computer-use integration",
        target: "chrome",
    },
    HierarchicalCommand {
        path: "integrations plugins",
        description: "Manage installed plugins",
        target: "plugin",
    },
    HierarchicalCommand {
        path: "integrations skills",
        description: "Browse available skills",
        target: "skills",
    },
    HierarchicalCommand {
        path: "project commit",
        description: "Create a commit from the current changes",
        target: "commit",
    },
    HierarchicalCommand {
        path: "project snapshot",
        description: "Inspect or restore project snapshots",
        target: "checkpoints",
    },
    HierarchicalCommand {
        path: "context summary",
        description: "Show or create a conversation summary",
        target: "summary",
    },
    HierarchicalCommand {
        path: "system health",
        description: "Check runtime and provider health",
        target: "health",
    },
];

/// Return the family roots that match an incomplete slash command.
///
/// Roots are derived from the same route table as child suggestions so the
/// TUI never needs a second list to keep in sync.
pub fn hierarchical_roots(input: &str) -> Vec<(&'static str, &'static str)> {
    let typed = input.strip_prefix('/').unwrap_or(input).trim_start();
    if typed.contains(char::is_whitespace) {
        return Vec::new();
    }

    let mut roots = Vec::new();
    for command in HIERARCHICAL_COMMANDS {
        let root = command.path.split(' ').next().unwrap_or_default();
        if root.starts_with(&typed.to_ascii_lowercase())
            && !roots.iter().any(|(existing, _)| *existing == root)
        {
            roots.push((
                root,
                match root {
                    "config" => "Configuration commands",
                    "provider" => "Provider and account commands",
                    "model" => "Model and routing commands",
                    "session" => "Session and history commands",
                    "project" => "Project commands",
                    "context" => "Context and memory commands",
                    "system" => "System and diagnostics commands",
                    "integrations" => "Integration commands",
                    _ => "Command family",
                },
            ));
        }
    }
    roots
}

/// Return nested paths that match the user's incomplete slash input.
///
/// Suggestions are only emitted after a root and a space have been typed, so
/// ordinary flat-command completion remains unchanged for `/co`, `/model`, etc.
pub fn hierarchical_completions(input: &str) -> Vec<HierarchicalCommand> {
    let typed = input.strip_prefix('/').unwrap_or(input).trim_start();
    let Some((root, child_prefix)) = typed.split_once(char::is_whitespace) else {
        return Vec::new();
    };
    let root = root.to_ascii_lowercase();
    let child_prefix = child_prefix.trim_start().to_ascii_lowercase();

    HIERARCHICAL_COMMANDS
        .iter()
        .copied()
        .filter(|command| {
            let mut parts = command.path.splitn(2, ' ');
            let command_root = parts.next().unwrap_or_default();
            let command_child = parts.next().unwrap_or_default();
            command_root == root && command_child.starts_with(&child_prefix)
        })
        .collect()
}

/// A parsed nested invocation with its existing flat target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedInvocation {
    pub target: &'static str,
    pub root: String,
    pub child: String,
    pub remainder: String,
}

/// Resolve a complete two-segment path to its existing flat target.
pub fn resolve_invocation(input: &str) -> Option<ResolvedInvocation> {
    let command_text = input.strip_prefix('/').unwrap_or(input).trim_start();
    let mut words = command_text.split_whitespace();
    let root_word = words.next()?;
    let child_word = words.next()?;
    let after_root = command_text.get(root_word.len()..)?.trim_start();
    let child_offset = after_root.find(child_word)?;
    let after_child = &after_root[child_offset + child_word.len()..];
    let root = root_word.to_ascii_lowercase();
    let child = child_word.to_ascii_lowercase();
    let target = HIERARCHICAL_COMMANDS.iter().find_map(|command| {
        command
            .path
            .split_once(' ')
            .filter(|(route_root, route_child)| *route_root == root && *route_child == child)
            .map(|_| command.target)
    })?;

    Some(ResolvedInvocation {
        target,
        root,
        child,
        remainder: after_child.trim_start().to_string(),
    })
}

/// Return the existing flat target for a complete nested path.
pub fn target_for_path(path: &str) -> Option<&'static str> {
    resolve_invocation(path).map(|resolved| resolved.target)
}

/// Return typed enum argument values for a route, filtered by prefix.
pub fn argument_completions(path: &str, partial: &str) -> Vec<&'static str> {
    let values = HIERARCHICAL_COMMANDS
        .iter()
        .find(|route| route.path == path)
        .map(|route| route.argument_kind())
        .and_then(|kind| match kind {
            HierarchicalArgument::Enum(values) => Some(values),
            _ => None,
        })
        .unwrap_or(&[]);
    let partial = partial.to_ascii_lowercase();
    values
        .iter()
        .copied()
        .filter(|value| value.starts_with(&partial))
        .collect()
}

/// Rewrite a hierarchical invocation to the existing flat command target.
///
/// For example, `/provider connect groq` becomes `/connect groq`; arguments
/// after the two path segments are retained for the existing handler.
pub fn normalize_invocation(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') || trimmed.starts_with("//") {
        return None;
    }
    let resolved = resolve_invocation(trimmed)?;

    if resolved.remainder.is_empty() {
        Some(format!("/{}", resolved.target))
    } else {
        Some(format!("/{} {}", resolved.target, resolved.remainder))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_registry_exposes_flat_commands_and_categories() {
        assert!(PROMPT_COMMANDS
            .iter()
            .any(|command| command.name == "connect"));
        assert_eq!(prompt_command_category("connect"), "Model & Provider");
        assert_eq!(prompt_command_category("unknown"), "Commands");
        assert!(prompt_command_pairs()
            .iter()
            .any(|(name, _)| *name == "help"));
    }

    #[test]
    fn root_completion_lists_families() {
        let roots = hierarchical_roots("/prov");
        assert_eq!(roots, vec![("provider", "Provider and account commands")]);
    }

    #[test]
    fn nested_completion_lists_children() {
        let values: Vec<&str> = hierarchical_completions("/provider ")
            .iter()
            .map(|command| command.path)
            .collect();
        assert!(values.contains(&"provider connect"));
        assert!(values.contains(&"provider health"));
        assert!(!values.contains(&"config theme"));
    }

    #[test]
    fn nested_completion_filters_child_prefix_case_insensitively() {
        let values: Vec<&str> = hierarchical_completions("/MoDeL R")
            .iter()
            .map(|command| command.path)
            .collect();
        assert_eq!(values, vec!["model routing"]);
    }

    #[test]
    fn normalizes_nested_invocation_and_preserves_args() {
        assert_eq!(
            normalize_invocation("/provider connect groq"),
            Some("/connect groq".to_string())
        );
        assert_eq!(
            normalize_invocation("/config theme dark"),
            Some("/theme dark".to_string())
        );
    }

    #[test]
    fn normalizes_multiple_spaces_and_preserves_quoted_args() {
        assert_eq!(
            normalize_invocation("/provider   connect   \"free tier\""),
            Some("/connect \"free tier\"".to_string())
        );
        let resolved = resolve_invocation("/provider   connect   \"free tier\"").unwrap();
        assert_eq!(resolved.root, "provider");
        assert_eq!(resolved.child, "connect");
        assert_eq!(resolved.remainder, "\"free tier\"");
    }

    #[test]
    fn typed_argument_completions_filter_enum_metadata() {
        assert_eq!(
            argument_completions("model routing", "seq"),
            vec!["sequential"]
        );
        assert_eq!(
            argument_completions("context auto-compact", ""),
            vec!["on", "off"]
        );
    }

    #[test]
    fn does_not_normalize_flat_or_unknown_commands() {
        assert_eq!(normalize_invocation("/model gpt-4o"), None);
        assert_eq!(normalize_invocation("/provider unknown"), None);
        assert_eq!(normalize_invocation("hello"), None);
    }
}
