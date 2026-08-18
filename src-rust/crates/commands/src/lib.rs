// clawde-commands: Slash command system for Clawde.
//
// This crate implements the /command framework that allows users to type
// commands like /help, /compact, /clear, /model, /config, /cost, etc.
// Each command is a struct implementing the `SlashCommand` trait.

use async_trait::async_trait;
use clawde_core::config::{Config, Settings, Theme};
use clawde_core::cost::CostTracker;
use clawde_core::types::{ContentBlock, Message};
use std::collections::BTreeMap;
#[allow(unused_imports)]
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Core trait
// ---------------------------------------------------------------------------

/// Context available to every slash command.
pub struct CommandContext {
    pub config: Config,
    pub cost_tracker: Arc<CostTracker>,
    pub messages: Vec<Message>,
    pub working_dir: std::path::PathBuf,
    pub session_id: String,
    pub session_title: Option<String>,
    /// Remote session URL set when a bridge connection is active.
    pub remote_session_url: Option<String>,
    // Note: config already contains hooks, mcp_servers, etc.
    /// Live MCP manager — present when servers are connected.
    pub mcp_manager: Option<Arc<clawde_mcp::McpManager>>,
    /// Optional callback for starting an MCP OAuth flow in the background.
    pub mcp_auth_runner: Option<Arc<dyn Fn(clawde_mcp::oauth::McpAuthSession) + Send + Sync>>,
    /// Live provider registry, when available. Lets commands such as
    /// `/keys health` surface runtime state (e.g. free-mode upstream
    /// empty-completion cooldowns, spec §6.3) that is not persisted to disk.
    pub provider_registry: Option<std::sync::Arc<clawde_api::ProviderRegistry>>,
    /// Test-only provider override. When set, commands that would otherwise
    /// build a provider from config (`provider_for_config`) use this instead,
    /// keeping unit tests hermetic (no network calls).
    pub test_provider: Option<std::sync::Arc<dyn clawde_api::LlmProvider>>,
    /// Current session thinking-effort override (None = provider/model default).
    /// Populated by the CLI runtime so auxiliary requests (spec/review/compact/
    /// summary/rename) inherit the same effort as the main loop.
    pub effort: Option<clawde_core::effort::EffortLevel>,
}

/// Session-scoped action requested by `/thinking`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingAction {
    /// Enable reasoning for the current session using the configured/default level.
    On,
    /// Disable reasoning for the current session where the provider supports it.
    Off,
    /// Report the current session/default state without changing anything.
    Status,
}

/// Result of running a slash command.
#[derive(Debug)]
pub enum CommandResult {
    /// Display a message to the user (does NOT go to the model).
    Message(String),
    /// Inject a message into the conversation as though the user typed it.
    UserMessage(String),
    /// Modify the configuration.
    ConfigChange(Config),
    /// Modify the configuration and show a specific status message.
    ConfigChangeMessage(Config, String),
    /// Change reasoning for the current session without persisting it.
    ThinkingChange(ThinkingAction),
    /// Trigger a background MCP OAuth flow and request runtime reconnect on success.
    McpAuthFlow {
        /// The configured MCP server name.
        server_name: String,
        /// The browser URL shown to the user while the background flow runs.
        auth_url: String,
        /// The local callback URL waiting for the OAuth redirect.
        redirect_uri: String,
    },
    /// Clear the conversation.
    ClearConversation,
    /// Replace the conversation with a specific message list (used by /rewind).
    SetMessages(Vec<Message>),
    /// Load a previously saved session into the live REPL.
    ResumeSession(clawde_core::history::ConversationSession),
    /// Update the current session title.
    RenameSession(String),
    /// Trigger the OAuth login flow (handled by the REPL in main.rs).
    /// The bool indicates whether to use Claude.ai auth (true) or Console auth (false).
    StartOAuthFlow(bool),
    /// Trigger the OAuth login flow for a specific provider with optional
    /// human-friendly label for the new account profile.
    ///
    /// `provider` is one of `clawde_core::accounts::PROVIDER_ANTHROPIC` or
    /// `PROVIDER_CODEX`. `login_with_claude_ai` is only meaningful for
    /// Anthropic.
    StartLoginForProvider {
        provider: String,
        login_with_claude_ai: bool,
        label: Option<String>,
    },
    /// Exit the REPL.
    Exit,
    /// No visible output.
    Silent,
    /// An error.
    Error(String),
    /// Open the rewind/message-selector overlay in the TUI.
    /// The TUI will call SetMessages when the user confirms.
    OpenRewindOverlay,
    /// Open the hooks configuration browser overlay in the TUI.
    /// Falls back to a text listing in non-TUI contexts.
    OpenHooksOverlay,
    /// Open the import-config overlay in the TUI.
    OpenImportConfigOverlay,
    /// Render a verification-round report as the boxed verify indicator
    /// (the same box the auto-verify loop draws after writing turns).
    Verify(clawde_query::VerifyReport),
    /// Clear saved provider auth, model selection, and model caches, then
    /// rebuild the live runtime state.
    RefreshProviderState,
    /// Start a fresh session (opencode's `/new`): reset to a blank home,
    /// preserving the current model/provider/effort selection and working
    /// directory. Lazy — the new session is only persisted on the first message.
    NewSession,
    /// Re-home the current session to another worktree/directory of the same
    /// project (opencode's `/move`). The git working-tree changes have already
    /// been relocated by the command; the CLI just repoints the live session.
    MoveSession {
        /// Absolute destination directory the session now lives in.
        destination: std::path::PathBuf,
        /// Whether uncommitted changes were carried across (for the status line).
        moved_changes: bool,
    },
}

/// A single argument completion for a slash command's inline typeahead.
///
/// Returned by [`SlashCommand::arg_completions`] so the prompt input can
/// offer a dropdown of valid arguments as the user types.
#[derive(Debug, Clone)]
pub struct ArgCompletion {
    /// The argument value (e.g. `"low"`, `"on"`, `"dark"`).
    pub value: String,
    /// Human-readable description (e.g. `"Quick, straightforward implementation"`).
    pub description: String,
    /// Whether this option is currently available (false → dimmed / disabled).
    pub available: bool,
}

/// Build a faded, non-selectable placeholder hint for the next argument when
/// it is a free-form value that cannot be completed — API keys, file paths,
/// model IDs, labels.
///
/// Commands call this from `arg_completions` when the next required argument
/// is expected but not yet typed, so the Tab/Space popup tells the user what
/// goes next instead of showing nothing. The hint is never selectable and is
/// hidden once the user starts typing the value (`value_already_typed`), so it
/// reads as temporary guidance rather than an echoed prefix. `prefix` is the
/// command text typed so far (e.g. `"set firecrawl"`); `placeholder` is the
/// `<...>` token shown in the popup (e.g. `"<api-key>"`).
pub(crate) fn free_form_arg_hint(
    prefix: &str,
    placeholder: &str,
    description: &str,
    value_already_typed: bool,
) -> Option<ArgCompletion> {
    if value_already_typed {
        return None;
    }
    Some(ArgCompletion {
        value: format!("{prefix} {placeholder}"),
        description: description.to_string(),
        available: false,
    })
}

/// Every slash command implements this trait.
#[async_trait]
pub trait SlashCommand: Send + Sync {
    /// The primary name (without the leading `/`).
    fn name(&self) -> &str;
    /// Alias names (e.g. `["h"]` for `/help`).
    fn aliases(&self) -> Vec<&str> {
        vec![]
    }
    /// One-line description for /help.
    fn description(&self) -> &str;
    /// Detailed help text (shown by `/help <command>`).
    fn help(&self) -> &str {
        self.description()
    }
    /// Whether this command is visible in /help output.
    fn hidden(&self) -> bool {
        false
    }
    /// Return argument completions for inline typeahead after the command name.
    ///
    /// Called when the user has typed the full command name followed by a space
    /// and (optionally) a partial argument.  The returned completions are shown
    /// in the typeahead popup; any whose [`ArgCompletion::available`] is `false`
    /// are rendered dimmed and cannot be selected.
    ///
    /// The default implementation returns an empty list (no arg completions).
    fn arg_completions(&self, _partial: &str) -> Vec<ArgCompletion> {
        vec![]
    }
    /// Execute the command with the given arguments string.
    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult;
}

fn stripped_model_for_provider<'a>(provider_id: &str, model_id: &'a str) -> &'a str {
    model_id
        .strip_prefix(&format!("{provider_id}/"))
        .unwrap_or(model_id)
}

fn canonical_model_for_provider(provider_id: &str, model_id: &str) -> String {
    if provider_id == "anthropic" || model_id.contains('/') {
        model_id.to_string()
    } else {
        format!("{provider_id}/{model_id}")
    }
}

fn provider_lookup_ids(provider_id: &str) -> Vec<&str> {
    match provider_id {
        "togetherai" | "together-ai" => vec!["togetherai", "together-ai"],
        "lmstudio" | "lm-studio" => vec!["lmstudio", "lm-studio"],
        "llamacpp" | "llama-cpp" | "llama-server" => {
            vec!["llamacpp", "llama-cpp", "llama-server"]
        }
        "moonshot" | "moonshotai" => vec!["moonshot", "moonshotai"],
        "zhipu" | "zhipuai" => vec!["zhipu", "zhipuai"],
        "vultr" | "vultr-ai" => vec!["vultr", "vultr-ai"],
        "google" | "google-vertex" => vec!["google", "google-vertex"],
        _ => vec![provider_id],
    }
}

fn resolve_fast_model_id(config: &Config) -> String {
    let provider_id = config.selected_provider_id();
    let registry = clawde_api::ModelRegistry::new();

    provider_lookup_ids(provider_id)
        .into_iter()
        .find_map(|lookup_id| registry.best_small_model_for_provider(lookup_id))
        .unwrap_or_else(|| {
            stripped_model_for_provider(provider_id, config.effective_model()).to_string()
        })
}

async fn provider_for_config(
    config: &Config,
) -> Option<std::sync::Arc<dyn clawde_api::LlmProvider>> {
    let anthropic_auth = config.resolve_anthropic_auth_async().await;
    let registry = clawde_api::ProviderRegistry::from_config(
        config,
        clawde_api::client::ClientConfig {
            api_key: anthropic_auth
                .as_ref()
                .map(|(credential, _)| credential.clone())
                .unwrap_or_default(),
            api_base: config.resolve_anthropic_api_base(),
            use_bearer_auth: anthropic_auth
                .as_ref()
                .is_some_and(|(_, use_bearer)| *use_bearer),
            ..Default::default()
        },
    );

    provider_lookup_ids(config.selected_provider_id())
        .into_iter()
        .find_map(|lookup_id| {
            registry
                .get(&clawde_core::ProviderId::new(lookup_id))
                .cloned()
        })
}

/// Resolve the provider a command should use: an explicit test override wins,
/// otherwise build one from config. The override keeps command unit tests
/// hermetic (no network) while leaving production behavior unchanged.
async fn resolve_command_provider(
    ctx: &CommandContext,
) -> Option<std::sync::Arc<dyn clawde_api::LlmProvider>> {
    if let Some(provider) = &ctx.test_provider {
        return Some(provider.clone());
    }
    provider_for_config(&ctx.config).await
}

fn text_from_content_blocks(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

// ---------------------------------------------------------------------------
// Feature command modules (extracted per issue #232 to shrink this file).
// Each module owns a cohesive group of SlashCommand impls plus its private
// helpers. Command structs are re-exported so the public surface is unchanged.
// ---------------------------------------------------------------------------
mod goal;
pub use goal::*;
mod speech;
pub use speech::*;
mod config_cmd;
pub use config_cmd::*;
mod plugin;
pub use plugin::*;
mod doctor;
pub use doctor::*;
mod health;
mod status;
pub use health::*;
mod accounts;
pub use accounts::*;
mod review;
pub use review::*;
mod spec;
pub use spec::*;
mod mcp;
pub use mcp::*;
mod export;
pub use export::*;
mod share;
pub use share::*;
mod copy;
pub use copy::*;
mod chrome;
pub use chrome::*;
mod teleport;
pub use teleport::*;
mod managed_agents;
pub use managed_agents::*;
mod appearance;
pub use appearance::*;
mod memory;
pub use memory::*;
mod permissions;
pub use permissions::*;
mod session;
pub use session::*;
mod remote;
pub use remote::*;
mod history;
pub use history::*;
mod sandbox;
pub use sandbox::*;
mod ultrareview;
pub use ultrareview::*;
mod thinkback;
pub use thinkback::*;
mod search;
pub use search::*;
mod session_tools;
pub use session_tools::*;
mod display;
pub use display::*;
mod maintenance;
pub use maintenance::*;
mod setup;
pub use setup::*;
mod diagnostics;
pub use diagnostics::*;
mod providers;
pub use providers::*;
mod usage;
pub mod verify_cmd;
pub use usage::*;
pub use verify_cmd::*;
mod extras;
pub use extras::*;
mod keys;
pub use keys::*;
mod ui_settings;
use ui_settings::*;
mod routing;
pub use routing::*;
mod compare;
pub use compare::*;
mod new_move;
pub use new_move::*;

// ---------------------------------------------------------------------------
// Built-in commands
// ---------------------------------------------------------------------------

pub struct HelpCommand;
pub struct ClearCommand;
pub struct CompactCommand;
pub struct CostCommand;
pub struct ExitCommand;
pub struct ModelCommand;
pub struct VersionCommand;
pub struct ResumeCommand;
pub struct StatusCommand;
pub struct DiffCommand;
pub struct InitCommand;
pub struct HooksCommand;
pub struct ImportConfigCommand;
pub struct ThinkingCommand;
pub struct AutoCompactCommand;
pub struct TaskCommand;
// New commands
// Batch-1 new commands
// New commands: teleport, btw, ctx-viz, sandbox-toggle
pub struct NamedCommandAdapter {
    pub slash_name: &'static str,
    pub target_name: &'static str,
    pub slash_aliases: &'static [&'static str],
    pub slash_description: &'static str,
    pub slash_help: &'static str,
}

#[derive(serde::Serialize)]
struct KeybindingTemplateFile {
    #[serde(rename = "$schema")]
    schema: &'static str,
    #[serde(rename = "$docs")]
    docs: &'static str,
    bindings: Vec<KeybindingTemplateBlock>,
}

#[derive(serde::Serialize)]
struct KeybindingTemplateBlock {
    context: String,
    bindings: BTreeMap<String, Option<String>>,
}

fn save_settings_mutation<F>(mutate: F) -> anyhow::Result<()>
where
    F: FnOnce(&mut Settings),
{
    let mut settings = Settings::load_sync()?;
    mutate(&mut settings);
    settings.save_sync()
}

fn open_with_system(target: &str) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        let ps_cmd = format!("Start-Process '{}'", target.replace('\'', "''"));
        std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &ps_cmd])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(target)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        Ok(())
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(target)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        Ok(())
    }
}

fn format_keystroke(keystroke: &clawde_core::keybindings::ParsedKeystroke) -> String {
    let mut parts = Vec::new();
    if keystroke.ctrl {
        parts.push("ctrl".to_string());
    }
    if keystroke.alt {
        parts.push("alt".to_string());
    }
    if keystroke.shift {
        parts.push("shift".to_string());
    }
    if keystroke.meta {
        parts.push("meta".to_string());
    }
    parts.push(match keystroke.key.as_str() {
        "space" => "space".to_string(),
        other => other.to_string(),
    });
    parts.join("+")
}

fn format_chord(chord: &[clawde_core::keybindings::ParsedKeystroke]) -> String {
    chord
        .iter()
        .map(format_keystroke)
        .collect::<Vec<_>>()
        .join(" ")
}

fn generate_keybindings_template() -> anyhow::Result<String> {
    let mut grouped: BTreeMap<String, BTreeMap<String, Option<String>>> = BTreeMap::new();
    for binding in clawde_core::keybindings::default_bindings() {
        let chord = format_chord(&binding.chord);
        if clawde_core::keybindings::NON_REBINDABLE.contains(&chord.as_str()) {
            continue;
        }
        grouped
            .entry(format!("{:?}", binding.context))
            .or_default()
            .insert(chord, binding.action.clone());
    }

    let template = KeybindingTemplateFile {
        schema: "https://www.schemastore.org/claude-code-keybindings.json",
        docs: "https://code.claude.com/docs/en/keybindings",
        bindings: grouped
            .into_iter()
            .map(|(context, bindings)| KeybindingTemplateBlock { context, bindings })
            .collect(),
    };

    Ok(format!("{}\n", serde_json::to_string_pretty(&template)?))
}

