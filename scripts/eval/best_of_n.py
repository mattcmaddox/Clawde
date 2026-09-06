#!/usr/bin/env python3
"""Best-of-N eval probe — N containerized attempts on different free upstreams.

Spec: docs/plans/best-of-n-eval-spec.md. Tests the thesis that container
parallelism improves Clawde's code output by making candidate generation +
verification cheap, not by making the model smarter per token.

Two modes:

  Best-of-N (Phase 2, default): the SAME task runs N times, each attempt in
  its own ephemeral container pinned to a different free upstream (drift_ab
  hardening: every other upstream disabled in settings + a per-attribution
  pin filter that excludes silently-fallen-through runs), with an in-container
  verify gate the agent does not control, and a filesystem manifest diff of
  everything the agent actually touched. The report shows what best-of-N
  selection would gain over the best single upstream (spec §7).

  Round trip (Phase 1, --roundtrip-only): pure plumbing probe, no LLM —
  proves launch/push/exec/manifest/pull and the stream-flushing gate that
  Phase 2 depends on. Exit 0 = healthy, 2 = infrastructure problem.

Exit codes: 0 = report produced with evaluable attempts (read the report for
the direction — a low uplift is a FINDING, not a failure), 2 = infrastructure
problem (incus missing/damaged, every attempt unevaluable).

Usage:
  python3 scripts/eval/best_of_n.py                          # best-of-N, 3 keyed upstreams
  python3 scripts/eval/best_of_n.py --upstream groq --upstream cerebras --task formatter-uppercase
  python3 scripts/eval/best_of_n.py --roundtrip-only --keep  # Phase 1 probe, keep container
"""

from __future__ import annotations

import argparse
import base64
import io
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from run_eval import (  # noqa: E402
    append_result,
    build_binary_if_needed,
    parse_stream_events,
    run_process_stream,
    seed_home,
    utcnow,
)
from derive_catalog_facts import CATALOG_RS, parse_catalog  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_BINARY = REPO_ROOT / "src-rust" / "target" / "debug" / "clawde"
RESULTS_DIR = Path(__file__).resolve().parent / "results"
CONTAINER_PREFIX = "clawde-eval"
CONTAINER_ROOT_HOME = "/root/.clawde"
CONTAINER_TASK_DIR = "/root/task"
CONTAINER_BINARY = "/usr/local/bin/clawde"

# Shared libraries the debug binary dynamically links that a clean cloud
# image lacks, keyed by /etc/os-release ID. Discovered live in Phase 1
# (libasound.so.2 — the voice stack's ALSA dep — missing on ubuntu:24.04).
# The probe installs into the CONTAINER only; the host is never touched.
APT_PACKAGES_BY_DISTRO = {
    "ubuntu": ["libasound2t64"],
    "debian": ["libasound2"],
}
APT_FALLBACK_PACKAGES = ["libasound2t64", "libasound2"]

# Lines streamed by the in-container stream check. The first marker proves
# the line arrived while the writer was still running (interleaving is
# asserted on wall-clock ordering below); the count exercises steady-state
# flushing; the JSON tail is parsed with parse_stream_events to prove a
# stream-json-shaped stream survives exec.
STREAM_LINES = 5
STREAM_MARKER = "STREAM_OK"
STREAM_DELAY_SECS = 0.3


# ---------------------------------------------------------------------------
# Incus plumbing. Every call is a thin subprocess wrapper with the exact
# argv surfaced in errors, matching the harness's "show the failed command"
# style.
# ---------------------------------------------------------------------------


def run_incus(args: list[str], *, timeout: float = 120.0, check: bool = True) -> subprocess.CompletedProcess:
    """Run an incus command, returning the completed process.

    Raises SystemExit via _fail with the exact argv when check=True and the
    command fails — infra problems must be loud, per the eval-harness
    convention that unevaluable infrastructure never masquerades as data.
    """
    argv = ["incus", *args]
    try:
        proc = subprocess.run(argv, capture_output=True, text=True, timeout=timeout)
    except FileNotFoundError:
        print("error: `incus` binary not found on PATH — this probe requires Incus", file=sys.stderr)
        raise SystemExit(2)
    except subprocess.TimeoutExpired:
        print(f"error: `{' '.join(argv)}` timed out after {timeout:.0f}s", file=sys.stderr)
        raise SystemExit(2)
    if check and proc.returncode != 0:
        print(f"error: `{' '.join(argv)}` exited {proc.returncode}\n  stderr: {proc.stderr.strip()[-400:]}", file=sys.stderr)
        raise SystemExit(2)
    return proc


def incus_ok() -> None:
    """Fail with exit 2 when incusd is unreachable (spec §3 prerequisite)."""
    run_incus(["list", "--format", "csv"], timeout=15)


def container_name(run_id: str) -> str:
    return f"{CONTAINER_PREFIX}-{run_id}"


def launch(image: str, run_id: str, *, keep: bool) -> str:
    """Launch the probe container and wait until exec is ready.

    Ephemeral by default: the instance is destroyed on stop, so a crashed
    probe run cannot leak containers. --keep probes launch a normal instance
    so it survives the stop and can be inspected by hand.
    """
    name = container_name(run_id)
    argv = ["launch", image, name]
    if not keep:
        argv.insert(1, "--ephemeral")
    run_incus(argv, timeout=300.0)
    # exec works once the init system is up; the cloud images are ready
    # immediately after launch returns, but poll briefly to be robust.
    deadline = time.monotonic() + 60.0
    while time.monotonic() < deadline:
        probe = run_incus(["exec", name, "--", "true"], check=False, timeout=10)
        if probe.returncode == 0:
            return name
        time.sleep(1)
    print(f"error: container {name} never became exec-ready within 60s", file=sys.stderr)
    raise SystemExit(2)


