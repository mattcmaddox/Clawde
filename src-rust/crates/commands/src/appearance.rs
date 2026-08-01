// Appearance/UI commands: `/color`, `/theme`, `/output-style`, `/keybindings`, `/privacy-settings`.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct ColorCommand;
pub struct ThemeCommand;
pub struct OutputStyleCommand;
pub struct KeybindingsCommand;
pub struct PrivacySettingsCommand;

// ---- /color --------------------------------------------------------------

#[async_trait]
impl SlashCommand for ColorCommand {
    fn name(&self) -> &str {
        "color"
    }
    fn description(&self) -> &str {
        "Set or show the prompt bar color for this session"
    }
    fn arg_completions(&self, _partial: &str) -> Vec<ArgCompletion> {
        vec![
            ArgCompletion {
                value: "default".into(),
                description: "Reset to default color".into(),
                available: true,
            },
            ArgCompletion {
                value: "red".into(),
                description: "Red prompt bar".into(),
                available: true,
            },
            ArgCompletion {
                value: "green".into(),
                description: "Green prompt bar".into(),
                available: true,
            },
            ArgCompletion {
                value: "blue".into(),
                description: "Blue prompt bar".into(),
                available: true,
            },
            ArgCompletion {
                value: "yellow".into(),
                description: "Yellow prompt bar".into(),
                available: true,
            },
            ArgCompletion {
                value: "cyan".into(),
                description: "Cyan prompt bar".into(),
                available: true,
            },
            ArgCompletion {
                value: "magenta".into(),
                description: "Magenta prompt bar".into(),
                available: true,
            },
            ArgCompletion {
                value: "white".into(),
                description: "White prompt bar".into(),
                available: true,
            },
            ArgCompletion {
                value: "orange".into(),
                description: "Orange prompt bar".into(),
                available: true,
            },
            ArgCompletion {
                value: "purple".into(),
                description: "Purple prompt bar".into(),
                available: true,
            },
        ]
    }
    fn help(&self) -> &str {
        "Usage: /color [<name|#RRGGBB|default>]\n\n\
         Sets the accent color for the prompt bar in this session.\n\
         Named colors: red, green, blue, yellow, cyan, magenta, white, orange, purple\n\
         Hex codes:    #RGB or #RRGGBB\n\
         Reset:        /color default\n\n\
         The color is persisted to ~/.claurst/ui-settings.json and\n\
         applied on the next REPL startup."
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

// ---- /theme --------------------------------------------------------------

/// Names of the built-in themes (kept in sync with theme_colors.rs).
const BUILTIN_THEMES: &[&str] = &[
    "default",
    "dark",
    "light",
    "solarized",
    "nord",
    "dracula",
    "monokai",
    "catppuccin",
    "deuteranopia",
];

/// List custom theme names from ~/.clawde/themes/*.json.
fn custom_theme_names() -> Vec<String> {
    let dir = Settings::config_dir().join("themes");
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    if !stem.is_empty() && stem.chars().all(|c| c.is_alphanumeric() || c == '_') {
                        names.push(stem.to_string());
                    }
                }
            }
        }
    }
    names.sort();
    names
}

