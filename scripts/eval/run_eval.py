#!/usr/bin/env python3
"""Headless eval harness for Clawde.

Runs a prompt through the real Clawde binary in `--print --output-format
stream-json` mode under an isolated CLAWDE_HOME, parses the machine-readable
event stream, records timing / attribution / cost / tool trajectory, and
evaluates the response against a promptfoo-style assertion set.

Two-tier model (see scripts/eval/README.md):

1. Content tier (this script): headless, deterministic where possible, drives
   the real provider stack. Assertions are deterministic text checks; LLM-as-
   judge grading plugs in via `--judge-prompt` later.
2. TUI tier: `tui_probe.py` drives the ratatui frontend in tmux to assert
   streaming and rendering behavior (spinner, key-ring footer, no crash).

Isolation: every run gets a fresh temp CLAWDE_HOME seeded with a copy of the
real auth store (or `--auth-file`). Real key-ring cooldown state is never
touched. `--sabotage <upstream>` replaces that upstream's keys with invalid
placeholders to force the free-provider chain to fall through — the exact tool
for testing how Clawde behaves when an upstream is down.

Exit code: 0 pass, 1 assertions/judge-gate failed, 2 could not be evaluated
(provider error, timeout, empty completion — infra flakiness, not a regression).
"""

import argparse
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from queue import Empty, Queue
from datetime import datetime, timezone
from pathlib import Path
from statistics import median

sys.path.insert(0, str(Path(__file__).resolve().parent))
from derive_catalog_facts import CATALOG_RS, parse_catalog, source_sha256  # noqa: E402
from embeddings import Embedder, hf_token_from_auth  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parents[2]
SRC_RUST = REPO_ROOT / "src-rust"
DEFAULT_BINARY = SRC_RUST / "target" / "debug" / "clawde"
DEFAULT_AUTH = Path(os.environ.get("HOME", "~")) / ".clawde" / "auth.json"
FIXTURES_DIR = Path(__file__).resolve().parent / "fixtures"
RESULTS_DIR = Path(__file__).resolve().parent / "results"
DEFAULT_RESULTS = RESULTS_DIR / "results.jsonl"

DEFAULT_JUDGE_RUBRIC = (
    "Correctness, completeness, and clarity. The answer should be factually "
    "accurate, fully address every part of the question, and reference "
    "specific files and function names when asked."
)

# Pinned judge model (advisory tier). Free/auto routes the judge to whatever
# upstream happens to be healthy, which makes scores incomparable run-to-run.
# Pin to a strong model so judge baselines mean something; override with
# --judge-model. (A truly reliable judge needs a paid provider; this is the
# most consistent free model that still parses the SCORE=/REASON= contract.)
DEFAULT_JUDGE_MODEL = "free/groq/openai/gpt-oss-120b"
MAX_RECORDED_RESPONSE_CHARS = 20_000

KNOWN_ASSERTION_TYPES = {
    "contains",
    "count",
    "icontains",
    "not-contains",
    "regex",
    "starts-with",
    "min-length",
    "mentions-upstreams",
    "tool-used",
    "not-tool-used",
    "tool-sequence",
    "tool-order",
    "max-tool-calls",
    "min-tool-calls",
    "tool-count",
    "upstream-present",
    "not-upstream",
    "similar",
    "no-error",
}

# assertion type -> callable(output: str, run: dict, value) -> (passed: bool, detail: str)
ASSERTION_DEFAULTS = {
    "contains": lambda out, run, v: (v in out, f"found {v!r}" if v in out else f"missing {v!r}"),
    "icontains": lambda out, run, v: (
        v.lower() in out.lower(),
        f"found {v!r} (case-insensitive)" if v.lower() in out.lower() else f"missing {v!r}",
    ),
    "not-contains": lambda out, run, v: (v not in out, f"absent {v!r}" if v not in out else f"unexpected {v!r}"),
    "starts-with": lambda out, run, v: (
        out.strip().startswith(v),
        f"starts with {v!r}" if out.strip().startswith(v) else f"does not start with {v!r}",
    ),
    "min-length": lambda out, run, v: (
        len(out) >= int(v),
        f"len={len(out)} >= {v}" if len(out) >= int(v) else f"len={len(out)} < {v}",
    ),
    "regex": lambda out, run, v: (
        bool(re.search(v, out, re.IGNORECASE)),
        f"matches {v!r}" if re.search(v, out, re.IGNORECASE) else f"no match for {v!r}",
    ),
}


def load_fixture_turns(fixture: Path | None, prompt: str) -> list[str] | None:
    """Return the ordered list of user turns for a fixture run.

    A fixture may ship `turns.json` (a non-empty list of prompt strings) to
    exercise a multi-turn conversation: each turn runs against the SAME
    session, so later turns resume the earlier context (instruction
    retention). Without it the fixture is a single turn (prompt.md). Returns
    `None` (after printing an error) when turns.json is malformed.
    """
    if fixture is None:
        return [prompt]
    turns_file = fixture / "turns.json"
    if not turns_file.exists():
        return [prompt]
    try:
        turns = json.loads(turns_file.read_text())
    except (OSError, json.JSONDecodeError) as error:
        print(f"error: could not load {turns_file}: {error}", file=sys.stderr)
        return None
    if not isinstance(turns, list) or not turns or not all(
        isinstance(t, str) and t.strip() for t in turns
    ):
        print(
            f"error: {turns_file} must be a non-empty list of non-empty strings",
            file=sys.stderr,
        )
        return None
    return turns


