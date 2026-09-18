//! Tracks fire-and-forget durable writes so the exit path can drain them.
//!
//! Why this exists: `main` is `#[tokio::main]`, so the runtime is dropped when
//! it returns. Tokio's own contract (runtime.rs) is that tasks spawned through
//! `Runtime::spawn` "keep running until they yield. Then they are dropped" and
//! are **not** guaranteed to run to completion. A durable write spawned in the
//! exit path therefore races the teardown: it yields on its first I/O await and
//! is cancelled with no error and no log.
//!
//! That made the auto-title and auto-synopsis writes unreliable — they landed
//! only when unrelated shutdown work (session save, LSP shutdown, the Ollama
//! unload) happened to outlast the provider call, and vanished silently
//! otherwise.
//!
//! Register the write with [`spawn`] and wait for it with [`drain`] before the
//! runtime goes away. Unlike [`crate::spawn_task_or_dedicated_thread`], which
//! exists so fire-and-forget work survives a missing runtime, this exists so
//! fire-and-forget work survives *runtime teardown*.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Tracked writes that have not finished yet.
static PENDING: AtomicUsize = AtomicUsize::new(0);

/// How often [`drain`] re-checks liveness.
///
/// A notification primitive would be tidier, but `Notify::notify_waiters` only
/// wakes waiters that are already registered, so a write completing between the
/// counter check and the `notified()` registration would be missed and the
/// drain would stall until its deadline. Polling a counter has no such race and
/// costs nothing at human shutdown timescales.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Decrements [`PENDING`] however the tracked task ends.
struct PendingGuard;

impl Drop for PendingGuard {
    fn drop(&mut self) {
        PENDING.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Spawn `task` on the current runtime, tracking it for [`drain`].
///
/// Returns `false` when no runtime is running, in which case nothing is
/// spawned — callers in sync contexts can fall back rather than panicking the
/// way a bare `tokio::spawn` would.
pub fn spawn<F>(task: F) -> bool
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return false;
    };
    PENDING.fetch_add(1, Ordering::SeqCst);
    // Built outside the future so the count is released in every ending:
    // normal completion, panic during unwind, and cancellation before the task
    // is ever polled (the future, and with it the guard, is dropped on runtime
    // teardown). A guard constructed inside the block would leak the count in
    // the last case and strand `drain` at its deadline.
    let guard = PendingGuard;
    handle.spawn(async move {
        let _guard = guard;
        task.await;
    });
    true
}

/// Wait up to `timeout` for every tracked write to finish.
///
/// Returns the number of writes still outstanding (`0` = fully drained) so the
/// caller can report a partial drain instead of failing silently.
pub async fn drain(timeout: Duration) -> usize {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let pending = PENDING.load(Ordering::SeqCst);
        if pending == 0 {
            return 0;
        }
        if tokio::time::Instant::now() >= deadline {
            return pending;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `PENDING` is process-global, so these tests would otherwise observe each
    /// other's writes and become order-dependent (cargo test runs them in
    /// parallel). Serialize on an *async* mutex: a `std` guard held across an
    /// await would trip the workspace `await_holding_lock` lint.
    static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn drain_waits_for_tracked_write() {
        let _serial = TEST_LOCK.lock().await;
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = done.clone();
        assert!(spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            flag.store(true, Ordering::SeqCst);
        }));
        let outstanding = drain(Duration::from_secs(5)).await;
        assert_eq!(outstanding, 0, "drain must wait for the tracked write");
        assert!(
            done.load(Ordering::SeqCst),
            "the tracked write must have actually run"
        );
    }

    #[tokio::test]
    async fn drain_reports_outstanding_rather_than_hanging() {
        let _serial = TEST_LOCK.lock().await;
        spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
        });
        let outstanding = drain(Duration::from_millis(50)).await;
        assert_eq!(outstanding, 1, "a write past the deadline is reported");
        // Let the task finish so the count does not leak into other tests.
        let _ = drain(Duration::from_secs(5)).await;
    }

    #[tokio::test]
    async fn panicking_write_does_not_strand_the_counter() {
        let _serial = TEST_LOCK.lock().await;
        // Silence the deliberate panic so it is not mistaken for a real
        // failure in test output. Safe under TEST_LOCK; passing tests do not
        // panic, so no concurrent test loses its message.
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        spawn(async move {
            panic!("simulated failure inside a tracked write");
        });
        // The drop guard must release the count, so drain returns promptly
        // instead of burning its whole deadline.
        let started = std::time::Instant::now();
        let outstanding = drain(Duration::from_secs(5)).await;
        std::panic::set_hook(previous_hook);
        assert_eq!(outstanding, 0, "a panicked write must not leak the count");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "drain returned only because the count was released"
        );
    }

    #[tokio::test]
    async fn drain_with_nothing_tracked_returns_immediately() {
        let _serial = TEST_LOCK.lock().await;
        let started = std::time::Instant::now();
        assert_eq!(drain(Duration::from_secs(5)).await, 0);
        assert!(started.elapsed() < Duration::from_millis(100));
    }
}
