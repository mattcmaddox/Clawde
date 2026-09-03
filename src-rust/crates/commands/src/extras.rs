// Assorted commands: `/advisor`, `/fast`, `/color` (full).
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct AdvisorCommand;
pub struct FastCommand;
pub struct ImageCommand;
pub struct ColorSetCommand;
pub struct OllamaModeCommand;

// ---- /advisor ------------------------------------------------------------

#[async_trait]
impl SlashCommand for AdvisorCommand {
    fn name(&self) -> &str {
        "advisor"
    }
    fn description(&self) -> &str {
        "Set or unset the server-side advisor model"
    }
    fn help(&self) -> &str {
        "Usage: /advisor [<model>|off|unset]\n\n\
         Sets the advisor model used for server-side suggestions.\n\
         Examples:\n\
           /advisor claude-opus-4-6   — set advisor model\n\
           /advisor off               — disable the advisor\n\
           /advisor                   — show current advisor setting"
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let arg = args.trim();
        let settings_dir = clawde_core::config::Settings::config_dir();
        let settings_path = settings_dir.join("settings.json");

        // Read or create settings JSON
        let mut settings_val: serde_json::Value = settings_path
            .exists()
            .then(|| std::fs::read_to_string(&settings_path).ok())
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| serde_json::json!({}));

        match arg {
            "" => {
                let current = settings_val
                    .get("advisorModel")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(not set)");
                CommandResult::Message(format!("Advisor model: {current}"))
            }
            "off" | "unset" | "none" => {
                settings_val
                    .as_object_mut()
                    .map(|m| m.remove("advisorModel"));
                if let Ok(json) = serde_json::to_string_pretty(&settings_val) {
                    let _ = std::fs::write(&settings_path, json);
                }
                CommandResult::Message("Advisor model unset.".to_string())
            }
            model => {
                // Basic validation: must look like a model identifier
                if model.starts_with("claude-") || model.contains('/') {
                    settings_val["advisorModel"] = serde_json::Value::String(model.to_string());
                    if let Ok(json) = serde_json::to_string_pretty(&settings_val) {
                        let _ = std::fs::write(&settings_path, json);
                    }
                    CommandResult::Message(format!("Advisor model set to: {model}"))
                } else {
                    CommandResult::Message(format!(
                        "Unknown model '{model}'. Model IDs should start with 'claude-'.\n\
                         Use /model to see available models."
                    ))
                }
            }
        }
    }
}

// ---- /image --------------------------------------------------------------

