// providers/metadata.rs — Shared provider metadata for the TUI connect dialog.
//
// Centralises the env-var name and key-URL for every provider so that both the
// FREE_CATALOG (free.rs) and the provider picker (app.rs) use the same data.
//
// To add a new provider: add a row to `ALL_PROVIDERS` with its ID, env var,
// and key URL. The title, description, and category live in `app.rs` because
// they are TUI-specific presentation strings.

/// Metadata for a single AI provider.
#[derive(Debug, Clone, Copy)]
pub struct ProviderMetadata {
    /// Canonical provider ID (matches `ProviderId` convention).
    pub id: &'static str,
    /// Environment variable name that holds the API key (e.g. `"ANTHROPIC_API_KEY"`).
    pub env_var: &'static str,
    /// Human-readable URL where a user can obtain an API key.
    pub key_url: &'static str,
}

/// Look up metadata by provider ID. Returns `MissingProvider` when the ID is
/// not found, so callers can provide a fallback message.
#[derive(Debug, Clone)]
pub enum MetaLookup {
    Meta(&'static ProviderMetadata),
    MissingProvider,
}

/// Look up provider metadata by ID.
pub fn provider_metadata(id: &str) -> MetaLookup {
    ALL_PROVIDERS
        .iter()
        .find(|m| m.id == id)
        .map(MetaLookup::Meta)
        .unwrap_or(MetaLookup::MissingProvider)
}

/// Return the environment variable name for a given provider ID.
/// Falls back to `"API_KEY"` for unknown providers.
pub fn env_var_for(id: &str) -> &'static str {
    match provider_metadata(id) {
        MetaLookup::Meta(m) => m.env_var,
        MetaLookup::MissingProvider => "API_KEY",
    }
}

/// Return a URL hint for obtaining an API key for a given provider ID.
/// Falls back to `"the provider's website"` for unknown providers.
pub fn key_url_for(id: &str) -> &'static str {
    match provider_metadata(id) {
        MetaLookup::Meta(m) => m.key_url,
        MetaLookup::MissingProvider => "the provider's website",
    }
}

