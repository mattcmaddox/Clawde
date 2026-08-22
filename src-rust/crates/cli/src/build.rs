// build.rs — `clawde build` subcommand.
//
// Rebuilds clawde from the local source checkout and replaces the running
// binary in place — the "rebuild and update $>clawde" workflow as one short
// command. Works wherever the source tree still lives at the path this binary
// was compiled from (the dev machine). On machines without the source, use
// `clawde upgrade` to update from GitHub releases instead.
//
// Accepts both `clawde build` and `clawde --build` (intercepted pre-clap in
// main.rs, same as `clawde upgrade`).

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};

struct BuildOptions {
    debug: bool,
    target: Option<String>,
    install: bool,
}

pub async fn run_build(args: &[String]) -> Result<()> {
    // -------- arg parsing --------
    let mut debug = false;
    let mut install = true;
    let mut target: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_help();
                return Ok(());
            }
            "--debug" => debug = true,
            "--no-install" => install = false,
            "-t" | "--target" => {
                target = iter.next().cloned();
                if target.is_none() {
                    bail!("--target requires an argument");
                }
            }
            unknown => bail!("Unknown option: {}", unknown),
        }
    }
    let opts = BuildOptions {
        debug,
        target,
        install,
    };

    // -------- locate source checkout --------
    let workspace = workspace_root()?;
    run_cargo_build(&workspace, &opts)?;

    // -------- locate freshly built binary --------
    let built = built_binary_path(&workspace, &opts);
    if !built.is_file() {
        bail!(
            "build finished but binary not found at {}\n\
             Did you pass --target without a matching installed rust target?",
            built.display()
        );
    }

    if !opts.install {
        println!(":: Built: {}", built.display());
        return Ok(());
    }

    // -------- replace the running binary --------
    let exe_path =
        std::env::current_exe().context("could not determine current executable path")?;
    let exe_path = std::fs::canonicalize(&exe_path).unwrap_or(exe_path);
    println!(":: Replacing {}", exe_path.display());
    crate::upgrade::swap_binary(&exe_path, &built)?;

    // Confirm the swap by asking the new binary for its version.
    if let Ok(out) = std::process::Command::new(&exe_path)
        .arg("--version")
        .output()
    {
        if let Ok(s) = String::from_utf8(out.stdout) {
            let version = s.trim();
            if !version.is_empty() {
                println!(":: Now running {}", version);
            }
        }
    }

    println!("Done — `clawde` from any directory is up to date.");
    Ok(())
}

fn print_help() {
    println!(
        "Usage: clawde build [options]\n\n\
         Options:\n\
           --debug           Build a debug binary instead of release (faster)\n\
           --no-install      Build only — do not replace the running binary\n\
           -t, --target <t>  Cross-compile for a rust target triple\n\
           -h, --help        Show this help\n\n\
         Rebuilds clawde from the local source checkout and replaces the\n\
         running binary in place. Requires the source tree at the location\n\
         this binary was compiled from. On machines without the source, use\n\
         `clawde upgrade` instead."
    );
}

// ---------------------------------------------------------------------------
// Source discovery
// ---------------------------------------------------------------------------

fn workspace_root() -> Result<PathBuf> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    workspace_root_from(manifest_dir)
}

fn workspace_root_from(manifest_dir: &Path) -> Result<PathBuf> {
    let workspace = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| {
            anyhow!(
                "cannot determine workspace root from {}",
                manifest_dir.display()
            )
        })?;
    if !workspace.join("Cargo.toml").is_file() {
        bail!(
            "source checkout not found at {}.\n\
             This binary was compiled from a different location, so `clawde build`\n\
             can only rebuild where the source tree lives. On machines without\n\
             the source, use `clawde upgrade` instead.",
            workspace.display()
        );
    }
    Ok(workspace.to_path_buf())
}

// ---------------------------------------------------------------------------
// Build
// ---------------------------------------------------------------------------

fn run_cargo_build(workspace: &Path, opts: &BuildOptions) -> Result<()> {
    let profile = if opts.debug { "debug" } else { "release" };
    println!(
        ":: Building clawde ({}) in {}",
        profile,
        workspace.display()
    );
    let mut cmd = std::process::Command::new("cargo");
    cmd.arg("build").arg("--package").arg("clawde-cli");
    if !opts.debug {
        cmd.arg("--release");
    }
    if let Some(t) = &opts.target {
        cmd.arg("--target").arg(t);
    }
    cmd.current_dir(workspace);
    let status = cmd.status().context("failed to spawn cargo")?;
    if !status.success() {
        bail!("cargo build failed with exit status {}", status);
    }
    Ok(())
}

fn built_binary_path(workspace: &Path, opts: &BuildOptions) -> PathBuf {
    let profile = if opts.debug { "debug" } else { "release" };
    let mut path = workspace.join("target");
    if let Some(t) = &opts.target {
        path = path.join(t);
    }
    path = path.join(profile);
    if cfg!(target_os = "windows") {
        path.join("clawde.exe")
    } else {
        path.join("clawde")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn workspace_root_rejects_missing_source() {
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let tmp = std::env::temp_dir().join(format!(
            "clawde-build-test-{}-{}",
            std::process::id(),
            unique
        ));
        let fake_manifest = tmp.join("src-rust").join("crates").join("cli");
        let err = workspace_root_from(&fake_manifest).unwrap_err();
        assert!(
            err.to_string().contains("source checkout not found"),
            "unexpected error: {err}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn built_binary_path_uses_profile_and_target() {
        let workspace = PathBuf::from("/tmp/clawde-ws");
        let release = BuildOptions {
            debug: false,
            target: None,
            install: true,
        };
        let p = built_binary_path(&workspace, &release);
        if cfg!(target_os = "windows") {
            assert_eq!(
                p,
                workspace.join("target").join("release").join("clawde.exe")
            );
        } else {
            assert_eq!(p, workspace.join("target").join("release").join("clawde"));
        }

        let cross = BuildOptions {
            debug: true,
            target: Some("aarch64-unknown-linux-gnu".to_string()),
            install: false,
        };
        let p = built_binary_path(&workspace, &cross);
        assert!(p.to_string_lossy().contains("aarch64-unknown-linux-gnu"));
        assert!(p.to_string_lossy().contains("debug"));
    }
}
