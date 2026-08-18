// ConfigTool: get or set Clawde configuration settings at runtime.
//
// Reads from and persists to ~/.clawde/settings.json.
// Supported settings: model, max_tokens, verbose, permission_mode.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

pub struct ConfigTool;

#[derive(Debug, Deserialize)]
struct ConfigInput {
    setting: String,
    value: Option<Value>,
}

static SUPPORTED_SETTINGS: &[(&str, &str)] = &[
    ("model", "LLM model to use (e.g. 'claude-opus-4-6')"),
    ("max_tokens", "Maximum output tokens per response"),
    ("verbose", "Enable verbose logging (true/false)"),
    (
        "permission_mode",
        "Permission mode: default | accept_edits | bypass_permissions | plan",
    ),
    (
        "auto_compact",
        "Auto-compact conversation when context fills (true/false)",
    ),
];

#[async_trait]
impl Tool for ConfigTool {
    fn name(&self) -> &str {
        "Config"
    }

    fn description(&self) -> &str {
        "Get or set Clawde configuration settings. Omit 'value' to read the current value. \
         Supported settings: model, max_tokens, verbose, permission_mode, auto_compact. \
         Changes persist to ~/.clawde/settings.json."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "setting": {
                    "type": "string",
                    "description": "Setting key (e.g. 'model', 'verbose', 'max_tokens', 'permission_mode')"
                },
                "value": {
                    "description": "New value to set. Omit to read the current value."
                }
            },
            "required": ["setting"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        let params: ConfigInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let key = params.setting.trim();

        // List all supported settings
        if key == "list" || key == "help" {
            let lines: Vec<String> = SUPPORTED_SETTINGS
                .iter()
                .map(|(k, d)| format!("  {} — {}", k, d))
                .collect();
            return ToolResult::success(format!("Supported settings:\n{}", lines.join("\n")));
        }

        // Load current settings
        let mut settings = match clawde_core::config::Settings::load().await {
            Ok(s) => s,
            Err(e) => return ToolResult::error(format!("Failed to load settings: {}", e)),
        };

        if let Some(new_value) = params.value {
            // SET operation
            match key {
                "model" => {
                    let s = match new_value.as_str() {
                        Some(s) => s.to_string(),
                        None => return ToolResult::error("'model' must be a string".to_string()),
                    };
                    settings.config.model = Some(s.clone());
                    if let Err(e) = settings.save().await {
                        return ToolResult::error(format!("Failed to save settings: {}", e));
                    }
                    ToolResult::success(format!("model = \"{}\"", s))
                }
                "max_tokens" => {
                    let n = match new_value.as_u64() {
                        Some(n) => n as u32,
                        None => {
                            return ToolResult::error(
                                "'max_tokens' must be a positive integer".to_string(),
                            )
                        }
                    };
                    settings.config.max_tokens = Some(n);
                    if let Err(e) = settings.save().await {
                        return ToolResult::error(format!("Failed to save settings: {}", e));
                    }
                    ToolResult::success(format!("max_tokens = {}", n))
                }
                "verbose" => {
                    let b = match new_value.as_bool() {
                        Some(b) => b,
                        None => {
                            return ToolResult::error("'verbose' must be true or false".to_string())
                        }
                    };
                    settings.config.verbose = b;
                    if let Err(e) = settings.save().await {
                        return ToolResult::error(format!("Failed to save settings: {}", e));
                    }
                    ToolResult::success(format!("verbose = {}", b))
                }
                "auto_compact" => {
                    let b = match new_value.as_bool() {
                        Some(b) => b,
                        None => {
                            return ToolResult::error(
                                "'auto_compact' must be true or false".to_string(),
                            )
                        }
                    };
                    settings.config.auto_compact = b;
                    if let Err(e) = settings.save().await {
                        return ToolResult::error(format!("Failed to save settings: {}", e));
                    }
                    ToolResult::success(format!("auto_compact = {}", b))
                }
                "permission_mode" => {
                    use clawde_core::config::PermissionMode;
                    let s = match new_value.as_str() {
                        Some(s) => s,
                        None => {
                            return ToolResult::error(
                                "'permission_mode' must be a string".to_string(),
                            )
                        }
                    };
                    let mode = match s {
                        "default" => PermissionMode::Default,
                        "accept_edits" | "acceptEdits" => PermissionMode::AcceptEdits,
                        "bypass_permissions" | "bypassPermissions" => {
                            PermissionMode::BypassPermissions
                        }
                        "plan" => PermissionMode::Plan,
                        _ => {
                            return ToolResult::error(format!(
                                "Unknown permission_mode '{}'. Use: default | accept_edits | bypass_permissions | plan",
                                s
                            ))
                        }
                    };
                    settings.config.permission_mode = mode;
                    if let Err(e) = settings.save().await {
                        return ToolResult::error(format!("Failed to save settings: {}", e));
                    }
                    ToolResult::success(format!("permission_mode = \"{}\"", s))
                }
                _ => ToolResult::error(format!(
                    "Unknown setting '{}'. Use setting='list' to see all supported settings.",
                    key
                )),
            }
        } else {
            // GET operation
            match key {
                "model" => ToolResult::success(format!(
                    "model = \"{}\"",
                    settings.config.effective_model()
                )),
                "max_tokens" => ToolResult::success(format!(
                    "max_tokens = {}",
                    settings.config.effective_max_tokens()
                )),
                "verbose" => ToolResult::success(format!("verbose = {}", settings.config.verbose)),
                "auto_compact" => {
                    ToolResult::success(format!("auto_compact = {}", settings.config.auto_compact))
                }
                "permission_mode" => ToolResult::success(format!(
                    "permission_mode = \"{}\"",
                    permission_mode_str(&settings.config.permission_mode)
                )),
                _ => ToolResult::error(format!(
                    "Unknown setting '{}'. Use setting='list' to see all supported settings.",
                    key
                )),
            }
        }
    }
}