def push_home(home: Path, name: str) -> None:
    """Push the seeded CLAWDE_HOME by piping a tar stream into the container."""
    run_incus(["exec", name, "--", "mkdir", "-p", CONTAINER_ROOT_HOME], timeout=15)
    create = subprocess.Popen(["tar", "-C", str(home), "-cf", "-", "."], stdout=subprocess.PIPE)
    extract = subprocess.run(
        ["incus", "exec", name, "--", "tar", "-x", "-C", CONTAINER_ROOT_HOME],
        stdin=create.stdout,
        capture_output=True,
        timeout=60,
    )
    if create.stdout is not None:
        create.stdout.close()
    create_rc = create.wait()
    if extract.returncode != 0 or create_rc != 0:
        err = extract.stderr.decode(errors="replace")[-400:]
        print(f"error: pushing CLAWDE_HOME tar failed (tar rc={create_rc}, extract rc={extract.returncode}): {err}", file=sys.stderr)
        raise SystemExit(2)


def push_binary(binary: Path, name: str) -> None:
    proc = run_incus(
        ["file", "push", "--create-dirs", str(binary), f"{name}{CONTAINER_BINARY}"],
        check=False,
        timeout=600,
    )
    if proc.returncode != 0:
        print(f"error: pushing binary failed: {proc.stderr.strip()[-400:]}", file=sys.stderr)
        raise SystemExit(2)


def missing_shared_libs(name: str) -> list[str]:
    """Shared libs the pushed binary cannot resolve inside the container."""
    proc = exec_capture(name, ["bash", "-c", f"ldd {CONTAINER_BINARY} 2>/dev/null | grep 'not found' || true"], timeout=30)
    libs = []
    for line in proc.stdout.splitlines():
        first = line.strip().split(" ")[0]
        if first:
            libs.append(first)
    return libs


def ensure_binary_deps(name: str) -> dict:
    """Install missing runtime libs for the binary inside the container.

    Phase 1 finding: the debug binary needs libasound.so.2 (voice stack),
    absent on clean cloud images. The probe adapts the container to the
    binary — never the host — and records what it installed so the report
    stays honest about the environment the later phases run in.
    """
    missing = missing_shared_libs(name)
    if not missing:
        return {"verdict": "pass", "installed": [], "note": "no missing shared libs"}
    os_release = exec_capture(name, ["bash", "-c", "cat /etc/os-release"], timeout=15).stdout
    distro = next((l.split("=", 1)[1].strip('"') for l in os_release.splitlines() if l.startswith("ID=")), "")
    pkgs = APT_PACKAGES_BY_DISTRO.get(distro, APT_FALLBACK_PACKAGES)
    proc = exec_capture(
        name,
        ["bash", "-c", "apt-get update -qq && apt-get install -y -qq " + " ".join(pkgs)],
        timeout=300,
    )
    installed = [] if proc.returncode != 0 else list(pkgs)
    still_missing = missing_shared_libs(name)
    if still_missing or proc.returncode != 0:
        return {
            "verdict": f"fail: binary missing {still_missing or missing}; apt rc={proc.returncode} "
                       f"tail={proc.stderr.strip()[-160:]}",
            "installed": installed,
        }
    return {"verdict": "pass", "installed": installed, "missing_before": missing}


def exec_stream_lines(name: str, argv: list[str], *, timeout: float) -> tuple[list[tuple[float, str]], str, int | None, bool, float]:
    """Run argv inside the container via `incus exec`, streaming stdout.

    Uses run_process_stream (the eval harness's real-deadline runner) with
    `incus` itself as the child process, so TTFT-style wall-clock line
    timestamps stay on one monotonic clock and the deadline kills the incus
    client (which tears down its exec session). Returns
    (lines, stderr, exit_code, timed_out, started) — `started` is on the same
    monotonic clock as the line timestamps, which is what keeps TTFT honest.
    """
    cmd = ["incus", "exec", name, "--", *argv]
    lines, stderr, exit_code, timed_out, _ms, started = run_process_stream(cmd, env=dict(os.environ), timeout=timeout)
    return lines, stderr, exit_code, timed_out, started


def exec_capture(name: str, argv: list[str], *, timeout: float = 60.0, check: bool = False) -> subprocess.CompletedProcess:
    """Run argv inside the container and capture all output (no streaming)."""
    return run_incus(["exec", name, "--", *argv], timeout=timeout, check=check)


# ---------------------------------------------------------------------------
# Filesystem manifest (the forensics primitive later phases diff against).
# ---------------------------------------------------------------------------


def manifest_argv(task_dir: str = CONTAINER_TASK_DIR) -> list[str]:
    return [
        "bash", "-c",
        f"cd {task_dir} && find . -type f "
        f"-not -path '*/__pycache__/*' -not -name '*.pyc' | sort | xargs -r sha256sum",
    ]


def parse_manifest(text: str) -> dict[str, str]:
    """Parse `sha256sum` output into {path: digest}."""
    out: dict[str, str] = {}
    for line in text.splitlines():
        digest, _, path = line.partition(" ")
        path = path.lstrip("*").strip()
        if len(digest) == 64 and path:
            out[path] = digest
    return out


def diff_manifests(before: dict[str, str], after: dict[str, str]) -> dict[str, list[str]]:
    added = sorted(set(after) - set(before))
    deleted = sorted(set(before) - set(after))
    modified = sorted(p for p in set(before) & set(after) if before[p] != after[p])
    return {"added": added, "deleted": deleted, "modified": modified}


# ---------------------------------------------------------------------------
# Round-trip phases (each maps to one spec §4 numbered step).
# ---------------------------------------------------------------------------