#[async_trait]
impl SlashCommand for ThemeCommand {
    fn name(&self) -> &str {
        "theme"
    }
    fn description(&self) -> &str {
        "Show, change, or create themes"
    }
    fn arg_completions(&self, partial: &str) -> Vec<super::ArgCompletion> {
        let p = partial.to_lowercase();
        let mut out: Vec<super::ArgCompletion> = Vec::new();
        for name in BUILTIN_THEMES {
            out.push(super::ArgCompletion {
                value: (*name).into(),
                description: "Built-in theme".into(),
                available: true,
            });
        }
        for name in custom_theme_names() {
            out.push(super::ArgCompletion {
                value: name.clone(),
                description: "Custom theme".into(),
                available: true,
            });
        }
        out.push(super::ArgCompletion {
            value: "list".into(),
            description: "List all themes".into(),
            available: true,
        });
        out.push(super::ArgCompletion {
            value: "create".into(),
            description: "Open the interactive theme creator (TUI)".into(),
            available: true,
        });
        out.push(super::ArgCompletion {
            value: "delete".into(),
            description: "Delete a custom theme".into(),
            available: true,
        });
        out.retain(|c| c.value.to_lowercase().starts_with(&p));
        out
    }
    fn help(&self) -> &str {
        "Usage: /theme [<name>|list|delete <name>]\n\n\
         In the TUI:\n\
           /theme             - quick-pick popup: browse built-in + custom\n\
                                themes (j/k or arrows navigate, n opens the\n\
                                creator, d deletes a custom theme with\n\
                                confirmation)\n\
           /theme create      - interactive theme creator with the ANSI\n\
                                256-color grid (create / edit / delete\n\
                                custom themes, scrollable list)\n\n\
         In non-interactive contexts:\n\
           /theme                - show the active theme\n\
           /theme <name>         - switch to a built-in or custom theme\n\
           /theme list           - list built-in and custom themes\n\
           /theme delete <name>  - delete a custom theme file\n\
         Custom themes live in ~/.clawde/themes/<name>.json."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let args = args.trim();

        if args.is_empty() {
            return CommandResult::Message(format!(
                "Current theme: {:?}\nUse /theme <name> to change it, or open the TUI theme creator from the interactive prompt.",
                ctx.config.theme
            ));
        }

        if args == "list" {
            let mut lines = String::from("Themes:\n");
            for name in BUILTIN_THEMES {
                lines.push_str(&format!("  {} (built-in)\n", name));
            }
            for name in custom_theme_names() {
                lines.push_str(&format!("  {} (custom)\n", name));
            }
            lines.push_str("\nUse /theme <name> to switch, or the TUI creator to make new ones.");
            return CommandResult::Message(lines);
        }

        if let Some(name) = args.strip_prefix("delete ") {
            let name = name.trim();
            if BUILTIN_THEMES.contains(&name) {
                return CommandResult::Error("Built-in themes can't be deleted.".to_string());
            }
            let file = Settings::config_dir()
                .join("themes")
                .join(format!("{}.json", name));
            if !file.exists() {
                return CommandResult::Error(format!("No custom theme named '{}'.", name));
            }
            match std::fs::remove_file(&file) {
                Ok(()) => CommandResult::Message(format!("Deleted custom theme '{}'.", name)),
                Err(e) => CommandResult::Error(format!("Failed to delete theme: {}", e)),
            }
        } else if args.starts_with("delete") {
            CommandResult::Error("Usage: /theme delete <name>".to_string())
        } else if args == "create" {
            CommandResult::Message(
                "The interactive theme creator opens from /theme in the TUI.\n\
                 Use /theme <name> to apply an existing theme here."
                    .to_string(),
            )
        } else {
            let Some(theme) = parse_theme(args) else {
                return CommandResult::Error(
                    "Unknown theme. Use /theme list to see available themes.".to_string(),
                );
            };

            let mut new_config = ctx.config.clone();
            new_config.theme = theme.clone();
            if let Err(err) =
                save_settings_mutation(|settings| settings.config.theme = theme.clone())
            {
                return CommandResult::Error(format!("Failed to save theme: {}", err));
            }

            CommandResult::ConfigChangeMessage(
                new_config,
                format!("Theme set to {}.", args.to_lowercase()),
            )
        }
    }
}

// ---- /output-style -------------------------------------------------------

