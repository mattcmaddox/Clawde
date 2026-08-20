#!/usr/bin/env python3
"""Pluggable text embedder for the `similar` eval assertion.

Primary path: Hugging Face free inference API
(`sentence-transformers/all-MiniLM-L6-v2`, 384 dims) using a token from the
seeded auth store. This works wherever HF's inference host is reachable.

Degraded path (no network to embedding hosts, e.g. restricted CI sandboxes):
deterministic lexical cosine over word multiset + character bigrams. This is
not semantic — it is a documented fallback so the assertion still scores
answers offline. The report records which embedder was used.

Both paths are pure stdlib.
"""

import json
import math
import os
import re
import urllib.request
from collections import Counter

MODEL = "sentence-transformers/all-MiniLM-L6-v2"
_HF_URL = (
    "https://api-inference.huggingface.co/pipeline/feature-extraction/"
    "sentence-transformers/all-MiniLM-L6-v2"
)


def _words(text: str) -> list[str]:
    return re.findall(r"[a-z0-9_]+", text.lower())


def _bigrams(text: str) -> list[str]:
    norm = re.sub(r"\s+", "", text.lower())
    return [norm[i : i + 2] for i in range(len(norm) - 1)]


def _cosine(a: list[float], b: list[float]) -> float:
    dot = sum(x * y for x, y in zip(a, b))
    na = math.sqrt(sum(x * x for x in a))
    nb = math.sqrt(sum(y * y for y in b))
    if na == 0 or nb == 0:
        return 0.0
    return dot / (na * nb)


def _lexical_similarity(a: str, b: str) -> float:
    """Word-overlap + char-bigram blended cosine (0..1), pure stdlib."""
    wa, wb = Counter(_words(a)), Counter(_words(b))
    ba, bb = Counter(_bigrams(a)), Counter(_bigrams(b))

    def bow_cos(x: Counter, y: Counter) -> float:
        keys = set(x) | set(y)
        dot = sum(x[k] * y[k] for k in keys)
        nx = math.sqrt(sum(v * v for v in x.values()))
        ny = math.sqrt(sum(v * v for v in y.values()))
        return dot / (nx * ny) if nx and ny else 0.0

    return 0.7 * bow_cos(wa, wb) + 0.3 * bow_cos(ba, bb)


class Embedder:
    """Cached embedder: HF inference when reachable, else lexical fallback."""

    def __init__(self, hf_token: str | None = None):
        self._hf_token = hf_token
        self._hf_ok: bool | None = None
        self._cache: dict[str, list[float] | None] = {}

    def embed(self, text: str) -> list[float] | None:
        if text in self._cache:
            return self._cache[text]
        vec = self._embed_hf(text) if self._hf_token else None
        self._cache[text] = vec
        return vec

    def _embed_hf(self, text: str) -> list[float] | None:
        if self._hf_ok is False:
            return None
        try:
            req = urllib.request.Request(
                _HF_URL,
                data=json.dumps({"inputs": text}).encode(),
                headers={
                    "Authorization": f"Bearer {self._hf_token}",
                    "Content-Type": "application/json",
                },
            )
            with urllib.request.urlopen(req, timeout=25) as resp:
                vec = json.loads(resp.read())
            if isinstance(vec, list) and vec and isinstance(vec[0], (int, float)):
                self._hf_ok = True
                return [float(x) for x in vec]
        except Exception:
            pass
        self._hf_ok = False
        return None

    def similarity(self, a: str, b: str) -> tuple[float, str]:
        """Return (score, method). Method is 'embedding' or 'lexical'."""
        va, vb = self.embed(a), self.embed(b)
        if va is not None and vb is not None:
            return _cosine(va, vb), "embedding"
        return _lexical_similarity(a, b), "lexical-fallback"


def hf_token_from_auth(path: str | None) -> str | None:
    """Extract the huggingface key from a seeded auth store (never printed)."""
    if not path or not os.path.exists(path):
        return None
    try:
        auth = json.load(open(path))
    except (json.JSONDecodeError, OSError):
        return None
    keys = auth.get("keys", {}).get("huggingface") or []
    for k in keys:
        if isinstance(k, str) and k.strip():
            return k.strip()
    return None