fn permission_mode_str(mode: &clawde_core::config::PermissionMode) -> &'static str {
    use clawde_core::config::PermissionMode;
    match mode {
        PermissionMode::Default => "default",
        PermissionMode::AcceptEdits => "accept_edits",
        PermissionMode::BypassPermissions => "bypass_permissions",
        PermissionMode::Plan => "plan",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawde_core::config::PermissionMode;

    #[test]
    fn permission_mode_str_maps_all_modes() {
        assert_eq!(permission_mode_str(&PermissionMode::Default), "default");
        assert_eq!(
            permission_mode_str(&PermissionMode::AcceptEdits),
            "accept_edits"
        );
        assert_eq!(
            permission_mode_str(&PermissionMode::BypassPermissions),
            "bypass_permissions"
        );
        assert_eq!(permission_mode_str(&PermissionMode::Plan), "plan");
    }

    // ---- execute path -----------------------------------------------------

    /// Serialises tests that mutate the process-global `CLAWDE_HOME` env var:
    /// `execute` reads and persists `settings.json` under the config dir.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Run a future with `CLAWDE_HOME` pointed at a fresh temp dir so
    /// settings reads/writes never touch the real config dir (and never race
    /// other env-mutating tests under parallelism).
    #[allow(clippy::await_holding_lock)]
    // The guard must span the whole future: it serialises the CLAWDE_HOME
    // mutation against other env-mutating tests (same std::sync::Mutex
    // convention as crate::paths::ENV_LOCK). Test-only, single acquisition.
    async fn with_temp_home<T>(f: impl FnOnce(std::path::PathBuf) -> T) -> T::Output
    where
        T: std::future::Future,
    {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("CLAWDE_HOME", dir.path());
        let out = f(dir.path().to_path_buf()).await;
        std::env::remove_var("CLAWDE_HOME");
        out
    }

    #[tokio::test]
    async fn list_setting_returns_all_supported_settings() {
        let res = ConfigTool
            .execute(
                json!({ "setting": "list" }),
                &crate::test_support::allow_all_context(".".into()),
            )
            .await;
        assert!(!res.is_error, "{}", res.content);
        for (key, _) in SUPPORTED_SETTINGS {
            assert!(
                res.content.contains(key),
                "missing {} in {}",
                key,
                res.content
            );
        }
    }

    #[tokio::test]
    async fn invalid_input_errors_without_settings() {
        let res = ConfigTool
            .execute(
                json!({ "setting": 42 }),
                &crate::test_support::allow_all_context(".".into()),
            )
            .await;
        assert!(res.is_error);
        assert!(res.content.contains("Invalid input"), "{}", res.content);
    }

    #[tokio::test]
    async fn set_model_persists_and_reads_back() {
        with_temp_home(|home| async move {
            let ctx = crate::test_support::allow_all_context(home.clone());
            let set = ConfigTool
                .execute(
                    json!({ "setting": "model", "value": "my-test-model" }),
                    &ctx,
                )
                .await;
            assert!(!set.is_error, "{}", set.content);
            assert_eq!(set.content, "model = \"my-test-model\"");
            // Persisted to disk under the temp home.
            let saved: Value =
                serde_json::from_str(&std::fs::read_to_string(home.join("settings.json")).unwrap())
                    .unwrap();
            assert_eq!(saved["config"]["model"], "my-test-model");
            // A fresh load sees the new value.
            let get = ConfigTool
                .execute(json!({ "setting": "model" }), &ctx)
                .await;
            assert!(!get.is_error, "{}", get.content);
            assert_eq!(get.content, "model = \"my-test-model\"");
        })
        .await;
    }

    #[tokio::test]
    async fn set_max_tokens_persists_and_reads_back() {
        with_temp_home(|home| async move {
            let ctx = crate::test_support::allow_all_context(home);
            let set = ConfigTool
                .execute(json!({ "setting": "max_tokens", "value": 2048 }), &ctx)
                .await;
            assert!(!set.is_error, "{}", set.content);
            assert_eq!(set.content, "max_tokens = 2048");
            let get = ConfigTool
                .execute(json!({ "setting": "max_tokens" }), &ctx)
                .await;
            assert_eq!(get.content, "max_tokens = 2048");
        })
        .await;
    }

    #[tokio::test]
    async fn set_boolean_settings_round_trip() {
        with_temp_home(|home| async move {
            let ctx = crate::test_support::allow_all_context(home);
            let v = ConfigTool
                .execute(json!({ "setting": "verbose", "value": true }), &ctx)
                .await;
            assert!(!v.is_error, "{}", v.content);
            assert_eq!(v.content, "verbose = true");
            let a = ConfigTool
                .execute(json!({ "setting": "auto_compact", "value": false }), &ctx)
                .await;
            assert!(!a.is_error, "{}", a.content);
            assert_eq!(a.content, "auto_compact = false");
        })
        .await;
    }

    #[tokio::test]
    async fn set_permission_mode_round_trip_and_validates() {
        with_temp_home(|home| async move {
            let ctx = crate::test_support::allow_all_context(home);
            let set = ConfigTool
                .execute(
                    json!({ "setting": "permission_mode", "value": "plan" }),
                    &ctx,
                )
                .await;
            assert!(!set.is_error, "{}", set.content);
            assert_eq!(set.content, "permission_mode = \"plan\"");
            let get = ConfigTool
                .execute(json!({ "setting": "permission_mode" }), &ctx)
                .await;
            assert_eq!(get.content, "permission_mode = \"plan\"");
            // Invalid mode is rejected before any persistence.
            let bad = ConfigTool
                .execute(
                    json!({ "setting": "permission_mode", "value": "nope" }),
                    &ctx,
                )
                .await;
            assert!(bad.is_error);
            assert!(
                bad.content.contains("Unknown permission_mode"),
                "{}",
                bad.content
            );
        })
        .await;
    }

    #[tokio::test]
    async fn wrong_value_types_error() {
        with_temp_home(|home| async move {
            let ctx = crate::test_support::allow_all_context(home);
            for (setting, value, needle) in [
                ("model", json!(42), "'model' must be a string"),
                ("verbose", json!("yes"), "'verbose' must be true or false"),
                (
                    "max_tokens",
                    json!("lots"),
                    "'max_tokens' must be a positive integer",
                ),
            ] {
                let res = ConfigTool
                    .execute(json!({ "setting": setting, "value": value }), &ctx)
                    .await;
                assert!(res.is_error, "{} should error", setting);
                assert!(res.content.contains(needle), "{}: {}", setting, res.content);
            }
        })
        .await;
    }

    #[tokio::test]
    async fn unknown_setting_errors_on_get_and_set() {
        with_temp_home(|home| async move {
            let ctx = crate::test_support::allow_all_context(home);
            let get = ConfigTool
                .execute(json!({ "setting": "bogus" }), &ctx)
                .await;
            assert!(get.is_error);
            assert!(get.content.contains("Unknown setting"), "{}", get.content);
            let set = ConfigTool
                .execute(json!({ "setting": "bogus", "value": 1 }), &ctx)
                .await;
            assert!(set.is_error);
            assert!(set.content.contains("Unknown setting"), "{}", set.content);
        })
        .await;
    }
}
