// File-change watcher for live reload.
//
// v0 uses a cheap polling snapshot (mtime + size per file) rather than a
// native filesystem-notify dependency: good enough for a dev-site watcher and
// zero new deps. A dedicated notify-based watcher is a later slice.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Relative-path -> (mtime_nanos, size). Keyed by forward-slash relative path.
pub type Snapshot = BTreeMap<String, (u128, u64)>;

/// Top-level subtrees that never belong in a dev-site reload: VCS metadata,
/// dependency trees, and build caches. Skipped wholesale (recursion and
/// entries) so an `npm install` or `cargo build` inside a served project root
/// doesn't trigger a page-reload storm or a full-tree walk every poll.
pub const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target"];

pub fn snapshot(root: &Path) -> Snapshot {
    let mut out = Snapshot::new();
    collect(root, root, &mut out);
    out
}

fn collect(root: &Path, dir: &Path, out: &mut Snapshot) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return, // a missing/removed dir mid-walk is not fatal
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        // The first path segment decides the skip; a symlink or nested copy
        // under one of these names is ignored entirely.
        if SKIP_DIRS.contains(&rel.split('/').next().unwrap_or("")) {
            continue;
        }
        match entry.metadata() {
            Ok(meta) if meta.is_dir() => collect(root, &path, out),
            Ok(meta) => {
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                out.insert(rel, (mtime, meta.len()));
            }
            _ => {}
        }
    }
}

/// True when the two snapshots differ (any add/remove/modify).
pub fn changed(a: &Snapshot, b: &Snapshot) -> bool {
    a != b
}

/// Spawn a background polling watcher; sends `()` on the broadcast channel
/// whenever the tree under `root` changes.
pub fn spawn_watcher(
    root: PathBuf,
    tx: tokio::sync::broadcast::Sender<()>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut prev = snapshot(&root);
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let now = snapshot(&root);
            if changed(&prev, &now) {
                prev = now;
                let _ = tx.send(());
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_file(root: &Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn snapshot_detects_add_modify_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_file(root, "index.html", "<h1>hello</h1>");
        write_file(root, "css/style.css", "body { color: red; }");

        let before = snapshot(root);
        assert!(!before.is_empty());

        // No change -> identical snapshot.
        let again = snapshot(root);
        assert!(!changed(&before, &again));

        // Modify.
        write_file(root, "index.html", "<h1>changed</h1>");
        assert!(changed(&before, &snapshot(root)));

        // Add.
        write_file(root, "js/app.js", "console.log(1);");
        assert!(changed(&before, &snapshot(root)));

        // Delete.
        let current = snapshot(root);
        fs::remove_file(root.join("js/app.js")).unwrap();
        assert!(changed(&current, &snapshot(root)));
    }

    #[test]
    fn snapshot_of_missing_root_is_empty() {
        assert!(snapshot(Path::new("/definitely/not/a/real/dir-12345")).is_empty());
    }

    #[test]
    fn snapshot_skips_noise_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_file(root, "index.html", "x");
        write_file(root, "node_modules/pkg/index.js", "x");
        write_file(root, ".git/config", "x");
        write_file(root, "target/debug/out", "x");

        let snap = snapshot(root);
        assert!(snap.contains_key("index.html"));
        assert!(!snap.keys().any(|k| k.starts_with("node_modules")
            || k.starts_with(".git")
            || k.starts_with("target")));

        // Changes inside skipped dirs never register as a reload.
        write_file(root, "node_modules/pkg/index.js", "changed");
        write_file(root, ".git/config", "changed");
        write_file(root, "target/debug/out", "changed");
        assert!(!changed(&snap, &snapshot(root)));

        // A real source change still does.
        write_file(root, "index.html", "changed");
        assert!(changed(&snap, &snapshot(root)));
    }
}
