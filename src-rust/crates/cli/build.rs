//! Build script for Clawde CLI
//!
//! Embeds build-time metadata (version, timestamp, git info) into the binary
//! for display and debugging purposes.

use std::process::Command;

fn main() {
    // Embed build timestamp (RFC 3339 format)
    let now = chrono::Utc::now().to_rfc3339();
    println!("cargo:rustc-env=BUILD_TIME={}", now);

    // Embed short git commit hash
    let commit = get_git_commit().unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GIT_COMMIT={}", commit);

    // SemVer build-number metadata: the repo's total commit count. Local
    // builds embed this as `+N` so every build is uniquely numbered while the
    // source version stays a clean X.Y.Z (only moved on release). This is
    // informational build metadata — it never affects version precedence and
    // never resets the patch number.
    let build_number = get_commit_count().unwrap_or_default();
    println!("cargo:rustc-env=CLAWDE_BUILD_NUMBER={}", build_number);

    // Package/distribution metadata
    println!("cargo:rustc-env=PACKAGE_URL=clawde-source-snapshot");
    println!("cargo:rustc-env=FEEDBACK_CHANNEL=github");
    println!("cargo:rustc-env=ISSUES_EXPLAINER=This build does not include Anthropic internal issue routing.");

    // Trigger rebuild if git HEAD changes
    println!("cargo:rerun-if-changed=.git/HEAD");
}

/// Get the total commit count on the current branch (used as the +N build
/// metadata suffix), or None if git is not available.
fn get_commit_count() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .ok()?;

    if output.status.success() {
        let count = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !count.is_empty() {
            return Some(count);
        }
    }

    None
}

/// Get the short git commit hash, or None if git is not available.
fn get_git_commit() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;

    if output.status.success() {
        let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !commit.is_empty() {
            return Some(commit);
        }
    }

    None
}
