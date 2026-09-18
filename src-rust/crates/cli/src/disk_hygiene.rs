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
//! tree is oversized past a configurable threshold, and only removes
//! rebuildable artifacts — `release` and cross-compile outputs are never
//! touched. It is a no-op for non-source installs (`clawde upgrade` binaries)
//! where no `target/` exists.
//!
//! Design notes:
//! * Threshold-triggered, not scheduled — the disk only grows when building,
//!   so a startup check is sufficient and avoids any runtime cost otherwise.
//! * The size probe bails early once the running total exceeds the threshold
//!   rather than fully enumerating an enormous tree.
//! * Tiered, cheapest layer first, and both tiers decide for themselves.
//!   Tier 1 drops rustc's incremental cache — it costs one non-incremental
//!   rebuild of the workspace's own crates and leaves dependency artifacts
//!   alone. Tier 2 is the full `cargo clean --profile dev`, which costs a
//!   dependency rebuild and therefore only runs when the tree has grown far
//!   past the threshold or the filesystem is nearly full; a merely-oversized
//!   tree on a roomy disk is left alone and the sizes are logged. Nothing ever
//!   waits on an answer from the user, so headless and interactive runs behave
//!   identically.
//! * Never deletes during or just after a build. `cargo clean` waits on the
//!   same lock cargo holds for a build, so an unchecked clean would queue
//!   behind an in-flight build and then wipe the tree the moment it finished —
//!   destroying work the developer had just completed.
//! * Runs detached in its own task; the TUI render loop is never blocked.
//! * `diskCleanThreshold` (or `CLAWDE_DISABLE_DISK_CLEAN=1`) fully disables it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The built-in threshold (GiB) used when `Config::disk_clean_threshold` is
/// unset. A dev tree rebuilt from scratch measures ~7 GiB on this workspace, so
/// 40 GiB leaves room for several rounds of incremental accumulation before a
/// background clean triggers — catching bloat well before it pressures the
/// disk.
pub const DEFAULT_DISK_CLEAN_THRESHOLD_GIB: u64 = 40;

/// Env var that hard-disables automatic debug-target hygiene, mirroring
/// `CLAWDE_DISABLE_MODELS_FETCH` as the escape hatch for the network-backed
/// background task.
const DISABLE_ENV: &str = "CLAWDE_DISABLE_DISK_CLEAN";

/// Subdirectory of the dev profile holding rustc's incremental compilation
/// cache. Cargo never prunes stale build stamp directories out of it, so it
/// grows with every rebuild — measured at 4.5 GiB of a 12 GiB tree on this
/// workspace. No cargo command trims it, so the cheap tier of the clean removes
/// the directory directly.
const INCREMENTAL_DIR: &str = "incremental";

/// How recently a build must have touched cargo's dev-profile lock file for
/// hygiene to stand down. A cold build of this workspace can run past ten
/// minutes, so anything inside half an hour is plausibly still in use by the
/// developer who triggered it — and deleting it would cost them a full rebuild.
const RECENT_BUILD_WINDOW: Duration = Duration::from_secs(30 * 60);

/// Multiple of the threshold at which the tree is big enough that a full
/// dev-profile clean is worth a dependency rebuild. At the default 40 GiB
/// threshold that is 120 GiB: far beyond the ~9 GiB of legitimate artifacts a
/// built workspace holds here, so reaching it means stale accumulation (rlibs
/// from superseded dependency versions, which cargo never collects and only a
/// full clean removes).
const FULL_CLEAN_MULTIPLIER: u64 = 3;

/// Free space below this share of the filesystem triggers a full clean
/// regardless of tree size. That is the case the whole module exists for: a
/// full rebuild is nothing next to a full disk.
const LOW_FREE_SPACE_PERCENT: u128 = 10;

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

/// Identity of a multiply-linked file, used to charge a hardlinked inode once.
/// `None` for ordinary single-link files (the overwhelming majority) and on
/// platforms where we cannot read an inode number.
#[cfg(unix)]
fn shared_inode(md: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    (md.nlink() > 1).then(|| (md.dev(), md.ino()))
}

