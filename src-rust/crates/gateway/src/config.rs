//! Gateway configuration — thin wrapper over `clawde_core::config::GatewayConfig`
//! plus CLI overrides.

use std::path::PathBuf;

use clawde_core::config::GatewayConfig;

use crate::tool_exec::GatewayPermissionMode;

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
    /// Server-side agent loop enabled regardless of client `max_tool_calls`.
    pub agent_mode: bool,
    /// Default agent-loop tool-call cap (client `max_tool_calls` overrides).
    pub max_tool_calls: u32,
    /// Tool permission posture for the agent loop.
    pub permission_mode: GatewayPermissionMode,
    /// Workspace paths for the built-in tools; `[0]` is the working dir.
    pub workspace_paths: Vec<PathBuf>,
    /// Replacement list for the default built-in tool surface.
    pub builtin_tools: Vec<String>,
}

impl EffectiveGatewayConfig {
    /// Build from settings + CLI overrides. Fails when `permissionMode` is not
    /// one of `allow-readonly` | `allow` | `deny`.
    pub fn from_settings(base: &GatewayConfig, cli_key: Option<String>) -> Result<Self, String> {
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
        let permission_mode = parse_permission_mode(&base.permission_mode)?;
        Ok(Self {
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
            agent_mode: base.agent_mode,
            max_tool_calls: base.max_tool_calls.max(1),
            permission_mode,
            workspace_paths: base.workspace_paths.clone(),
            builtin_tools: base.builtin_tools.clone(),
        })
    }
}

impl Default for EffectiveGatewayConfig {
    fn default() -> Self {
        Self::from_settings(&GatewayConfig::default(), None)
            .expect("default gateway config is valid")
    }
}

/// Parse the `permissionMode` settings string into the executor posture.
fn parse_permission_mode(value: &str) -> Result<GatewayPermissionMode, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "allow-readonly" | "allow_readonly" | "readonly" => {
            Ok(GatewayPermissionMode::AllowReadonly)
        }
        "allow" => Ok(GatewayPermissionMode::Allow),
        "deny" => Ok(GatewayPermissionMode::Deny),
        other => Err(format!(
            "invalid gateway.permissionMode '{other}' (expected 'allow-readonly', 'allow', or 'deny')"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_permission_modes() {
        for (s, want) in [
            ("allow-readonly", GatewayPermissionMode::AllowReadonly),
            ("allow_readonly", GatewayPermissionMode::AllowReadonly),
            ("allow", GatewayPermissionMode::Allow),
            ("deny", GatewayPermissionMode::Deny),
        ] {
            let cfg = GatewayConfig {
                permission_mode: s.to_string(),
                ..GatewayConfig::default()
            };
            let eff = EffectiveGatewayConfig::from_settings(&cfg, None).unwrap();
            assert_eq!(eff.permission_mode, want, "for {s}");
        }
    }

    #[test]
    fn rejects_invalid_permission_mode() {
        let cfg = GatewayConfig {
            permission_mode: "everything".to_string(),
            ..GatewayConfig::default()
        };
        assert!(EffectiveGatewayConfig::from_settings(&cfg, None).is_err());
    }

    #[test]
    fn client_cap_defaults_from_settings() {
        let cfg = GatewayConfig {
            max_tool_calls: 5,
            agent_mode: true,
            ..GatewayConfig::default()
        };
        let eff = EffectiveGatewayConfig::from_settings(&cfg, None).unwrap();
        assert_eq!(eff.max_tool_calls, 5);
        assert!(eff.agent_mode);
    }
}
