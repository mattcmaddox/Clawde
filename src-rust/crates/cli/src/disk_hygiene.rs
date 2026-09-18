//! Automatic disk hygiene for the cargo debug build tree.
//!
//! Building Clawde from a source checkout accumulates build artifacts in
//! `<workspace>/target/debug` that cargo never garbage-collects beyond its own
//! incremental heuristics. Across many `cargo build`/`cargo check` runs the
//! directory can grow to hundreds of GiB (a fresh dev build is ~6 GiB; this
//! author's tree reached 685 GiB before the disk hit 100%) without any signal
//! until space actually runs out.
//!
//! This module runs a *background* check at interactive startup: it only acts
//! on a source checkout (a `target/debug` next to the binary), only when that
//! tree is oversized past a configurable threshold, and only removes the
//! rebuildable dev profile — `release` and cross-compile artifacts are never
//! touched. It is a no-op for non-source installs (`clawde upgrade` binaries)
//! where no `target/` exists.
//!
//! Design notes:
//! * Threshold-triggered, not scheduled — the disk only grows when building,
//!   so a startup check is sufficient and avoids any runtime cost otherwise.
//! * The size probe bails early once the running total exceeds the threshold
//!   rather than fully enumerating an enormous tree.
//! * Never deletes unattended. Deleting the tree costs the next build a full
//!   rebuild, so the user is asked to confirm; a run with nobody to ask only
//!   warns, and an unanswered or dismissed prompt means the tree is kept.
//! * Never deletes during or just after a build. `cargo clean` waits on the
//!   same lock cargo holds for a build, so an unchecked clean would queue
//!   behind an in-flight build and then wipe the tree the moment it finished —
//!   destroying work the developer had just completed.
//! * Runs detached in its own task; the TUI render loop is never blocked.
//! * `diskCleanThreshold` (or `CLAWDE_DISABLE_DISK_CLEAN=1`) fully disables it.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// The built-in threshold (GiB) used when `Config::disk_clean_threshold` is
/// unset. A fresh dev build is ~6 GiB, so 40 GiB leaves ~6x room for
/// incremental accumulation before a background clean triggers — catching bloat
/// well before it pressures the disk.
pub const DEFAULT_DISK_CLEAN_THRESHOLD_GIB: u64 = 40;

/// Env var that hard-disables automatic debug-target hygiene, mirroring
/// `CLAWDE_DISABLE_MODELS_FETCH` as the escape hatch for the network-backed
/// background task.
const DISABLE_ENV: &str = "CLAWDE_DISABLE_DISK_CLEAN";

/// How recently a build must have touched cargo's dev-profile lock file for
/// hygiene to stand down. A cold build of this workspace can run past ten
/// minutes, so anything inside half an hour is plausibly still in use by the
/// developer who triggered it — and deleting it would cost them a full rebuild.
const RECENT_BUILD_WINDOW: Duration = Duration::from_secs(30 * 60);

/// How long to wait for an answer to the cleanup prompt before giving up and
/// leaving the tree alone. The user may leave the dialog open indefinitely.
const ASK_TIMEOUT: Duration = Duration::from_secs(120);

/// Option label that authorises the cleanup.
const CLEAN_OPTION: &str = "Clean it";

/// Option label that keeps the tree.
const KEEP_OPTION: &str = "Keep it";

/// Locate the cargo workspace root from the compiled-in manifest dir, exactly
/// mirroring `build.rs::workspace_root_from`. Returns `None` when this binary
/// was not compiled from a source checkout (e.g. an `upgrade`-installed
/// binary), which makes the whole hygiene pass a fast no-op.
fn workspace_root() -> Option<PathBuf> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest_dir
        .parent()
        .and_then(|p| p.parent())?
        .to_path_buf();
    if !workspace.join("Cargo.toml").is_file() {
        return None;
    }
    Some(workspace)
}