#[cfg(not(unix))]
fn shared_inode(_md: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

/// Result of a size walk.
struct Sized {
    /// Block allocation summed so far — an underestimate when `exceeded` is set.
    bytes: u64,
    /// Set when the walk stopped early because the running total passed the
    /// limit, so the caller learns only *that* it is over, never the total.
    exceeded: bool,
}

/// Walk `roots` accumulating on-disk size in bytes, stopping as soon as the
/// running total exceeds `limit`. Pass `u64::MAX` for an exhaustive total.
///
/// Uses an explicit stack rather than recursion so arbitrarily deep trees
/// cannot overflow the call stack. The early bail is what makes the common
/// "is this tree oversized?" question cheap: it never has to enumerate the
/// remainder of an enormous tree to answer it.
///
/// Sizes use filesystem *block allocation* (`st_blocks`), not logical `len`:
/// cargo's `incremental`/`.fingerprint` trees hold thousands of small files
/// that each occupy a full 4 KiB block, so a logical scan under-reports real
/// disk pressure — and hygiene would then only trigger once the disk was
/// already nearly full. Directories themselves allocate negligible blocks and
/// are skipped; symlinks are not followed (their own small allocation is
/// ignored).
///
/// Multiply-linked files are charged once, matching `du`: cargo hardlinks every
/// workspace binary from `deps/` into the profile root, so without inode
/// de-duplication the same bytes are counted twice — measured at 2.4 GiB of an
/// 8.6 GiB tree here, enough to make hygiene fire early.
fn walk_alloc(roots: &[PathBuf], limit: u64) -> Sized {
    let mut stack: Vec<PathBuf> = roots.to_vec();
    let mut bytes: u64 = 0;
    let mut charged: HashSet<(u64, u64)> = HashSet::new();

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
            if let Some(inode) = shared_inode(&md) {
                if !charged.insert(inode) {
                    continue; // the same inode under another name
                }
            }
            bytes = bytes.saturating_add(allocated_bytes(&md));
            if bytes > limit {
                return Sized {
                    bytes,
                    exceeded: true,
                };
            }
        }
    }
    Sized {
        bytes,
        exceeded: false,
    }
}

/// The dev-profile output directory that hygiene manages.
fn debug_dir(workspace: &Path) -> PathBuf {
    workspace.join("target").join("debug")
}

/// rustc's incremental cache inside the dev profile.
fn incremental_dir(workspace: &Path) -> PathBuf {
    debug_dir(workspace).join(INCREMENTAL_DIR)
}

/// Decide whether the debug target tree is oversized relative to `limit_bytes`.
/// `None` when there is no source-checkout debug dir to manage.
///
/// Byte-native (not GiB) so it stays directly testable with small limits.
fn oversized_debug_target(workspace: &Path, limit_bytes: u64) -> Option<bool> {
    let debug = debug_dir(workspace);
    if !debug.is_dir() {
        return None; // not a debug source checkout (already cleaned or release-only)
    }
    Some(walk_alloc(&[debug], limit_bytes).exceeded)
}

/// The dev-profile lock file cargo holds (and refreshes the mtime of) for the
/// whole of a build.
fn debug_lock_path(workspace: &Path) -> PathBuf {
    debug_dir(workspace).join(".cargo-lock")
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

/// Bytes as GiB, for log lines.
fn gib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0 * 1024.0)
}

/// Free-space facts for the filesystem holding the build tree.
#[derive(Clone, Copy)]
struct Space {
    /// Bytes available to this process.
    free: u64,
    /// Filesystem size, so "running out of room" is a share rather than a raw
    /// byte count that means something different on a laptop than on a 2 TB
    /// drive.
    total: u64,
}

/// Space on the filesystem holding `path`.
///
/// `None` when it cannot be read, which keeps hygiene from escalating a
/// dependency-rebuild-costing clean on a guess.
#[cfg(unix)]
fn space_at(path: &Path) -> Option<Space> {
    let vfs = nix::sys::statvfs::statvfs(path).ok()?;
    // Block counts are in fragment units, not `block_size` — mixing the two is
    // the classic statvfs error and skews the answer by orders of magnitude.
    // Accumulated in u128 so a multi-exabyte filesystem cannot overflow, then
    // saturated back down. (Not `u64::from`: the libc counters are already u64
    // on Linux, which clippy flags as a useless conversion.)
    let unit = u128::from(vfs.fragment_size());
    let free = u128::from(vfs.blocks_available()) * unit;
    let total = u128::from(vfs.blocks()) * unit;
    Some(Space {
        free: u64::try_from(free).unwrap_or(u64::MAX),
        total: u64::try_from(total).unwrap_or(u64::MAX),
    })
}

#[cfg(not(unix))]
fn space_at(_path: &Path) -> Option<Space> {
    None
}

