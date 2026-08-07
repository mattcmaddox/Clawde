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
        "Usage: /spec <task description>\n\n\
         Analyzes the repository (tracked files + current diff), asks the LLM\n\
         for a structured spec, and writes it to specs/<title>.json. The spec\n\
         contains requirements, a file plan, data models, acceptance tests,\n\
         and edge cases.\n\n\
         Example:\n\
           /spec Add a rate-limiting middleware to the API server"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let task = args.trim();
        if task.is_empty() {
            return CommandResult::Message(
                "Usage: /spec <task description>\n\
                 e.g. /spec add a rate-limiting middleware to the API server"
                    .to_string(),
            );
        }

        // ------------------------------------------------------------------
        // 1. Gather repository context
        // ------------------------------------------------------------------
        let repo_root = clawde_core::git_utils::get_repo_root(&ctx.working_dir)
            .unwrap_or_else(|| ctx.working_dir.clone());

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
        let spec = match clawde_core::spec::Spec::parse_json(&spec_json) {
            Ok(spec) => spec,
            Err(e) => {
                return CommandResult::Error(format!(
                    "{e}\n\nRaw model output (first 2000 chars):\n\n{}",
                    &spec_json[..spec_json.len().min(2000)]
                ));
            }
        };

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
        }
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
