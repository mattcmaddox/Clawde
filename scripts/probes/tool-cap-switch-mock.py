#!/usr/bin/env python3
"""Local OpenAI-compatible upstreams for the tool-call-cap switch probe.

Two instances are used by `tool-cap-switch-probe.sh`:

* one stands in for the capped upstream (nvidia): on the first request that
  carries the probe marker it answers with text and
  `finish_reason: "max_tool_calls"` — the wire shape of an upstream that stopped
  the turn at its own tool-call cap — and answers normally afterwards;
* one stands in for the upstream the resume must move to (groq): it always
  answers normally and numbers each reply so the pane can be checked turn by
  turn.

Why this exists: the unit tests around `StopKind` drive a Rust `LlmProvider`
double, which never exercises the transport (request shaping, SSE decoding,
`finish_reason` mapping) nor the real TUI loop. The claim "a provider cap resumes
on another upstream, with the prior transcript" is a property of that stack, so
it needs a real socket and the real binary.

Every chat request is logged as JSONL with:

* `mode` — `"cap"` (the synthetic cap turn) or `"reply"`;
* `contains_probe` — whether the request carried the probe marker at all (the
  startup health sweep does not, so it can be filtered out);
* `contains_resume_prompt` — whether the request's messages contain the
  tool-call-cap resume prompt, i.e. the handoff actually carried the note;
* `message_count` — the transcript length the upstream was handed.

Never prints or persists a key: the log carries only the model and message
metadata. Debug builds only — the client-side override refuses non-loopback
hosts and release binaries.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# The prompt the probe types. Only requests containing it are candidates for the
# synthetic cap, so the startup health sweep's 1-token chat confirm can never
# consume it.
DEFAULT_MARKER = "TOOLCAP-PROBE"


class Recorder:
    """Append-only JSONL request log plus a reply counter for numbered output."""

    def __init__(self, log_path: str | None) -> None:
        self.log_path = log_path
        self.lock = threading.Lock()
        self.replies = 0
        self.capped = False
        self.handle = (
            open(log_path, "a", encoding="utf-8") if log_path else None  # noqa: SIM115
        )

    def record(self, entry: dict) -> None:
        with self.lock:
            if self.handle is not None:
                entry = {"ts": round(time.time(), 3), **entry}
                self.handle.write(json.dumps(entry) + "\n")
                self.handle.flush()
                os.fsync(self.handle.fileno())

    def take_cap(self, eligible: bool, cap_once: bool) -> bool:
        """Claim the single synthetic cap turn, if this request is eligible.

        An instance only caps when started with `--cap-once`: the resume mock
        must answer normally, so absence of the flag means "never cap", not
        "cap every time".
        """
        with self.lock:
            if not (eligible and cap_once) or self.capped:
                return False
            self.capped = True
            return True

    def next_reply_number(self) -> int:
        with self.lock:
            self.replies += 1
            return self.replies

    def close(self) -> None:
        if self.handle is not None:
            self.handle.close()


def message_texts(body: dict) -> list[str]:
    out: list[str] = []
    for message in body.get("messages") or []:
        content = message.get("content")
        if isinstance(content, str):
            out.append(content)
        elif isinstance(content, list):
            for part in content:
                if isinstance(part, dict) and isinstance(part.get("text"), str):
                    out.append(part["text"])
    return out


def make_handler(
    recorder: Recorder,
    *,
    upstream: str,
    reply: str,
    marker: str,
    cap_once: bool,
):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"
        server_version = "mock-tool-cap-upstream/1.0"

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

        def do_GET(self) -> None:  # noqa: N802
            path = self.path.split("?", 1)[0]
            if path.rstrip("/").endswith("/models"):
                recorder.record(
                    {
                        "upstream": upstream,
                        "model": "",
                        "path": path,
                        "status": 200,
                        "mode": "models",
                        "contains_probe": False,
                        "contains_resume_prompt": False,
                        "message_count": 0,
                    }
                )
                self._send_json(
                    200,
                    {
                        "object": "list",
                        "data": [{"id": "probe/wire-model", "object": "model"}],
                    },
                )
                return
            recorder.record(
                {
                    "upstream": upstream,
                    "model": "",
                    "path": path,
                    "status": 404,
                    "mode": "not-found",
                    "contains_probe": False,
                    "contains_resume_prompt": False,
                    "message_count": 0,
                }
            )
            self._send_json(404, {"error": {"message": "not found"}})

        def do_POST(self) -> None:  # noqa: N802
            path = self.path.split("?", 1)[0]
            length = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(length) if length else b"{}"
            try:
                body = json.loads(raw or b"{}")
            except json.JSONDecodeError:
                body = {}
            model = str(body.get("model", ""))
            texts = message_texts(body)
            joined = "\n".join(texts)
            contains_probe = marker in joined
            contains_resume_prompt = "tool-call cap" in joined

            if not path.rstrip("/").endswith("/chat/completions"):
                recorder.record(
                    {
                        "upstream": upstream,
                        "model": model,
                        "path": path,
                        "status": 404,
                        "mode": "not-found",
                        "contains_probe": contains_probe,
                        "contains_resume_prompt": contains_resume_prompt,
                        "message_count": len(texts),
                    }
                )
                self._send_json(404, {"error": {"message": "not found"}})
                return

            # A resume request already carries the cap prompt, so it is never
            # itself capped: only the turn that started the task can stop at an
            # upstream's tool-call cap.
            cap_turn = recorder.take_cap(
                contains_probe and not contains_resume_prompt, cap_once
            )
            if cap_turn:
                finish = "max_tool_calls"
                pieces = ["partial work before the cap"]
            elif contains_probe:
                finish = "stop"
                pieces = [f"{reply}-{recorder.next_reply_number()}"]
            else:
                # Health sweep / key probes: answer without consuming a reply
                # number, so the pane's turn markers stay 1 and 2.
                finish = "stop"
                pieces = [f"{reply}-probe"]

            recorder.record(
                {
                    "upstream": upstream,
                    "model": model,
                    "path": path,
                    "status": 200,
                    "mode": "cap" if cap_turn else "reply",
                    "contains_probe": contains_probe,
                    "contains_resume_prompt": contains_resume_prompt,
                    "message_count": len(texts),
                }
            )

            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-cache")
            self.send_header("Connection", "close")
            self.end_headers()

            def chunk(payload: dict) -> None:
                frame = f"data: {json.dumps(payload)}\n\n".encode("utf-8")
                self.wfile.write(frame)
                self.wfile.flush()

            base = {"id": "chatcmpl-mock-toolcap", "object": "chat.completion.chunk", "model": model}
            try:
                for piece in pieces:
                    event = dict(base)
                    event["choices"] = [
                        {"index": 0, "delta": {"content": piece}, "finish_reason": None}
                    ]
                    chunk(event)
                stop = dict(base)
                stop["choices"] = [{"index": 0, "delta": {}, "finish_reason": finish}]
                chunk(stop)
                self.wfile.write(b"data: [DONE]\n\n")
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                # The client hung up mid-stream (watchdog, Ctrl-C, turn abort).
                pass

    return Handler


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=18110)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument(
        "--upstream",
        default="nvidia",
        help="catalog id this instance stands in for (log label only)",
    )
    parser.add_argument(
        "--reply",
        default="PROBE-REPLY",
        help="text streamed back; each reply appends its 1-based number",
    )
    parser.add_argument(
        "--marker",
        default=DEFAULT_MARKER,
        help="prompt substring that makes a request eligible for the cap turn",
    )
    parser.add_argument(
        "--cap-once",
        action="store_true",
        help="answer the first marked request with finish_reason max_tool_calls",
    )
    parser.add_argument("--log", default=None, help="JSONL request log path")
    parser.add_argument(
        "--ready-file",
        default=None,
        help="touch this file once the socket is listening, so a caller can wait",
    )
    args = parser.parse_args()

    recorder = Recorder(args.log)
    handler = make_handler(
        recorder,
        upstream=args.upstream,
        reply=args.reply,
        marker=args.marker,
        cap_once=args.cap_once,
    )
    server = ThreadingHTTPServer((args.host, args.port), handler)

    if args.ready_file:
        with open(args.ready_file, "w", encoding="utf-8") as handle:
            handle.write(str(args.port))

    print(f"mock {args.upstream} upstream on http://{args.host}:{args.port}", flush=True)
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
