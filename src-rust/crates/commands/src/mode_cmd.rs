//! `/mode` — switch between mode presets (careful, fast, default, custom).
//!
//! Bare `/mode` shows the current mode and lists available presets.
//! `/mode <name>` switches to the named preset for this session.
//! `/mode <name> --turn` switches transiently for one turn only.

use crate::{CommandContext, CommandResult, SlashCommand};
use async_trait::async_trait;
use clawde_core::modes::{all_modes, apply_mode, find_mode};

pub struct ModeCommand;

#[async_trait]
impl SlashCommand for ModeCommand {
    fn name(&self) -> &'static str {
        "mode"
    }

    fn aliases(&self) -> Vec<&'static str> {
        vec!["preset"]
    }

    fn description(&self) -> &'static str {
        "Switch mode preset (careful, fast, default, ...)"
    }

    fn help(&self) -> &'static str {
        "Usage: /mode [name] [--turn]\n\n\
         Switch the active mode preset for this session.\n\n\
         /mode                          — show current mode and list available presets\n\
         /mode careful                  — switch to the 'careful' preset\n\
         /mode fast                     — switch to the 'fast' preset\n\
         /mode default                  — reset to default behavior\n\
         /mode careful --turn           — apply 'careful' for this turn only (transient)\n\n\
         Mode presets bundle config knobs (effort, permission mode, output style)\n\
         with decision-rule guidance (plan posture, check-in cadence, ask-on-ambiguity).\n\n\
         Custom modes can be defined as .json files in ~/.clawde/modes/ or\n\
         .clawde/modes/ (project-local). See the modes spec for the schema."
    }

    fn arg_completions(&self, _partial: &str) -> Vec<crate::ArgCompletion> {
        vec![
            crate::ArgCompletion {
                value: "default".to_string(),
                description: "Reset to default behavior".to_string(),
                available: true,
            },
            crate::ArgCompletion {
                value: "careful".to_string(),
                description: "Plan before writing, ask on design decisions".to_string(),
                available: true,
            },
            crate::ArgCompletion {
                value: "fast".to_string(),
                description: "Low reasoning effort, minimal check-ins".to_string(),
                available: true,
            },
        ]
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let args = args.trim();

        if args.is_empty() {
            return self.show_status(ctx);
        }

        // Parse `--turn` flag for transient mode.
        let (name, transient) = if let Some(idx) = args.rfind("--turn") {
            let name = args[..idx].trim();
            (name, true)
        } else {
            (args, false)
        };

        if name.is_empty() {
            return self.show_status(ctx);
        }

        // Resolve the mode from built-ins + user-defined.
        let modes = all_modes(&ctx.working_dir);
        let Some(mode) = find_mode(&modes, name) else {
            let available: Vec<&str> = modes.iter().map(|m| m.name.as_str()).collect();
            return CommandResult::Error(format!(
                "Unknown mode '{}'. Available: {}",
                name,
                available.join(", ")
            ));
        };

        if transient {
            // Transient: apply for this turn only. Store in config for the
            // query loop's `effective_mode_name_for_turn` to pick up.
            ctx.config.mode = Some(mode.name.clone());
            CommandResult::Message(format!(
                "Mode '{}' applied transiently for this turn. It will revert after.",
                mode.label
            ))
        } else {
            // Persistent: apply the mode's config knobs and store the name.
            let mut new_config = ctx.config.clone();
            apply_mode(&mut new_config, mode);
            new_config.mode = Some(mode.name.clone());
            let mut msg = format!("Mode switched to '{}' — {}", mode.label, mode.description);

            // Show what changed.
            let mut changes = Vec::new();
            if let Some(ref eff) = mode.effort {
                changes.push(format!("effort: {:?}", eff));
            }
            if mode.permission_mode.is_some() {
                changes.push("permissions: plan".to_string());
            }
            if let Some(ref style) = mode.output_style {
                changes.push(format!("output style: {}", style));
            }
            if !changes.is_empty() {
                msg.push_str(&format!("\nChanges: {}", changes.join(", ")));
            }

            // Show the prompt guidance if any.
            if let Some(ref block) = clawde_core::modes::mode_prompt_block(mode) {
                let preview = block.lines().take(3).collect::<Vec<_>>().join("\n");
                msg.push_str(&format!("\nGuidance:\n{}", preview));
            }

            // Return ConfigChangeMessage so the CLI propagates the mode to
            // base_query_config.mode, tool_ctx, and app.config immediately.
            CommandResult::ConfigChangeMessage(new_config, msg)
        }
    }
}

