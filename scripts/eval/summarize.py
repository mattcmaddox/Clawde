#!/usr/bin/env python3
"""Summarize eval results stored by run_eval.py (results.jsonl).

Aggregates pass rate, TTFT, total time, cost, and scores — overall and
broken down by tag / fixture / upstream — so a release that silently
degrades answer quality trips a visible signal. Also prints a per-run
trend tail (oldest -> newest) for regression spotting.

Usage:
    python3 scripts/eval/summarize.py [--json] [--tag TAG] [--fixture NAME] [--tail N]

Exit code 0 on success (even with failures — this is a report, not a gate).
"""

import argparse
import json
import os
import sys
from collections import defaultdict
from statistics import mean, median
from pathlib import Path

EVAL_DIR = Path(__file__).resolve().parent
REPO_ROOT = EVAL_DIR.parents[1]
DEFAULT_RESULTS = str(EVAL_DIR / "results" / "results.jsonl")


def normalize_fixture(value):
    """Use one stable fixture key for old absolute and new relative records."""
    if not value:
        return value
    try:
        path = Path(value)
        if path.is_absolute():
            return str(path.resolve().relative_to(REPO_ROOT))
    except ValueError:
        pass
    return str(value)


def load_records(path):
    records = []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                record = json.loads(line)
                if isinstance(record, dict):
                    record["fixture"] = normalize_fixture(record.get("fixture"))
                    records.append(record)
                else:
                    print(f"warning: skipping non-object line in {path}", file=sys.stderr)
            except json.JSONDecodeError as e:
                print(f"warning: skipping malformed line in {path}: {e}", file=sys.stderr)
    return records


def agg(records):
    if not records:
        return None
    ttfts = [r.get("ttft_ms") for r in records if isinstance(r.get("ttft_ms"), (int, float))]
    totals = [r.get("total_ms") for r in records if isinstance(r.get("total_ms"), (int, float))]
    costs = [r.get("cost_usd") for r in records if isinstance(r.get("cost_usd"), (int, float))]
    scores = [r.get("score") for r in records if isinstance(r.get("score"), (int, float))]
    judges = [r.get("judge_score") for r in records if isinstance(r.get("judge_score"), (int, float))]
    return {
        "runs": len(records),
        "passed": sum(1 for r in records if r.get("passed")),
        "errors": sum(1 for r in records if r.get("error")),
        "ttft_ms_median": round(median(ttfts)) if ttfts else None,
        "ttft_ms_mean": round(mean(ttfts)) if ttfts else None,
        "ttft_ms_max": round(max(ttfts)) if ttfts else None,
        "total_ms_mean": round(mean(totals)) if totals else None,
        "cost_usd_total": round(sum(costs), 4) if costs else None,
        "score_mean": round(mean(scores), 3) if scores else None,
        "score_min": round(min(scores), 3) if scores else None,
        "judge_mean": round(mean(judges), 3) if judges else None,
        "upstreams": sorted({r.get("upstream") for r in records if r.get("upstream")}),
    }


def fmt(a):
    if a is None:
        return "-"
    if isinstance(a, float):
        return f"{a:.3f}"
    return str(a)


def print_table(title, groups):
    print(f"\n== {title} ==")
    header = ("group", "runs", "pass", "TTFT med", "TTFT max", "cost $", "score mean", "judge mean")
    rows = []
    for key, recs in sorted(groups.items()):
        a = agg(recs)
        rows.append((key, a["runs"], f"{a['passed']}/{a['runs']}", fmt(a["ttft_ms_median"]),
                     fmt(a["ttft_ms_max"]), fmt(a["cost_usd_total"]), fmt(a["score_mean"]),
                     fmt(a["judge_mean"])))
    widths = [max(len(str(r[i])) for r in rows + [header]) for i in range(len(header))]
    print("  ".join(h.ljust(w) for h, w in zip(header, widths)))
    for r in rows:
        print("  ".join(str(c).ljust(w) for c, w in zip(r, widths)))


