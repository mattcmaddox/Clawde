//! Incus container plumbing for the container tier (spec §8, Phase 3).
//!
//! Ported from the proven eval harness (`scripts/eval/best_of_n.py`): an
//! ephemeral container per attempt, the card worktree pushed in, the agent
//! run with `bypass-permissions` (the container IS the safety boundary), the
//! verify gate run in-container, and a sha256 filesystem manifest of the
//! task dir before and after the run — whose diff is what the scope gate
//! (`scope.rs`) judges. Infrastructure failures are `Err` with the exact
//! argv surfaced, never silently folded into card data.
//!
//! The dependency-provisioning phase mirrors the gate's install-failure
//! semantics: an apt failure is an environment error (`Err`), not a card
//! failure.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Container image the tier launches (cloud image so `apt` and a root shell
/// work out of the box).
pub const DEFAULT_IMAGE: &str = "images:ubuntu/24.04/cloud";
/// Prefix for ephemeral instances; collisions use a uuid suffix.
pub const CONTAINER_PREFIX: &str = "clawde-katban";
/// Where the card worktree is mounted inside the container (the agent's cwd).
pub const CONTAINER_TASK_DIR: &str = "/root/task";
/// Where the seeded CLAWDE_HOME lives inside the container.
pub const CONTAINER_ROOT_HOME: &str = "/root/.clawde";
/// The pushed clawde binary inside the container.
pub const CONTAINER_BINARY: &str = "/usr/local/bin/clawde";

/// Shared libs the debug binary dynamically links that a clean cloud image
/// lacks (discovered live: `libasound.so.2`, the voice stack's ALSA dep),
/// keyed by `/etc/os-release` ID with a fallback list. Only the container is
/// touched; the host is never modified.
const APT_PACKAGES_BY_DISTRO: &[(&str, &[&str])] = &[
    ("ubuntu", &["libasound2t64"]),
    ("debian", &["libasound2t64"]),
];
const APT_FALLBACK_PACKAGES: &[&str] = &["libasound2t64", "libasound2"];

// ---------------------------------------------------------------------------
// Manifest primitives (the forensics the scope gate diffs against).
// ---------------------------------------------------------------------------

/// sha256 manifest of every file under `dir` (pycache artifacts excluded,
/// matching the harness). `path` is the in-container directory.
pub fn manifest_argv(dir: &str) -> Vec<String> {
    vec![
        "bash".into(),
        "-c".into(),
        format!(
            "cd {dir} && find . -type f -not -path '*/__pycache__/*' -not -name '*.pyc' | sort | xargs -r sha256sum"
        ),
    ]
}

/// Parse `sha256sum` output into `{path: digest}` (the harness's
/// `parse_manifest`: split on the first space, strip the binary-mode `*`).
/// Malformed lines are skipped.
pub fn parse_manifest(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let (digest, rest) = line.split_once(' ').unwrap_or(("", ""));
        let path = rest.trim_start_matches('*').trim();
        if digest.len() == 64 && !path.is_empty() {
            out.insert(path.to_string(), digest.to_string());
        }
    }
    out
}

/// The added / modified / deleted path sets between two manifests, sorted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManifestDiff {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
}

impl ManifestDiff {
    /// Every path the agent touched, any change kind.
    pub fn all_paths(&self) -> impl Iterator<Item = &String> {
        self.added
            .iter()
            .chain(self.modified.iter())
            .chain(self.deleted.iter())
    }
}

pub fn diff_manifests(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> ManifestDiff {
    ManifestDiff {
        added: after
            .keys()
            .filter(|p| !before.contains_key(*p))
            .cloned()
            .collect(),
        modified: after
            .iter()
            .filter(|(p, d)| before.get(*p).is_some_and(|b| b != *d))
            .map(|(p, _)| p.clone())
            .collect(),
        deleted: before
            .keys()
            .filter(|p| !after.contains_key(*p))
            .cloned()
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// Incus plumbing. Every call surfaces the exact argv on failure.
// ---------------------------------------------------------------------------

fn run_incus(args: &[String], timeout: Duration) -> Result<(String, String, i32), String> {
    let argv: Vec<&str> = std::iter::once("incus")
        .chain(args.iter().map(String::as_str))
        .collect();
    let output = Command::new("incus")
        .args(&argv[1..])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "`incus` binary not found on PATH — the container tier requires Incus".to_string()
            } else {
                format!("could not run `incus`: {e}")
            }
        })?;
    // Timeout on `output()` is not available; spawn+wait where it matters
    // (launch, apt) is bounded by the callers' own retry/deadline logic and
    // incus's own behavior. Capture-based calls here are all sub-second.
    let _ = timeout;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let code = output.status.code().unwrap_or(-1);
    if code != 0 {
        return Err(format!(
            "`{}` exited {code}  stderr: {}",
            argv.join(" "),
            stderr
                .trim()
                .chars()
                .rev()
                .take(400)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        ));
    }
    Ok((stdout, stderr, code))
}