/// Complete list of all known providers with their env-var names and key URLs.
/// Sorted alphabetically by ID for easy scanning.
pub const ALL_PROVIDERS: &[ProviderMetadata] = &[
    ProviderMetadata {
        id: "alibaba",
        env_var: "DASHSCOPE_API_KEY",
        key_url: "the provider's website",
    },
    ProviderMetadata {
        id: "amazon-bedrock",
        env_var: "AWS_ACCESS_KEY_ID",
        key_url: "console.aws.amazon.com/bedrock",
    },
    ProviderMetadata {
        id: "anthropic",
        env_var: "ANTHROPIC_API_KEY",
        key_url: "console.anthropic.com",
    },
    ProviderMetadata {
        id: "azure",
        env_var: "AZURE_API_KEY",
        key_url: "portal.azure.com",
    },
    ProviderMetadata {
        id: "baseten",
        env_var: "BASETEN_API_KEY",
        key_url: "the provider's website",
    },
    ProviderMetadata {
        id: "cerebras",
        env_var: "CEREBRAS_API_KEY",
        key_url: "cloud.cerebras.ai",
    },
    ProviderMetadata {
        id: "cline",
        env_var: "CLINE_API_KEY",
        key_url: "app.cline.bot/settings",
    },
    ProviderMetadata {
        id: "cloudflare",
        env_var: "CLOUDFLARE_API_TOKEN",
        key_url: "dash.cloudflare.com",
    },
    ProviderMetadata {
        id: "cloudflare-ai-gateway",
        env_var: "CLOUDFLARE_API_TOKEN",
        key_url: "the provider's website",
    },
    ProviderMetadata {
        id: "cloudflare-workers-ai",
        env_var: "CLOUDFLARE_API_TOKEN",
        key_url: "the provider's website",
    },
    ProviderMetadata {
        id: "cohere",
        env_var: "COHERE_API_KEY",
        key_url: "dashboard.cohere.com/api-keys",
    },
    ProviderMetadata {
        id: "deepinfra",
        env_var: "DEEPINFRA_API_KEY",
        key_url: "deepinfra.com/dash/api_keys",
    },
    ProviderMetadata {
        id: "deepseek",
        env_var: "DEEPSEEK_API_KEY",
        key_url: "platform.deepseek.com/api_keys",
    },
    ProviderMetadata {
        id: "fireworks",
        env_var: "FIREWORKS_API_KEY",
        key_url: "the provider's website",
    },
    ProviderMetadata {
        id: "friendli",
        env_var: "FRIENDLI_TOKEN",
        key_url: "the provider's website",
    },
    ProviderMetadata {
        id: "github-copilot",
        env_var: "GITHUB_TOKEN",
        key_url: "github.com/settings/tokens",
    },
    ProviderMetadata {
        id: "gitlab",
        env_var: "GITLAB_TOKEN",
        key_url: "the provider's website",
    },
    ProviderMetadata {
        id: "google",
        env_var: "GOOGLE_API_KEY",
        key_url: "aistudio.google.com/apikey",
    },
    ProviderMetadata {
        id: "google-vertex",
        env_var: "GOOGLE_API_KEY",
        key_url: "the provider's website",
    },
    ProviderMetadata {
        id: "groq",
        env_var: "GROQ_API_KEY",
        key_url: "console.groq.com/keys",
    },
    ProviderMetadata {
        id: "helicone",
        env_var: "HELICONE_API_KEY",
        key_url: "the provider's website",
    },
    ProviderMetadata {
        id: "huggingface",
        env_var: "HF_TOKEN",
        key_url: "huggingface.co/settings/tokens",
    },
    ProviderMetadata {
        id: "minimax",
        env_var: "MINIMAX_API_KEY",
        key_url: "platform.minimaxi.com",
    },
    ProviderMetadata {
        id: "mistral",
        env_var: "MISTRAL_API_KEY",
        key_url: "console.mistral.ai/api-keys",
    },
    ProviderMetadata {
        id: "moonshotai",
        env_var: "MOONSHOT_API_KEY",
        key_url: "the provider's website",
    },
    ProviderMetadata {
        id: "nebius",
        env_var: "NEBIUS_API_KEY",
        key_url: "the provider's website",
    },
    ProviderMetadata {
        id: "novita",
        env_var: "NOVITA_API_KEY",
        key_url: "the provider's website",
    },
    ProviderMetadata {
        id: "nvidia",
        env_var: "NVIDIA_API_KEY",
        key_url: "build.nvidia.com",
    },
    ProviderMetadata {
        id: "openai",
        env_var: "OPENAI_API_KEY",
        key_url: "platform.openai.com/api-keys",
    },
    ProviderMetadata {
        id: "opencode-zen",
        env_var: "OPENCODE_API_KEY",
        key_url: "opencode.ai/auth",
    },
    ProviderMetadata {
        id: "opencode-go",
        env_var: "OPENCODE_API_KEY",
        key_url: "opencode.ai/auth",
    },
    ProviderMetadata {
        id: "openrouter",
        env_var: "OPENROUTER_API_KEY",
        key_url: "openrouter.ai/keys",
    },
    ProviderMetadata {
        id: "ovhcloud",
        env_var: "OVHCLOUD_API_KEY",
        key_url: "the provider's website",
    },
    ProviderMetadata {
        id: "perplexity",
        env_var: "PERPLEXITY_API_KEY",
        key_url: "perplexity.ai/settings/api",
    },
    ProviderMetadata {
        id: "sambanova",
        env_var: "SAMBANOVA_API_KEY",
        key_url: "cloud.sambanova.ai",
    },
    ProviderMetadata {
        id: "sap-ai-core",
        env_var: "AICORE_SERVICE_KEY",
        key_url: "the provider's website",
    },
    ProviderMetadata {
        id: "scaleway",
        env_var: "SCALEWAY_API_KEY",
        key_url: "the provider's website",
    },
    ProviderMetadata {
        id: "siliconflow",
        env_var: "SILICONFLOW_API_KEY",
        key_url: "the provider's website",
    },
    ProviderMetadata {
        id: "stepfun",
        env_var: "STEPFUN_API_KEY",
        key_url: "the provider's website",
    },
    ProviderMetadata {
        id: "togetherai",
        env_var: "TOGETHER_API_KEY",
        key_url: "api.together.xyz/settings/api-keys",
    },
    ProviderMetadata {
        id: "upstage",
        env_var: "UPSTAGE_API_KEY",
        key_url: "the provider's website",
    },
    ProviderMetadata {
        id: "venice",
        env_var: "VENICE_API_KEY",
        key_url: "venice.ai/settings/api",
    },
    ProviderMetadata {
        id: "vercel",
        env_var: "AI_GATEWAY_API_KEY",
        key_url: "the provider's website",
    },
    ProviderMetadata {
        id: "vultr",
        env_var: "VULTR_API_KEY",
        key_url: "the provider's website",
    },
    ProviderMetadata {
        id: "xai",
        env_var: "XAI_API_KEY",
        key_url: "console.x.ai",
    },
    ProviderMetadata {
        id: "zai",
        env_var: "ZAI_API_KEY",
        key_url: "z.ai/manage-apikey/apikey-list",
    },
    ProviderMetadata {
        id: "zhipuai",
        env_var: "ZHIPU_API_KEY",
        key_url: "the provider's website",
    },
];