fn parse_theme(name: &str) -> Option<Theme> {
    match name.trim().to_lowercase().as_str() {
        "default" | "system" => Some(Theme::Default),
        "dark" => Some(Theme::Dark),
        "light" => Some(Theme::Light),
        custom if !custom.is_empty() => Some(Theme::Custom(custom.to_string())),
        _ => None,
    }
}

fn current_output_style_name(config: &Config) -> &str {
    config.output_style.as_deref().unwrap_or("default")
}

fn available_output_style_names() -> Vec<String> {
    // Cache the disk read per process lifetime — called on every keystroke
    // while the user types /output-style arguments.
    use std::sync::OnceLock;
    static STYLES: OnceLock<Vec<String>> = OnceLock::new();
    STYLES
        .get_or_init(|| {
            clawde_core::output_styles::all_styles(&Settings::config_dir())
                .into_iter()
                .map(|style| style.name)
                .collect()
        })
        .clone()
}

fn split_command_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escape = false;

    for ch in args.chars() {
        if escape {
            current.push(ch);
            escape = false;
            continue;
        }

        match ch {
            '\\' => escape = true,
            '\'' | '"' if quote == Some(ch) => quote = None,
            '\'' | '"' if quote.is_none() => quote = Some(ch),
            ch if ch.is_whitespace() && quote.is_none() => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }

    if !current.is_empty() {
        out.push(current);
    }

    out
}

fn execute_named_command_from_slash(
    target_name: &str,
    args: &str,
    ctx: &CommandContext,
) -> CommandResult {
    let Some(cmd) = named_commands::find_named_command(target_name) else {
        return CommandResult::Error(format!(
            "Named command '{}' is not available in this build.",
            target_name
        ));
    };

    let parsed_args = split_command_args(args);
    let parsed_refs = parsed_args.iter().map(String::as_str).collect::<Vec<_>>();
    cmd.execute_named(&parsed_refs, ctx)
}

// ---- /help ---------------------------------------------------------------

/// Category labels for help grouping.
fn command_category(name: &str) -> &'static str {
    match name {
        "clear" | "new" | "compact" | "rewind" | "summary" | "export" | "rename" | "branch"
        | "fork" => "Conversation",
        "model" | "config" | "theme" | "color" | "vim" | "fast" | "effort" | "voice"
        | "statusline" | "output-style" | "keybindings" | "privacy-settings"
        | "rate-limit-options" | "sandbox-toggle" => "Settings",
        "cost" | "stats" | "usage" | "extra-usage" | "context" | "ctx-viz" => "Usage & Cost",
        "status" | "doctor" | "terminal-setup" | "version" | "update" | "upgrade"
        | "release-notes" => "System",
        "login" | "logout" | "refresh" | "permissions" | "keys" => "Auth & Permissions",
        "memory" | "files" | "diff" | "init" | "commit" | "review" | "security-review"
        | "import-config" => "Project",
        "mcp" | "hooks" | "ide" | "chrome" => "Integrations",
        "session" | "resume" | "remote-control" | "remote-env" | "teleport" | "move" => {
            "Sessions & Remote"
        }
        "help" | "exit" => "General",
        "think-back" | "thinkback-play" | "thinking" | "plan" | "tasks" | "auto-compact" => {
            "AI & Thinking"
        }
        "copy" | "skills" | "agents" | "plugin" | "reload-plugins" | "stickers" | "passes"
        | "desktop" | "mobile" | "btw" => "Tools & Extras",
        _ => "Other",
    }
}

#[async_trait]
impl SlashCommand for HelpCommand {
    fn name(&self) -> &str {
        "help"
    }
    fn aliases(&self) -> Vec<&str> {
        vec!["h", "?"]
    }
    fn description(&self) -> &str {
        "Show available commands and usage information"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        if !args.is_empty() {
            // Show help for a specific command. Nested paths resolve through
            // the shared hierarchy and then reuse the target command's help.
            let nested_target = clawde_core::slash_commands::target_for_path(args);
            if let Some(cmd) = nested_target
                .and_then(find_command)
                .or_else(|| find_command(args))
            {
                let aliases = cmd.aliases();
                let alias_line = if aliases.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\nAliases: {}",
                        aliases
                            .iter()
                            .map(|a| format!("/{}", a))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                return CommandResult::Message(format!(
                    "/{name}{aliases}\n{desc}\n\n{help}",
                    name = cmd.name(),
                    aliases = alias_line,
                    desc = cmd.description(),
                    help = cmd.help(),
                ));
            }
            return CommandResult::Error(format!("Unknown command: /{}", args));
        }

        // Grouped output
        let commands = all_commands();
        let visible: Vec<_> = commands.iter().filter(|c| !c.hidden()).collect();

        // Collect categories in stable order
        let category_order = [
            "Conversation",
            "Settings",
            "Usage & Cost",
            "System",
            "Auth & Permissions",
            "Project",
            "Integrations",
            "Sessions & Remote",
            "AI & Thinking",
            "Tools & Extras",
            "General",
            "Other",
        ];

        let mut by_cat: std::collections::HashMap<&str, Vec<String>> =
            std::collections::HashMap::new();

        for cmd in &visible {
            let cat = command_category(cmd.name());
            let aliases = cmd.aliases();
            let alias_str = if aliases.is_empty() {
                String::new()
            } else {
                format!(
                    " ({})",
                    aliases
                        .iter()
                        .map(|a| format!("/{}", a))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            by_cat.entry(cat).or_default().push(format!(
                "  /{:<20} {}",
                format!("{}{}", cmd.name(), alias_str),
                cmd.description()
            ));
        }

        let mut output = String::from("Clawde — Slash Commands\n");
        output.push_str("════════════════════════════\n");

        for cat in &category_order {
            if let Some(entries) = by_cat.get(cat) {
                output.push_str(&format!("\n{}\n", cat));
                for entry in entries {
                    output.push_str(&format!("{}\n", entry));
                }
            }
        }

        // The hierarchy is shared with TUI completion, help, and the command
        // palette. Keep `/help` useful in headless mode by listing roots and
        // leaves here too, while retaining the flat compatibility commands.
        output.push_str("\nCommand families\n");
        for (root, description) in clawde_core::slash_commands::hierarchical_roots("") {
            output.push_str(&format!("  /{:<20} {}\n", root, description));
        }
        for route in clawde_core::slash_commands::HIERARCHICAL_COMMANDS {
            let args = match route.argument_kind() {
                clawde_core::slash_commands::HierarchicalArgument::None => String::new(),
                clawde_core::slash_commands::HierarchicalArgument::FreeText => {
                    " <value>".to_string()
                }
                clawde_core::slash_commands::HierarchicalArgument::Enum(values) => {
                    format!(" [{}]", values.join("|"))
                }
            };
            output.push_str(&format!(
                "  /{:<20} {} (legacy: /{})\n",
                format!("{}{}", route.path, args),
                route.description,
                route.target
            ));
        }

        // Append user-defined command templates from settings and discovered
        // skill commands so custom commands are discoverable, not just
        // executable. Both are built through the shared collection builders;
        // names colliding with a built-in command are skipped so the listing
        // never shows the same name twice (dispatch resolves built-ins first).
        // `commands` (bound above for the `visible` filter) is still in scope;
        // reusing it avoids a second `all_commands()` allocation and the
        // borrow-lifetime error of borrowing from a temporary Vec.
        let builtin_names: std::collections::HashSet<&str> =
            commands.iter().map(|c| c.name()).collect();
        let user_cmds: Vec<Box<dyn SlashCommand>> = commands_from_settings(&ctx.config)
            .into_iter()
            .filter(|c| !builtin_names.contains(c.name()))
            .collect();
        if !user_cmds.is_empty() {
            output.push_str("\nUser-defined\n");
            for cmd in &user_cmds {
                output.push_str(&format!("  /{:<20} {}\n", cmd.name(), cmd.description()));
            }
        }
        let skill_cmds = commands_from_discovered_skills(&ctx.working_dir, &ctx.config.skills);
        if !skill_cmds.is_empty() {
            output.push_str("\nSkills\n");
            for cmd in &skill_cmds {
                output.push_str(&format!("  /{:<20} {}\n", cmd.name(), cmd.description()));
            }
        }

        output.push_str("\nType /help <command> for detailed help on a specific command.");
        CommandResult::Message(output)
    }
}

// ---- /clear --------------------------------------------------------------

#[async_trait]
impl SlashCommand for ClearCommand {
    fn name(&self) -> &str {
        "clear"
    }
    fn aliases(&self) -> Vec<&str> {
        vec!["c", "reset"]
    }
    fn description(&self) -> &str {
        "Clear the conversation history"
    }

    async fn execute(&self, _args: &str, _ctx: &mut CommandContext) -> CommandResult {
        CommandResult::ClearConversation
    }
}

// ---- /compact ------------------------------------------------------------

/// Timeout for the compaction summarisation API call. User-facing strings
/// that mention the timeout must interpolate this value so they stay in sync
/// when it changes.
const COMPACT_API_TIMEOUT: Duration = Duration::from_secs(10);

/// Errors that can occur during compact summary generation.
#[derive(Debug)]
enum CompactError {
    /// No provider available.
    NoProvider,
    /// The provider returned an error.
    ProviderError,
    /// The API request timed out.
    Timeout,
    /// The generated summary was empty.
    EmptySummary,
}

/// Generate a compact summary of the conversation, returning the formatted
/// summary text on success or a [`CompactError`] on failure.
///
/// Handles the common logic shared by `/compact` (preview) and
/// `/compact send` (inject): transcript building, provider lookup, request
/// construction, API call with timeout, response parsing, and formatting.
/// Each caller maps the result into the appropriate [`CommandResult`] variant.
async fn try_compact(
    ctx: &CommandContext,
    msg_count: usize,
    custom_instructions: Option<&str>,
    compact_model: &str,
) -> Result<String, CompactError> {
    let transcript = build_conversation_transcript(&ctx.messages);

    let provider = match resolve_command_provider(ctx).await {
        Some(p) => p,
        None => return Err(CompactError::NoProvider),
    };

    let compact_prompt_text = clawde_query::compact::get_compact_prompt(custom_instructions, None);

    let system_prompt_text =
        "You are an expert conversation summariser that creates thorough, structured          summaries preserving all technical details, file names, code snippets, and          decisions. Follow the instructions carefully and respond with the structured          format requested.";

    let user_content = format!(
        "{}\n\n<conversation_to_summarize original_messages=\"{}\">\n{}\n</conversation_to_summarize>",
        compact_prompt_text,
        msg_count,
        transcript
    );

    let request = clawde_api::ProviderRequest {
        model: compact_model.to_string(),
        messages: vec![clawde_core::types::Message::user(user_content)],
        system_prompt: Some(clawde_api::SystemPrompt::Text(
            system_prompt_text.to_string(),
        )),
        tools: vec![],
        max_tokens: 8192,
        temperature: None,
        top_p: None,
        top_k: None,
        stop_sequences: vec![],
        thinking: None,
        effort_level: ctx.effort,
        provider_options: serde_json::Value::Object(Default::default()),
    };

    let response =
        match tokio::time::timeout(COMPACT_API_TIMEOUT, provider.create_message(request)).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => return Err(CompactError::ProviderError),
            Err(_) => {
                tracing::warn!(
                    "Compact request timed out after {}s",
                    COMPACT_API_TIMEOUT.as_secs()
                );
                return Err(CompactError::Timeout);
            }
        };

    let raw_text = crate::text_from_content_blocks(&response.content);
    if raw_text.trim().is_empty() {
        return Err(CompactError::EmptySummary);
    }

    Ok(clawde_query::compact::format_compact_summary(&raw_text))
}

#[async_trait]
impl SlashCommand for CompactCommand {
    fn name(&self) -> &str {
        "compact"
    }
    fn description(&self) -> &str {
        "Compact the conversation to reduce token usage"
    }
    fn help(&self) -> &str {
        "Usage: /compact [custom instructions|send]\n\n\
         Summarises the conversation using the active provider, preserving\n\
         key technical details, decisions, file paths, and current task status.\n\
         The summary replaces earlier messages so the model can continue with\n\
         reduced token usage.\n\n\
         Subcommands:\n\
           /compact                    - preview the compact summary\n\
           /compact <instructions>     - preview with custom focus\n\
           /compact send               - inject the summary as a user message"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let msg_count = ctx.messages.len();
        if msg_count < 2 {
            return CommandResult::Message(
                "Conversation has fewer than 2 messages -- nothing to compact.".to_string(),
            );
        }

        // Determine whether to inject (send) or preview, and collect any
        // custom instructions from the arguments (skipped for "send").
        let is_send = args.trim().eq_ignore_ascii_case("send");
        let custom_instructions = if is_send || args.trim().is_empty() {
            None
        } else {
            Some(args.trim())
        };

        let compact_model = resolve_fast_model_id(&ctx.config);

        match try_compact(ctx, msg_count, custom_instructions, &compact_model).await {
            Ok(formatted) => {
                if is_send {
                    CommandResult::UserMessage(format!(
                        "[Compact requested - {} earlier messages summarized. Summary below replaces them. Please continue from where we left off.]\n\n<compact-summary>\n{}\n</compact-summary>",
                        msg_count, formatted
                    ))
                } else {
                    CommandResult::Message(format!(
                        "Conversation Compact\n------------------\nOriginal messages: {msg_count}\nModel: {compact_model}\n\n{formatted}\n\n----\nUse /compact send to ask the model to perform the compaction (replace history with this summary)."
                    ))
                }
            }
            Err(CompactError::NoProvider) => CommandResult::Error(
                "No provider available for compaction. Configure an API key first.".to_string(),
            ),
            Err(CompactError::ProviderError) => {
                if is_send {
                    CommandResult::Error(
                        "Compact send failed. Try /compact first to preview.".to_string(),
                    )
                } else {
                    let fallback_instruction = if args.trim().is_empty() {
                        "Provide a detailed summary of our conversation so far, preserving all key technical details, decisions made, file paths mentioned, and current task status."
                    } else {
                        args.trim()
                    };
                    CommandResult::UserMessage(format!(
                        "[Compact requested ({} messages). Instruction: {}]",
                        msg_count, fallback_instruction
                    ))
                }
            }
            Err(CompactError::Timeout) => {
                if is_send {
                    CommandResult::Error(format!(
                        "Compact send timed out after {} seconds. Try /compact first to preview.",
                        COMPACT_API_TIMEOUT.as_secs()
                    ))
                } else {
                    CommandResult::Message(format!(
                        "Compact request timed out after {} seconds. Try again or use /compact send to request the model to summarise the conversation in a new message.",
                        COMPACT_API_TIMEOUT.as_secs()
                    ))
                }
            }
            Err(CompactError::EmptySummary) => {
                if is_send {
                    CommandResult::Error(
                        "Compact summary was empty. Cannot perform compaction.".to_string(),
                    )
                } else {
                    CommandResult::Error(
                        "Compact summary was empty. Try again or use /compact send.".to_string(),
                    )
                }
            }
        }
    }
}
/// Build a plain-text transcript of all messages for the compaction prompt.
/// Parse a capability name string (or alias) into the corresponding
/// `ModelCapability` value.  Returns `None` for unknown strings.
///
/// Delegates to [`clawde_api::ModelCapability::from_name`].
pub fn parse_capability_str(s: &str) -> Option<clawde_api::ModelCapability> {
    clawde_api::ModelCapability::from_name(s)
}

/// Return a human-friendly display label for a capability.
///
/// Delegates to [`clawde_api::ModelCapability::label`].
pub fn capability_label(cap: clawde_api::ModelCapability) -> &'static str {
    cap.label()
}

