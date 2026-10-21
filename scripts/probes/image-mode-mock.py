#!/usr/bin/env python3
"""Local stand-in for the Google free-catalog upstream, for the image-mode probe.

Purpose: prove, against the *real* clawde binary, that a turn sent while the TUI
is in IMAGE mode carries the attached image all the way to the wire. The image
path is the one thing no Rust test can cover end to end: image mode lives in the
TUI (Tab cycle), the attachment is assembled in the CLI turn loop, the free
provider's vision gate decides which upstream may receive it, and only Google's
native request shaping turns it into an `inlineData` part. A mock `LlmProvider`
skips all four.

Google is the only vision-capable free-catalog upstream a mock can stand in for
(`github-copilot` fixes its endpoint inside the provider), which is why
`GoogleProvider` grew a base-URL override reachable through the dev-only,
debug-build-only, loopback-only `CLAWDE_FREE_BASE_URL_GOOGLE` hook.

Endpoints:

* `GET  /v1/models`            — the startup health sweep's key probe, sent to
  `{override}/models` with `?key=`.
* `GET  /v1beta/models`        — the provider's own `health_check`.
* `POST /v1beta/models/<m>:streamGenerateContent` — the TUI's streaming turn.
* `POST /v1beta/models/<m>:generateContent`       — non-streaming fallback.

Every POST is logged as JSONL with the image facts the probe asserts on:

* `image_count`   — how many `inlineData` / `fileData` parts the request carried;
* `image_sha256`  — SHA-256 of each decoded payload, so the probe can match the
  bytes it wrote to disk against the bytes that reached the wire;
* `image_mime`    — the `mimeType` the client chose (sniffed from the file);
* `text`          — the text parts of the user turn, for diagnosis.

Never prints or persists a credential: the log has no key material at all.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

DEFAULT_MODEL = "gemini-2.5-flash"


class Recorder:
    """Append-only JSONL request log."""

    def __init__(self, log_path: str | None) -> None:
        self.lock = threading.Lock()
        # Held open for the process lifetime: opening per record and flushing
        # after the `with` block exits closes the handle before the response is
        # written, which aborts the handler and turns the request under test
        # into a bare connection failure.
        self.handle = (
            open(log_path, "a", encoding="utf-8") if log_path else None  # noqa: SIM115
        )

    def record(self, entry: dict) -> None:
        with self.lock:
            if self.handle is not None:
                self.handle.write(json.dumps({"ts": round(time.time(), 3), **entry}) + "\n")
                self.handle.flush()
                os.fsync(self.handle.fileno())

    def close(self) -> None:
        if self.handle is not None:
            self.handle.close()


def image_parts(body: dict) -> list[dict]:
    """Every image-bearing part in the request, across all contents."""
    found: list[dict] = []
    for content in body.get("contents") or []:
        for part in content.get("parts") or []:
            if isinstance(part, dict) and ("inlineData" in part or "fileData" in part):
                found.append(part)
    return found


def describe_images(parts: list[dict]) -> dict:
    """Summarize the image parts without keeping the payload in the log."""
    sha256: list[str] = []
    mime: list[str] = []
    for part in parts:
        inline = part.get("inlineData")
        if isinstance(inline, dict):
            data = inline.get("data")
            if isinstance(data, str):
                try:
                    raw = base64.b64decode(data, validate=False)
                except (ValueError, TypeError):
                    raw = b""
                sha256.append(hashlib.sha256(raw).hexdigest())
            if isinstance(inline.get("mimeType"), str):
                mime.append(inline["mimeType"])
            continue
        file_part = part.get("fileData")
        if isinstance(file_part, dict):
            if isinstance(file_part.get("fileUri"), str):
                sha256.append(hashlib.sha256(file_part["fileUri"].encode()).hexdigest())
            if isinstance(file_part.get("mimeType"), str):
                mime.append(file_part["mimeType"])
    return {"image_sha256": sha256, "image_mime": mime}


def message_text(body: dict) -> str:
    out: list[str] = []
    for content in body.get("contents") or []:
        for part in content.get("parts") or []:
            if isinstance(part, dict) and isinstance(part.get("text"), str):
                out.append(part["text"])
    return "\n".join(out)


def parse_model(path: str) -> tuple[str, str]:
    """Split `/v1beta/models/<model>:<method>` into `(model, method)`."""
    tail = path.split("/v1beta/models/", 1)[-1]
    model, _, method = tail.partition(":")
    return model, method


def make_handler(recorder: Recorder, *, model_id: str, reply: str):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"
        server_version = "mock-gemini-vision/1.0"

        # Quiet: stderr chatter competes with the tmux pane being captured.
        def log_message(self, fmt: str, *args) -> None:  # noqa: A003
            return

        def _send_json(self, status: int, payload: dict) -> None:
            body = json.dumps(payload).encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def _models_payload(self) -> dict:
            return {
                "models": [
                    {
                        "name": f"models/{model_id}",
                        "displayName": "Mock Gemini",
                        "supportedGenerationMethods": [
                            "generateContent",
                            "streamGenerateContent",
                        ],
                    }
                ]
            }

        def do_GET(self) -> None:  # noqa: N802
            path = self.path.split("?", 1)[0]
            if path in ("/v1/models", "/v1beta/models") or path.rstrip("/").endswith("/models"):
                recorder.record({"method": "GET", "path": path, "status": 200})
                self._send_json(200, self._models_payload())
                return
            recorder.record({"method": "GET", "path": path, "status": 404})
            self._send_json(404, {"error": {"message": "not found", "code": 404}})

        def do_POST(self) -> None:  # noqa: N802
            path = self.path.split("?", 1)[0]
            length = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(length) if length else b"{}"
            try:
                body = json.loads(raw or b"{}")
            except json.JSONDecodeError:
                body = {}

            model, method = parse_model(path)
            parts = image_parts(body)
            # `model`/`method` come from the URL path; a request the client never
            # meant for a Gemini model (a health probe, a different wire shape)
            # is logged and refused rather than answered as if it were the turn.
            if method not in ("streamGenerateContent", "generateContent"):
                recorder.record(
                    {
                        "method": "POST",
                        "path": path,
                        "status": 404,
                        "model": model,
                        "image_count": len(parts),
                    }
                )
                self._send_json(404, {"error": {"message": "not found", "code": 404}})
                return

            entry = {
                "method": "POST",
                "path": path,
                "status": 200,
                "model": model,
                "stream": method == "streamGenerateContent",
                "image_count": len(parts),
                "text": message_text(body),
            }
            entry.update(describe_images(parts))
            recorder.record(entry)

            candidate = {
                "content": {"role": "model", "parts": [{"text": reply}]},
                "finishReason": "STOP",
            }
            usage = {
                "promptTokenCount": 12,
                "candidatesTokenCount": max(1, len(reply)),
                "totalTokenCount": 12 + max(1, len(reply)),
            }

            if method == "generateContent":
                self._send_json(
                    200, {"candidates": [candidate], "usageMetadata": usage}
                )
                return

            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-cache")
            self.send_header("Connection", "close")
            self.end_headers()

            # Gemini's SSE frames are plain `data: {candidates:[…]}` lines; the
            # client's decoder wants the `data: ` prefix (space included) and
            # treats blank lines as separators.
            frame = json.dumps(
                {"candidates": [candidate], "usageMetadata": usage}
            ).encode("utf-8")
            try:
                self.wfile.write(b"data: " + frame + b"\n\n")
                self.wfile.write(b"data: [DONE]\n\n")
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                # The client hung up mid-stream (turn abort, Ctrl-C).
                pass

    return Handler


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=18120)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument(
        "--model",
        default=DEFAULT_MODEL,
        help="the Gemini model id this instance serves (log/path label)",
    )
    parser.add_argument(
        "--reply",
        default="IMAGEOK-STREAM",
        help="text streamed back; the probe greps the pane for it",
    )
    parser.add_argument("--log", default=None, help="JSONL request log path")
    parser.add_argument(
        "--ready-file",
        default=None,
        help="touch this file once the socket is listening, so a caller can wait",
    )
    args = parser.parse_args()

    recorder = Recorder(args.log)
    handler = make_handler(recorder, model_id=args.model, reply=args.reply)
    server = ThreadingHTTPServer((args.host, args.port), handler)

    if args.ready_file:
        with open(args.ready_file, "w", encoding="utf-8") as handle:
            handle.write(str(args.port))

    print(f"mock gemini upstream on http://{args.host}:{args.port}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
        recorder.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
