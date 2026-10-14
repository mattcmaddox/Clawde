//! Docker container plumbing for the container tier (spec §8, Phase 3).
//!
//! Ported from the proven eval harness (`scripts/eval/best_of_n.py`, which
//! still drives Incus) and moved from Incus to Docker on 2026-10-13 so this
//! plumbing can run in CI — GitHub runners ship Docker, not Incus. The shape
//! is unchanged: an ephemeral container per attempt, the card worktree pushed in, the agent
//! run with `bypass-permissions` (the container IS the safety boundary), the
//! verify gate run in-container, and a sha256 filesystem manifest of the
//! task dir before and after the run — whose diff is what the scope gate
//! (`scope.rs`) judges. Infrastructure failures are `Err` with the exact
//! argv surfaced, never silently folded into card data.
//!
//! The dependency-provisioning phase mirrors the gate's install-failure
//! semantics: an apt failure is an environment error (`Err`), not a card
//! failure. Docker's stock `ubuntu:24.04` is barer than the Incus cloud image
//! the lane was proven on — no CA store, no git — so `ensure_binary_deps`
//! installs its base set unconditionally (see `BASE_PACKAGES`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Container image the tier launches. The binary pushed in is the *host's*
/// debug build, so the image has to be the host's distro for its glibc link
/// to resolve; its remaining dynamic deps are apt-installed in-container (see
/// [`ensure_binary_deps`]).
pub const DEFAULT_IMAGE: &str = "ubuntu:24.04";
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
// Docker plumbing. Every call surfaces the exact argv on failure.
// ---------------------------------------------------------------------------

