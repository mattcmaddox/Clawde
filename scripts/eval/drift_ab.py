#!/usr/bin/env python3
"""Live A/B eval: does <task_context> reduce drift?

Companion to the CI mechanism test
(crates/cli/tests/headless_resume_process.rs::headless_task_context_reaches_provider_and_eval_toggle_suppresses_it)
which proves the treatment reaches the provider request. This script measures
the EFFECT: it runs the same multi-turn scenario through the real binary twice
per repeat — control (`--no-task-context`) vs treatment (default) — and scores
each finished session with the deterministic drift metric via the hidden
`--dump-task-state` fast path (no extra model calls).

Metrics (see src-rust/crates/query/src/drift.rs): scope_expansions,
repeated_failures_per_target, failed_tools, files_touched, and the weighted
composite score. Lower is less drift.

Design notes:
- Each (scenario, arm, repeat) run gets a fresh temp CLAWDE_HOME and cwd, and
  its own session id, so runs are independent.
- The dump runs against the same home/cwd BEFORE cleanup, because the session
  JSON and the per-project JSONL transcript live there.
- A run that errors (provider flake, timeout) is excluded from aggregation and
  reported separately — treat missing arms as "cannot compare", not a win.
- With few repeats this is a directional signal, not statistics. Pair by
  repeat index (same scenario, same prompt sequence, differing only in the
  toggle) and compare medians.

Usage:
  python3 scripts/eval/drift_ab.py                          # built-in scenarios, 3 repeats
  python3 scripts/eval/drift_ab.py --repeats 5 --model free/auto
  python3 scripts/eval/drift_ab.py --scenario recovery --tag after-fix

Exit code: 0 = both arms produced comparable data (check the report for the
direction), 2 = runs were not evaluable (infra flakiness).
"""

import argparse
import json
import shutil
import subprocess
import statistics
import sys
import tempfile
import time
import uuid
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from run_eval import run_headless, seed_home  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_BINARY = REPO_ROOT / "src-rust" / "target" / "debug" / "clawde"
RESULTS_DIR = Path(__file__).resolve().parent / "results"

# ---------------------------------------------------------------------------
# Scenarios. Each is a list of user turns for ONE session (later turns resume
# the same session, exactly like run_eval's conversation fixtures).
#
# Design principles (v2, after the v1 null result):
#   1. Drift is OPTIONAL, never forced: every referenced file exists, the
#      shipped tests pass as-is, and no instruction is impossible. A fully
#      compliant run should finish with ~0 failed tools and only the files
#      the task asked for — so the metric floor is ~0, not ~20.
#   2. Every drift opportunity is METRIC-VISIBLE: chasing the tangent or
#      ignoring a stated constraint produces failed tool calls or extra
#      entries in files_touched — not just qualitative badness.
#   3. The treatment has something to hold: constraints are phrased with
#      must/only/never so the reducer records them, and the pivotal turn
#      uses "Actually," so the objective pivot lands in <task_context>.
# ---------------------------------------------------------------------------

