// Account/auth commands: `/login`, `/logout`, `/accounts`, `/switch`, `/refresh`.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct LoginCommand;
pub struct LogoutCommand;
pub struct RefreshCommand;

// ---- /login --------------------------------------------------------------

#[async_trait]
impl SlashCommand for LoginCommand {
    fn name(&self) -> &str {
        "login"
    }
    fn description(&self) -> &str {
        "Authenticate with Anthropic or Codex (multi-account)"
    }
    fn arg_completions(&self, partial: &str) -> Vec<ArgCompletion> {
        let trimmed = partial.trim();

        // When the user has typed --label, suggest existing profile IDs as
        // common naming hints.  Use a prefixed pattern so the caller's prefix
        // filter (which matches the full partial text) works correctly.
        if trimmed == "--label" || trimmed.starts_with("--label ") {
            let registry = clawde_core::accounts::AccountRegistry::load();
            let after = trimmed
                .strip_prefix("--label")
                .unwrap_or("")
                .trim();
            let mut results = Vec::new();
            for provider in [
                clawde_core::accounts::PROVIDER_ANTHROPIC,
                clawde_core::accounts::PROVIDER_CODEX,
            ] {
                for profile in registry.list(provider) {
                    if after.is_empty()
                        || profile
                            .id
                            .to_lowercase()
                            .starts_with(&after.to_lowercase())
                    {
                        results.push(ArgCompletion {
                            value: format!("--label {}", profile.id),
                            description: format!(
                                "{} profile: {}",
                                provider,
                                profile.display_name()
                            ),
                            available: true,
                        });
                    }
                }
            }
            return results;
        }

        // Otherwise offer the flags.
        vec![
            ArgCompletion { value: "--console".into(), description: "Login with API key (Console)".into(), available: true },
            ArgCompletion { value: "--codex".into(), description: "Login with ChatGPT/Codex account".into(), available: true },
            ArgCompletion { value: "--label".into(), description: "Set a profile label for the saved account".into(), available: true },
        ]
    }
    fn help(&self) -> &str {
        "Usage: /login [--console] [--codex] [--label <name>]\n\n\
         Start an OAuth login. By default authenticates with Claude.ai. Pass\n\
         `--console` for an API-key (Console) login, or `--codex` to add a\n\
         ChatGPT/Codex account. `--label work` names the saved profile so you\n\
         can `switch` to it later by that name."
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let tokens: Vec<&str> = args.split_whitespace().collect();
        let use_codex = tokens.contains(&"--codex");
        let login_with_claude_ai = !tokens.contains(&"--console");
        let label = parse_label_arg(&tokens);

        let provider = if use_codex {
            clawde_core::accounts::PROVIDER_CODEX
        } else {
            clawde_core::accounts::PROVIDER_ANTHROPIC
        };

        CommandResult::StartLoginForProvider {
            provider: provider.to_string(),
            login_with_claude_ai,
            label,
        }
    }
}

fn parse_label_arg(tokens: &[&str]) -> Option<String> {
    let mut it = tokens.iter();
    while let Some(t) = it.next() {
        if *t == "--label" || *t == "-l" {
            return it.next().map(|s| s.to_string());
        }
        if let Some(rest) = t.strip_prefix("--label=") {
            return Some(rest.to_string());
        }
    }
    None
}

// ---- /logout -------------------------------------------------------------