/// Compute a path's on-disk block allocation in bytes using `st_blocks`.
///
/// Returns `0` when the filesystem reports no block info (or for symlinks,
/// whose own small allocation is negligible next to a multi-GiB tree).
#[cfg(unix)]
fn allocated_bytes(md: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    // `st_blocks` is in 512-byte units; that is the value `du -sk` totals,
    // so this matches what the OS actually charges the disk (block-allocated,
    // not the logical file size).
    md.blocks().saturating_mul(512)
}

/// Non-unix fallback: we can't read allocation, so fall back to logical size
/// (an underestimate, but syscalls here still bound the walk correctly).
#[cfg(not(unix))]
fn allocated_bytes(md: &std::fs::Metadata) -> u64 {
    md.len()
}

/// Cheap, early-bail scan of a directory tree's on-disk size in bytes.
///
/// Uses an explicit stack rather than recursion so arbitrarily deep trees
/// cannot overflow the call stack. Stops accumulating (and returns `true`)
/// as soon as the running total exceeds `limit` — it never needs to enumerate
/// the remainder of an oversized tree to know it is oversized.
///
/// Sizes use filesystem *block allocation* (`st_blocks`), not logical `len`:
/// cargo's `incremental`/`.fingerprint` dirs hold hundreds of thousands of
/// tiny files that each occupy a 4 KiB block despite tiny logical sizes, so a
/// logical scan would under-report real disk pressure by an order of magnitude
/// — meaning hygiene would only trigger after the disk was already nearly
/// full. Directories themselves allocate negligible blocks and are skipped;
/// symlinks are not followed (their own small allocation is ignored).
fn tree_exceeds(roots: &[PathBuf], limit: u64) -> bool {
    let mut stack: Vec<PathBuf> = roots.to_vec();
    let mut total: u64 = 0;

    while let Some(path) = stack.pop() {
        let md = match std::fs::symlink_metadata(&path) {
            Ok(md) => md,
            Err(_) => continue, // gone or unreadable — skip, never fatal
        };
        if md.is_dir() {
            if let Ok(iter) = std::fs::read_dir(&path) {
                for entry in iter.flatten() {
                    stack.push(entry.path());
                }
            }
        } else if !md.is_symlink() {
            total = total.saturating_add(allocated_bytes(&md));
            if total > limit {
                return true;
            }
        }
    }
    false
}

/// Decide whether the debug target tree is oversized relative to `limit_bytes`.
/// `None` when there is no source-checkout debug dir to manage.
///
/// Byte-native (not GiB) so it stays directly testable with small limits.
fn oversized_debug_target(workspace: &Path, limit_bytes: u64) -> Option<bool> {
    let debug = workspace.join("target").join("debug");
    if !debug.is_dir() {
        return None; // not a debug source checkout (already cleaned or release-only)
    }
    Some(tree_exceeds(&[debug], limit_bytes))
}

/// The dev-profile lock file cargo holds (and refreshes the mtime of) for the
/// whole of a build.
fn debug_lock_path(workspace: &Path) -> PathBuf {
    workspace.join("target").join("debug").join(".cargo-lock")
}

/// True when the dev profile is being built right now, or was built recently.
///
/// Cargo holds an exclusive `flock` on the dev-profile lock file for the
/// duration of a build and refreshes that file's mtime on every build, so two
/// cheap probes cover both cases: a non-blocking exclusive lock that fails
/// means cargo holds it at this instant, and an mtime inside
/// [`RECENT_BUILD_WINDOW`] means a build finished moments ago.
///
/// Both matter because `cargo clean` waits on the *same* lock: without this
/// check it queues behind an in-flight build and then wipes the tree the
/// instant that build finishes.
fn build_active_or_recent(workspace: &Path) -> bool {
    let lock = debug_lock_path(workspace);
    let Ok(metadata) = std::fs::metadata(&lock) else {
        return false; // never built here, or already cleaned
    };
    if let Ok(modified) = metadata.modified() {
        match modified.elapsed() {
            Ok(age) if age < RECENT_BUILD_WINDOW => return true,
            // Unreadable age (clock skew, exotic filesystem): fail safe by
            // treating the tree as in use rather than deleting it.
            Err(_) => return true,
            Ok(_) => {}
        }
    }
    lock_is_held(&lock)
}

