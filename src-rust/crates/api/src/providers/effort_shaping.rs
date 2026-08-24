//! Shared mappings from the canonical effort ladder to provider-native
//! thinking parameters.
//!
//! Single source of truth for two consumers that must never disagree:
//!
//! - the query layer, which shapes `provider_options` for direct (non-free)
//!   providers, and
//! - the composite FreeProvider, which re-shapes the same request per chain
//!   entry at dispatch time (the query layer cannot know which upstream will
//!   actually serve the request).
//!
//! Every function here is pure and deterministic over its inputs.

use clawde_core::effort::EffortLevel;

/// OpenAI-family `reasoning_effort` string for a level. Passed through
/// verbatim to OpenAI-compatible endpoints; `none`/`minimal` are the two rungs
/// below `low` and are only offered where the model's ladder exposes them.
pub fn openai_reasoning_effort_for_level(level: EffortLevel) -> &'static str {
    match level {
        EffortLevel::None => "none",
        EffortLevel::Minimal => "minimal",
        EffortLevel::Low => "low",
        EffortLevel::Medium => "medium",
        // XHigh/Max/Ultracode collapse to "high" for the generic OpenAI-family
        // `reasoning_effort` value. Providers that accept a higher tier get it
        // via a provider-specific mapping (e.g. DeepSeek's "max" below);
        // defaulting to "high" keeps unknown providers safe.
        EffortLevel::High | EffortLevel::XHigh | EffortLevel::Max | EffortLevel::Ultracode => {
            "high"
        }
    }
}

/// DeepSeek's `reasoning_effort` tier: the API only accepts `high` and `max`
/// (low/medium are absorbed by "high"; only the top of the ladder gets "max").
pub fn deepseek_reasoning_effort_for_level(level: EffortLevel) -> &'static str {
    match level {
        EffortLevel::XHigh | EffortLevel::Max | EffortLevel::Ultracode => "max",
        _ => "high",
    }
}

/// Google `thinkingLevel` for an effort override. Google's thinkingLevel has
/// no "none" — it floors at "low"; "minimal" is a real gemini-3 level, so
/// it passes through. `None` (no override) means the provider default which
/// Google treats as the highest tier, so we map it to "high".
pub fn google_thinking_level_for_effort(effort: Option<EffortLevel>) -> &'static str {
    match effort.unwrap_or(EffortLevel::High) {
        EffortLevel::None => "low",
        EffortLevel::Minimal => "minimal",
        EffortLevel::Low => "low",
        EffortLevel::Medium => "medium",
        // Gemini's top thinking level is "high".
        EffortLevel::High | EffortLevel::XHigh | EffortLevel::Max | EffortLevel::Ultracode => {
            "high"
        }
    }
}

/// Whether a model id belongs to the OpenAI reasoning families that accept
/// `reasoning_effort` on OpenAI-compatible endpoints (GPT-5 and O-series).
pub fn openai_reasoning_model(model_id: &str) -> bool {
    let model_id = model_id.to_ascii_lowercase();
    model_id.starts_with("gpt-5")
        || model_id.starts_with("o1")
        || model_id.starts_with("o3")
        || model_id.starts_with("o4")
}

/// Whether a model id is an OpenAI-compatible model known to expose thinking
/// via `reasoning_effort` on free-chain upstreams (DeepSeek, Qwen3, GPT-5,
/// O-series). Gate keeps unsupported models from receiving a parameter their
/// API would reject with a 400.
pub fn openai_compat_reasoning_model(model_id: &str) -> bool {
    let model_id = model_id.to_ascii_lowercase();
    model_id.starts_with("deepseek")
        || model_id.contains("/deepseek")
        || model_id.contains("qwen3")
        || model_id.contains("qwen-3")
        || openai_reasoning_model(&model_id)
}

