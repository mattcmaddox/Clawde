// Provider-option assembly: reasoning-effort mapping and per-provider request
// options. Extracted from lib.rs (issue #232). Behavior-preserving move.

use crate::*;

pub(crate) fn reasoning_effort_for_level(
    effort_level: clawde_core::effort::EffortLevel,
) -> &'static str {
    // Single source of truth: the composite FreeProvider shapes per-upstream
    // thinking options from the same ladder mapping, so the two paths can
    // never drift apart.
    clawde_api::providers::effort_shaping::openai_reasoning_effort_for_level(effort_level)
}

pub(crate) fn google_thinking_level_for_effort(
    effort_level: Option<clawde_core::effort::EffortLevel>,
) -> &'static str {
    clawde_api::providers::effort_shaping::google_thinking_level_for_effort(effort_level)
}

pub(crate) fn is_openai_reasoning_model(model_id: &str) -> bool {
    clawde_api::providers::effort_shaping::openai_reasoning_model(model_id)
}

pub(crate) fn is_openaiish_provider(provider_id: &str) -> bool {
    matches!(
        provider_id,
        "openai"
            | "azure"
            | "groq"
            | "mistral"
            | "deepseek"
            | "xai"
            | "openrouter"
            | "togetherai"
            | "together-ai"
            | "perplexity"
            | "cerebras"
            | "deepinfra"
            | "venice"
            | "huggingface"
            | "nvidia"
            | "cloudflare"
            | "siliconflow"
            | "sambanova"
            | "moonshot"
            | "zhipu"
            | "zai"
            | "qwen"
            | "alibaba"
            | "nebius"
            | "novita"
            | "ovhcloud"
            | "scaleway"
            | "vultr"
            | "vultr-ai"
            | "baseten"
            | "friendli"
            | "upstage"
            | "stepfun"
            | "fireworks"
            | "ollama"
            | "codex"
            | "openai-codex"
            | "poolside"
            | "lmstudio"
            | "lm-studio"
            | "llamacpp"
            | "llama-cpp"
    )
}