/// Non-blocking probe for an exclusive `flock` held by another process.
///
/// Taking the lock and dropping the guard immediately releases it again, so
/// this leaves no state behind. Any failure other than "lock free" is treated
/// as held, which fails safe by leaving the tree alone.
#[cfg(unix)]
fn lock_is_held(path: &Path) -> bool {
    use nix::fcntl::{Flock, FlockArg};
    let Ok(file) = std::fs::File::open(path) else {
        return false; // unreadable — fall back to the mtime signal alone
    };
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(_held) => false, // we took it, so cargo is not holding it
        Err(_) => true,     // EWOULDBLOCK: a build is running
    }
}

#[cfg(not(unix))]
fn lock_is_held(_path: &Path) -> bool {
    false
}

/// Perform automatic debug-target hygiene if warranted. Idempotent, silent on
/// success unless action was taken, never errors the process.
///
/// The passed `threshold_gib` is `Config::disk_clean_threshold` resolved via
/// `unwrap_or(DEFAULT_DISK_CLEAN_THRESHOLD_GIB)` (0 disables hygiene entirely);
/// a `DISK_CLEAN_THRESHOLD_GIB` env override wins over it.
/// Cleanup runs `cargo clean --profile dev` in the workspace — it removes the
/// dev profile's build artifacts but preserves `release` and cross-compile
/// outputs (which the release pipeline still needs).
///
/// Deleting is never unattended: `ask` is the interactive session's question
/// channel, and `None` (or an unanswered/dismissed prompt) only warns. Cargo's
/// dev-profile lock is honoured too, so a build in flight — or one that just
/// finished — is never wiped.
pub async fn run(
    threshold_gib: u64,
    ask: Option<tokio::sync::mpsc::UnboundedSender<clawde_tools::UserQuestionEvent>>,
) {
    // Hard opt-out (belt and suspenders to the config knob).
    if std::env::var(DISABLE_ENV).is_ok() {
        tracing::debug!("{DISABLE_ENV} set — skipping automatic debug-target hygiene");
        return;
    }

    let threshold_gib = std::env::var("DISK_CLEAN_THRESHOLD_GIB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(threshold_gib);

    let workspace = match workspace_root() {
        Some(w) => w,
        None => {
            tracing::debug!("no source checkout for this binary — disk hygiene is a no-op");
            return;
        }
    };

    if threshold_gib == 0 {
        tracing::debug!("disk clean threshold is 0 — hygiene disabled");
        return;
    }

    // The size probe walks the whole (potentially multi-GiB) tree with
    // blocking `std::fs` reads. Run it on the blocking threadpool so it cannot
    // stall a runtime worker, then act on the result.
    let probe = workspace.clone();
    let limit_bytes = threshold_gib.saturating_mul(1024 * 1024 * 1024);
    let oversize = match tokio::task::spawn_blocking(move || {
        oversized_debug_target(&probe, limit_bytes)
    })
    .await
    {
        Ok(r) => match r {
            Some(o) => o,
            None => return, // no debug dir
        },
        Err(_) => return, // probe panicked — never fail the process
    };

    if !oversize {
        tracing::debug!("debug target under {threshold_gib} GiB — no clean needed");
        return;
    }

    let debug_dir = workspace.join("target").join("debug");

    // `cargo clean` waits on the same lock cargo takes for a build, so without
    // this it would queue behind an in-flight build and then wipe the tree the
    // instant that build finished — destroying work just completed.
    if build_active_or_recent(&workspace) {
        tracing::debug!(
            "dev profile is being built or was built recently — leaving {} alone",
            debug_dir.display()
        );
        return;
    }

    // Deleting the tree costs the next build a full rebuild, so it is never done
    // unattended: ask first, and only warn when there is nobody to ask.
    let Some(ask) = ask else {
        tracing::warn!(
            "debug target at {} exceeded {threshold_gib} GiB — not cleaning automatically \
             (no interactive session to confirm); run `cargo clean --profile dev` to reclaim the space",
            debug_dir.display()
        );
        return;
    };

    let question = format!(
        "The cargo debug build tree is over {threshold_gib} GiB:\n  {}\n\nDelete the dev-profile \
         artifacts to reclaim the space? The next `cargo build` then has to rebuild everything.",
        debug_dir.display()
    );
    let options = vec![CLEAN_OPTION.to_string(), KEEP_OPTION.to_string()];
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if ask
        .send(clawde_tools::UserQuestionEvent {
            question,
            options: Some(options),
            reply_tx,
        })
        .is_err()
    {
        tracing::debug!("question channel closed — leaving the debug target alone");
        return;
    }

    // The dialog may be left open, or the session may exit with it unanswered.
    // Dismissing replies with an empty string, which is not consent either.
    let answer = match tokio::time::timeout(ASK_TIMEOUT, reply_rx).await {
        Ok(Ok(answer)) => answer,
        Ok(Err(_)) => {
            tracing::debug!("cleanup prompt dropped unanswered — leaving the tree alone");
            return;
        }
        Err(_) => {
            tracing::debug!(
                "no answer to the cleanup prompt within {}s — leaving the tree alone",
                ASK_TIMEOUT.as_secs()
            );
            return;
        }
    };
    if answer.trim() != CLEAN_OPTION {
        tracing::info!("debug-target cleanup not confirmed — nothing removed");
        return;
    }

    tracing::warn!(
        "debug target at {} exceeded {threshold_gib} GiB — confirmed by the user, running `cargo clean --profile dev`",
        debug_dir.display()
    );
    let output = tokio::process::Command::new("cargo")
        .arg("clean")
        .arg("--profile")
        .arg("dev")
        .current_dir(&workspace)
        .output()
        .await;

    match output {
        Ok(out) if out.status.success() => {
            tracing::warn!("debug target cleaned (freed rebuildable dev artifacts)");
        }
        Ok(out) => {
            tracing::warn!(
                "cargo clean failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Err(e) => {
            tracing::warn!("could not run cargo clean: {e}");
        }
    }
}

/// Spawn the hygiene pass as a detached background task. `threshold_gib` comes
/// from `Config::disk_clean_threshold` (0 disables), and `ask` is the
/// interactive session's question channel used to confirm the deletion. Callers
/// invoke this once at startup; it never blocks and cannot fail the process.
pub fn spawn(
    threshold_gib: u64,
    ask: Option<tokio::sync::mpsc::UnboundedSender<clawde_tools::UserQuestionEvent>>,
) {
    tokio::spawn(async move {
        run(threshold_gib, ask).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_workspace() -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("clawde-hygiene-{}-{stamp}", std::process::id()));
        fs::create_dir_all(dir.join("target").join("debug")).unwrap();
        // Pretend a cargo workspace so workspace_root_from-style checks pass.
        fs::write(dir.join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
        dir
    }

    fn add_file(root: &Path, rel: &str, bytes: u64) {
        let p = root.join("target").join("debug").join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut fd = fs::File::create(p).unwrap();
        // Write real (non-sparse) bytes in chunks so the blocks are actually
        // allocated — `set_len` would create a sparse file that occupies
        // almost no disk, defeating the block-allocation probe it exercises.
        const CHUNK: usize = 64 * 1024;
        let mut remaining = bytes;
        while remaining > 0 {
            let n = remaining.min(CHUNK as u64) as usize;
            use std::io::Write;
            fd.write_all(&vec![0x5au8; n]).unwrap();
            remaining -= n as u64;
        }
        fd.sync_all().unwrap();
    }

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn empty_or_small_tree_is_not_oversized() {
        let ws = tmp_workspace();
        add_file(&ws, "deps/foo", 1024);
        let result = oversized_debug_target(&ws, 100 * MIB).unwrap();
        assert!(!result, "1 KiB under a 100 MiB threshold must not trigger");
        fs::remove_dir_all(&ws).unwrap();
    }

    #[test]
    fn tree_over_threshold_triggers() {
        let ws = tmp_workspace();
        // 2 MiB of fake deps vs a 1 MiB threshold → over.
        add_file(&ws, "deps/big", 2 * MIB);
        let result = oversized_debug_target(&ws, MIB).unwrap();
        assert!(result, "2 MiB over a 1 MiB threshold must trigger");
        fs::remove_dir_all(&ws).unwrap();
    }

    #[test]
    fn missing_debug_dir_is_noop() {
        let ws = tmp_workspace();
        fs::remove_dir_all(ws.join("target").join("debug")).unwrap();
        assert!(oversized_debug_target(&ws, 100 * MIB).is_none());
        fs::remove_dir_all(&ws).unwrap();
    }

    #[test]
    fn missing_build_lock_is_not_active() {
        let ws = tmp_workspace();
        assert!(
            !build_active_or_recent(&ws),
            "no lock file means nothing is building"
        );
        fs::remove_dir_all(&ws).unwrap();
    }

    #[test]
    fn fresh_build_lock_blocks_cleanup() {
        // Regression: hygiene must never wipe a tree the developer just built.
        // Cargo refreshes this file's mtime on every build.
        let ws = tmp_workspace();
        fs::write(ws.join("target").join("debug").join(".cargo-lock"), "").unwrap();
        assert!(
            build_active_or_recent(&ws),
            "a build that just finished must stop hygiene destroying its output"
        );
        fs::remove_dir_all(&ws).unwrap();
    }

    #[test]
    fn stale_build_lock_does_not_block_cleanup() {
        let ws = tmp_workspace();
        let lock = ws.join("target").join("debug").join(".cargo-lock");
        let file = fs::File::create(&lock).unwrap();
        let long_ago = SystemTime::now() - RECENT_BUILD_WINDOW * 4;
        file.set_modified(long_ago).unwrap();
        drop(file);
        assert!(
            !build_active_or_recent(&ws),
            "an old build must not block hygiene forever"
        );
        fs::remove_dir_all(&ws).unwrap();
    }

    #[test]
    fn nested_tree_is_measured_recursively() {
        let ws = tmp_workspace();
        add_file(&ws, "a/b/c/deep", MIB + 1); // strictly over the 1 MiB threshold
        assert!(
            oversized_debug_target(&ws, MIB).unwrap(),
            "nested tree counts too"
        );
        fs::remove_dir_all(&ws).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn many_small_files_measured_by_block_allocation() {
        // Regression: the probe must charge per-file block allocation, not
        // logical size. Cargo's incremental dirs hold hundreds of thousands of
        // tiny files — if we summed logical `len`, 40 × 1-byte files would
        // measure 40 bytes and under-report real disk pressure by an order of
        // magnitude. The probe must instead measure the same `du -sk` total.
        let ws = tmp_workspace();
        for i in 0..40 {
            add_file(&ws, &format!("incremental/unit-{i}"), 1); // 1 logical byte each
        }

        // 40 files × ≤4 KiB alloc ≈ ≥160 KiB. A threshold slightly below the
        // block total MUST trip even though logical bytes are only 40 —
        // proving allocation, not logical size, is what's measured.
        let tight = 1024 * 21; // ~21 KiB mouth allocations
        assert!(oversized_debug_target(&ws, tight).unwrap());

        // A generous threshold well above the block total does not trip.
        let loose = 8 * MIB;
        assert!(!oversized_debug_target(&ws, loose).unwrap());

        fs::remove_dir_all(&ws).unwrap();
    }
}