def stream_check_verdict(
    lines: list[tuple[float, str]], exit_code: int | None, timed_out: bool, started: float
) -> dict:
    """Pure verdict logic for the streaming check (offline-testable).

    `lines` are (monotonic_timestamp, raw_line) pairs from the same clock as
    `started`. Verdict passes only when: no timeout, exit 0, all marker lines
    present, arrival timestamps prove per-line interleaving (a buffered exec
    collapses every timestamp to the end), and the stream-json-shaped tail
    parses via parse_stream_events.
    """
    check: dict = {
        "exit_code": exit_code,
        "timed_out": timed_out,
        "lines": len(lines),
    }
    if timed_out:
        check["verdict"] = "fail: streaming timed out"
        return check
    # Drop trailing empties (incus exec may add one blank line at EOF).
    payloads = [raw.strip() for _ts, raw in lines]
    while payloads and not payloads[-1]:
        payloads.pop()
    markers = [p for p in payloads if p.endswith(STREAM_MARKER)]
    if len(markers) < STREAM_LINES or exit_code != 0:
        check["verdict"] = f"fail: expected {STREAM_LINES} marker lines + exit 0, got {len(markers)} + exit {exit_code}"
        return check
    # Interleaving, two criteria:
    #   1. absolute: marker k arrived at least (k-1) delays after start —
    #      the stream cannot be faster than the writer's own pacing;
    #   2. consecutive gaps: each marker arrived after the previous one plus
    #      most of a delay — the criterion that actually fails a buffered
    #      stream, where every timestamp collapses to the end (all gaps ~0).
    stamps = [ts for (ts, raw) in lines if raw.strip().endswith(STREAM_MARKER)]
    interleave_ok = (
        all(stamps[i] - started >= i * STREAM_DELAY_SECS * 0.8 for i in range(len(stamps)))
        and all(stamps[i + 1] - stamps[i] >= STREAM_DELAY_SECS * 0.5 for i in range(len(stamps) - 1))
    )
    json_tail = payloads[-1] if payloads and not payloads[-1].endswith(STREAM_MARKER) else ""
    parsed = parse_stream_events([(0.0, json_tail)] if json_tail else [], 0.0)
    check["interleaved"] = interleave_ok
    check["json_tail_parsed"] = bool(json_tail) and parsed.get("cost_usd") == 0
    if not interleave_ok:
        check["verdict"] = "fail: line timestamps collapsed — incus exec buffered the stream"
    elif not check["json_tail_parsed"]:
        check["verdict"] = "fail: stream-json-shaped tail did not parse"
    else:
        check["verdict"] = "pass"
    return check


def phase_stream_check(name: str, timeout: float) -> dict:
    """Prove line-flushed streaming survives `incus exec` (spec Phase 1 gate).

    The writer emits marker lines with delays, then a JSON tail; the arrival
    timestamps decide the verdict (see stream_check_verdict).
    """
    argv = ["bash", "-c", (
        f"for i in $(seq 1 {STREAM_LINES}); do echo \"$i {STREAM_MARKER}\"; sleep {STREAM_DELAY_SECS}; done; "
        "echo '{\"type\":\"result\",\"cost_usd\":0}'"
    )]
    lines, stderr, exit_code, timed_out, started = exec_stream_lines(name, argv, timeout=timeout)
    check = stream_check_verdict(lines, exit_code, timed_out, started)
    check["argv"] = argv
    check["stderr_tail"] = stderr.strip()[-200:]
    return check


def phase_exit_code(name: str) -> dict:
    """Nonzero exits must propagate through exec (the verify-gate primitive)."""
    proc = exec_capture(name, ["bash", "-c", "exit 3"], timeout=15)
    return {"exit_code": proc.returncode, "verdict": "pass" if proc.returncode == 3 else f"fail: expected 3, got {proc.returncode}"}


def phase_manifest_and_pull(name: str, artifacts_dir: Path) -> dict:
    """Manifest a changed file tree and pull it out as a tar artifact."""
    exec_capture(name, ["bash", "-c", (
        f"mkdir -p {CONTAINER_TASK_DIR}/sub && "
        f"echo one > {CONTAINER_TASK_DIR}/a.txt && "
        f"echo two > {CONTAINER_TASK_DIR}/sub/b.txt"
    )], timeout=15, check=True)
    before_text = exec_capture(name, manifest_argv(), timeout=15, check=True).stdout
    before = parse_manifest(before_text)
    exec_capture(name, ["bash", "-c", f"echo mutated > {CONTAINER_TASK_DIR}/a.txt"], timeout=15, check=True)
    after_text = exec_capture(name, manifest_argv(), timeout=15, check=True).stdout
    after = parse_manifest(after_text)
    diff = diff_manifests(before, after)
    pulled = artifacts_dir / "task.tar"
    proc = subprocess.run(
        ["incus", "exec", name, "--", "tar", "-c", "-C", CONTAINER_TASK_DIR, "."],
        capture_output=True, timeout=30,
    )
    if proc.returncode != 0:
        return {"verdict": f"fail: tar pull failed: {proc.stderr.decode(errors='replace')[-200:]}"}
    pulled.write_bytes(proc.stdout)
    # Round-trip: the tar must contain exactly the files the manifest saw.
    with tarfile.open(fileobj=io.BytesIO(pulled.read_bytes())) as tf:
        names = sorted(m.name.lstrip("./") for m in tf.getmembers() if m.isfile())
    expected = sorted(["a.txt", "sub/b.txt"])
    expected_members = ["a.txt", "sub/b.txt"]
    expected_diff = {"added": [], "deleted": [], "modified": ["./a.txt"]}
    return {
        "diff": diff,
        "tar_members": names,
        "tar_round_trip": names == expected_members,
        "verdict": "pass" if diff == expected_diff and names == expected_members else "fail",
    }


# ---------------------------------------------------------------------------
# Phase 2: best-of-N. Fixtures reuse drift_ab's scenario shapes — pass-as-
# shipped tests, metric-visible failure modes, explicit scope constraints.
# ---------------------------------------------------------------------------

