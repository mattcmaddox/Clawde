#!/usr/bin/env python3
"""Probe the GitHub Copilot API to diagnose auth issues.

Reads GITHUB_TOKEN from env or ``gh auth token``.  Tests three endpoints:
  1. GET  /models                     – auth check (should return model list)
  2. POST /chat/completions           – 1-token smoke test
  3. GET  /copilot_internal/v2/token  – token-exchange (may 404)

Usage:
  GITHUB_TOKEN=ghp_xxx ./scripts/probes/copilot-probe.py
  ./scripts/probes/copilot-probe.py          # reads from gh auth token

Status codes and first 400 bytes of each response are printed.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import urllib.error
import urllib.request

BASE = "https://api.githubcopilot.com"


def _get_token() -> str:
    tok = os.environ.get("GITHUB_TOKEN", "")
    if tok:
        return tok
    try:
        tok = subprocess.run(
            ["gh", "auth", "token"], capture_output=True, text=True, check=True
        ).stdout.strip()
    except Exception:
        pass
    if not tok:
        sys.exit("Set GITHUB_TOKEN or run `gh auth login` first.")
    return tok


def _probe(
    url: str,
    data: bytes | None = None,
    extra: dict[str, str] | None = None,
    token: str | None = None,
) -> tuple[int, str]:
    if token is None:
        token = _get_token()
    req = urllib.request.Request(url, data=data)
    req.add_header("User-Agent", "clawde-copilot-probe/1.0")
    req.add_header("Accept", "application/json")
    req.add_header("Authorization", f"Bearer {token}")
    # Copilot-specific headers (so the server knows this is an editor session)
    req.add_header("Openai-Intent", "conversation-edits")
    req.add_header("x-initiator", "user")
    if data:
        req.add_header("Content-Type", "application/json")
    if extra:
        for k, v in extra.items():
            req.add_header(k, v)
    try:
        r = urllib.request.urlopen(req, timeout=20)
        return r.status, r.read(1500).decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read(1500).decode(errors="replace")


def main() -> None:
    token = _get_token()
    prefix = token[:10] + "..." if len(token) > 10 else token
    print(f"Token: {prefix}  (length {len(token)})")
    print()

    # 1. Models endpoint (auth gate)
    print("── GET /models ──")
    st, body = _probe(f"{BASE}/models", token=token)
    print(f"  HTTP {st}")
    print(f"  {body[:400]}")
    print()

    # 2. Chat completion smoke test (1 token, cheapest model)
    print("── POST /chat/completions (gpt-4o-mini, 1 token) ──")
    payload = json.dumps(
        {
            "model": "gpt-4o-mini",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 1,
            "stream": False,
        }
    ).encode()
    st, body = _probe(f"{BASE}/chat/completions", data=payload, token=token)
    print(f"  HTTP {st}")
    print(f"  {body[:400]}")
    print()

    # 3. Token exchange (may 404 — endpoint is gated/obsolete)
    print("── GET /copilot_internal/v2/token ──")
    st, body = _probe(
        f"{BASE}/copilot_internal/v2/token",
        extra={"Editor-Version": "vscode/1.100.0"},
        token=token,
    )
    print(f"  HTTP {st}")
    print(f"  {body[:400]}")
    print()


if __name__ == "__main__":
    main()
