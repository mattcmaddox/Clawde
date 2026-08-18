// `/spec` command — Spec-Driven Development mode (audit spec §10).
//
// Generates a structured specification for a non-trivial task *before* any
// code is written: requirements, a file plan, data models, acceptance tests,
// and edge cases. The user reviews the generated spec (accept/edit/reject
// TUI flow is a later milestone), and the acceptance tests become the
// verification criteria for the implementation (§10.4).
//
// The LLM is asked for pure JSON; output is parsed into
// [`clawde_core::spec::Spec`] and written to `specs/<slug>.json` in the
// repository root.

use super::*;
use async_trait::async_trait;

pub struct SpecCommand;

// ---- /spec -----------------------------------------------------------------

#[async_trait]
impl SlashCommand for SpecCommand {
    fn name(&self) -> &str {
        "spec"
    }
    fn description(&self) -> &str {
        "Generate a structured specification for a task before writing code"
    }
    fn help(&self) -> &str {
        "Usage: /spec <task description> | /spec list\n\n\
         Analyzes the repository (tracked files + current diff), asks the LLM\n\
         for a structured spec, and writes it to specs/<title>.json. The spec\n\
         contains requirements, a file plan, data models, acceptance tests,\n\
         and edge cases.\n\n\
         /spec list prints every spec in specs/ (newest first) with its title\n\
         and last-modified time — the headless counterpart to the TUI's\n\
         /spec-review picker.\n\n\
         Examples:\n\
           /spec Add a rate-limiting middleware to the API server\n\
           /spec list"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let task = args.trim();
        if task.is_empty() {
            return CommandResult::Message(
                "Usage: /spec <task description> | /spec list\n\
                 e.g. /spec add a rate-limiting middleware to the API server"
                    .to_string(),
            );
        }

        // ------------------------------------------------------------------
        // 1. Gather repository context
        // ------------------------------------------------------------------
        let repo_root = clawde_core::git_utils::get_repo_root(&ctx.working_dir)
            .unwrap_or_else(|| ctx.working_dir.clone());

        // /spec list: enumerate every parseable spec, newest first — the
        // headless counterpart to the /spec-review picker.
        if task == "list" {
            let specs = clawde_core::spec::Spec::list_specs(&repo_root);
            if specs.is_empty() {
                return CommandResult::Message(
                    "No specs found — run /spec <task> to generate one first.".to_string(),
                );
            }
            let mut out = String::from("# Specs\n");
            for (i, path) in specs.iter().enumerate() {
                let title = std::fs::read_to_string(path)
                    .ok()
                    .and_then(|raw| clawde_core::spec::Spec::parse_json(&raw).ok())
                    .map(|s| s.title)
                    .unwrap_or_else(|| path.display().to_string());
                let modified = std::fs::metadata(path)
                    .and_then(|m| m.modified())
                    .ok()
                    .map(|t| format_modified(&t));
                let modified = modified.unwrap_or_else(|| "?".to_string());
                out.push_str(&format!(
                    "{}. {} — {} ({modified})\n",
                    i + 1,
                    title,
                    path.display()
                ));
            }
            out.push_str(
                "\nReview a spec with /spec-review <path> (or the picker with /spec-review).",
            );
            return CommandResult::Message(out);
        }

        let file_tree = tracked_files(&repo_root);

        // Combine staged + unstaged diff as the "current state" hint.
        let staged = clawde_core::git_utils::get_staged_diff(&repo_root);
        let unstaged = clawde_core::git_utils::get_unstaged_diff(&repo_root);
        let mut diff = String::new();
        if !staged.is_empty() {
            diff.push_str(&staged);
        }
        if !unstaged.is_empty() {
            if !diff.is_empty() {
                diff.push('\n');
            }
            diff.push_str(&unstaged);
        }
        const MAX_DIFF_CHARS: usize = 60_000;
        if diff.len() > MAX_DIFF_CHARS {
            diff = format!(
                "{}\n\n[... diff truncated at {} chars ...]",
                &diff[..MAX_DIFF_CHARS],
                MAX_DIFF_CHARS
            );
        }

        // ------------------------------------------------------------------
        // 2. Ask the LLM for a structured spec
        // ------------------------------------------------------------------
        let model = ctx.config.effective_model().to_string();
        let provider = match resolve_command_provider(ctx).await {
            Some(provider) => provider,
            None => {
                return CommandResult::Error(
                    "Cannot initialise provider client for spec generation.".to_string(),
                );
            }
        };

        let spec_prompt = format!(
            "You are a senior software architect. Produce a structured specification\n\
             for the following task. Analyze the repository context (file tree and\n\
             current diff) and plan the implementation carefully.\n\n\
             Task:\n\
             {task}\n\n\
             Repository file tree (tracked files):\n\
             ```\n\
             {file_tree}\n\
             ```\n\n\
             Current diff:\n\
             ```diff\n\
             {diff}\n\
             ```\n\n\
             Respond with a single JSON object (no markdown fences, no prose\n\
             outside the JSON) shaped exactly like this:\n\
             {{\n\
               \"title\": \"Short task title\",\n\
               \"requirements\": [\"Functional requirement 1\", \"...\"],\n\
               \"files_to_touch\": [\n\
                 {{\"path\": \"path/relative/to/repo\", \"action\": \"Create\" | \"Modify\" | \"Delete\", \"description\": \"why/what\"}}\n\
               ],\n\
               \"data_models\": [{{\"name\": \"TypeName\", \"definition\": \"fields or shape\"}}],\n\
               \"acceptance_tests\": [{{\"description\": \"Testable acceptance criterion\"}}],\n\
               \"edge_cases\": [\"Edge case to handle\"]\n\
             }}"
        );

        let request = clawde_api::ProviderRequest {
            model,
            messages: vec![Message::user(spec_prompt)],
            system_prompt: Some(clawde_api::SystemPrompt::Text(
                "You are a precise, structured systems architect. Always emit valid \
                 JSON exactly matching the requested schema — nothing else."
                    .to_string(),
            )),
            tools: vec![],
            max_tokens: 4096,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: vec![],
            thinking: None,
            effort_level: ctx.effort,
            provider_options: serde_json::Value::Object(Default::default()),
        };

        let spec_json = match provider.create_message(request).await {
            Err(e) => return CommandResult::Error(format!("LLM call failed: {e}")),
            Ok(response) => {
                let text = text_from_content_blocks(&response.content);
                if text.trim().is_empty() {
                    return CommandResult::Error("LLM returned an empty spec.".to_string());
                }
                text
            }
        };

        // ------------------------------------------------------------------
        // 3. Parse, persist, and present the spec
        // ------------------------------------------------------------------
        let mut spec = match clawde_core::spec::Spec::parse_json(&spec_json) {
            Ok(spec) => spec,
            Err(e) => {
                return CommandResult::Error(format!(
                    "{e}\n\nRaw model output (first 2000 chars):\n\n{}",
                    &spec_json[..spec_json.len().min(2000)]
                ));
            }
        };

        // Bind this artifact to the exact task and session that generated it.
        // The model owns the plan content; the command owns provenance.
        spec.task = task.to_string();
        spec.task_id = uuid::Uuid::new_v4().to_string();
        spec.session_id = Some(ctx.session_id.clone());

        let specs_dir = repo_root.join("specs");
        let slug = slugify(&spec.title);
        let filename = if slug.is_empty() {
            format!(
                "spec-{}.json",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            )
        } else {
            format!("{slug}.json")
        };
        let path = specs_dir.join(&filename);
        if let Err(e) = spec.write_to(&path) {
            return CommandResult::Error(format!(
                "Spec generated but could not be written to {}: {e}",
                path.display()
            ));
        }
        if let Err(e) = clawde_core::spec::Spec::clear_approval(&repo_root) {
            return CommandResult::Error(format!(
                "Spec generated at {}, but the previous approval could not be cleared: {e}",
                path.display()
            ));
        }

        CommandResult::Message(format_spec_message(&spec, &path))
    }
}