pub(crate) fn build_provider_options(
    provider_id: &str,
    model_id: &str,
    effort_level: Option<clawde_core::effort::EffortLevel>,
    thinking_budget: Option<u32>,
    provider_settings_options: Option<&std::collections::HashMap<String, Value>>,
) -> Value {
    let mut options = serde_json::Map::new();
    let model_id = model_id.to_ascii_lowercase();

    if provider_id == "github-copilot" {
        if model_id.contains("claude") {
            options.insert(
                "thinking_budget".to_string(),
                serde_json::json!(thinking_budget.unwrap_or(4_000)),
            );
        } else if model_id.starts_with("gpt-5") && !model_id.contains("gpt-5-pro") {
            let reasoning_effort = effort_level
                .map(reasoning_effort_for_level)
                .unwrap_or("medium");
            options.insert(
                "reasoningEffort".to_string(),
                serde_json::json!(reasoning_effort),
            );
            options.insert("reasoningSummary".to_string(), serde_json::json!("auto"));
            options.insert(
                "include".to_string(),
                serde_json::json!(["reasoning.encrypted_content"]),
            );

            if model_id.contains("gpt-5.")
                && !model_id.contains("codex")
                && !model_id.contains("-chat")
            {
                options.insert("textVerbosity".to_string(), serde_json::json!("low"));
            }
        }
    }

    if provider_id == "google" && model_id.contains("gemini") {
        if model_id.contains("2.5") {
            if effort_level == Some(clawde_core::effort::EffortLevel::None) {
                options.insert(
                    "thinkingConfig".to_string(),
                    serde_json::json!({
                        "includeThoughts": false,
                        "thinkingBudget": 0,
                    }),
                );
            } else if let Some(budget) = thinking_budget {
                options.insert(
                    "thinkingConfig".to_string(),
                    serde_json::json!({
                        "includeThoughts": true,
                        "thinkingBudget": budget,
                    }),
                );
            }
        } else if model_id.contains("3.") || model_id.contains("gemini-3") {
            let disabled = effort_level == Some(clawde_core::effort::EffortLevel::None);
            options.insert(
                "thinkingConfig".to_string(),
                serde_json::json!({
                    "includeThoughts": !disabled,
                    "thinkingLevel": if disabled {
                        "minimal"
                    } else {
                        google_thinking_level_for_effort(effort_level)
                    },
                }),
            );
        }
    }

    // DeepSeek exposes thinking independently of the OpenAI GPT reasoning
    // model families. Keep this mapping provider/model-specific so DeepSeek V4
    // and reasoner/chat variants do not depend on an unrelated GPT-5 check.
    if provider_id == "deepseek" {
        match effort_level {
            Some(clawde_core::effort::EffortLevel::None)
            | Some(clawde_core::effort::EffortLevel::Low) => {
                options.insert(
                    "thinking".to_string(),
                    serde_json::json!({ "type": "disabled" }),
                );
            }
            Some(clawde_core::effort::EffortLevel::XHigh)
            | Some(clawde_core::effort::EffortLevel::Max)
            | Some(clawde_core::effort::EffortLevel::Ultracode) => {
                options.insert(
                    "thinking".to_string(),
                    serde_json::json!({ "type": "enabled" }),
                );
                options.insert("reasoningEffort".to_string(), serde_json::json!("max"));
            }
            None
            | Some(clawde_core::effort::EffortLevel::Minimal)
            | Some(clawde_core::effort::EffortLevel::Medium)
            | Some(clawde_core::effort::EffortLevel::High) => {
                options.insert(
                    "thinking".to_string(),
                    serde_json::json!({ "type": "enabled" }),
                );
                options.insert("reasoningEffort".to_string(), serde_json::json!("high"));
            }
        }
    }

    if provider_id == "amazon-bedrock" {
        if model_id.contains("anthropic") || model_id.contains("claude") {
            if let Some(budget) = thinking_budget {
                options.insert(
                    "reasoningConfig".to_string(),
                    serde_json::json!({
                        "type": "enabled",
                        "budgetTokens": budget.min(31_999),
                    }),
                );
            }
        } else if let Some(level) = effort_level {
            options.insert(
                "reasoningConfig".to_string(),
                serde_json::json!({
                    "type": "enabled",
                    "maxReasoningEffort": reasoning_effort_for_level(level),
                }),
            );
        }
    }

    if is_openaiish_provider(provider_id) && is_openai_reasoning_model(&model_id) {
        let reasoning_effort = effort_level
            .map(reasoning_effort_for_level)
            .unwrap_or("medium");
        // Codex (ChatGPT) accepts the full gpt-5 effort ladder including
        // `xhigh`, so surface the top tiers (XHigh / Max / Ultracode) as "extra
        // high" there — matching opencode — without changing the value sent to
        // other OpenAI-compatible providers that may not accept it.
        let reasoning_effort = if matches!(provider_id, "codex" | "openai-codex")
            && matches!(
                effort_level,
                Some(clawde_core::effort::EffortLevel::XHigh)
                    | Some(clawde_core::effort::EffortLevel::Max)
                    | Some(clawde_core::effort::EffortLevel::Ultracode)
            ) {
            "xhigh"
        } else {
            reasoning_effort
        };
        options.insert(
            "reasoningEffort".to_string(),
            serde_json::json!(reasoning_effort),
        );

        // Match opencode's gpt-5 defaults for the Codex (ChatGPT) endpoint:
        // request an auto reasoning summary and carry encrypted reasoning state
        // across stateless turns. Scoped to Codex so other OpenAI-compatible
        // providers that ignore these fields are unaffected.
        if matches!(provider_id, "codex" | "openai-codex") {
            options.insert("reasoningSummary".to_string(), serde_json::json!("auto"));
            options.insert(
                "include".to_string(),
                serde_json::json!(["reasoning.encrypted_content"]),
            );
        }

        if model_id.starts_with("gpt-5")
            && model_id.contains("gpt-5.")
            && !model_id.contains("codex")
            && !model_id.contains("-chat")
            && provider_id != "azure"
        {
            options.insert("textVerbosity".to_string(), serde_json::json!("low"));
        }
    }

    if provider_id == "openrouter" {
        options.insert("usage".to_string(), serde_json::json!({ "include": true }));
        if model_id.contains("gemini-3") {
            options.insert(
                "reasoning".to_string(),
                serde_json::json!({ "effort": "high" }),
            );
        }
    }

    if provider_id == "qwen" && thinking_budget.is_some() && !model_id.contains("kimi-k2-thinking")
    {
        options.insert("enable_thinking".to_string(), serde_json::json!(true));
    }

    // Z.AI / Zhipu: GLM models use `thinking.type` enabled/disabled.
    // Thinking is on by default for reasoning models; effort None disables it.
    if provider_id == "zhipu" || provider_id == "zai" {
        let enabled = effort_level != Some(clawde_core::effort::EffortLevel::None);
        options.insert(
            "thinking".to_string(),
            serde_json::json!({
                "type": if enabled { "enabled" } else { "disabled" },
                "clear_thinking": false,
            }),
        );
    }

    // Poolside: binary thinking toggle. Thinking is on by default and
    // consumes from max_tokens; effort None turns it off via
    // chat_template_kwargs. No multi-level control exists.
    if provider_id == "poolside" && effort_level == Some(clawde_core::effort::EffortLevel::None) {
        options.insert(
            "chat_template_kwargs".to_string(),
            serde_json::json!({ "enable_thinking": false }),
        );
    }

    // Merge provider-specific options from settings.json (e.g. Ollama's
    // num_ctx). Settings options override auto-generated ones so the user
    // can pin a context window from their dev machine without remoting
    // into the inference host.
    if let Some(settings_opts) = provider_settings_options {
        for (key, value) in settings_opts {
            options.insert(key.clone(), value.clone());
        }
    }

    if options.is_empty() {
        Value::Null
    } else {
        Value::Object(options)
    }
}