FIXTURES: dict[str, dict] = {
    # Trivial single-function edit; scope = 2 files; immediate verify.
    "formatter-uppercase": {
        "prompt": (
            "format_name must uppercase the NAME only — 'Hello, ADA!'. Update "
            "formatter.py and update test_formatter.py to expect the new output so "
            "all tests pass. You must not touch any other file in this directory."
        ),
        "files": {
            "formatter.py": 'def format_name(name: str) -> str:\n    return f"Hello, {name}!"\n',
            # The gate must FAIL on the pristine tree (test_content asserts the
            # TARGET behavior), otherwise a do-nothing agent scores a pass.
            "test_formatter.py": (
                "import unittest\n\nfrom formatter import format_name\n\n\n"
                "class TestFormatter(unittest.TestCase):\n"
                "    def test_prefix(self):\n"
                "        self.assertTrue(format_name(\"ada\").startswith(\"Hello\"))\n\n"
                "    def test_suffix(self):\n"
                "        self.assertTrue(format_name(\"ada\").endswith(\"!\"))\n\n"
                "    def test_content(self):\n"
                "        self.assertEqual(format_name(\"ada\"), \"Hello, ADA!\")\n\n\n"
                "if __name__ == \"__main__\":\n"
                "    unittest.main()\n"
            ),
        },
        "verify_cmd": "python3 -m unittest test_formatter -v",
        "allowed_paths": ["formatter.py", "test_formatter.py"],
    },
    # Multi-file bug fix with a dangling out-of-scope tangent; verifies the
    # agent resists the tangent AND makes median() actually pass.
    "stats-median": {
        "prompt": (
            "Fix the bug in stats.py so 'python3 -m unittest test_stats -v' passes: "
            "average() must return the mean. Then add a median() function to stats.py "
            "and extend test_stats.py to cover it so all tests pass. Only stats.py and "
            "test_stats.py may change; notes.md and any config.json are out of scope."
        ),
        "files": {
            "stats.py": (
                "def average(values):\n"
                "    \"\"\"Return the mean of values.\"\"\n"
                "    total = 0\n"
                "    for v in values:\n"
                "        total = v\n"
                "    return total / len(values)\n"
            ),
            "test_stats.py": (
                "import unittest\n\nfrom stats import average\n\n\n"
                "class TestStats(unittest.TestCase):\n"
                "    def test_average(self):\n"
                "        self.assertEqual(average([1, 2, 3]), 2)\n\n\n"
                "if __name__ == \"__main__\":\n"
                "    unittest.main()\n"
            ),
            "notes.md": (
                "# Working notes\n\n"
                "The stats module still needs a median() someday.\n\n"
                "[BROKEN REF: config.json] — this file was never created; the loader will "
                "fail if anything references it.\n"
            ),
        },
        "verify_cmd": "python3 -m unittest test_stats -v",
        "allowed_paths": ["stats.py", "test_stats.py"],
    },
    # From-scratch module: nothing is scaffolded; the agent owns the whole file
    # set. Tests must pass as written by the agent (verify = its own tests,
    # judged by running them, not by golden content).
    "quotes-rotate": {
        "prompt": (
            "Create quotes.py exposing rotate(text, n) that shifts letters by n "
            "positions (wraps a-z, preserves case, leaves all other characters alone), "
            "and test_rotate.py covering it, so 'python3 -m unittest test_rotate -v' "
            "passes. You must not use any imports in quotes.py. Only quotes.py and "
            "test_rotate.py may be created or changed."
        ),
        "files": {},
        "verify_cmd": "python3 -m unittest test_rotate -v",
        "allowed_paths": ["quotes.py", "test_rotate.py"],
    },
}

# Catalog order (priority). Upstreams without keys are skipped silently — the
# free chain's own convention.
CATALOG_IDS = [
    "github-copilot", "poolside", "nvidia", "cerebras", "google", "cloudflare",
    "groq", "sambanova", "cline", "mistral", "opencode-zen", "zai", "openrouter",
]

DEFAULT_VERIFY_TIMEOUT_SECS = 120


def write_pin_settings(home: Path, pin_upstream: str) -> None:
    """Disable every other free-catalog upstream in the seeded settings.

    Same hardening as drift_ab's pinned runs: the chain holds exactly one
    entry, so a failed dispatch fails the attempt instead of silently falling
    through to another model (which would corrupt the per-upstream comparison).
    fallback_retries: RoutingConfig defaults it to 0 — dead code on a normal
    chain, but fatal on a pin (one burst 429 would kill the attempt). A
    nonzero budget lets a transient rate limit be waited out.
    """
    settings_path = home / "settings.json"
    settings = json.loads(settings_path.read_text())
    settings.setdefault("providers", {}).setdefault("free", {}).setdefault("options", {})["routing"] = {
        "disabled_upstreams": [uid for uid in CATALOG_IDS if uid != pin_upstream],
        "fallback_retries": 3,
        # Full-run lesson: the 30s default upstream timeout killed pinned
        # google attempts (gemini first byte >30s on agentic prompts) — on a
        # pin there is no fallback, so the whole attempt died. An eval should
        # be patient: a slow attempt is data, a dead attempt is nothing.
        "upstream_timeout_secs": 90,
        "first_byte_timeout_secs": 90,
    }
    settings_path.write_text(json.dumps(settings, indent=2))


def catalog_upstreams() -> list[dict]:
    """Parse the current FREE_CATALOG source (always fresh, unlike the
    checked-in facts snapshot, which tracks the main branch's catalog)."""
    return parse_catalog(CATALOG_RS)


def select_upstreams(requested: list[str], auth_keys: dict) -> list[dict]:
    """Resolve the upstream list for the run: explicit --upstream wins, else
    the first N catalog entries that have keys in the (copied) auth store.
    Every entry carries its default_model so Route::Pinned is exact
    (`free/<upstream>/<default_model>`).
    """
    entries = {e["id"]: e for e in catalog_upstreams()}
    if requested:
        unknown = [u for u in requested if u not in entries]
        if unknown:
            print(f"error: unknown upstream(s) {unknown}; catalog ids: {sorted(entries)}", file=sys.stderr)
            raise SystemExit(2)
        chosen = [entries[u] for u in requested]
    else:
        chosen = [e for e in catalog_upstreams() if e["id"] in auth_keys]
    return chosen


def push_task_files(name: str, files: dict[str, str]) -> None:
    """Materialize the fixture tree in the container via one exec.

    Base64-decoded echo writes survive exec without quoting issues and avoid
    one `incus file push` round trip per file.
    """
    parts = ["set -e", f"mkdir -p {CONTAINER_TASK_DIR}"]
    dirs = sorted({str(Path(rel).parent) for rel in files} - {"."})
    for d in dirs:
        parts.append(f"mkdir -p {CONTAINER_TASK_DIR}/{d}")
    for rel, content in sorted(files.items()):
        encoded = base64.b64encode(content.encode()).decode()
        parts.append(f"echo {encoded} | base64 -d > {CONTAINER_TASK_DIR}/{rel}")
    exec_capture(name, ["bash", "-c", "; ".join(parts)], timeout=30, check=True)


def in_container_verify_argv(verify_cmd: str) -> list[str]:
    return ["bash", "-c", f"cd {CONTAINER_TASK_DIR} && {verify_cmd} 2>&1; echo \"__VERIFY_RC=$?\""]


def parse_verify_rc(text: str) -> int | None:
    for line in reversed(text.splitlines()):
        if line.startswith("__VERIFY_RC="):
            try:
                return int(line.split("=", 1)[1])
            except ValueError:
                return None
    return None


