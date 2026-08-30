// Katban configuration: a small `katban.json` under Clawde's config dir.
//
// v0 holds the hosted-site list. Board state and auth secrets arrive in later
// slices; secrets will be encrypted at rest per the spec (auth.json).

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::env;
use std::path::{Path, PathBuf};

pub const CONFIG_VERSION: u32 = 1;
/// Default port for hosted sites (127.0.0.1 only in v0).
pub const DEFAULT_SITE_PORT: u16 = 8788;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct KatbanConfig {
    pub version: u32,
    #[serde(default)]
    pub sites: Vec<SiteConfig>,
}

impl Default for KatbanConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            sites: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SiteConfig {
    pub name: String,
    pub root: PathBuf,
    #[serde(default = "default_site_port")]
    pub port: u16,
    /// Subdomain for a public site (e.g. `myproject.example.com`).
    /// Required before the site can be exposed through caddy.
    #[serde(default)]
    pub public_subdomain: Option<String>,
    /// Publish/lock: when true the site is a stable snapshot — served without
    /// the live-reload script and cacheable. When false, live reload.
    #[serde(default)]
    pub locked: bool,
}

fn default_site_port() -> u16 {
    DEFAULT_SITE_PORT
}

/// Clawde's config dir: `$CLAWDE_HOME` if set, else `~/.clawde` (Windows:
/// `%USERPROFILE%\.clawde`). Mirrors the convention used by the rest of Clawde.
pub fn clawde_home() -> PathBuf {
    if let Ok(dir) = env::var("CLAWDE_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    #[cfg(windows)]
    {
        if let Ok(profile) = env::var("USERPROFILE") {
            return PathBuf::from(profile).join(".clawde");
        }
    }
    #[cfg(not(windows))]
    {
        if let Ok(home) = env::var("HOME") {
            return PathBuf::from(home).join(".clawde");
        }
    }
    PathBuf::from(".clawde")
}

pub fn katban_data_dir() -> PathBuf {
    clawde_home().join("katban")
}

pub fn config_path() -> PathBuf {
    katban_data_dir().join("katban.json")
}

pub fn load() -> anyhow::Result<KatbanConfig> {
    let path = config_path();
    if !path.exists() {
        return Ok(KatbanConfig::default());
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let config: KatbanConfig =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    Ok(config)
}

/// Persist the config atomically (write temp, rename) so a crash never leaves
/// a half-written file — same discipline the spec requires for `katban.conf`.
pub fn save(config: &KatbanConfig) -> anyhow::Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(config)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn canonical_site_root(root: &Path) -> anyhow::Result<PathBuf> {
    let canon = root
        .canonicalize()
        .with_context(|| format!("site root does not exist: {}", root.display()))?;
    if !canon.is_dir() {
        anyhow::bail!("site root is not a directory: {}", canon.display());
    }
    Ok(canon)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_home<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = env::var("CLAWDE_HOME").ok();
        env::set_var("CLAWDE_HOME", dir);
        let result = f();
        match previous {
            Some(value) => env::set_var("CLAWDE_HOME", value),
            None => env::remove_var("CLAWDE_HOME"),
        }
        result
    }

    #[test]
    fn data_dir_joins_katban_under_clawde_home() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            assert_eq!(katban_data_dir(), tmp.path().join("katban"));
        });
    }

    #[test]
    fn default_config_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let config = load().unwrap();
            assert_eq!(config, KatbanConfig::default());
            assert!(config.sites.is_empty());
        });
    }

    #[test]
    fn save_and_load_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let config = KatbanConfig {
                version: CONFIG_VERSION,
                sites: vec![SiteConfig {
                    name: "demo".to_string(),
                    root: PathBuf::from("/tmp/demo-site"),
                    port: DEFAULT_SITE_PORT,
                    public_subdomain: None,
                    locked: false,
                }],
            };
            save(&config).unwrap();
            assert!(config_path().exists());
            let loaded = load().unwrap();
            assert_eq!(loaded, config);
        });
    }
}