/// Whether the tree justifies the expensive tier.
///
/// Two independent triggers, because either on its own is wrong: a tree far past
/// the threshold is pathological accumulation worth clearing even on an empty
/// drive, while a modest tree is worth clearing when the filesystem is nearly
/// full — and a merely-oversized tree on a roomy disk is worth neither.
fn warrants_full_clean(tree_bytes: u64, threshold_bytes: u64, space: Option<Space>) -> bool {
    if tree_bytes >= threshold_bytes.saturating_mul(FULL_CLEAN_MULTIPLIER) {
        return true;
    }
    match space {
        Some(space) if space.total > 0 => {
            let free_percent = u128::from(space.free) * 100 / u128::from(space.total);
            free_percent < LOW_FREE_SPACE_PERCENT
        }
        _ => false,
    }
}

/// Outcome of the unattended tier-1 trim.
enum Trimmed {
    /// Bytes reclaimed; `0` when there was no cache present to remove.
    Freed(u64),
    /// A build appeared between the probe and the trim, so the whole pass
    /// stands down rather than deleting files a live `rustc` may be writing.
    BuildStarted,
}

/// Remove rustc's incremental cache, returning the bytes it occupied.
///
/// `0` when there is nothing there or the removal failed (logged, never fatal).
/// The caller owns the build-in-progress check — this deletes unconditionally,
/// because removing the cache out from under a live `rustc` breaks the build
/// already in flight.
fn trim_incremental(workspace: &Path) -> u64 {
    let dir = incremental_dir(workspace);
    if !dir.is_dir() {
        return 0;
    }
    let freed = walk_alloc(std::slice::from_ref(&dir), u64::MAX).bytes;
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        tracing::warn!("could not remove {}: {e}", dir.display());
        return 0;
    }
    freed
}