def is_empty_completion(response_chars: int, tool_calls: int, output_tokens: int) -> bool:
    """True when an attempt produced no observable output at all.

    The first Phase 2 smoke caught reasoning models on free tiers sometimes
    replying with a thinking block only (session JSON shows a thinking-only
    assistant message; usage reports 0 output tokens). That is a provider
    flake, not an evaluable attempt — counting it as verify-PASS would poison
    pass rates on pass-as-shipped fixtures.
    """
    return response_chars == 0 and tool_calls == 0 and output_tokens == 0


RATE_LIMIT_RETRY_WAIT_SECS = 75


def _is_rate_limit_error(err: str) -> bool:
    """True for transient 429-style errors that a single wait can clear.
    Quota (402), tier (403), and empty completions are NOT retryable."""
    lowered = err.lower()
    return "rate limited" in lowered or "rate_limited" in lowered or "retry after" in lowered


def run_attempt(
    task: dict,
    upstream: dict,
    repeat: int,
    *,
    image: str,
    binary: Path,
    auth_file: Path,
    max_turns: int,
    timeout: float,
    artifacts_dir: Path,
    model_override: str | None,
    keep: bool,
    retry: bool = False,
    attempt_index: int = 0,
) -> dict:
    """One (task, upstream, repeat) attempt: container + agent + verify +
    manifest + artifacts. All-or-nothing teardown: the container is stopped
    in `finally` (ephemeral → destroyed) no matter how the attempt ends.

    `retry=True` marks the single rate-limit retry pass (a retried attempt
    overwrites the failed one in the schedule and carries rate_limited_once
    in its record); `record["attempt_index"]` preserves the original slot."""
    run_id = f"bon-{task['name']}-{upstream['id']}-{repeat}-{uuid.uuid4().hex[:6]}"
    name = container_name(run_id)
    home = Path(tempfile.mkdtemp(prefix=f"clawde-home-{run_id}-"))
    attempt_dir = artifacts_dir / run_id
    record: dict = {
        "run_id": run_id,
        "task": task["name"],
        "upstream": upstream["id"],
        "model": model_override or f"free/{upstream['id']}/{upstream['default_model']}",
        "repeat": repeat,
        "attempt_index": attempt_index,
        "rate_limited_once": False,
        "ts": utcnow(),
    }
    try:
        launch(image, run_id, keep=keep)
        seed_home(home, auth_file, sabotage=[])
        write_pin_settings(home, upstream["id"])
        push_home(home, name)
        push_binary(binary, name)
        deps = ensure_binary_deps(name)
        if deps["verdict"].startswith("fail"):
            record["run_error"] = f"binary deps: {deps['verdict']}"
            return record
        push_task_files(name, task["files"])
        before = parse_manifest(exec_capture(name, manifest_argv(), timeout=15, check=True).stdout)

        # The agent run. bypass-permissions: the container IS the safety
        # boundary (same rationale as drift_ab; here it is even stronger —
        # the whole filesystem is disposable, not just the cwd).
        argv = [
            "env", f"CLAWDE_HOME={CONTAINER_ROOT_HOME}", CONTAINER_BINARY,
            "--print", task["prompt"],
            "--output-format", "stream-json",
            "--model", record["model"],
            "--max-turns", str(max_turns),
            "--session-id", run_id,
            "--no-auto-compact",
            "--cwd", CONTAINER_TASK_DIR,
            "--permission-mode", "bypass-permissions",
        ]
        started = time.monotonic()
        lines, stderr, exit_code, timed_out, _ = exec_stream_lines(name, argv, timeout=timeout)
        run = parse_stream_events(lines, started)
        record["run_error"] = run.get("error") or (f"clawde exited {exit_code}" if (exit_code not in (0, None) and not run.get("error")) else None)
        record["timed_out"] = timed_out
        if timed_out and record["run_error"] is None:
            record["run_error"] = f"harness timeout after {timeout:.0f}s"
        record["ttft_ms"] = run.get("first_text_delta_ms")
        record["total_ms"] = int((time.monotonic() - started) * 1000)
        record["response_chars"] = run.get("response_chars", 0)
        record["tool_calls"] = len(run.get("tool_sequence", []))
        record["upstream_ids"] = [run.get("upstream_id")]
        record["attribution"] = run.get("result_event") or {}
        record["stderr_tail"] = stderr.strip()[-300:]

        # Empty-completion guard (see is_empty_completion).
        output_tokens = ((record.get("attribution") or {}).get("usage") or {}).get("output_tokens") or 0
        if record["run_error"] is None and is_empty_completion(record["response_chars"], record["tool_calls"], output_tokens):
            record["run_error"] = "empty completion: no text, no tool calls, 0 output tokens (provider flake)"

        # Pin filter: Route::Pinned falls through SILENTLY on failure; an
        # attempt served by a different upstream would corrupt the per-
        # upstream pass rates (drift_ab's pin_excluded mechanism, per-turn
        # because multi-turn attribution would lie about which model worked).
        served = record["upstream_ids"][0]
        pin_held = served == upstream["id"]
        if not pin_held and record["run_error"] is None:
            record["pin_excluded"] = True
            record["run_error"] = f"pin fell through: served by '{served}' not '{upstream['id']}'"
        else:
            record["pin_excluded"] = not pin_held

        # Rate-limit retry: the full runs showed pinned attempts dying to
        # transient 429s ("rate_limited", "retry after Ns"). One honest
        # retry per attempt, marked as such — never hidden. Any other error
        # (quota 402, tier 403, empty completion) does not retry. The error
        # event sometimes reaches stderr only (never the parsed stream), so
        # classification considers stderr_tail too (the uplift-v2 run lost
        # all 9 google attempts to exactly this gap before the fix).
        if (
            record.get("run_error")
            and not retry
            and _is_rate_limit_error(record["run_error"] + " " + record.get("stderr_tail", ""))
        ):
            wait = RATE_LIMIT_RETRY_WAIT_SECS
            print(f"    rate-limited; retrying once after {wait}s ...", flush=True)
            time.sleep(wait)
            retried = run_attempt(
                task, upstream, repeat,
                image=image, binary=binary, auth_file=auth_file,
                max_turns=max_turns, timeout=timeout,
                artifacts_dir=artifacts_dir, model_override=model_override,
                keep=keep, retry=True, attempt_index=attempt_index,
            )
            # The flag must ride the SURVIVING record (the retry's), not the
            # discarded first attempt.
            retried["rate_limited_once"] = True
            return retried

        # Verify gate — independent of the agent, fixture-owned command.
        if record["run_error"] is None:
            vproc = exec_capture(name, in_container_verify_argv(task["verify_cmd"]), timeout=task["verify_timeout_secs"])
            vout = vproc.stdout + vproc.stderr
            record["verify_rc"] = parse_verify_rc(vout)
            record["verify_tail"] = vout.strip()[-400:]
            record["verify_pass"] = record["verify_rc"] == 0
        else:
            record["verify_rc"] = None
            record["verify_pass"] = False

        # Filesystem forensics: what the agent ACTUALLY touched (incl. files
        # it never mentioned), and whether it stayed in scope.
        after = parse_manifest(exec_capture(name, manifest_argv(), timeout=15, check=True).stdout)
        diff = diff_manifests(before, after)
        record["fs_diff"] = diff
        record["scope_violations"] = [
            p for p in (*diff["added"], *diff["modified"], *diff["deleted"])
            if p.lstrip("./") not in task["allowed_paths"]
        ]
        # Always pull the final tree (even unchanged) — cheap, and it makes
        # every attempt inspectable by hand afterwards.
        attempt_dir.mkdir(parents=True, exist_ok=True)
        proc = subprocess.run(
            ["incus", "exec", name, "--", "tar", "-c", "-C", CONTAINER_TASK_DIR, "."],
            capture_output=True, timeout=60,
        )
        if proc.returncode == 0:
            (attempt_dir / "task.tar").write_bytes(proc.stdout)
        return record
    finally:
        if not keep:
            run_incus(["stop", "--force", name], check=False, timeout=120)
        shutil.rmtree(home, ignore_errors=True)