/// Whether a provider speaks the OpenAI-compatible chat wire format and can
/// accept OpenAI-style thinking parameters (`reasoning_effort`, `thinking`,
/// `chat_template_kwargs`, …) on its request body. Previously the query
/// layer's private list; moved here so the FreeProvider chain and the future
/// HTTP gateway share one definition with the direct-provider path.
pub fn is_openaiish_provider(provider_id: &str) -> bool {
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

/// Shape per-provider thinking / effort parameters into `options`.
///
/// Single source of truth for the per-provider mapping, consumed by:
///
/// - the query layer's `build_provider_options` (direct providers), and
/// - FreeProvider's `shape_thinking_for_upstream` (per chain entry at
///   dispatch time — the query layer cannot know which upstream will serve a
///   `free` request).
///
/// Both callers previously maintained their own copies and drifted; every
/// provider arm below is shared. Provider-specific *extras* that only the
/// direct path needs (Codex effort tiers, `textVerbosity`, settings merge,
/// …) stay in the query layer and are layered on after this returns.
///
/// `model_id` must be pre-lowercased by the caller. `max_tokens` is the
/// request's (already capped) output budget and only affects Google 2.5's
/// `thinkingBudget`, which must stay below `maxOutputTokens`. No-op for
/// providers/models without thinking control.
pub fn shape_provider_thinking(
    options: &mut serde_json::Map<String, serde_json::Value>,
    provider_id: &str,
    model_id: &str,
    effort_level: Option<EffortLevel>,
    thinking_budget: Option<u32>,
    max_tokens: Option<u32>,
) {
    // Gemini: thinkingConfig lives in generationConfig and is expressed as a
    // budget (2.5 models) or a level (3.x models).
    if provider_id == "google" && model_id.contains("gemini") {
        if model_id.contains("2.5") {
            match effort_level {
                Some(EffortLevel::None) => {
                    options.insert(
                        "thinkingConfig".to_string(),
                        serde_json::json!({
                            "includeThoughts": false,
                            "thinkingBudget": 0,
                        }),
                    );
                }
                Some(level) => {
                    let mut budget = thinking_budget
                        .or_else(|| level.thinking_budget_tokens())
                        .unwrap_or(0);
                    // Gemini requires budget < maxOutputTokens; clamp against
                    // the request's (already capped) max_tokens when known.
                    if let Some(max_tokens) = max_tokens {
                        budget = budget.min(max_tokens.saturating_sub(1));
                    }
                    options.insert(
                        "thinkingConfig".to_string(),
                        serde_json::json!({
                            "includeThoughts": true,
                            "thinkingBudget": budget,
                        }),
                    );
                }
                None => {
                    // No effort override: leave the provider default.
                }
            }
        } else if model_id.contains("3.") || model_id.contains("gemini-3") {
            let off = effort_level == Some(EffortLevel::None);
            options.insert(
                "thinkingConfig".to_string(),
                serde_json::json!({
                    "includeThoughts": !off,
                    "thinkingLevel": if off {
                        "minimal"
                    } else {
                        google_thinking_level_for_effort(effort_level)
                    },
                }),
            );
        }
    }

    // DeepSeek exposes thinking independently of the OpenAI GPT reasoning
    // families. Model-gated so a deepseek model on any OpenAI-compatible
    // upstream (direct `deepseek`, or free-chain cline / opencode-zen /
    // openrouter) shapes identically. `none` and `low` both disable it.
    if model_id.starts_with("deepseek") || model_id.contains("/deepseek") {
        let disabled = matches!(
            effort_level,
            Some(EffortLevel::None) | Some(EffortLevel::Low)
        );
        options.insert(
            "thinking".to_string(),
            serde_json::json!({ "type": if disabled { "disabled" } else { "enabled" } }),
        );
        if disabled {
            // Drop any leftover effort key so a disabled request never
            // carries reasoning parameters.
            options.remove("reasoningEffort");
        } else {
            options.insert(
                "reasoningEffort".to_string(),
                serde_json::json!(deepseek_reasoning_effort_for_level(
                    effort_level.unwrap_or(EffortLevel::Medium)
                )),
            );
        }
    }

    // Qwen (DashScope): `enable_thinking` boolean, on only when a budget was
    // requested. Qwen3 models on *other* OpenAI-compatible upstreams fall
    // through to the reasoningEffort arm below (their APIs accept it).
    if provider_id == "qwen" && thinking_budget.is_some() && !model_id.contains("kimi-k2-thinking")
    {
        options.insert("enable_thinking".to_string(), serde_json::json!(true));
    }

    // Z.AI / Zhipu: GLM models expose thinking via `thinking.type` with
    // `clear_thinking` for preserved thinking across coding turns.
    if provider_id == "zai" || provider_id == "zhipu" {
        let enabled = effort_level != Some(EffortLevel::None);
        options.insert(
            "thinking".to_string(),
            serde_json::json!({
                "type": if enabled { "enabled" } else { "disabled" },
                "clear_thinking": false,
            }),
        );
    }

    // Poolside: binary thinking toggle via chat_template_kwargs. Thinking is
    // on by default and consumes from max_tokens; effort None is the only
    // level that turns it off.
    if provider_id == "poolside" && effort_level == Some(EffortLevel::None) {
        options.insert(
            "chat_template_kwargs".to_string(),
            serde_json::json!({ "enable_thinking": false }),
        );
    }

    // OpenAI reasoning families (GPT-5 / O-series / Qwen3) on OpenAI-
    // compatible providers. Explicit-off maps to "none" so the model thinks
    // at its minimum rather than not at all. DeepSeek models and the qwen
    // provider are handled by their own arms above.
    if provider_id != "qwen"
        && is_openaiish_provider(provider_id)
        && !model_id.starts_with("deepseek")
        && !model_id.contains("/deepseek")
        && openai_compat_reasoning_model(model_id)
    {
        options.insert(
            "reasoningEffort".to_string(),
            serde_json::json!(openai_reasoning_effort_for_level(
                effort_level.unwrap_or(EffortLevel::Medium)
            )),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_effort_matches_query_ladder() {
        assert_eq!(openai_reasoning_effort_for_level(EffortLevel::None), "none");
        assert_eq!(
            openai_reasoning_effort_for_level(EffortLevel::Minimal),
            "minimal"
        );
        assert_eq!(openai_reasoning_effort_for_level(EffortLevel::Low), "low");
        assert_eq!(
            openai_reasoning_effort_for_level(EffortLevel::Medium),
            "medium"
        );
        assert_eq!(openai_reasoning_effort_for_level(EffortLevel::High), "high");
        assert_eq!(openai_reasoning_effort_for_level(EffortLevel::Max), "high");
    }

    #[test]
    fn deepseek_effort_only_has_high_and_max() {
        assert_eq!(
            deepseek_reasoning_effort_for_level(EffortLevel::Medium),
            "high"
        );
        assert_eq!(
            deepseek_reasoning_effort_for_level(EffortLevel::High),
            "high"
        );
        assert_eq!(
            deepseek_reasoning_effort_for_level(EffortLevel::XHigh),
            "max"
        );
        assert_eq!(deepseek_reasoning_effort_for_level(EffortLevel::Max), "max");
        assert_eq!(
            deepseek_reasoning_effort_for_level(EffortLevel::Ultracode),
            "max"
        );
    }

    #[test]
    fn google_level_floors_none_at_low() {
        assert_eq!(
            google_thinking_level_for_effort(Some(EffortLevel::None)),
            "low"
        );
        assert_eq!(
            google_thinking_level_for_effort(Some(EffortLevel::Minimal)),
            "minimal"
        );
        assert_eq!(
            google_thinking_level_for_effort(Some(EffortLevel::Medium)),
            "medium"
        );
        assert_eq!(
            google_thinking_level_for_effort(Some(EffortLevel::Ultracode)),
            "high"
        );
        assert_eq!(google_thinking_level_for_effort(None), "high");
    }

    #[test]
    fn reasoning_model_guards_are_case_and_prefix_tolerant() {
        assert!(openai_reasoning_model("gpt-5-mini"));
        assert!(openai_reasoning_model("o3-2025"));
        assert!(!openai_reasoning_model("gpt-4o"));
        assert!(!openai_reasoning_model("openai/gpt-oss-120b"));
        assert!(openai_compat_reasoning_model("deepseek-v4-flash"));
        assert!(openai_compat_reasoning_model("deepseek/deepseek-v4-flash"));
        assert!(openai_compat_reasoning_model("qwen/qwen3-30b-a3b-fp8"));
        assert!(openai_compat_reasoning_model("gpt-5-codex"));
        assert!(!openai_compat_reasoning_model(
            "meta-llama/Llama-3.3-70B-Instruct"
        ));
        assert!(!openai_compat_reasoning_model("openai/gpt-oss-120b"));
    }

    #[test]
    fn shape_google_25_clamps_budget_below_max_tokens() {
        let mut o = serde_json::Map::new();
        shape_provider_thinking(
            &mut o,
            "google",
            "gemini-2.5-flash",
            Some(EffortLevel::High),
            None,
            Some(32),
        );
        assert_eq!(o["thinkingConfig"]["thinkingBudget"], serde_json::json!(31));
        assert_eq!(
            o["thinkingConfig"]["includeThoughts"],
            serde_json::json!(true)
        );

        // Explicit off maps to a zero budget with thoughts disabled.
        let mut o = serde_json::Map::new();
        shape_provider_thinking(
            &mut o,
            "google",
            "gemini-2.5-flash",
            Some(EffortLevel::None),
            None,
            None,
        );
        assert_eq!(o["thinkingConfig"]["thinkingBudget"], serde_json::json!(0));
        assert_eq!(
            o["thinkingConfig"]["includeThoughts"],
            serde_json::json!(false)
        );
    }

    #[test]
    fn shape_google_3_uses_level_and_floors_off_at_minimal() {
        let mut o = serde_json::Map::new();
        shape_provider_thinking(
            &mut o,
            "google",
            "gemini-3-flash-preview",
            Some(EffortLevel::Medium),
            None,
            None,
        );
        assert_eq!(
            o["thinkingConfig"]["thinkingLevel"],
            serde_json::json!("medium")
        );

        let mut o = serde_json::Map::new();
        shape_provider_thinking(
            &mut o,
            "google",
            "gemini-3-flash-preview",
            Some(EffortLevel::None),
            None,
            None,
        );
        assert_eq!(
            o["thinkingConfig"]["thinkingLevel"],
            serde_json::json!("minimal")
        );
        assert_eq!(
            o["thinkingConfig"]["includeThoughts"],
            serde_json::json!(false)
        );
    }

    #[test]
    fn shape_deepseek_model_any_provider_enabled_max_disabled() {
        // Model-gated: shapes the same on the direct deepseek provider and on
        // free-chain openai-compatible upstreams.
        for provider in ["deepseek", "openrouter"] {
            let mut o = serde_json::Map::new();
            shape_provider_thinking(
                &mut o,
                provider,
                "deepseek/deepseek-v4-flash",
                Some(EffortLevel::High),
                None,
                None,
            );
            assert_eq!(o["thinking"]["type"], serde_json::json!("enabled"));
            assert_eq!(o["reasoningEffort"], serde_json::json!("high"));

            let mut o = serde_json::Map::new();
            shape_provider_thinking(
                &mut o,
                provider,
                "deepseek-v4",
                Some(EffortLevel::Max),
                None,
                None,
            );
            assert_eq!(o["reasoningEffort"], serde_json::json!("max"));

            let mut o = serde_json::Map::new();
            shape_provider_thinking(
                &mut o,
                provider,
                "deepseek-v4",
                Some(EffortLevel::None),
                None,
                None,
            );
            assert_eq!(o["thinking"]["type"], serde_json::json!("disabled"));
            assert!(o.get("reasoningEffort").is_none());
        }
    }

    #[test]
    fn shape_catch_all_gates_reasoning_families_per_provider() {
        // Qwen3 on an openaiish upstream gets reasoningEffort; explicit off
        // maps to "none" (think at minimum, don't error).
        let mut o = serde_json::Map::new();
        shape_provider_thinking(
            &mut o,
            "groq",
            "qwen/qwen3-30b-a3b-fp8",
            Some(EffortLevel::Medium),
            None,
            None,
        );
        assert_eq!(o["reasoningEffort"], serde_json::json!("medium"));

        // The qwen provider itself is handled by enable_thinking, never the
        // reasoningEffort arm — DashScope would reject the unknown param.
        let mut o = serde_json::Map::new();
        shape_provider_thinking(
            &mut o,
            "qwen",
            "qwen3-235b-a22b",
            Some(EffortLevel::Medium),
            Some(5_000),
            None,
        );
        assert_eq!(o["enable_thinking"], serde_json::json!(true));
        assert!(o.get("reasoningEffort").is_none());

        // Non-reasoning models must not receive a parameter their API may
        // reject.
        let mut o = serde_json::Map::new();
        shape_provider_thinking(
            &mut o,
            "sambanova",
            "meta-llama/llama-3.3-70b-instruct",
            Some(EffortLevel::High),
            None,
            None,
        );
        assert!(o.is_empty());
    }

    #[test]
    fn shape_zai_and_poolside_toggles() {
        let mut o = serde_json::Map::new();
        shape_provider_thinking(
            &mut o,
            "zai",
            "glm-4.7-flash",
            Some(EffortLevel::High),
            None,
            None,
        );
        assert_eq!(o["thinking"]["type"], serde_json::json!("enabled"));
        assert_eq!(o["thinking"]["clear_thinking"], serde_json::json!(false));

        let mut o = serde_json::Map::new();
        shape_provider_thinking(
            &mut o,
            "zai",
            "glm-4.7-flash",
            Some(EffortLevel::None),
            None,
            None,
        );
        assert_eq!(o["thinking"]["type"], serde_json::json!("disabled"));

        let mut o = serde_json::Map::new();
        shape_provider_thinking(
            &mut o,
            "poolside",
            "poolside/laguna-s-2.1",
            Some(EffortLevel::None),
            None,
            None,
        );
        assert_eq!(
            o["chat_template_kwargs"]["enable_thinking"],
            serde_json::json!(false)
        );

        // Non-None effort leaves poolside on its default (thinking on).
        let mut o = serde_json::Map::new();
        shape_provider_thinking(
            &mut o,
            "poolside",
            "poolside/laguna-s-2.1",
            Some(EffortLevel::Medium),
            None,
            None,
        );
        assert!(o.is_empty());
    }
}