/// Perform automatic debug-target hygiene if warranted. Idempotent, silent on
/// success unless action was taken, never errors the process.
///
/// The passed `threshold_gib` is `Config::disk_clean_threshold` resolved via
/// `unwrap_or(DEFAULT_DISK_CLEAN_THRESHOLD_GIB)` (0 disables hygiene entirely);
/// a `DISK_CLEAN_THRESHOLD_GIB` env override wins over it.
///
/// Cleaning is tiered and both tiers decide for themselves — nothing waits on an
/// answer from the user, so a headless run behaves exactly like an interactive
/// one. Tier 1 drops rustc's incremental cache. Tier 2 runs `cargo clean
/// --profile dev` in the workspace, removing the rest of the dev profile but
/// preserving `release` and cross-compile outputs (which the release pipeline
/// still needs), and only when the tree has grown far past the threshold or the
/// filesystem is nearly full. Cargo's dev-profile lock is honoured before every
/// deletion, so a build in flight — or one that just finished — is never wiped.
pub async fn run(threshold_gib: u64) {
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

    let tree_dir = debug_dir(&workspace);

    // `cargo clean` waits on the same lock cargo takes for a build, so without
    // this it would queue behind an in-flight build and then wipe the tree the
    // instant that build finished — destroying work just completed.
    if build_active_or_recent(&workspace) {
        tracing::debug!(
            "dev profile is being built or was built recently — leaving {} alone",
            tree_dir.display()
        );
        return;
    }

    // Tier 1: rustc's incremental cache on its own. This runs unattended
    // because it is the cheap lever — it costs one non-incremental rebuild of
    // the workspace's own crates (a measured ~14 s here) while leaving
    // dependency rlibs, build-script output and the binary in place. Prompting
    // for a seconds-long action would only train the user to click through the
    // dialog that guards the expensive one. The build check is repeated inside
    // the blocking task so a build that started since the probe is not caught
    // with a live `rustc` writing into the directory being removed.
    let trim_ws = workspace.clone();
    let trimmed = tokio::task::spawn_blocking(move || {
        if build_active_or_recent(&trim_ws) {
            Trimmed::BuildStarted
        } else {
            Trimmed::Freed(trim_incremental(&trim_ws))
        }
    })
    .await;
    let freed = match trimmed {
        Ok(Trimmed::Freed(freed)) => freed,
        Ok(Trimmed::BuildStarted) => {
            tracing::debug!("a build started during the hygiene pass — leaving the tree alone");
            return;
        }
        Err(_) => return, // blocking task panicked — never fail the process
    };
    if freed > 0 {
        tracing::warn!(
            "trimmed rustc's incremental cache ({:.1} GiB) — regenerated on the next build",
            gib(freed)
        );
    }

    // Measure exactly now that the cheap tier is done. The walk is ~60 ms on a
    // 13k-file tree and this branch only runs while the tree is over threshold,
    // so the cost is irrelevant — and the real number lets the log state what
    // was measured instead of restating the threshold.
    let measure_path = debug_dir(&workspace);
    let tree_bytes =
        tokio::task::spawn_blocking(move || walk_alloc(&[measure_path], u64::MAX).bytes)
            .await
            .unwrap_or(0);
    if tree_bytes <= limit_bytes {
        tracing::debug!(
            "debug target is back under {threshold_gib} GiB after trimming the incremental cache"
        );
        return;
    }

    // Tier 2 costs a dependency rebuild, so it has to earn it: the tree must be
    // pathological or the disk must actually be under pressure. Being over an
    // arbitrary GiB threshold is not by itself a problem — this workspace holds
    // ~9 GiB of legitimate artifacts even with every cache trimmed — so a tree
    // that is merely oversized on a roomy disk is logged and left alone.
    let space = space_at(&tree_dir);
    if !warrants_full_clean(tree_bytes, limit_bytes, space) {
        match space {
            Some(space) => tracing::info!(
                "debug target at {} is {:.1} GiB, {:.0} GiB free of {:.0} GiB — keeping it; \
                 a full clean runs above {} GiB or below {LOW_FREE_SPACE_PERCENT}% free",
                tree_dir.display(),
                gib(tree_bytes),
                gib(space.free),
                gib(space.total),
                threshold_gib * FULL_CLEAN_MULTIPLIER
            ),
            None => tracing::info!(
                "debug target at {} is {:.1} GiB — keeping it (free space unreadable)",
                tree_dir.display(),
                gib(tree_bytes)
            ),
        }
        return;
    }

    // `cargo clean` waits on the same lock cargo holds for a build — without
    // this re-check the clean would queue behind a build and wipe the tree the
    // moment that build finished.
    if build_active_or_recent(&workspace) {
        tracing::debug!(
            "a build started before the clean could run — leaving {} alone",
            tree_dir.display()
        );
        return;
    }

    tracing::warn!(
        "debug target at {} is {:.1} GiB — running `cargo clean --profile dev`",
        tree_dir.display(),
        gib(tree_bytes)
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
/// from `Config::disk_clean_threshold` (0 disables). Callers invoke this once at
/// startup; it never blocks and cannot fail the process.
pub fn spawn(threshold_gib: u64) {
    // Deliberately a detached task rather than a `pending_writes::spawn`: a
    // cleanup already underway should be allowed to finish even if the user
    // quits while it runs, and waiting on a multi-GiB delete at exit would make
    // quitting slow. Cancelling it would leave the tree half-removed; letting
    // it complete (or be killed by the OS at worst) is the better failure mode.
    tokio::spawn(async move {
        run(threshold_gib).await;
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
        // logical size. Cargo's incremental/.fingerprint trees hold thousands
        // of small files — if we summed logical `len`, 40 × 1-byte files would
        // measure 40 bytes and under-report real disk pressure by an order of
        // magnitude. The probe must instead measure the same `du -sk` total.
        let ws = tmp_workspace();
        for i in 0..40 {
            add_file(&ws, &format!("incremental/unit-{i}"), 1); // 1 logical byte each
        }

        // 40 files × ≤4 KiB alloc ≈ ≥160 KiB. A threshold slightly below the
        // block total MUST trip even though logical bytes are only 40 —
        // proving allocation, not logical size, is what's measured.
        let tight = 1024 * 21; // ~21 KiB of block allocations
        assert!(oversized_debug_target(&ws, tight).unwrap());

        // A generous threshold well above the block total does not trip.
        let loose = 8 * MIB;
        assert!(!oversized_debug_target(&ws, loose).unwrap());

        fs::remove_dir_all(&ws).unwrap();
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn full_clean_needs_gross_oversize_or_disk_pressure() {
        let threshold = 40 * GIB;
        let roomy = Some(Space {
            free: 700 * GIB,
            total: 1900 * GIB,
        });

        // A modest oversize on a roomy disk must not cost a dependency rebuild.
        assert!(!warrants_full_clean(2 * threshold, threshold, roomy));
        // Gross accumulation is worth clearing even with the drive half empty.
        assert!(warrants_full_clean(3 * threshold, threshold, roomy));
        // A nearly-full disk is worth clearing at any size above the threshold.
        assert!(warrants_full_clean(
            threshold + 1,
            threshold,
            Some(Space {
                free: 10,
                total: 200
            })
        ));
        // Exactly at the free-space bar is not yet pressure.
        assert!(!warrants_full_clean(
            threshold + 1,
            threshold,
            Some(Space {
                free: 200,
                total: 2000
            })
        ));
        // Unreadable free space must not escalate on a guess...
        assert!(!warrants_full_clean(2 * threshold, threshold, None));
        // ...but gross oversize still stands on its own.
        assert!(warrants_full_clean(4 * threshold, threshold, None));
        // A zero-size filesystem record is not an emergency.
        assert!(!warrants_full_clean(
            threshold + 1,
            threshold,
            Some(Space { free: 0, total: 0 })
        ));
    }

    #[test]
    #[cfg(unix)]
    fn space_at_reads_a_real_filesystem() {
        let space = space_at(&std::env::temp_dir()).expect("statvfs must work on a real dir");
        assert!(space.total > 0, "a mounted filesystem has a size");
        assert!(
            space.free <= space.total,
            "free space cannot exceed the filesystem size"
        );
    }

    #[test]
    #[cfg(unix)]
    fn hardlinked_binaries_are_charged_once() {
        // Regression: cargo hardlinks every workspace binary from `deps/` into
        // the profile root and `du` charges that inode once. Summing per-link
        // block counts over-reports by the size of every binary (2.4 GiB on an
        // 8.6 GiB tree when this was found), which trips hygiene early.
        let ws = tmp_workspace();
        add_file(&ws, "deps/clawde-deadbeef", 2 * MIB);
        fs::hard_link(
            ws.join("target")
                .join("debug")
                .join("deps")
                .join("clawde-deadbeef"),
            ws.join("target").join("debug").join("clawde"),
        )
        .unwrap();

        let total = walk_alloc(&[ws.join("target").join("debug")], u64::MAX).bytes;
        assert!(
            total >= 2 * MIB,
            "the inode itself must still be charged, got {total}"
        );
        assert!(
            total < 3 * MIB,
            "a second link must not be charged again, got {total} bytes"
        );

        fs::remove_dir_all(&ws).unwrap();
    }

    #[test]
    fn walk_alloc_bails_early_only_when_over_limit() {
        let ws = tmp_workspace();
        add_file(&ws, "deps/one", 2 * MIB);
        let debug = ws.join("target").join("debug");

        let over = walk_alloc(std::slice::from_ref(&debug), MIB);
        assert!(
            over.exceeded,
            "2 MiB over a 1 MiB limit must report exceeded"
        );
        assert!(over.bytes >= 2 * MIB, "the crossing file is included");

        let under = walk_alloc(&[debug], 100 * MIB);
        assert!(!under.exceeded);
        assert!(
            under.bytes >= 2 * MIB,
            "an exhaustive walk must total the tree, got {}",
            under.bytes
        );

        fs::remove_dir_all(&ws).unwrap();
    }

    #[test]
    fn trim_incremental_removes_only_the_cache() {
        // The tier-1 clean must never cost the developer a dependency rebuild:
        // deps/ and build/ have to survive it. This is the whole reason the
        // tiers exist rather than one blanket `cargo clean`.
        let ws = tmp_workspace();
        add_file(&ws, "incremental/unit-a", MIB);
        add_file(&ws, "incremental/unit-b", MIB);
        add_file(&ws, "deps/keep-me", MIB);
        add_file(&ws, "build/keep-me", MIB);

        let freed = trim_incremental(&ws);

        assert!(
            freed >= 2 * MIB,
            "must report the cache it removed, got {freed}"
        );
        assert!(
            !incremental_dir(&ws).exists(),
            "the incremental cache must be gone"
        );
        let debug = ws.join("target").join("debug");
        assert!(
            debug.join("deps").join("keep-me").is_file(),
            "dependency artifacts must survive a tier-1 trim"
        );
        assert!(
            debug.join("build").join("keep-me").is_file(),
            "build-script output must survive a tier-1 trim"
        );

        fs::remove_dir_all(&ws).unwrap();
    }

    #[test]
    fn trim_incremental_is_a_noop_without_a_cache() {
        let ws = tmp_workspace();
        add_file(&ws, "deps/keep-me", 1024);

        assert_eq!(
            trim_incremental(&ws),
            0,
            "nothing to trim reports zero bytes"
        );

        fs::remove_dir_all(&ws).unwrap();
    }
}
