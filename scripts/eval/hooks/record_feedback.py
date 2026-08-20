#!/usr/bin/env python3
"""Record Stop-hook feedback for Clawde turns.

Wire it as a Stop hook in settings.json (under the `config` object, flat
HookEntry shape) so every completed turn appends a JSONL evidence record
(upstream, model, latency, cost, fallback, response excerpt) to an
append-only file for mining failure modes:

    {
      "config": {
        "hooks": {
          "Stop": [
            {"command": "python3 /path/to/scripts/eval/hooks/record_feedback.py"}
          ]
        }
      }
    }

(The import-config migration format with `matcher`/nested `hooks` is only for
importing TS configs; the native settings.json schema nests under `config`.)

The synchronous Stop hook invocation (run_hooks) delivers the enriched
HookContext JSON on stdin — upstream_id, model, elapsed_ms, cost_usd,
fallback_used, retries — which this script persists. The fire-and-forget
background invocation (CLAUDE_HOOK_OUTPUT env, null stdin) is skipped so each
turn records exactly one line.

Output file: first CLI arg, else $CLAWDE_EVAL_HOOKS_FILE, else
scripts/eval/results/hooks.jsonl. Never fails the turn: any error exits 0.
"""

import json
import os
import sys
from datetime import datetime, timezone
from pathlib import Path

DEFAULT_OUT = Path(__file__).resolve().parents[1] / "results" / "hooks.jsonl"


def utcnow() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z"


def main() -> int:
    raw = sys.stdin.read().strip()
    if not raw:
        # Background fire-and-forget invocation (stdin null). The rich record
        # is written by the synchronous invocation; skip to avoid duplicates.
        return 0
    try:
        ctx = json.loads(raw)
    except json.JSONDecodeError:
        return 0

    if ctx.get("event") != "Stop":
        return 0

    out = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(
        os.environ.get("CLAWDE_EVAL_HOOKS_FILE", DEFAULT_OUT)
    )
    out.parent.mkdir(parents=True, exist_ok=True)

    record = {
        "ts": utcnow(),
        "session_id": ctx.get("session_id"),
        "upstream_id": ctx.get("upstream_id"),
        "model": ctx.get("model"),
        "elapsed_ms": ctx.get("elapsed_ms"),
        "cost_usd": ctx.get("cost_usd"),
        "fallback_used": ctx.get("fallback_used"),
        "retries": ctx.get("retries"),
        # Bounded excerpt only — the full response stays in the session store.
        "response_excerpt": (ctx.get("tool_output") or "")[:1000],
        "response_chars": len(ctx.get("tool_output") or ""),
    }
    with out.open("a") as f:
        f.write(json.dumps(record) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