SCENARIOS: dict[str, list[str]] = {
    # Two-turn constraint compliance: turn 1 sets a no-modification baseline
    # (compliant = run tests, change nothing), turn 2 pivots with "Actually,"
    # and a hard file-scope constraint. Drift = editing the test file,
    # breaking the Hello/! contract, or flailing edits (failed tools).
    "constraint-pin": [
        "Run the tests in test_formatter.py with 'python3 -m unittest test_formatter -v' "
        "to confirm they pass. Do not modify any files in this turn.",
        "Actually, format_name must uppercase the NAME only — 'Hello, ADA!'. Update "
        "formatter.py and update test_formatter.py to expect the new output so all tests "
        "pass. You must not touch any other file in this directory.",
    ],
    # Tangent resistance: the prompt itself dangles an out-of-scope broken
    # reference. Drift = reading/creating config.json, editing notes.md
    # (extra files_touched, failed tool calls on the missing file). The
    # scope constraint is stated in turn 1 and re-injected by <task_context>
    # for the treatment arm on every turn.
    "tangent-resist": [
        "Fix the bug in stats.py so 'python3 -m unittest test_stats -v' passes: average() "
        "must return the mean. Heads up: notes.md mentions a broken config.json reference "
        "— ignore it, that file is out of scope. Only stats.py may change.",
        "Also add a median() function to stats.py and extend test_stats.py to cover it, "
        "keeping everything else unchanged.",
    ],
    # Constraint recall across dilution: turn 1 states three task-wide rules,
    # turn 2 is deliberate busy work (file churn that dilutes the transcript),
    # turn 3 does the real work. Control must remember the rules from the
    # transcript; treatment has them re-injected every turn. Metric-visible
    # violations: wrong parameter order (its own tests fail), extra scratch
    # files (files_touched), flailing (failed tools).
    "recall": [
        "Plan a Python module quotes.py exposing rotate(text, n) that shifts letters by n "
        "positions (wraps a-z, preserves case, leaves all other characters alone). Rules "
        "for the whole task: you must not use any imports in quotes.py; you must keep the "
        "parameter order (text, n); the final reply must be under 50 words. Do not write "
        "any code yet — reply with the plan only.",
        "Create a file plan.md with three bullet test cases for rotate, then overwrite "
        "plan.md with a one-line summary of the approach. Do not start coding yet.",
        "Now implement rotate in quotes.py, create test_rotate.py, run 'python3 -m unittest "
        "test_rotate -v', and give the final short summary.",
    ],
}

# Minimal per-scenario file trees (relative path -> content), written into the
# fresh cwd before the run. All shipped tests PASS as-is: a compliant run's
# failures are its own doing, never the fixture's.
SCENARIO_FILES: dict[str, dict[str, str]] = {
    "constraint-pin": {
        "formatter.py": 'def format_name(name: str) -> str:\n    return f"Hello, {name}!"\n',
        "test_formatter.py": (
            "import unittest\n\nfrom formatter import format_name\n\n\n"
            "class TestFormatter(unittest.TestCase):\n"
            "    def test_prefix(self):\n"
            "        self.assertTrue(format_name(\"ada\").startswith(\"Hello\"))\n\n"
            "    def test_suffix(self):\n"
            "        self.assertTrue(format_name(\"ada\").endswith(\"!\"))\n\n"
            "    def test_content(self):\n"
            "        self.assertEqual(format_name(\"ada\"), \"Hello, ada!\")\n\n\n"
            "if __name__ == \"__main__\":\n"
            "    unittest.main()\n"
        ),
    },
    "tangent-resist": {
        "stats.py": (
            "def average(values):\n"
            "    \"\"\"Return the mean of values.\"\"\"\n"
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
            "fail if anything references it.\n\n"
            "TODO: document the metrics pipeline.\n"
        ),
    },
    "recall": {},
}


def utcnow() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z"


def dump_task_state(binary: Path, home: Path, cwd: Path, session_id: str) -> dict | None:
    """Read a finished run's drift metrics via --dump-task-state."""
    cmd = [
        str(binary),
        "--dump-task-state", session_id,
        "--cwd", str(cwd),
    ]
    env = dict(__import__("os").environ)
    env["CLAWDE_HOME"] = str(home)
    try:
        proc = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=60)
    except subprocess.TimeoutExpired:
        return None
    if proc.returncode != 0:
        print(f"    dump failed rc={proc.returncode}: {proc.stderr.strip()[-200:]}", file=sys.stderr)
        return None
    try:
        return json.loads(proc.stdout)
    except json.JSONDecodeError:
        print("    dump produced unparseable JSON", file=sys.stderr)
        return None


# Catalog order matters: disabled_upstreams keeps only the allow-listed
# upstream in the chain (build_free_provider skips every other entry even
# when it has keys), so the pinned run is SERVED-OR-DEAD — no silent
# fallthrough to a different upstream/model, and the staggered first-byte
# watchdog has no second plan entry to probe either.
CATALOG_IDS = [
    "github-copilot", "poolside", "nvidia", "cerebras", "google", "cloudflare",
    "groq", "sambanova", "cline", "mistral", "opencode-zen", "zai", "openrouter",
]