def load_catalog_facts() -> dict:
    """Load source-derived catalog facts and reject stale fixture data.

    The checked-in JSON remains useful for review and offline tooling, but the
    live source is authoritative. Failing clearly here is safer than silently
    scoring a response against an obsolete provider order.
    """
    fixture_path = FIXTURES_DIR / "catalog-order" / "catalog_facts.json"
    fixture = json.loads(fixture_path.read_text()) if fixture_path.exists() else {}
    entries = parse_catalog(CATALOG_RS)
    if not entries:
        raise RuntimeError(f"could not parse any FreeUpstream entries from {CATALOG_RS}")

    current_hash = source_sha256(CATALOG_RS)
    current = {
        "source": str(CATALOG_RS.relative_to(REPO_ROOT)),
        "source_sha256": current_hash,
        "upstreams": entries,
        "ids": [entry["id"] for entry in entries],
    }
    if fixture:
        if fixture.get("source_sha256") != current_hash:
            raise RuntimeError(
                "catalog facts are stale; run "
                "python3 scripts/eval/derive_catalog_facts.py --out "
                "scripts/eval/fixtures/catalog-order/catalog_facts.json"
            )
        if fixture.get("upstreams") != entries or fixture.get("ids") != current["ids"]:
            raise RuntimeError("catalog facts do not match catalog.rs; regenerate catalog_facts.json")
    return current


def validate_expected(expected: dict) -> list[str]:
    """Return actionable schema errors for a fixture's expected.json."""
    errors = []
    if not isinstance(expected, dict):
        return ["expected.json must contain an object"]
    threshold = expected.get("threshold", 1.0)
    if not isinstance(threshold, (int, float)) or not 0 <= threshold <= 1:
        errors.append("threshold must be a number between 0 and 1")
    assertions = expected.get("assert", [])
    if assertions is not None and not isinstance(assertions, list):
        errors.append("assert must be an array")
        assertions = []
    for index, assertion in enumerate(assertions):
        if not isinstance(assertion, dict):
            errors.append(f"assert[{index}] must be an object")
            continue
        atype = assertion.get("type")
        if atype not in KNOWN_ASSERTION_TYPES:
            errors.append(f"assert[{index}] has unknown type {atype!r}")
            continue
        weight = assertion.get("weight", 1)
        if not isinstance(weight, (int, float)) or weight <= 0:
            errors.append(f"assert[{index}].weight must be positive")
        needs_value = atype not in {"upstream-present", "no-error", "mentions-upstreams"}
        if needs_value and "value" not in assertion:
            errors.append(f"assert[{index}] ({atype}) requires value")
        if atype == "mentions-upstreams" and (
            not isinstance(assertion.get("min", 3), int) or assertion.get("min", 3) < 0
        ):
            errors.append(f"assert[{index}].min must be a non-negative integer")
        if atype in {"tool-order"} and (
            not isinstance(assertion.get("value"), list) or len(assertion["value"]) != 2
        ):
            errors.append(f"assert[{index}].value must be a two-item list")
        if atype == "tool-sequence" and not isinstance(assertion.get("value"), list):
            errors.append(f"assert[{index}].value must be a list")
        if atype == "similar":
            similarity_threshold = assertion.get("threshold", 0.6)
            if not isinstance(similarity_threshold, (int, float)) or not 0 <= similarity_threshold <= 1:
                errors.append(f"assert[{index}].threshold must be between 0 and 1")
        if atype == "count":
            for bound in ("min", "max"):
                if bound in assertion and (
                    not isinstance(assertion[bound], int) or assertion[bound] < 0
                ):
                    errors.append(f"assert[{index}].{bound} must be a non-negative integer")
    judge = expected.get("judge")
    if judge is not None:
        if not isinstance(judge, dict):
            errors.append("judge must be an object")
        elif "min_score" in judge and (
            not isinstance(judge["min_score"], (int, float)) or not 0 <= judge["min_score"] <= 1
        ):
            errors.append("judge.min_score must be between 0 and 1")
    return errors


def build_assertions(expected: dict) -> list[dict]:
    """Return the assertion list from expected.json."""
    asserts = expected.get("assert", [])
    if not asserts:
        # Default: any non-trivial completion.
        asserts = [{"type": "min-length", "value": 50}]
    return asserts


def is_subsequence(needle: list, haystack: list) -> bool:
    """True when every element of `needle` appears in `haystack` in order."""
    it = iter(haystack)
    return all(any(x == y for y in it) for x in needle)


