#!/usr/bin/env python3
"""Calibrate a fixture's `judge.min_score` by grading stored good/degraded responses.

The judge is advisory and flaky (free providers emit empty completions and
unparseable scores), so this grades each response several times and reports the
median. A sensible `min_score` sits between the degraded and good bands — reject
what is clearly degraded without flunking every borderline good answer.

Usage:
    python3 scripts/eval/calibrate_judge.py --fixture scripts/eval/fixtures/catalog-order \
        [--reports 'scripts/eval/results/*/report.json'] [--attempts 3]

Good vs degraded is taken from each report's `passed` flag; empty responses
(0 chars — provider flake, not an answer) are skipped. Grades use the fixture's
`judge.rubric` (falling back to the generic default) and the pinned judge model.
"""

import argparse
import glob
import json
import os
import shutil
import sys
import tempfile
import uuid
from pathlib import Path
from statistics import median

sys.path.insert(0, str(Path(__file__).resolve().parent))
from run_eval import (  # noqa: E402
    DEFAULT_AUTH,
    DEFAULT_BINARY,
    DEFAULT_JUDGE_MODEL,
    DEFAULT_JUDGE_RUBRIC,
    SRC_RUST,
    seed_home,
    run_judge,
)


def load_responses(reports_glob: str, fixture_path: str) -> tuple[list[str], list[str]]:
    good, bad = [], []
    fixture_name = os.path.basename(fixture_path)
    for p in sorted(glob.glob(reports_glob)):
        try:
            r = json.loads(Path(p).read_text())
        except (json.JSONDecodeError, OSError):
            continue
        if os.path.basename(r.get("fixture") or "") != fixture_name:
            continue
        text = (r.get("run") or {}).get("response_text") or ""
        if not text.strip():
            continue  # empty completion — provider flake, not an answer
        (good if r.get("passed") else bad).append(text)
    return good, bad


def grade(judge, text: str, rubric: str, binary, home, attempts: int) -> list[float | None]:
    scores = []
    for i in range(attempts):
        sid = f"calibrate-{uuid.uuid4().hex[:8]}"
        res = judge(
            "",
            text,
            rubric,
            binary=binary,
            model=DEFAULT_JUDGE_MODEL,
            cwd=SRC_RUST,
            home=home,
            max_turns=1,
            session_id=sid,
            timeout=300.0,
        )
        scores.append(res.get("score"))
    return scores


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--fixture", required=True, type=Path, help="fixture dir with expected.json")
    ap.add_argument("--reports", default="scripts/eval/results/*/report.json", help="glob of report.json files")
    ap.add_argument("--attempts", type=int, default=3, help="judge attempts per response (default 3)")
    ap.add_argument("--max-per-band", type=int, default=2, help="cap responses graded per good/bad band (default 2)")
    args = ap.parse_args()

    fixture = args.fixture.resolve()
    expected = json.loads((fixture / "expected.json").read_text())
    rubric = (expected.get("judge") or {}).get("rubric") or DEFAULT_JUDGE_RUBRIC

    good, bad = load_responses(args.reports, str(fixture))
    if not good or not bad:
        print(f"need at least one good and one degraded response (found {len(good)} good, {len(bad)} bad)", file=sys.stderr)
        return 1
    good = good[: args.max_per_band]
    bad = bad[: args.max_per_band]

    home = Path(tempfile.mkdtemp(prefix="clawde-calibrate-"))
    try:
        seed_home(home, DEFAULT_AUTH, None)
        bands = {}
        for label, texts in (("GOOD", good), ("DEGRADED", bad)):
            all_scores = []
            for text in texts:
                scores = grade(run_judge, text, rubric, DEFAULT_BINARY, home, args.attempts)
                parsed = [s for s in scores if s is not None]
                med = round(median(parsed), 3) if parsed else None
                all_scores.append(med)
                print(f"{label:8} [{len(text):4} chars] scores={scores} median={med}")
            parsed = [s for s in all_scores if s is not None]
            bands[label] = (round(median(parsed), 3) if parsed else None, min(parsed) if parsed else None, max(parsed) if parsed else None)
    finally:
        shutil.rmtree(home, ignore_errors=True)

    g_med, g_min, g_max = bands["GOOD"]
    b_med, b_min, b_max = bands["DEGRADED"]
    print(f"\nGOOD     median={g_med} range=[{g_min}, {g_max}]")
    print(f"DEGRADED median={b_med} range=[{b_min}, {b_max}]")
    if g_med is not None and b_med is not None and g_med > b_med:
        # Reject the degraded band, keep the good band: put the floor between
        # the degraded max and the good min, biased toward rejecting degradation.
        suggested = round((b_max + g_min) / 2, 3)
        print(f"suggested judge.min_score = {suggested}  (midpoint of degraded max {b_max} and good min {g_min})")
    else:
        print("no clean separation — the judge may be too flaky to gate on; rerun with more attempts")
    return 0


if __name__ == "__main__":
    sys.exit(main())