def regression_report(records, recent=3, margin=0.15):
    """Compare the last `recent` runs of each fixture against the older
    baseline. Returns (rows, regressed) where rows carry judge/score deltas and
    regressed is True when any fixture degraded below the margin."""
    by_fixture = defaultdict(list)
    for r in records:
        by_fixture[r.get("fixture") or "(unknown)"].append(r)
    rows = []
    regressed = False
    for fixture, recs in sorted(by_fixture.items()):
        if len(recs) < recent + 1:
            continue  # not enough history for a baseline + recent window
        baseline, recent_recs = recs[:-recent], recs[-recent:]
        row = {"fixture": fixture}
        b_judge = [r["judge_score"] for r in baseline if isinstance(r.get("judge_score"), (int, float))]
        r_judge = [r["judge_score"] for r in recent_recs if isinstance(r.get("judge_score"), (int, float))]
        if b_judge and r_judge:
            delta = mean(r_judge) - mean(b_judge)
            row["judge_baseline"] = round(mean(b_judge), 3)
            row["judge_recent"] = round(mean(r_judge), 3)
            row["judge_delta"] = round(delta, 3)
            row["judge_regressed"] = delta <= -margin
            regressed |= row["judge_regressed"]
        b_score = [r["score"] for r in baseline if isinstance(r.get("score"), (int, float))]
        r_score = [r["score"] for r in recent_recs if isinstance(r.get("score"), (int, float))]
        if b_score and r_score:
            delta = mean(r_score) - mean(b_score)
            row["score_baseline"] = round(mean(b_score), 3)
            row["score_recent"] = round(mean(r_score), 3)
            row["score_delta"] = round(delta, 3)
            row["score_regressed"] = delta <= -margin
            regressed |= row["score_regressed"]
        if "judge_regressed" in row or "score_regressed" in row:
            rows.append(row)
    return rows, regressed


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--json", action="store_true", help="emit machine-readable JSON instead of tables")
    ap.add_argument("--tag", help="filter to one tag")
    ap.add_argument("--fixture", help="filter to one fixture (substring match on path)")
    ap.add_argument("--tail", type=int, default=10, help="trend-tail length (default 10)")
    ap.add_argument("--results", default=DEFAULT_RESULTS, help="path to results.jsonl")
    ap.add_argument("--regression", action="store_true", help="compare recent runs vs older baseline per fixture")
    ap.add_argument("--recent", type=int, default=3, help="recent-window size for --regression (default 3)")
    ap.add_argument("--margin", type=float, default=0.15, help="regression threshold delta (default 0.15)")
    args = ap.parse_args()

    if not os.path.exists(args.results):
        print(f"no results at {args.results} — run scripts/eval/run_eval.py first", file=sys.stderr)
        return 1

    records = load_records(args.results)
    if args.tag:
        records = [r for r in records if r.get("tag") == args.tag]
    if args.fixture:
        fixture_filter = normalize_fixture(args.fixture)
        records = [r for r in records if fixture_filter in r.get("fixture", "")]
    if not records:
        print("no records match the filters", file=sys.stderr)
        return 0

    by_tag = defaultdict(list)
    by_fixture = defaultdict(list)
    by_upstream = defaultdict(list)
    for r in records:
        by_tag[r.get("tag") or "(untagged)"].append(r)
        by_fixture[r.get("fixture") or "(unknown)"].append(r)
        by_upstream[r.get("upstream") or "(none)"].append(r)

    overall = agg(records)

    if args.json:
        out = {
            "overall": overall,
            "by_tag": {k: agg(v) for k, v in by_tag.items()},
            "by_fixture": {k: agg(v) for k, v in by_fixture.items()},
            "by_upstream": {k: agg(v) for k, v in by_upstream.items()},
        }
        if args.regression:
            rows, regressed = regression_report(records, args.recent, args.margin)
            out["regression"] = {"rows": rows, "regressed": regressed}
        print(json.dumps(out, indent=2))
        return 0

    if args.regression:
        rows, regressed = regression_report(records, args.recent, args.margin)
        if not rows:
            print(f"no fixture has >= {args.recent + 1} runs to compare a baseline against")
        else:
            print(f"\n== regression (last {args.recent} runs vs older baseline, margin {args.margin}) ==")
            print("fixture                    judge delta                        score delta")
            for r in rows:
                name = (r["fixture"] or "").rsplit("/", 1)[-1][:26]
                j = f"{r['judge_recent']} vs {r['judge_baseline']} ({r['judge_delta']:+.3f}) REGRESSED" \
                    if r.get("judge_regressed") else \
                    (f"{r['judge_recent']} vs {r['judge_baseline']} ({r['judge_delta']:+.3f})" if "judge_delta" in r else "-")
                s = f"{r['score_recent']} vs {r['score_baseline']} ({r['score_delta']:+.3f}) REGRESSED" \
                    if r.get("score_regressed") else \
                    (f"{r['score_recent']} vs {r['score_baseline']} ({r['score_delta']:+.3f})" if "score_delta" in r else "-")
                print(f"{name:26}  {j:36}  {s}")
        if regressed:
            print("\nREGRESSION DETECTED")
        return 1 if regressed else 0

    a = overall
    print(f"eval summary: {a['runs']} runs, {a['passed']}/{a['runs']} passed, "
          f"{a['errors']} errored")
    print(f"TTFT median {fmt(a['ttft_ms_median'])} ms (max {fmt(a['ttft_ms_max'])} ms), "
          f"total cost ${fmt(a['cost_usd_total'])}, "
          f"mean score {fmt(a['score_mean'])}, judge mean {fmt(a['judge_mean'])}")
    if a["upstreams"]:
        print(f"upstreams seen: {', '.join(a['upstreams'])}")

    print_table("by tag", by_tag)
    print_table("by fixture", by_fixture)
    print_table("by upstream", by_upstream)

    trend = records[-args.tail:]
    print(f"\n== trend (last {len(trend)} runs, oldest -> newest) ==")
    print("ts                          tag           fixture             upstream   ttft_ms score passed judge")
    for r in trend:
        fixture = (r.get("fixture") or "").rsplit("/", 1)[-1][:18]
        print(f"{r.get('ts', '')[:26]:30} {(r.get('tag') or '')[:12]:13} {fixture:20} "
              f"{(r.get('upstream') or '')[:10]:11} {str(r.get('ttft_ms')):7} "
              f"{str(r.get('score')):6} {'PASS' if r.get('passed') else 'FAIL':5} "
              f"{str(r.get('judge_score')) if r.get('judge_score') is not None else ''}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
