// providers/openai_compat_providers.rs — Factory functions for all
// OpenAI-compatible provider instances.
//
// Each function constructs a pre-configured [`OpenAiCompatProvider`] for a
// specific service.  API keys are read from environment variables; if the
// variable is absent or empty the provider is still constructed but
// `health_check()` will return `ProviderStatus::Unavailable`.

use clawde_core::config::Settings;
use clawde_core::provider_id::ProviderId;

use super::openai_compat::{OpenAiCompatProvider, ProviderQuirks};

pub fn provider_for_id(provider_id: &str) -> Option<OpenAiCompatProvider> {
    match provider_id {
        "ollama" => Some(ollama()),
        "lmstudio" | "lm-studio" => Some(lm_studio()),
        "llamacpp" | "llama-cpp" | "llama-server" => Some(llama_cpp()),
        "deepseek" => Some(deepseek()),
        "groq" => Some(groq()),
        "xai" => Some(xai()),
        "deepinfra" => Some(deepinfra()),
        "cerebras" => Some(cerebras()),
        "together-ai" => Some(together_ai()),
        "perplexity" => Some(perplexity()),
        "venice" => Some(venice()),
        "qwen" | "alibaba" => Some(qwen()),
        "mistral" => Some(mistral()),
        "openrouter" => Some(openrouter()),
        "sambanova" => Some(sambanova()),
        "huggingface" => Some(huggingface()),
        "nvidia" => Some(nvidia()),
        "siliconflow" => Some(siliconflow()),
        "moonshot" | "moonshotai" => Some(moonshot()),
        "zhipu" | "zhipuai" => Some(zhipu()),
        "zai" => Some(zai()),
        "nebius" => Some(nebius()),
        "novita" => Some(novita()),
        "ovhcloud" => Some(ovhcloud()),
        "scaleway" => Some(scaleway()),
        "vultr" | "vultr-ai" => Some(vultr_ai()),
        "baseten" => Some(baseten()),
        "crof" => Some(crof()),
        "friendli" => Some(friendli()),
        "upstage" => Some(upstage()),
        "stepfun" => Some(stepfun()),
        "fireworks" => Some(fireworks()),
        "opencode-go" => Some(opencode_go()),
        "opencode-zen" => Some(opencode_zen()),
        "synthetic" => Some(synthetic()),
        "routing" => Some(routing()),
        "neuralwatt" => Some(neuralwatt()),
        "cline" => Some(cline()),
        "cloudflare" => Some(cloudflare()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Local / self-hosted providers (no API key required)
// ---------------------------------------------------------------------------

/// Ollama — explicitly configured remote GPU inference server.
/// Resolves the host from settings or `OLLAMA_HOST`; there is no localhost
/// fallback. Both the OpenAI-compatible `/v1` base URL and the native API host
/// (used for health checks and model discovery) are derived from the same
/// resolved host so a configured remote server is honored everywhere.
///
/// When no valid remote host is configured, the provider points at an
/// unroutable address and its native host is `None`, so it cannot silently run
/// inference on a local CPU.
pub fn ollama() -> OpenAiCompatProvider {
    let native_host = clawde_core::config::resolve_ollama_host();
    let base_url = native_host
        .as_deref()
        .map(|host| format!("{}/v1", host))
        .unwrap_or_else(|| "http://0.0.0.0:1/v1".to_string());
    OpenAiCompatProvider::new(ProviderId::OLLAMA, "Ollama", base_url).with_quirks(ProviderQuirks {
        overflow_patterns: vec![
            "prompt too long".to_string(),
            "exceeded.*context length".to_string(),
        ],
        no_api_key_required: true,
        ollama_native_host: native_host,
        ..Default::default()
    })
}

/// LM Studio — local OpenAI-compatible server.
/// Reads `LM_STUDIO_HOST` for the base URL; defaults to `http://localhost:1234`.
pub fn lm_studio() -> OpenAiCompatProvider {
    let host =
        std::env::var("LM_STUDIO_HOST").unwrap_or_else(|_| "http://localhost:1234".to_string());
    let base_url = format!("{}/v1", host.trim_end_matches('/'));
    OpenAiCompatProvider::new(ProviderId::LM_STUDIO, "LM Studio", base_url).with_quirks(
        ProviderQuirks {
            overflow_patterns: vec!["greater than the context length".to_string()],
            no_api_key_required: true,
            ..Default::default()
        },
    )
}

/// llama.cpp — lightweight C++ inference server.
/// Reads `LLAMA_CPP_HOST` for the base URL; defaults to `http://localhost:8080`.
pub fn llama_cpp() -> OpenAiCompatProvider {
    let host =
        std::env::var("LLAMA_CPP_HOST").unwrap_or_else(|_| "http://localhost:8080".to_string());
    let base_url = format!("{}/v1", host.trim_end_matches('/'));
    OpenAiCompatProvider::new(ProviderId::LLAMA_CPP, "llama.cpp", base_url).with_quirks(
        ProviderQuirks {
            overflow_patterns: vec!["exceeds the available context size".to_string()],
            no_api_key_required: true,
            ..Default::default()
        },
    )
}

// ---------------------------------------------------------------------------
// Remote / cloud providers (API key required)
// ---------------------------------------------------------------------------
/// Custom OpenAI-compatible provider supplied by the user.
pub fn custom_openai_with_url(base_url: impl Into<String>) -> OpenAiCompatProvider {
    let key = std::env::var("CUSTOM_OPENAI_API_KEY").unwrap_or_default();

    OpenAiCompatProvider::new("custom-openai", "Custom OpenAI-Compatible", base_url.into())
        .with_api_key(key)
}

/// Custom OpenAI-compatible provider supplied by the user.
pub fn custom_openai() -> OpenAiCompatProvider {
    let settings = Settings::load_sync().unwrap_or_default();
    let base_url = settings
        .providers
        .get("custom-openai")
        .and_then(|config| config.api_base.as_deref())
        .filter(|url| !url.trim().is_empty())
        .unwrap_or("http://127.0.0.1:1/v1");

    custom_openai_with_url(base_url)
}

/// DeepSeek V4 — supports reasoning output via `reasoning_content` field.
/// V4 models require reasoning_content to be echoed back on subsequent turns
/// in multi-turn conversations with tool calls.
/// Reads `DEEPSEEK_API_KEY`.
pub fn deepseek() -> OpenAiCompatProvider {
    let key = std::env::var("DEEPSEEK_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::DEEPSEEK,
        "DeepSeek",
        "https://api.deepseek.com/v1",
    )
    .with_api_key(key)
    .with_quirks(ProviderQuirks {
        reasoning_field: Some("reasoning_content".to_string()),
        requires_reasoning_roundtrip: true,
        overflow_patterns: vec!["maximum context length is".to_string()],
        include_usage_in_stream: true,
        max_tokens_cap: None,
        ..Default::default()
    })
}

/// Groq — fast inference cloud.  Reads `GROQ_API_KEY`.
pub fn groq() -> OpenAiCompatProvider {
    let key = std::env::var("GROQ_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(ProviderId::GROQ, "Groq", "https://api.groq.com/openai/v1")
        .with_api_key(key)
        .with_quirks(ProviderQuirks {
            overflow_patterns: vec!["reduce the length of the messages".to_string()],
            include_usage_in_stream: true,
            max_tokens_cap: Some(512),
            // Groq's free tier limits to 12,000 TPM. The Clawde system prompt
            // is extremely code-dense (AGENTS.md rules + tool definitions +
            // JSON examples), tokenizing at ~1.07 bytes/token on Llama BPE.
            // Set max_total_tokens to 8,500 so the truncation budget of
            // (8,500-512)*1.3 = 10,384 bytes produces at most ~9,705 prompt
            // tokens (10,384/1.07) + 512 output = ~10,217 total — well under
            // the 12,000 limit with 1,783 tokens of headroom.
            max_total_tokens: Some(8_500),
            bytes_per_token: 1.3,
            ..Default::default()
        })
}

/// xAI (Grok).  Reads `XAI_API_KEY`.
pub fn xai() -> OpenAiCompatProvider {
    let key = std::env::var("XAI_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(ProviderId::XAI, "xAI (Grok)", "https://api.x.ai/v1")
        .with_api_key(key)
        .with_quirks(ProviderQuirks {
            overflow_patterns: vec!["maximum prompt length is".to_string()],
            ..Default::default()
        })
}

/// DeepInfra — hosted open-weight models.  Reads `DEEPINFRA_API_KEY`.
pub fn deepinfra() -> OpenAiCompatProvider {
    let key = std::env::var("DEEPINFRA_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::DEEPINFRA,
        "DeepInfra",
        "https://api.deepinfra.com/v1/openai",
    )
    .with_api_key(key)
}

/// Cerebras — wafer-scale inference.  Reads `CEREBRAS_API_KEY`.
pub fn cerebras() -> OpenAiCompatProvider {
    let key = std::env::var("CEREBRAS_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::CEREBRAS,
        "Cerebras",
        "https://api.cerebras.ai/v1",
    )
    .with_api_key(key)
    .with_quirks(ProviderQuirks {
        include_usage_in_stream: true,
        ..Default::default()
    })
}

/// Together AI — hosted open-source models.  Reads `TOGETHER_API_KEY`.
pub fn together_ai() -> OpenAiCompatProvider {
    let key = std::env::var("TOGETHER_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::TOGETHER_AI,
        "Together AI",
        "https://api.together.xyz/v1",
    )
    .with_api_key(key)
    .with_quirks(ProviderQuirks {
        include_usage_in_stream: true,
        ..Default::default()
    })
}

/// Perplexity — search-augmented LLM API.  Reads `PERPLEXITY_API_KEY`.
pub fn perplexity() -> OpenAiCompatProvider {
    let key = std::env::var("PERPLEXITY_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::PERPLEXITY,
        "Perplexity",
        "https://api.perplexity.ai",
    )
    .with_api_key(key)
    .with_quirks(ProviderQuirks {
        include_usage_in_stream: true,
        ..Default::default()
    })
}

/// Venice AI — privacy-focused inference.  Reads `VENICE_API_KEY`.
pub fn venice() -> OpenAiCompatProvider {
    let key = std::env::var("VENICE_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::VENICE,
        "Venice AI",
        "https://api.venice.ai/api/v1",
    )
    .with_api_key(key)
}

/// Qwen / Alibaba DashScope.  Reads `DASHSCOPE_API_KEY`.
/// Uses a default temperature of 0.55 as recommended by Alibaba's docs.
pub fn qwen() -> OpenAiCompatProvider {
    let key = std::env::var("DASHSCOPE_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        "qwen",
        "Qwen (Alibaba)",
        "https://dashscope.aliyuncs.com/compatible-mode/v1",
    )
    .with_api_key(key)
    .with_quirks(ProviderQuirks {
        default_temperature: Some(0.55),
        ..Default::default()
    })
}

/// Mistral AI — Reads `MISTRAL_API_KEY`.
/// Uses OpenAI-compatible format with Mistral-specific quirks:
///   - Tool call IDs must be alphanumeric only, truncated to 9 chars and
///     right-padded with zeroes to exactly 9 chars.
///   - An assistant "Done." turn is inserted between tool→user message transitions.
pub fn mistral() -> OpenAiCompatProvider {
    let key = std::env::var("MISTRAL_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::MISTRAL,
        "Mistral AI",
        "https://api.mistral.ai/v1",
    )
    .with_api_key(key)
    .with_quirks(ProviderQuirks {
        tool_id_max_len: Some(9),
        tool_id_alphanumeric_only: true,
        fix_tool_user_sequence: true,
        include_usage_in_stream: true,
        overflow_patterns: vec!["too large for model with".to_string()],
        ..Default::default()
    })
}

/// OpenRouter — unified API gateway to many models.  Reads `OPENROUTER_API_KEY`.
pub fn openrouter() -> OpenAiCompatProvider {
    let key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::OPENROUTER,
        "OpenRouter",
        "https://openrouter.ai/api/v1",
    )
    .with_api_key(key)
    .with_header("HTTP-Referer", "https://claurst.ai/")
    .with_header("X-Title", "Claurst")
    .with_quirks(ProviderQuirks {
        include_usage_in_stream: true,
        ..Default::default()
    })
}

/// SambaNova — fast inference cloud.  Reads `SAMBANOVA_API_KEY`.
pub fn sambanova() -> OpenAiCompatProvider {
    let key = std::env::var("SAMBANOVA_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::SAMBANOVA,
        "SambaNova",
        "https://api.sambanova.ai/v1",
    )
    .with_api_key(key)
}

/// Hugging Face Inference API.  Reads `HF_TOKEN`.
pub fn huggingface() -> OpenAiCompatProvider {
    let key = std::env::var("HF_TOKEN").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::HUGGINGFACE,
        "Hugging Face",
        "https://router.huggingface.co/v1",
    )
    .with_api_key(key)
}

/// Nvidia NIM — enterprise AI inference.  Reads `NVIDIA_API_KEY`.
pub fn nvidia() -> OpenAiCompatProvider {
    let key = std::env::var("NVIDIA_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::NVIDIA,
        "Nvidia",
        "https://integrate.api.nvidia.com/v1",
    )
    .with_api_key(key)
}

/// SiliconFlow — DeepSeek / Qwen hosting.  Reads `SILICONFLOW_API_KEY`.
pub fn siliconflow() -> OpenAiCompatProvider {
    let key = std::env::var("SILICONFLOW_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::SILICONFLOW,
        "SiliconFlow",
        "https://api.siliconflow.com/v1",
    )
    .with_api_key(key)
}

/// Moonshot AI / Kimi.  Reads `MOONSHOT_API_KEY`.
pub fn moonshot() -> OpenAiCompatProvider {
    let key = std::env::var("MOONSHOT_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::MOONSHOT,
        "Moonshot AI",
        "https://api.moonshot.ai/v1",
    )
    .with_api_key(key)
}

/// Zhipu AI / GLM.  Reads `ZHIPU_API_KEY`.
pub fn zhipu() -> OpenAiCompatProvider {
    let key = std::env::var("ZHIPU_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::ZHIPU,
        "Zhipu AI",
        "https://open.bigmodel.cn/api/paas/v4",
    )
    .with_api_key(key)
}

/// Z.AI (Zhipu) — current-generation GLM models (GLM-5.1, GLM-5, GLM-5-Turbo, GLM-4.7, etc.).
/// Uses the Z.AI international endpoint per docs.z.ai.
/// Reads `ZAI_API_KEY`.
pub fn zai() -> OpenAiCompatProvider {
    let key = std::env::var("ZAI_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::ZAI,
        "Z.AI",
        "https://api.z.ai/api/coding/paas/v4",
    )
    .with_api_key(key)
}

/// Nebius — Llama / Qwen hosting.  Reads `NEBIUS_API_KEY`.
pub fn nebius() -> OpenAiCompatProvider {
    let key = std::env::var("NEBIUS_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::NEBIUS,
        "Nebius",
        "https://api.tokenfactory.nebius.com/v1",
    )
    .with_api_key(key)
}

/// Novita — Llama / Stable Diffusion hosting.  Reads `NOVITA_API_KEY`.
pub fn novita() -> OpenAiCompatProvider {
    let key = std::env::var("NOVITA_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::NOVITA,
        "Novita",
        "https://api.novita.ai/v3/openai",
    )
    .with_api_key(key)
}

/// OVHcloud — EU-hosted AI.  Reads `OVHCLOUD_API_KEY`.
pub fn ovhcloud() -> OpenAiCompatProvider {
    let key = std::env::var("OVHCLOUD_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::OVHCLOUD,
        "OVHcloud",
        "https://oai.endpoints.kepler.ai.cloud.ovh.net/v1",
    )
    .with_api_key(key)
}

/// Scaleway — EU cloud AI.  Reads `SCALEWAY_API_KEY`.
pub fn scaleway() -> OpenAiCompatProvider {
    let key = std::env::var("SCALEWAY_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::SCALEWAY,
        "Scaleway",
        "https://api.scaleway.ai/v1",
    )
    .with_api_key(key)
}

/// Vultr — cloud inference.  Reads `VULTR_API_KEY`.
pub fn vultr_ai() -> OpenAiCompatProvider {
    let key = std::env::var("VULTR_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::VULTR,
        "Vultr",
        "https://api.vultrinference.com/v1",
    )
    .with_api_key(key)
}

/// Baseten — model serving.  Reads `BASETEN_API_KEY`.
pub fn baseten() -> OpenAiCompatProvider {
    let key = std::env::var("BASETEN_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::BASETEN,
        "Baseten",
        "https://inference.baseten.co/v1",
    )
    .with_api_key(key)
}

/// Friendli — serverless inference.  Reads `FRIENDLI_TOKEN`.
pub fn friendli() -> OpenAiCompatProvider {
    let key = std::env::var("FRIENDLI_TOKEN").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::FRIENDLI,
        "Friendli",
        "https://api.friendli.ai/serverless/v1",
    )
    .with_api_key(key)
}

/// Upstage — Solar models.  Reads `UPSTAGE_API_KEY`.
pub fn upstage() -> OpenAiCompatProvider {
    let key = std::env::var("UPSTAGE_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::UPSTAGE,
        "Upstage",
        "https://api.upstage.ai/v1/solar",
    )
    .with_api_key(key)
}

/// StepFun — Step models.  Reads `STEPFUN_API_KEY`.
pub fn stepfun() -> OpenAiCompatProvider {
    let key = std::env::var("STEPFUN_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(ProviderId::STEPFUN, "StepFun", "https://api.stepfun.com/v1")
        .with_api_key(key)
}

/// Fireworks AI — fast inference.  Reads `FIREWORKS_API_KEY`.
pub fn fireworks() -> OpenAiCompatProvider {
    let key = std::env::var("FIREWORKS_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::FIREWORKS,
        "Fireworks AI",
        "https://api.fireworks.ai/inference/v1",
    )
    .with_api_key(key)
}

/// OpenCode Go — flat-rate subscription endpoint hosted by opencode.ai.
/// OpenAI-compatible chat completions surface; same key works for the Zen
/// metered tier, hence the shared `OPENCODE_API_KEY` env var.
pub fn opencode_go() -> OpenAiCompatProvider {
    let key = std::env::var("OPENCODE_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::OPENCODE_GO,
        "OpenCode Go",
        "https://opencode.ai/zen/go/v1",
    )
    .with_api_key(key)
    .with_quirks(ProviderQuirks {
        include_usage_in_stream: true,
        ..Default::default()
    })
}

/// OpenCode Zen — pay-as-you-go metered endpoint hosted by opencode.ai.
/// Exposes the free pool (Big Pickle, MiniMax M2.5 Free, Ring 2.6 1T Free,
/// Nemotron 3 Super Free) alongside paid models.  Same `OPENCODE_API_KEY` as
/// OpenCode Go.
pub fn opencode_zen() -> OpenAiCompatProvider {
    let key = std::env::var("OPENCODE_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::OPENCODE_ZEN,
        "OpenCode Zen",
        "https://opencode.ai/zen/v1",
    )
    .with_api_key(key)
    .with_quirks(ProviderQuirks {
        include_usage_in_stream: true,
        ..Default::default()
    })
}

/// Crof.ai — OpenAI-compatible endpoint for model routing.
/// Reads `CROF_API_KEY` for authentication.
pub fn crof() -> OpenAiCompatProvider {
    let key = std::env::var("CROF_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(ProviderId::CROF, "Crof.ai", "https://api.crof.ai/v1")
        .with_api_key(key)
        .with_quirks(ProviderQuirks {
            include_usage_in_stream: true,
            ..Default::default()
        })
}

/// Synthetic.dev — OpenAI-compatible endpoint with curated model selection.
/// Reads `SYNTHETIC_API_KEY` for authentication.
pub fn synthetic() -> OpenAiCompatProvider {
    let key = std::env::var("SYNTHETIC_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::SYNTHETIC,
        "Synthetic.dev",
        "https://api.synthetic.new/openai/v1",
    )
    .with_api_key(key)
    .with_quirks(ProviderQuirks {
        include_usage_in_stream: true,
        ..Default::default()
    })
}

/// routing.run — OpenAI-compatible endpoint for model routing.
/// Reads `ROUTING_API_KEY` for authentication.
pub fn routing() -> OpenAiCompatProvider {
    let key = std::env::var("ROUTING_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::ROUTING,
        "routing.run",
        "https://api.routing.run/v1",
    )
    .with_api_key(key)
    .with_quirks(ProviderQuirks {
        include_usage_in_stream: true,
        ..Default::default()
    })
}

/// Cloudflare Workers AI — free tier (10,000 neurons/day, no credit card).
///
/// The OpenAI-compatible endpoint embeds the Cloudflare account ID in the
/// URL path (`/accounts/{ACCOUNT_ID}/ai/v1`), so a usable credential is the
/// composite `ACCOUNT_ID:API_TOKEN` string. The account ID can alternatively
/// be supplied via the `CLOUDFLARE_ACCOUNT_ID` env var, in which case
/// `CLOUDFLARE_API_TOKEN` holds only the API token.
///
/// `cloudflare_with_key()` is the canonical constructor for the free-mode
/// chain, where keys are stored as single strings in the auth store.
pub fn cloudflare() -> OpenAiCompatProvider {
    // Env-var path: two dedicated Cloudflare tooling vars.
    let account = std::env::var("CLOUDFLARE_ACCOUNT_ID").unwrap_or_default();
    let token = std::env::var("CLOUDFLARE_API_TOKEN").unwrap_or_default();
    if !account.trim().is_empty() && !token.trim().is_empty() {
        return cloudflare_with_parts(&account, &token);
    }
    // Composite single-key path (auth store / free chain):
    // `CLOUDFLARE_API_TOKEN` carries `ACCOUNT_ID:API_TOKEN`.
    match cloudflare_parts(&token) {
        Some((a, t)) => cloudflare_with_parts(a, t),
        None => cloudflare_with_parts(&account, &token),
    }
}

/// Build a Cloudflare provider from a composite `ACCOUNT_ID:API_TOKEN` key
/// (the format the free-mode dialog and `/keys` store). Falls back to the
/// `CLOUDFLARE_ACCOUNT_ID` env var when the key has no `:` separator.
pub fn cloudflare_with_key(key: &str) -> OpenAiCompatProvider {
    match cloudflare_parts(key) {
        Some((a, t)) => cloudflare_with_parts(a, t),
        None => {
            let account = std::env::var("CLOUDFLARE_ACCOUNT_ID").unwrap_or_default();
            cloudflare_with_parts(&account, key)
        }
    }
}

/// Split a Cloudflare credential into `(account_id, api_token)`.
/// `pub(crate)` so the free-mode chain (`free.rs`) can reuse the same
/// parsing for key validation probes.
pub(crate) fn cloudflare_parts(key: &str) -> Option<(&str, &str)> {
    let (account, token) = key.trim().split_once(':')?;
    if account.is_empty() || token.is_empty() {
        return None;
    }
    Some((account, token))
}

/// Construct the Cloudflare OpenAI-compat provider from explicit parts.
fn cloudflare_with_parts(account_id: &str, api_token: &str) -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(
        ProviderId::CLOUDFLARE,
        "Cloudflare Workers AI",
        format!(
            "https://api.cloudflare.com/client/v4/accounts/{}/ai/v1",
            account_id.trim()
        ),
    )
    .with_api_key(api_token.to_string())
    .with_quirks(ProviderQuirks {
        include_usage_in_stream: true,
        max_tokens_cap: Some(8_192),
        // Workers AI rejects OpenAI-style content arrays in multi-turn
        // conversations (`/messages/0/content` must be a string).
        string_content_only: true,
        ..Default::default()
    })
}

/// Cline (cline.bot) — OpenAI-compatible proxy with frequently rotating
/// free model selection. Provides unified access to various models via
/// a single API key. Free tier models rotate regularly.
/// Reads `CLINE_API_KEY` for authentication.
pub fn cline() -> OpenAiCompatProvider {
    let key = std::env::var("CLINE_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(ProviderId::CLINE, "Cline", "https://api.cline.bot/api/v1")
        .with_api_key(key)
        .with_quirks(ProviderQuirks {
            include_usage_in_stream: true,
            ..Default::default()
        })
}

/// NeuralWatt — OpenAI-compatible endpoint for fast inference.
/// Reads `NEURALWATT_API_KEY` for authentication.
pub fn neuralwatt() -> OpenAiCompatProvider {
    let key = std::env::var("NEURALWATT_API_KEY").unwrap_or_default();
    OpenAiCompatProvider::new(
        ProviderId::NEURALWATT,
        "NeuralWatt",
        "https://api.neuralwatt.com/v1",
    )
    .with_api_key(key)
    .with_quirks(ProviderQuirks {
        include_usage_in_stream: true,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::LlmProvider;

    #[test]
    fn groq_factory_sets_bytes_per_token() {
        let p = groq();
        assert_eq!(
            p.max_tokens_cap_for(""),
            Some(512),
            "expected Groq max_tokens_cap=512"
        );
        // Verify bytes_per_token is set to 1.3 (code-heavy ratio, tuned for
        // ~1.3 bytes/token density of the Clawde system prompt on Llama BPE).
        assert!(
            (p.quirks.bytes_per_token - 1.3).abs() < f64::EPSILON,
            "expected Groq bytes_per_token=1.3, got {}",
            p.quirks.bytes_per_token
        );
    }

    #[test]
    fn cloudflare_sets_string_content_only() {
        // Cloudflare Workers AI rejects OpenAI-style content arrays in
        // multi-turn conversations; the factory must opt in to flattening.
        let p = cloudflare_with_key("test-account:test-token");
        assert!(
            p.quirks.string_content_only,
            "expected cloudflare to flatten content arrays"
        );
        assert_eq!(p.quirks.max_tokens_cap, Some(8_192));
    }

    #[test]
    fn alibaba_resolves_to_qwen_backend() {
        // "alibaba" is an alias for "qwen" — Alibaba's DashScope is the
        // OpenAI-compatible backend behind both ids.
        let alibaba = provider_for_id("alibaba").expect("alibaba should resolve");
        let qwen = provider_for_id("qwen").expect("qwen should resolve");
        assert_eq!(alibaba.id(), qwen.id());
        assert_eq!(alibaba.name(), qwen.name());
    }
}