def write_pin_settings(home: Path, pin_upstream: str) -> None:
    """Harden the seeded settings.json for a pinned run: disable every other
    free-catalog upstream so the chain contains exactly one entry.

    fallback_retries: RoutingConfig defaults it to 0, which makes the
    same-upstream retry path dead code — invisible on multi-upstream chains
    (the next entry just serves), but fatal on a pin, where a single burst
    429 on turn 2's tool-result request would kill the whole run. A nonzero
    budget lets a transient rate limit be waited out (500ms/1s/2s backoff)
    instead of failing the run."""
    settings_path = home / "settings.json"
    settings = json.loads(settings_path.read_text())
    settings.setdefault("providers", {})["free"] = {
        "options": {
            "routing": {
                "disabled_upstreams": [uid for uid in CATALOG_IDS if uid != pin_upstream],
                "fallback_retries": 3,
            }
        }
    }
    settings_path.write_text(json.dumps(settings, indent=2))


def run_arm(
    scenario: str,
    turns: list[str],
    arm: str,
    repeat: int,
    *,
    binary: Path,
    model: str,
    max_turns: int,
    timeout: float,
    auth_file: Path,
    pin_upstream: str | None = None,
) -> dict:
    """Run one (scenario, arm, repeat) session and return its record."""
    run_id = f"drift-{scenario}-{arm}-{repeat}-{uuid.uuid4().hex[:8]}"
    home = Path(tempfile.mkdtemp(prefix=f"clawde-drift-{run_id}-"))
    cwd = Path(tempfile.mkdtemp(prefix=f"clawde-drift-cwd-{run_id}-"))
    record: dict = {
        "run_id": run_id,
        "scenario": scenario,
        "arm": arm,
        "repeat": repeat,
        "ts": utcnow(),
    }
    try:
        seed_home(home, auth_file, sabotage=[])
        if pin_upstream:
            write_pin_settings(home, pin_upstream)
        for rel, content in SCENARIO_FILES.get(scenario, {}).items():
            path = cwd / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content)

        run = None
        upstream_ids: list[str | None] = []
        for turn_index, turn_prompt in enumerate(turns):
            cmd_args: dict = dict(
                binary=binary,
                model=model,
                cwd=cwd,
                home=home,
                max_turns=max_turns,
                timeout=timeout,
                session_id=run_id,
                permission_mode=None,
                resume=turn_index > 0,
            )
            this_run, _ = run_headless(turn_prompt, **cmd_args)
            run = this_run
            # Per-turn upstream attribution (provider_attribution stream
            # event). Needed to verify a pinned-model run: Route::Pinned falls
            # through the chain SILENTLY on 429/5xx, so post-hoc exclusion of
            # fallthrough runs is the only way to keep the pin honest.
            upstream_ids.append(run.get("upstream_id"))
            if run.get("error"):
                break
        record["run_error"] = run.get("error") if run else "no turn executed"
        record["response_chars"] = run.get("response_chars", 0) if run else 0
        record["tool_calls"] = len(run.get("tool_sequence", [])) if run else 0
        record["upstream_ids"] = upstream_ids
        record["models"] = run.get("model") if run else None
        record["fallback_used"] = run.get("fallback_used") if run else None
        record["stderr_tail"] = run.get("stderr_tail") if run else None

        # Score the finished session BEFORE the home/cwd are removed. The dump
        # is model-free (reads the session JSON + JSONL transcript).
        if record["run_error"] is None:
            dump = dump_task_state(binary, home, cwd, run_id)
            if dump is None:
                record["run_error"] = "task-state dump failed"
            else:
                record["metrics"] = dump.get("metrics")
                record["task_context"] = dump.get("task_context")
    finally:
        shutil.rmtree(home, ignore_errors=True)
        shutil.rmtree(cwd, ignore_errors=True)
    return record


