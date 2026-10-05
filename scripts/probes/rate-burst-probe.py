#!/usr/bin/env python3
"""Burst probe: is a provider's limit a per-MINUTE request rate, and is it per
model or shared?

Why this exists. The sequential `limit-scope-probe.py` CANNOT test an RPM
limit: it waits for each full round-trip before sending the next, so its own
request rate stays under the limit it is trying to find. Measured against
NVIDIA's `gpt-oss-20b`, 200 sequential requests produced no 429 - which says
nothing about a 40 RPM cap, because 200 requests spread over 200 seconds is
6 requests per minute.

So this probe sends a BURST: N requests in parallel, timed. Then it immediately
repeats against a second model on the same key.

  model A 429s, model B succeeds  -> per-model rate limit, rotation is useless
  model A 429s, model B also 429s -> the rate is shared across models (per key)
  neither 429s                   -> no per-minute limit at this concurrency

The observed 429 count is reported either way, so a real limit is measured
rather than just detected.

Keys come from the local auth store; never printed.
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import sys
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor

TIMEOUT = 60

BASE_URLS = {
    "nvidia": "https://integrate.api.nvidia.com/v1",
    "cerebras": "https://api.cerebras.ai/v1",
    "groq": "https://api.groq.com/openai/v1",
    "sambanova": "https://api.sambanova.ai/v1",
}


def load_keys() -> dict[str, list[str]]:
    path = pathlib.Path(os.path.expanduser("~/.clawde/auth.json"))
    if not path.exists():
        return {}
    return {k: list(v) for k, v in json.load(open(path)).get("keys", {}).items()}


def one(url: str, key: str, model: str) -> int:
    body = json.dumps(
        {"model": model, "messages": [{"role": "user", "content": "hi"}], "max_tokens": 1}
    ).encode()
    req = urllib.request.Request(
        f"{url}/chat/completions",
        data=body,
        headers={
            "Authorization": f"Bearer {key}",
            "Content-Type": "application/json",
            "User-Agent": "clawde-burst-probe/1.0",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=TIMEOUT) as r:
            return r.status
    except urllib.error.HTTPError as e:
        return e.code
    except Exception:  # noqa: BLE001
        return 0


def burst(url: str, key: str, model: str, n: int, workers: int) -> dict:
    """Fire `n` requests with `workers` in flight; report counts and duration."""
    start = time.time()
    with ThreadPoolExecutor(max_workers=workers) as pool:
        codes = list(pool.map(lambda _: one(url, key, model), range(n)))
    elapsed = time.time() - start
    ok = sum(1 for c in codes if 200 <= c < 300)
    limited = sum(1 for c in codes if c == 429)
    other = len(codes) - ok - limited
    return {
        "model": model,
        "sent": n,
        "ok": ok,
        "rate_limited": limited,
        "other": other,
        "elapsed_secs": round(elapsed, 2),
        "achieved_rpm": round(n / max(elapsed / 60.0, 1e-9), 1),
    }


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--provider", default="nvidia")
    ap.add_argument("--model-a", required=True)
    ap.add_argument("--model-b", required=True)
    ap.add_argument("-n", type=int, default=60, help="requests per burst")
    ap.add_argument("--workers", type=int, default=20, help="concurrency")
    ap.add_argument("--settle", type=float, default=2.0)
    args = ap.parse_args()

    keys = load_keys().get(args.provider) or []
    if not keys:
        print(f"no key configured for {args.provider}", file=sys.stderr)
        return 2
    url = BASE_URLS.get(args.provider)
    if url is None:
        print(f"no base url for {args.provider}", file=sys.stderr)
        return 2

    a = burst(url, keys[0], args.model_a, args.n, args.workers)
    time.sleep(args.settle)
    b = burst(url, keys[0], args.model_b, args.n, args.workers)

    scope = "UNKNOWN"
    if a["rate_limited"] and b["rate_limited"] == 0:
        scope = "PER-MODEL (rotation across models is useful)"
    elif a["rate_limited"] and b["rate_limited"]:
        scope = "SHARED across models (per key/org)"
    elif not a["rate_limited"]:
        scope = f"NO per-minute limit seen at {a['achieved_rpm']} rpm"

    print(json.dumps({"provider": args.provider, "model_a": a, "model_b": b}, indent=2))
    print(f"\nA: {a['ok']} ok / {a['rate_limited']} limited  @ {a['achieved_rpm']} rpm", file=sys.stderr)
    print(f"B: {b['ok']} ok / {b['rate_limited']} limited  @ {b['achieved_rpm']} rpm", file=sys.stderr)
    print(f"VERDICT: {scope}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
