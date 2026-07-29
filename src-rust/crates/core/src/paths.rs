//! Canonical filesystem locations for claurst.
//!
//! Everything claurst persists lives under a single root directory. This module
//! exposes the one resolver ([`clawde_home`]) that the whole workspace routes
//! through, so the home-dir precedence (see [`crate::config::Settings::config_dir`])
//! is defined in exactly one place.

use std::path::PathBuf;

/// The canonical claurst home directory — the single source of truth for where
/// claurst keeps its data. Thin wrapper over
/// [`crate::config::Settings::config_dir`]; prefer this at call sites that only
/// need the root path.
///
/// Resolution precedence (issue #207 — XDG support, back-compatible):
/// 1. `$CLAWDE_HOME` if set and non-empty (verbatim).
/// 2. Legacy `~/.clawde` if it already exists.
/// 3. `$XDG_CONFIG_HOME/clawde` (when absolute) else `~/.config/clawde`.
pub fn clawde_home() -> PathBuf {
    crate::config::Settings::config_dir()
}

// These tests drive the resolver through `HOME`/`XDG_CONFIG_HOME`, which only
// govern `dirs::home_dir()` on Unix — on Windows the home dir comes from the OS
// profile API and can't be pinned via env, so they'd be non-hermetic there.
#[cfg(all(test, unix))]
mod tests {
    use crate::config::Settings;
    use std::path::PathBuf;
    use std::sync::Mutex;

    // The resolver reads process-global env (`CLAWDE_HOME`, `HOME`,
    // `XDG_CONFIG_HOME`). Serialize every test that mutates them.
    pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvGuard {
        fn new() -> Self {
            let keys = ["CLAWDE_HOME", "HOME", "XDG_CONFIG_HOME"];
            let saved = keys
                .iter()
                .map(|k| (*k, std::env::var_os(k)))
                .collect::<Vec<_>>();
            for k in keys {
                std::env::remove_var(k);
            }
            Self { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    #[test]
    fn clawde_home_env_override_wins() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        // Set HOME + an existing legacy dir + XDG too, to prove the override
        // takes precedence over every other rule and is used verbatim.
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".clawde")).unwrap();
        std::env::set_var("HOME", home.path());
        std::env::set_var("XDG_CONFIG_HOME", home.path());
        std::env::set_var("CLAWDE_HOME", tmp.path());

        assert_eq!(Settings::config_dir(), tmp.path());
    }

    #[test]
    fn clawde_home_empty_env_override_ignored() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        std::env::set_var("CLAWDE_HOME", "");

        // Empty override falls through to XDG (no legacy dir, no XDG_CONFIG_HOME).
        assert_eq!(
            Settings::config_dir(),
            home.path().join(".config").join("clawde")
        );
    }

    #[test]
    fn clawde_home_legacy_dir_used_when_present() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        let legacy = home.path().join(".clawde");
        std::fs::create_dir_all(&legacy).unwrap();
        std::env::set_var("HOME", home.path());
        // XDG set, but legacy already exists → legacy wins (back-compat).
        std::env::set_var("XDG_CONFIG_HOME", home.path().join("xdg"));

        assert_eq!(Settings::config_dir(), legacy);
    }

    #[test]
    fn clawde_home_xdg_used_when_set_and_no_legacy() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        let xdg = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        std::env::set_var("XDG_CONFIG_HOME", xdg.path());

        assert_eq!(Settings::config_dir(), xdg.path().join("clawde"));
    }

    #[test]
    fn clawde_home_xdg_default_when_no_env() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());

        // No CLAWDE_HOME, no legacy dir, no XDG_CONFIG_HOME → ~/.config/clawde.
        assert_eq!(
            Settings::config_dir(),
            home.path().join(".config").join("clawde")
        );
    }

    #[test]
    fn clawde_home_relative_xdg_ignored() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        // Per the XDG spec a relative $XDG_CONFIG_HOME is invalid and ignored.
        std::env::set_var("XDG_CONFIG_HOME", "relative/path");

        assert_eq!(
            Settings::config_dir(),
            home.path().join(".config").join("clawde")
        );
    }

    #[test]
    fn clawde_home_wrapper_matches_config_dir() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("CLAWDE_HOME", tmp.path());
        assert_eq!(super::clawde_home(), Settings::config_dir());
        assert_eq!(super::clawde_home(), PathBuf::from(tmp.path()));
    }

    /// Auto-migration: when `~/.clawde` doesn't exist but `~/.claurst` does,
    /// `config_dir()` renames the legacy dir to the new name on first run.
    #[test]
    fn clawde_home_migrates_legacy_claurst_dir() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();

        // Create legacy dir with a settings file.
        let legacy = home.path().join(".claurst");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("settings.json"), "{\"auto_compact\":true}").unwrap();

        std::env::set_var("HOME", home.path());

        let new = home.path().join(".clawde");
        assert!(!new.exists(), "~/.clawde must not exist before migration");

        let resolved = Settings::config_dir();

        // After migration, ~/.clawde exists and contains the settings file.
        assert!(new.is_dir(), "~/.clawde must exist after migration");
        assert!(
            new.join("settings.json").exists(),
            "settings.json must be in the migrated dir"
        );
        assert_eq!(resolved, new, "config_dir must return the migrated path");
        assert!(
            !legacy.exists(),
            "~/.claurst must not exist after migration"
        );

        // Verify file content survived the rename.
        let content = std::fs::read_to_string(new.join("settings.json")).unwrap();
        assert_eq!(
            content, "{\"auto_compact\":true}",
            "file content must survive migration"
        );
    }
}

// Re-export so the lock is accessible from sibling modules (lib.rs, accounts.rs, etc.)
// without exposing the private `tests` module itself.
#[cfg(all(test, unix))]
pub(crate) use tests::ENV_LOCK;
