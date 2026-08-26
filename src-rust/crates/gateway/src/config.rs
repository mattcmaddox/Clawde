//! Gateway configuration — thin wrapper over `clawde_core::config::GatewayConfig`
//! plus CLI overrides.

use clawde_core::config::GatewayConfig;

/// Effective gateway config: settings base + CLI overrides.
///
/// `key` can come from `CLAWDE_GATEWAY_KEY` env or `--key` CLI flag; the
/// settings `allowed_keys` list is merged in.
#[derive(Debug, Clone)]
pub struct EffectiveGatewayConfig {
    pub listen: String,
    pub allow_non_loopback: bool,
    pub tls_cert_path: Option<String>,
    pub tls_key_path: Option<String>,
    pub allowed_keys: Vec<String>,
    pub rpm: u32,
    pub tpm: u32,
    pub max_in_flight_per_upstream: usize,
    pub request_timeout_secs: u64,
    pub discovery_refresh_secs: u64,
    pub shutdown_grace_secs: u64,
}

impl EffectiveGatewayConfig {
    /// Build from settings + CLI overrides.
    pub fn from_settings(base: &GatewayConfig, cli_key: Option<String>) -> Self {
        let mut allowed_keys = base.allowed_keys.clone();
        if let Some(key) = cli_key {
            if !allowed_keys.contains(&key) {
                allowed_keys.push(key);
            }
        }
        // Environment override is a single key.
        if let Ok(env_key) = std::env::var("CLAWDE_GATEWAY_KEY") {
            if !env_key.is_empty() && !allowed_keys.contains(&env_key) {
                allowed_keys.push(env_key);
            }
        }
        Self {
            listen: base.listen.clone(),
            allow_non_loopback: base.allow_non_loopback,
            tls_cert_path: base.tls_cert_path.clone(),
            tls_key_path: base.tls_key_path.clone(),
            allowed_keys,
            rpm: base.rpm,
            tpm: base.tpm,
            max_in_flight_per_upstream: base.max_in_flight_per_upstream,
            request_timeout_secs: base.request_timeout_secs,
            discovery_refresh_secs: base.discovery_refresh_secs,
            shutdown_grace_secs: base.shutdown_grace_secs,
        }
    }
}

impl Default for EffectiveGatewayConfig {
    fn default() -> Self {
        Self::from_settings(&GatewayConfig::default(), None)
    }
}