/// List tracked files via `git ls-files`, one per line, truncated.
fn tracked_files(repo_root: &std::path::Path) -> String {
    let out = std::process::Command::new("git")
        .current_dir(repo_root)
        .args(["ls-files"])
        .output();
    let mut list = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => return "(not a git repository)".to_string(),
    };
    const MAX_FILES: usize = 600;
    let line_count = list.lines().count();
    if line_count > MAX_FILES {
        let keep: Vec<&str> = list.lines().take(MAX_FILES).collect();
        list = format!(
            "{}\n[... {} more files ...]",
            keep.join("\n"),
            line_count - MAX_FILES
        );
    }
    list
}

// ---- /spec-mode ----------------------------------------------------------

/// Toggle Spec-Driven Development mode (`/spec-mode [on|off]`).
///
/// When enabled, the continuation policy stops after a turn that produced a
/// spec (`specs/<title>.json`) so the user can review (Accept/Edit/Reject)
/// it before implementation (audit spec §10.2).
pub struct SpecModeCommand;

#[async_trait]
impl SlashCommand for SpecModeCommand {
    fn name(&self) -> &str {
        "spec-mode"
    }
    fn description(&self) -> &str {
        "Toggle Spec-Driven Development mode on/off"
    }
    fn arg_completions(&self, _partial: &str) -> Vec<ArgCompletion> {
        vec![
            ArgCompletion {
                value: "on".into(),
                description: "Enable spec mode".into(),
                available: true,
            },
            ArgCompletion {
                value: "off".into(),
                description: "Disable spec mode".into(),
                available: true,
            },
        ]
    }
    fn help(&self) -> &str {
        "Usage: /spec-mode [on|off]\n\n\
         Toggles Spec-Driven Development mode (audit spec §10). When enabled, the\n\
         agent stops after generating a spec (specs/<title>.json) so you can\n\
         review it — Accept to implement, Edit to change, Reject to discard —\n\
         before any code is written.\n\n\
         Subcommands:\n\
           /spec-mode        - toggle status\n\
           /spec-mode on     - enable spec mode\n\
           /spec-mode off    - disable spec mode\n\
         The setting is persisted in settings.json (\"specMode\")."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let current = ctx.config.spec_mode;
        let new_value = match args.trim() {
            "on" | "enable" | "1" | "true" => true,
            "off" | "disable" | "0" | "false" => false,
            "" => !current,
            other => {
                return CommandResult::Error(format!(
                    "Unknown argument '{other}'. Use 'on', 'off', or no argument to toggle."
                ));
            }
        };

        if new_value == current {
            return CommandResult::Message(format!(
                "Spec mode is already {}.",
                if current { "enabled" } else { "disabled" }
            ));
        }

        // Persist the setting via settings.json.
        if let Err(e) = save_settings_mutation(|settings| {
            settings.config.spec_mode = new_value;
        }) {
            return CommandResult::Error(format!("Failed to save setting: {e}"));
        }

        let mut new_config = ctx.config.clone();
        new_config.spec_mode = new_value;
        let msg = format!(
            "Spec mode {}. In this mode the agent writes a structured spec and waits\
             for your review before implementing.",
            if new_value { "enabled" } else { "disabled" }
        );
        CommandResult::ConfigChangeMessage(new_config, msg)
    }
}