def run_assertions(
    asserts: list[dict],
    output: str,
    run: dict,
    catalog: dict,
    embedder: Embedder | None = None,
) -> list[dict]:
    results = []
    for a in asserts:
        atype = a.get("type", "contains")
        value = a.get("value")
        weight = a.get("weight", 1)
        if atype == "mentions-upstreams":
            ids = catalog.get("ids", [])
            min_hits = int(a.get("min", 3))
            hits = [
                identifier
                for identifier in ids
                if re.search(rf"(?<![A-Za-z0-9_-]){re.escape(identifier)}(?![A-Za-z0-9_-])", output)
            ]
            passed = len(hits) >= min_hits
            detail = f"{len(hits)}/{len(ids)} upstreams named (need >= {min_hits}): {', '.join(hits[:8])}"
            results.append({"type": atype, "passed": passed, "weight": weight, "detail": detail})
        elif atype == "tool-used":
            tools = [t for t in run.get("tools_used", [])]
            passed = value in tools
            results.append(
                {
                    "type": atype,
                    "passed": passed,
                    "weight": weight,
                    "detail": f"tool {value!r} used: {tools if passed else 'not seen in ' + str(tools)}",
                }
            )
        elif atype == "not-tool-used":
            tools = [t for t in run.get("tools_used", [])]
            passed = value not in tools
            results.append(
                {
                    "type": atype,
                    "passed": passed,
                    "weight": weight,
                    "detail": f"tool {value!r} not used: {'ok' if passed else 'seen in ' + str(tools)}",
                }
            )
        elif atype == "tool-sequence":
            # Ordered tool trajectory: `value` is a list of tool names. Default
            # mode asserts the list appears as an ordered subsequence (the
            # canonical "locate then read" pattern); mode=exact asserts the full
            # sequence matches, repeats included.
            seq = run.get("tool_sequence", [])
            want = list(value) if isinstance(value, list) else [value]
            mode = a.get("mode", "subsequence")
            if mode == "exact":
                passed = seq == want
            else:
                passed = is_subsequence(want, seq)
            results.append(
                {
                    "type": atype,
                    "passed": passed,
                    "weight": weight,
                    "detail": f"tool sequence {want} (mode={mode}): {'ok' if passed else 'actual ' + str(seq)}",
                }
            )
        elif atype == "tool-order":
            # `value` is a [A, B] pair: A must appear before B somewhere in the
            # tool trajectory. A convenient spelling of a 2-element subsequence.
            seq = run.get("tool_sequence", [])
            pair = list(value) if isinstance(value, list) else []
            passed = len(pair) == 2 and is_subsequence(pair, seq)
            results.append(
                {
                    "type": atype,
                    "passed": passed,
                    "weight": weight,
                    "detail": f"order {pair}: {'ok' if passed else 'actual ' + str(seq)}",
                }
            )
        elif atype in ("max-tool-calls", "min-tool-calls"):
            # Step-count gate: bound the total number of tool rounds so a run
            # that flails in a loop is caught. Operates on the full sequence
            # (repeats included), not the deduplicated tool list.
            n = len(run.get("tool_sequence", []))
            cap = int(value)
            if atype == "max-tool-calls":
                passed = n <= cap
                detail = f"{n} tool calls <= {cap}" if passed else f"{n} tool calls > {cap}"
            else:
                passed = n >= cap
                detail = f"{n} tool calls >= {cap}" if passed else f"{n} tool calls < {cap}"
            results.append({"type": atype, "passed": passed, "weight": weight, "detail": detail})
        elif atype == "tool-count":
            # `value` is a tool name; `min`/`max` bound how many times it fired.
            counts = run.get("tool_counts", {})
            n = counts.get(value, 0)
            lo = int(a.get("min", 0))
            hi = int(a.get("max", 10**9))
            passed = lo <= n <= hi
            results.append(
                {
                    "type": atype,
                    "passed": passed,
                    "weight": weight,
                    "detail": f"tool {value!r} used {n}x (need {lo}..{hi}): {'ok' if passed else 'out of range'}",
                }
            )
        elif atype == "upstream-present":
            passed = run.get("upstream_id") is not None
            results.append(
                {
                    "type": atype,
                    "passed": passed,
                    "weight": weight,
                    "detail": f"upstream attribution present: {run.get('upstream_id') or 'MISSING'}",
                }
            )
        elif atype == "not-upstream":
            served = run.get("upstream_id")
            passed = served is not None and served != value
            results.append(
                {
                    "type": atype,
                    "passed": passed,
                    "weight": weight,
                    "detail": f"served by {served} (expected != {value})",
                }
            )
        elif atype == "no-error":
            passed = run.get("error") is None
            results.append(
                {"type": atype, "passed": passed, "weight": weight, "detail": run.get("error") or "no error"}
            )
        elif atype == "count":
            # Regex-count assertion (IFEval-style verifiable constraint):
            # `value` is a regex; the response must contain between `min`
            # (default 1) and `max` (default unbounded) matches. `finditer`
            # counts whole matches regardless of capture groups.
            n = len(list(re.finditer(value, output)))
            lo = int(a.get("min", 1))
            hi = int(a.get("max", 10**9))
            passed = lo <= n <= hi
            results.append(
                {
                    "type": atype,
                    "passed": passed,
                    "weight": weight,
                    "detail": f"{n} matches of {value!r} (need {lo}..{hi})",
                }
            )
        elif atype == "similar":
            # Semantic-similarity assertion (promptfoo-style): cosine of the
            # answer vs golden reference text. Uses free-provider embeddings
            # when reachable, else a deterministic lexical fallback.
            golden = str(value or "")
            threshold = float(a.get("threshold", 0.6))
            if embedder is None or not golden:
                results.append(
                    {
                        "type": atype,
                        "passed": False,
                        "weight": weight,
                        "detail": "similar needs a golden 'value' and an embedder",
                    }
                )
            else:
                sim, method = embedder.similarity(output, golden)
                passed = sim >= threshold
                results.append(
                    {
                        "type": atype,
                        "passed": passed,
                        "weight": weight,
                        "detail": f"similarity={sim:.3f} >= {threshold} (method={method})",
                    }
                )
        elif atype in ASSERTION_DEFAULTS:
            passed, detail = ASSERTION_DEFAULTS[atype](output, run, value)
            results.append({"type": atype, "passed": passed, "weight": weight, "detail": detail})
        else:
            results.append(
                {"type": atype, "passed": False, "weight": weight, "detail": f"unknown assertion type {atype!r}"}
            )
    return results