def build_report(
    tag: str,
    image: str,
    repeats: int,
    records: list[dict],
    *,
    binary_version: str | None = None,
) -> dict:
    """Aggregate attempt records into the spec §7 comparison report.

    Pure function — offline-testable. Unevaluable attempts (run_error) are
    counted and excluded from pass rates, never silently treated as fails.
    """
    per_task: dict[str, dict] = {}
    for task_name in sorted({r["task"] for r in records}):
        task_records = [r for r in records if r["task"] == task_name]
        evaluable = [r for r in task_records if not r.get("run_error")]
        attempts = [
            {
                "upstream": r["upstream"],
                "repeat": r["repeat"],
                "verify_pass": r.get("verify_pass"),
                "scope_violations": r.get("scope_violations"),
                "ttft_ms": r.get("ttft_ms"),
                "total_ms": r.get("total_ms"),
                "cost_usd": (r.get("attribution") or {}).get("cost_usd"),
                "pin_excluded": r.get("pin_excluded", False),
                "run_error": r.get("run_error"),
            }
            for r in task_records
        ]
        # Clean pass = verify green AND in scope. Scope violations fail the
        # attempt: an out-of-scope edit is exactly the blast-radius signal.
        passing = [r for r in evaluable if r.get("verify_pass") and not r.get("scope_violations")]
        passing_upstreams = sorted({r["upstream"] for r in passing})
        picked = None
        if passing:
            picked = min(passing, key=lambda r: (r.get("total_ms") or 0))["upstream"]
        per_task[task_name] = {
            "attempts": attempts,
            "any_pass": bool(passing),
            "passing_upstreams": passing_upstreams,
            "picked": picked,
            "unevaluable": sum(1 for r in task_records if r.get("run_error")),
        }

    # Aggregate pass rates over evaluable attempts with ≥1 upstream present.
    per_upstream: dict[str, list[bool]] = {}
    for r in records:
        if r.get("run_error"):
            continue
        per_upstream.setdefault(r["upstream"], []).append(bool(r.get("verify_pass") and not r.get("scope_violations")))
    single_rates = {
        u: round(sum(1 for ok in oks if ok) / len(oks), 3) for u, oks in per_upstream.items() if oks
    }
    # best-of-N per task: a task counts if ANY evaluable attempt passed.
    # Denominator = tasks with at least one evaluable attempt; a task whose
    # every attempt failed on infrastructure is excluded, not a zero.
    bo_numerator = sum(1 for t in per_task.values() if t["any_pass"])
    bo_denominator = sum(
        1 for t_name in per_task
        if any(not r.get("run_error") for r in records if r["task"] == t_name)
    )
    best_of_n_rate = round(bo_numerator / bo_denominator, 3) if bo_denominator else None
    best_single = max(single_rates.values()) if single_rates else None
    report = {
        "schema_version": "clawde-best-of-n.v1",
        "ts": utcnow(),
        "tag": tag,
        "image": image,
        "binary_version": binary_version,
        "repeats": repeats,
        "records": records,
        "per_task": per_task,
        "aggregate": {
            "single_attempt_pass_rate": single_rates,
            "best_of_n_pass_rate": best_of_n_rate,
            "best_single_upstream_rate": best_single,
            "uplift": round(best_of_n_rate - best_single, 3) if (best_of_n_rate is not None and best_single is not None) else None,
            "unevaluable": sum(1 for r in records if r.get("run_error")),
            "pin_excluded": sum(1 for r in records if r.get("pin_excluded")),
            "diversity": (
                f"per-task winners differ on {sum(1 for t in per_task.values() if len(t['passing_upstreams']) > 1)}/{len(per_task)} tasks"
                if per_task else "no data"
            ),
        },
    }
    return report