#[async_trait]
impl SlashCommand for ImageCommand {
    fn name(&self) -> &str {
        "image"
    }
    fn description(&self) -> &str {
        "Switch to a vision-capable (or other capability) model"
    }
    fn arg_completions(&self, partial: &str) -> Vec<ArgCompletion> {
        // If the user is typing --capability <value>, return capability names.
        if let Some(cap_val) = partial
            .strip_prefix("--capability ")
            .or_else(|| partial.strip_prefix("-c "))
            .or_else(|| partial.strip_prefix("--capability="))
        {
            crate::capability_arg_completions(cap_val)
        } else {
            vec![]
        }
    }
    fn help(&self) -> &str {
        "Usage: /image [--capability <cap>]\n\n\
         Switches to the best capability-matched model for the current provider.\n\
         Defaults to vision when no --capability flag is given.\n\
         After switching, paste an image with Ctrl+V to include it in your prompt.\n\n\
         Examples:\n\
           /image                          — switch to a vision model\n\
           /image --capability audio       — switch to an audio-capable model\n\
           /image -c tools                 — switch to a tool-calling model"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let trimmed = args.trim();

        // --help: show available capability flags.
        if trimmed == "--help" || trimmed == "-h" {
            return CommandResult::Message(format!(
                "/image [--capability <cap>]\n\n\
                     Switches to the best model with the given capability.\n\
                     Defaults to vision when no flag is given.\n\
                     After switching, paste an image with Ctrl+V to include it in your prompt.\n\n\
                     {}\n\n\
                     Examples:\n\
                       /image\n\
                       /image --capability audio\n\
                       /image -c tools",
                crate::capability_help_text()
            ));
        }

        let provider_id = ctx.config.selected_provider_id();
        let registry = clawde_api::ModelRegistry::new();

        // Determine which capability to filter by (default: Vision)
        // using the shared helper to keep the match logic in one place.
        let target_cap = if let Some(cap_val) = trimmed
            .strip_prefix("--capability ")
            .or_else(|| trimmed.strip_prefix("-c "))
            .or_else(|| trimmed.strip_prefix("--capability="))
            .map(|s| s.trim())
        {
            match crate::parse_capability_str(&cap_val.to_lowercase()) {
                Some(cap) => cap,
                None => {
                    return CommandResult::Message(format!(
                        "Unknown capability '{}'. Valid: image, audio, pdf, video, tools, reasoning, json",
                        cap_val
                    ));
                }
            }
        } else {
            clawde_api::ModelCapability::Vision
        };

        let cap_label = crate::capability_label(target_cap);

        // Use the generic capability-based lookup.
        let model = provider_lookup_ids(provider_id)
            .into_iter()
            .flat_map(|lookup_id| {
                registry
                    .list_by_capability(target_cap)
                    .into_iter()
                    .filter(move |m| &*m.info.provider_id == lookup_id)
            })
            .next()
            .map(|m| m.info.id.to_string())
            .or_else(|| {
                // Fallback: search across ALL providers for any model with this capability.
                registry
                    .list_by_capability(target_cap)
                    .first()
                    .map(|m| m.info.id.to_string())
            });

        if let Some(model_id) = model {
            let model_name = stripped_model_for_provider(provider_id, &model_id).to_string();
            let mut new_config = ctx.config.clone();
            new_config.model = Some(canonical_model_for_provider(provider_id, &model_id));
            CommandResult::ConfigChangeMessage(
                new_config,
                format!("Switched to {} — capable of {}.\n", model_name, cap_label),
            )
        } else {
            CommandResult::Message(format!(
                "No {}-capable model found for the current provider.\n\
                     Try switching to a different provider with /connect.",
                cap_label
            ))
        }
    }
}

// ---- /fast (/speed) ------------------------------------------------------

#[async_trait]
impl SlashCommand for FastCommand {
    fn name(&self) -> &str {
        "fast"
    }
    fn aliases(&self) -> Vec<&str> {
        vec!["speed"]
    }
    fn description(&self) -> &str {
        "Toggle fast mode (uses a faster/cheaper model)"
    }
    fn help(&self) -> &str {
        "Usage: /fast [on|off]\n\n\
         Fast mode switches to the active provider's smaller, faster model\n\
         for quick responses. Toggle without argument to switch.\n\
         The setting is persisted to ~/.clawde/ui-settings.json."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let current = load_ui_settings();
        let currently_on = current.fast_mode.unwrap_or(false);

        let enable = match args.trim() {
            "on" | "enable" | "true" | "1" => true,
            "off" | "disable" | "false" | "0" => false,
            "" => !currently_on,
            other => {
                return CommandResult::Error(format!(
                    "Unknown argument '{}'. Use: /fast [on|off]",
                    other
                ))
            }
        };

        if let Err(e) = mutate_ui_settings(|s| s.fast_mode = Some(enable)) {
            return CommandResult::Error(format!("Failed to save setting: {}", e));
        }

        let provider_id = ctx.config.selected_provider_id();
        let fast_model = resolve_fast_model_id(&ctx.config);
        let normal_model =
            stripped_model_for_provider(provider_id, ctx.config.effective_model()).to_string();

        if enable {
            let mut new_config = ctx.config.clone();
            new_config.model = Some(canonical_model_for_provider(provider_id, &fast_model));
            CommandResult::ConfigChangeMessage(
                new_config,
                format!(
                    "Fast mode ON. Using {} for quicker, cheaper responses.\n\
                     Use /fast off to return to {}.",
                    fast_model, normal_model
                ),
            )
        } else {
            let mut new_config = ctx.config.clone();
            // Restore default / saved model
            new_config.model = None;
            let restored_model =
                stripped_model_for_provider(provider_id, new_config.effective_model()).to_string();
            CommandResult::ConfigChangeMessage(
                new_config,
                format!(
                    "Fast mode OFF. Restored to default model ({}).",
                    restored_model
                ),
            )
        }
    }
}

// ---- /color (full implementation) ----------------------------------------

