#!/usr/bin/env python3
"""Measure whether rotating to a second API key escapes a rate limit.

The question this answers: when a provider rate-limits key A, does key B have
its own quota, or is the quota shared across the key set? Vendor docs *claim*
the latter for several providers, but a doc claim is not a measurement, and the
routing consequence differs sharply:

  shared   -> rotating spends one real request per stored key to rediscover
              that the next key is throttled too
  distinct -> rotation is the whole point of storing multiple keys

Method, per provider, using only keys already in the local auth store:

  1. Read the advertised limit headers from one live request.
  2. Drive key A to 429 on model A.
  3. Send the SAME model A on key B.
       succeeds -> distinct quotas, rotation genuinely works
       429      -> shared quota
  4. Send a DIFFERENT model on the exhausted key A.
       succeeds -> the limit is per-model, not cumulative
       429      -> the limit is cumulative across models

Consumes free-tier quota. No key value is ever printed or written to disk.
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

# (model used to exhaust, different model used to test per-model scoping)
MODELS = {
    "groq": ("openai/gpt-oss-120b", "openai/gpt-oss-20b"),
    # Both verified live 2026-09-27. NOTE the catalog default gpt-oss-120b is
    # EOL (HTTP 410 since 2026-09-03), so it cannot be used to probe.
    "nvidia": ("openai/gpt-oss-20b", "nvidia/nemotron-3.5-lightning-30b-a3b"),  # no ratelimit headers at all
    "sambanova": ("Meta-Llama-3.3-70B-Instruct", "DeepSeek-V3.1"),
    "cline": ("moonshotai/kimi-k3", "spacexai/grok-4.7"),
    # CLOUDFLARE_PROBE_MODEL from catalog.rs — the model clawde actually probes.
    "cloudflare": (
        "@cf/qwen/qwen3-30b-a3b-fp8",
        "@cf/openai/gpt-oss-120b",
    ),
    "cerebras": ("llama3.1-8b", "llama-3.3-70b"),
    "openrouter": ("meta-llama/llama-3.3-70b-instruct:free", "qwen/qwen-3-8b:free"),
}

BASE_URLS = {
    "cline": "https://api.cline.bot/api/v1",
    "groq": "https://api.groq.com/openai/v1",
    "nvidia": "https://integrate.api.nvidia.com/v1",
    "sambanova": "https://api.sambanova.ai/v1",
    "cerebras": "https://api.cerebras.ai/v1",
    "openrouter": "https://openrouter.ai/api/v1",
}

TIMEOUT = 30

# Stop draining a key after this many requests without a 429. Hitting the cap
# means we failed to exhaust the key, so the run is inconclusive, not a pass.
DRAIN_CAP = 80




def load_keys() -> dict[str, list[str]]:
    """Read the auth store. Values stay in memory and are never printed."""
    path = pathlib.Path(os.path.expanduser("~/.clawde/auth.json"))
    if not path.exists():
        return {}
    return {k: list(v) for k, v in json.load(open(path)).get("keys", {}).items()}


def post(url: str, key: str, model: str) -> tuple[int, dict[str, str], str]:
    """One minimal chat completion. Returns (status, headers, body-snippet)."""
    body = json.dumps(
        {
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 1,
        }
    ).encode()
    req = urllib.request.Request(
        f"{url}/chat/completions",
        data=body,
        headers={
            "Authorization": f"Bearer {key}",
            "Content-Type": "application/json",
            # Several free-tier hosts (Groq's edge, Cloudflare-fronted ones)
            # return 403 "error code: 1010" to clients with no User-Agent.
            # Identify as a normal HTTP client rather than a blank client.
            "User-Agent": "clawde-limit-scope-probe/1.0",
            # Cline gates its free models on this header. Without it every
            # cline-free/* id returns 403 "only available via Cline product
            # surfaces" — which looks like "free models are unavailable over
            # the API" and is wrong. clawde already sends it in production
            # (CLINE_SDK_CLIENT_TYPE); the probe must too, or it measures an
            # auth wall instead of a rate limit.
            "X-CLIENT-TYPE": "cline-sdk",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=TIMEOUT) as resp:
            hdrs = {k.lower(): v for k, v in resp.headers.items()}
            return resp.status, hdrs, ""
    except urllib.error.HTTPError as e:
        hdrs = {k.lower(): v for k, v in e.headers.items()}
        return e.code, hdrs, e.read().decode("utf-8", "replace")[:300]
    except Exception as e:  # noqa: BLE001 - surface any transport failure
        return 0, {}, f"{type(e).__name__}: {e}"


def rate_limit_headers(hdrs: dict[str, str]) -> dict[str, str]:
    return {k: v for k, v in hdrs.items() if "ratelimit" in k or k == "retry-after"}


def drain_key(url: str, key: str, model: str, cap: int = 80) -> tuple[int, int]:
    """Send requests until the key returns 429.

    Returns (requests_sent, consecutive_failures_at_stop). A single transport
    error must not abort the run: these free-tier models are slow enough that
    an occasional timeout is normal, and giving up on the first one would report
    INCONCLUSIVE for a key that was never really tested. Consecutive failures
    are counted instead, and only a sustained run of them ends the drain early.
    """
    consecutive = 0
    for i in range(cap):
        status, _, _ = post(url, key, model)
        if status == 429:
            return i + 1, 0
        if status == 0:
            consecutive += 1
            if consecutive >= 10:
                return 0, consecutive
        else:
            consecutive = 0
    return cap, 0


def probe(provider: str, keys: list[str], drain_cap: int = DRAIN_CAP) -> dict:
    model_a, model_b = MODELS[provider]
    # Cloudflare stores a composite "ACCOUNT_ID:API_TOKEN" and serves the model
    # from an account-scoped path, so neither the bearer nor the URL is a plain
    # constant. Split it here rather than teaching every caller about it.
    key_a, key_b = keys[0], keys[1]
    if provider == "cloudflare":
        acct_a, tok_a = key_a.split(":", 1)
        acct_b, tok_b = key_b.split(":", 1)
        key_a, key_b = tok_a, tok_b
        url = f"https://api.cloudflare.com/client/v4/accounts/{acct_a}/ai/v1"
    else:
        url = BASE_URLS[provider]

    out: dict = {
        "provider": provider,
        "advertised_headers": {},
        "requests_to_429": None,
        "same_model_other_key": None,
        "other_model_same_key": None,
    }

    # 1. Baseline: one request, to read the advertised limit headers.
    status, hdrs, err = post(url, key_a, model_a)
    out["advertised_headers"] = rate_limit_headers(hdrs)
    if status == 0:
        out["error"] = f"baseline transport failure: {err}"
        return out
    if status not in (200, 429):
        out["error"] = f"baseline HTTP {status}: {err}"
        return out
    if status == 429:
        out["error"] = "key A already rate-limited before the test began"
        return out

    # 2. Exhaust key A on model A.
    n, failures = drain_key(url, key_a, model_a, drain_cap)
    if n == 0:
        out["error"] = (
            f"transport failure while draining key A "
            f"({failures} consecutive failures; endpoint may be down)"
        )
        return out
    if n >= drain_cap:
        # We never actually saw a 429, so nothing downstream is meaningful:
        # a "success" on key B would be true regardless of quota sharing.
        # Reporting a verdict here would be a false positive.
        out["error"] = (
            f"key A never rate-limited within {drain_cap} requests; "
            "raise --drain-cap or test a model with a tighter limit"
        )
        out["requests_to_429"] = None
        return out
    out["requests_to_429"] = n
    time.sleep(1.0)

    # 3. Same model on the OTHER key -> distinct vs shared quota.
    status, _, err = post(url, key_b, model_a)
    out["same_model_other_key"] = "success" if status == 200 else f"HTTP {status}"
    if status == 0:
        out["same_model_other_key"] = f"transport: {err}"

    # 4. Different model on the exhausted key -> per-model vs cumulative.
    # Anything other than 200/429 (404, 410, 400) means we picked a model the
    # account cannot use at all, which says nothing about scoping.
    status, _, err = post(url, key_a, model_b)
    if status == 200:
        out["other_model_same_key"] = "success"
    elif status == 429:
        out["other_model_same_key"] = "HTTP 429"
    else:
        out["other_model_same_key"] = (
            f"INCONCLUSIVE (model_b unusable: HTTP {status})"
        )
        out["per_model_inconclusive"] = True
    if status == 0:
        out["other_model_same_key"] = f"transport: {err}"

    return out




def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--provider",
        action="append",
        help="limit to a specific provider (default: every measurable one)",
    )
    ap.add_argument(
        "--drain-cap",
        type=int,
        default=DRAIN_CAP,
        help="give up after this many requests without a 429 (default: %(default)s)",
    )
    args = ap.parse_args()

    store = load_keys()
    targets = args.provider or list(MODELS)
    results = []

    for provider in targets:
        keys = store.get(provider) or []
        if len(keys) < 2:
            print(
                f"skip {provider}: needs 2 keys to compare, has {len(keys)}",
                file=sys.stderr,
            )
            continue
        if provider not in MODELS:
            print(f"skip {provider}: no model pair configured", file=sys.stderr)
            continue
        print(f"probing {provider} ...", file=sys.stderr)
        results.append(probe(provider, keys, args.drain_cap))

    print(json.dumps(results, indent=2))

    print("\n=== verdicts ===", file=sys.stderr)
    for r in results:
        if r.get("error"):
            print(f"{r['provider']}: INCONCLUSIVE ({r['error']})", file=sys.stderr)
            continue
        other_key = r["same_model_other_key"]
        other_model = r["other_model_same_key"]
        quota = (
            "SHARED (rotation futile)"
            if other_key != "success"
            else "DISTINCT (rotation works)"
        )
        if other_model == "success":
            limit = "per-model"
        elif other_model == "HTTP 429":
            limit = "cumulative"
        else:
            limit = other_model
        print(
            f"{r['provider']}: quota={quota}; limit={limit}; "
            f"took {r['requests_to_429']} reqs to 429; "
            f"keyB/same-model={other_key}; keyA/other-model={other_model}",
            file=sys.stderr,
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