#[async_trait]
impl SlashCommand for LogoutCommand {
    fn name(&self) -> &str {
        "logout"
    }
    fn description(&self) -> &str {
        "Clear credentials for the active account"
    }
    fn arg_completions(&self, partial: &str) -> Vec<ArgCompletion> {
        let registry = clawde_core::accounts::AccountRegistry::load();
        let trimmed = partial.trim();

        // Base flags always shown.
        let mut results: Vec<ArgCompletion> = vec![
            ArgCompletion { value: "--codex".into(), description: "Log out the Codex account".into(), available: true },
            ArgCompletion { value: "--all".into(), description: "Log out all accounts".into(), available: true },
        ];

        // Show active Anthropic profile name as informational hint when no
        // flag or --all is specified (default targets the active Anthropic
        // account). Use `available: false` so it renders dimmed — visible
        // but not selectable (the command does not accept a profile-id arg).
        if !trimmed.contains("--codex") {
            if let Some(active) = registry.active(clawde_core::accounts::PROVIDER_ANTHROPIC) {
                results.push(ArgCompletion {
                    value: active.to_string(),
                    description: "Active Anthropic profile (logged out by default)".into(),
                    available: false,
                });
            }
        }

        // Show active Codex profile name when --codex is involved.
        // Dimmed (available: false) because /logout doesn't accept a profile-id
        // argument — it always targets the active account.  The hint is purely
        // informational so the user knows which profile will be affected.
        if trimmed.contains("--codex") {
            if let Some(active) = registry.active(clawde_core::accounts::PROVIDER_CODEX) {
                results.push(ArgCompletion {
                    value: active.to_string(),
                    description: "Active Codex profile (will be logged out)".into(),
                    available: false,
                });
            }
        }

        // When --all is specified, list all profiles that would be purged
        // as informational items.
        if trimmed.contains("--all") {
            for provider in [
                clawde_core::accounts::PROVIDER_ANTHROPIC,
                clawde_core::accounts::PROVIDER_CODEX,
            ] {
                for profile in registry.list(provider) {
                    results.push(ArgCompletion {
                        value: profile.id.clone(),
                        description: format!("Would purge {} profile", provider),
                        available: false,
                    });
                }
            }
        }

        results
    }
    fn help(&self) -> &str {
        "Usage: /logout [--codex] [--all]\n\n\
         By default removes the active Anthropic account. `--codex` targets\n\
         Codex instead. `--all` purges every stored credential for the chosen\n\
         provider and clears any API key in settings."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let tokens: Vec<&str> = args.split_whitespace().collect();
        let use_codex = tokens.contains(&"--codex");
        let purge_all = tokens.contains(&"--all");

        if use_codex {
            if purge_all {
                let mut registry = clawde_core::accounts::AccountRegistry::load();
                let ids: Vec<String> = registry
                    .list(clawde_core::accounts::PROVIDER_CODEX)
                    .into_iter()
                    .map(|p| p.id)
                    .collect();
                for id in &ids {
                    let _ = registry.remove(clawde_core::accounts::PROVIDER_CODEX, id);
                }
                return CommandResult::Message(format!(
                    "Removed {} stored Codex account(s).",
                    ids.len()
                ));
            }
            if let Err(e) = clawde_core::oauth_config::clear_codex_tokens() {
                return CommandResult::Error(format!("Failed to clear Codex tokens: {}", e));
            }
            return CommandResult::Message("Logged out of the active Codex account.".to_string());
        }

        // Anthropic logout.
        if purge_all {
            let mut registry = clawde_core::accounts::AccountRegistry::load();
            let ids: Vec<String> = registry
                .list(clawde_core::accounts::PROVIDER_ANTHROPIC)
                .into_iter()
                .map(|p| p.id)
                .collect();
            for id in &ids {
                let _ = registry.remove(clawde_core::accounts::PROVIDER_ANTHROPIC, id);
            }
            let mut settings = clawde_core::config::Settings::load()
                .await
                .unwrap_or_default();
            settings.config.api_key = None;
            let _ = settings.save().await;
            ctx.config.api_key = None;
            return CommandResult::Message(format!(
                "Removed {} stored Anthropic account(s) and cleared API key.",
                ids.len()
            ));
        }

        if let Err(e) = clawde_core::oauth::OAuthTokens::clear().await {
            return CommandResult::Error(format!("Failed to clear OAuth tokens: {}", e));
        }
        let mut settings = clawde_core::config::Settings::load()
            .await
            .unwrap_or_default();
        settings.config.api_key = None;
        if let Err(e) = settings.save().await {
            return CommandResult::Error(format!("Failed to update settings: {}", e));
        }
        ctx.config.api_key = None;
        CommandResult::Message("Logged out of the active Anthropic account.".to_string())
    }
}

// ---- /accounts ------------------------------------------------------------

pub struct AccountsCommand;

#[async_trait]
impl SlashCommand for AccountsCommand {
    fn name(&self) -> &str {
        "accounts"
    }
    fn description(&self) -> &str {
        "List stored Anthropic and Codex accounts"
    }
    fn help(&self) -> &str {
        "Usage: /accounts\n\n\
         Lists every stored Anthropic and Codex account along with the\n\
         currently active one (marked with `*`). Use /switch to change\n\
         accounts, /login to add a new one, /logout to remove one."
    }