/// Return a formatted help-text block listing all available capability values
/// with their descriptions. Shared by /image --help and /model --help.
///
/// Delegates to [`clawde_api::ModelCapability::help_text`].
pub fn capability_help_text() -> String {
    clawde_api::ModelCapability::help_text()
}

/// Return arg completions for capability values, filtered by the typed prefix.
fn capability_arg_completions(partial: &str) -> Vec<ArgCompletion> {
    let p = partial.to_lowercase();
    clawde_api::ModelCapability::all_entries()
        .iter()
        .flat_map(|(value, desc)| {
            // Always show the primary value ("json")
            let mut entries = vec![ArgCompletion {
                value: value.to_string(),
                description: desc.to_string(),
                available: true,
            }];
            // Also suggest "structured_output" alias for json.
            if *value == "json" {
                entries.push(ArgCompletion {
                    value: "structured_output".into(),
                    description: desc.to_string(),
                    available: true,
                });
            }
            entries
        })
        .filter(|ac| ac.value.to_lowercase().starts_with(&p))
        .collect()
}

fn build_conversation_transcript(messages: &[Message]) -> String {
    let mut transcript = String::new();
    for msg in messages {
        let role_label = match msg.role {
            clawde_core::types::Role::User => "Human",
            clawde_core::types::Role::Assistant => "Assistant",
        };
        let text = msg.get_all_text();
        transcript.push_str(&format!("{}: {}\n\n", role_label, text));

        if let clawde_core::types::MessageContent::Blocks(blocks) = &msg.content {
            for block in blocks {
                match block {
                    clawde_core::types::ContentBlock::ToolUse {
                        name, input, id, ..
                    } => {
                        transcript.push_str(&format!(
                            "[Tool Call: {} (id={})]\nInput: {}\n\n",
                            name, id, input
                        ));
                    }
                    clawde_core::types::ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        ..
                    } => {
                        let result_text = match content {
                            clawde_core::types::ToolResultContent::Text(t) => {
                                if t.len() > 2000 {
                                    let safe_end = t
                                        .char_indices()
                                        .nth(2000)
                                        .map(|(i, _)| i)
                                        .unwrap_or(t.len());
                                    format!(
                                        "{}... (truncated, {} total chars)",
                                        &t[..safe_end],
                                        t.len()
                                    )
                                } else {
                                    t.clone()
                                }
                            }
                            clawde_core::types::ToolResultContent::Blocks(_) => {
                                "[complex content]".to_string()
                            }
                        };
                        let error_flag = if is_error.unwrap_or(false) {
                            " [ERROR]"
                        } else {
                            ""
                        };
                        transcript.push_str(&format!(
                            "[Tool Result (id={}){}]\n{}\n\n",
                            tool_use_id, error_flag, result_text
                        ));
                    }
                    _ => {}
                }
            }
        }
    }

    const MAX_TRANSCRIPT_CHARS: usize = 80_000;
    if transcript.len() > MAX_TRANSCRIPT_CHARS {
        let safe_end = transcript
            .char_indices()
            .nth(MAX_TRANSCRIPT_CHARS)
            .map(|(i, _)| i)
            .unwrap_or(transcript.len());
        format!(
            "{}...\n\n[TRANSCRIPT TRUNCATED: {} total chars, showing first {}]\n",
            &transcript[..safe_end],
            transcript.len(),
            MAX_TRANSCRIPT_CHARS
        )
    } else {
        transcript
    }
}
#[async_trait]
impl SlashCommand for CostCommand {
    fn name(&self) -> &str {
        "cost"
    }
    fn description(&self) -> &str {
        "Show token usage and cost for this session"
    }
    fn help(&self) -> &str {
        "Usage: /cost\n\n\
         Shows per-category token counts and the estimated cost for this session.\n\
         Cache write tokens are priced slightly higher than input; cache read tokens\n\
         are ~10x cheaper — caching reduces cost significantly in long sessions.\n\
         For per-call breakdown use /extra-usage. For account quotas use /usage."
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        let tracker = &ctx.cost_tracker;
        let model = ctx.config.effective_model();
        let pricing = clawde_core::cost::ModelPricing::for_model(model);

        let input = tracker.input_tokens();
        let output = tracker.output_tokens();
        let cache_create = tracker.cache_creation_tokens();
        let cache_read = tracker.cache_read_tokens();
        let total = tracker.total_tokens();
        let cost = tracker.total_cost_usd();

        // Per-category cost breakdown.
        let input_cost = (input as f64 * pricing.input_per_mtk) / 1_000_000.0;
        let output_cost = (output as f64 * pricing.output_per_mtk) / 1_000_000.0;
        let cc_cost = (cache_create as f64 * pricing.cache_creation_per_mtk) / 1_000_000.0;
        let cr_cost = (cache_read as f64 * pricing.cache_read_per_mtk) / 1_000_000.0;

        // Pricing info line.
        let pricing_line = format!(
            "  Rates ($/MTok): input ${:.2} | output ${:.2} | cache-write ${:.3} | cache-read ${:.3}",
            pricing.input_per_mtk,
            pricing.output_per_mtk,
            pricing.cache_creation_per_mtk,
            pricing.cache_read_per_mtk,
        );

        // Cache savings note: how much input cost was avoided by using cache-read
        // instead of re-sending those tokens as normal input.
        let savings = if cache_read > 0 {
            let saved = (cache_read as f64 * (pricing.input_per_mtk - pricing.cache_read_per_mtk))
                / 1_000_000.0;
            format!(
                "\n  Cache savings:  ${:.4}  ({} tokens served from cache)",
                saved, cache_read
            )
        } else {
            String::new()
        };

        CommandResult::Message(format!(
            "Session Cost — {model}\n\
             ──────────────────────────────\n\
             {pricing_line}\n\n\
               Input tokens:   {input:>10}   ${input_cost:.4}\n\
               Output tokens:  {output:>10}   ${output_cost:.4}\n\
               Cache write:    {cache_create:>10}   ${cc_cost:.4}\n\
               Cache read:     {cache_read:>10}   ${cr_cost:.4}\n\
             ─────────────────────────────\n\
               Total tokens:   {total:>10}\n\
               Total cost:              ${cost:.4}{savings}\n\n\
             Use /usage for quota info · /extra-usage for per-call breakdown",
            model = model,
            pricing_line = pricing_line,
            input = input,
            input_cost = input_cost,
            output = output,
            output_cost = output_cost,
            cache_create = cache_create,
            cc_cost = cc_cost,
            cache_read = cache_read,
            cr_cost = cr_cost,
            total = total,
            cost = cost,
            savings = savings,
        ))
    }
}

// ---- /exit ---------------------------------------------------------------

#[async_trait]
impl SlashCommand for ExitCommand {
    fn name(&self) -> &str {
        "exit"
    }
    fn aliases(&self) -> Vec<&str> {
        vec!["quit", "q"]
    }
    fn description(&self) -> &str {
        "Exit Clawde"
    }

    async fn execute(&self, _args: &str, _ctx: &mut CommandContext) -> CommandResult {
        CommandResult::Exit
    }
}

// ---- /model --------------------------------------------------------------

#[async_trait]
impl SlashCommand for ModelCommand {
    fn name(&self) -> &str {
        "model"
    }
    fn description(&self) -> &str {
        "Show or change the current model"
    }
    fn arg_completions(&self, partial: &str) -> Vec<ArgCompletion> {
        // If the user is typing --capability <value>, return capability names.
        if let Some(cap_val) = partial
            .strip_prefix("--capability ")
            .or_else(|| partial.strip_prefix("-c "))
            .or_else(|| partial.strip_prefix("--capability="))
        {
            return capability_arg_completions(cap_val);
        }
        // Return model IDs for plain /model <partial>.
        use std::collections::HashSet;
        use std::sync::OnceLock;
        static MODELS: OnceLock<Vec<ArgCompletion>> = OnceLock::new();
        let models = MODELS.get_or_init(|| {
            let registry = clawde_api::ModelRegistry::new();
            let mut seen: HashSet<String> = HashSet::new();
            let mut completions: Vec<ArgCompletion> = Vec::new();
            for m in registry.list_all() {
                let id = m.info.id.to_string();
                // Deduplicate: same model ID can appear from multiple
                // providers (e.g. gpt-4o via both OpenAI and Azure).
                if !seen.insert(id.clone()) {
                    continue;
                }
                completions.push(ArgCompletion {
                    value: id,
                    description: format!("{} ({}K ctx)", m.info.name, m.info.context_window / 1000),
                    available: true,
                });
            }
            completions.sort_by(|a, b| a.value.cmp(&b.value));
            completions
        });
        let partial_lower = partial.to_lowercase();
        models
            .iter()
            .filter(|ac| ac.value.to_lowercase().starts_with(&partial_lower))
            .cloned()
            .collect()
    }
    fn help(&self) -> &str {
        "Usage: /model [<model-id>]\n\n\
         Without arguments, shows the current model.\n\n\
         With a model ID, switches to that model.  Accepts both bare model\n\
         names (e.g. claude-sonnet-4-6) and provider-prefixed format\n\
         (e.g. openai/gpt-4o, google/gemini-2.0-flash).\n\n\
         Examples:\n\
           /model                        — show current model\n\
           /model claude-opus-4-6        — switch to Claude Opus 4.6\n\
           /model openai/gpt-4o          — switch to GPT-4o via OpenAI\n\
           /model google/gemini-2.0-flash — switch to Gemini 2.0 Flash"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let args = args.trim();

        // --help: show available capability flags.
        if args == "--help" || args == "-h" {
            return CommandResult::Message(format!(
                "/model [<model-id>|--capability <cap>]\n\n\
                     Without arguments, shows the current model.\n\
                     With a model ID, switches to that model.\n\
                     With --capability, opens the model picker filtered by capability.\n\n\
                     {}\n\n\
                     Examples:\n\
                       /model\n\
                       /model claude-sonnet-4-6\n\
                       /model --capability image\n\
                       /model --capability vision|audio,tools",
                crate::capability_help_text()
            ));
        }

        if args.is_empty() {
            return CommandResult::Message(format!(
                "Current model: {}",
                ctx.config.effective_model()
            ));
        }

        // Accept both "provider/model" and bare model names.
        let model_str = args.to_string();
        let confirmation = if let Some((provider, model)) = model_str.split_once('/') {
            if provider == "anthropic" {
                format!("Switched to {}", model)
            } else {
                format!("Switched to {}/{}", provider, model)
            }
        } else {
            format!("Switched to {}", model_str)
        };
        let mut new_config = ctx.config.clone();
        new_config.model = Some(model_str.clone());
        if let Some((provider, _)) = model_str.split_once('/') {
            new_config.provider = Some(provider.to_string());
        }
        CommandResult::ConfigChangeMessage(new_config, confirmation)
    }
}

// ---- /version ------------------------------------------------------------

#[async_trait]
impl SlashCommand for VersionCommand {
    fn name(&self) -> &str {
        "version"
    }
    fn aliases(&self) -> Vec<&str> {
        vec!["v"]
    }
    fn description(&self) -> &str {
        "Show version information"
    }

    async fn execute(&self, _args: &str, _ctx: &mut CommandContext) -> CommandResult {
        CommandResult::Message(format!("Clawde v{}", clawde_core::constants::APP_VERSION))
    }
}

// ---- /resume -------------------------------------------------------------

#[async_trait]
impl SlashCommand for ResumeCommand {
    fn name(&self) -> &str {
        "resume"
    }
    fn aliases(&self) -> Vec<&str> {
        vec!["r", "continue"]
    }
    fn description(&self) -> &str {
        "Resume a previous conversation"
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        if args.is_empty() {
            let sessions = clawde_core::history::list_sessions().await;
            if sessions.is_empty() {
                return CommandResult::Message("No previous sessions found.".to_string());
            }
            let last = &sessions[0];
            match clawde_core::history::load_session(&last.id).await {
                Ok(session) => CommandResult::ResumeSession(session),
                Err(e) => {
                    CommandResult::Error(format!("Failed to load session {}: {}", last.id, e))
                }
            }
        } else {
            match clawde_core::history::load_session(args.trim()).await {
                Ok(session) => CommandResult::ResumeSession(session),
                Err(e) => {
                    CommandResult::Error(format!("Failed to load session {}: {}", args.trim(), e))
                }
            }
        }
    }
}

// ---- /status -------------------------------------------------------------

#[async_trait]
impl SlashCommand for StatusCommand {
    fn name(&self) -> &str {
        "status"
    }
    fn description(&self) -> &str {
        "Show session, system, and provider health status"
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        // Auth status
        let auth_status = match clawde_core::oauth::OAuthTokens::load().await {
            Some(tokens) => {
                let sub = tokens.subscription_type.as_deref().unwrap_or("oauth");
                format!("Authenticated ({})", sub)
            }
            None => {
                if ctx.config.resolve_api_key().is_some() {
                    "Authenticated (API key)".to_string()
                } else {
                    "Not authenticated".to_string()
                }
            }
        };

        // MCP status
        let mcp_count = ctx.config.mcp_servers.len();
        let mcp_status = if mcp_count == 0 {
            "none configured".to_string()
        } else {
            format!("{} server(s) configured", mcp_count)
        };

        // Hook status
        let hook_count: usize = ctx.config.hooks.values().map(|v| v.len()).sum();

        // UI settings
        let ui = load_ui_settings();
        let editor_mode = ui.editor_mode.as_deref().unwrap_or("normal");
        let fast_mode = ui.fast_mode.unwrap_or(false);

        // Git status
        let git_branch = tokio::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(&ctx.working_dir)
            .output()
            .await
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|_| "n/a".to_string());

        CommandResult::Message(format!(
            "Clawde Status\n\
             ══════════════════\n\
             Auth:           {auth_status}\n\
             Model:          {model}\n\
             Permission mode: {perm:?}\n\
             Fast mode:      {fast}\n\
             Editor mode:    {editor}\n\n\
             Session\n\
             ───────\n\
             Session ID:     {sid}\n\
             Title:          {title}\n\
             Messages:       {msgs}\n\
             Working dir:    {wd}\n\
             Git branch:     {branch}\n\n\
             Integrations\n\
             ────────────\n\
             MCP servers:    {mcp}\n\
             Hooks:          {hooks} configured\n\n\
             Usage\n\
             ─────\n\
             {summary}\n\n\
             {health}",
            auth_status = auth_status,
            model = ctx.config.effective_model(),
            perm = ctx.config.permission_mode,
            fast = if fast_mode { "on" } else { "off" },
            editor = editor_mode,
            sid = &ctx.session_id[..ctx.session_id.len().min(12)],
            title = ctx.session_title.as_deref().unwrap_or("(untitled)"),
            msgs = ctx.messages.len(),
            wd = ctx.working_dir.display(),
            branch = git_branch,
            mcp = mcp_status,
            hooks = hook_count,
            summary = ctx.cost_tracker.summary(),
            health = crate::status::gather_provider_status(),
        ))
    }
}

// ---- /diff ---------------------------------------------------------------

#[async_trait]
impl SlashCommand for DiffCommand {
    fn name(&self) -> &str {
        "diff"
    }
    fn description(&self) -> &str {
        "Show git diff of changes in the working directory"
    }
    fn arg_completions(&self, _partial: &str) -> Vec<ArgCompletion> {
        vec![
            ArgCompletion {
                value: "--stat".into(),
                description: "Summary of changed files".into(),
                available: true,
            },
            ArgCompletion {
                value: "--staged".into(),
                description: "Diff of staged changes".into(),
                available: true,
            },
        ]
    }
    fn help(&self) -> &str {
        "Usage: /diff [--stat|--staged|<ref>]\n\n\
         Shows git diff output for the current working directory.\n\n\
         Options:\n\
           /diff           — diff of all unstaged changes (git diff)\n\
           /diff --stat    — summary of changed files\n\
           /diff --staged  — diff of staged changes (git diff --cached)\n\
           /diff <ref>     — diff against a branch, tag, or commit"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let args = args.trim();

        let git_args: Vec<&str> = if args == "--stat" {
            vec!["diff", "--stat"]
        } else if args == "--staged" || args == "--cached" {
            vec!["diff", "--cached"]
        } else if args.is_empty() {
            vec!["diff"]
        } else {
            // Treat as a ref
            vec!["diff", args]
        };

        let output = tokio::process::Command::new("git")
            .args(&git_args)
            .current_dir(&ctx.working_dir)
            .output()
            .await;

        match output {
            Ok(out) if out.status.success() || out.status.code() == Some(1) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                if stdout.trim().is_empty() {
                    CommandResult::Message(
                        "No changes found. Working tree is clean (or not a git repository)."
                            .to_string(),
                    )
                } else {
                    // Truncate very long diffs
                    let text = stdout.as_ref();
                    let display = if text.len() > 8000 {
                        format!(
                            "{}\n… (truncated — {} total bytes; use `git diff` for full output)",
                            &text[..8000],
                            text.len()
                        )
                    } else {
                        text.to_string()
                    };
                    CommandResult::Message(format!("Changes:\n{}", display))
                }
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                CommandResult::Error(format!(
                    "git diff failed (exit {}): {}",
                    out.status.code().unwrap_or(-1),
                    stderr.trim()
                ))
            }
            Err(e) => CommandResult::Error(format!("Failed to run git diff: {}", e)),
        }
    }
}