def score_results(results: list[dict], threshold: float = 1.0) -> tuple[float, bool]:
    total_w = sum(r["weight"] for r in results) or 1.0
    score = sum(r["weight"] for r in results if r["passed"]) / total_w
    return score, score >= threshold


def parse_stream_events(lines: list[tuple[float, str]], started: float) -> dict:
    """Turn timestamped NDJSON lines into a run record."""
    run = {
        "text": [],
        "text_deltas": 0,
        "first_text_delta_ms": None,
        "upstream_id": None,
        "model": None,
        "context_tokens_est": None,
        "retries": None,
        "fallback_used": None,
        "cost_usd": None,
        "tools_used": [],
        "tool_sequence": [],
        "tool_counts": {},
        "tool_errors": [],
        "status": [],
        "verify": [],
        "error": None,
        "result_event": None,
        "provider_id": None,
        "skipped_lines": 0,
    }
    for ts, line in lines:
        line = line.strip()
        if not line:
            continue
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            run["skipped_lines"] += 1
            continue
        etype = ev.get("type")
        if etype == "text_delta":
            run["text"].append(ev.get("text", ""))
            run["text_deltas"] += 1
            if run["first_text_delta_ms"] is None:
                run["first_text_delta_ms"] = int((ts - started) * 1000)
        elif etype == "provider_attribution":
            run["provider_id"] = ev.get("provider_id")
            run["upstream_id"] = ev.get("upstream_id")
            run["model"] = ev.get("model")
            run["context_tokens_est"] = ev.get("context_tokens_est")
            run["retries"] = ev.get("retries")
            run["fallback_used"] = ev.get("fallback_used")
        elif etype == "tool_start":
            tool = ev.get("tool")
            if tool:
                if tool not in run["tools_used"]:
                    run["tools_used"].append(tool)
                run["tool_sequence"].append(tool)
                run["tool_counts"][tool] = run["tool_counts"].get(tool, 0) + 1
        elif etype == "tool_end":
            if ev.get("is_error") and ev.get("tool"):
                run["tool_errors"].append(f"{ev['tool']}: {ev.get('error_code', 'tool_error')}")
        elif etype == "status":
            run["status"].append(ev.get("status", ""))
        elif etype == "verify":
            run["verify"].append(ev.get("report", {}))
        elif etype == "error":
            run["error"] = ev.get("error")
        elif etype == "result":
            run["result_event"] = ev
            run["cost_usd"] = ev.get("cost_usd")
            # The result event also carries the full attribution summary.
            for key in ("provider", "upstream", "model", "retries", "fallback_used", "context_tokens_est"):
                if ev.get(key) is not None:
                    run[key.replace("upstream", "upstream_id")] = ev.get(key)
    run["response_text"] = "".join(run["text"])
    run["response_chars"] = len(run["response_text"])
    return run


def seed_home(home: Path, auth_file: Path, sabotage: list[str] | None) -> None:
    """Seed an isolated CLAWDE_HOME with a copy of the auth store (+settings)."""
    home.mkdir(parents=True, exist_ok=True)
    # Auth store: copy real keys; optionally sabotage upstreams with invalid
    # but >= 8-char placeholders so the resolver's placeholder guard lets them
    # through and the chain must fall through to a later upstream.
    auth = {}
    if auth_file.exists():
        try:
            loaded = json.loads(auth_file.read_text())
            if isinstance(loaded, dict):
                auth = loaded
            else:
                print(f"warning: auth store {auth_file} is not a JSON object; continuing with empty keys")
        except (json.JSONDecodeError, OSError) as e:
            print(f"warning: could not parse auth store {auth_file}: {e}; continuing with empty keys")
    keys = auth.get("keys")
    if not isinstance(keys, dict):
        keys = {}
        auth["keys"] = keys
    for up in sabotage or []:
        if up in keys:
            keys[up] = [f"sabotaged-invalid-key-{up[:20]}-{uuid.uuid4().hex[:6]}"]
        else:
            print(f"warning: --sabotage {up}: no keys for that upstream in the auth store; nothing to replace")
    (home / "auth.json").write_text(json.dumps(auth, indent=2))
    # Minimal settings: no hooks, no auto-compact, onboarding done so headless
    # mode never waits on interactive setup.
    settings = {
        "auto_compact": False,
        "verbose": False,
        "hasCompletedOnboarding": True,
        "hooks": {},
    }
    (home / "settings.json").write_text(json.dumps(settings, indent=2))
    for sub in ("sessions", "key-ring-state", "free-state", "projects"):
        (home / sub).mkdir(parents=True, exist_ok=True)


def fixture_label(path: Path | None) -> str | None:
    if path is None:
        return None
    try:
        return str(path.resolve().relative_to(REPO_ROOT))
    except ValueError:
        return str(path.resolve())


