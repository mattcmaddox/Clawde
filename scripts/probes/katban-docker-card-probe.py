#!/usr/bin/env python3
"""Katban Docker container tier — live end-to-end probe (manual, not CI).

The container tier moves a card's `attempts: N` ladder into ephemeral Docker
containers (`board runtime docker`). Nothing in `cargo test` can cover the
happy path: it needs a reachable Docker daemon, an image pull, and (for a full
card) live free-provider keys, so it belongs here with the other live probes.
The plumbing phase is the exception — it needs no keys and CI runs it:

    python3 scripts/probes/katban-docker-card-probe.py                # all three phases
    python3 scripts/probes/katban-docker-card-probe.py --check        # prerequisites only
    python3 scripts/probes/katban-docker-card-probe.py --plumbing     # docker tar round-trip, no keys
    python3 scripts/probes/katban-docker-card-probe.py --card         # full card (needs keys)

Phases and what each one is for:

  check     Docker daemon reachable, debug binary present, git present, and
            (for --card) a readable auth store. No side effects.
  plumbing  One ephemeral container: push a directory in, mutate it inside,
            pull the task tar back, and extract it on the host. This is the
            mechanism `container::restore_task_dir` depends on — that the tar
            taken by `docker exec ... tar -c -C /root/task .` extracts locally,
            and that `./.git` IS a member of it (which is why the restore
            excludes it) — and a tar cannot express a deletion, so the
            manifest-diff deletion list is required. No keys, no agent, and no
            auth store: it runs under `--no-seed-auth` on a bare CI runner.
  card      A scratch git repo, a scratch CLAWDE_HOME seeded with the real auth
            store, `board attempts 2` + `board runtime docker`, one trivial card
            driven by the scheduler to completion. Asserts the card reaches
            review with a pinned commit whose tree contains the agent's file,
            that the artifact tar exists, and that no container is left behind.

Exit codes: 0 pass, 1 assertion failed, 2 prerequisites missing (the phase
never ran). Requires a dev box: `docker info` must succeed.

Cleanup: the card phase starts the scheduler in its own session and stops it in
a `finally`, so Ctrl-C is safe. A SIGKILL of the probe (or of the terminal
running it) skips that, leaving the scheduler and possibly a container behind;
`--check` warns about a stray probe scheduler, and the leftovers are removed
explicitly with:

    pkill -f 'clawde katban board run --project probe'
    docker ps -a --format '{{.Names}}' | grep '^clawde-katban' | xargs -r docker rm -f
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_BINARY = REPO_ROOT / "src-rust" / "target" / "debug" / "clawde"
# Mirrors container::DEFAULT_IMAGE — the same distro as the pushed debug
# binary, so its glibc link resolves without a matching-image dance (the
# tier's apt provisioning covers the other shared libs).
DEFAULT_IMAGE = "ubuntu:24.04"
CONTAINER_PREFIX = "clawde-katban"
PROBE_PROJECT = "probe"
CARD_PROMPT = (
    "Create a file named probe.txt in the current directory containing exactly "
    "the text hello-from-container. Do not create or change anything else."
)
PROBE_FILE_BODY = "hello-from-container"
# The host worktree's `.git` is a pointer file. The probe gives the container's
# copy a different body so the restore's `--exclude=./.git` is load-bearing.
HOST_GIT_POINTER = "gitdir: /repo/.git/worktrees/card"
CONTAINER_GIT_POINTER = "container-pointer"


class PrereqError(Exception):
    """A prerequisite is missing: the phase did not run (exit 2)."""


def run(argv, **kwargs):
    """Run argv, returning CompletedProcess; never raises on a non-zero exit."""
    return subprocess.run(argv, capture_output=True, **kwargs)


def run_text(argv, **kwargs):
    proc = run(argv, text=True, **kwargs)
    return proc.returncode, (proc.stdout or "").strip(), (proc.stderr or "").strip()


def section(title):
    print(f"\n== {title}")


def ok(message):
    print(f"[ok]   {message}")


def fail(message):
    print(f"[FAIL] {message}")
    return 1


def require(condition, message):
    if not condition:
        raise PrereqError(message)


def stray_schedulers():
    """Scheduler processes for the probe's own project. The card phase starts
    `board run` in its own session, so a probe that is killed outright (SIGKILL)
    cannot reap it."""
    try:
        found = subprocess.run(
            ["pgrep", "-af", f"clawde katban board run --project {PROBE_PROJECT}"],
            capture_output=True,
            text=True,
        ).stdout
    except OSError:
        return []
    return [line for line in found.splitlines() if line.strip()]


def stop_scheduler(proc):
    """Terminate the scheduler's whole process group and wait for it, so the
    containers it owns are torn down before the probe asserts on leftovers."""
    if proc is None or proc.poll() is not None:
        return
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(os.getpgid(proc.pid), sig)
        except (ProcessLookupError, PermissionError):
            return
        try:
            proc.wait(timeout=30)
            return
        except subprocess.TimeoutExpired:
            continue


def containers_including(prefix):
    """Names of containers (running or stopped) whose name starts with prefix."""
    code, out, _ = run_text(["docker", "ps", "-a", "--format", "{{.Names}}"])
    if code != 0:
        return []
    return [name.strip() for name in out.splitlines() if name.strip().startswith(prefix)]


def launch(name, image):
    """Start a detached, ephemeral container that stays alive for `docker exec`.

    A stale container with this name — a previous run killed with SIGKILL —
    would otherwise fail `docker run` on the name alone, so it is removed first
    (free when absent). `--rm` is the tier's ephemeral teardown.
    """
    run(["docker", "rm", "-f", name])
    code, _, err = run_text(
        [
            "docker",
            "run",
            "-d",
            "--name",
            name,
            "--rm",
            image,
            "sleep",
            "infinity",
        ],
        timeout=600,
    )
    if code != 0:
        raise AssertionError(f"docker run failed: {err}")
    # Wait for exec readiness.
    for _ in range(60):
        if run_text(["docker", "exec", name, "true"])[0] == 0:
            return
        time.sleep(1)
    raise AssertionError("container never became exec-ready within 60s")


def push_dir(host_dir, name, container_dir):
    mk = run(["docker", "exec", name, "mkdir", "-p", container_dir])
    if mk.returncode != 0:
        raise AssertionError(f"mkdir in container failed: {mk.stderr.decode()}")
    tar = subprocess.Popen(
        ["tar", "-C", str(host_dir), "-cf", "-", "."], stdout=subprocess.PIPE
    )
    extract = run(
        ["docker", "exec", "-i", name, "tar", "-x", "-C", container_dir],
        stdin=tar.stdout,
    )
    tar.stdout.close()
    tar.wait()
    if extract.returncode != 0:
        raise AssertionError(f"push into container failed: {extract.stderr.decode()}")


def push_file(host_file, name, container_path):
    parent = str(Path(container_path).parent)
    mk = run(["docker", "exec", name, "mkdir", "-p", parent])
    if mk.returncode != 0:
        raise AssertionError(f"mkdir for {parent} failed: {mk.stderr.decode()}")
    code, _, err = run_text(
        ["docker", "cp", str(host_file), f"{name}:{container_path}"], timeout=600
    )
    if code != 0:
        raise AssertionError(f"docker cp {host_file} -> {name}:{container_path} failed: {err}")


def pull_task_tar(name):
    proc = run(["docker", "exec", name, "tar", "-c", "-C", "/root/task", "."])
    if proc.returncode != 0:
        raise AssertionError(f"task tar pull failed: {proc.stderr.decode()}")
    return proc.stdout


# ---------------------------------------------------------------------------
# check
# ---------------------------------------------------------------------------


def phase_check(args):
    section("check: prerequisites")
    require(shutil.which("docker"), "`docker` is not on PATH")
    code, _, err = run_text(["docker", "info"])
    require(
        code == 0,
        f"`docker info` failed (is dockerd running?) — {err or 'no output'}",
    )
    ok("Docker daemon reachable")
    require(
        args.binary.exists(),
        f"no debug binary at {args.binary} — run `cargo build -p clawde-cli`",
    )
    ok(f"binary present: {args.binary}")
    require(shutil.which("git"), "`git` is not on PATH")
    require(shutil.which("tar"), "`tar` is not on PATH")
    ok("git and tar present")
    if args.needs_auth:
        require(
            args.auth_json.exists(),
            f"no auth store at {args.auth_json} — the card's rungs need free keys",
        )
        ok(f"auth store present: {args.auth_json}")
    strays = stray_schedulers()
    if strays:
        print(
            f"[warn] {len(strays)} probe scheduler(s) still running from an earlier "
            f"run — stop with: pkill -f 'clawde katban board run --project {PROBE_PROJECT}'"
        )
    return 0


# ---------------------------------------------------------------------------
# plumbing
# ---------------------------------------------------------------------------


def phase_plumbing(args):
    section("plumbing: docker tar round-trip (the restore mechanism)")
    name = f"{CONTAINER_PREFIX}-probe-{os.getpid()}-plumbing"
    if containers_including(name):
        run(["docker", "rm", "-f", name])
    work = Path(tempfile.mkdtemp(prefix="katban-probe-work-"))
    # Scratch this phase creates beyond `work`, removed in the same `finally`
    # (a passing phase used to leave its restore target behind on every run).
    extra_scratch = []
    try:
        (work / "src").mkdir()
        (work / "src" / "keep.txt").write_text("original\n")
        (work / "src" / "gone.txt").write_text("delete me\n")
        # A worktree's `.git` is a pointer file; the probe pushes one so the
        # archive's membership can be asserted below.
        (work / ".git").write_text(HOST_GIT_POINTER + "\n")

        print(f"launching {name} ({args.image}) ...")
        launch(name, args.image)
        ok("container is exec-ready")

        push_dir(work, name, "/root/task")
        ok("pushed the pinned tree into /root/task")

        # The tier's other push path: `docker cp` for single files (the debug
        # binary, the seeded `auth.json`) into a directory that does not exist
        # yet. incus created it with `--create-dirs`; docker needs the parent
        # made first, and the destination is a *path*, not a directory, when it
        # does not exist — both are exercised here because getting either wrong
        # breaks every rung before the agent starts.
        seeded = work / "settings.json"
        seeded.write_text('{"probe": true}\n')
        push_file(seeded, name, "/root/.clawde/settings.json")
        code, out, _ = run_text(["docker", "exec", name, "cat", "/root/.clawde/settings.json"])
        if code != 0 or out.strip() != '{"probe": true}':
            return fail(
                f"`docker cp` into a missing directory did not land the file at its "
                f"path (exit={code}, content={out.strip()!r})"
            )
        ok("pushed a single file into a not-yet-existing directory (docker cp)")

        # What an agent does inside: modify one file, add another, delete a
        # third, and (as a container's worktree pointer does) rewrite `.git`.
        run(
            [
                "docker",
                "exec",
                name,
                "bash",
                "-c",
                "cd /root/task && echo agent-version > src/keep.txt && "
                "echo brand-new > src/added.txt && rm src/gone.txt && "
                f"echo {CONTAINER_GIT_POINTER} > .git",
            ]
        )
        code, _, _ = run_text(["docker", "exec", name, "cat", "/root/task/src/keep.txt"])
        if code != 0:
            raise AssertionError("the in-container mutation did not apply")

        tar_bytes = pull_task_tar(name)
        members = (
            subprocess.run(["tar", "-tf", "-"], input=tar_bytes, capture_output=True)
            .stdout.decode(errors="replace")
            .splitlines()
        )
        if "./.git" not in members:
            return fail(
                f"expected `./.git` in the task tar (it is why the restore excludes "
                f"it) — members: {members}"
            )
        ok("pulled the task tar; `./.git` is a member (so the restore must exclude it)")

        # Extract exactly as container::restore_task_dir does: over a copy of the
        # host worktree as it stood before the restore (that is what the real
        # target is), excluding the container's `.git`.
        restored = Path(tempfile.mkdtemp(prefix="katban-probe-restored-"))
        extra_scratch.append(restored)
        shutil.copytree(work, restored, dirs_exist_ok=True)
        proc = subprocess.run(
            ["tar", "-x", "-C", str(restored), "--exclude=./.git"],
            input=tar_bytes,
            capture_output=True,
        )
        if proc.returncode != 0:
            return fail(f"extracting the task tar on the host failed: {proc.stderr.decode()}")
        problems = []
        if (restored / "src" / "keep.txt").read_text().strip() != "agent-version":
            problems.append("the agent's modification did not survive the round-trip")
        if not (restored / "src" / "added.txt").exists():
            problems.append("the agent's new file did not survive the round-trip")
        stale = restored / "src" / "gone.txt"
        if not stale.exists():
            problems.append(
                "the host's copy of the deleted file vanished on extraction — then the "
                "manifest-driven deletion would not be needed at all"
            )
        if (restored / ".git").read_text().strip() != HOST_GIT_POINTER:
            problems.append("the container's `.git` overwrote the host worktree metadata")
        if problems:
            return fail("; ".join(problems))
        ok("extraction applied the agent's modify + add and left the host `.git` alone")

        # A tar cannot express a deletion: the stale host copy survives
        # extraction, which is why restore_task_dir also applies the manifest
        # diff's deleted list. This is that step.
        stale.unlink()
        if stale.exists():
            return fail("the explicit deletion did not remove the stale host file")
        ok("the stale host copy survives until the manifest-diff deletion removes it")
        return 0
    except AssertionError as error:
        return fail(str(error))
    finally:
        run(["docker", "rm", "-f", name])
        shutil.rmtree(work, ignore_errors=True)
        for path in extra_scratch:
            shutil.rmtree(path, ignore_errors=True)


# ---------------------------------------------------------------------------
# card
# ---------------------------------------------------------------------------


def board_file(home):
    """The single board.json in a fresh scratch home (project name independent)."""
    found = sorted((home / "katban").rglob("board.json"))
    return found[0] if found else None


def read_board(home):
    path = board_file(home)
    if path is None:
        return {}
    try:
        return json.loads(path.read_text())
    except (OSError, ValueError):
        return {}


def fail_card(card, status, attempts, result, reason=None):
    """Report a card-level failure with the data needed to diagnose it: the
    RungVerdict rules are about `verify_passed`, the gate note and the manifest
    diff, and a 40-char summary in `result` hides all three."""
    print(f"card status: {status}")
    print(f"card result: {result}")
    print(f"pinned commit: {card.get('commit')!r}  failure_kind: {card.get('failure_kind')!r}")
    for i, attempt in enumerate(attempts):
        print(
            f"  rung {i}: upstream={attempt.get('upstream')!r} "
            f"served={attempt.get('served_upstream')!r} "
            f"verify_passed={attempt.get('verify_passed')!r} "
            f"scope_violations={attempt.get('scope_violations')!r} "
            f"elapsed_ms={attempt.get('elapsed_ms')!r}"
        )
        print(f"          error={attempt.get('error')!r}")
    print(f"attempts (raw): {json.dumps(attempts)}")
    return fail(
        reason or f"card ended '{status}' with {len(attempts)} rung(s) instead of reaching "
        "review with a pinned commit"
    )


def the_card(board):
    """The probe's card. A fresh scratch home holds exactly one board and the
    probe adds exactly one card, so the last card is it."""
    cards = board.get("cards", [])
    return cards[-1] if cards else None


def phase_card(args):
    section("card: attempts ladder in containers, end to end")
    home = Path(tempfile.mkdtemp(prefix="katban-probe-home-"))
    repo = Path(tempfile.mkdtemp(prefix="katban-probe-repo-"))
    env = dict(os.environ, CLAWDE_HOME=str(home))
    binary = str(args.binary)
    scheduler = None
    passed = False
    try:
        if not args.no_seed_auth:
            shutil.copy(args.auth_json, home / "auth.json")
            ok("seeded the scratch home with the real auth store")

        code, _, err = run_text(["git", "init", "-q", "-b", "main"], cwd=repo)
        if code != 0:
            return fail(f"git init failed: {err}")
        for key, value in (("user.email", "probe@example.com"), ("user.name", "Probe")):
            run_text(["git", "config", key, value], cwd=repo)
        (repo / "README.md").write_text("# probe\n")
        for argv in (["git", "add", "."], ["git", "commit", "-qm", "init"]):
            code, _, err = run_text(argv, cwd=repo)
            if code != 0:
                return fail(f"{' '.join(argv)} failed: {err}")

        def clawde(*argv, timeout=300):
            return run_text([binary, "katban", *argv, "--project", PROBE_PROJECT],
                            env=env, cwd=repo, timeout=timeout)

        code, out, err = run_text(
            [binary, "katban", "project", "set", PROBE_PROJECT, str(repo)],
            env=env,
            cwd=repo,
        )
        if code != 0:
            return fail(f"project set failed: {err or out}")
        ok(f"registered {PROBE_PROJECT} -> {repo}")

        # The empty-ladder guard, live: `runtime docker` on the default
        # (attempts 1) board must refuse and name the fix.
        code, out, err = clawde("board", "runtime", "docker")
        text = f"{out}\n{err}"
        if code == 0 or "attempts ladder" not in text:
            return fail(
                "`board runtime docker` with attempts 1 must refuse with the ladder "
                f"guidance (exit={code}): {text.strip()}"
            )
        ok("`runtime docker` on an attempts:1 board refuses and names the fix")

        code, out, err = clawde("board", "attempts", "2")
        if code != 0:
            return fail(f"board attempts 2 failed: {err or out}")
        code, out, err = clawde("board", "attempts-upstreams", "nvidia")
        if code != 0:
            return fail(f"pinning the live-test ladder to nvidia failed: {err or out}")
        ok("pinned the live-test ladder to nvidia")
        code, out, err = clawde("board", "runtime", "docker")
        if code != 0:
            return fail(f"board runtime docker failed after attempts 2: {err or out}")
        ok("board is runtime=docker with attempts 2")

        code, out, err = clawde("board", "card", "add", CARD_PROMPT)
        if code != 0:
            return fail(f"card add failed: {err or out}")
        card = the_card(read_board(home))
        if card is None:
            return fail(f"no card found in the board after add: {out.strip()}")
        card_id = card["id"]
        ok(f"card {card_id} added")

        code, out, err = clawde("board", "card", "set", card_id, "queued")
        if code != 0:
            return fail(f"card set queued failed: {err or out}")
        ok("card queued")

        print("running the scheduler (this launches a container, provisions it, "
              f"and runs the agent — up to {args.card_timeout}s) ...")
        # To a file, not a pipe: the probe never reads a pipe (the scheduler
        # could block on a full buffer) and its `tracing` warnings are the only
        # place a `finalize` failure says *why* (e.g. a failed commit pin).
        sched_log = home / "scheduler.log"
        sched_log_file = sched_log.open("w")
        scheduler = subprocess.Popen(
            [
                binary,
                "katban",
                "board",
                "run",
                "--project",
                PROBE_PROJECT,
            ],
            env=env,
            cwd=repo,
            stdout=sched_log_file,
            stderr=subprocess.STDOUT,
            text=True,
            start_new_session=True,
        )
        deadline = time.time() + args.card_timeout
        status = None
        while time.time() < deadline:
            current = the_card(read_board(home))
            if current is None:
                time.sleep(2)
                continue
            status = current.get("status")
            if status not in ("queued", "running", "backlog"):
                break
            if scheduler.poll() is not None and status not in ("queued", "running"):
                break
            time.sleep(3)
        else:
            stop_scheduler(scheduler)
            return fail(f"card {card_id} never left queued/running within {args.card_timeout}s")

        # Stop the scheduler before asserting on leftovers: its rungs own the
        # containers, and a still-running one would be a false leftover.
        stop_scheduler(scheduler)
        scheduler = None
        final = the_card(read_board(home)) or {}
        result = final.get("result") or ""
        attempts = final.get("attempts") or []

        if status != "review":
            return fail_card(final, status, attempts, result)
        if not final.get("commit"):
            return fail_card(final, status, attempts, result)
        if not attempts:
            return fail_card(final, status, attempts, result)
        if attempts[0].get("upstream") != "nvidia":
            return fail_card(
                final, status, attempts, result,
                reason=f"expected rung 0 pinned to nvidia, got {attempts[0].get('upstream')!r}",
            )
        ok("first rung was pinned to nvidia")
        ok(f"card {card_id} reached review with a pinned commit ({final['commit'][:10]}) "
           f"from {len(attempts)} rung(s)")

        code, out, _ = run_text(
            ["git", "show", f"katban/{card_id}:probe.txt"], cwd=repo
        )
        if code != 0 or PROBE_FILE_BODY not in out:
            return fail(
                "the pinned branch does not contain the agent's file "
                f"(exit={code}, content={out.strip()!r}) — the container's work was "
                "not written back"
            )
        ok("the container's work is on the card's pinned branch")

        artifacts = sorted(home.rglob("task.tar"))
        if not artifacts:
            return fail("no task.tar artifact was pulled for the attempt")
        ok(f"artifact pulled: {artifacts[-1].relative_to(home)}")

        leftovers = containers_including(CONTAINER_PREFIX)
        if leftovers:
            return fail(f"containers left behind: {leftovers}")
        ok("no clawde-katban container left behind")
        passed = True
        return 0
    except AssertionError as error:
        return fail(str(error))
    finally:
        stop_scheduler(scheduler)
        for name in containers_including(CONTAINER_PREFIX):
            # Only ours: the probe owns this prefix.
            if "-probe-" in name:
                run(["docker", "rm", "-f", name])
        if passed:
            shutil.rmtree(home, ignore_errors=True)
            shutil.rmtree(repo, ignore_errors=True)
        else:
            # Kept so a failure can be diagnosed: the board file holds the
            # attempt records (verify_passed / error / scope violations).
            print(f"[info] scratch kept for inspection:\n        CLAWDE_HOME={home}\n        repo={repo}")
            log = home / "scheduler.log"
            if log.exists():
                tail = [line for line in log.read_text(errors="replace").splitlines() if line.strip()]
                print("[info] scheduler log (last 30 lines):")
                for line in tail[-30:]:
                    print(f"      {line}")


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--check", action="store_const", const="check", dest="mode",
                       help="prerequisites only")
    group.add_argument("--plumbing", action="store_const", const="plumbing", dest="mode",
                       help="docker tar round-trip only (no keys)")
    group.add_argument("--card", action="store_const", const="card", dest="mode",
                       help="full card through the scheduler (needs free keys)")
    parser.set_defaults(mode="all")
    parser.add_argument("--binary", type=Path, default=DEFAULT_BINARY,
                        help=f"clawde binary (default {DEFAULT_BINARY})")
    parser.add_argument("--auth-json", type=Path,
                        default=Path(os.path.expanduser("~/.clawde/auth.json")),
                        help="auth store to seed the scratch home with")
    parser.add_argument("--no-seed-auth", action="store_true",
                        help="do not copy the auth store into the scratch home")
    parser.add_argument("--image", default=DEFAULT_IMAGE,
                        help=f"container image (default {DEFAULT_IMAGE})")
    parser.add_argument("--card-timeout", type=int, default=1500,
                        help="seconds to wait for the card to finish (default 1500)")
    args = parser.parse_args()

    if args.mode == "all":
        phases = [("check", phase_check), ("plumbing", phase_plumbing), ("card", phase_card)]
    elif args.mode == "check":
        phases = [("check", phase_check)]
    elif args.mode == "plumbing":
        phases = [("check", phase_check), ("plumbing", phase_plumbing)]
    else:
        phases = [("check", phase_check), ("card", phase_card)]

    args.needs_auth = args.mode in ("all", "card")

    failures = []
    for name, fn in phases:
        try:
            code = fn(args)
        except PrereqError as error:
            print(f"\n[skip] {name}: {error}", file=sys.stderr)
            return 2
        except subprocess.TimeoutExpired as error:
            code = fail(f"{name}: command timed out: {error}")
        if code != 0:
            failures.append(name)
            break

    print()
    if failures:
        print(f"FAIL ({', '.join(failures)})")
        return 1
    print(f"PASS ({', '.join(name for name, _ in phases)})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
