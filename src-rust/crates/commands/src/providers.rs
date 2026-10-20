// Provider/agent commands: `/providers`, `/connect`, `/agent`.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct ProvidersCommand;
pub struct ConnectCommand;
pub struct AgentCommand;
pub struct NewAgentCommand;

// ---- /providers -------------------------------------------------------------

#[async_trait]
impl SlashCommand for ProvidersCommand {
    fn name(&self) -> &str {
        "providers"
    }
    fn description(&self) -> &str {
        "List available AI providers and their status"
    }
    fn help(&self) -> &str {
        "Usage: /providers\n\nList all providers registered in the model registry with their\nmodel counts, context windows, and pricing information."
    }

    async fn execute(&self, _args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let registry = clawde_api::ModelRegistry::new();
        let all = registry.list_all();

        if all.is_empty() {
            return CommandResult::Message("No providers available.".to_string());
        }

        // Group by provider
        use std::collections::HashMap;
        let mut by_provider: HashMap<String, Vec<_>> = HashMap::new();
        for entry in &all {
            by_provider
                .entry(entry.info.provider_id.to_string())
                .or_default()
                .push(entry);
        }

        // Enumerate providers from the registry's canonical provider table
        // (sorted by id), filtered to those that actually have models.
        let provider_entries = registry.list_providers();

        let mut lines = vec!["Available providers:\n".to_string()];
        for entry in &provider_entries {
            let provider = entry.id.to_string();
            let Some(models) = by_provider.get(&provider) else {
                continue;
            };
            lines.push(format!(
                "\n{} ({} model{})",
                if entry.name.is_empty() {
                    provider.to_uppercase()
                } else {
                    entry.name.to_uppercase()
                },
                models.len(),
                if models.len() == 1 { "" } else { "s" }
            ));
            for m in models.iter().take(3) {
                let cost_str = match (m.cost_input, m.cost_output) {
                    (Some(i), Some(o)) => format!("${:.2}/${:.2} per 1M", i, o),
                    _ => "free/local".to_string(),
                };
                lines.push(format!(
                    "  {} — {}K ctx, {}",
                    m.info.id,
                    m.info.context_window / 1000,
                    cost_str
                ));
            }
            if models.len() > 3 {
                lines.push(format!("  ... and {} more", models.len() - 3));
            }
        }

        CommandResult::Message(lines.join("\n"))
    }
}

// ---- /new-agent -----------------------------------------------------------

#[async_trait]
impl SlashCommand for NewAgentCommand {
    fn name(&self) -> &str {
        "new-agent"
    }
    fn description(&self) -> &str {
        "Create a new sub-agent"
    }
    fn help(&self) -> &str {
        "Usage: /new-agent\n\nIn the TUI this opens the agent editor pre-filled for a new agent.\nIn headless mode, create a definition by adding a markdown file to\n.clawde/agents/ (see `clawde agents create <name>` for the template)."
    }

    async fn execute(&self, _args: &str, _ctx: &mut CommandContext) -> CommandResult {
        // The TUI intercepts /new-agent before this executes and opens the
        // agent editor directly. If we get here (headless mode), point the
        // user at the template.
        CommandResult::Message(
            "Create a new agent by adding a markdown file to .clawde/agents/.\n\
             Run `clawde agents create <name>` to see the template, or use /agents\n\
             in the TUI and select 'Create new agent'."
                .to_string(),
        )
    }
}

// ---- /connect -------------------------------------------------------------

#[async_trait]
impl SlashCommand for ConnectCommand {
    fn name(&self) -> &str {
        "connect"
    }
    fn description(&self) -> &str {
        "Connect an AI provider"
    }
    fn help(&self) -> &str {
        "Usage: /connect\n\nOpens the interactive provider picker dialog.\nSelect a provider to see setup instructions."
    }

    async fn execute(&self, _args: &str, _ctx: &mut CommandContext) -> CommandResult {
        // This is handled by the TUI interceptor — opening the connect dialog.
        CommandResult::Message("Use the connect dialog to set up a provider.".to_string())
    }
}

// ---- /agent ---------------------------------------------------------------

#[async_trait]
impl SlashCommand for AgentCommand {
    fn name(&self) -> &str {
        "agent"
    }
    fn description(&self) -> &str {
        "List available agents or get info about a specific agent"
    }
    fn arg_completions(&self, _partial: &str) -> Vec<ArgCompletion> {
        use std::sync::OnceLock;
        static AGENTS: OnceLock<Vec<(String, String)>> = OnceLock::new();
        let agents = AGENTS.get_or_init(|| {
            clawde_core::default_agents()
                .into_iter()
                .filter(|(_, d)| d.visible)
                .map(|(name, def)| (name, def.description.unwrap_or_default()))
                .collect()
        });
        agents
            .iter()
            .map(|(name, desc)| ArgCompletion {
                value: name.clone(),
                description: desc.clone(),
                available: true,
            })
            .collect()
    }
    fn help(&self) -> &str {
        "Usage: /agent [name]\n\nWithout arguments, lists every available agent.\nWith a name, shows that agent's details.\n\nIn the TUI this opens the /agents popup (pre-selected on <name>).\n\n         To use an agent, start Clawde with: --agent <name>"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        // One loader for every surface: built-in defaults, the `agents` map in
        // settings.json, and markdown files under ~/.clawde/agents and
        // .clawde/agents. Precedence is resolved there, so a shadowed
        // definition is listed as such rather than duplicated silently.
        let defs =
            clawde_tui::agents_view::load_agent_definitions(&ctx.working_dir, &ctx.config.agents);
        let agent_name = args.trim();

        if agent_name.is_empty() {
            let mut listed: Vec<&clawde_tui::agents_view::AgentDefinition> = defs
                .iter()
                .filter(|def| def.visible && def.shadowed_by.is_none())
                .collect();
            listed.sort_by(|a, b| a.name.cmp(&b.name));

            let mut output = format!("Available agents ({}):\n\n", listed.len());
            for def in listed {
                output.push_str(&format!(
                    "  @{} — {}\n    access: {}  ·  source: {}\n",
                    def.name,
                    def.description,
                    def.access_tier(),
                    def.source_label(),
                ));
            }
            output.push_str(
                "\nUse --agent <name> when starting Clawde, or /agents in the TUI to edit them.",
            );
            CommandResult::Message(output)
        } else if let Some(def) = defs.iter().find(|def| def.name == agent_name) {
            let mut output = format!("Agent: @{}\n", agent_name);
            for (label, value) in clawde_tui::agents_view::agent_detail_fields(def) {
                if label == "Name" {
                    continue;
                }
                output.push_str(&format!("{}: {}\n", label, value));
            }
            if !def.description.is_empty() {
                output.push_str(&format!("\nDescription:\n  {}\n", def.description));
            }
            if !def.instructions.is_empty() {
                output.push_str(&format!(
                    "\nSystem prompt prefix:\n  {}\n",
                    def.instructions
                ));
            }
            if let Some(shadow) = &def.shadowed_by {
                output.push_str(&format!(
                    "\n⚠ This definition is shadowed by {}; activating @{} runs that one.",
                    shadow, agent_name
                ));
            }
            output.push_str(&format!("\nTo activate: clawde --agent {}", agent_name));
            CommandResult::Message(output)
        } else {
            CommandResult::Error(format!(
                "Unknown agent '{}'. Run /agent to see available agents.",
                agent_name
            ))
        }
    }
}
