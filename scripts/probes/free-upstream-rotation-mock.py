#!/usr/bin/env python3
"""Local OpenAI-compatible upstream that rate-limits exactly one API key.

Purpose: prove, against the *real* clawde binary rather than a Rust mock, that
a 429 on one credential is invisible to the user — the request is re-dispatched
on another key of the same upstream, on the same model, and the reply arrives
normally.

This exists because the Rust unit tests in
`crates/api/src/providers/key_rotating.rs` drive a mock `LlmProvider` and
therefore skip the whole transport stack: request shaping, HTTP status
handling, SSE decoding, and the TUI turn loop. The seamlessness claim in
`docs/plans/seamless-key-rotation-goals-2026-09-27.md` Goal 1 is a property of
that transport ("a rate limit must be rejected before the response object
exists"), so it can only be checked with a real socket.

Usage (debug builds only — the override hook refuses to fire in release):

    python3 scripts/probes/free-upstream-rotation-mock.py \\
        --port 18080 --throttle-key "$KEY_A" --log /tmp/rotation.jsonl &

    CLAWDE_HOME=/scratch CLAWDE_FREE_BASE_URL_NVIDIA=http://127.0.0.1:18080/v1 \\
        ./target/debug/clawde -m free/nvidia/nvidia/nemotron-3.5-lightning-30b-a3b \\
        "say hello"

Then read `--log`: the JSONL shows which credential served which model, in
order. A correct run contains a 429 for the throttled key followed by a 200 for
the healthy key, both carrying the identical `model` string.

Mirrors the NVIDIA quirks that matter here: no `Retry-After`, no
`x-ratelimit-*` headers, and a 429 body shaped like the real one. Never prints
or persists a key: the log carries a `key_id` derived from the key's last four
characters only.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# ---------------------------------------------------------------------------
# Shared state
# ---------------------------------------------------------------------------


class Recorder:
    """Append-only JSONL request log plus in-memory 429 tallies."""

    def __init__(self, log_path: str | None) -> None:
        self.log_path = log_path
        self.lock = threading.Lock()
        # key_id -> count of 429s served
        self.throttled_counts: dict[str, int] = {}
        # ordered list of (key_id, model, status) so a run can be replayed
        self.sequence: list[tuple[str, str, int]] = []
        # Held open for the process lifetime. Opening per record and flushing
        # after the `with` exits raises `ValueError: I/O operation on closed
        # file`, which aborts the handler BEFORE the response is sent — the
        # client then sees a bare connection failure instead of the status
        # under test, and the probe silently measures the wrong thing.
        self.handle = (
            open(log_path, "a", encoding="utf-8") if log_path else None  # noqa: SIM115
        )

    def record(self, key_id: str, model: str, path: str, status: int) -> None:
        with self.lock:
            self.sequence.append((key_id, model, status))
            if status == 429:
                self.throttled_counts[key_id] = self.throttled_counts.get(key_id, 0) + 1
            if self.handle is not None:
                entry = {
                    "ts": round(time.time(), 3),
                    "key_id": key_id,
                    "model": model,
                    "path": path,
                    "status": status,
                }
                self.handle.write(json.dumps(entry) + "\n")
                self.handle.flush()
                os.fsync(self.handle.fileno())

    def close(self) -> None:
        if self.handle is not None:
            self.handle.close()

    def summary(self) -> dict:
        with self.lock:
            return {
                "sequence": [list(item) for item in self.sequence],
                "throttled_counts": dict(self.throttled_counts),
            }


def key_id(authorization: str | None) -> str:
    """A stable, non-reversible-enough label for a key: its last four chars.

    These are fabricated probe keys, but the log is still written to disk, so
    only a suffix is kept — never the whole credential.
    """
    if not authorization:
        return "anonymous"
    token = authorization.strip()
    if token.lower().startswith("bearer "):
        token = token[7:].strip()
    if len(token) <= 4:
        return "short"
    return ".." + token[-4:]


# ---------------------------------------------------------------------------
# Handler
# ---------------------------------------------------------------------------


def make_handler(recorder: Recorder, throttle_key: str | None, reply: str):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"
        server_version = "mock-free-upstream/1.0"

        # Quiet: stderr chatter competes with the tmux pane being captured.
        def log_message(self, fmt: str, *args) -> None:  # noqa: A003
            return

        def _auth_key(self) -> str:
            header = self.headers.get("Authorization")
            if not header:
                return ""
            token = header.strip()
            if token.lower().startswith("bearer "):
                token = token[7:].strip()
            return token

        def _send_json(self, status: int, payload: dict) -> None:
            body = json.dumps(payload).encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self) -> None:  # noqa: N802
            path = self.path.split("?", 1)[0]
            kid = key_id(self.headers.get("Authorization"))
            if path.rstrip("/").endswith("/models"):
                # nvidia is probed through /models (models_endpoint_validates_auth)
                # so every stored key answers healthy and the chat path stays the
                # only discriminator.
                recorder.record(kid, "", path, 200)
                self._send_json(
                    200,
                    {
                        "object": "list",
                        "data": [
                            {"id": "nvidia/nemotron-3.5-lightning-30b-a3b", "object": "model"},
                            {"id": "openai/gpt-oss-20b", "object": "model"},
                        ],
                    },
                )
                return
            recorder.record(kid, "", path, 404)
            self._send_json(404, {"error": {"message": "not found", "type": "not_found"}})

        def do_POST(self) -> None:  # noqa: N802
            path = self.path.split("?", 1)[0]
            length = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(length) if length else b"{}"
            try:
                body = json.loads(raw or b"{}")
            except json.JSONDecodeError:
                body = {}
            model = str(body.get("model", ""))
            auth = self._auth_key()
            kid = key_id(self.headers.get("Authorization"))

            if not path.rstrip("/").endswith("/chat/completions"):
                recorder.record(kid, model, path, 404)
                self._send_json(404, {"error": {"message": "not found", "type": "not_found"}})
                return

            if throttle_key is not None and auth == throttle_key:
                # Deliberately no Retry-After and no x-ratelimit-* headers:
                # NVIDIA sends neither, so production falls back to the
                # per-signal default cooldown and this mock must not hand the
                # code a hint the real endpoint would never provide.
                recorder.record(kid, model, path, 429)
                self._send_json(
                    429,
                    {
                        "error": {
                            "message": "Too Many Requests",
                            "type": "rate_limit_exceeded",
                            "code": "rate_limit_exceeded",
                        }
                    },
                )
                return

            recorder.record(kid, model, path, 200)
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-cache")
            self.send_header("Connection", "close")
            self.end_headers()

            def chunk(payload: dict) -> None:
                frame = f"data: {json.dumps(payload)}\n\n".encode("utf-8")
                self.wfile.write(frame)
                self.wfile.flush()

            base = {"id": "chatcmpl-mock-1", "object": "chat.completion.chunk", "model": model}
            try:
                for piece in reply:
                    event = dict(base)
                    event["choices"] = [
                        {"index": 0, "delta": {"content": piece}, "finish_reason": None}
                    ]
                    chunk(event)
                stop = dict(base)
                stop["choices"] = [{"index": 0, "delta": {}, "finish_reason": "stop"}]
                chunk(stop)
                self.wfile.write(b"data: [DONE]\n\n")
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                # The client hung up mid-stream (watchdog, Ctrl-C, turn abort).
                pass

    return Handler


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=18080)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument(
        "--throttle-key",
        required=True,
        help="the credential that answers 429 on /chat/completions",
    )
    parser.add_argument(
        "--reply",
        default="HELLO-FROM-THE-ROTATED-KEY",
        help="text streamed back on a healthy key",
    )
    parser.add_argument("--log", default=None, help="JSONL request log path")
    parser.add_argument(
        "--ready-file",
        default=None,
        help="touch this file once the socket is listening, so a caller can wait",
    )
    args = parser.parse_args()

    recorder = Recorder(args.log)
    handler = make_handler(recorder, args.throttle_key, args.reply)
    server = ThreadingHTTPServer((args.host, args.port), handler)

    if args.ready_file:
        with open(args.ready_file, "w", encoding="utf-8") as handle:
            handle.write(str(args.port))

    print(f"mock free upstream on http://{args.host}:{args.port}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
        recorder.close()
        summary = recorder.summary()
        print(json.dumps(summary, indent=2), flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