impl ModeCommand {
    fn show_status(&self, ctx: &CommandContext) -> CommandResult {
        let current = ctx.config.mode.as_deref().unwrap_or("default");
        let modes = all_modes(&ctx.working_dir);
        let mut out = format!("Current mode: {}\n\nAvailable presets:\n", current);
        for mode in &modes {
            let marker = if mode.name == current { " *" } else { "" };
            out.push_str(&format!(
                "  {}{} — {}\n",
                mode.name, marker, mode.description
            ));
        }
        out.push_str("\nUse /mode <name> to switch, or type 'mode:<name>' inline for one turn.");
        CommandResult::Message(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawde_core::config::Config;
    use clawde_core::cost::CostTracker;

    fn ctx() -> CommandContext {
        CommandContext {
            config: Config::default(),
            cost_tracker: CostTracker::new(),
            messages: vec![],
            working_dir: std::env::current_dir().unwrap_or_else(|_| "/tmp".into()),
            session_id: "test".to_string(),
            session_title: None,
            remote_session_url: None,
            mcp_manager: None,
            mcp_auth_runner: None,
            provider_registry: None,
            test_provider: None,
            effort: None,
            tool_use_tracker: None,
            autonomy: None,
        }
    }

    #[tokio::test]
    async fn bare_mode_shows_status() {
        let mut c = ctx();
        let result = ModeCommand.execute("", &mut c).await;
        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("Current mode:"));
                assert!(msg.contains("careful"));
                assert!(msg.contains("fast"));
            }
            other => panic!("expected Message, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn switch_to_careful() {
        let mut c = ctx();
        let result = ModeCommand.execute("careful", &mut c).await;
        match result {
            CommandResult::ConfigChangeMessage(new_cfg, msg) => {
                assert!(msg.contains("Careful"), "{msg}");
                assert_eq!(new_cfg.mode.as_deref(), Some("careful"));
            }
            other => panic!("expected ConfigChangeMessage, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn switch_to_fast() {
        let mut c = ctx();
        let result = ModeCommand.execute("fast", &mut c).await;
        match result {
            CommandResult::ConfigChangeMessage(new_cfg, msg) => {
                assert!(msg.contains("Fast"), "{msg}");
                assert_eq!(new_cfg.mode.as_deref(), Some("fast"));
            }
            other => panic!("expected ConfigChangeMessage, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn switch_to_default_resets() {
        let mut c = ctx();
        // First switch to careful via the command.
        let _ = ModeCommand.execute("careful", &mut c).await;

        let result = ModeCommand.execute("default", &mut c).await;
        match result {
            CommandResult::ConfigChangeMessage(new_cfg, msg) => {
                assert!(msg.contains("Default"), "{msg}");
                assert_eq!(new_cfg.mode.as_deref(), Some("default"));
            }
            other => panic!("expected ConfigChangeMessage, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn unknown_mode_returns_error() {
        let mut c = ctx();
        let result = ModeCommand.execute("nonexistent", &mut c).await;
        match result {
            CommandResult::Error(msg) => {
                assert!(msg.contains("Unknown mode"), "{msg}");
                assert!(msg.contains("careful"), "{msg}");
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn transient_mode_flag() {
        let mut c = ctx();
        let result = ModeCommand.execute("careful --turn", &mut c).await;
        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("transiently"), "{msg}");
                assert_eq!(c.config.mode.as_deref(), Some("careful"));
            }
            other => panic!("expected Message, got {:?}", other),
        }
    }

    #[test]
    fn completes_include_builtins() {
        let completions = ModeCommand.arg_completions("");
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"careful"));
        assert!(values.contains(&"fast"));
        assert!(values.contains(&"default"));
    }
}