// ---- /init ---------------------------------------------------------------

#[async_trait]
impl SlashCommand for InitCommand {
    fn name(&self) -> &str {
        "init"
    }
    fn description(&self) -> &str {
        "Initialize a new project with AGENTS.md"
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        let path = ctx.working_dir.join("AGENTS.md");
        if path.exists() {
            return CommandResult::Message(format!(
                "AGENTS.md already exists at {}",
                path.display()
            ));
        }

        let default_content = "# Project Instructions\n\n\
            Add project-specific instructions and context here.\n\n\
            ## Guidelines\n\n\
            - Describe your project structure\n\
            - Note any coding conventions\n\
            - List important files and their purposes\n";

        match tokio::fs::write(&path, default_content).await {
            Ok(()) => CommandResult::Message(format!("Created AGENTS.md at {}", path.display())),
            Err(e) => CommandResult::Error(format!("Failed to create AGENTS.md: {}", e)),
        }
    }
}

// ---- /import-config ------------------------------------------------------

#[async_trait]
impl SlashCommand for ImportConfigCommand {
    fn name(&self) -> &str {
        "import-config"
    }
    fn description(&self) -> &str {
        "Import CLAUDE.md and settings.json from ~/.claude"
    }
    fn help(&self) -> &str {
        "Usage: /import-config\n\
         Import user-level Claude Code configuration from ~/.claude:\n\
           - ~/.claude/CLAUDE.md\n\
           - ~/.claude/settings.json\n\n\
         This command opens an interactive import dialog with preview and confirmation."
    }

    async fn execute(&self, _args: &str, _ctx: &mut CommandContext) -> CommandResult {
        CommandResult::OpenImportConfigOverlay
    }
}

// ---- /hooks --------------------------------------------------------------

#[async_trait]
impl SlashCommand for HooksCommand {
    fn name(&self) -> &str {
        "hooks"
    }
    fn description(&self) -> &str {
        "Show configured event hooks"
    }
    fn help(&self) -> &str {
        "Usage: /hooks\n\
         Show hooks configured in settings.json under 'hooks'.\n\
         Hooks fire shell commands on events: PreToolUse, PostToolUse, Stop, UserPromptSubmit."
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        // In TUI mode this command is intercepted by intercept_slash_command("hooks")
        // before execute() is ever called, so this path only runs in non-TUI
        // contexts (e.g., `claude hooks` on the CLI, pipes, or tests).
        //
        // Signal to the CLI driver that it should open the TUI overlay if possible;
        // the CLI will fall back to the text listing when no TUI is active.
        if ctx.config.hooks.is_empty() {
            // If there is nothing to show in the overlay, emit a helpful message
            // so the user knows what to do.
            return CommandResult::Message(
                "No hooks configured.\n\
                 Add hooks to ~/.clawde/settings.json under the 'hooks' key.\n\
                 Example:\n\
                 \x20 \"hooks\": {\n\
                 \x20   \"PreToolUse\": [{ \"matcher\": \"*\", \"hooks\": [{ \"type\": \"command\", \"command\": \"echo $STDIN\" }] }]\n\
                 \x20 }"
                    .to_string(),
            );
        }

        // Return the overlay-open signal; the CLI driver will call
        // app.hooks_config_menu.open() or fall back to text output if running
        // without a TUI.
        CommandResult::OpenHooksOverlay
    }
}

// ---- /thinking -----------------------------------------------------------

#[async_trait]
impl SlashCommand for ThinkingCommand {
    fn name(&self) -> &str {
        "thinking"
    }
    fn description(&self) -> &str {
        "Toggle extended thinking mode"
    }
    fn aliases(&self) -> Vec<&str> {
        vec!["think"]
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let action = match args.trim().to_ascii_lowercase().as_str() {
            "" => ThinkingAction::Status,
            "on" | "enable" | "enabled" | "true" | "1" => ThinkingAction::On,
            "off" | "disable" | "disabled" | "false" | "0" => ThinkingAction::Off,
            other => {
                return CommandResult::Error(format!(
                    "Unknown argument '{}'. Use /thinking on, /thinking off, or /thinking.",
                    other
                ));
            }
        };
        CommandResult::ThinkingChange(action)
    }
}

// ---- /auto-compact ----------------------------------------------------------

#[async_trait]
impl SlashCommand for AutoCompactCommand {
    fn name(&self) -> &str {
        "auto-compact"
    }
    fn aliases(&self) -> Vec<&str> {
        vec!["autocompact"]
    }
    fn description(&self) -> &str {
        "Toggle automatic context compaction on/off"
    }
    fn arg_completions(&self, _partial: &str) -> Vec<ArgCompletion> {
        vec![
            ArgCompletion {
                value: "on".into(),
                description: "Enable automatic compaction".into(),
                available: true,
            },
            ArgCompletion {
                value: "off".into(),
                description: "Disable automatic compaction".into(),
                available: true,
            },
        ]
    }
    fn help(&self) -> &str {
        "Usage: /auto-compact [on|off]\n\n\
         Toggles automatic context compaction. When enabled, the conversation\n\
         is automatically compacted as the context window fills up.\n\n\
         Subcommands:\n\
           /auto-compact        - toggle status (show current state)\n\
           /auto-compact on     - enable auto-compact\n\
           /auto-compact off    - disable auto-compact\n\
         The setting is persisted in settings.json."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let current = ctx.config.auto_compact;
        let new_value = match args.trim() {
            "on" | "enable" | "1" | "true" => true,
            "off" | "disable" | "0" | "false" => false,
            "" => !current, // toggle
            other => {
                return CommandResult::Error(format!(
                    "Unknown argument '{}'. Use 'on', 'off', or no argument to toggle.",
                    other
                ));
            }
        };

        if new_value == current {
            return CommandResult::Message(format!(
                "Auto-compact is already {}.",
                if current { "enabled" } else { "disabled" }
            ));
        }

        // Persist the setting via settings.json
        if let Err(e) = save_settings_mutation(|settings| {
            settings.auto_compact = new_value;
        }) {
            return CommandResult::Error(format!("Failed to save setting: {}", e));
        }

        let mut new_config = ctx.config.clone();
        new_config.auto_compact = new_value;
        let msg = format!(
            "Auto-compact {}.",
            if new_value { "enabled" } else { "disabled" }
        );
        CommandResult::ConfigChangeMessage(new_config, msg)
    }
}

// ---- /task ---------------------------------------------------------------

#[async_trait]
impl SlashCommand for TaskCommand {
    fn name(&self) -> &str {
        "task"
    }

    fn description(&self) -> &str {
        "Choose the free-model task lane"
    }

    fn arg_completions(&self, _partial: &str) -> Vec<ArgCompletion> {
        [
            ("all", "Use the full free-model catalog"),
            ("coding", "Prefer coding-capable models"),
            ("reasoning", "Prefer reasoning-capable models"),
            ("creative", "Prefer creative-capable models"),
            ("fast", "Prefer fast-response models"),
            ("multimodal", "Prefer multimodal models"),
            ("long-context", "Prefer long-context models"),
        ]
        .into_iter()
        .map(|(value, description)| ArgCompletion {
            value: value.to_string(),
            description: description.to_string(),
            available: true,
        })
        .collect()
    }

    fn help(&self) -> &str {
        "Usage: /task [all|coding|reasoning|creative|fast|multimodal|long-context]"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let task = args.trim().to_ascii_lowercase();
        let valid = [
            "all",
            "coding",
            "reasoning",
            "creative",
            "fast",
            "multimodal",
            "long-context",
        ];
        if task.is_empty() {
            return CommandResult::Message(format!(
                "Current free-model task lane: {}",
                ctx.config.free_task_sort.as_deref().unwrap_or("all")
            ));
        }
        if !valid.contains(&task.as_str()) {
            return CommandResult::Error(format!(
                "Unknown task '{}'. Choose one of: {}",
                task,
                valid.join(", ")
            ));
        }
        let mut new_config = ctx.config.clone();
        new_config.free_task_sort = Some(task.clone());
        CommandResult::ConfigChangeMessage(
            new_config,
            format!("Free-model task lane set to '{}'.", task),
        )
    }
}

// ---- /sources ------------------------------------------------------------

pub struct SourcesCommand;

#[async_trait]
impl SlashCommand for SourcesCommand {
    fn name(&self) -> &str {
        "sources"
    }
    fn description(&self) -> &str {
        "Show which web search backend was used for the last search"
    }
    fn help(&self) -> &str {
        "Usage: /sources\n\n\
         Shows which search backend was used for the most recent web search.\n\
         The search backend is also shown in the footer as 'search:<backend>'.\n\n\
         Possible backends: searxng, firecrawl, duckduckgo"
    }

    async fn execute(&self, _args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let backend = clawde_tools::web_search::get_last_search_backend();
        if backend.is_empty() {
            CommandResult::Message(
                "No web search has been performed yet in this session.\n\
                 Backends: SearXNG, Firecrawl, DuckDuckGo (in order of priority)"
                    .to_string(),
            )
        } else {
            CommandResult::Message(format!(
                "Last search backend used: {}\n\
                 \n\
                 The search backend is also shown in the footer as 'search:{backend}'.\n\
                 Configure credentials via FIRECRAWL_API_KEY or /keys, and choose a backend via\n\
                 PREFERRED_SEARCH_BACKEND or the persisted preferredSearchBackend setting.",
                backend
            ))
        }
    }
}

// ---- Named-command slash adapters ----------------------------------------

#[async_trait]
impl SlashCommand for NamedCommandAdapter {
    fn name(&self) -> &str {
        self.slash_name
    }

    fn aliases(&self) -> Vec<&str> {
        self.slash_aliases.to_vec()
    }

    fn description(&self) -> &str {
        self.slash_description
    }

    fn help(&self) -> &str {
        self.slash_help
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        execute_named_command_from_slash(self.target_name, args, ctx)
    }
}

// ---- /unload ------------------------------------------------------------

/// Unload the Ollama model from VRAM by sending a request with keep_alive=0.
pub struct UnloadCommand;

