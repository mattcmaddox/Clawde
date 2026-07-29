// `/permissions` command.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;
use std::sync::OnceLock;

pub struct PermissionsCommand;

/// Cached list of all built-in tool names for `allow`/`deny` completions.
fn tool_names() -> &'static Vec<String> {
    static TOOL_NAMES: OnceLock<Vec<String>> = OnceLock::new();
    TOOL_NAMES.get_or_init(|| {
        clawde_tools::all_tools()
            .into_iter()
            .map(|t| t.name().to_string())
            .collect()
    })
}

// ---- /permissions --------------------------------------------------------

#[async_trait]
impl SlashCommand for PermissionsCommand {
    fn name(&self) -> &str {
        "permissions"
    }
    fn description(&self) -> &str {
        "View or change tool permission settings"
    }
    fn arg_completions(&self, partial: &str) -> Vec<ArgCompletion> {
        let mut completions = vec![
            ArgCompletion {
                value: "set".into(),
                description:
                    "Set permission mode (default, accept-edits, bypass-permissions, plan)".into(),
                available: true,
            },
            ArgCompletion {
                value: "allow".into(),
                description: "Allow a specific tool".into(),
                available: true,
            },
            ArgCompletion {
                value: "deny".into(),
                description: "Deny a specific tool".into(),
                available: true,
            },
            ArgCompletion {
                value: "reset".into(),
                description: "Clear all permission overrides".into(),
                available: true,
            },
        ];
        // Second-level completions: /permissions set <mode>
        if partial == "set" || partial.starts_with("set ") {
            completions.push(ArgCompletion {
                value: "set default".into(),
                description: "Default permission mode".into(),
                available: true,
            });
            completions.push(ArgCompletion {
                value: "set accept-edits".into(),
                description: "Auto-accept file edits".into(),
                available: true,
            });
            completions.push(ArgCompletion {
                value: "set bypass-permissions".into(),
                description: "Bypass all permissions".into(),
                available: true,
            });
            completions.push(ArgCompletion {
                value: "set plan".into(),
                description: "Plan mode".into(),
                available: true,
            });
        }
        // Second-level completions: /permissions allow <tool> and deny <tool>
        // Values must include the subcommand prefix so the get_arg_completions
        // prefix-match filter works correctly (e.g. "allow Ba" matches "allow Bash").
        let prefix = if partial == "allow" || partial.starts_with("allow ") {
            Some("allow ")
        } else if partial == "deny" || partial.starts_with("deny ") {
            Some("deny ")
        } else {
            None
        };
        if let Some(pfx) = prefix {
            for name in tool_names() {
                completions.push(ArgCompletion {
                    value: format!("{pfx}{name}"),
                    description: String::new(),
                    available: true,
                });
            }
        }
        completions
    }
    fn help(&self) -> &str {
        "Usage: /permissions [set <mode>|allow <tool>|deny <tool>|reset]\n\n\
         Modes: default, accept-edits, bypass-permissions, plan\n\n\
         Examples:\n\
           /permissions                    — show current permissions\n\
           /permissions set accept-edits   — auto-accept file edits\n\
           /permissions allow Bash         — allow a specific tool\n\
           /permissions deny Write         — deny a specific tool\n\
           /permissions reset              — clear overrides"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let args = args.trim();

        if args.is_empty() {
            let allowed_display = if ctx.config.allowed_tools.is_empty() {
                "(all tools allowed)".to_string()
            } else {
                ctx.config.allowed_tools.join(", ")
            };
            let denied_display = if ctx.config.disallowed_tools.is_empty() {
                "(none)".to_string()
            } else {
                ctx.config.disallowed_tools.join(", ")
            };
            return CommandResult::Message(format!(
                "Permission Settings\n\
                 ───────────────────\n\
                 Mode:          {:?}\n\
                 Allowed tools: {}\n\
                 Denied tools:  {}\n\n\
                 Use /permissions set <mode> to change the permission mode.\n\
                 Use /permissions allow|deny <tool> to override individual tools.\n\
                 Use /permissions reset to clear all overrides.",
                ctx.config.permission_mode, allowed_display, denied_display,
            ));
        }

        let mut parts = args.splitn(2, ' ');
        let sub = parts.next().unwrap_or("").trim();
        let arg = parts.next().unwrap_or("").trim();

        match sub {
            "set" => {
                let mode = match arg.to_lowercase().as_str() {
                    "default" => clawde_core::config::PermissionMode::Default,
                    "accept-edits" | "accept_edits" => {
                        clawde_core::config::PermissionMode::AcceptEdits
                    }
                    "bypass-permissions" | "bypass_permissions" => {
                        clawde_core::config::PermissionMode::BypassPermissions
                    }
                    "plan" => clawde_core::config::PermissionMode::Plan,
                    _ => {
                        return CommandResult::Error(
                            "Mode must be: default, accept-edits, bypass-permissions, or plan"
                                .to_string(),
                        )
                    }
                };
                let mut new_config = ctx.config.clone();
                new_config.permission_mode = mode.clone();
                if let Err(e) = save_settings_mutation(|s| s.config.permission_mode = mode.clone())
                {
                    return CommandResult::Error(format!("Failed to save: {}", e));
                }
                CommandResult::ConfigChangeMessage(
                    new_config,
                    format!("Permission mode set to {:?}.", mode),
                )
            }
            "allow" => {
                if arg.is_empty() {
                    return CommandResult::Error("Usage: /permissions allow <tool>".to_string());
                }
                let tool = arg.to_string();
                let mut new_config = ctx.config.clone();
                if !new_config.allowed_tools.contains(&tool) {
                    new_config.allowed_tools.push(tool.clone());
                }
                new_config.disallowed_tools.retain(|t| t != &tool);
                if let Err(e) = save_settings_mutation(|s| {
                    if !s.config.allowed_tools.contains(&tool) {
                        s.config.allowed_tools.push(tool.clone());
                    }
                    s.config.disallowed_tools.retain(|t| t != &tool);
                }) {
                    return CommandResult::Error(format!("Failed to save: {}", e));
                }
                CommandResult::ConfigChangeMessage(new_config, format!("Allowed tool: {}", tool))
            }
            "deny" => {
                if arg.is_empty() {
                    return CommandResult::Error("Usage: /permissions deny <tool>".to_string());
                }
                let tool = arg.to_string();
                let mut new_config = ctx.config.clone();
                if !new_config.disallowed_tools.contains(&tool) {
                    new_config.disallowed_tools.push(tool.clone());
                }
                new_config.allowed_tools.retain(|t| t != &tool);
                if let Err(e) = save_settings_mutation(|s| {
                    if !s.config.disallowed_tools.contains(&tool) {
                        s.config.disallowed_tools.push(tool.clone());
                    }
                    s.config.allowed_tools.retain(|t| t != &tool);
                }) {
                    return CommandResult::Error(format!("Failed to save: {}", e));
                }
                CommandResult::ConfigChangeMessage(new_config, format!("Denied tool: {}", tool))
            }
            "reset" => {
                let mut new_config = ctx.config.clone();
                new_config.allowed_tools.clear();
                new_config.disallowed_tools.clear();
                new_config.permission_mode = clawde_core::config::PermissionMode::Default;
                if let Err(e) = save_settings_mutation(|s| {
                    s.config.allowed_tools.clear();
                    s.config.disallowed_tools.clear();
                    s.config.permission_mode = clawde_core::config::PermissionMode::Default;
                }) {
                    return CommandResult::Error(format!("Failed to save: {}", e));
                }
                CommandResult::ConfigChangeMessage(
                    new_config,
                    "Permissions reset to defaults.".to_string(),
                )
            }
            other => CommandResult::Error(format!(
                "Unknown subcommand '{}'. Use: /permissions [set|allow|deny|reset]",
                other
            )),
        }
    }
}
