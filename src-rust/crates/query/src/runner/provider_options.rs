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

pub(crate) fn is_openai_reasoning_model(model_id: &str) -> bool {
    clawde_api::providers::effort_shaping::openai_reasoning_model(model_id)
}

pub(crate) fn is_openaiish_provider(provider_id: &str) -> bool {
    // Single source of truth moved to clawde-api so the FreeProvider chain
    // and the gateway share the same list (effort_shaping.rs).
    clawde_api::providers::effort_shaping::is_openaiish_provider(provider_id)
}

pub(crate) fn build_provider_options(
    provider_id: &str,
    model_id: &str,
    effort_level: Option<clawde_core::effort::EffortLevel>,
    thinking_budget: Option<u32>,
    max_tokens: Option<u32>,
    provider_settings_options: Option<&std::collections::HashMap<String, Value>>,
) -> Value {
    let mut options = serde_json::Map::new();
    let model_id = model_id.to_ascii_lowercase();

    // Per-provider thinking / effort shaping — single source of truth shared
    // with the FreeProvider chain (effort_shaping::shape_provider_thinking).
    // Direct-only extras (Codex effort tiers, reasoning summary, gpt-5 text
    // verbosity) are layered on below so the shared core stays free of
    // provider-specific quirks.
    clawde_api::providers::effort_shaping::shape_provider_thinking(
        &mut options,
        provider_id,
        &model_id,
        effort_level,
        thinking_budget,
        max_tokens,
    );

    // GitHub Copilot adapter: claude models take a thinking budget; gpt-5
    // (non-pro) takes reasoningEffort + encrypted reasoning round-trip.
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

    // Amazon Bedrock: reasoningConfig for claude (budgetTokens) or effort
    // mapped to maxReasoningEffort.
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

    // OpenAI reasoning families: the shared core already inserted
    // reasoningEffort; layer the direct-only Codex (ChatGPT) extras on top.
    if is_openaiish_provider(provider_id) && is_openai_reasoning_model(&model_id) {
        // Codex accepts the full gpt-5 effort ladder including `xhigh`, so
        // surface the top tiers (XHigh / Max / Ultracode) as "extra high"
        // there — matching opencode — without changing the value sent to
        // other OpenAI-compatible providers that may not accept it.
        if matches!(provider_id, "codex" | "openai-codex")
            && matches!(
                effort_level,
                Some(clawde_core::effort::EffortLevel::XHigh)
                    | Some(clawde_core::effort::EffortLevel::Max)
                    | Some(clawde_core::effort::EffortLevel::Ultracode)
            )
        {
            options.insert("reasoningEffort".to_string(), serde_json::json!("xhigh"));
        }

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

    // OpenRouter: request usage in responses; gemini-3 reasoning effort hint.
    if provider_id == "openrouter" {
        options.insert("usage".to_string(), serde_json::json!({ "include": true }));
        if model_id.contains("gemini-3") {
            options.insert(
                "reasoning".to_string(),
                serde_json::json!({ "effort": "high" }),
            );
        }
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

    // Ollama: route the canonical options through the centralized conversion
    // helper. Chat runs on the native `/api/chat` transport, which honors
    // every canonical option (`options.num_ctx`, `num_predict`, sampling
    // controls, top-level `keep_alive`), so the full set rides to dispatch.
    if provider_id == "ollama" {
        let canonical: serde_json::Map<String, Value> = options
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        return clawde_api::providers::ollama_options::native_options_value(&canonical);
    }

    if options.is_empty() {
        Value::Null
    } else {
        Value::Object(options)
    }
}
