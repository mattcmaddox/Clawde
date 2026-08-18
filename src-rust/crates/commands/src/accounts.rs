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
            let after = trimmed.strip_prefix("--label").unwrap_or("").trim();
            let mut results = Vec::new();
            for provider in [
                clawde_core::accounts::PROVIDER_ANTHROPIC,
                clawde_core::accounts::PROVIDER_CODEX,
            ] {
                for profile in registry.list(provider) {
                    if after.is_empty()
                        || profile.id.to_lowercase().starts_with(&after.to_lowercase())
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
            // The label is a free-form name; show a dimmed placeholder hint
            // while it is still empty so the popup says what goes next.
            if after.is_empty() {
                if let Some(hint) = super::free_form_arg_hint(
                    "--label",
                    "<name>",
                    "Name this profile so /switch can find it later",
                    false,
                ) {
                    results.push(hint);
                }
            }
            return results;
        }

        // Otherwise offer the flags.
        vec![
            ArgCompletion {
                value: "--console".into(),
                description: "Login with API key (Console)".into(),
                available: true,
            },
            ArgCompletion {
                value: "--codex".into(),
                description: "Login with ChatGPT/Codex account".into(),
                available: true,
            },
            ArgCompletion {
                value: "--label".into(),
                description: "Set a profile label for the saved account".into(),
                available: true,
            },
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
            ArgCompletion {
                value: "--codex".into(),
                description: "Log out the Codex account".into(),
                available: true,
            },
            ArgCompletion {
                value: "--all".into(),
                description: "Log out all accounts".into(),
                available: true,
            },
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
                        || c.value
                            .to_lowercase()
                            .starts_with(&format!("--codex {}", after))
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Panic-safe guard: sets `CLAWDE_HOME` to a temp dir with an optional
    /// test registry, and restores the original env var on drop (even during
    /// unwinding from a panic).
    ///
    /// Holds the shared [`crate::tests::CLAWDE_HOME_LOCK`] for its lifetime so
    /// tests that mutate the process-global env var never race each other.
    struct TestAccounts {
        _lock: std::sync::MutexGuard<'static, ()>,
        _tmp: tempfile::TempDir,
        prev_clawde_home: Option<std::ffi::OsString>,
    }

    impl TestAccounts {
        /// Seed a registry with 2 Anthropic profiles + 1 Codex profile,
        /// marking "work" as the active Anthropic account and "gpt" as the
        /// active Codex account.
        fn seeded() -> Self {
            let lock = crate::tests::CLAWDE_HOME_LOCK
                .get_or_init(|| std::sync::Mutex::new(()))
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let prev = std::env::var_os("CLAWDE_HOME");
            let tmp = tempfile::tempdir().unwrap();
            std::env::set_var("CLAWDE_HOME", tmp.path());

            let registry = clawde_core::accounts::AccountRegistry {
                version: 1,
                providers: {
                    let mut pm = std::collections::BTreeMap::new();

                    let mut anthropic = clawde_core::accounts::ProviderAccounts {
                        active: Some("work".to_string()),
                        ..Default::default()
                    };
                    anthropic.profiles.insert(
                        "work".to_string(),
                        clawde_core::accounts::AccountProfile {
                            id: "work".into(),
                            email: Some("me@work.com".into()),
                            ..Default::default()
                        },
                    );
                    anthropic.profiles.insert(
                        "pro".to_string(),
                        clawde_core::accounts::AccountProfile {
                            id: "pro".into(),
                            email: Some("me@pro.com".into()),
                            ..Default::default()
                        },
                    );
                    pm.insert(
                        clawde_core::accounts::PROVIDER_ANTHROPIC.to_string(),
                        anthropic,
                    );

                    let mut codex = clawde_core::accounts::ProviderAccounts {
                        active: Some("gpt".to_string()),
                        ..Default::default()
                    };
                    codex.profiles.insert(
                        "gpt".to_string(),
                        clawde_core::accounts::AccountProfile {
                            id: "gpt".into(),
                            email: Some("me@openai.com".into()),
                            ..Default::default()
                        },
                    );
                    pm.insert(clawde_core::accounts::PROVIDER_CODEX.to_string(), codex);
                    pm
                },
            };
            registry.save().unwrap();

            TestAccounts {
                _lock: lock,
                _tmp: tmp,
                prev_clawde_home: prev,
            }
        }

        /// Point CLAWDE_HOME at a temp dir with NO accounts.json (empty
        /// registry).
        fn empty() -> Self {
            let lock = crate::tests::CLAWDE_HOME_LOCK
                .get_or_init(|| std::sync::Mutex::new(()))
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let prev = std::env::var_os("CLAWDE_HOME");
            let tmp = tempfile::tempdir().unwrap();
            std::env::set_var("CLAWDE_HOME", tmp.path());
            TestAccounts {
                _lock: lock,
                _tmp: tmp,
                prev_clawde_home: prev,
            }
        }
    }

    impl Drop for TestAccounts {
        fn drop(&mut self) {
            match &self.prev_clawde_home {
                Some(v) => std::env::set_var("CLAWDE_HOME", v),
                None => std::env::remove_var("CLAWDE_HOME"),
            }
        }
    }

    #[test]
    fn login_arg_completions_empty_returns_flags() {
        let _env = TestAccounts::seeded();
        let cmd = LoginCommand;
        let completions = cmd.arg_completions("");
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"--console"));
        assert!(values.contains(&"--codex"));
        assert!(values.contains(&"--label"));
        assert_eq!(completions.len(), 3, "expected exactly 3 flags");
    }

    #[test]
    fn login_arg_completions_label_returns_profile_ids() {
        let _env = TestAccounts::seeded();
        let cmd = LoginCommand;
        let completions = cmd.arg_completions("--label");
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        // Should include all three profiles, prefixed with --label
        assert!(values.contains(&"--label work"));
        assert!(values.contains(&"--label pro"));
        assert!(values.contains(&"--label gpt"));
        // Should NOT include bare flags
        assert!(!values.contains(&"--console"));
        assert!(!values.contains(&"--codex"));
    }

    #[test]
    fn login_arg_completions_label_filters_by_prefix() {
        let _env = TestAccounts::seeded();
        let cmd = LoginCommand;
        let completions = cmd.arg_completions("--label wo");
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        // Should only match "work" (starts with "wo")
        assert!(
            values.contains(&"--label work"),
            "expected --label work to match"
        );
        assert!(
            !values.contains(&"--label pro"),
            "pro should not match 'wo'"
        );
        assert!(
            !values.contains(&"--label gpt"),
            "gpt should not match 'wo'"
        );
    }

    #[test]
    fn login_arg_completions_label_no_profiles_shows_placeholder_hint() {
        let _env = TestAccounts::empty();
        let cmd = LoginCommand;
        let completions = cmd.arg_completions("--label");
        // No profiles exist, so only the faded free-form hint remains: the
        // popup must tell the user a name goes next instead of showing nothing.
        assert_eq!(
            completions.len(),
            1,
            "expected only the placeholder hint: {:?}",
            completions
        );
        assert_eq!(completions[0].value, "--label <name>");
        assert!(!completions[0].available);
    }

    // -----------------------------------------------------------------------
    // /switch arg completions
    // -----------------------------------------------------------------------

    #[test]
    fn switch_arg_completions_empty_returns_flag_and_profiles() {
        let _env = TestAccounts::seeded();
        let cmd = SwitchCommand;
        let completions = cmd.arg_completions("");
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"--codex"), "should offer --codex flag");
        assert!(values.contains(&"work"), "should list Anthropic profiles");
        assert!(
            values.contains(&"pro"),
            "should list both Anthropic profiles"
        );
        assert!(!values.contains(&"gpt"), "should NOT list Codex profiles");
        assert_eq!(
            completions.len(),
            3,
            "expected --codex + 2 Anthropic profiles"
        );
    }

    #[test]
    fn switch_arg_completions_prefix_filters_anthropic_profiles() {
        let _env = TestAccounts::seeded();
        let cmd = SwitchCommand;
        // arg_completions returns all possible items; the prefix filter is
        // applied by get_arg_completions.  Here we just verify the method
        // returns Anthropic profiles (unfiltered) alongside the --codex flag.
        let completions = cmd.arg_completions("wo");
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        // The method returns --codex + all Anthropic profiles; prefix
        // filtering happens at the get_arg_completions layer.
        assert!(
            values.contains(&"--codex"),
            "raw arg_completions always includes flag"
        );
        assert!(
            values.contains(&"work"),
            "raw results include all Anthropic profiles"
        );
        assert!(
            values.contains(&"pro"),
            "raw results include all Anthropic profiles"
        );
        assert!(
            values.len() >= 3,
            "expected at least 3 items (flag + 2 profiles)"
        );
    }

    #[test]
    fn switch_arg_completions_codex_returns_codex_profiles() {
        let _env = TestAccounts::seeded();
        let cmd = SwitchCommand;
        let completions = cmd.arg_completions("--codex");
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(
            values.contains(&"--codex gpt"),
            "should return prefixed Codex profile"
        );
        assert!(
            !values.contains(&"--codex"),
            "should NOT return bare --codex flag"
        );
        assert!(
            !values.contains(&"work"),
            "should NOT return Anthropic profiles"
        );
        assert_eq!(completions.len(), 1, "expected exactly 1 Codex profile");
    }

    #[test]
    fn switch_arg_completions_codex_prefix_filters() {
        let _env = TestAccounts::seeded();
        let cmd = SwitchCommand;
        let completions = cmd.arg_completions("--codex g");
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"--codex gpt"), "'g' should match 'gpt'");
        assert_eq!(completions.len(), 1, "expected only --codex gpt");
    }

    #[test]
    fn switch_arg_completions_empty_registry_returns_only_flag() {
        let _env = TestAccounts::empty();
        let cmd = SwitchCommand;
        let completions = cmd.arg_completions("");
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"--codex"), "flag always offered");
        assert_eq!(completions.len(), 1, "only --codex when no profiles exist");
    }

    // -----------------------------------------------------------------------
    // /logout arg completions
    // -----------------------------------------------------------------------

    #[test]
    fn logout_arg_completions_empty_returns_flags_and_active_hint() {
        let _env = TestAccounts::seeded();
        let cmd = LogoutCommand;
        let completions = cmd.arg_completions("");
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"--codex"), "should offer --codex flag");
        assert!(values.contains(&"--all"), "should offer --all flag");
        assert!(
            values.contains(&"work"),
            "should show active Anthropic profile"
        );
        // The active hint should be dimmed (not selectable)
        let work_hint = completions.iter().find(|c| c.value == "work").unwrap();
        assert!(!work_hint.available, "active hint should be dimmed");
        assert!(
            !values.contains(&"gpt"),
            "should NOT show active Codex profile"
        );
        assert_eq!(completions.len(), 3, "expected 2 flags + 1 active hint");
    }

    #[test]
    fn logout_arg_completions_codex_shows_active_codex_hint() {
        let _env = TestAccounts::seeded();
        let cmd = LogoutCommand;
        let completions = cmd.arg_completions("--codex");
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"--codex"), "should offer --codex flag");
        assert!(values.contains(&"--all"), "should offer --all flag");
        assert!(values.contains(&"gpt"), "should show active Codex profile");
        let gpt_hint = completions.iter().find(|c| c.value == "gpt").unwrap();
        assert!(!gpt_hint.available, "Codex hint should be dimmed");
        assert!(
            !values.contains(&"work"),
            "should NOT show Anthropic profile"
        );
        // 2 flags + 1 codex hint = 3. (--all stays because arg_completions adds
        // it unconditionally; get_arg_completions would filter it out.)
        assert_eq!(completions.len(), 3, "expected 2 flags + 1 codex hint");
    }

    #[test]
    fn logout_arg_completions_all_lists_profiles() {
        let _env = TestAccounts::seeded();
        let cmd = LogoutCommand;
        let completions = cmd.arg_completions("--all");
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"--codex"), "should offer --codex flag");
        assert!(values.contains(&"--all"), "should offer --all flag");
        // All three profiles should be listed as dimmed informational items
        assert!(values.contains(&"work"), "should list Anthropic 'work'");
        assert!(values.contains(&"pro"), "should list Anthropic 'pro'");
        assert!(values.contains(&"gpt"), "should list Codex 'gpt'");
        // Check --all-listed profiles are all dimmed
        // (work may appear twice: once from active hint, once from --all listing)
        for profile in ["pro", "gpt"] {
            let hint = completions.iter().find(|c| c.value == profile).unwrap();
            assert!(!hint.available, "{} profile should be dimmed", profile);
        }
        // work should appear at least once and be dimmed
        let work_hints: Vec<&ArgCompletion> =
            completions.iter().filter(|c| c.value == "work").collect();
        assert!(!work_hints.is_empty(), "work should appear at least once");
        for h in &work_hints {
            assert!(!h.available, "all work hints should be dimmed");
        }
    }

    #[test]
    fn logout_arg_completions_empty_without_profiles() {
        let _env = TestAccounts::empty();
        let cmd = LogoutCommand;
        let completions = cmd.arg_completions("");
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"--codex"));
        assert!(values.contains(&"--all"));
        assert_eq!(completions.len(), 2, "flags only when no active profile");
    }
}