    async fn execute(&self, _args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let registry = clawde_core::accounts::AccountRegistry::load();
        let mut out = String::new();
        for (provider, label) in [
            (clawde_core::accounts::PROVIDER_ANTHROPIC, "Anthropic"),
            (clawde_core::accounts::PROVIDER_CODEX, "Codex"),
        ] {
            let profiles = registry.list(provider);
            let active = registry.active(provider);
            if profiles.is_empty() {
                out.push_str(&format!("{}: (no accounts stored)\n", label));
                continue;
            }
            out.push_str(&format!("{}:\n", label));
            for p in profiles {
                let marker = if active == Some(&p.id) { "*" } else { " " };
                let email = p.email.as_deref().unwrap_or("");
                let tier = p
                    .subscription_tier
                    .as_deref()
                    .map(|t| format!(" [{}]", t))
                    .unwrap_or_default();
                out.push_str(&format!("  {} {}{}  {}\n", marker, p.id, tier, email));
            }
        }
        if out.is_empty() {
            out.push_str("No accounts stored. Use /login to add one.");
        }
        CommandResult::Message(out.trim_end().to_string())
    }
}

// ---- /switch --------------------------------------------------------------

pub struct SwitchCommand;

#[async_trait]
impl SlashCommand for SwitchCommand {
    fn name(&self) -> &str {
        "switch"
    }
    fn description(&self) -> &str {
        "Switch the active account for a provider"
    }
    fn arg_completions(&self, partial: &str) -> Vec<ArgCompletion> {
        let registry = clawde_core::accounts::AccountRegistry::load();
        let trimmed = partial.trim();

        // If the user has already typed --codex, show Codex profiles.
        // Return them with a `--codex ` prefix so the caller's prefix filter
        // (which matches the full partial text) works correctly.
        if trimmed == "--codex" || trimmed.starts_with("--codex ") {
            let after = trimmed.trim_start_matches("--codex").trim();
            return registry
                .list(clawde_core::accounts::PROVIDER_CODEX)
                .into_iter()
                .map(|p| ArgCompletion {
                    value: format!("--codex {}", p.id),
                    description: p.display_name(),
                    available: true,
                })
                .filter(|c| {
                    after.is_empty()
                        || c.value.to_lowercase().starts_with(&format!("--codex {}", after))
                })
                .collect();
        }

        // Otherwise offer --codex flag + Anthropic profile ids.
        let mut results = vec![ArgCompletion {
            value: "--codex".into(),
            description: "Switch the Codex account instead of Anthropic".into(),
            available: true,
        }];
        for profile in registry.list(clawde_core::accounts::PROVIDER_ANTHROPIC) {
            results.push(ArgCompletion {
                value: profile.id.clone(),
                description: profile.display_name(),
                available: true,
            });
        }
        results
    }
    fn help(&self) -> &str {
        "Usage: /switch [--codex] <profile-id>\n\n\
         Make a stored account active. Defaults to Anthropic; pass `--codex`\n\
         to switch the Codex account instead. Run /accounts first to see\n\
         available profile ids."
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let tokens: Vec<&str> = args.split_whitespace().collect();
        let use_codex = tokens.contains(&"--codex");
        let provider = if use_codex {
            clawde_core::accounts::PROVIDER_CODEX
        } else {
            clawde_core::accounts::PROVIDER_ANTHROPIC
        };
        let display = if use_codex { "Codex" } else { "Anthropic" };
        let id = tokens.iter().find(|t| !t.starts_with("--"));

        let Some(id) = id else {
            return CommandResult::Error(format!(
                "Usage: /switch {}<profile-id> (try /accounts to see options)",
                if use_codex { "--codex " } else { "" }
            ));
        };

        let mut registry = clawde_core::accounts::AccountRegistry::load();
        match registry.switch_to(provider, id) {
            Ok(()) => {
                CommandResult::Message(format!("Switched {} active account to '{}'.", display, id))
            }
            Err(e) => CommandResult::Error(format!("{}", e)),
        }
    }
}

// ---- /refresh ------------------------------------------------------------

#[async_trait]
impl SlashCommand for RefreshCommand {
    fn name(&self) -> &str {
        "refresh"
    }
    fn description(&self) -> &str {
        "Clear saved provider auth and model caches"
    }
    fn help(&self) -> &str {
        "Usage: /refresh\n\n\
         Clears saved provider credentials, provider/model selection, and model caches, then rebuilds the live runtime state.\n\
         After refreshing, run /connect to authenticate and choose a provider again."
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        if !args.trim().is_empty() {
            return CommandResult::Error("Usage: /refresh".to_string());
        }
        CommandResult::RefreshProviderState
    }
}