def append_result(path: Path, record: dict) -> None:
    """Append one JSONL record without interleaving concurrent eval runs."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a") as stream:
        try:
            import fcntl

            fcntl.flock(stream.fileno(), fcntl.LOCK_EX)
        except (ImportError, AttributeError, OSError):
            pass
        try:
            stream.write(json.dumps(record) + chr(10))
            stream.flush()
        finally:
            try:
                import fcntl

                fcntl.flock(stream.fileno(), fcntl.LOCK_UN)
            except (ImportError, AttributeError, OSError):
                pass


def build_binary_if_needed(binary: Path) -> None:
    if binary.exists():
        return
    print("debug binary missing; building (this can take a while on first run)...")
    subprocess.run(["cargo", "build"], cwd=SRC_RUST, check=True)


def _read_process_stream(kind: str, stream, events: Queue) -> None:
    try:
        for raw in stream:
            events.put((kind, time.monotonic(), raw))
    finally:
        events.put((kind, None, None))


def _kill_process_tree(proc: subprocess.Popen) -> None:
    """Kill the child and descendants so a timeout cannot leak a Clawde run."""
    try:
        if os.name == "posix":
            os.killpg(proc.pid, signal.SIGKILL)
        else:
            proc.kill()
    except (ProcessLookupError, PermissionError):
        pass


def run_process_stream(
    cmd: list[str], *, env: dict[str, str], timeout: float
) -> tuple[list[tuple[float, str]], str, int | None, bool, int, float]:
    """Run a command while draining both pipes and enforcing a real deadline.

    The returned timestamps use the same monotonic clock as
    :func:`parse_stream_events`, which keeps TTFT measurements meaningful.
    """
    started = time.monotonic()
    proc = subprocess.Popen(
        cmd,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
        env=env,
        start_new_session=os.name == "posix",
    )
    if proc.stdout is None or proc.stderr is None:
        _kill_process_tree(proc)
        proc.wait()
        return [], "missing child pipes", None, False, int((time.monotonic() - started) * 1000), started

    events: Queue = Queue()
    readers = [
        threading.Thread(target=_read_process_stream, args=("stdout", proc.stdout, events), daemon=True),
        threading.Thread(target=_read_process_stream, args=("stderr", proc.stderr, events), daemon=True),
    ]
    for reader in readers:
        reader.start()

    lines: list[tuple[float, str]] = []
    stderr_lines: list[str] = []
    closed_streams = 0
    timed_out = False
    deadline = started + max(0.01, timeout)
    while closed_streams < 2:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            if proc.poll() is None:
                timed_out = True
                _kill_process_tree(proc)
            break
        try:
            kind, timestamp, raw = events.get(timeout=min(0.1, remaining))
        except Empty:
            continue
        if raw is None:
            closed_streams += 1
        elif kind == "stdout":
            lines.append((timestamp, raw))
        else:
            stderr_lines.append(raw)

    if timed_out:
        try:
            proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            _kill_process_tree(proc)
            proc.wait()
    else:
        proc.wait()

    # The process is gone; let readers consume EOF without allowing a broken
    # descendant to hold the harness open indefinitely.
    for reader in readers:
        reader.join(timeout=1)
    for stream in (proc.stdout, proc.stderr):
        if stream is not None:
            stream.close()
    while True:
        try:
            kind, timestamp, raw = events.get_nowait()
        except Empty:
            break
        if raw is None:
            continue
        if kind == "stdout":
            lines.append((timestamp, raw))
        else:
            stderr_lines.append(raw)
    stderr = "".join(stderr_lines)
    return lines, stderr, None if timed_out else proc.returncode, timed_out, int((time.monotonic() - started) * 1000), started


def run_headless(
    prompt: str,
    *,
    binary: Path,
    model: str,
    cwd: Path,
    home: Path,
    max_turns: int,
    timeout: float,
    session_id: str,
    permission_mode: str | None = None,
    resume: bool = False,
) -> tuple[dict, list[tuple[float, str]]]:
    cmd = [
        str(binary),
        "--print",
        prompt,
        "--output-format", "stream-json",
        "--model", model,
        "--max-turns", str(max_turns),
        "--session-id", session_id,
        "--no-auto-compact",
        "--cwd", str(cwd),
    ]
    if resume:
        # Continue the persisted conversation: the CLI loads the session's
        # prior messages only when `--resume` is present (--session-id alone
        # just names the session). Same id on both flags — the CLI validates
        # they match.
        cmd.extend(["--resume", session_id])
    if permission_mode:
        cmd.extend(["--permission-mode", permission_mode])
    env = dict(os.environ)
    env["CLAWDE_HOME"] = str(home)
    lines, stderr, exit_code, timed_out, total_ms, started = run_process_stream(
        cmd, env=env, timeout=timeout
    )
    run = parse_stream_events(lines, started)
    run["exit_code"] = exit_code
    run["total_ms"] = total_ms
    run["stderr_tail"] = chr(10).join(stderr.splitlines()[-5:])
    if timed_out:
        run["error"] = f"harness timeout after {timeout:.0f}s"
    elif exit_code != 0 and not run["error"]:
        run["error"] = f"clawde exited {exit_code}"
    return run, lines


def utcnow() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z"


def parse_judge_output(text: str) -> tuple[float | None, str]:
    """Extract (score, reason) from the judge's response.

    Accepts bare JSON, fenced JSON, or JSON embedded in prose, and normalizes
    0-1, 0-10, and 0-100 scales to 0-1. Returns (None, reason) when no score
    could be parsed.
    """
    score = None
    for m in re.finditer(
        r'"?score"?\s*[:=]\s*(\d+(?:\.\d+)?|\.\d+)', text, re.IGNORECASE
    ):
        raw = float(m.group(1))
        if raw > 10.0:
            raw /= 100.0
        elif raw > 1.0:
            raw /= 10.0
        score = max(0.0, min(1.0, raw))
        break
    reason = ""
    # Prefer the simple `REASON=...` contract, fall back to JSON `"reason": "..."`.
    for m in re.finditer(r'"?reason"?\s*[:=]\s*"?([^"\n]+)', text, re.IGNORECASE):
        reason = m.group(1).strip()
        break
    return score, reason


def run_judge(
    prompt: str,
    output: str,
    rubric: str,
    *,
    binary: Path,
    model: str,
    cwd: Path,
    home: Path,
    max_turns: int,
    session_id: str,
    timeout: float,
    permission_mode: str | None = None,
) -> dict:
    """Grade the response with a pinned LLM judge via a fresh headless run.

    The judge is advisory: deterministic assertions remain the authoritative
    gate (live_smoke.rs philosophy). Returns a bounded record.
    """
    bounded = output[:8000]
    judge_prompt = (
        "You are a strict, fair judge. Grade the response below against the "
        "rubric. Be critical: dock points for omissions and inaccuracies.\n\n"
        f"RUBRIC:\n{rubric}\n\n"
        f"RESPONSE:\n{bounded}\n\n"
        "Reply with EXACTLY two lines and nothing else:\n"
        "SCORE=<a decimal number between 0 and 1, e.g. 0.8>\n"
        "REASON=<one short sentence>\n"
        "Write the score as a plain digit like 0.85 — never empty, never a "
        "placeholder, never a dash."
    )
    judge_session = f"{session_id}-judge"
    last_text = ""
    last_error = None
    # The free judge is noisy: a single run can score an identical answer
    # anywhere from 0.0 to 0.85. Take the median of up to 3 parsed scores to
    # damp single-run flakiness (mirrors live_smoke's strict-parse resilience:
    # unparseable output gets a corrective attempt quoting the malformed text).
    scores = []
    reason = ""
    for attempt in range(3):
        run, _ = run_headless(
            judge_prompt,
            binary=binary,
            model=model,
            cwd=cwd,
            home=home,
            max_turns=1,
            timeout=min(timeout, 120.0),
            # Every score must be an independent conversation. Reusing one
            # session would append prompts and make the median measure history
            # contamination rather than judge variance.
            session_id=f"{judge_session}-{attempt + 1}",
            permission_mode=permission_mode,
        )
        last_text = run.get("response_text", "")
        last_error = run.get("error")
        s, r = parse_judge_output(last_text)
        if s is not None:
            scores.append(s)
            reason = r
        else:
            judge_prompt += (
                "\n\nYour previous answer could not be parsed. You returned:\n"
                f"```\n{last_text[:400]}\n```\n"
                "Fix it. Reply with EXACTLY two lines:\n"
                "SCORE=0.75\n"
                "REASON=the score must be a plain digit, not a placeholder."
            )
    score = float(median(scores)) if scores else None
    return {
        "score": round(score, 3) if score is not None else None,
        "reason": reason[:300],
        "model": model,
        "attempts": len(scores),
        "response_chars": run.get("response_chars", 0),
        "error": None if score is not None else (last_error or "unparseable_score"),
        "raw_excerpt": last_text[:400],
    }


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--fixture", type=Path, help="Fixture dir containing prompt.md + expected.json")
    ap.add_argument("--prompt", type=str, help="Prompt text (used when --fixture is absent)")
    ap.add_argument("--model", default="free/auto", help="Model to dispatch (default free/auto)")
    ap.add_argument("--max-turns", type=int, default=12, help="Max agentic turns (default 12)")
    ap.add_argument("--timeout", type=float, default=300, help="Hard timeout seconds (default 300)")
    ap.add_argument(
        "--permission-mode",
        choices=("default", "accept-edits", "bypass-permissions", "plan"),
        default=None,
        help="Permission mode for headless runs (use plan for read-only eval fixtures)",
    )
    ap.add_argument("--cwd", type=Path, default=None, help="Working dir for the run (default: repo src-rust)")
    ap.add_argument("--binary", type=Path, default=DEFAULT_BINARY, help="Path to the clawde binary")
    ap.add_argument("--auth-file", type=Path, default=DEFAULT_AUTH, help="Auth store to seed the isolated home from")
    ap.add_argument("--sabotage", action="append", default=[], help="Replace this upstream's keys with invalid ones (repeatable)")
    ap.add_argument("--session-id", default=None, help="Session id tag (default: eval-<timestamp>)")
    ap.add_argument("--output", type=Path, default=None, help="Dir for report.json (default scripts/eval/results/<ts>)")
    ap.add_argument("--results", type=Path, default=Path(DEFAULT_RESULTS), help="JSONL index path (default scripts/eval/results/results.jsonl)")
    ap.add_argument("--no-results", action="store_true", help="Do not append to the JSONL trend index")
    ap.add_argument("--keep-home", action="store_true", help="Keep the isolated CLAWDE_HOME for inspection")
    ap.add_argument("--tag", default="", help="Free-form tag recorded in the report (e.g. 'before-change')")
    ap.add_argument("--quiet", action="store_true", help="Only print the summary when the eval fails (for pre-commit gating)")
    ap.add_argument("--judge", action="store_true", help="Grade the response with an LLM judge (G-Eval style rubric grading)")
    ap.add_argument("--judge-model", default=DEFAULT_JUDGE_MODEL, help=f"Model for the judge (default: {DEFAULT_JUDGE_MODEL}; pin for comparability)")
    ap.add_argument("--judge-rubric", default=None, help="Rubric text for the judge (default: generic; fixture 'judge.rubric' wins)")
    args = ap.parse_args()

    if args.fixture:
        fixture = args.fixture.resolve()
        prompt_file = fixture / "prompt.md"
        expected_file = fixture / "expected.json"
        turns_file = fixture / "turns.json"
        # A conversation fixture (turns.json) needs expected.json only;
        # prompt.md is required for single-turn fixtures.
        if not expected_file.exists() or (not prompt_file.exists() and not turns_file.exists()):
            print(
                f"error: fixture {fixture} needs expected.json plus prompt.md or turns.json",
                file=sys.stderr,
            )
            return 1
        try:
            prompt = prompt_file.read_text().strip() if prompt_file.exists() else ""
            expected = json.loads(expected_file.read_text())
        except (OSError, json.JSONDecodeError) as error:
            print(f"error: could not load fixture {fixture}: {error}", file=sys.stderr)
            return 1
    elif args.prompt:
        prompt = args.prompt
        expected = {}
    else:
        ap.print_usage()
        print("error: provide --fixture DIR or --prompt TEXT", file=sys.stderr)
        return 1

    turns = load_fixture_turns(args.fixture, prompt)
    if turns is None:
        return 1
    prompt = turns[0]

    schema_errors = validate_expected(expected)
    if schema_errors:
        print(f"error: invalid fixture configuration for {args.fixture or '<prompt>'}:", file=sys.stderr)
        for error in schema_errors:
            print(f"  - {error}", file=sys.stderr)
        return 1

    build_binary_if_needed(args.binary)
    try:
        catalog = load_catalog_facts()
    except (OSError, json.JSONDecodeError, RuntimeError) as error:
        print(f"error: catalog facts unavailable: {error}", file=sys.stderr)
        return 1

    # Deterministic fallback testing: when an upstream is sabotaged and no
    # explicit model was requested, pin to the sabotaged upstream's default
    # model. The pinned route tries that upstream first, its fake key fails,
    # and the chain MUST fall through to a later upstream — so `fallback_used`
    # and the surviving upstream are observable on every run.
    if args.sabotage and args.model == "free/auto":
        first = args.sabotage[0]
        defaults = {u["id"]: u["default_model"] for u in catalog.get("upstreams", [])}
        if first in defaults and defaults[first]:
            args.model = f"{first}/{defaults[first]}"
            print(f"--sabotage {first}: pinned model to {args.model} so the chain must fall through")

    session_id = args.session_id or f"eval-{int(time.time())}"
    ts = utcnow().replace(":", "").replace("-", "").replace("Z", "")
    home = Path(tempfile.mkdtemp(prefix=f"clawde-eval-{session_id}-"))
    try:
        seed_home(home, args.auth_file, args.sabotage)
        # Capture the HF token before the temp home is removed, so the
        # embedder (built after cleanup) can still reach the inference API.
        seeded_hf_token = hf_token_from_auth(str(home / "auth.json"))
        workdir = (args.cwd or SRC_RUST).resolve()
        # Conversation fixtures (turns.json): each turn runs against the same
        # isolated home + session id, resuming the previous context, so a
        # later turn must still honor constraints stated in an earlier one.
        # The last turn's response is what the assertions and judge evaluate.
        runs = []
        for turn_index, turn_prompt in enumerate(turns):
            run, _ = run_headless(
                turn_prompt,
                binary=args.binary,
                model=args.model,
                cwd=workdir,
                home=home,
                max_turns=args.max_turns,
                timeout=args.timeout,
                session_id=session_id,
                permission_mode=args.permission_mode,
                resume=turn_index > 0,
            )
            run["turn_index"] = turn_index
            run["turn_prompt"] = turn_prompt[:200]
            runs.append(run)
        run = runs[-1]
        run["turn_count"] = len(runs)
        # Lightweight per-turn ledger so reports show whether every turn ran
        # (e.g. that turn 1 actually investigated before turn 2 answered).
        run["turns"] = [
            {
                "turn_index": r.get("turn_index", index),
                "error": r.get("error"),
                "tools_used": len(r.get("tool_sequence", [])),
                "text_deltas": r.get("text_deltas", 0),
                "response_chars": r.get("response_chars", 0),
            }
            for index, r in enumerate(runs)
        ]
        if len(runs) > 1:
            run["prior_turn_errors"] = [
                r.get("error") for r in runs[:-1] if r.get("error")
            ]
            # A conversation fixture measures retention ACROSS turns: if an
            # earlier turn failed (provider timeout, crash), the session the
            # final turn resumed may be incomplete, so the measurement is
            # invalid. Fail the run instead of letting a partial conversation
            # go green on a lucky answer.
            if run["prior_turn_errors"]:
                run["error"] = (
                    "conversation turn(s) failed: "
                    + "; ".join(run["prior_turn_errors"])
                )
        # LLM-as-judge tier (G-Eval style): grade the response with a second
        # headless run inside the same isolated home. Advisory only — the
        # deterministic assertions remain the authoritative gate.
        judge_result = None
        if args.judge and run.get("response_chars", 0) > 0 and not run.get("error"):
            fixture_judge = expected.get("judge") if isinstance(expected.get("judge"), dict) else {}
            rubric = (
                fixture_judge.get("rubric")
                or args.judge_rubric
                or DEFAULT_JUDGE_RUBRIC
            )
            judge_model = args.judge_model or DEFAULT_JUDGE_MODEL
            judge_result = run_judge(
                prompt,
                run.get("response_text", ""),
                rubric,
                binary=args.binary,
                model=judge_model,
                cwd=workdir,
                home=home,
                max_turns=1,
                session_id=session_id,
                timeout=args.timeout,
                permission_mode=args.permission_mode,
            )
            judge_result["rubric"] = rubric[:200]
            if fixture_judge.get("min_score") is not None:
                judge_result["min_score"] = fixture_judge["min_score"]
        elif args.judge:
            judge_result = {
                "score": None,
                "reason": "primary run was not evaluable",
                "model": args.judge_model,
                "attempts": 0,
                "response_chars": 0,
                "error": "primary_run_not_evaluable",
                "raw_excerpt": "",
            }
    finally:
        if not args.keep_home:
            shutil.rmtree(home, ignore_errors=True)

    # Drop the raw stream from the persisted record; keep a bounded excerpt.
    output = run.get("response_text", "")
    run["response_excerpt"] = output[:600]
    if len(output) > MAX_RECORDED_RESPONSE_CHARS:
        run["response_text"] = output[:MAX_RECORDED_RESPONSE_CHARS]
        run["response_truncated"] = True
    run.pop("text", None)
    run.pop("status", None)

    # A completed run that emitted no text (free providers intermittently
    # return empty completions) is not evaluable — treat it like a provider
    # error so consumers (pre-commit gate) warn instead of blocking.
    if run.get("response_chars", 0) == 0 and not run.get("error"):
        run["error"] = "empty_completion (no text emitted)"

    asserts = build_assertions(expected)
    # When an upstream is sabotaged, the run MUST be served by a different
    # upstream — with the auto-pin above, that is the deterministic proof that
    # the chain fell through past the dead upstream.
    if args.sabotage:
        asserts.append({"type": "not-upstream", "value": args.sabotage[0], "weight": 3})

    # Embedder for `similar` assertions, seeded with the HF token captured
    # from the isolated home's auth store before cleanup.
    embedder = Embedder(seeded_hf_token)

    results = run_assertions(asserts, output, run, catalog, embedder)
    # Fixture-level weighted threshold (default: every assertion must pass).
    score, passed = score_results(results, float(expected.get("threshold", 1.0)))

    # Judge gate (opt-in per fixture via `judge.min_score`): when the advisory
    # judge produced a numeric score below the fixture's floor, fail the run.
    # This is the regression baseline's hard edge — a fixture that opts in
    # refuses to go green on a degraded answer even if its deterministic
    # assertions still pass. Never triggered when the judge could not parse.
    judge_min = None
    if isinstance(expected.get("judge"), dict):
        judge_min = expected.get("judge").get("min_score")
    judge_gate_failed = False
    if judge_min is not None and judge_result and judge_result.get("score") is not None:
        judge_gate_failed = judge_result["score"] < float(judge_min)
        if judge_gate_failed:
            passed = False

    report = {
        "schema_version": "clawde-eval.v2",
        "ts": utcnow(),
        "session_id": session_id,
        "tag": args.tag,
        "fixture": fixture_label(args.fixture),
        "model": args.model,
        "sabotage": args.sabotage,
        "prompt_chars": len(prompt),
        "run": {k: v for k, v in run.items() if k not in ("stderr_tail",)},
        "assertions": results,
        "judge": judge_result,
        "judge_gate": {
            "min_score": judge_min,
            "failed": judge_gate_failed,
        },
        "score": round(score, 3),
        "passed": passed,
    }

    out_dir = (args.output or RESULTS_DIR / ts).resolve()
    out_dir.mkdir(parents=True, exist_ok=True)
    (out_dir / "report.json").write_text(json.dumps(report, indent=2))
    (out_dir / "prompt.txt").write_text(prompt)
    if not args.no_results:
        append_result(
            args.results,
            {
                "schema_version": "clawde-eval.v2",
                "score": round(score, 3),
                "passed": passed,
                "session_id": session_id,
                "ts": report["ts"],
                "tag": args.tag,
                "fixture": report["fixture"],
                "upstream": run.get("upstream_id"),
                "model": run.get("model"),
                "ttft_ms": run.get("first_text_delta_ms"),
                "total_ms": run.get("total_ms"),
                "response_chars": run.get("response_chars"),
                "cost_usd": run.get("cost_usd"),
                "fallback_used": run.get("fallback_used"),
                "error": run.get("error"),
                "judge_score": (judge_result or {}).get("score"),
                "judge_model": (judge_result or {}).get("model"),
                "tool_calls": len(run.get("tool_sequence", [])),
            },
        )

    if not args.quiet or not passed or run.get("error"):
        print(f"\n=== eval {session_id} ({args.model}) ===")
        print(f"  upstream : {run.get('upstream_id')}  model: {run.get('model')}")
        print(f"  ttft     : {run.get('first_text_delta_ms')} ms   total: {run.get('total_ms')} ms")
        print(f"  chars    : {run.get('response_chars')}   cost: ${run.get('cost_usd') or 0:.6f}")
        print(f"  tools    : {run.get('tools_used') or []}")
        print(f"  fallback : {run.get('fallback_used')}   retries: {run.get('retries')}")
        if run.get("error"):
            print(f"  error    : {run['error']}")
        for r in results:
            mark = "PASS" if r["passed"] else "FAIL"
            print(f"  [{mark}] {r['type']}: {r['detail']}")
        if judge_result and judge_result.get("score") is not None:
            gate = f" (min {judge_min})" if judge_min is not None else ""
            flag = "  <-- below gate" if judge_gate_failed else ""
            print(f"  judge    : {judge_result['score']:.2f}{gate} [{judge_result.get('model', '')}]{flag}")
        print(f"  score    : {score:.3f}  {'PASS' if passed else 'FAIL'}")
        print(f"  report   : {out_dir / 'report.json'}")
    # Exit codes: 0 = pass, 1 = assertions failed, 2 = run could not be
    # evaluated (provider/timeout error). Consumers like the pre-commit hook
    # treat 2 as infra flakiness, not a quality regression.
    if not passed:
        if run.get("error"):
            return 2
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
