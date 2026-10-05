#!/usr/bin/env python3
"""Verify every FREE_CATALOG default model actually dispatches.

Sends one minimal request per upstream using the real key, base url and
headers Clawde uses, and reports the HTTP status. A catalog entry that cannot
answer is worse than no entry: it sits at a known chain position and costs a
request every turn until something notices.

  2xx  dispatches       404/410  retired       403  blocked / plan gate
  429  per-model cap    402  no credits      5xx  upstream broken

A 429 is a PASS: the model exists and the free quota is merely spent. The body
is checked for "not found" so a misnamed model is not mistaken for a cap.

Keys come from the local auth store and are never printed or written out.
"""

from __future__ import annotations

import json
import os
import pathlib
import sys
import urllib.error
import urllib.request

CLOUDFLARE_PROBE_MODEL = "@cf/qwen/qwen3-30b-a3b-fp8"

CASES = {
    "poolside": ("https://inference.poolside.ai/v1", "poolside/laguna-s-2.1", {}, "first"),
    "nvidia": (
        "https://integrate.api.nvidia.com/v1",
        "nvidia/nemotron-3.5-lightning-30b-a3b",
        {},
        "first",
    ),
    "cerebras": ("https://api.cerebras.ai/v1", "gpt-oss-120b", {}, "first"),
    "google": ("https://generativelanguage.googleapis.com/v1beta", "gemini-2.5-flash", {}, "first"),
    "groq": ("https://api.groq.com/openai/v1", "openai/gpt-oss-120b", {}, "first"),
    "sambanova": ("https://api.sambanova.ai/v1", "Meta-Llama-3.3-70B-Instruct", {}, "first"),
    "cline": (
        "https://api.cline.bot/api/v1",
        "cline-free/deepseek-v4.1-flash",
        {"X-CLIENT-TYPE": "cline-sdk"},
        "first",
    ),
    "opencode-zen": ("https://api.opencode.ai/v1", "deepseek-v4-flash-free", {}, "first"),
    "zai": ("https://open.bigmodel.cn/api/paas/v4", "glm-4.7-flash", {}, "first"),
    "openrouter": ("https://openrouter.ai/api/v1", "openrouter/free", {}, "first"),
    "cloudflare": ("", CLOUDFLARE_PROBE_MODEL, {}, "composite"),
}


TIMEOUT = 90  # nvidia nemotron-lightning can take >40s to first token


def load_keys() -> dict[str, list[str]]:
    path = pathlib.Path(os.path.expanduser("~/.clawde/auth.json"))
    if not path.exists():
        return {}
    return {k: list(v) for k, v in json.load(open(path)).get("keys", {}).items()}


def _send(req: urllib.request.Request, model: str) -> dict:
    try:
        with urllib.request.urlopen(req, timeout=TIMEOUT) as r:
            return {"model": model, "status": r.status, "verdict": "OK"}
    except urllib.error.HTTPError as e:
        raw = e.read().decode("utf-8", "replace")
        return {"model": model, "status": e.code, "verdict": _classify(e.code, raw)}
    except Exception as e:  # noqa: BLE001
        return {
            "model": model,
            "status": None,
            "verdict": f"ERR {type(e).__name__}",
            "detail": str(e)[:90],
        }


def _classify(status: int, raw: str) -> str:
    low = raw.lower()
    if status == 429:
        if "not found" in low or "does not exist" in low or "unknown model" in low:
            return "MODEL-NOT-FOUND (429 body)"
        return "OK (rate-limited = exists)"
    if status == 402:
        return "NO-CREDITS (paid id?)"
    if status in (401, 403):
        if "product surfaces" in low or "entitlement" in low or "not subscribed" in low:
            return "BLOCKED (plan/surface)"
        return "BLOCKED (auth)"
    if status in (404, 410):
        return "GONE"
    if status >= 500:
        return "UPSTREAM-BROKEN"
    if "empty response" in low:
        return "EMPTY-CONTENT"
    return f"HTTP {status}"


def probe(provider: str, url: str, model: str, extra: dict, mode: str, key: str) -> dict:
    if mode == "composite":
        # Cloudflare stores ACCOUNT_ID:API_TOKEN. The account goes in the URL and
        # only the token is the bearer — sending the whole pair gives a 401.
        acct, key = key.split(":", 1)
        url = f"https://api.cloudflare.com/client/v4/accounts/{acct}/ai/v1"
        body = json.dumps(
            {"model": model, "messages": [{"role": "user", "content": "hi"}], "max_tokens": 2}
        ).encode()
    elif provider == "google":
        body = json.dumps(
            {"contents": [{"parts": [{"text": "hi"}]}], "generationConfig": {"maxOutputTokens": 8}}
        ).encode()
        req = urllib.request.Request(
            f"{url}/models/{model}:generateContent?key={key}",
            data=body,
            headers={"Content-Type": "application/json", "User-Agent": "clawde-verify/1.0"},
        )
        return _send(req, model)
    else:
        body = json.dumps(
            {"model": model, "messages": [{"role": "user", "content": "hi"}], "max_tokens": 2}
        ).encode()
    headers = {
        "Authorization": f"Bearer {key}",
        "Content-Type": "application/json",
        "User-Agent": "clawde-verify/1.0",
    }
    headers.update(extra)
    return _send(urllib.request.Request(f"{url}/chat/completions", data=body, headers=headers), model)


def main() -> int:
    store = load_keys()
    targets = sys.argv[1:] or list(CASES)
    results = []
    for provider in targets:
        case = CASES.get(provider)
        if case is None:
            print(f"skip {provider}: no case configured", file=sys.stderr)
            continue
        keys = store.get(provider) or []
        if not keys:
            results.append({"provider": provider, "status": None, "verdict": "NO KEY CONFIGURED"})
            continue
        url, model, extra, mode = case
        r = probe(provider, url, model, extra, mode, keys[0])
        r["provider"] = provider
        results.append(r)
        print(f"{provider:15} {str(r['status']):>4}  {r['verdict']}", file=sys.stderr)

    print(json.dumps(results, indent=2))
    bad = [r for r in results if not r["verdict"].startswith("OK")]
    print(f"\n{len(results) - len(bad)}/{len(results)} healthy", file=sys.stderr)
    return 1 if bad else 0


if __name__ == "__main__":
    raise SystemExit(main())