/// Lowercase, hyphenated, filesystem-safe slug from a title.
fn slugify(title: &str) -> String {
    let mut slug: Vec<char> = Vec::new();
    for c in title.to_lowercase().chars() {
        if c.is_alphanumeric() {
            slug.push(c);
        } else if !slug.is_empty() && *slug.last().unwrap() != '-' {
            slug.push('-');
        }
    }
    while slug.last().copied() == Some('-') {
        slug.pop();
    }
    slug.into_iter().collect()
}

/// Render the spec as a markdown message for the TUI / REPL.
/// Format a file modification time as a compact local date-time.
fn format_modified(t: &std::time::SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Local> = (*t).into();
    dt.format("%Y-%m-%d %H:%M").to_string()
}

fn format_spec_message(spec: &clawde_core::spec::Spec, path: &std::path::Path) -> String {
    use clawde_core::spec::FileAction;

    let mut out = format!("# Spec: {}\n", spec.title);

    out.push_str("\n## Requirements\n");
    for (i, req) in spec.requirements.iter().enumerate() {
        out.push_str(&format!("{}. {}\n", i + 1, req));
    }

    out.push_str("\n## Files to Touch\n");
    if spec.files_to_touch.is_empty() {
        out.push_str("_none_\n");
    }
    for f in &spec.files_to_touch {
        let action = match f.action {
            FileAction::Create => "NEW",
            FileAction::Modify => "MODIFY",
            FileAction::Delete => "DELETE",
        };
        out.push_str(&format!("- [{action}] {} — {}\n", f.path, f.description));
    }

    out.push_str("\n## Data Models\n");
    if spec.data_models.is_empty() {
        out.push_str("_none_\n");
    }
    for d in &spec.data_models {
        out.push_str(&format!("- `{}` — {}\n", d.name, d.definition));
    }

    out.push_str("\n## Acceptance Tests\n");
    if spec.acceptance_tests.is_empty() {
        out.push_str("_none_\n");
    }
    for (i, t) in spec.acceptance_tests.iter().enumerate() {
        out.push_str(&format!("{}. {}\n", i + 1, t.description));
    }

    out.push_str("\n## Edge Cases\n");
    if spec.edge_cases.is_empty() {
        out.push_str("_none_\n");
    }
    for e in &spec.edge_cases {
        out.push_str(&format!("- {e}\n"));
    }

    out.push_str(&format!(
        "\n---\nSaved to `{}`.\n\n*Next: implement against this spec, then verify \
         the acceptance tests (Verify loop).*",
        path.display()
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ctx() -> CommandContext {
        CommandContext {
            config: clawde_core::config::Config::default(),
            cost_tracker: clawde_core::CostTracker::new(),
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

    #[test]
    fn spec_list_lists_specs_newest_first() {
        let dir = std::env::temp_dir().join(format!("clawde-spec-list-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("specs")).unwrap();
        let old = clawde_core::spec::Spec {
            title: "Old Spec".to_string(),
            ..Default::default()
        };
        old.write_to(&dir.join("specs/old.json")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let fresh = clawde_core::spec::Spec {
            title: "Fresh Spec".to_string(),
            ..Default::default()
        };
        fresh.write_to(&dir.join("specs/fresh.json")).unwrap();

        let mut ctx = make_ctx();
        ctx.working_dir = dir.clone();
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(SpecCommand.execute("list", &mut ctx));
        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("Old Spec"), "msg: {msg}");
                assert!(msg.contains("Fresh Spec"), "msg: {msg}");
                // Newest (by mtime) is listed first.
                let fresh_at = msg.find("Fresh Spec").expect("fresh listed");
                let old_at = msg.find("Old Spec").expect("old listed");
                assert!(fresh_at < old_at, "newest spec must sort first");
            }
            other => panic!("expected Message, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spec_list_reports_none_without_specs() {
        let dir =
            std::env::temp_dir().join(format!("clawde-spec-list-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut ctx = make_ctx();
        ctx.working_dir = dir.clone();
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(SpecCommand.execute("list", &mut ctx));
        match result {
            CommandResult::Message(msg) => assert!(msg.contains("No specs found")),
            other => panic!("expected Message, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A deterministic provider returning a fixed spec JSON.
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
                model: "canned".into(),
                content: vec![clawde_core::ContentBlock::Text {
                    text: r#"{"title":"Example Feature","requirements":["Do the thing"],"files_to_touch":[{"path":"src/thing.rs","action":"Create","description":"New file"}],"data_models":[],"acceptance_tests":[{"description":"Thing works"}],"edge_cases":[]}"#
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
            unimplemented!("canned spec provider does not support streaming")
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

    #[test]
    fn spec_command_writes_and_renders_spec() {
        // Run in a temp working dir so the command's specs/ output never
        // pollutes the crate checkout (hermetic under parallel tests).
        let dir = std::env::temp_dir().join(format!("clawde-spec-cmd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let mut ctx = make_ctx();
        ctx.working_dir = dir.clone();
        ctx.test_provider = Some(std::sync::Arc::new(CannedSpecProvider));
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(SpecCommand.execute("add example feature", &mut ctx));

        let artifact = dir.join("specs/example-feature.json");
        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("# Spec: Example Feature"), "msg: {msg}");
                assert!(msg.contains("Do the thing"));
                assert!(msg.contains("[NEW] src/thing.rs"));
                assert!(msg.contains("Thing works"));
                assert!(msg.contains("specs/example-feature.json"));
            }
            other => panic!("expected Message, got {other:?}"),
        }
        assert!(artifact.exists(), "spec file written on disk");
        let on_disk = std::fs::read_to_string(&artifact).expect("read spec back");
        assert!(on_disk.contains("Example Feature"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slugify_produces_safe_hyphenated_slug() {
        assert_eq!(
            slugify("Rate-Limiting Middleware!"),
            "rate-limiting-middleware"
        );
        assert_eq!(slugify(""), "");
        assert_eq!(slugify("Hello, World"), "hello-world");
    }

    #[test]
    fn spec_mode_command_toggles_config() {
        // The command persists via settings.json — point CLAWDE_HOME at a
        // temp dir (with the shared env lock) so the real settings file is
        // never touched.
        let _home = crate::keys::tests::TestHome::new();
        let mut ctx = make_ctx();
        assert!(!ctx.config.spec_mode);
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(SpecModeCommand.execute("on", &mut ctx));
        let new_cfg = match result {
            CommandResult::ConfigChangeMessage(new_cfg, msg) => {
                assert!(new_cfg.spec_mode, "spec_mode must flip on");
                assert!(msg.contains("Spec mode enabled"), "msg: {msg}");
                new_cfg
            }
            other => panic!("expected ConfigChangeMessage, got: {other:?}"),
        };
        // The caller applies the returned config before the next invocation
        // (mirrors the CLI's `cmd_ctx.config = applied_cfg.clone()`).
        ctx.config = new_cfg;

        // Off flips it back.
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(SpecModeCommand.execute("off", &mut ctx));
        match result {
            CommandResult::ConfigChangeMessage(new_cfg, msg) => {
                assert!(!new_cfg.spec_mode);
                assert!(msg.contains("Spec mode disabled"), "msg: {msg}");
            }
            other => panic!("expected ConfigChangeMessage, got: {other:?}"),
        }
    }

    #[test]
    fn spec_command_requires_args() {
        let mut ctx = make_ctx();
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(SpecCommand.execute("   ", &mut ctx));
        match result {
            CommandResult::Message(msg) => assert!(msg.contains("Usage: /spec")),
            other => panic!("expected usage Message, got {other:?}"),
        }
    }
}