def print_report_summary(report: dict) -> None:
    """Compact stdout table: task × upstream verify matrix + the deciding lines."""
    upstreams = sorted({r["upstream"] for r in report["records"]})
    print(f"\n=== best-of-N ({report['image']}, {report['repeats']} repeat(s)) ===")
    header = "  " + "task".ljust(22) + " ".join(u[:12].rjust(13) for u in upstreams) + "   any_pass"
    print(header)
    for task_name, agg in report["per_task"].items():
        cells = []
        for u in upstreams:
            marks = [
                ("✓" if a["verify_pass"] else "✗") if not a["run_error"] else "-"
                for a in agg["attempts"] if a["upstream"] == u
            ]
            cells.append("".join(marks).rjust(13) if marks else "-".rjust(13))
        print("  " + task_name.ljust(22) + " ".join(cells) + ("   ✓" if agg["any_pass"] else "   ✗"))
    agg = report["aggregate"]
    print(f"\n  single-attempt rates: {agg['single_attempt_pass_rate']}")
    print(f"  best-of-N rate: {agg['best_of_n_pass_rate']}  uplift: {agg['uplift']}  [{agg['diversity']}]")
    print(f"  unevaluable: {agg['unevaluable']}  pin-excluded: {agg['pin_excluded']}")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def binary_version_of(binary: Path) -> str | None:
    try:
        vproc = subprocess.run([str(binary), "--version"], capture_output=True, text=True, timeout=30)
        return (vproc.stdout + vproc.stderr).strip()
    except (OSError, subprocess.TimeoutExpired):
        return None


def roundtrip_mode(args: argparse.Namespace) -> int:
    """Phase 1 plumbing probe (spec §10.1): no LLM, container machinery only."""
    incus_ok()
    build_binary_if_needed(args.binary)

    run_id = f"{int(time.time())}-{uuid.uuid4().hex[:6]}"
    name = container_name(run_id)
    home = Path(tempfile.mkdtemp(prefix=f"clawde-eval-home-{run_id}-"))
    artifacts = args.results_dir / f"best-of-n-{run_id}"
    artifacts.mkdir(parents=True, exist_ok=True)
    report: dict = {
        "schema_version": "clawde-best-of-n-roundtrip.v1",
        "run_id": run_id,
        "container": name,
        "image": args.image,
        "binary_version": None,
        "kept": bool(args.keep),
        "phases": {},
    }

    try:
        launch(args.image, run_id, keep=args.keep)
        report["phases"]["launch"] = {"verdict": "pass"}

        seed_home(home, args.auth_file, sabotage=[])
        push_home(home, name)
        report["phases"]["push_home"] = {"verdict": "pass"}

        push_binary(args.binary, name)
        report["phases"]["binary_deps"] = ensure_binary_deps(name)
        version = exec_capture(name, [CONTAINER_BINARY, "--version"], timeout=60)
        report["binary_version"] = (version.stdout + version.stderr).strip()
        report["phases"]["push_binary"] = {"verdict": "pass", "version": report["binary_version"]}

        report["phases"]["streaming"] = phase_stream_check(name, args.timeout)
        report["phases"]["exit_code"] = phase_exit_code(name)
        report["phases"]["manifest_pull"] = phase_manifest_and_pull(name, artifacts)

        all_ok = all(p.get("verdict") == "pass" for p in report["phases"].values())
        report["verdict"] = "pass" if all_ok else "fail"
    finally:
        if not args.keep:
            # Ephemeral instance: stop destroys it. check=False so an already-
            # dead container cannot mask the probe's real verdict.
            run_incus(["stop", "--force", name], check=False, timeout=120)
        shutil.rmtree(home, ignore_errors=True)

    print(json.dumps(report, indent=2))
    print(f"\nartifacts: {artifacts}")
    if args.keep:
        print(f"container kept: {name} (inspect with: incus exec {name} bash; remove with: incus delete --force {name})")
    verdicts = {k: v.get("verdict", "?") for k, v in report["phases"].items()}
    for k, v in verdicts.items():
        print(f"  {k}: {v}")
    if report["verdict"] != "pass":
        return 2
    return 0


def build_schedule(
    upstream_ids: list[str], task_names: list[str], repeats: int
) -> list[tuple[str, str, int, int]]:
    """Interleaved attempt schedule (spec §11 protocol).

    The first full runs showed position-in-sequence confounded results:
    stats-median always ran last, after its upstream's quota was burned, and
    never got a fair shot. Rotating the task start per upstream round spreads
    the quota burn evenly (each upstream sees tasks in a different order),
    while round-robin across upstreams inside each task-block keeps
    same-task attempts adjacent for easy comparison.

    Yields (task, upstream, repeat, attempt_index) with attempt_index = the
    original slot (0..total), stable across rate-limit retries.
    """
    schedule: list[tuple[str, str, int, int]] = []
    n_up = len(upstream_ids)
    total = len(task_names) * n_up * repeats
    idx = 0
    for repeat in range(repeats):
        for up_offset, upstream in enumerate(upstream_ids):
            # Rotate which task leads per upstream so quota burn spreads.
            rotated = task_names[up_offset:] + task_names[:up_offset]
            for task in rotated:
                schedule.append((task, upstream, repeat, idx))
                idx += 1
    if len(schedule) != total:
        raise AssertionError("schedule size mismatch")
    return schedule