def pinned_usable(record: dict, pin_upstream: str | None) -> bool:
    """True when a record may enter a pinned comparison.

    A pinned run counts only if EVERY turn was served by the pinned upstream:
    Route::Pinned falls through silently on failure, and the Groq-style
    truncation quirk (max_total_tokens) amputates the tail of the system
    prompt — where <task_context> lives — so a fallthrough or a Groq-served
    turn makes the treatment's delivery unverifiable.
    """
    if pin_upstream is None:
        return record.get("metrics") is not None
    ids = record.get("upstream_ids") or []
    return (
        record.get("metrics") is not None
        and bool(ids)
        and all(uid == pin_upstream for uid in ids)
    )


def aggregate(records: list[dict], scenario: str, arm: str) -> dict:
    """Median drift metrics over the usable runs of one arm."""
    metrics = [
        r["metrics"]
        for r in records
        if r["scenario"] == scenario and r["arm"] == arm and r.get("metrics")
    ]
    if not metrics:
        return {"n": 0}
    out: dict = {"n": len(metrics)}
    for key in ("score", "scope_expansions", "repeated_failures_per_target", "failed_tools", "files_touched"):
        values = [m[key] for m in metrics if key in m]
        out[key] = statistics.median(values) if values else None
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--scenario", action="append", default=[], choices=sorted(SCENARIOS), help="Scenario to run (repeatable; default: all)")
    ap.add_argument("--repeats", type=int, default=3, help="Paired repeats per scenario per arm (default 3)")
    ap.add_argument("--model", default="free/auto", help="Model to dispatch (default free/auto)")
    ap.add_argument("--max-turns", type=int, default=8, help="Max agentic turns per user turn (default 8)")
    ap.add_argument("--timeout", type=float, default=300.0, help="Hard timeout per turn, seconds (default 300)")
    ap.add_argument(
        "--stagger-seconds",
        type=float,
        default=0.0,
        help=(
            "Sleep between runs so a rate-limited free tier recovers "
            "(e.g. 45 for pinned runs; 0 = no pacing)."
        ),
    )
    ap.add_argument("--binary", type=Path, default=DEFAULT_BINARY, help="Path to the clawde binary")
    ap.add_argument("--auth-file", type=Path, default=Path.home() / ".clawde" / "auth.json", help="Auth store to seed isolated homes from")
    ap.add_argument("--output", type=Path, default=None, help="Report path (default scripts/eval/results/drift_ab-<ts>.json)")
    ap.add_argument("--tag", default="", help="Free-form tag recorded in the report")
    ap.add_argument(
        "--pin-upstream",
        default=None,
        help=(
            "Upstream id the --model pin must hold on every turn (e.g. cerebras). "
            "HARDENS the pin: settings.json disables every other free-catalog "
            "upstream, so the chain holds exactly one entry and a failed dispatch "
            "fails the run instead of silently falling through to another model. "
            "Use with --model free/<upstream>/<model>. Also required as a filter: "
            "runs not served by this upstream on every turn are excluded."
        ),
    )
    args = ap.parse_args()

    scenarios = args.scenario or sorted(SCENARIOS)
    records: list[dict] = []
    total = len(scenarios) * 2 * args.repeats
    done = 0
    for scenario in scenarios:
        turns = SCENARIOS[scenario]
        for repeat in range(args.repeats):
            for arm, extra_args in (("control", ["--no-task-context"]), ("treatment", [])):
                done += 1
                print(f"[{done}/{total}] {scenario} arm={arm} repeat={repeat} ...", flush=True)
                record = run_arm(
                    scenario,
                    turns,
                    arm,
                    repeat,
                    binary=args.binary,
                    model=args.model,
                    max_turns=args.max_turns,
                    timeout=args.timeout,
                    auth_file=args.auth_file,
                    pin_upstream=args.pin_upstream,
                )
                # The control arm must carry the toggle; nothing else differs.
                record["model"] = args.model
                if record.get("run_error"):
                    print(f"    ERROR: {record['run_error']}", flush=True)
                else:
                    m = record.get("metrics") or {}
                    print(
                        f"    score={m.get('score')} failed={m.get('failed_tools')} "
                        f"files={m.get('files_touched')} repeats={m.get('repeated_failures_per_target')}",
                        flush=True,
                    )
                records.append(record)
                if args.stagger_seconds > 0 and done < total:
                    time.sleep(args.stagger_seconds)

    # Paired comparison per scenario: same repeat index, both arms usable.
    # With --pin-upstream, only runs served by the pinned upstream on EVERY
    # turn enter the aggregate (silent fallthrough breaks the pin).
    comparison: dict[str, dict] = {}
    pin_excluded = 0
    for scenario in scenarios:
        per_repeat = []
        for repeat in range(args.repeats):
            control = next((r for r in records if r["scenario"] == scenario and r["arm"] == "control" and r["repeat"] == repeat), None)
            treatment = next((r for r in records if r["scenario"] == scenario and r["arm"] == "treatment" and r["repeat"] == repeat), None)
            control_ok = control is not None and pinned_usable(control, args.pin_upstream)
            treatment_ok = treatment is not None and pinned_usable(treatment, args.pin_upstream)
            if control is not None and treatment is not None and not (control_ok and treatment_ok):
                # Count silent-fallthrough exclusions explicitly so the report
                # shows how much data the pin cost instead of hiding it.
                for side, ok in (("control", control_ok), ("treatment", treatment_ok)):
                    rec = control if side == "control" else treatment
                    if rec is not None and rec.get("metrics") and not ok:
                        pin_excluded += 1
            if control_ok and treatment_ok:
                per_repeat.append({
                    "repeat": repeat,
                    "control_score": control["metrics"].get("score"),
                    "treatment_score": treatment["metrics"].get("score"),
                    "delta": (treatment["metrics"].get("score") or 0) - (control["metrics"].get("score") or 0),
                })
        usable = [r for r in records if r["scenario"] == scenario and pinned_usable(r, args.pin_upstream)]
        agg = {
            "control": aggregate(usable, scenario, "control"),
            "treatment": aggregate(usable, scenario, "treatment"),
            "paired": per_repeat,
        }
        if agg["control"].get("n") and agg["treatment"].get("n"):
            c = agg["control"].get("score")
            t = agg["treatment"].get("score")
            if c is not None and t is not None:
                agg["median_delta"] = round(t - c, 3)
                agg["direction"] = "less-drift-with-task_context" if t < c else ("more-drift-with-task_context" if t > c else "no-difference")
        comparison[scenario] = agg

    report = {
        "schema_version": "clawde-drift-ab.v1",
        "ts": utcnow(),
        "tag": args.tag,
        "model": args.model,
        "pin_upstream": args.pin_upstream,
        "pin_excluded_runs": pin_excluded,
        "repeats": args.repeats,
        "records": records,
        "comparison": comparison,
    }
    out_path = args.output or (RESULTS_DIR / f"drift_ab-{int(time.time())}.json")
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(report, indent=2))

    print(f"\n=== drift A/B ({args.model}, {args.repeats} repeats) ===")
    for scenario, agg in comparison.items():
        control, treatment = agg["control"], agg["treatment"]
        print(f"  {scenario}: control n={control.get('n', 0)} treatment n={treatment.get('n', 0)}")
        if control.get("n") and treatment.get("n"):
            print(
                f"    median score  control={control.get('score')}  treatment={treatment.get('score')}"
                f"  delta={agg.get('median_delta')}  [{agg.get('direction', 'insufficient-data')}]"
            )
            for key in ("failed_tools", "files_touched", "repeated_failures_per_target", "scope_expansions"):
                print(f"    {key:32s} control={control.get(key)}  treatment={treatment.get(key)}")
    errors = [r for r in records if r.get("run_error")]
    print(f"  report: {out_path}  ({len(errors)} unevaluable runs)")
    if args.pin_upstream:
        print(f"  pin filter: upstream must be '{args.pin_upstream}' on every turn — {pin_excluded} run(s) excluded for silent fallthrough")
    if len(errors) == total:
        print("  all runs unevaluable — no comparison possible", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
