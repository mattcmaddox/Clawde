//! FS-manifest scope gate (container tier, spec §8).
//!
//! A card may carry an allowlist of repo-relative paths (`Card::scope_paths`).
//! After a container attempt, the FS-manifest diff (everything the agent
//! actually added/modified/deleted, including files it never mentioned) is
//! checked against that list; any path outside it is a scope violation and
//! the attempt does not win — an out-of-scope edit is exactly the
//! blast-radius signal the git diff cannot give (untracked-but-ignored files
//! and files outside the diff's reach are invisible to host-tier git).
//!
//! Pattern forms (mirroring the eval fixtures' `allowed_paths`):
//! - empty list, `.`, or `./` — the whole repo is in scope;
//! - `dir/` or `dir` — the directory subtree (any depth);
//! - `file.ext` — exactly that file;
//! - `*.ext` — files with that suffix at any depth (the only glob; `*` may
//!   appear only as a leading component).

/// Fold a manifest path (`sha256sum` emits `./x/y`) to repo-relative form.
fn canonical(path: &str) -> &str {
    path.strip_prefix("./").unwrap_or(path)
}

/// Whether `path` (repo-relative, as emitted by the manifest) falls inside
/// `allowed`. An empty allowlist means everything is allowed (the host-tier
/// default: no manifest, no scope opinion).
pub fn path_allowed(path: &str, allowed: &[String]) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let path = canonical(path);
    if path.is_empty() {
        return false;
    }
    allowed.iter().any(|pattern| {
        let pattern = pattern.trim();
        if pattern.is_empty() || pattern == "." || pattern == "./" {
            return true;
        }
        if let Some(suffix) = pattern.strip_prefix("*.") {
            // `*.ext`: suffix match on the file name at any depth.
            let file = path.rsplit('/').next().unwrap_or(path);
            return file.ends_with(suffix);
        }
        let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
        let pattern = pattern.strip_suffix('/').unwrap_or(pattern);
        if pattern.is_empty() {
            return true;
        }
        // Exact file, or any path under the directory.
        path == pattern || path.starts_with(&format!("{pattern}/"))
    })
}

/// Every path in the manifest diff that the allowlist does not cover, sorted
/// added-first then modified then deleted (the same order the eval's
/// `scope_violations` reports, so records stay comparable across tiers).
pub fn violations(diff: &crate::container::ManifestDiff, allowed: &[String]) -> Vec<String> {
    let mut out: Vec<String> = diff
        .added
        .iter()
        .chain(diff.modified.iter())
        .chain(diff.deleted.iter())
        .filter(|p| !path_allowed(p, allowed))
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(added: &[&str], modified: &[&str], deleted: &[&str]) -> crate::container::ManifestDiff {
        crate::container::ManifestDiff {
            added: added.iter().map(|s| s.to_string()).collect(),
            modified: modified.iter().map(|s| s.to_string()).collect(),
            deleted: deleted.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn empty_allowlist_allows_everything() {
        assert!(path_allowed("src/anything.rs", &[]));
        assert!(violations(&diff(&["x"], &["y"], &["z"]), &[]).is_empty());
    }

    #[test]
    fn whole_repo_markers_allow_everything() {
        for marker in [".", "./"] {
            assert!(
                path_allowed("src/main.rs", &[marker.to_string()]),
                "{marker}"
            );
        }
    }

    #[test]
    fn directory_pattern_covers_subtree() {
        let allowed = vec!["src/".to_string()];
        assert!(path_allowed("src/main.rs", &allowed));
        assert!(path_allowed("src/deep/nested.rs", &allowed));
        assert!(path_allowed("./src/main.rs", &allowed), "manifest ./ form");
        assert!(
            !path_allowed("srcb/other.rs", &allowed),
            "no prefix clobber"
        );
        assert!(!path_allowed("docs/readme.md", &allowed));
    }

    #[test]
    fn bare_dir_name_and_exact_file_forms() {
        let allowed = vec!["stats.py".to_string(), "notes".to_string()];
        assert!(path_allowed("stats.py", &allowed));
        assert!(
            !path_allowed("sub/stats.py", &allowed),
            "bare name is not a subtree"
        );
        assert!(
            path_allowed("notes/todo.txt", &allowed),
            "bare dir name acts as subtree"
        );
    }

    #[test]
    fn star_suffix_matches_any_depth() {
        let allowed = vec!["*.rs".to_string()];
        assert!(path_allowed("main.rs", &allowed));
        assert!(path_allowed("crates/core/src/lib.rs", &allowed));
        assert!(!path_allowed("crates/core/src/lib.rs.bak", &allowed));
        assert!(!path_allowed("docs/readme.md", &allowed));
    }

    #[test]
    fn violations_list_every_offending_path_sorted() {
        let d = diff(
            &["notes.md", "src/ok.rs", "config.json"],
            &["src/other.rs"],
            &["stray.txt"],
        );
        let v = violations(&d, &["src/".to_string()]);
        assert_eq!(v, vec!["config.json", "notes.md", "stray.txt"]);
    }

    #[test]
    fn deleted_in_scope_files_are_not_violations() {
        let d = diff(&[], &[], &["src/old.rs"]);
        assert!(violations(&d, &["src/".to_string()]).is_empty());
    }
}
