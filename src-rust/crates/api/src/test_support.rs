// api/src/test_support.rs — test-only helpers shared across the crate's
// `#[cfg(test)]` modules.
//
// The api crate's tests construct `clawde_core::AuthStore` values and several
// production paths persist them (e.g. `AuthStore::set_keys` → `save()`).
// Without redirecting `CLAWDE_HOME` those writes land in the user's real
// `~/.clawde/auth.json`. Every test that touches the auth store (or the
// settings dir in general) must hold a [`TestHome`] guard so disk writes stay
// inside a temp dir.

use std::sync::{Mutex, MutexGuard, OnceLock};

/// Serializes every `CLAWDE_HOME`-mutating test in this crate. `cargo test`
/// runs tests within a binary on parallel threads; without a shared lock two
/// guards would restore each other's env var mid-test.
static CLAWDE_HOME_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Panic-safe guard: points `CLAWDE_HOME` at a fresh temp dir for its
/// lifetime, restoring the previous value on drop — even during unwinding
/// from a panic.
///
/// Holds the shared [`CLAWDE_HOME_LOCK`] so tests that mutate the
/// process-global env var never race each other.
pub(crate) struct TestHome {
    _lock: MutexGuard<'static, ()>,
    _tmp: tempfile::TempDir,
    prev_clawde_home: Option<std::ffi::OsString>,
}

impl TestHome {
    pub(crate) fn new() -> Self {
        let lock = CLAWDE_HOME_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = std::env::var_os("CLAWDE_HOME");
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("CLAWDE_HOME", tmp.path());
        TestHome {
            _lock: lock,
            _tmp: tmp,
            prev_clawde_home: prev,
        }
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        match &self.prev_clawde_home {
            Some(v) => std::env::set_var("CLAWDE_HOME", v),
            None => std::env::remove_var("CLAWDE_HOME"),
        }
    }
}

/// Build an in-memory auth store behind a [`TestHome`] CLAWDE_HOME redirect
/// so any accidental persistence (e.g. a future `set_keys` call) lands in a
/// temp dir instead of the user's real `~/.clawde/auth.json`. The guard is
/// returned so it stays alive for the whole test body.
pub(crate) fn test_auth_store() -> (clawde_core::AuthStore, TestHome) {
    let home = TestHome::new();
    (clawde_core::AuthStore::default(), home)
}