#[async_trait]
impl SlashCommand for OutputStyleCommand {
    fn name(&self) -> &str {
        "output-style"
    }
    fn description(&self) -> &str {
        "Show or switch the current output style"
    }
    fn arg_completions(&self, _partial: &str) -> Vec<super::ArgCompletion> {
        available_output_style_names()
            .into_iter()
            .map(|name| super::ArgCompletion {
                value: name.clone(),
                description: name.clone(),
                available: true,
            })
            .collect()
    }
    fn help(&self) -> &str {
        "Usage: /output-style [style-name]\n\n\
         With no argument: list available styles and show the current one.\n\
         With a style name: switch to that style (persisted to settings).\n\n\
         Built-in styles: default, concise, explanatory, learning, caveman, rocky\n\
         Personas (caveman/rocky) are also reachable via /caveman, /rocky, and\n\
         by typing the single word inline in a prompt (transient for one turn).\n\
         Plugin-defined and user styles are listed automatically.\n\n\
         Changes take effect on the next request."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let arg = args.trim();
        let valid_styles = available_output_style_names();
        let current = current_output_style_name(&ctx.config);

        if arg.is_empty() {
            // List available styles
            let mut lines = format!("Current output style: {}\n\nAvailable styles:\n", current);
            for style in &valid_styles {
                let marker = if style == current { " *" } else { "" };
                lines.push_str(&format!("  {}{}\n", style, marker));
            }
            lines.push_str("\nUse /output-style <name> to switch.");
            return CommandResult::Message(lines);
        }

        let normalized = arg.to_lowercase();
        if !valid_styles.iter().any(|name| name == &normalized) {
            return CommandResult::Error(format!(
                "Unknown output style '{}'. Available styles: {}",
                arg,
                valid_styles.join(", ")
            ));
        }

        let mut new_config = ctx.config.clone();
        new_config.output_style = (normalized != "default").then(|| normalized.clone());
        if let Err(err) = save_settings_mutation(|settings| {
            settings.config.output_style = (normalized != "default").then(|| normalized.clone());
        }) {
            return CommandResult::Error(format!("Failed to save configuration: {}", err));
        }

        CommandResult::ConfigChangeMessage(
            new_config,
            format!(
                "Output style set to '{}'. Changes take effect on the next request.",
                normalized
            ),
        )
    }
}

// ---- /keybindings --------------------------------------------------------

#[async_trait]
impl SlashCommand for KeybindingsCommand {
    fn name(&self) -> &str {
        "keybindings"
    }
    fn description(&self) -> &str {
        "Create or open ~/.claurst/keybindings.json"
    }

    async fn execute(&self, _args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let config_dir = Settings::config_dir();
        let path = config_dir.join("keybindings.json");
        let existed = path.exists();

        if !existed {
            if let Err(err) = std::fs::create_dir_all(&config_dir) {
                return CommandResult::Error(format!(
                    "Failed to create {}: {}",
                    config_dir.display(),
                    err
                ));
            }

            let template = match generate_keybindings_template() {
                Ok(template) => template,
                Err(err) => {
                    return CommandResult::Error(format!(
                        "Failed to generate keybindings template: {}",
                        err
                    ))
                }
            };

            if let Err(err) = std::fs::write(&path, template) {
                return CommandResult::Error(format!(
                    "Failed to write {}: {}",
                    path.display(),
                    err
                ));
            }
        }

        match open_with_system(&path.display().to_string()) {
            Ok(_) => CommandResult::Message(if existed {
                format!("Opened {} in your editor.", path.display())
            } else {
                format!(
                    "Created {} with a template and opened it in your editor.",
                    path.display()
                )
            }),
            Err(err) => CommandResult::Message(if existed {
                format!(
                    "Opened {}. Could not launch an editor automatically: {}",
                    path.display(),
                    err
                )
            } else {
                format!(
                    "Created {} with a template. Could not launch an editor automatically: {}",
                    path.display(),
                    err
                )
            }),
        }
    }
}

// ---- /privacy-settings ---------------------------------------------------

#[async_trait]
impl SlashCommand for PrivacySettingsCommand {
    fn name(&self) -> &str {
        "privacy-settings"
    }
    fn description(&self) -> &str {
        "Open Claurst privacy settings"
    }

    async fn execute(&self, _args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let url = "https://claude.ai/settings/data-privacy-controls";
        let fallback = format!("Review and manage your privacy settings at {}", url);
        match open_with_system(url) {
            Ok(_) => CommandResult::Message(format!("Opened privacy settings: {}", url)),
            Err(_) => CommandResult::Message(fallback),
        }
    }
}
