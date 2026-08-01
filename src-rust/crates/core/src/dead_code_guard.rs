//! Test-support helper for the workspace dead-code guard.
//!
//! rustc's `dead_code` lint never fires for `pub` items, so a `pub fn`
//! that nothing calls (like the former `build_help_entries` in the commands
//! crate) silently rots. Every crate in the workspace carries a test that
//! calls [`assert_no_dead_pub_functions`] with its own `CARGO_MANIFEST_DIR`;
//! the scan fails if any `pub fn` / `pub async fn` declared in that crate's
//! `src/` has no reference anywhere in the workspace except its own
//! declaration.
//!
//! A name that appears only once across every `*.rs` file in the workspace
//! (its declaration) has zero call sites — that is dead. Functions carrying
//! an explicit `#[allow(dead_code)]` attribute are skipped: that attribute is
//! the author's declared intent to keep the item (e.g. a public API surface
//! reserved for a future feature), so the guard does not second-guess it.

use regex::Regex;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Recursively collect every `*.rs` file under `dir`, skipping `target/`.
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) == Some("target") {
                continue;
            }
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Whether `#[allow(dead_code)]` appears on any of the up to `lookback`
/// lines immediately before byte offset `start` (doc comments may sit
/// between the attribute and the item).
fn preceded_by_allow_dead_code(content: &str, start: usize, lookback: usize) -> bool {
    let mut end = start;
    for _ in 0..lookback {
        let Some(nl) = content[..end].rfind('\n') else {
            break;
        };
        let line = content[nl + 1..end].trim();
        if line.starts_with("#[allow(dead_code") {
            return true;
        }
        end = nl;
    }
    false
}

/// Fail the calling test if any `pub fn` declared in the crate rooted at
/// `manifest_dir` has no reference anywhere in the workspace.
///
/// `manifest_dir` is the crate's `CARGO_MANIFEST_DIR` (from `env!`).
#[doc(hidden)]
pub fn assert_no_dead_pub_functions(manifest_dir: &str) {
    let crate_root = PathBuf::from(manifest_dir);
    // Walk up to the directory that holds the workspace `Cargo.toml` and a
    // `crates/` tree. Finding it dynamically (rather than assuming a fixed
    // depth) means a layout change fails loudly instead of silently
    // scanning the wrong root and passing vacuously.
    let workspace_root = crate_root
        .ancestors()
        .find(|p| p.join("Cargo.toml").is_file() && p.join("crates").is_dir())
        .expect("workspace root (dir containing workspace Cargo.toml and crates/)");

    let mut files = Vec::new();
    collect_rs_files(workspace_root, &mut files);

    let decl_re = Regex::new(r"\bpub\s+(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap();

    // Declarations: only within this crate's own src/. A `pub fn` carrying an
    // explicit `#[allow(dead_code)]` attribute is intentionally kept (e.g. an
    // API surface reserved for a future feature), so it is skipped.
    let crate_src = crate_root.join("src");
    let mut declared: BTreeSet<String> = BTreeSet::new();
    for path in &files {
        if !path.starts_with(&crate_src) {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(path) {
            for cap in decl_re.captures_iter(&content) {
                let whole = cap.get(0).expect("declaration regex always captures");
                if preceded_by_allow_dead_code(&content, whole.start(), 5) {
                    continue;
                }
                declared.insert(cap[1].to_string());
            }
        }
    }

    // Workspace-wide reference scan (target/ already excluded by walker).
    let mut all_src = String::new();
    for path in &files {
        if let Ok(content) = std::fs::read_to_string(path) {
            all_src.push_str(&content);
            all_src.push('\n');
        }
    }

    let mut dead: Vec<String> = Vec::new();
    for name in &declared {
        let re = Regex::new(&format!(r"\b{}\b", regex::escape(name))).unwrap();
        if re.find_iter(&all_src).count() < 2 {
            dead.push(name.clone());
        }
    }

    assert!(
        dead.is_empty(),
        "pub functions declared but never referenced anywhere in the workspace: {:?}",
        dead
    );
}