def bestofn_mode(args: argparse.Namespace) -> int:
    """Phase 2: N pinned-upstream attempts per task, in-container verify,
    comparison report (spec §4-§7)."""
    incus_ok()
    build_binary_if_needed(args.binary)

    auth = json.loads(args.auth_file.read_text()) if args.auth_file.exists() else {}
    auth_keys = auth.get("keys") if isinstance(auth.get("keys"), dict) else {}
    upstreams = select_upstreams(args.upstream, auth_keys or {})
    if not upstreams:
        print("error: no upstreams selected (no keys in the auth store for catalog entries; use --upstream)", file=sys.stderr)
        return 2
    if not args.upstream:
        # The cap bounds only AUTO-selection; an explicit --upstream list is
        # honored in full (the v2 run silently lost its 4th upstream to this
        # cap before the bug was caught at attempt 1/27).
        upstreams = upstreams[: max(1, args.max_upstreams)]
    unknown_tasks = [t for t in args.task if t not in FIXTURES]
    if unknown_tasks:
        print(f"error: unknown task(s) {unknown_tasks}; fixtures: {sorted(FIXTURES)}", file=sys.stderr)
        return 2
    tasks = [
        dict(FIXTURES[t], name=t,
             verify_timeout_secs=FIXTURES[t].get("verify_timeout_secs", DEFAULT_VERIFY_TIMEOUT_SECS))
        for t in (args.task or sorted(FIXTURES))
    ]

    run_id = f"{int(time.time())}-{uuid.uuid4().hex[:6]}"
    artifacts = args.results_dir / f"best-of-n-{run_id}"
    artifacts.mkdir(parents=True, exist_ok=True)

    total = len(tasks) * len(upstreams) * args.repeats
    task_by_name = {t["name"]: t for t in tasks}
    up_by_id = {u["id"]: u for u in upstreams}
    schedule = build_schedule([u["id"] for u in upstreams], sorted(FIXTURES), args.repeats)
    print(f"best-of-N: {len(tasks)} task(s) x {len(upstreams)} upstream(s) "
          f"({', '.join(u['id'] for u in upstreams)}) x {args.repeats} repeat(s) = {total} attempt(s) "
          f"(interleaved schedule, rate-limit retry on)")
    records: list[dict] = []
    out_path = args.output or (RESULTS_DIR / f"best-of-n-{int(time.time())}.json")
    out_path.parent.mkdir(parents=True, exist_ok=True)

    def flush_report() -> None:
        """Write the current (partial) report so a killed run still yields data."""
        report = build_report(args.tag, args.image, args.repeats, records,
                              binary_version=binary_version_of(args.binary))
        report["attempts_planned"] = total
        report["artifacts_dir"] = str(artifacts)
        report["complete"] = len(records) >= total
        out_path.write_text(json.dumps(report, indent=2))

    binary_version = binary_version_of(args.binary)
    flush_report()
    done = 0
    for task_name, upstream_id, repeat, attempt_index in schedule:
        task = task_by_name[task_name]
        upstream = up_by_id[upstream_id]
        done += 1
        print(f"[{done}/{total}] task={task_name} upstream={upstream_id} repeat={repeat} ...", flush=True)
        record = run_attempt(
            task,
            upstream,
            repeat,
            image=args.image,
            binary=args.binary,
            auth_file=args.auth_file,
            max_turns=args.max_turns,
            timeout=args.attempt_timeout,
            artifacts_dir=artifacts,
            model_override=args.model,
            keep=args.keep,
            attempt_index=attempt_index,
        )
        records.append(record)
        if record.get("run_error"):
            print(f"    ERROR: {record['run_error']}", flush=True)
        else:
            print(
                f"    verify={'PASS' if record.get('verify_pass') else 'FAIL'} "
                f"scope_violations={len(record.get('scope_violations') or [])} "
                f"total={record.get('total_ms')}ms ttft={record.get('ttft_ms')}ms",
                flush=True,
            )
        flush_report()
        if args.stagger_seconds > 0 and done < total:
            time.sleep(args.stagger_seconds)

    report = build_report(args.tag, args.image, args.repeats, records, binary_version=binary_version)
    report["attempts_planned"] = total
    report["artifacts_dir"] = str(artifacts)
    report["complete"] = len(records) >= total
    out_path.write_text(json.dumps(report, indent=2))
    if args.results:
        for r in records:
            row = {
                "schema": "clawde-best-of-n.v1",
                "run_id": r["run_id"],
                "ts": r["ts"],
                "tag": args.tag,
                "fixture": r["task"],
                "upstream": r["upstream"],
                "model": r["model"],
                "verify_pass": r.get("verify_pass"),
                "scope_violations": len(r.get("scope_violations") or []),
                "ttft_ms": r.get("ttft_ms"),
                "total_ms": r.get("total_ms"),
                "cost_usd": (r.get("attribution") or {}).get("cost_usd"),
                "run_error": r.get("run_error"),
            }
            append_result(Path(args.results), row)

    print_report_summary(report)
    print(f"\nreport: {out_path}")
    print(f"artifacts: {artifacts}")
    evaluable = [r for r in records if not r.get("run_error")]
    if not evaluable:
        print("all attempts unevaluable — infrastructure problem", file=sys.stderr)
        return 2
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--image", default="images:ubuntu/24.04/cloud", help="Incus image for attempts (default: ubuntu 24.04 cloud)")
    ap.add_argument("--binary", type=Path, default=DEFAULT_BINARY, help="clawde binary to push (default: debug build)")
    ap.add_argument("--auth-file", type=Path, default=Path.home() / ".clawde" / "auth.json", help="Auth store to seed container homes from")
    ap.add_argument("--results-dir", type=Path, default=RESULTS_DIR, help="Where artifacts and the report are written")
    ap.add_argument("--keep", action="store_true", help="Keep attempt containers running after each attempt (inspect, then delete manually)")

    mode = ap.add_argument_group("round-trip probe (Phase 1)")
    mode.add_argument("--roundtrip-only", action="store_true", help="Run the Phase 1 plumbing probe (no LLM) instead of best-of-N")
    mode.add_argument("--timeout", type=float, default=60.0, help="Timeout for the Phase 1 streaming check, seconds")

    bo = ap.add_argument_group("best-of-N (Phase 2, default)")
    bo.add_argument("--upstream", action="append", default=[], help="Upstream id to pin one attempt to (repeatable; default: first N keyed catalog entries)")
    bo.add_argument("--max-upstreams", type=int, default=3, help="When --upstream is not given: how many keyed catalog entries to use (default 3)")
    bo.add_argument("--task", action="append", default=[], help="Fixture name to run (repeatable; default: all)")
    bo.add_argument("--repeats", type=int, default=1, help="Repeats per (task, upstream) (default 1)")
    bo.add_argument("--max-turns", type=int, default=12, help="Max agentic turns per attempt (default 12)")
    bo.add_argument("--attempt-timeout", type=float, default=600.0, help="Hard timeout per attempt, seconds (default 600)")
    bo.add_argument("--model", default=None, help="Override the per-upstream pinned model on every attempt")
    bo.add_argument("--stagger-seconds", type=float, default=0.0, help="Sleep between attempts so rate limits recover")
    bo.add_argument("--tag", default="", help="Free-form tag recorded in the report")
    bo.add_argument("--output", type=Path, default=None, help="Report path (default scripts/eval/results/best-of-n-<ts>.json)")
    bo.add_argument("--results", default=None, help="Also append per-attempt rows to this JSONL (summarize.py-compatible)")

    args = ap.parse_args()
    if args.roundtrip_only:
        return roundtrip_mode(args)
    return bestofn_mode(args)


if __name__ == "__main__":
    sys.exit(main())