#[async_trait]
impl SlashCommand for ColorSetCommand {
    fn name(&self) -> &str {
        "color-set"
    }
    fn hidden(&self) -> bool {
        true
    }
    fn description(&self) -> &str {
        "Internal: set prompt color — use /color instead"
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let color = args.trim();
        if color.is_empty() {
            let current = load_ui_settings();
            return CommandResult::Message(format!(
                "Current prompt color: {}\n\
                 Use /color <name|#RRGGBB|default> to change it.\n\n\
                 Named colors: red, green, blue, yellow, cyan, magenta, white, orange, purple",
                current.prompt_color.as_deref().unwrap_or("default"),
            ));
        }

        let normalized = if color == "default" {
            None
        } else {
            // Validate hex or named color
            let known_colors = [
                "red", "green", "blue", "yellow", "cyan", "magenta", "white", "orange", "purple",
                "pink", "gray", "grey",
            ];
            let is_hex = color.starts_with('#')
                && (color.len() == 4 || color.len() == 7)
                && color[1..].chars().all(|c| c.is_ascii_hexdigit());
            if !is_hex && !known_colors.contains(&color.to_lowercase().as_str()) {
                return CommandResult::Error(format!(
                    "Unknown color '{}'. Use a color name (red, green, …) or a hex code (#RGB or #RRGGBB).",
                    color
                ));
            }
            Some(color.to_string())
        };

        match mutate_ui_settings(|s| s.prompt_color = normalized.clone()) {
            Ok(_) => CommandResult::Message(format!(
                "Prompt color set to {}.\n\
                 Restart the REPL for the change to take effect.",
                normalized.as_deref().unwrap_or("default")
            )),
            Err(e) => CommandResult::Error(format!("Failed to save color: {}", e)),
        }
    }
}

// ---- /ollama --------------------------------------------------------------

#[async_trait]
impl SlashCommand for OllamaModeCommand {
    fn name(&self) -> &str {
        "ollama"
    }
    fn description(&self) -> &str {
        "Configure Ollama mode, remote host, and inference options"
    }
    fn arg_completions(&self, _partial: &str) -> Vec<ArgCompletion> {
        vec![
            ArgCompletion {
                value: "config".into(),
                description: "Open the Ollama configuration screen".into(),
                available: true,
            },
            ArgCompletion {
                value: "status".into(),
                description: "Show loaded models and reported VRAM usage".into(),
                available: true,
            },
            ArgCompletion {
                value: "online".into(),
                description: "Apply and persist Online mode (network tools allowed)".into(),
                available: true,
            },
            ArgCompletion {
                value: "isolated".into(),
                description: "Apply and persist Isolated mode (network tools blocked)".into(),
                available: true,
            },
        ]
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let arg = args.trim();
        if arg.eq_ignore_ascii_case("status") {
            return match clawde_core::ollama_status_for_config(&ctx.config).await {
                Ok(status) if status.models.is_empty() => CommandResult::Message(
                    "Ollama is reachable; no models are loaded in VRAM.".to_string(),
                ),
                Ok(status) => {
                    let total_vram: u64 = status
                        .models
                        .iter()
                        .filter_map(|model| model.size_vram)
                        .sum();
                    let models = status
                        .models
                        .iter()
                        .map(|model| {
                            let vram = model
                                .size_vram
                                .map(|bytes| format!("{} MB VRAM", bytes / 1_048_576))
                                .unwrap_or_else(|| "VRAM unknown".to_string());
                            format!("{} ({})", model.name, vram)
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    CommandResult::Message(format!(
                        "Ollama loaded: {}\nTotal reported VRAM: {} MB",
                        models,
                        total_vram / 1_048_576
                    ))
                }
                Err(error) => CommandResult::Error(error),
            };
        }

        if arg.eq_ignore_ascii_case("config") || arg.is_empty() {
            return CommandResult::Message(
                "Open the Ollama configuration screen with /ollama to set the remote host, select an installed model, and tune context, keep-alive, and output limits.".to_string(),
            );
        }

        match arg.to_ascii_lowercase().as_str() {
            "online" | "isolated" => CommandResult::Message(format!(
                "Ollama mode is applied from the TUI with /ollama {arg} (or Alt+O, then switch mode inside the screen)."
            )),
            _ => CommandResult::Message(
                "Usage: /ollama [config|status|online|isolated]. The TUI configuration screen is the centralized place for Ollama host, installed-model selection, and inference options.".to_string(),
            ),
        }
    }
}