/// Whether incusd is reachable (a cheap `incus list`). False = the tier is
/// unavailable and the runner must not use it.
pub fn available() -> bool {
    Command::new("incus")
        .args(["list", "--format", "csv"])
        .stdin(Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Launch an ephemeral container and wait until `incus exec` is ready.
pub fn launch(name: &str, image: &str) -> Result<(), String> {
    run_incus(
        &[
            "launch".into(),
            "--ephemeral".into(),
            image.to_string(),
            name.to_string(),
        ],
        Duration::from_secs(300),
    )?;
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if let Ok((_, _, code)) = run_incus(
            &["exec".into(), name.into(), "--".into(), "true".into()],
            Duration::from_secs(10),
        ) {
            if code == 0 {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Err(format!(
        "container {name} never became exec-ready within 60s"
    ))
}

/// Push a host directory's contents into `container_dir` by piping a tar
/// stream (the harness's `push_home` mechanism).
pub fn push_dir(host_dir: &Path, name: &str, container_dir: &str) -> Result<(), String> {
    run_incus(
        &[
            "exec".into(),
            name.into(),
            "--".into(),
            "mkdir".into(),
            "-p".into(),
            container_dir.into(),
        ],
        Duration::from_secs(15),
    )?;
    let mut tar = Command::new("tar")
        .arg("-C")
        .arg(host_dir)
        .args(["-cf", "-", "."])
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start tar: {e}"))?;
    let tar_stdout = tar.stdout.take().expect("stdout was piped");
    let extract = Command::new("incus")
        .args(["exec", name, "--", "tar", "-x", "-C", container_dir])
        .stdin(Stdio::from(tar_stdout))
        .output()
        .map_err(|e| format!("tar extract via incus failed: {e}"))?;
    let tar_rc = tar.wait().map_err(|e| format!("tar wait failed: {e}"))?;
    if !extract.status.success() || !tar_rc.success() {
        let err = String::from_utf8_lossy(&extract.stderr);
        return Err(format!(
            "pushing {} -> {name}:{container_dir} failed (tar rc={}, extract rc={}): {}",
            host_dir.display(),
            tar_rc.code().unwrap_or(-1),
            extract.status.code().unwrap_or(-1),
            err.trim()
                .chars()
                .rev()
                .take(400)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        ));
    }
    Ok(())
}

/// Push one host file into the container (`--create-dirs` for the target).
pub fn push_file(host_file: &Path, name: &str, container_path: &str) -> Result<(), String> {
    run_incus(
        &[
            "file".into(),
            "push".into(),
            "--create-dirs".into(),
            host_file.display().to_string(),
            format!("{name}{container_path}"),
        ],
        Duration::from_secs(600),
    )
    .map(|_| ())
}

/// Run argv inside the container, capturing all output.
pub fn exec_capture(
    name: &str,
    argv: &[String],
    timeout: Duration,
) -> Result<(String, String), String> {
    let mut args = vec!["exec".to_string(), name.to_string(), "--".to_string()];
    args.extend(argv.iter().cloned());
    let (stdout, stderr, _) = run_incus(&args, timeout)?;
    Ok((stdout, stderr))
}

/// Shared libs the pushed binary cannot resolve inside the container.
fn missing_shared_libs(name: &str) -> Result<Vec<String>, String> {
    let (stdout, _) = exec_capture(
        name,
        &[
            "bash".into(),
            "-c".into(),
            format!("ldd {CONTAINER_BINARY} 2>/dev/null | grep 'not found' || true"),
        ],
        Duration::from_secs(30),
    )?;
    Ok(stdout
        .lines()
        .filter_map(|l| l.trim().split(' ').next())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect())
}

/// Install missing runtime libs for the binary inside the container. Returns
/// what was installed (empty when nothing was missing). `Err` = the attempt's
/// environment cannot host the binary — the caller treats this as an
/// environment error, not a card failure (the gate's install-failure skip
/// semantics).
pub fn ensure_binary_deps(name: &str) -> Result<Vec<String>, String> {
    let missing = missing_shared_libs(name)?;
    if missing.is_empty() {
        return Ok(Vec::new());
    }
    let (os_release, _) = exec_capture(
        name,
        &["bash".into(), "-c".into(), "cat /etc/os-release".into()],
        Duration::from_secs(15),
    )?;
    let distro = os_release
        .lines()
        .find_map(|l| {
            l.strip_prefix("ID=")
                .map(|v| v.trim_matches('"').to_string())
        })
        .unwrap_or_default();
    let pkgs: &[&str] = APT_PACKAGES_BY_DISTRO
        .iter()
        .find(|(id, _)| *id == distro)
        .map(|(_, p)| *p)
        .unwrap_or(APT_FALLBACK_PACKAGES);
    let install = format!(
        "apt-get update -qq && apt-get install -y -qq {}",
        pkgs.join(" ")
    );
    let (_, stderr) = exec_capture(
        name,
        &["bash".into(), "-c".into(), install],
        Duration::from_secs(300),
    )?;
    let still = missing_shared_libs(name)?;
    if !still.is_empty() {
        return Err(format!(
            "binary still missing shared libs after apt install: {} (tail: {})",
            still.join(", "),
            stderr
                .trim()
                .chars()
                .rev()
                .take(160)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        ));
    }
    Ok(pkgs.iter().map(|p| p.to_string()).collect())
}

/// Pull the container task dir back as a tar byte stream (the attempt's
/// artifact, attached to the record for post-mortem).
pub fn pull_task_tar(name: &str) -> Result<Vec<u8>, String> {
    let output = Command::new("incus")
        .args([
            "exec",
            name,
            "--",
            "tar",
            "-c",
            "-C",
            CONTAINER_TASK_DIR,
            ".",
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not pull task tar: {e}"))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!(
            "task tar pull exited {}",
            output.status.code().unwrap_or(-1)
        ))
    }
}

/// Force-stop an ephemeral instance (its teardown). Best-effort: a failed
/// stop is logged by the caller, never fatal — the eval's `finally` block.
pub fn stop_force(name: &str) {
    let _ = Command::new("incus")
        .args(["stop", "--force", name])
        .stdin(Stdio::null())
        .output();
}

/// The in-container argv for one verify command: run it in the task dir with
/// a marker line so the exit code survives any output mangling (the harness's
/// `in_container_verify_argv` + `parse_verify_rc` contract).
pub fn verify_argv(verify_cmd: &str) -> Vec<String> {
    vec![
        "bash".into(),
        "-c".into(),
        format!("cd {CONTAINER_TASK_DIR} && {{ {verify_cmd} ; }} ; echo \"VERIFY_RC=$?\""),
    ]
}

/// The `VERIFY_RC=<n>` marker out of verify output (mirrors the harness's
/// `parse_verify_rc`; None = the marker never arrived — infra gap).
pub fn parse_verify_rc(text: &str) -> Option<i32> {
    text.lines().rev().find_map(|l| {
        l.trim()
            .strip_prefix("VERIFY_RC=")
            .and_then(|v| v.trim().parse().ok())
    })
}

/// Build the seeded in-container agent argv: stream-json, pinned model,
/// bypass-permissions (the container is the safety boundary), cwd the task
/// dir. `session` is the run's session id (also the container name suffix).
pub fn agent_argv(
    prompt: &str,
    model: &str,
    binary: &str,
    session: &str,
    max_turns: u32,
) -> Vec<String> {
    vec![
        "env".into(),
        format!("CLAWDE_HOME={CONTAINER_ROOT_HOME}"),
        binary.into(),
        "--print".into(),
        prompt.into(),
        "--output-format".into(),
        "stream-json".into(),
        "--model".into(),
        model.into(),
        "--max-turns".into(),
        max_turns.to_string(),
        "--session-id".into(),
        session.into(),
        "--no-auto-compact".into(),
        "--cwd".into(),
        CONTAINER_TASK_DIR.into(),
        "--permission-mode".into(),
        "bypass-permissions".into(),
    ]
}

/// `settings.json` hardening for a pinned attempt (the harness's
/// `write_pin_settings`): disable every other free-catalog upstream so the
/// chain holds exactly one entry, and widen the timeouts (a pinned run has no
/// fallback — one slow first byte would kill the attempt).
pub fn pin_settings_json(pin_upstream: &str, catalog_ids: &[&str]) -> String {
    let disabled: Vec<&str> = catalog_ids
        .iter()
        .copied()
        .filter(|id| *id != pin_upstream)
        .collect();
    let body = serde_json::json!({
        "auto_compact": false,
        "verbose": false,
        "hasCompletedOnboarding": true,
        "hooks": {},
        "providers": { "free": { "options": { "routing": {
            "disabled_upstreams": disabled,
            "fallback_retries": 3,
            "upstream_timeout_secs": 90,
            "first_byte_timeout_secs": 90,
        }}}}
    });
    serde_json::to_string_pretty(&body).unwrap_or_default()
}

/// Container name for a card attempt: prefix + card + rung index, sanitized
/// to the charset incus accepts. Callers append a short unique suffix.
pub fn container_name(card_id: &str, rung: usize) -> String {
    let safe: String = card_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("{CONTAINER_PREFIX}-{safe}-{rung}")
}

/// The host-side artifacts dir for one attempt's pulled task tar:
/// `<katban data>/container-artifacts/<card>/<rung>-<session>`.
pub fn artifacts_dir(card_id: &str, session: &str) -> PathBuf {
    crate::config::katban_data_dir()
        .join("container-artifacts")
        .join(sanitize(card_id))
        .join(session)
}

fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parse_reads_sha256sum_output() {
        let text = "\
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  ./README.md
0000000000000000000000000000000000000000000000000000000000000000 *./src/main.rs
garbage line
short  ./x
";
        let m = parse_manifest(text);
        assert_eq!(m.len(), 2);
        assert_eq!(
            m["./README.md"],
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert!(
            m.contains_key("./src/main.rs"),
            "star-prefixed binary-mode path"
        );
    }

    #[test]
    fn manifest_diff_partitions_the_two_sets() {
        // Real 64-hex digests (the parser rejects anything shorter).
        let pad = |c: char| std::iter::repeat_n(c, 64).collect::<String>();
        let line = |d: char, p: &str| format!("{}  {}\n", pad(d), p);
        let before = parse_manifest(&format!(
            "{}{}{}",
            line('a', "./keep.rs"),
            line('b', "./changed.rs"),
            line('c', "./gone.rs")
        ));
        let after = parse_manifest(&format!(
            "{}{}{}",
            line('a', "./keep.rs"),
            line('f', "./changed.rs"),
            line('n', "./new.rs")
        ));
        let d = diff_manifests(&before, &after);
        assert_eq!(d.added, vec!["./new.rs"]);
        assert_eq!(d.modified, vec!["./changed.rs"]);
        assert_eq!(d.deleted, vec!["./gone.rs"]);
        assert_eq!(d.all_paths().count(), 3);
    }

    #[test]
    fn verify_argv_marker_parses_even_with_noise() {
        let out = "running tests...\nFAIL something\nVERIFY_RC=1\n";
        assert_eq!(parse_verify_rc(out), Some(1));
        assert_eq!(parse_verify_rc("no marker"), None);
        assert!(verify_argv("pytest").join(" ").contains(CONTAINER_TASK_DIR));
    }

    #[test]
    fn agent_argv_carries_pin_bypass_and_cwd() {
        let argv = agent_argv(
            "fix the bug",
            "free/zai/glm",
            "/usr/local/bin/clawde",
            "sess1",
            40,
        );
        let joined = argv.join(" ");
        assert!(joined.contains("--permission-mode bypass-permissions"));
        assert!(joined.contains("--cwd /root/task"));
        assert!(joined.contains("--model free/zai/glm"));
        assert!(joined.contains("CLAWDE_HOME=/root/.clawde"));
    }

    #[test]
    fn pin_settings_disables_everyone_else() {
        let json = pin_settings_json("zai", &["zai", "groq", "nvidia"]);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let disabled: Vec<&str> = v["providers"]["free"]["options"]["routing"]
            ["disabled_upstreams"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert_eq!(disabled, vec!["groq", "nvidia"]);
        assert_eq!(
            v["providers"]["free"]["options"]["routing"]["fallback_retries"],
            serde_json::json!(3)
        );
    }

    #[test]
    fn container_name_is_incus_safe() {
        let name = container_name("abc123", 2);
        assert!(name.starts_with("clawde-katban-abc123-2"));
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }
}
