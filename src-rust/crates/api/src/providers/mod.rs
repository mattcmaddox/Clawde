pub mod anthropic;
pub use anthropic::AnthropicProvider;

pub(crate) mod message_normalization;
pub(crate) mod request_options;

pub mod openai;
pub use openai::OpenAiProvider;

pub mod google;
pub use google::GoogleProvider;

pub mod minimax;
pub use minimax::MinimaxProvider;

pub mod openai_compat;
pub use openai_compat::OpenAiCompatProvider;

pub mod openai_compat_providers;
pub use openai_compat_providers::{
    baseten, cerebras, cline, deepinfra, deepseek, fireworks, friendli, groq, huggingface,
    llama_cpp, lm_studio, mistral, moonshot, nebius, novita, nvidia, ollama, opencode_zen,
    openrouter, ovhcloud, perplexity, qwen, sambanova, scaleway, siliconflow, stepfun, together_ai,
    upstage, venice, vultr_ai, xai, zai, zhipu,
};

pub mod metadata;
pub use metadata::{env_var_for, key_url_for, provider_metadata, MetaLookup, ProviderMetadata};

pub mod free;
pub use free::{
    catalog_entry, CircuitBreakerConfig, FreeEntry, FreeProvider, FreeUpstream, LatencyConfig,
    RoutingConfig, RoutingStrategy, FREE_CATALOG,
};

pub mod cohere;
pub use cohere::CohereProvider;

pub mod azure;
pub use azure::AzureProvider;

pub mod bedrock;
pub use bedrock::BedrockProvider;

pub mod copilot;
pub use copilot::CopilotProvider;

pub mod codex;
pub use codex::CodexProvider;

pub mod key_rotating;
pub use key_rotating::KeyRotatingProvider;