/// Run `program` with a real wall-clock deadline. `Command::output()` has no
/// timeout seam, so the child is spawned with piped streams, two reader
/// threads drain them (a full pipe would otherwise deadlock it), and the poll
/// loop kills the client at the deadline — an unbounded runtime call used to
/// hang the whole attempt, and with it the card's runner slot forever.
/// `not_found_hint` is the caller's advice for a missing binary.
fn run_program(
    program: &str,
    args: &[String],
    timeout: Duration,
    not_found_hint: &str,
) -> Result<(String, String, i32), String> {
    use std::io::Read;
    let argv: Vec<&str> = std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .collect();
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                not_found_hint.to_string()
            } else {
                format!("could not run `{program}`: {e}")
            }
        })?;
    let mut stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| "child stdout was not piped".to_string())?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| "child stderr was not piped".to_string())?;
    let stdout_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!("could not wait for `{}`: {e}", argv.join(" ")));
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(format!(
                "`{}` exceeded its {}s deadline and was killed",
                argv.join(" "),
                timeout.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stdout = String::from_utf8_lossy(&stdout_reader.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_reader.join().unwrap_or_default()).into_owned();
    let code = status.code().unwrap_or(-1);
    if code != 0 {
        return Err(format!(
            "`{}` exited {code}  stderr: {}",
            argv.join(" "),
            tail(&stderr, 400)
        ));
    }
    Ok((stdout, stderr, code))
}

/// `docker` with the container tier's not-found advice. Every call site is
/// deadline-bounded (see [`run_program`]).
fn run_docker(args: &[String], timeout: Duration) -> Result<(String, String, i32), String> {
    run_program(
        "docker",
        args,
        timeout,
        "`docker` binary not found on PATH — the container tier requires Docker",
    )
}

/// The last `n` characters of `text`, for the error tails the tier prints.
fn tail(text: &str, n: usize) -> String {
    let mut chars: Vec<char> = text.trim().chars().rev().take(n).collect();
    chars.reverse();
    chars.into_iter().collect()
}

/// Whether dockerd is reachable (a cheap `docker info`, deadline-bounded like
/// every other docker call — an unresponsive daemon must not wedge
/// `board runtime docker`). False = the tier is unavailable and the runner
/// must not use it.
pub fn available() -> bool {
    run_docker(&["info".to_string()], Duration::from_secs(10)).is_ok()
}

/// Launch an ephemeral container and wait until `docker exec` is ready.
///
/// `--rm` is the ephemeral half: Docker removes the container when it exits,
/// so a killed attempt cannot leave an instance behind. A stale container
/// still holding this name (a SIGKILLed run) would otherwise fail `docker run`
/// on the name alone, so it is force-removed first — free when there is
/// nothing to remove.
pub fn launch(name: &str, image: &str) -> Result<(), String> {
    stop_force(name);
    run_docker(
        &[
            "run".into(),
            "-d".into(),
            "--name".into(),
            name.to_string(),
            "--rm".into(),
            image.to_string(),
            "sleep".into(),
            "infinity".into(),
        ],
        Duration::from_secs(300),
    )?;
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if let Ok((_, _, code)) = run_docker(
            &["exec".into(), name.into(), "true".into()],
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
/// stream through `docker exec -i` (the harness's `push_home` mechanism).
pub fn push_dir(host_dir: &Path, name: &str, container_dir: &str) -> Result<(), String> {
    run_docker(
        &[
            "exec".into(),
            name.into(),
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
    let extract = Command::new("docker")
        .args(["exec", "-i", name, "tar", "-x", "-C", container_dir])
        .stdin(Stdio::from(tar_stdout))
        .output()
        .map_err(|e| format!("tar extract via docker failed: {e}"))?;
    let tar_rc = tar.wait().map_err(|e| format!("tar wait failed: {e}"))?;
    if !extract.status.success() || !tar_rc.success() {
        let err = String::from_utf8_lossy(&extract.stderr);
        return Err(format!(
            "pushing {} -> {name}:{container_dir} failed (tar rc={}, extract rc={}): {}",
            host_dir.display(),
            tar_rc.code().unwrap_or(-1),
            extract.status.code().unwrap_or(-1),
            tail(&err, 400)
        ));
    }
    Ok(())
}

/// Push one host file into the container.
///
/// `docker cp` has no `--create-dirs` (its Incus counterpart did), so the
/// destination's parent is created first — the binary lands at
/// `/usr/local/bin/clawde` and the seeded auth store at
/// `/root/.clawde/auth.json`, neither of which exists in a bare image.
/// Creating the parent also pins `docker cp`'s meaning: a destination that
/// exists as a *directory* means "copy into it", which is not what any caller
/// here wants.
pub fn push_file(host_file: &Path, name: &str, container_path: &str) -> Result<(), String> {
    let parent = container_path
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .filter(|dir| !dir.is_empty())
        .unwrap_or("/");
    run_docker(
        &[
            "exec".into(),
            name.into(),
            "mkdir".into(),
            "-p".into(),
            parent.to_string(),
        ],
        Duration::from_secs(15),
    )?;
    run_docker(
        &[
            "cp".into(),
            host_file.display().to_string(),
            format!("{name}:{container_path}"),
        ],
        Duration::from_secs(600),
    )
    .map(|_| ())
}

/// Run argv inside the container, capturing all output.
///
/// No `--` separator (Incus wanted one): Docker's CLI takes everything after
/// CONTAINER as the command, so a literal `--` would be read as the command.
pub fn exec_capture(
    name: &str,
    argv: &[String],
    timeout: Duration,
) -> Result<(String, String), String> {
    let mut args = vec!["exec".to_string(), name.to_string()];
    args.extend(argv.iter().cloned());
    let (stdout, stderr, _) = run_docker(&args, timeout)?;
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

/// Packages every attempt installs, whatever `ldd` reports.
///
/// `ca-certificates` is the one that bit live (2026-10-13, first Docker run):
/// the pushed binary builds a `reqwest` client at startup, and a bare
/// `ubuntu:24.04` ships no system trust store, so the process panicked before
/// its first request ("No CA certificates were loaded from the system"). A
/// missing *shared library* is visible to `ldd`; a missing trust store is
/// not — which is why this set is unconditional. `git` restores parity with
/// the image the lane was proven on (the Incus cloud image had it), so an
/// agent or a project check that shells out to git finds the tool.
const BASE_PACKAGES: &[&str] = &["ca-certificates", "git"];

/// The container's system trust store — what `BASE_PACKAGES` exists to
/// provide, asserted rather than assumed.
const CA_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";

/// Provision the container so the pushed binary and the agent can run in it:
/// the base packages every attempt needs plus whichever runtime libs `ldd`
/// reports missing. Returns what was installed. `Err` = the attempt's
/// environment cannot host the binary — the caller treats this as an
/// environment error, not a card failure (the gate's install-failure skip
/// semantics).
pub fn ensure_binary_deps(name: &str) -> Result<Vec<String>, String> {
    let missing = missing_shared_libs(name)?;
    let mut pkgs: Vec<String> = BASE_PACKAGES.iter().map(|p| p.to_string()).collect();
    if !missing.is_empty() {
        // The distro decides which package carries the missing .so (libasound
        // is `libasound2t64` on Ubuntu/Debian post-t64).
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
        let lib_pkgs = APT_PACKAGES_BY_DISTRO
            .iter()
            .find(|(id, _)| *id == distro)
            .map(|(_, p)| *p)
            .unwrap_or(APT_FALLBACK_PACKAGES);
        pkgs.extend(lib_pkgs.iter().map(|p| p.to_string()));
    }
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
            "binary still missing shared libs after installing {}: {} (tail: {})",
            pkgs.join(" "),
            still.join(", "),
            tail(&stderr, 160)
        ));
    }
    // Prove the trust store landed. Without it the binary panics on its first
    // HTTPS client, which would surface on the rung as an agent error instead
    // of the environment error it is.
    let (certs, _) = exec_capture(
        name,
        &[
            "bash".into(),
            "-c".into(),
            format!("test -s {CA_BUNDLE} && echo ok"),
        ],
        Duration::from_secs(15),
    )?;
    if !certs.contains("ok") {
        return Err(format!(
            "the container has no system CA bundle at {CA_BUNDLE} after installing {} — the \
             pushed binary cannot make an HTTPS request without one",
            pkgs.join(" ")
        ));
    }
    Ok(pkgs)
}

/// Pull the container task dir back as a tar byte stream (the attempt's
/// artifact, attached to the record for post-mortem).
pub fn pull_task_tar(name: &str) -> Result<Vec<u8>, String> {
    let output = Command::new("docker")
        .args(["exec", name, "tar", "-c", "-C", CONTAINER_TASK_DIR, "."])
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

/// Force-stop an ephemeral container (its teardown). Best-effort: a failed
/// stop is logged by the caller, never fatal — the eval's `finally` block.
/// `rm -f` covers both states (running and already-exited) and, together with
/// `--rm` on the run, is what removes the instance for good.
pub fn stop_force(name: &str) {
    let _ = Command::new("docker")
        .args(["rm", "-f", name])
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
/// to the charset Docker accepts. Callers append a short unique suffix.
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

/// Write the container's final task tree back into the host worktree.
///
/// The tar is the archive `pull_task_tar` took from the container's task dir,
/// which was itself pushed from this worktree — so extracting it restores the
/// agent's modified and added files. A tar cannot express a *deletion*, so the
/// paths the agent removed come from the manifest diff (`deleted`) and are
/// removed on the host explicitly.
///
/// `.git` is excluded: inside the container the worktree's `.git` is a pointer
/// file into a path that does not exist there, so the archive's copy is
/// meaningless and must never overwrite the host's real worktree metadata.
///
/// Without this the host checkout is exactly what was pushed in, `finalize`
/// diffs an untouched tree, and the card reaches review with no pinned commit.
pub fn restore_task_dir(tar: &[u8], work_dir: &Path, deleted: &[String]) -> Result<(), String> {
    use std::io::Write;
    let dir = work_dir
        .to_str()
        .ok_or_else(|| "worktree path is not valid UTF-8".to_string())?;
    let mut child = Command::new("tar")
        .args(["-x", "-C", dir, "--exclude=./.git"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start tar: {e}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "tar stdin was not piped".to_string())?;
    let payload = tar.to_vec();
    // A writer thread: tar may exit early (a truncated archive) and the write
    // then fails with EPIPE rather than blocking this thread on a full pipe.
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&payload);
    });
    let output = child
        .wait_with_output()
        .map_err(|e| format!("tar wait failed: {e}"))?;
    let _ = writer.join();
    if !output.status.success() {
        return Err(format!(
            "extracting the container task tar into {} failed (rc={}): {}",
            work_dir.display(),
            output.status.code().unwrap_or(-1),
            tail(&String::from_utf8_lossy(&output.stderr), 400)
        ));
    }
    for entry in deleted {
        // The manifest is container data: refuse absolute paths and any
        // traversal rather than deleting outside the worktree.
        let Some(relative) = entry.strip_prefix("./") else {
            continue;
        };
        let path = Path::new(relative);
        if path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            continue;
        }
        let _ = std::fs::remove_file(work_dir.join(path));
    }
    Ok(())
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
    fn container_name_is_docker_safe() {
        let name = container_name("abc123", 2);
        assert!(name.starts_with("clawde-katban-abc123-2"));
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }

    // The deadline used to be discarded (`let _ = timeout;`), so a `docker`
    // call that never returned hung the attempt — and its runner slot —
    // forever. `run_program` is the seam that made the deadline real, and it
    // is exercised here with a plain `sleep` so no Docker daemon is needed.
    #[cfg(unix)]
    #[test]
    fn run_program_kills_a_child_that_outlives_its_deadline() {
        let started = Instant::now();
        let err = run_program(
            "sleep",
            &["30".to_string()],
            Duration::from_millis(200),
            "missing",
        )
        .expect_err("a 30s child must not outlive a 200ms deadline");
        assert!(err.contains("deadline"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the child should be killed promptly, took {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_program_captures_output_and_reports_a_nonzero_exit() {
        let (out, _, code) = run_program(
            "sh",
            &["-c".to_string(), "echo hi".to_string()],
            Duration::from_secs(10),
            "missing",
        )
        .unwrap();
        assert_eq!(code, 0);
        assert_eq!(out.trim(), "hi");

        let err = run_program(
            "sh",
            &["-c".to_string(), "echo boom >&2; exit 3".to_string()],
            Duration::from_secs(10),
            "missing",
        )
        .unwrap_err();
        assert!(err.contains("exited 3"), "{err}");
        assert!(err.contains("boom"), "{err}");
    }

    /// Build the archive `pull_task_tar` would return for `dir` (the same
    /// `tar -c -C <dir> .` argv), so the restore test needs no Docker daemon.
    #[cfg(unix)]
    fn task_tar_of(dir: &Path) -> Vec<u8> {
        let out = Command::new("tar")
            .arg("-C")
            .arg(dir)
            .args(["-cf", "-", "."])
            .output()
            .expect("tar is required by the container tier");
        assert!(out.status.success(), "tar failed: {:?}", out.status);
        out.stdout
    }

    #[cfg(unix)]
    #[test]
    fn restore_task_dir_writes_the_container_tree_back() {
        let tmp = tempfile::tempdir().unwrap();
        let container = tmp.path().join("container");
        std::fs::create_dir_all(container.join("src")).unwrap();
        std::fs::write(container.join("src/modified.rs"), "agent version\n").unwrap();
        std::fs::write(container.join("src/added.rs"), "brand new\n").unwrap();
        std::fs::write(container.join(".git"), "container pointer\n").unwrap();

        // The host worktree as it was pushed in: the old file, a file the
        // agent deleted, and the real worktree git metadata.
        let work = tmp.path().join("work");
        std::fs::create_dir_all(work.join("src")).unwrap();
        std::fs::write(work.join("src/modified.rs"), "old version\n").unwrap();
        std::fs::write(work.join("src/gone.rs"), "deleted by the agent\n").unwrap();
        std::fs::write(work.join(".git"), "gitdir: /repo/.git/worktrees/card\n").unwrap();

        let tar = task_tar_of(&container);
        restore_task_dir(&tar, &work, &["./src/gone.rs".to_string()])
            .expect("restore should succeed");

        assert_eq!(
            std::fs::read_to_string(work.join("src/modified.rs")).unwrap(),
            "agent version\n",
            "the agent's edit must land on the host"
        );
        assert_eq!(
            std::fs::read_to_string(work.join("src/added.rs")).unwrap(),
            "brand new\n"
        );
        assert!(
            !work.join("src/gone.rs").exists(),
            "a manifest-deleted path must be removed from the worktree"
        );
        assert_eq!(
            std::fs::read_to_string(work.join(".git")).unwrap(),
            "gitdir: /repo/.git/worktrees/card\n",
            "the host worktree metadata must never be overwritten"
        );
    }

    #[cfg(unix)]
    #[test]
    fn restore_task_dir_refuses_to_delete_outside_the_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let container = tmp.path().join("container");
        std::fs::create_dir_all(&container).unwrap();
        std::fs::write(container.join("keep.txt"), "hi\n").unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let outside = tmp.path().join("outside.txt");
        std::fs::write(&outside, "must survive\n").unwrap();

        let tar = task_tar_of(&container);
        restore_task_dir(
            &tar,
            &work,
            &["../outside.txt".to_string(), "/etc/hosts".to_string()],
        )
        .unwrap();
        assert!(
            outside.exists(),
            "a traversal entry must not delete outside the worktree"
        );
    }
}
