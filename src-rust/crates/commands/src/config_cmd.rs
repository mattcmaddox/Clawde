// `/config` command.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct ConfigCommand;

// ---- /config -------------------------------------------------------------

#[async_trait]
impl SlashCommand for ConfigCommand {
    fn name(&self) -> &str {
        "config"
    }
    fn aliases(&self) -> Vec<&str> {
        vec!["settings"]
    }
    fn description(&self) -> &str {
        "Show or modify configuration settings"
    }
    fn arg_completions(&self, partial: &str) -> Vec<ArgCompletion> {
        let mut completions = vec![
            ArgCompletion {
                value: "show".into(),
                description: "Show current configuration".into(),
                available: true,
            },
            ArgCompletion {
                value: "get".into(),
                description: "Get a config key value".into(),
                available: true,
            },
            ArgCompletion {
                value: "set".into(),
                description: "Set a config key".into(),
                available: true,
            },
            ArgCompletion {
                value: "unset".into(),
                description: "Unset a config key (revert to default)".into(),
                available: true,
            },
        ];
        // Second-level: /config set <key>, /config get <key>, /config unset <key>
        if partial == "set" || partial.starts_with("set ") {
            completions.push(ArgCompletion {
                value: "set theme".into(),
                description: "Set the UI theme (default, dark, light)".into(),
                available: true,
            });
            completions.push(ArgCompletion {
                value: "set output-style".into(),
                description: "Set the output style".into(),
                available: true,
            });
            completions.push(ArgCompletion {
                value: "set model".into(),
                description: "Set the active model".into(),
                available: true,
            });
            completions.push(ArgCompletion {
                value: "set permission-mode".into(),
                description:
                    "Set permission mode (default, accept-edits, bypass-permissions, plan)".into(),
                available: true,
            });
            completions.push(ArgCompletion {
                value: "set default-effort".into(),
                description: "Set the default reasoning effort".into(),
                available: true,
            });
            completions.push(ArgCompletion {
                value: "set mode".into(),
                description: "Set the active mode preset".into(),
                available: true,
            });
        }
        if partial == "get" || partial.starts_with("get ") {
            completions.push(ArgCompletion {
                value: "get theme".into(),
                description: "Show current theme".into(),
                available: true,
            });
            completions.push(ArgCompletion {
                value: "get output-style".into(),
                description: "Show current output style".into(),
                available: true,
            });
            completions.push(ArgCompletion {
                value: "get model".into(),
                description: "Show current model".into(),
                available: true,
            });
            completions.push(ArgCompletion {
                value: "get permission-mode".into(),
                description: "Show current permission mode".into(),
                available: true,
            });
            completions.push(ArgCompletion {
                value: "get default-effort".into(),
                description: "Show the default reasoning effort".into(),
                available: true,
            });
            completions.push(ArgCompletion {
                value: "get mode".into(),
                description: "Show the active mode preset".into(),
                available: true,
            });
        }
        if partial == "unset" || partial.starts_with("unset ") {
            completions.push(ArgCompletion {
                value: "unset model".into(),
                description: "Reset model to default".into(),
                available: true,
            });
            completions.push(ArgCompletion {
                value: "unset output-style".into(),
                description: "Reset output style to default".into(),
                available: true,
            });
            completions.push(ArgCompletion {
                value: "unset default-effort".into(),
                description: "Use the provider/model default effort".into(),
                available: true,
            });
            completions.push(ArgCompletion {
                value: "unset mode".into(),
                description: "Reset to default behavior (no mode preset)".into(),
                available: true,
            });
        }

        let mut add_values = |prefix: &str, values: &[&str]| {
            for value in values {
                completions.push(ArgCompletion {
                    value: format!("{prefix} {value}"),
                    description: String::new(),
                    available: true,
                });
            }
        };
        if partial == "set theme" || partial.starts_with("set theme ") {
            add_values("set theme", &["default", "dark", "light"]);
        }
        if partial == "set permission-mode" || partial.starts_with("set permission-mode ") {
            add_values(
                "set permission-mode",
                &["default", "accept-edits", "bypass-permissions", "plan"],
            );
        }
        if partial == "set default-effort" || partial.starts_with("set default-effort ") {
            add_values(
                "set default-effort",
                &[
                    "none",
                    "minimal",
                    "low",
                    "medium",
                    "high",
                    "xhigh",
                    "max",
                    "ultracode",
                ],
            );
        }
        if partial == "set output-style" || partial.starts_with("set output-style ") {
            let styles = available_output_style_names();
            for style in styles {
                completions.push(ArgCompletion {
                    value: format!("set output-style {style}"),
                    description: String::new(),
                    available: true,
                });
            }
        }
        if partial == "set mode" || partial.starts_with("set mode ") {
            for mode in available_mode_names() {
                completions.push(ArgCompletion {
                    value: format!("set mode {mode}"),
                    description: String::new(),
                    available: true,
                });
            }
        }
        // `set model` takes a free-form model ID that cannot be completed.
        // Show a dimmed placeholder hint while the value is still empty so the
        // popup says what goes next instead of repeating the key.
        if partial.starts_with("set model ") {
            let typed_value = partial.strip_prefix("set model").unwrap_or("").trim();
            if let Some(hint) = super::free_form_arg_hint(
                "set model",
                "<model>",
                "Type a model ID, e.g. claude-sonnet-4-6 or openai/gpt-4o",
                !typed_value.is_empty(),
            ) {
                completions.push(hint);
            }
        }
        completions
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let args = args.trim();
        if args.is_empty() || matches!(args, "show" | "get") {
            let json = serde_json::to_string_pretty(&ctx.config).unwrap_or_default();
            return CommandResult::Message(format!(
                "Current configuration:\n{}\n\nUsage:\n  /config\n  /config set theme <default|dark|light>\n  /config set output-style <default|concise|explanatory|learning|formal|casual>\n  /config set model <model>\n  /config set permission-mode <default|accept-edits|bypass-permissions|plan>\n  /config set default-effort <none|minimal|low|medium|high|xhigh|max|ultracode>\n  /config set mode <default|careful|fast|…>\n  /config unset <model|output-style|default-effort|mode>",
                json
            ));
        }

        if let Some(key) = args.strip_prefix("get ").map(str::trim) {
            return match key {
                "theme" => CommandResult::Message(format!("theme = {:?}", ctx.config.theme)),
                "output-style" | "output_style" => CommandResult::Message(format!(
                    "output-style = {}",
                    current_output_style_name(&ctx.config)
                )),
                "model" => {
                    CommandResult::Message(format!("model = {}", ctx.config.effective_model()))
                }
                "permission-mode" | "permission_mode" => CommandResult::Message(format!(
                    "permission-mode = {:?}",
                    ctx.config.permission_mode
                )),
                "default-effort" | "default_effort" => CommandResult::Message(format!(
                    "default-effort = {}",
                    ctx.config
                        .default_effort
                        .map(|level| level.as_str())
                        .unwrap_or("provider-default")
                )),
                "mode" => {
                    let current = ctx.config.mode.as_deref().unwrap_or("default");
                    let description = clawde_core::modes::all_modes_for_project(
                        &Settings::config_dir(),
                        &clawde_core::git_utils::project_root(&ctx.working_dir),
                    )
                    .iter()
                    .find(|m| m.name == current)
                    .map(|m| m.description.clone())
                    .unwrap_or_default();
                    CommandResult::Message(format!(
                        "mode = {}{}",
                        current,
                        if description.is_empty() {
                            String::new()
                        } else {
                            format!(" — {description}")
                        }
                    ))
                }
                other => CommandResult::Error(format!("Unknown config key '{}'", other)),
            };
        }

        if let Some(key) = args.strip_prefix("unset ").map(str::trim) {
            return match key {
                "model" => {
                    let mut new_config = ctx.config.clone();
                    new_config.model = None;
                    if let Err(err) =
                        save_settings_mutation(|settings| settings.config.model = None)
                    {
                        return CommandResult::Error(format!(
                            "Failed to save configuration: {}",
                            err
                        ));
                    }
                    CommandResult::ConfigChangeMessage(
                        new_config,
                        "Model reset to the default for new sessions.".to_string(),
                    )
                }
                "output-style" | "output_style" => {
                    let mut new_config = ctx.config.clone();
                    new_config.output_style = None;
                    if let Err(err) =
                        save_settings_mutation(|settings| settings.config.output_style = None)
                    {
                        return CommandResult::Error(format!(
                            "Failed to save configuration: {}",
                            err
                        ));
                    }
                    CommandResult::ConfigChangeMessage(
                        new_config,
                        "Output style reset to default.".to_string(),
                    )
                }
                "default-effort" | "default_effort" => {
                    let mut new_config = ctx.config.clone();
                    new_config.default_effort = None;
                    if let Err(err) = save_settings_mutation(|settings| {
                        settings.config.default_effort = None;
                    }) {
                        return CommandResult::Error(format!(
                            "Failed to save configuration: {}",
                            err
                        ));
                    }
                    CommandResult::ConfigChangeMessage(
                        new_config,
                        "Default effort reset to the provider/model default.".to_string(),
                    )
                }
                "mode" => {
                    let mut new_config = ctx.config.clone();
                    new_config.mode = None;
                    if let Err(err) = save_settings_mutation(|settings| settings.config.mode = None)
                    {
                        return CommandResult::Error(format!(
                            "Failed to save configuration: {}",
                            err
                        ));
                    }
                    CommandResult::ConfigChangeMessage(
                        new_config,
                        "Mode reset to default behavior. Takes effect on the next request."
                            .to_string(),
                    )
                }
                other => CommandResult::Error(format!("Unknown config key '{}'", other)),
            };
        }

        let mut parts = args.splitn(3, ' ');
        let command = parts.next().unwrap_or_default();
        let key = parts.next().unwrap_or_default().trim();
        let value = parts.next().unwrap_or_default().trim();
        if command != "set" || key.is_empty() || value.is_empty() {
            return CommandResult::Error("Usage: /config set <key> <value>".to_string());
        }

        match key {
            "theme" => {
                let Some(theme) = parse_theme(value) else {
                    return CommandResult::Error(
                        "Theme must be one of: default, dark, light".to_string(),
                    );
                };
                let mut new_config = ctx.config.clone();
                new_config.theme = theme.clone();
                if let Err(err) =
                    save_settings_mutation(|settings| settings.config.theme = theme.clone())
                {
                    return CommandResult::Error(format!("Failed to save configuration: {}", err));
                }
                CommandResult::ConfigChangeMessage(
                    new_config,
                    format!("Theme set to {}.", value.trim().to_lowercase()),
                )
            }
            "output-style" | "output_style" => {
                let normalized = value.trim().to_lowercase();
                let valid = available_output_style_names();
                if !valid.iter().any(|name| name == &normalized) {
                    return CommandResult::Error(format!(
                        "Unsupported output style '{}'. Use one of: {}",
                        value,
                        valid.join(", ")
                    ));
                }

                let mut new_config = ctx.config.clone();
                new_config.output_style = (normalized != "default").then(|| normalized.clone());
                if let Err(err) = save_settings_mutation(|settings| {
                    settings.config.output_style =
                        (normalized != "default").then(|| normalized.clone());
                }) {
                    return CommandResult::Error(format!("Failed to save configuration: {}", err));
                }
                CommandResult::ConfigChangeMessage(
                    new_config,
                    format!(
                        "Output style set to {}. Changes take effect on the next request.",
                        normalized
                    ),
                )
            }
            "model" => {
                let mut new_config = ctx.config.clone();
                new_config.model = Some(value.to_string());
                let inferred_provider = value
                    .split_once('/')
                    .map(|(provider, _)| provider.to_string());
                if let Some(ref provider) = inferred_provider {
                    new_config.provider = Some(provider.clone());
                }
                if let Err(err) = save_settings_mutation(|settings| {
                    settings.config.model = Some(value.to_string());
                    if let Some(ref provider) = inferred_provider {
                        settings.provider = Some(provider.clone());
                        settings.config.provider = Some(provider.clone());
                    }
                }) {
                    return CommandResult::Error(format!("Failed to save configuration: {}", err));
                }
                CommandResult::ConfigChangeMessage(new_config, format!("Model set to {}.", value))
            }
            "permission-mode" | "permission_mode" => {
                let mode = match value.trim().to_lowercase().as_str() {
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
                            "Permission mode must be one of: default, accept-edits, bypass-permissions, plan"
                                .to_string(),
                        )
                    }
                };

                let mut new_config = ctx.config.clone();
                new_config.permission_mode = mode.clone();
                if let Err(err) = save_settings_mutation(|settings| {
                    settings.config.permission_mode = mode.clone();
                }) {
                    return CommandResult::Error(format!("Failed to save configuration: {}", err));
                }
                CommandResult::ConfigChangeMessage(
                    new_config,
                    format!("Permission mode set to {}.", value.trim().to_lowercase()),
                )
            }
            "default-effort" | "default_effort" => {
                let Some(level) = clawde_core::effort::EffortLevel::from_str(value) else {
                    return CommandResult::Error(format!(
                        "Unknown effort level '{}'. Use: none | minimal | low | medium | high | xhigh | max | ultracode",
                        value
                    ));
                };
                let mut new_config = ctx.config.clone();
                new_config.default_effort = Some(level);
                if let Err(err) = save_settings_mutation(|settings| {
                    settings.config.default_effort = Some(level);
                }) {
                    return CommandResult::Error(format!("Failed to save configuration: {}", err));
                }
                CommandResult::ConfigChangeMessage(
                    new_config,
                    format!(
                        "Default effort set to {} for subsequent requests without a session override.",
                        level.label()
                    ),
                )
            }
            "mode" => {
                let normalized = value.trim().to_lowercase();
                let modes = clawde_core::modes::all_modes_for_project(
                    &Settings::config_dir(),
                    &clawde_core::git_utils::project_root(&ctx.working_dir),
                )
                .into_iter()
                .map(|mode| mode.name)
                .collect::<Vec<_>>();
                if normalized == "default" {
                    // `default` is the reset, mirroring `/config set output-style default`.
                    let mut new_config = ctx.config.clone();
                    new_config.mode = None;
                    if let Err(err) = save_settings_mutation(|settings| settings.config.mode = None)
                    {
                        return CommandResult::Error(format!(
                            "Failed to save configuration: {}",
                            err
                        ));
                    }
                    return CommandResult::ConfigChangeMessage(
                        new_config,
                        "Mode reset to default behavior. Takes effect on the next request."
                            .to_string(),
                    );
                }
                if !modes.iter().any(|name| name == &normalized) {
                    return CommandResult::Error(format!(
                        "Unknown mode '{}'. Available modes: {}",
                        value,
                        modes.join(", ")
                    ));
                }
                let mut new_config = ctx.config.clone();
                new_config.mode = Some(normalized.clone());
                if let Err(err) = save_settings_mutation(|settings| {
                    settings.config.mode = Some(normalized.clone());
                }) {
                    return CommandResult::Error(format!("Failed to save configuration: {}", err));
                }
                CommandResult::ConfigChangeMessage(
                    new_config,
                    format!(
                        "Mode set to {}. Knobs apply on the next request; guidance applies per turn.",
                        normalized
                    ),
                )
            }
            other => CommandResult::Error(format!("Unknown config key '{}'", other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawde_core::cost::CostTracker;

    /// Redirect `CLAWDE_HOME` to a fresh temp dir for the lifetime of the
    /// guard, serialised on the shared crate lock (the /config mutations
    /// write settings.json under the config dir).
    struct TestHome {
        _lock: std::sync::MutexGuard<'static, ()>,
        _tmp: tempfile::TempDir,
        prev: Option<std::ffi::OsString>,
    }

    impl TestHome {
        fn new() -> Self {
            let lock = crate::tests::CLAWDE_HOME_LOCK
                .get_or_init(|| std::sync::Mutex::new(()))
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let prev = std::env::var_os("CLAWDE_HOME");
            let tmp = tempfile::tempdir().unwrap();
            std::env::set_var("CLAWDE_HOME", tmp.path());
            TestHome {
                _lock: lock,
                _tmp: tmp,
                prev,
            }
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("CLAWDE_HOME", v),
                None => std::env::remove_var("CLAWDE_HOME"),
            }
        }
    }

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
            tool_use_tracker: None,
            autonomy: None,
        }
    }

    #[tokio::test]
    async fn config_set_mode_validates_and_persists() {
        let _home = TestHome::new();
        let mut ctx = make_ctx();
        match ConfigCommand.execute("set mode careful", &mut ctx).await {
            CommandResult::ConfigChangeMessage(cfg, msg) => {
                assert_eq!(cfg.mode.as_deref(), Some("careful"));
                assert!(msg.contains("Mode set to careful"), "{}", msg);
            }
            other => panic!("expected ConfigChangeMessage, got {:?}", other),
        }
        // Persisted to settings.json under the (redirected) config dir.
        let settings = Settings::load_sync().expect("settings load");
        assert_eq!(settings.config.mode.as_deref(), Some("careful"));
    }

    #[tokio::test]
    async fn config_set_mode_rejects_unknown_name() {
        let _home = TestHome::new();
        let mut ctx = make_ctx();
        match ConfigCommand.execute("set mode bogus", &mut ctx).await {
            CommandResult::Error(e) => {
                assert!(e.contains("Unknown mode 'bogus'"), "{}", e);
                assert!(
                    e.contains("careful"),
                    "error should list available modes: {}",
                    e
                );
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn config_set_mode_default_resets() {
        let _home = TestHome::new();
        let mut ctx = make_ctx();
        let _ = ConfigCommand.execute("set mode fast", &mut ctx).await;
        match ConfigCommand.execute("set mode default", &mut ctx).await {
            CommandResult::ConfigChangeMessage(cfg, _) => {
                assert_eq!(cfg.mode, None, "default resets the mode");
            }
            other => panic!("expected ConfigChangeMessage, got {:?}", other),
        }
        let settings = Settings::load_sync().expect("settings load");
        assert_eq!(settings.config.mode, None);
    }

    #[tokio::test]
    async fn config_get_mode_shows_current() {
        let _home = TestHome::new();
        let mut ctx = make_ctx();
        let _ = ConfigCommand.execute("set mode careful", &mut ctx).await;
        ctx.config.mode = Some("careful".to_string());
        match ConfigCommand.execute("get mode", &mut ctx).await {
            CommandResult::Message(m) => assert!(m.contains("mode = careful"), "{}", m),
            other => panic!("expected Message, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn config_unset_mode_resets() {
        let _home = TestHome::new();
        let mut ctx = make_ctx();
        let _ = ConfigCommand.execute("set mode careful", &mut ctx).await;
        match ConfigCommand.execute("unset mode", &mut ctx).await {
            CommandResult::ConfigChangeMessage(cfg, _) => assert_eq!(cfg.mode, None),
            other => panic!("expected ConfigChangeMessage, got {:?}", other),
        }
        let settings = Settings::load_sync().expect("settings load");
        assert_eq!(settings.config.mode, None);
    }
}