#[async_trait]
impl SlashCommand for UnloadCommand {
    fn name(&self) -> &str {
        "unload"
    }
    fn description(&self) -> &str {
        "Unload the Ollama model from GPU VRAM"
    }
    fn help(&self) -> &str {
        "Usage: /unload [model]\n\n\
         Forces Ollama to immediately unload the selected model from VRAM.\n\
         With no model argument, unloads every model currently loaded by the\n\
         configured Ollama server. The model reloads on the next chat request."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let requested = args.trim();
        let requested = (!requested.is_empty()).then(|| {
            requested
                .strip_prefix("ollama/")
                .unwrap_or(requested)
                .to_string()
        });
        match clawde_core::ollama_unload_models_for_config(&ctx.config, requested.as_deref()).await
        {
            Ok(0) if requested.is_some() => CommandResult::Message(format!(
                "Model '{}' is not currently loaded.",
                requested.unwrap_or_default()
            )),
            Ok(0) => CommandResult::Message("No models currently loaded in Ollama.".to_string()),
            Ok(n) => CommandResult::Message(format!("Unloaded {} model(s) from VRAM.", n)),
            Err(e) => CommandResult::Error(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Return all built-in slash commands.
pub fn all_commands() -> Vec<Box<dyn SlashCommand>> {
    vec![
        Box::new(HelpCommand),
        Box::new(ClearCommand),
        Box::new(CompactCommand),
        Box::new(CostCommand),
        Box::new(ExitCommand),
        Box::new(ModelCommand),
        Box::new(ConfigCommand),
        Box::new(ColorCommand),
        Box::new(PluginCommand),
        Box::new(VersionCommand),
        Box::new(ResumeCommand),
        Box::new(ReloadPluginsCommand),
        Box::new(StatusCommand),
        Box::new(DiffCommand),
        Box::new(MemoryCommand),
        Box::new(UsageCommand),
        Box::new(DoctorCommand),
        Box::new(HealthCommand),
        Box::new(LoginCommand),
        Box::new(LogoutCommand),
        Box::new(AccountsCommand),
        Box::new(SwitchCommand),
        Box::new(RefreshCommand),
        Box::new(CavemanCommand),
        Box::new(RockyCommand),
        Box::new(NormalCommand),
        Box::new(InitCommand),
        Box::new(ReviewCommand),
        Box::new(SpecCommand),
        Box::new(SpecModeCommand),
        Box::new(HooksCommand),
        Box::new(ImportConfigCommand),
        Box::new(McpCommand),
        Box::new(PermissionsCommand),
        Box::new(PlanCommand),
        Box::new(TasksCommand),
        Box::new(SessionCommand),
        Box::new(HistoryCommand),
        Box::new(ForkCommand),
        Box::new(ThinkingCommand),
        Box::new(AutoCompactCommand),
        Box::new(TaskCommand),
        Box::new(UnloadCommand),
        Box::new(ThemeCommand),
        Box::new(OutputStyleCommand),
        Box::new(KeybindingsCommand),
        Box::new(PrivacySettingsCommand),
        // New commands
        Box::new(ExportCommand),
        Box::new(ShareCommand),
        Box::new(LinksCommand),
        Box::new(SkillsCommand),
        Box::new(RewindCommand),
        Box::new(StatsCommand),
        Box::new(FilesCommand),
        Box::new(RenameCommand),
        Box::new(EffortCommand),
        Box::new(SummaryCommand),
        Box::new(CommitCommand),
        Box::new(NamedCommandAdapter {
            slash_name: "add-dir",
            target_name: "add-dir",
            slash_aliases: &[],
            slash_description: "Add a directory to Clawde's allowed workspace paths",
            slash_help: "Usage: /add-dir <path>",
        }),
        Box::new(NamedCommandAdapter {
            slash_name: "agents",
            target_name: "agents",
            slash_aliases: &[],
            slash_description: "Manage and configure sub-agents",
            slash_help: "Usage: /agents [list|create|edit|delete] [name]",
        }),
        Box::new(NewAgentCommand),
        Box::new(NamedCommandAdapter {
            slash_name: "branch",
            target_name: "branch",
            slash_aliases: &[],
            slash_description: "Create a branch of the current conversation at this point",
            slash_help: "Usage: /branch [create|switch|list] [name]",
        }),
        Box::new(NamedCommandAdapter {
            slash_name: "tag",
            target_name: "tag",
            slash_aliases: &[],
            slash_description: "Toggle a searchable tag on the current session",
            slash_help: "Usage: /tag [list|add|remove] [tag]",
        }),
        Box::new(NamedCommandAdapter {
            slash_name: "passes",
            target_name: "passes",
            slash_aliases: &[],
            slash_description: "Share a free week of Clawde with friends",
            slash_help: "Usage: /passes",
        }),
        Box::new(NamedCommandAdapter {
            slash_name: "ide",
            target_name: "ide",
            slash_aliases: &[],
            slash_description: "Manage IDE integrations and show status",
            slash_help: "Usage: /ide [status|connect|disconnect|open]",
        }),
        Box::new(NamedCommandAdapter {
            slash_name: "pr-comments",
            target_name: "pr-comments",
            slash_aliases: &[],
            slash_description: "Get comments from a GitHub pull request",
            slash_help: "Usage: /pr-comments <PR-number>",
        }),
        Box::new(NamedCommandAdapter {
            slash_name: "desktop",
            target_name: "desktop",
            slash_aliases: &[],
            slash_description: "Open the Clawde desktop app",
            slash_help: "Usage: /desktop",
        }),
        Box::new(NamedCommandAdapter {
            slash_name: "mobile",
            target_name: "mobile",
            slash_aliases: &[],
            slash_description: "Set up Clawde on mobile",
            slash_help: "Usage: /mobile",
        }),
        Box::new(NamedCommandAdapter {
            slash_name: "install-github-app",
            target_name: "install-github-app",
            slash_aliases: &[],
            slash_description: "Set up Clawde GitHub Actions for a repository",
            slash_help: "Usage: /install-github-app",
        }),
        Box::new(NamedCommandAdapter {
            slash_name: "web-setup",
            target_name: "remote-setup",
            slash_aliases: &["remote-setup"],
            slash_description: "Configure a remote Clawde environment",
            slash_help: "Usage: /web-setup",
        }),
        Box::new(NamedCommandAdapter {
            slash_name: "stickers",
            target_name: "stickers",
            slash_aliases: &[],
            slash_description: "View collected stickers",
            slash_help: "Usage: /stickers",
        }),
        // Batch-1 new commands
        Box::new(RemoteControlCommand),
        Box::new(RemoteEnvCommand),
        Box::new(ContextCommand),
        Box::new(CopyCommand),
        Box::new(ChromeCommand),
        Box::new(VimCommand),
        Box::new(VoiceCommand),
        Box::new(UpgradeCommand),
        Box::new(ReleaseNotesCommand),
        Box::new(RateLimitOptionsCommand),
        Box::new(StatuslineCommand),
        Box::new(SecurityReviewCommand),
        Box::new(TerminalSetupCommand),
        Box::new(ExtraUsageCommand),
        Box::new(ImageCommand),
        Box::new(FastCommand),
        Box::new(OllamaModeCommand),
        Box::new(ThinkBackCommand),
        Box::new(ThinkBackPlayCommand),
        Box::new(ColorSetCommand),
        // New commands: teleport, btw, ctx-viz, sandbox-toggle
        Box::new(TeleportCommand),
        Box::new(BtwCommand),
        Box::new(CtxVizCommand),
        Box::new(SandboxToggleCommand),
        Box::new(VerifyCommand),
        // Advisor
        Box::new(AdvisorCommand),
        // Diagnostics / analysis
        Box::new(HeapdumpCommand),
        Box::new(InsightsCommand),
        Box::new(UltrareviewCommand),
        // Snapshot / revert system
        Box::new(UndoCommand),
        Box::new(RevertCommand),
        Box::new(CheckpointsCommand),
        Box::new(SnapshotDiffCommand),
        // Multi-provider support
        Box::new(ProvidersCommand),
        Box::new(ConnectCommand),
        // Named agent system
        Box::new(AgentCommand),
        // Session search (SQLite)
        Box::new(SearchCommand),
        // Managed agent (manager-executor) architecture
        Box::new(ManagedAgentsCommand),
        // Durable long-running goals
        Box::new(GoalCommand),
        // Multi-key management
        Box::new(KeysCommand),
        // Rate-limit query
        Box::new(LimitsCommand),
        // Routing strategy for free mode
        Box::new(RoutingCommand),
        // Smart-router performance comparison
        Box::new(CompareCommand),
        Box::new(RoutingAlias {
            name: "sr",
            target: "sequential",
        }),
        Box::new(RoutingAlias {
            name: "rr",
            target: "random_failover",
        }),
        Box::new(RoutingAlias {
            name: "lr",
            target: "latency_based",
        }),
        Box::new(RoutingAlias {
            name: "tr",
            target: "task_based",
        }),
        // Search source tracking
        Box::new(SourcesCommand),
        // Session navigation ported from opencode: /new (lazy home) + /move.
        Box::new(NewCommand),
        Box::new(MoveCommand),
        // Free upstream model browser — provides arg_completions for --capability.
        Box::new(NamedCommandAdapter {
            slash_name: "models",
            target_name: "models",
            slash_aliases: &[],
            slash_description: "Browse free upstream models",
            slash_help: "Usage: /models [--capability <cap>]",
        }),
    ]
}

/// Find a command by name or alias.
pub fn find_command(name: &str) -> Option<Box<dyn SlashCommand>> {
    let name = name.trim_start_matches('/');
    all_commands()
        .into_iter()
        .find(|c| c.name() == name || c.aliases().contains(&name))
}

/// Return every `(alias, canonical name, description)` triple across all
/// registered commands.
///
/// This is the single source of truth for hidden aliases: any command that
/// declares an alias in its [`SlashCommand::aliases`] implementation is
/// automatically included. The TUI uses this to make alias prefixes
/// autocomplete to the canonical command name (e.g. `/history` → `/session`)
/// without maintaining a separate hardcoded alias table. The description is
/// carried along so the typeahead can render a suggestion for any alias even
/// when the canonical command is not part of the TUI's curated prompt list.
///
/// Each `aliases()` entry maps to the command that declared it. Aliases must
/// not collide with another command's canonical name — `find_command` prefers
/// a name match at dispatch, so an alias claiming a canonical name would be
/// ambiguous. `test_all_command_aliases_no_canonical_collisions` enforces
/// this invariant.
pub fn all_command_aliases() -> Vec<(String, String, String)> {
    all_commands()
        .into_iter()
        .flat_map(|cmd| {
            let canonical = cmd.name().to_string();
            let description = cmd.description().to_string();
            // Collect inside the closure: `cmd.aliases()` borrows `cmd`, which
            // is local to the closure, so the mapped iterator cannot be
            // returned directly — materializing the triples ends the borrow.
            cmd.aliases()
                .into_iter()
                .map(move |alias| (alias.to_string(), canonical.clone(), description.clone()))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Validate the shared hierarchical routes against the executable command
/// registry. This keeps route metadata from silently pointing at a removed or
/// misspelled flat command.
pub fn validate_hierarchical_routes() -> Result<(), Vec<String>> {
    let mut errors = clawde_core::slash_commands::validate_hierarchy()
        .err()
        .unwrap_or_default();
    for route in clawde_core::slash_commands::HIERARCHICAL_COMMANDS {
        if find_command(route.target).is_none() {
            errors.push(format!(
                "route '{}' targets unknown command '{}'",
                route.path, route.target
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Validate the shared flat-command discovery registry against executable
/// commands and its own metadata invariants.
///
/// Prompt completion and the TUI help/palette consume this core-owned table,
/// while command implementations live here. Keeping this check beside the
/// executable registry prevents a renamed or removed command from becoming a
/// dead item in the prompt UI.
pub fn validate_prompt_commands() -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    let mut names = std::collections::HashSet::new();

    for command in clawde_core::slash_commands::PROMPT_COMMANDS {
        if command.name.trim().is_empty() {
            errors.push("prompt registry contains an empty command name".to_string());
        }
        if command.description.trim().is_empty() {
            errors.push(format!(
                "prompt command '{}' has an empty description",
                command.name
            ));
        }
        if command.category.trim().is_empty() {
            errors.push(format!(
                "prompt command '{}' has an empty category",
                command.name
            ));
        }
        if !names.insert(command.name) {
            errors.push(format!(
                "prompt registry contains duplicate command '{}'",
                command.name
            ));
        }
        if !command.tui_only && find_command(command.name).is_none() {
            errors.push(format!(
                "prompt command '{}' is not an executable command or alias",
                command.name
            ));
        }
    }

    // The registry must be complete in the other direction too: every
    // user-visible executable slash command needs a prompt entry. Hidden
    // implementation aliases (for example /sr and /rr) are intentionally
    // excluded from autocomplete and help.
    let prompt_names: std::collections::HashSet<&str> =
        clawde_core::slash_commands::PROMPT_COMMANDS
            .iter()
            .map(|command| command.name)
            .collect();
    for command in all_commands() {
        if !command.hidden() && !prompt_names.contains(command.name()) {
            errors.push(format!(
                "executable command '{}' is missing from the prompt registry",
                command.name()
            ));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Get argument completions for a registered slash command.
///
/// `cmd_name` is the command name without the leading `/`.
/// `partial` is the text after the command name and space (may be empty).
///
/// Returns completions whose `value` case-insensitively starts with `partial`.
///
/// Uses a `OnceLock`-cached command list so the full `all_commands()` vec is
/// only allocated once per process lifetime, not on every keystroke.
///
/// **Filtering note:** This function always applies a prefix filter after
/// calling [`SlashCommand::arg_completions`].  Commands that pre-filter for
/// performance (like `/model`) will see a redundant but harmless second pass
/// — a no-op on already-filtered results.  The cost is negligible
/// (typically O(1–20) entries) and keeping the filter here means every
/// command benefits from it without needing to implement it themselves.
pub fn get_arg_completions(cmd_name: &str, partial: &str) -> Vec<ArgCompletion> {
    // Early return for capability completions — handles /model, /image, /models.
    // Must happen before the normal dispatch so the downstream partial_lower
    // filter (which compares against the full partial like "--capability vi")
    // doesn't incorrectly drop results like "vision".
    if let Some(cap_val) = partial
        .strip_prefix("--capability ")
        .or_else(|| partial.strip_prefix("-c "))
        .or_else(|| partial.strip_prefix("--capability="))
    {
        return capability_arg_completions(cap_val);
    }

    use std::sync::OnceLock;
    static CMDS: OnceLock<Vec<Box<dyn SlashCommand>>> = OnceLock::new();
    let cmds = CMDS.get_or_init(all_commands);

    let cmd = match cmds
        .iter()
        .find(|c| c.name() == cmd_name || c.aliases().contains(&cmd_name))
    {
        Some(c) => c,
        None => return vec![],
    };
    let completions = cmd.arg_completions(partial);
    let partial_lower = partial.to_lowercase();
    completions
        .into_iter()
        .filter(|ac| ac.value.to_lowercase().starts_with(&partial_lower))
        .collect()
}

// ---------------------------------------------------------------------------
// User-defined command templates (Feature 2)
// ---------------------------------------------------------------------------

/// A slash command backed by a user-defined template in `settings.json`.
struct TemplateCommand {
    name: String,
    template: clawde_core::CommandTemplate,
}

#[async_trait]
impl SlashCommand for TemplateCommand {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        self.template
            .description
            .as_deref()
            .unwrap_or("Custom command")
    }
    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let mut words = args.split_whitespace();
        let arg1 = words.next().unwrap_or("");
        let arg2 = words.next().unwrap_or("");
        let prompt = self
            .template
            .template
            .replace("$ARGUMENTS", args)
            .replace("$1", arg1)
            .replace("$2", arg2);
        CommandResult::UserMessage(prompt)
    }
}

/// Build slash commands from user-defined command templates stored in
/// `config.commands` (copied from settings.json on load).
pub fn commands_from_settings(config: &Config) -> Vec<Box<dyn SlashCommand>> {
    config
        .commands
        .iter()
        .map(|(name, template)| {
            Box::new(TemplateCommand {
                name: name.clone(),
                template: template.clone(),
            }) as Box<dyn SlashCommand>
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Discovered skill commands (from .claurst/skills/ and git URLs)
// ---------------------------------------------------------------------------

/// A slash command backed by a discovered skill markdown file.
struct SkillCommand {
    name: String,
    description: String,
    template: String,
}

#[async_trait]
impl SlashCommand for SkillCommand {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let mut words = args.split_whitespace();
        let arg1 = words.next().unwrap_or("");
        let arg2 = words.next().unwrap_or("");
        let prompt = self
            .template
            .replace("$ARGUMENTS", args)
            .replace("$1", arg1)
            .replace("$2", arg2);
        CommandResult::UserMessage(prompt)
    }
}

/// Build slash commands from skill markdown files discovered on the filesystem
/// and from configured git URLs.
///
/// Pass the project `cwd` and the `skills` section of the effective config.
/// Bundled skills take precedence — any discovered skill whose name clashes
/// with a built-in command will be silently skipped.
pub fn commands_from_discovered_skills(
    cwd: &std::path::Path,
    skills_config: &clawde_core::SkillsConfig,
) -> Vec<Box<dyn SlashCommand>> {
    let discovered = clawde_core::discover_skills(cwd, skills_config);
    // Build a set of built-in command names so we can skip collisions.
    let all_cmds = all_commands();
    let builtin_names: std::collections::HashSet<&str> =
        all_cmds.iter().map(|c| c.name()).collect();

    discovered
        .into_values()
        .filter(|skill| !builtin_names.contains(skill.name.as_str()))
        .map(|skill| {
            Box::new(SkillCommand {
                name: skill.name,
                description: skill.description,
                template: skill.template,
            }) as Box<dyn SlashCommand>
        })
        .collect()
}

/// Execute a slash command string (with leading /).
pub async fn execute_command(input: &str, ctx: &mut CommandContext) -> Option<CommandResult> {
    if !clawde_tui::input::is_slash_command(input) {
        return None;
    }
    // Resolve a hierarchical path before the existing parser/registry. The
    // route table is additive: flat commands and their legacy aliases still
    // take the normal path, while `/provider connect foo` becomes
    // `/connect foo` for the existing handler.
    let normalized = clawde_core::slash_commands::normalize_invocation(input);
    let dispatch_input = normalized.as_deref().unwrap_or(input);
    let (name, args) = clawde_tui::input::parse_slash_command(dispatch_input);

    // First check built-in commands.
    if let Some(cmd) = find_command(name) {
        return Some(cmd.execute(args, ctx).await);
    }

    // Check user-defined command templates from settings.
    let cmd_name = name.trim_start_matches('/');
    if let Some(tmpl) = ctx.config.commands.get(cmd_name).cloned() {
        let tc = TemplateCommand {
            name: cmd_name.to_string(),
            template: tmpl,
        };
        return Some(tc.execute(args, ctx).await);
    }

    // Check discovered skill commands (from .claurst/skills/, git URLs, etc.).
    {
        let discovered = clawde_core::discover_skills(&ctx.working_dir, &ctx.config.skills);
        if let Some(skill) = discovered.get(cmd_name) {
            let sc = SkillCommand {
                name: skill.name.clone(),
                description: skill.description.clone(),
                template: skill.template.clone(),
            };
            return Some(sc.execute(args, ctx).await);
        }
    }

    // Then check plugin-defined slash commands.
    let project_dir = ctx.working_dir.clone();
    let registry = clawde_plugins::load_plugins(&project_dir, &[]).await;
    for cmd_def in registry.all_command_defs() {
        if cmd_def.name == cmd_name {
            let adapter = PluginSlashCommandAdapter { def: cmd_def };
            return Some(adapter.execute(args, ctx).await);
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Named commands module (top-level `claude <name>` subcommands)
// ---------------------------------------------------------------------------
pub mod named_commands;

// ---------------------------------------------------------------------------
// Stats analytics (persisted transcript aggregation) — backs `clawde stats`.
// The current-session `/stats` slash command lives above; this module reads
// JSONL transcripts on disk.
// ---------------------------------------------------------------------------
pub mod stats;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use clawde_core::cost::CostTracker;
    use std::sync::{Mutex, OnceLock};

    /// Serialises every test that mutates the process-global `CLAWDE_HOME`
    /// env var. Multiple tests setting `CLAWDE_HOME` to their own temp dir in
    /// parallel races the environment: one test's `save()` can target a path
    /// whose parent directory belongs to (and was already cleaned up by)
    /// another test, producing flaky "No such file or directory" failures.
    /// Test harnesses (`TestAccounts` in accounts.rs, `TestHome` in keys.rs,
    /// and the theme-completion test below) acquire this lock for their whole
    /// lifetime so only one such test runs at a time.
    pub(crate) static CLAWDE_HOME_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn make_ctx() -> CommandContext {
        CommandContext {
            config: clawde_core::config::Config::default(),
            cost_tracker: CostTracker::new(),
            messages: vec![],
            working_dir: std::path::PathBuf::from("."),
            session_id: "test-session".to_string(),
            session_title: None,
            remote_session_url: None,
            mcp_manager: None,
            mcp_auth_runner: None,
            provider_registry: None,
            test_provider: None,
            effort: None,
        }
    }

    /// Build a [`CommandContext`] with a canned [`clawde_api::LlmProvider`]
    /// injected so network-touching commands (`/compact`, `/summary`) run
    /// hermetically in unit tests.
    fn make_ctx_with_canned_provider() -> CommandContext {
        let mut ctx = make_ctx();
        ctx.test_provider = Some(std::sync::Arc::new(CannedProvider::new()));
        ctx
    }

    /// A deterministic [`clawde_api::LlmProvider`] returning a fixed text
    /// response, so tests never touch the network. Mirror of the mock patterns
    /// in `query/src/compact.rs` (GateMockProvider) and
    /// `api/src/providers/key_rotating.rs` (MockProvider). Records the last
    /// request's `effort_level` per instance so tests can assert the session
    /// override flowed into the auxiliary request.
    struct CannedProvider {
        last_effort: std::sync::Mutex<Option<clawde_core::effort::EffortLevel>>,
    }

    impl CannedProvider {
        fn new() -> Self {
            Self {
                last_effort: std::sync::Mutex::new(None),
            }
        }
    }

    #[async_trait::async_trait]
    impl clawde_api::LlmProvider for CannedProvider {
        fn id(&self) -> &clawde_core::ProviderId {
            static ID: std::sync::LazyLock<clawde_core::ProviderId> =
                std::sync::LazyLock::new(|| clawde_core::ProviderId::new("canned"));
            &ID
        }

        fn name(&self) -> &str {
            "canned"
        }

        async fn create_message(
            &self,
            request: clawde_api::ProviderRequest,
        ) -> Result<clawde_api::ProviderResponse, clawde_api::ProviderError> {
            *self.last_effort.lock().unwrap() = request.effort_level;
            Ok(clawde_api::ProviderResponse {
                id: "canned".into(),
                model: request.model,
                content: vec![clawde_core::types::ContentBlock::Text {
                    text: "Canned test summary response.".to_string(),
                }],
                stop_reason: clawde_api::StopReason::EndTurn,
                usage: Default::default(),
            })
        }

        async fn create_message_stream(
            &self,
            _request: clawde_api::ProviderRequest,
        ) -> Result<
            std::pin::Pin<
                Box<
                    dyn futures::Stream<
                            Item = Result<clawde_api::StreamEvent, clawde_api::ProviderError>,
                        > + Send,
                >,
            >,
            clawde_api::ProviderError,
        > {
            unimplemented!("canned provider does not support streaming")
        }

        async fn health_check(
            &self,
        ) -> Result<clawde_api::ProviderStatus, clawde_api::ProviderError> {
            Ok(clawde_api::ProviderStatus::Healthy)
        }

        fn capabilities(&self) -> clawde_api::ProviderCapabilities {
            clawde_api::ProviderCapabilities {
                streaming: false,
                tool_calling: false,
                thinking: false,
                image_input: false,
                pdf_input: false,
                audio_input: false,
                video_input: false,
                caching: false,
                structured_output: false,
                system_prompt_style: clawde_api::SystemPromptStyle::TopLevel,
            }
        }
    }

    /// A canned structured response used by the cross-crate `/spec` → review
    /// integration test below. It deliberately implements the same provider
    /// seam used by production command dispatch without making a network call.
    struct CannedSpecProvider;

    #[async_trait::async_trait]
    impl clawde_api::LlmProvider for CannedSpecProvider {
        fn id(&self) -> &clawde_core::ProviderId {
            static ID: std::sync::LazyLock<clawde_core::ProviderId> =
                std::sync::LazyLock::new(|| clawde_core::ProviderId::new("canned-spec"));
            &ID
        }

        fn name(&self) -> &str {
            "canned-spec"
        }

        async fn create_message(
            &self,
            _request: clawde_api::ProviderRequest,
        ) -> Result<clawde_api::ProviderResponse, clawde_api::ProviderError> {
            Ok(clawde_api::ProviderResponse {
                id: "canned-spec".into(),
                model: "canned-spec".into(),
                content: vec![clawde_core::types::ContentBlock::Text {
                    text: r#"{"title":"Cross-Crate Demo","requirements":["Create the harmless demo artifact"],"files_to_touch":[{"path":"phase-c-demo.txt","action":"Create","description":"Demo artifact"}],"data_models":[],"acceptance_tests":[{"description":"The demo artifact exists with the expected content"}],"edge_cases":[]}"#
                        .to_string(),
                }],
                stop_reason: clawde_api::StopReason::EndTurn,
                usage: Default::default(),
            })
        }

        async fn create_message_stream(
            &self,
            _request: clawde_api::ProviderRequest,
        ) -> Result<
            std::pin::Pin<
                Box<
                    dyn futures::Stream<
                            Item = Result<clawde_api::StreamEvent, clawde_api::ProviderError>,
                        > + Send,
                >,
            >,
            clawde_api::ProviderError,
        > {
            unimplemented!("cross-crate spec fixture does not stream")
        }

        async fn health_check(
            &self,
        ) -> Result<clawde_api::ProviderStatus, clawde_api::ProviderError> {
            Ok(clawde_api::ProviderStatus::Healthy)
        }

        fn capabilities(&self) -> clawde_api::ProviderCapabilities {
            clawde_api::ProviderCapabilities {
                streaming: false,
                tool_calling: false,
                thinking: false,
                image_input: false,
                pdf_input: false,
                audio_input: false,
                video_input: false,
                caching: false,
                structured_output: false,
                system_prompt_style: clawde_api::SystemPromptStyle::TopLevel,
            }
        }
    }

    // ---- Command registry tests ---------------------------------------------

    #[test]
    fn test_all_commands_non_empty() {
        assert!(!all_commands().is_empty());
    }

    #[test]
    fn test_hierarchical_routes_target_registered_commands() {
        validate_hierarchical_routes().unwrap_or_else(|errors| panic!("{errors:#?}"));
    }

    #[test]
    fn test_prompt_discovery_entries_are_registered_commands() {
        validate_prompt_commands().unwrap_or_else(|errors| panic!("{errors:#?}"));
    }

    #[test]
    fn test_all_commands_have_unique_names() {
        let mut names = std::collections::HashSet::new();
        for cmd in all_commands() {
            assert!(
                names.insert(cmd.name().to_string()),
                "Duplicate command name: {}",
                cmd.name()
            );
        }
    }

    #[test]
    fn test_find_command_by_name() {
        assert!(find_command("help").is_some());
        assert!(find_command("clear").is_some());
        assert!(find_command("exit").is_some());
        assert!(find_command("model").is_some());
        assert!(find_command("refresh").is_some());
        assert!(find_command("version").is_some());
    }

    #[tokio::test]
    async fn spec_generation_to_approval_initializes_bound_plan() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

        let dir = tempfile::tempdir().expect("create isolated project");
        let session_id = "cross-crate-spec-session";
        let mut ctx = make_ctx();
        ctx.working_dir = dir.path().to_path_buf();
        ctx.session_id = session_id.to_string();
        ctx.test_provider = Some(std::sync::Arc::new(CannedSpecProvider));

        // Exercise the real command registry and `/spec` command, not the
        // command implementation directly.
        let result = execute_command("/spec Create a harmless demo note", &mut ctx)
            .await
            .expect("/spec must resolve through the registry");
        match result {
            CommandResult::Message(message) => {
                assert!(message.contains("# Spec: Cross-Crate Demo"));
                assert!(message.contains("Saved to"));
            }
            other => panic!("expected generated spec message, got {other:?}"),
        }

        let spec_path = clawde_core::spec::Spec::list_specs(dir.path())
            .into_iter()
            .next()
            .expect("/spec writes a parseable spec artifact");
        let raw_spec = std::fs::read_to_string(&spec_path).expect("read generated spec");
        let spec = clawde_core::spec::Spec::parse_json(&raw_spec).expect("parse generated spec");
        assert_eq!(spec.session_id.as_deref(), Some(session_id));
        assert!(!spec.task_id.is_empty());

        // Cross the crate boundary into the real TUI review state and accept
        // the default action through App::handle_key_event.
        let mut app = clawde_tui::App::new(ctx.config.clone(), ctx.cost_tracker.clone());
        app.set_working_directory(dir.path());
        app.spec_review.set_session_id(session_id);
        assert!(app.intercept_slash_command_with_args("spec-review", ""));
        assert_eq!(app.spec_review.path.as_ref(), Some(&spec_path));
        app.handle_key_event(KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });

        let (approved_path, approved_spec) =
            clawde_core::spec::Spec::approved_in(dir.path(), session_id)
                .expect("Accept persists approval");
        assert_eq!(approved_path, spec_path.canonicalize().unwrap());
        assert_eq!(approved_spec.task_id, spec.task_id);
        let progress = clawde_core::plan::PlanProgress::load_for(
            dir.path(),
            &spec.task_id,
            session_id,
            &clawde_core::spec::Spec::content_hash(&raw_spec),
        )
        .unwrap()
        .expect("Accept initializes bound plan progress");
        assert_eq!(progress.task_id, spec.task_id);
        assert_eq!(progress.session_id, session_id);
        assert_eq!(
            progress.spec_hash,
            clawde_core::spec::Spec::content_hash(&raw_spec)
        );
        assert!(
            clawde_core::plan::PlanProgress::path_for(dir.path(), &spec.task_id)
                .unwrap()
                .is_file()
        );
        assert_eq!(app.queued_messages.len(), 1);
        assert!(app.queued_messages[0].contains("ACCEPTED"));
    }

    #[test]
    fn test_find_command_with_slash_prefix() {
        // find_command should strip the leading / before lookup
        assert!(find_command("/help").is_some());
        assert!(find_command("/clear").is_some());
    }

    #[test]
    fn test_find_command_by_alias() {
        // /help has aliases "h" and "?"
        assert!(find_command("h").is_some());
        assert!(find_command("?").is_some());
        // /clear has alias "c"
        assert!(find_command("c").is_some());
        assert!(find_command("settings").is_some());
        assert!(find_command("continue").is_some());
        assert!(find_command("bashes").is_some());
        assert!(find_command("remote").is_some());
        assert!(find_command("remote-setup").is_some());
    }

    #[test]
    fn test_history_is_a_real_command_not_an_alias() {
        // `/history` was promoted from a `/session` alias to a dedicated command
        // (lists the current project's sessions). It must resolve as its own
        // command, not as an alias pointing at `/session`.
        let cmd = find_command("history");
        assert!(cmd.is_some(), "expected a dedicated /history command");
        if let Some(cmd) = cmd {
            assert_eq!(cmd.name(), "history");
        }
        let aliases = all_command_aliases();
        assert!(
            !aliases
                .iter()
                .any(|(a, c, _)| a == "history" && c == "session"),
            "/history must no longer alias /session"
        );
    }

    #[test]
    fn test_all_command_aliases_cover_all_aliases() {
        // Every alias declared by a command must appear in the alias table,
        // mapped to the command that declared it, with a non-empty description.
        let aliases = all_command_aliases();
        for cmd in all_commands() {
            let canonical = cmd.name().to_string();
            for alias in cmd.aliases() {
                assert!(
                    aliases
                        .iter()
                        .any(|(a, c, d)| { a == alias && *c == canonical && !d.is_empty() }),
                    "alias {} → {} missing (or missing description) from all_command_aliases",
                    alias,
                    canonical
                );
            }
        }
    }

    #[test]
    fn test_all_command_aliases_no_canonical_collisions() {
        // An alias must never claim a name that is another command's canonical
        // name — dispatch prefers the canonical name, so the typeahead must too.
        let commands = all_commands();
        let canonical_names: std::collections::HashSet<String> =
            commands.iter().map(|c| c.name().to_string()).collect();
        for (alias, _, _) in all_command_aliases() {
            assert!(
                !canonical_names.contains(&alias),
                "alias {alias} collides with a canonical command name"
            );
        }
    }

    #[test]
    fn test_shared_alias_first_match_semantics() {
        // If two commands ever declare the same alias string, dispatch
        // (`find_command`) and the typeahead (`all_command_aliases` first
        // match) must agree on the winner: both iterate `all_commands()` in
        // order and pick the first command that claims the alias. Lock in the
        // invariant that for every alias, `find_command` resolves to the same
        // canonical as the first (alias → canonical) triple in the table.
        let aliases = all_command_aliases();
        for (alias, canonical, _) in &aliases {
            let resolved = find_command(alias)
                .map(|c| c.name().to_string())
                .expect("alias must resolve to a command");
            assert_eq!(
                &resolved, canonical,
                "alias `{alias}`: dispatch resolves to `{resolved}` but typeahead first-match says `{canonical}`"
            );
        }

        // Document the shared-alias scenario explicitly: the first command in
        // `all_commands()` order wins. (No two commands currently share an
        // alias, so this just asserts the ordering invariant holds for the
        // real table — if a collision is ever introduced, the loop above keeps
        // dispatch and typeahead in agreement.)
        let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for cmd in all_commands() {
            let canonical = cmd.name().to_string();
            for alias in cmd.aliases() {
                if let Some(prev) = seen.insert(alias.to_string(), canonical.clone()) {
                    // A collision exists: dispatch must pick the first-declared
                    // canonical (the earlier command in all_commands() order).
                    let resolved = find_command(alias)
                        .map(|c| c.name().to_string())
                        .unwrap_or_default();
                    assert_eq!(
                        resolved, prev,
                        "first-match wins for shared alias `{alias}`"
                    );
                }
            }
        }
    }

    #[test]
    fn test_find_command_not_found() {
        assert!(find_command("nonexistent_command_xyz").is_none());
    }

    #[test]
    fn test_core_commands_present() {
        let expected = [
            "help",
            "clear",
            "compact",
            "cost",
            "exit",
            "model",
            "config",
            "version",
            "status",
            "diff",
            "memory",
            "hooks",
            "permissions",
            "plan",
            "tasks",
            "session",
            "login",
            "logout",
            "refresh",
            "usage",
            "plugin",
            "reload-plugins",
            "add-dir",
            "agents",
            "branch",
            "tag",
            "passes",
            "ide",
            "pr-comments",
            "desktop",
            "mobile",
            "install-github-app",
            "web-setup",
            "stickers",
        ];
        for name in &expected {
            assert!(
                find_command(name).is_some(),
                "Expected command '{}' not in all_commands()",
                name
            );
        }
    }

    // ---- Command execution tests --------------------------------------------

    #[tokio::test]
    async fn test_clear_command_returns_clear_conversation() {
        let mut ctx = make_ctx();
        let cmd = find_command("clear").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        assert!(matches!(result, CommandResult::ClearConversation));
    }

    // ---- /output-style + /keybindings end-to-end (issue #278 point 2) ------

    #[tokio::test]
    async fn output_style_lists_personas_and_current() {
        // The empty-arg path only reads (no disk write) and must surface the
        // built-in styles including the newly-consolidated personas.
        let mut ctx = make_ctx();
        let cmd = find_command("output-style").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        let CommandResult::Message(text) = result else {
            panic!("empty /output-style should list styles, got {result:?}");
        };
        assert!(
            text.contains("caveman"),
            "personas must appear in the list: {text}"
        );
        assert!(text.contains("rocky"));
        assert!(text.contains("default"));
        // Default config → default is the current style.
        assert!(text.contains("Current output style: default"));
    }

    #[tokio::test]
    async fn output_style_rejects_unknown_name() {
        let mut ctx = make_ctx();
        let cmd = find_command("output-style").unwrap();
        let result = cmd.execute("definitely-not-a-style", &mut ctx).await;
        assert!(matches!(result, CommandResult::Error(_)));
    }

    #[test]
    fn available_output_styles_include_personas() {
        let names = available_output_style_names();
        for expected in ["default", "concise", "caveman", "rocky"] {
            assert!(
                names.iter().any(|n| n == expected),
                "output style '{expected}' should be available"
            );
        }
    }

    #[test]
    fn persisted_persona_resolves_to_its_prompt() {
        // End-to-end of the persist path: /output-style / /rocky set
        // config.output_style, which resolves to the persona's prompt text for
        // the system prompt.
        let config = clawde_core::config::Config {
            output_style: Some("rocky".to_string()),
            ..clawde_core::config::Config::default()
        };
        let prompt = config
            .resolve_output_style_prompt()
            .expect("rocky must resolve to a prompt");
        assert!(prompt.contains("Project Hail Mary"));
    }

    #[test]
    fn keybindings_template_is_valid_json() {
        // /keybindings writes this template on first run; ensure it always
        // generates and parses so the command cannot fail generating its file.
        let template = generate_keybindings_template().expect("template must generate");
        let parsed: serde_json::Value =
            serde_json::from_str(&template).expect("template must be valid JSON");
        assert!(
            parsed.get("bindings").is_some(),
            "template needs a bindings block"
        );
    }

    #[test]
    fn test_new_and_move_commands_present() {
        assert!(find_command("new").is_some());
        assert!(find_command("move").is_some());
    }

    #[test]
    fn test_clear_no_longer_aliases_new() {
        // /new is now its own lazy-home command; /clear keeps its other aliases.
        let clear = find_command("clear").unwrap();
        assert!(!clear.aliases().contains(&"new"));
        assert_eq!(find_command("new").unwrap().name(), "new");
    }

    #[tokio::test]
    async fn test_new_command_returns_new_session() {
        let mut ctx = make_ctx();
        let cmd = find_command("new").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        assert!(matches!(result, CommandResult::NewSession));
    }

    #[tokio::test]
    async fn test_move_command_without_dir_shows_usage() {
        let mut ctx = make_ctx();
        let cmd = find_command("move").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        // No target → usage message, never a MoveSession side effect.
        assert!(matches!(result, CommandResult::Message(_)));
    }

    #[tokio::test]
    async fn test_move_command_rejects_missing_directory() {
        let mut ctx = make_ctx();
        let cmd = find_command("move").unwrap();
        let result = cmd
            .execute("/definitely/not/a/real/path/xyz123", &mut ctx)
            .await;
        assert!(matches!(result, CommandResult::Error(_)));
    }

    #[tokio::test]
    async fn test_refresh_command_requests_provider_reset() {
        let mut ctx = make_ctx();
        let cmd = find_command("refresh").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        assert!(matches!(result, CommandResult::RefreshProviderState));
    }

    #[tokio::test]
    async fn test_exit_command_returns_exit() {
        let mut ctx = make_ctx();
        let cmd = find_command("exit").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        assert!(matches!(result, CommandResult::Exit));
    }

    #[tokio::test]
    async fn test_version_command_returns_message() {
        let mut ctx = make_ctx();
        let cmd = find_command("version").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        assert!(matches!(result, CommandResult::Message(_)));
        if let CommandResult::Message(msg) = result {
            assert!(
                msg.contains("claude") || msg.contains("Clawde") || msg.contains('.'),
                "Version message should contain version number, got: {}",
                msg
            );
        }
    }

    #[tokio::test]
    async fn test_cost_command_returns_message() {
        let mut ctx = make_ctx();
        let cmd = find_command("cost").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        assert!(matches!(result, CommandResult::Message(_)));
    }

    #[tokio::test]
    async fn test_login_command_starts_oauth_flow() {
        let mut ctx = make_ctx();
        let cmd = find_command("login").unwrap();
        // Default (no --console) → Anthropic, login_with_claude_ai = true
        let result = cmd.execute("", &mut ctx).await;
        match result {
            CommandResult::StartLoginForProvider {
                provider,
                login_with_claude_ai,
                label,
            } => {
                assert_eq!(provider, clawde_core::accounts::PROVIDER_ANTHROPIC);
                assert!(login_with_claude_ai);
                assert!(label.is_none());
            }
            other => panic!("expected StartLoginForProvider, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_login_command_console_flag() {
        let mut ctx = make_ctx();
        let cmd = find_command("login").unwrap();
        let result = cmd.execute("--console", &mut ctx).await;
        match result {
            CommandResult::StartLoginForProvider {
                provider,
                login_with_claude_ai,
                ..
            } => {
                assert_eq!(provider, clawde_core::accounts::PROVIDER_ANTHROPIC);
                assert!(!login_with_claude_ai);
            }
            other => panic!("expected StartLoginForProvider, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_login_command_codex_flag() {
        let mut ctx = make_ctx();
        let cmd = find_command("login").unwrap();
        let result = cmd.execute("--codex --label work", &mut ctx).await;
        match result {
            CommandResult::StartLoginForProvider {
                provider, label, ..
            } => {
                assert_eq!(provider, clawde_core::accounts::PROVIDER_CODEX);
                assert_eq!(label.as_deref(), Some("work"));
            }
            other => panic!("expected StartLoginForProvider, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_accounts_command_returns_message() {
        let mut ctx = make_ctx();
        let cmd = find_command("accounts").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        // Should return a Message regardless of registry contents.
        assert!(matches!(result, CommandResult::Message(_)));
    }

    #[tokio::test]
    async fn test_switch_command_requires_id() {
        let mut ctx = make_ctx();
        let cmd = find_command("switch").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        assert!(matches!(result, CommandResult::Error(_)));
    }

    #[tokio::test]
    async fn test_help_command_returns_message() {
        let mut ctx = make_ctx();
        let cmd = find_command("help").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        // help returns either Message or Silent
        assert!(
            matches!(result, CommandResult::Message(_) | CommandResult::Silent),
            "help should return Message or Silent"
        );
    }

    #[tokio::test]
    async fn test_help_resolves_alias_to_canonical() {
        // `/help history` must resolve the hidden alias to its canonical
        // command (/session) and show the command plus its aliases.
        let mut ctx = make_ctx();
        let cmd = find_command("help").unwrap();
        let result = cmd.execute("history", &mut ctx).await;
        match result {
            CommandResult::Message(msg) => {
                assert!(
                    msg.contains("/session"),
                    "expected canonical /session in help output, got: {msg}"
                );
                assert!(
                    msg.contains("history"),
                    "expected alias /history listed in help output, got: {msg}"
                );
            }
            other => panic!("expected Message, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_help_unknown_alias_errors() {
        // `/help not-an-alias` must fall through to the unknown-command error
        // rather than silently returning empty help.
        let mut ctx = make_ctx();
        let cmd = find_command("help").unwrap();
        let result = cmd.execute("not-an-alias", &mut ctx).await;
        assert!(matches!(result, CommandResult::Error(_)));
    }

    #[test]
    fn test_no_unreferenced_pub_functions_in_workspace() {
        // Dead-code guard: rustc's `dead_code` lint never fires for `pub`
        // items, so a `pub fn` that nothing calls (like the former
        // `build_help_entries`) silently rots. The shared implementation in
        // `clawde_core::dead_code_guard` scans the workspace and fails if any
        // `pub fn` / `pub async fn` declared in this crate has no reference
        // anywhere except its own declaration.
        clawde_core::dead_code_guard::assert_no_dead_pub_functions(env!("CARGO_MANIFEST_DIR"));
    }

    #[tokio::test]
    async fn test_web_setup_proxy_executes_named_command() {
        let mut ctx = make_ctx();
        let cmd = find_command("web-setup").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        assert!(matches!(result, CommandResult::Message(_)));
    }

    #[tokio::test]
    async fn test_import_config_command_opens_overlay() {
        let mut ctx = make_ctx();
        let cmd = find_command("import-config").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        assert!(matches!(result, CommandResult::OpenImportConfigOverlay));
    }

    #[test]
    fn test_split_command_args_preserves_quoted_segments() {
        assert_eq!(
            split_command_args("create \"agent alpha\" 'second value'"),
            vec![
                "create".to_string(),
                "agent alpha".to_string(),
                "second value".to_string(),
            ]
        );
    }
    // ---- /plan tests -------------------------------------------------------

    #[tokio::test]
    async fn test_plan_command_registered() {
        assert!(find_command("plan").is_some());
    }

    #[tokio::test]
    async fn test_plan_no_args_empty_conversation() {
        let mut ctx = make_ctx();
        let cmd = find_command("plan").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        // Without a plan file, returns a message about plan status.
        match result {
            CommandResult::Message(msg) => {
                assert!(
                    msg.contains("No active plan") || msg.contains("Current Plan"),
                    "Expected plan status message, got: {}",
                    msg
                );
            }
            other => panic!("expected Message, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_plan_exit_returns_user_message() {
        let mut ctx = make_ctx();
        let cmd = find_command("plan").unwrap();
        let result = cmd.execute("exit", &mut ctx).await;
        assert!(matches!(result, CommandResult::UserMessage(_)));
    }

    #[tokio::test]
    async fn test_plan_with_description_returns_user_message() {
        let mut ctx = make_ctx();
        let cmd = find_command("plan").unwrap();
        let result = cmd.execute("refactor the auth module", &mut ctx).await;
        assert!(matches!(result, CommandResult::UserMessage(_)));
    }

    // ---- /compact tests ----------------------------------------------------

    #[tokio::test]
    async fn test_compact_empty_conversation() {
        let mut ctx = make_ctx();
        let cmd = find_command("compact").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        match result {
            CommandResult::Message(msg) => {
                assert!(
                    msg.contains("fewer than 2 messages"),
                    "Expected 'fewer than 2 messages', got: {}",
                    msg
                );
            }
            other => panic!("expected Message, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_compact_single_message() {
        let mut ctx = make_ctx();
        ctx.messages.push(Message::user("Hello"));
        let cmd = find_command("compact").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        match result {
            CommandResult::Message(msg) => {
                assert!(
                    msg.contains("fewer than 2 messages"),
                    "Expected 'fewer than 2 messages', got: {}",
                    msg
                );
            }
            other => panic!("expected Message, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_compact_two_messages_with_canned_provider() {
        let mut ctx = make_ctx_with_canned_provider();
        ctx.messages.push(Message::user("Hello"));
        ctx.messages.push(Message::assistant("Hi there!"));
        let cmd = find_command("compact").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        // The canned provider always succeeds, so /compact preview returns
        // a Message with the generated summary.
        match result {
            CommandResult::Message(msg) => {
                assert!(
                    msg.contains("Conversation Compact"),
                    "Expected compact message, got: {}",
                    msg
                );
                assert!(
                    msg.contains("Canned test summary response."),
                    "Expected canned summary text, got: {}",
                    msg
                );
            }
            other => panic!("expected Message, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_compact_inherits_session_effort_override() {
        let provider = std::sync::Arc::new(CannedProvider::new());
        let mut ctx = make_ctx();
        ctx.test_provider = Some(provider.clone());
        ctx.effort = Some(clawde_core::effort::EffortLevel::High);
        ctx.messages.push(Message::user("Hello"));
        ctx.messages.push(Message::assistant("Hi there!"));
        let cmd = find_command("compact").unwrap();
        let _ = cmd.execute("", &mut ctx).await;
        assert_eq!(
            *provider.last_effort.lock().unwrap(),
            Some(clawde_core::effort::EffortLevel::High),
            "/compact must propagate the session effort override into the request"
        );
    }

    #[tokio::test]
    async fn test_compact_send_with_two_messages_with_canned_provider() {
        let mut ctx = make_ctx_with_canned_provider();
        ctx.messages.push(Message::user("Hello"));
        ctx.messages.push(Message::assistant("Hi there!"));
        let cmd = find_command("compact").unwrap();
        let result = cmd.execute("send", &mut ctx).await;
        // The canned provider always succeeds, so /compact send injects the
        // summary as a user message.
        match result {
            CommandResult::UserMessage(msg) => {
                assert!(
                    msg.contains("[Compact requested"),
                    "Expected injected compact instruction, got: {}",
                    msg
                );
            }
            other => panic!("expected UserMessage, got {:?}", other),
        }
    }

    #[test]
    fn test_build_conversation_transcript_empty() {
        let result = build_conversation_transcript(&[]);
        assert_eq!(result, "");
    }

    #[test]
    fn test_build_conversation_transcript_single_user() {
        let msgs = vec![Message::user("Hello world")];
        let result = build_conversation_transcript(&msgs);
        assert!(result.contains("Human: Hello world"));
    }

    #[test]
    fn test_build_conversation_transcript_user_assistant() {
        let msgs = vec![
            Message::user("What is Rust?"),
            Message::assistant("Rust is a systems programming language."),
        ];
        let result = build_conversation_transcript(&msgs);
        assert!(result.contains("Human: What is Rust?"));
        assert!(result.contains("Assistant: Rust is a systems programming language."));
    }

    #[test]
    fn test_build_conversation_transcript_with_tool_call() {
        use clawde_core::types::ContentBlock;
        let blocks = vec![ContentBlock::ToolUse {
            id: "toolu_abc".to_string(),
            name: "bash".to_string(),
            input: serde_json::json!({"command": "echo hi"}),
            thought_signature: None,
        }];
        let msg = Message::assistant_blocks(blocks);
        let result = build_conversation_transcript(&[msg]);
        assert!(
            result.contains("Tool Call: bash (id=toolu_abc)"),
            "Result: {}",
            result
        );
        assert!(result.contains("echo hi"), "Result: {}", result);
    }

    #[test]
    fn test_build_conversation_transcript_with_tool_result() {
        use clawde_core::types::{ContentBlock, ToolResultContent};
        let blocks = vec![ContentBlock::ToolResult {
            tool_use_id: "toolu_abc".to_string(),
            content: ToolResultContent::Text("success".to_string()),
            is_error: Some(false),
        }];
        let msg = Message::user_blocks(blocks);
        let result = build_conversation_transcript(&[msg]);
        assert!(
            result.contains("Tool Result (id=toolu_abc)"),
            "Result: {}",
            result
        );
        assert!(result.contains("success"), "Result: {}", result);
    }

    #[test]
    fn test_build_conversation_transcript_with_tool_error() {
        use clawde_core::types::{ContentBlock, ToolResultContent};
        let blocks = vec![ContentBlock::ToolResult {
            tool_use_id: "toolu_err".to_string(),
            content: ToolResultContent::Text("command not found".to_string()),
            is_error: Some(true),
        }];
        let msg = Message::user_blocks(blocks);
        let result = build_conversation_transcript(&[msg]);
        assert!(
            result.contains("[ERROR]"),
            "Error flag missing in: {}",
            result
        );
        assert!(result.contains("command not found"), "Result: {}", result);
    }

    #[test]
    fn test_build_conversation_transcript_utf8_truncation() {
        let long_text = "\\u{1F600}".repeat(45_000);
        let msgs = vec![Message::user(&long_text)];
        let result = build_conversation_transcript(&msgs);
        assert!(
            result.contains("TRANSCRIPT TRUNCATED"),
            "Expected truncation marker, got {} chars: {}...",
            result.len(),
            &result[..result.len().min(100)]
        );
    }

    // ---- /ctx-viz tests ----------------------------------------------------

    #[test]
    fn test_ctx_viz_command_registered() {
        assert!(find_command("ctx-viz").is_some());
    }

    #[test]
    fn test_ctx_viz_alias_ctx() {
        assert!(find_command("ctx").is_some());
    }

    #[test]
    fn test_ctx_viz_alias_context_visualizer() {
        assert!(find_command("context-visualizer").is_some());
    }

    #[tokio::test]
    async fn test_ctx_viz_empty_conversation_returns_message() {
        let mut ctx = make_ctx();
        let cmd = find_command("ctx-viz").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        match result {
            CommandResult::Message(msg) => {
                assert!(
                    msg.starts_with("Context:"),
                    "Expected 'Context:' prefix, got: {}",
                    msg
                );
                assert!(msg.contains("tokens"), "Expected token info in: {}", msg);
                assert!(msg.contains("msgs"), "Expected msg count in: {}", msg);
            }
            other => panic!("expected Message, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_ctx_viz_shows_model_and_window() {
        let mut ctx = make_ctx();
        let cmd = find_command("ctx-viz").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("tokens"), "No token info in: {}", msg);
                assert!(msg.contains("tools"), "No tool count in: {}", msg);
            }
            other => panic!("expected Message, got {:?}", other),
        }
    }

    // ---- /summary tests ----------------------------------------------------

    #[test]
    fn test_summary_command_registered() {
        assert!(find_command("summary").is_some());
    }

    #[tokio::test]
    async fn test_summary_empty_conversation() {
        let mut ctx = make_ctx();
        let cmd = find_command("summary").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        match result {
            CommandResult::Message(msg) => {
                assert!(
                    msg.contains("No messages in conversation yet"),
                    "Expected empty conversation message, got: {}",
                    msg
                );
            }
            other => panic!("expected Message, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_summary_single_message_falls_back() {
        // count < 3 short-circuits before any provider is resolved, so this
        // never touches the network.
        let mut ctx = make_ctx();
        ctx.messages.push(Message::user("Hello"));
        let cmd = find_command("summary").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        match result {
            CommandResult::UserMessage(msg) => {
                assert!(
                    msg.contains("summary"),
                    "Expected UserMessage with summary instruction, got: {}",
                    msg
                );
            }
            other => panic!("expected UserMessage (fallback), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_summary_two_messages_falls_back() {
        // count < 3 short-circuits before any provider is resolved, so this
        // never touches the network.
        let mut ctx = make_ctx();
        ctx.messages.push(Message::user("Hello"));
        ctx.messages.push(Message::assistant("Hi there!"));
        let cmd = find_command("summary").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        match result {
            CommandResult::UserMessage(msg) => {
                assert!(
                    msg.contains("summary"),
                    "Expected UserMessage with summary instruction, got: {}",
                    msg
                );
            }
            other => panic!("expected UserMessage (fallback), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_summary_with_focus_arg_small() {
        // count < 3 short-circuits before any provider is resolved, so this
        // never touches the network.
        let mut ctx = make_ctx();
        ctx.messages.push(Message::user("Hello"));
        let cmd = find_command("summary").unwrap();
        let result = cmd.execute("decisions", &mut ctx).await;
        match result {
            CommandResult::UserMessage(msg) => {
                assert!(
                    msg.contains("Focus on: decisions"),
                    "Expected focus instruction in message, got: {}",
                    msg
                );
            }
            other => panic!("expected UserMessage, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_summary_three_messages_with_canned_provider() {
        let mut ctx = make_ctx_with_canned_provider();
        ctx.messages.push(Message::user("Hello"));
        ctx.messages.push(Message::assistant("Hi there!"));
        ctx.messages.push(Message::user("What is Rust?"));
        let cmd = find_command("summary").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        // The canned provider always succeeds, so /summary returns a Message
        // with the generated summary.
        match result {
            CommandResult::Message(msg) => {
                assert!(
                    msg.contains("Conversation Summary"),
                    "Expected summary message, got: {}",
                    msg
                );
                assert!(
                    msg.contains("Canned test summary response."),
                    "Expected canned summary text, got: {}",
                    msg
                );
            }
            other => panic!("expected Message, got {:?}", other),
        }
    }

    #[test]
    fn test_new_commands_present() {
        assert!(
            find_command("summary").is_some(),
            "/summary should be registered"
        );
        assert!(
            find_command("ctx-viz").is_some(),
            "/ctx-viz should be registered"
        );
        assert!(find_command("plan").is_some(), "/plan should be registered");
        assert!(
            find_command("compact").is_some(),
            "/compact should be registered"
        );
    }

    // ---- /auto-compact tests (Gap 6: ConfigChange sync regression) ----------

    #[tokio::test]
    async fn auto_compact_command_toggle_on_from_off() {
        let mut ctx = make_ctx();
        ctx.config.auto_compact = false;
        let cmd = AutoCompactCommand;

        let result = cmd.execute("on", &mut ctx).await;

        match result {
            CommandResult::ConfigChangeMessage(new_cfg, msg) => {
                assert!(
                    new_cfg.auto_compact,
                    "ConfigChangeMessage should have auto_compact = true after /auto-compact on"
                );
                assert!(
                    msg.to_lowercase().contains("enabled"),
                    "Status message should indicate auto-compact was enabled, got: {msg}"
                );
            }
            other => panic!("Expected ConfigChangeMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn auto_compact_command_toggle_off_from_on() {
        let mut ctx = make_ctx();
        ctx.config.auto_compact = true;
        let cmd = AutoCompactCommand;

        let result = cmd.execute("off", &mut ctx).await;

        match result {
            CommandResult::ConfigChangeMessage(new_cfg, msg) => {
                assert!(
                    !new_cfg.auto_compact,
                    "ConfigChangeMessage should have auto_compact = false after /auto-compact off"
                );
                assert!(
                    msg.to_lowercase().contains("disabled"),
                    "Status message should indicate auto-compact was disabled, got: {msg}"
                );
            }
            other => panic!("Expected ConfigChangeMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn auto_compact_command_toggle_no_args_flips_state() {
        let mut ctx = make_ctx();
        ctx.config.auto_compact = false;
        let cmd = AutoCompactCommand;

        let result = cmd.execute("", &mut ctx).await;

        match result {
            CommandResult::ConfigChangeMessage(new_cfg, _msg) => {
                assert!(
                    new_cfg.auto_compact,
                    "Toggle from off should produce auto_compact = true"
                );
            }
            other => panic!("Expected ConfigChangeMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn auto_compact_command_noop_when_already_in_state() {
        let mut ctx = make_ctx();
        ctx.config.auto_compact = true;
        let cmd = AutoCompactCommand;

        let result = cmd.execute("on", &mut ctx).await;

        match result {
            CommandResult::Message(msg) => {
                assert!(
                    msg.to_lowercase().contains("already"),
                    "No-op should say 'already enabled', got: {msg}"
                );
                // Verify config was NOT changed by the no-op.
                assert!(
                    ctx.config.auto_compact,
                    "auto_compact should still be true after no-op"
                );
            }
            other => panic!("Expected Message for no-op, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn auto_compact_command_rejects_unknown_arg() {
        let mut ctx = make_ctx();
        let cmd = AutoCompactCommand;

        let result = cmd.execute("maybe", &mut ctx).await;

        match result {
            CommandResult::Error(msg) => {
                assert!(
                    msg.contains("Unknown"),
                    "Error message should say 'Unknown', got: {msg}"
                );
            }
            other => panic!("Expected Error for unknown arg, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn thinking_command_returns_session_actions() {
        let mut ctx = make_ctx();
        let command = ThinkingCommand;

        assert!(matches!(
            command.execute("on", &mut ctx).await,
            CommandResult::ThinkingChange(ThinkingAction::On)
        ));
        assert!(matches!(
            command.execute("off", &mut ctx).await,
            CommandResult::ThinkingChange(ThinkingAction::Off)
        ));
        assert!(matches!(
            command.execute("", &mut ctx).await,
            CommandResult::ThinkingChange(ThinkingAction::Status)
        ));
    }
    #[tokio::test]
    async fn thinking_command_rejects_unknown_arguments() {
        let mut ctx = make_ctx();
        let result = ThinkingCommand.execute("sometimes", &mut ctx).await;
        assert!(
            matches!(result, CommandResult::Error(message) if message.contains("/thinking on"))
        );
    }

    #[tokio::test]
    async fn config_command_reports_persisted_default_effort() {
        let mut ctx = make_ctx();
        ctx.config.default_effort = Some(clawde_core::effort::EffortLevel::High);
        let result = ConfigCommand.execute("get default-effort", &mut ctx).await;
        assert!(
            matches!(result, CommandResult::Message(message) if message == "default-effort = high")
        );
    }
}
// ---- arg_completions system tests ----------------------------------

#[test]
fn get_arg_completions_filters_case_insensitive() {
    let completions = crate::get_arg_completions("effort", "m");
    let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
    assert!(values.contains(&"medium"), "should match 'medium'");
    assert!(values.contains(&"minimal"), "should match 'minimal'");
    assert!(values.contains(&"max"), "should match 'max'");
    assert!(!values.contains(&"low"), "should not match 'low'");
    assert!(!values.contains(&"high"), "should not match 'high'");
}

#[test]
fn get_arg_completions_empty_partial_returns_all() {
    let completions = crate::get_arg_completions("effort", "");
    assert_eq!(completions.len(), 8, "should return all 8 levels");
}

#[test]
fn get_arg_completions_unknown_command_returns_empty() {
    let completions = crate::get_arg_completions("nonexistent", "x");
    assert!(completions.is_empty());
}

#[test]
fn get_arg_completions_exact_match() {
    let completions = crate::get_arg_completions("effort", "high");
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].value, "high");
}

#[test]
fn get_arg_completions_no_match_returns_empty() {
    let completions = crate::get_arg_completions("effort", "zzz");
    assert!(completions.is_empty());
}

#[test]
fn auto_compact_completions_on_off() {
    let completions = crate::get_arg_completions("auto-compact", "");
    assert_eq!(completions.len(), 2);
    let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
    assert!(values.contains(&"on"));
    assert!(values.contains(&"off"));
}

#[test]
fn theme_completions_all_builtins_and_subcommands() {
    // The theme command reads custom themes from the config dir, which the
    // CLAWDE_HOME env var redirects. TestHome acquires the shared
    // CLAWDE_HOME_LOCK and points the env var at a fresh temp dir, so no
    // user themes leak into the count and no env race occurs.
    let _home = crate::keys::tests::TestHome::new();

    let completions = crate::get_arg_completions("theme", "");
    // 9 built-ins + list/create/delete subcommands, with no custom themes
    // in the isolated temp home.
    assert_eq!(completions.len(), 12);
    let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
    assert!(values.contains(&"default"));
    assert!(values.contains(&"dark"));
    assert!(values.contains(&"light"));
    assert!(values.contains(&"catppuccin"));
    assert!(values.contains(&"list"));
    assert!(values.contains(&"create"));
    assert!(values.contains(&"delete"));
}

#[test]
fn diff_completions_flags() {
    let completions = crate::get_arg_completions("diff", "");
    let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
    assert!(values.contains(&"--stat"));
    assert!(values.contains(&"--staged"));
}

#[test]
fn multi_argument_completions_bridge_to_next_stage() {
    let key_hint = crate::keys::KeysCommand.arg_completions("set firecrawl ");
    assert!(key_hint.iter().any(|completion| {
        completion.value == "set firecrawl <api-key>" && !completion.available
    }));

    let config_values = crate::config_cmd::ConfigCommand.arg_completions("set theme d");
    assert!(config_values
        .iter()
        .any(|completion| completion.value == "set theme dark"));
}

#[test]
fn free_form_next_argument_shows_placeholder_hint_until_typed() {
    // /keys: the api-key hint is present while the value is empty, faded and
    // non-selectable, and disappears once the user starts typing the key.
    let empty = crate::keys::KeysCommand.arg_completions("set firecrawl ");
    assert!(empty
        .iter()
        .any(|c| { c.value == "set firecrawl <api-key>" && !c.available }));
    let typed = crate::keys::KeysCommand.arg_completions("set firecrawl sk-abc");
    assert!(
        !typed.iter().any(|c| c.value.contains("<api-key>")),
        "hint must disappear once the key is typed: {:?}",
        typed.iter().map(|c| c.value.as_str()).collect::<Vec<_>>()
    );

    // /config set model: free-form model ID gets a dimmed <model> hint.
    let config_empty = crate::config_cmd::ConfigCommand.arg_completions("set model ");
    assert!(config_empty
        .iter()
        .any(|c| { c.value == "set model <model>" && !c.available }));
    let config_typed = crate::config_cmd::ConfigCommand.arg_completions("set model claude");
    assert!(!config_typed.iter().any(|c| c.value.contains("<model>")));

    // /export --output: free-form file path gets a dimmed <file path> hint.
    let export_empty = crate::export::ExportCommand.arg_completions("--output ");
    assert!(export_empty
        .iter()
        .any(|c| { c.value == "--output <file path>" && !c.available }));
    let export_typed = crate::export::ExportCommand.arg_completions("--output chat");
    assert!(!export_typed.iter().any(|c| c.value.contains("<file path>")));

    // /login --label: free-form profile name gets a dimmed <name> hint.
    let login_empty = crate::accounts::LoginCommand.arg_completions("--label ");
    assert!(login_empty
        .iter()
        .any(|c| { c.value == "--label <name>" && !c.available }));
}

#[test]
fn enum_stages_keep_selectable_values_without_placeholder_hint() {
    // Enum-valued stages complete real values; no faded placeholder appears.
    let theme = crate::config_cmd::ConfigCommand.arg_completions("set theme ");
    assert!(theme
        .iter()
        .any(|c| c.value == "set theme dark" && c.available));
    assert!(
        !theme.iter().any(|c| !c.available),
        "enum stage must not carry a faded hint: {:?}",
        theme.iter().map(|c| c.value.as_str()).collect::<Vec<_>>()
    );

    let format = crate::export::ExportCommand.arg_completions("--format ");
    assert!(format
        .iter()
        .any(|c| c.value == "--format markdown" && c.available));
    assert!(
        !format.iter().any(|c| !c.available),
        "format enum stage must not carry a faded hint: {:?}",
        format.iter().map(|c| c.value.as_str()).collect::<Vec<_>>()
    );
}

#[test]
fn get_arg_completions_model_with_capability_flag_returns_vision() {
    let completions = crate::get_arg_completions("model", "--capability vision");
    let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
    assert!(
        values.contains(&"vision"),
        "should return 'vision' completion, got: {:?}",
        values
    );
}

#[test]
fn get_arg_completions_model_with_capability_short_flag_returns_reasoning() {
    let completions = crate::get_arg_completions("model", "-c reasoning");
    let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
    assert!(values.contains(&"reasoning"), "should return 'reasoning'");
}

#[test]
fn get_arg_completions_model_with_capability_partial_prefix_filters() {
    let completions = crate::get_arg_completions("model", "--capability vis");
    let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
    assert_eq!(values, vec!["vision"], "only 'vision' starts with 'vis'");
}

#[test]
fn get_arg_completions_model_no_capability_returns_model_ids() {
    // Without --capability prefix, returns model IDs (not capability names).
    let completions = crate::get_arg_completions("model", "claude");
    let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
    assert!(
        !values.contains(&"vision"),
        "should NOT return capability names"
    );
    assert!(
        values.iter().any(|v| v.contains("claude")),
        "should contain model IDs matching 'claude', got: {:?}",
        values
    );
}

#[test]
fn get_arg_completions_image_with_capability_flag_returns_tools() {
    let completions = crate::get_arg_completions("image", "--capability tools");
    let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
    assert!(
        values.contains(&"tools"),
        "should return 'tools' completion, got: {:?}",
        values
    );
}

#[test]
fn get_arg_completions_models_with_capability_flag_returns_audio() {
    let completions = crate::get_arg_completions("models", "--capability audio");
    let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
    assert!(
        values.contains(&"audio"),
        "should return 'audio' completion, got: {:?}",
        values
    );
}

#[test]
fn get_arg_completions_models_no_flag_returns_empty() {
    // Without --capability, /models has no arg completions.
    let completions = crate::get_arg_completions("models", "");
    assert!(
        completions.is_empty(),
        "models without flag should be empty"
    );
}

#[test]
fn get_arg_completions_capability_equals_syntax_works() {
    let completions = crate::get_arg_completions("model", "--capability=vision");
    let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
    assert!(
        values.contains(&"vision"),
        "should return 'vision' completion"
    );
}

#[test]
fn get_arg_completions_cmd_name_is_case_sensitive() {
    // find_command is case-sensitive but get_arg_completions uses it directly.
    // Commands are always lowercase, so this tests the normal path.
    let completions = crate::get_arg_completions("EFFORT", "HIGH");
    // "EFFORT" won't match "effort" in find_command's case-sensitive comparison
    assert!(
        completions.is_empty(),
        "EFFORT != effort in case-sensitive find_command"
    );
}

#[test]
fn arg_completions_all_available_by_default() {
    let completions = crate::get_arg_completions("effort", "");
    for c in &completions {
        assert!(c.available, "{} should be available by default", c.value);
    }
}

#[test]
fn once_lock_caches_command_list() {
    // Call twice — the second call reuses the cached list.
    let first = crate::get_arg_completions("effort", "");
    let second = crate::get_arg_completions("effort", "");
    assert_eq!(first.len(), second.len());
    for (a, b) in first.iter().zip(second.iter()) {
        assert_eq!(a.value, b.value);
    }
}
