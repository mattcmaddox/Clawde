#!/usr/bin/env python3
"""Browser-driven UX probe for the Cat Chat guest journey.

Why: the Cat Chat pages are one embedded HTML+JS document inside
`crates/katban/src/catchat/pages.rs` with no frontend build step, so nothing in
`cargo test` touches them. The Rust tests cover the routes, the lockout ladder
and the session store; this probe covers what a guest actually gets — that the
URL opens a sign-in gate rather than the chat box, that a wrong password is
refused in the page, that the shared password lands them in the chat and
survives a reload, that a reply and the downloadable summary render clean, and
that a locked-out local guest can be let back in.

Two rules the assertions follow, so a stale expectation cannot masquerade as a
pass or a failure:

* Observe what the guest can observe — element visibility, rendered text, the
  requests the page made. Never an internal page global: the page keeps its
  state in `let` bindings on purpose, and a top-level `let` is NOT a `window`
  property, so a probe reading `window.<state>` would assert on `undefined`.
* Wait for the content you expect (`wait_text`) instead of waiting for an
  element to be non-empty.

The guest chat needs a provider, so the probe does not depend on configured
free-tier keys: it stands up a local OpenAI-compatible upstream and points the
guest chain at it with `CLAWDE_FREE_BASE_URL_GROQ` (debug builds only, loopback
only). That also makes the reply deterministic — the mock answers with a
`reasoning</think>VISIBLE-ANSWER` shaped text, which is how an upstream that
leaves its chain of thought in the content field looks, so the probe proves the
guest never sees it.

Guest web search is covered the same way. A second local mock speaks the
SearXNG JSON API on a port the probe chooses and passes with
`--search-endpoint`, so nothing depends on a host's real sidecar (the
documented default, 127.0.0.1:8080, is the most contested port on a home host —
it was Nextcloud's here on 2026-10-13). The mock upstream answers one marked
guest message with a `web_search` tool call, which lets the probe check the
whole path: the model's query reaching the endpoint as `?q=…&format=json`, the
screened result coming back in the model's next request, the answer reaching the
guest, and both verdicts of the startup probe (a working endpoint is reported
as such; an unreachable one is reported as unavailable instead of advertised).

Safety: the probe drives a scratch `CLAWDE_HOME` (never the real `~/.clawde`),
including a scratch `auth.json`, and binds both servers to a free loopback port.

Prerequisites. The probe exits 2 (a skip, not a pass) when one is missing, so a
CI wiring that treats non-zero as failure can never read a skip as success.
`--require-browser` turns that skip into a failure instead, for CI — where a
silently skipped browser probe must not read as green:

    scripts/probes/browser-probes.sh setup     # playwright, into a private venv
    scripts/probes/browser-probes.sh catchat   # then just run it

  (pip install into the system python is refused on an externally-managed
  distro, PEP 668 — see scripts/probes/requirements.txt.)

Usage:

    python3 scripts/probes/catchat-ux-probe.py \\
        [--binary src-rust/target/debug/clawde] [--chromium /usr/bin/chromium] \\
        [--timeout-secs 180] [--keep] [--require-browser]
"""

import argparse
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# Cat Chat accepts memorable passwords; this one only has to be non-trivial to
# spray lists, and the probe owns both ends of it.
GUEST_PASSWORD = "probe-guest-pass"
# The mock's answer. The first half stands in for reasoning an upstream forgot
# to strip, the marker is what a guest must actually see.
REPLY_REASONING = "weighing the options"
REPLY_ANSWER = "PURRR-VISIBLE-ANSWER"
MOCK_REPLY = f"{REPLY_REASONING}</think>{REPLY_ANSWER}"
MOCK_KEY = "probe-groq-key-0001"

# --- Guest web search -------------------------------------------------------
# The search leg is driven the same way as the chat leg: a local mock stands in
# for the search endpoint, so the probe pins the whole path (query the model
# chose -> the HTTP request Cat Chat sends -> the content that comes back into
# the model's next request) without needing a SearXNG, a network, or a key.
# The tool call is forced by a marker in the guest's message, so the other turns
# (and the summary) stay single-round and deterministic.
SEARCH_MARKER = "SEARCH-PROBE"
SEARCH_QUERY = "clawde katban"
SEARXNG_TITLE = "SEARXNG-TITLE-MARKER"
SEARXNG_URL = "https://search.example/clawde-katban"
SEARXNG_SNIPPET = "SEARXNG-SNIPPET-MARKER the board lives at /board"
# The tool-call id the mock hands back, so the probe can name it in a failure.
SEARCH_TOOL_CALL_ID = "call_probe_search"

EXIT_PASS = 0
EXIT_FAIL = 1
EXIT_SKIP = 2
POLL_SECS = 0.2


class Skip(Exception):
    """A missing prerequisite. Reported as a skip, never as a pass."""


class Report:
    def __init__(self):
        self.failures = []
        self.checks = 0

    def check(self, label, ok, detail=""):
        self.checks += 1
        print(("PASS: " if ok else "FAIL: ") + label)
        if detail:
            for line in str(detail).splitlines():
                print("  " + line)
        if not ok:
            self.failures.append(label)


def free_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def http(method, url, payload=None, headers=None):
    """One request. Returns (status, body_text, header_text)."""
    body = json.dumps(payload).encode() if payload is not None else None
    request = urllib.request.Request(url, data=body, method=method, headers=headers or {})
    if body:
        request.add_header("content-type", "application/json")
    try:
        with urllib.request.urlopen(request, timeout=15) as response:
            return response.status, response.read().decode(), "\n".join(
                f"{key}: {value}" for key, value in response.headers.items()
            )
    except urllib.error.HTTPError as error:
        return error.code, error.read().decode(), "\n".join(
            f"{key}: {value}" for key, value in error.headers.items()
        )


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    """A 303 is the answer, not a step: the `Set-Cookie` lives on it."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


_no_redirect = urllib.request.build_opener(_NoRedirect)


def post_form(url, fields, follow_redirects=True):
    """POST a form. With `follow_redirects=False` a 303 comes back as-is."""
    body = urllib.parse.urlencode(fields).encode()
    request = urllib.request.Request(url, data=body, method="POST")
    request.add_header("content-type", "application/x-www-form-urlencoded")
    opener = urllib.request.urlopen if follow_redirects else _no_redirect.open
    try:
        with opener(request, timeout=15) as response:
            return response.status, response.read().decode(), "\n".join(
                f"{key}: {value}" for key, value in response.headers.items()
            )
    except urllib.error.HTTPError as error:
        return error.code, error.read().decode(), "\n".join(
            f"{key}: {value}" for key, value in error.headers.items()
        )


def js(page, expression):
    """Evaluate a JS expression in the page and return it JSON-normalized."""
    return json.loads(json.dumps(page.evaluate("() => (" + expression + ")")))


def text_of(page, selector):
    return js(page, f"document.querySelector({json.dumps(selector)}).textContent") or ""


def wait_text(page, selector, needle, timeout=10.0):
    """Wait until `selector`'s text contains `needle`. Returns (ok, text)."""
    deadline = time.monotonic() + timeout
    text = ""
    while time.monotonic() < deadline:
        try:
            text = text_of(page, selector)
        except Exception:  # noqa: BLE001 - page mid-navigation
            text = ""
        if needle in text:
            return True, text
        time.sleep(POLL_SECS)
    return False, text


def wait_nonempty(page, selector, timeout=10.0):
    """Wait until `selector` has any text at all. Returns (ok, text)."""
    deadline = time.monotonic() + timeout
    text = ""
    while time.monotonic() < deadline:
        try:
            text = text_of(page, selector)
        except Exception:  # noqa: BLE001 - page mid-navigation
            text = ""
        if text.strip():
            return True, text
        time.sleep(POLL_SECS)
    return False, text


def wait_visible(page, selector, timeout=10.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if page.locator(selector).first.is_visible():
                return True
        except Exception:  # noqa: BLE001 - page mid-navigation
            pass
        time.sleep(POLL_SECS)
    return False


def bot_rows(page):
    return js(
        page,
        "Array.from(document.querySelectorAll('#log .msg.bot')).map(e => e.textContent)",
    )


def launch_browser(playwright, chromium_path):
    """Prefer the requested/system chromium, fall back to playwright's build."""
    candidates = [chromium_path] if chromium_path else []
    candidates += [None, "/usr/bin/chromium", "/usr/bin/chromium-browser", "/usr/bin/google-chrome"]
    errors = []
    for candidate in candidates:
        try:
            if candidate is None:
                return playwright.chromium.launch(headless=True, args=["--no-sandbox"])
            if not os.path.exists(candidate):
                continue
            return playwright.chromium.launch(
                headless=True, args=["--no-sandbox"], executable_path=candidate
            )
        except Exception as error:  # noqa: BLE001 - collected and reported below
            errors.append(f"{candidate or 'playwright bundled chromium'}: {error}")
    raise Skip(
        "could not launch chromium — run `playwright install chromium`, or pass "
        "--chromium PATH:\n  " + "\n  ".join(errors)
    )


def tail(path, lines=30):
    if not os.path.exists(path):
        return "(no log)"
    with open(path, encoding="utf-8", errors="replace") as handle:
        return "\n".join(handle.read().splitlines()[-lines:])


# ---------------------------------------------------------------------------
# Mock upstream
# ---------------------------------------------------------------------------


def make_upstream_handler(state):
    """An OpenAI-compatible upstream that answers every chat call the same way.

    Non-streaming JSON when the client asks for it, SSE otherwise, because the
    provider may force `stream: true` on the wire. The answer text carries the
    inline reasoning this probe is about.
    """

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"
        server_version = "mock-catchat-upstream/1.0"

        def log_message(self, fmt, *args):  # noqa: A003 - quiet the socket log
            return

        def _send_json(self, status, payload):
            body = json.dumps(payload).encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def _send_tool_call(self, stream, model):
            """Answer a chat call with a single `web_search` tool call.

            Both wire shapes are needed: the free chain streams, and a decoder
            that never sees `finish_reason: tool_calls` would hand the engine a
            text turn instead of a tool round. The SSE frames mirror the shape
            the OpenAI decoder expects \u2014 one fragment carrying id, name and
            complete arguments, then the finish frame.
            """
            arguments = json.dumps({"query": SEARCH_QUERY})
            if stream:
                base = {
                    "id": "chatcmpl-probe",
                    "object": "chat.completion.chunk",
                    "model": model or "probe-model",
                }
                opening = dict(base)
                opening["choices"] = [
                    {
                        "index": 0,
                        "delta": {
                            "role": "assistant",
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "id": SEARCH_TOOL_CALL_ID,
                                    "type": "function",
                                    "function": {
                                        "name": "web_search",
                                        "arguments": arguments,
                                    },
                                }
                            ],
                        },
                        "finish_reason": None,
                    }
                ]
                stop = dict(base)
                stop["choices"] = [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Cache-Control", "no-cache")
                self.end_headers()
                for event in (opening, stop):
                    self.wfile.write(f"data: {json.dumps(event)}\n\n".encode("utf-8"))
                    self.wfile.flush()
                self.wfile.write(b"data: [DONE]\n\n")
                self.wfile.flush()
                return
            self._send_json(
                200,
                {
                    "id": "chatcmpl-probe",
                    "object": "chat.completion",
                    "model": model or "probe-model",
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": None,
                                "tool_calls": [
                                    {
                                        "id": SEARCH_TOOL_CALL_ID,
                                        "type": "function",
                                        "function": {
                                            "name": "web_search",
                                            "arguments": arguments,
                                        },
                                    }
                                ],
                            },
                            "finish_reason": "tool_calls",
                        }
                    ],
                    "usage": {
                        "prompt_tokens": 30,
                        "completion_tokens": 12,
                        "total_tokens": 42,
                    },
                },
            )

        def do_GET(self):  # noqa: N802
            with state["lock"]:
                state["requests"].append(("GET", self.path))
            if self.path.rstrip("/").endswith("/models"):
                self._send_json(
                    200,
                    {"object": "list", "data": [{"id": "probe-model", "object": "model"}]},
                )
                return
            self._send_json(404, {"error": {"message": "not found", "type": "not_found"}})

        def do_POST(self):  # noqa: N802
            length = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(length) if length else b"{}"
            try:
                body = json.loads(raw or b"{}")
            except json.JSONDecodeError:
                body = {}
            model = str(body.get("model", ""))
            text = raw.decode("utf-8", "replace")
            with state["lock"]:
                state["requests"].append(("POST", f"{self.path} model={model}"))
                state["bodies"].append(text)
                # Once per run: a guest message carrying the marker forces the
                # tool round, so the search leg is exercised deterministically.
                serve_tool_call = SEARCH_MARKER in text and not state["tool_call_served"]
                if serve_tool_call:
                    state["tool_call_served"] = True
            if not self.path.rstrip("/").endswith("/chat/completions"):
                self._send_json(404, {"error": {"message": "not found", "type": "not_found"}})
                return

            if serve_tool_call:
                self._send_tool_call(bool(body.get("stream")), model)
                return

            if body.get("stream"):
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Cache-Control", "no-cache")
                self.end_headers()

                def chunk(payload):
                    self.wfile.write(f"data: {json.dumps(payload)}\n\n".encode("utf-8"))
                    self.wfile.flush()

                base = {
                    "id": "chatcmpl-probe",
                    "object": "chat.completion.chunk",
                    "model": model or "probe-model",
                }
                event = dict(base)
                event["choices"] = [
                    {"index": 0, "delta": {"content": MOCK_REPLY}, "finish_reason": None}
                ]
                chunk(event)
                stop = dict(base)
                stop["choices"] = [{"index": 0, "delta": {}, "finish_reason": "stop"}]
                chunk(stop)
                self.wfile.write(b"data: [DONE]\n\n")
                self.wfile.flush()
                return

            self._send_json(
                200,
                {
                    "id": "chatcmpl-probe",
                    "object": "chat.completion",
                    "model": model or "probe-model",
                    "choices": [
                        {
                            "index": 0,
                            "message": {"role": "assistant", "content": MOCK_REPLY},
                            "finish_reason": "stop",
                        }
                    ],
                    "usage": {
                        "prompt_tokens": 12,
                        "completion_tokens": 8,
                        "total_tokens": 20,
                    },
                },
            )

    return Handler


def start_upstream(port):
    state = {
        "requests": [],
        # Every request body, so a check can read what the model was actually
        # handed (specifically: the screened search results in the tool round).
        "bodies": [],
        "tool_call_served": False,
        "lock": threading.Lock(),
    }
    server = ThreadingHTTPServer(("127.0.0.1", port), make_upstream_handler(state))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, state


def make_searxng_handler(state):
    """A SearXNG JSON API that also models the two ways a host gets it wrong.

    `/search` with `format=json` answers the real shape. Anything else answers
    the way a SearXNG with the JSON API disabled does \u2014 a 200 that is a page —
    so the probe would catch a client that stopped asking for JSON. Any other
    path 404s, like the unrelated service that squats on a host's 8080.
    """

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"
        server_version = "mock-searxng/1.0"

        def log_message(self, fmt, *args):  # noqa: A003 - quiet the socket log
            return

        def _send(self, status, content_type, body):
            payload = body.encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def do_GET(self):  # noqa: N802
            parsed = urllib.parse.urlparse(self.path)
            params = urllib.parse.parse_qs(parsed.query)
            with state["lock"]:
                state["requests"].append((parsed.path, params))
            if parsed.path != "/search":
                self._send(404, "text/html", "<html><body>not found</body></html>")
                return
            if params.get("format") != ["json"]:
                # JSON API disabled: a 200 page instead of results.
                self._send(200, "text/html", "<html><body>search</body></html>")
                return
            self._send(
                200,
                "application/json",
                json.dumps(
                    {
                        "query": (params.get("q") or [""])[0],
                        "results": [
                            {
                                "title": SEARXNG_TITLE,
                                "url": SEARXNG_URL,
                                "content": SEARXNG_SNIPPET,
                            }
                        ],
                    }
                ),
            )

    return Handler


def start_searxng(port):
    state = {"requests": [], "lock": threading.Lock()}
    server = ThreadingHTTPServer(("127.0.0.1", port), make_searxng_handler(state))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, state


# ---------------------------------------------------------------------------
# Harness
# ---------------------------------------------------------------------------


def run_cli(binary, env, *args, timeout=60):
    completed = subprocess.run(
        [binary, *args],
        env=env,
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    return completed.returncode, completed.stdout, completed.stderr


def create_link(binary, env):
    """Create a guest link with a password the probe knows, and return its id."""
    code, out, err = run_cli(binary, env, "catchat", "links", "create", "probe")
    if code != 0:
        raise RuntimeError(f"`catchat links create` failed ({code}): {out}{err}")
    link_id = out.split("(")[-1].split(")")[0].strip()
    code, out, err = run_cli(
        binary, env, "catchat", "links", "password", link_id, "--set", GUEST_PASSWORD
    )
    if code != 0:
        raise RuntimeError(f"`catchat links password --set` failed ({code}): {out}{err}")
    return link_id


def start_server(binary, env, port, log_path, timeout, search_endpoint=None):
    log = open(log_path, "w", encoding="utf-8")
    args = [binary, "catchat", "serve", "--host", "127.0.0.1", "--port", str(port)]
    if search_endpoint:
        args += ["--search-endpoint", search_endpoint]
    process = subprocess.Popen(
        args,
        env=env,
        stdout=log,
        stderr=subprocess.STDOUT,
        text=True,
    )
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(
                f"catchat server exited early with code {process.returncode}; log:\n{tail(log_path)}"
            )
        try:
            status, body, _ = http("GET", f"http://127.0.0.1:{port}/")
            if status == 200 and "Cat Chat" in body:
                return process
        except Exception:  # noqa: BLE001 - not listening yet
            pass
        time.sleep(0.25)
    process.kill()
    raise RuntimeError(
        f"catchat server did not answer / within {timeout}s; log:\n{tail(log_path)}"
    )


def read_text(path):
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as handle:
            return handle.read()
    except OSError:
        return ""


def serve_startup_search_line(binary, env, port, endpoint, log_path, timeout=30):
    """Run `catchat serve` just long enough to read its `guest search:` verdict.

    The startup line is the only place a host learns that the endpoint it
    configured is not actually a SearXNG JSON API, so the probe asserts on the
    real log of the real binary rather than on the adapter in isolation.
    """
    with open(log_path, "w", encoding="utf-8") as log:
        process = subprocess.Popen(
            [
                binary,
                "catchat",
                "serve",
                "--host",
                "127.0.0.1",
                "--port",
                str(port),
                "--search-endpoint",
                endpoint,
            ],
            env=env,
            stdout=log,
            stderr=subprocess.STDOUT,
            text=True,
        )
    deadline = time.monotonic() + timeout
    try:
        while time.monotonic() < deadline:
            if "guest search:" in read_text(log_path):
                # The verdict's continuation lines follow immediately; give the
                # process a moment to finish writing them before reading.
                time.sleep(0.5)
                break
            time.sleep(0.2)
    finally:
        process.kill()
        try:
            process.wait(timeout=10)
        except Exception:  # noqa: BLE001
            pass
    return read_text(log_path)


def run_checks(args, report, page, base, env, binary, log_path):
    # --- The gate a stranger hits first ------------------------------------
    page.goto(base + "/", wait_until="domcontentloaded")
    report.check(
        "the URL opens a sign-in gate, not the chat box",
        page.locator("#pw").first.is_visible() and not page.locator("#msg").first.is_visible(),
        f"pw visible={page.locator('#pw').first.is_visible()} "
        f"msg visible={page.locator('#msg').first.is_visible()}",
    )

    # --- A wrong password is refused in the page ---------------------------
    page.fill("#pw", "not-the-password")
    page.click("#join")
    # Any refusal text will do — the noise is picked at random per request, so
    # asserting on one of them would flake.
    ok, text = wait_nonempty(page, "#err")
    report.check(
        "a wrong password is refused in the page and keeps the gate",
        ok and page.url.rstrip("/") == base,
        f"url={page.url} err={text!r}",
    )
    report.check(
        "the gate is still the visible surface after a refusal",
        page.locator("#pw").first.is_visible() and not page.locator("#msg").first.is_visible(),
    )

    # --- The shared password lands the guest in the chat -------------------
    page.fill("#pw", GUEST_PASSWORD)
    page.click("#join")
    page.wait_for_url("**/chat", timeout=15000)
    report.check(
        "the shared password lands the guest in the chat",
        page.locator("#msg").first.is_visible(),
        f"url={page.url}",
    )

    # --- The reply renders, and the upstream's reasoning does not ----------
    page.fill("#msg", "say something")
    page.click("#send")
    deadline = time.monotonic() + args.timeout_secs
    rows = []
    while time.monotonic() < deadline:
        rows = bot_rows(page)
        if rows:
            break
        page.wait_for_timeout(POLL_SECS * 1000)
    report.check(
        "a reply renders in the guest's transcript",
        bool(rows) and any(REPLY_ANSWER in row for row in rows),
        f"rows={rows}",
    )
    joined = " | ".join(rows)
    leaked = [
        marker
        for marker in (REPLY_REASONING, "think>", "[thinking:", "[used ")
        if marker in joined
    ]
    report.check(
        "the guest never sees upstream reasoning or tool markers",
        not leaked,
        f"leaked={leaked} rows={rows}",
    )

    # --- Guest slash commands are answered by the host, not the model ------
    # `/help` must render the host's own vocabulary, which means it also must
    # not cost a model call: the mock upstream sees no new request and the
    # reply is not the mock's answer text. `/katban` is the one command the
    # default host withholds, so it must refuse instead of pretending not to
    # exist. The modeline has to follow the command that changed it.
    ok, hint = wait_text(page, "#mlHint", "/help")
    report.check(
        "the modeline advertises the guest command surface",
        ok and "commands" in hint,
        f"hint={hint!r}",
    )
    with args.upstream_state["lock"]:
        before_requests = len(args.upstream_state["requests"])
    before_rows = len(bot_rows(page))
    page.fill("#msg", "/help")
    page.click("#send")
    deadline = time.monotonic() + args.timeout_secs
    help_rows = []
    while time.monotonic() < deadline:
        help_rows = bot_rows(page)
        if len(help_rows) > before_rows:
            break
        page.wait_for_timeout(POLL_SECS * 1000)
    help_text = " | ".join(help_rows[before_rows:])
    report.check(
        "a guest `/help` is answered with the host's command list",
        "/model" in help_text and "/cathead" in help_text and REPLY_ANSWER not in help_text,
        f"rows={help_rows}",
    )
    with args.upstream_state["lock"]:
        after_requests = len(args.upstream_state["requests"])
    report.check(
        "a guest command never reaches the upstream",
        after_requests == before_requests,
        f"before={before_requests} after={after_requests}",
    )
    # A command the host withholds is refused by name, still without a model
    # call, rather than answered as prose.
    before_rows = len(bot_rows(page))
    page.fill("#msg", "/katban")
    page.click("#send")
    deadline = time.monotonic() + args.timeout_secs
    katban_rows = []
    while time.monotonic() < deadline:
        katban_rows = bot_rows(page)
        if len(katban_rows) > before_rows:
            break
        page.wait_for_timeout(POLL_SECS * 1000)
    report.check(
        "a command the host withholds is refused, not sent to the model",
        "host-only command" in " | ".join(katban_rows[before_rows:]),
        f"rows={katban_rows}",
    )
    # `/effort` moves the live modeline, which is the browser-visible proof
    # that the command mutated this session rather than answering in prose.
    page.fill("#msg", "/effort high")
    page.click("#send")
    ok, effort = wait_text(page, "#mlEffort", "high")
    report.check(
        "`/effort` updates the live modeline",
        ok,
        f"effort={effort!r}",
    )

    # --- The startup line tells the truth about the search endpoint --------
    # `catchat serve` used to announce a local SearXNG without asking, so a host
    # whose port belonged to something else advertised a search feature that
    # could only fail. It now probes, and the verdict has to reach the log.
    log_text = read_text(log_path)
    report.check(
        "`catchat serve` reports a probed endpoint, not a blind claim",
        "guest search:" in log_text and "(SearXNG JSON API answered)" in log_text,
        f"log={log_text[:400]!r}",
    )

    # --- The session survives a reload ------------------------------------
    page.reload(wait_until="domcontentloaded")
    report.check(
        "the session survives a reload (cookie, not page state)",
        wait_visible(page, "#msg") and page.url.rstrip("/").endswith("/chat"),
        f"url={page.url}",
    )

    # --- A guest turn that has to search the web ---------------------------
    # The mock upstream answers this message (and only this one) with a
    # `web_search` tool call, so the whole guest search path is exercised for
    # real: the model's query goes to the search endpoint, the screened result
    # comes back as a tool result, and the guest sees the follow-up answer.
    before = len(bot_rows(page))
    page.fill("#msg", f"{SEARCH_MARKER} what is on the web about the board?")
    page.click("#send")
    deadline = time.monotonic() + args.timeout_secs
    rows = []
    while time.monotonic() < deadline:
        rows = bot_rows(page)
        if len(rows) > before:
            break
        page.wait_for_timeout(POLL_SECS * 1000)
    report.check(
        "a guest turn that needs the web gets an answer",
        any(REPLY_ANSWER in row for row in rows),
        f"before={before} rows={rows}",
    )
    report.check(
        "a searched turn never tells the guest search is unavailable",
        not any("unavailable" in row.lower() for row in rows),
        f"rows={rows}",
    )

    with args.searxng_state["lock"]:
        searches = list(args.searxng_state["requests"])
    query_params = [params for path, params in searches if path == "/search"]
    report.check(
        "the search goes to the configured endpoint as a SearXNG JSON query",
        bool(query_params) and all(params.get("format") == ["json"] for params in query_params),
        f"searches={searches}",
    )
    report.check(
        "the query the model chose reaches the endpoint verbatim",
        any(params.get("q") == [SEARCH_QUERY] for params in query_params),
        f"queries={[params.get('q') for params in query_params]}",
    )
    with args.upstream_state["lock"]:
        bodies = list(args.upstream_state["bodies"])
    carried = sum(1 for body in bodies if SEARXNG_SNIPPET in body)
    report.check(
        "the screened search result reaches the model's next request",
        carried > 0,
        f"bodies carrying the snippet: {carried}/{len(bodies)}",
    )

    # --- The downloadable summary renders clean ---------------------------
    page.click("#sumBtn")
    ok, summary = wait_text(page, "#sumBody", REPLY_ANSWER, timeout=args.timeout_secs)
    report.check(
        "the downloadable summary renders the session's answer",
        ok and page.locator("#summary").first.is_visible(),
        f"summary={summary[:200]!r}",
    )
    summary_leaked = [
        marker for marker in (REPLY_REASONING, "think>", "[thinking:", "[used ") if marker in summary
    ]
    report.check(
        "the summary never carries upstream reasoning either",
        not summary_leaked,
        f"leaked={summary_leaked} summary={summary[:200]!r}",
    )

    # --- HTTP-level guarantees about the guest session --------------------
    status, _, headers = post_form(
        base + "/auth", {"password": GUEST_PASSWORD}, follow_redirects=False
    )
    cookie_line = next(
        (line for line in headers.splitlines() if line.lower().startswith("set-cookie:")), ""
    )
    report.check(
        "the guest cookie is HttpOnly and SameSite",
        status in (200, 303) and "HttpOnly" in cookie_line and "SameSite" in cookie_line,
        f"status={status} set-cookie={cookie_line}",
    )
    status, body, _ = http("POST", base + "/api/chat", {"message": "hi"})
    report.check(
        "an unauthenticated chat call is refused",
        status == 401,
        f"status={status} body={body[:120]!r}",
    )

    # --- The lockout ladder and the operator's escape hatch ---------------
    # Four wrong passwords arm the 3-minute window. A local client is filed
    # under the 'loopback' bucket, so this also pins the bucket name an
    # operator has to see (and pass) to let the guest back in.
    for _ in range(4):
        post_form(base + "/auth", {"password": "wrong-again"})
    code, out, err = run_cli(binary, env, "catchat", "status")
    report.check(
        "a locked-out local guest shows up as the 'loopback' bucket",
        code == 0 and "loopback (lockout" in out,
        f"rc={code} out={out!r} err={err!r}",
    )
    status, _, _ = post_form(base + "/auth", {"password": GUEST_PASSWORD})
    report.check(
        "the correct password is refused while the lockout is live",
        status == 429,
        f"status={status}",
    )
    code, out, err = run_cli(binary, env, "catchat", "unblock", "127.0.0.1")
    report.check(
        "`catchat unblock 127.0.0.1` clears the loopback bucket",
        code == 0 and "loopback" in out,
        f"rc={code} out={out!r} err={err!r}",
    )
    status, _, _ = post_form(base + "/auth", {"password": GUEST_PASSWORD})
    report.check(
        "the guest can sign in again after the unblock",
        status in (200, 303),
        f"status={status}",
    )
    code, out, err = run_cli(binary, env, "catchat", "unblock", "127.0.0.1")
    report.check(
        "a second unblock reports the miss instead of claiming a clear",
        code != 0 and "nothing to clear" in (out + err),
        f"rc={code} out={out!r} err={err!r}",
    )

    # --- The mock actually served the chat --------------------------------
    with args.upstream_state["lock"]:
        served = list(args.upstream_state["requests"])
    report.check(
        "the reply came from the mocked upstream (the probe graded a real turn)",
        any("chat/completions" in entry[1] for entry in served),
        f"requests={served}",
    )
    _ = log_path


def main():
    parser = argparse.ArgumentParser(description="Cat Chat guest-journey UX probe")
    parser.add_argument("--binary", default="src-rust/target/debug/clawde")
    parser.add_argument("--chromium", default=None, help="chrome/chromium to drive")
    parser.add_argument("--timeout-secs", type=int, default=180)
    parser.add_argument("--keep", action="store_true", help="keep the scratch dir")
    parser.add_argument(
        "--require-browser",
        action="store_true",
        help="fail (1) instead of skipping (2) when playwright/chromium is missing",
    )
    args = parser.parse_args()

    args.binary = os.path.abspath(args.binary)
    if not os.path.exists(args.binary):
        print(f"catchat-ux-probe: binary not found: {args.binary} (build first: cargo build)")
        return EXIT_FAIL
    try:
        from playwright.sync_api import sync_playwright
    except Exception as error:  # noqa: BLE001
        reason = (
            f"playwright is not installed — {error}\n"
            "  scripts/probes/browser-probes.sh setup   # installs it into a venv\n"
            "  scripts/probes/browser-probes.sh catchat # then run this probe"
        )
        if args.require_browser:
            print(f"catchat-ux-probe: FAIL — {reason}")
            return EXIT_FAIL
        print(f"catchat-ux-probe: SKIPPED — {reason}")
        return EXIT_SKIP

    scratch = tempfile.mkdtemp(prefix="catchat-ux-probe-")
    home = os.path.join(scratch, "home")
    os.makedirs(home)
    log_path = os.path.join(scratch, "server.log")
    # A scratch auth store holding one fabricated upstream key, so the guest
    # chain has an upstream to call and the real ~/.clawde is never read.
    with open(os.path.join(home, "auth.json"), "w", encoding="utf-8") as handle:
        json.dump({"credentials": {}, "keys": {"groq": [MOCK_KEY]}}, handle)

    upstream_port = free_port()
    searxng_port = free_port()
    port = free_port()
    base = f"http://127.0.0.1:{port}"
    env = os.environ.copy()
    env["CLAWDE_HOME"] = home
    env["NO_COLOR"] = "1"
    env["CI"] = "1"
    # Debug-build-only override, loopback-only (see crates/api providers/free):
    # points the groq upstream at the mock instead of the real endpoint.
    env["CLAWDE_FREE_BASE_URL_GROQ"] = f"http://127.0.0.1:{upstream_port}/v1"

    upstream, state = start_upstream(upstream_port)
    args.upstream_state = state
    searxng, searxng_state = start_searxng(searxng_port)
    args.searxng_state = searxng_state
    server = None
    report = Report()
    try:
        try:
            create_link(args.binary, env)
        except Exception as error:  # noqa: BLE001
            print(f"FAIL: could not create the guest link\n  {error}")
            return EXIT_FAIL
        try:
            server = start_server(
                args.binary,
                env,
                port,
                log_path,
                args.timeout_secs,
                search_endpoint=f"http://127.0.0.1:{searxng_port}",
            )
        except RuntimeError as error:
            print(f"FAIL: the Cat Chat server did not come up\n  {error}")
            return EXIT_FAIL

        with sync_playwright() as play:
            browser = launch_browser(play, args.chromium)
            context = browser.new_context(viewport={"width": 1200, "height": 950}, locale="en-US")
            page = context.new_page()
            dialogs = []
            page.on("dialog", lambda dialog: (dialogs.append(dialog.type), dialog.accept())[1])
            console_errors = []
            page.on(
                "console",
                lambda message: console_errors.append(
                    (message.type, message.text, (message.location or {}).get("url", ""))
                )
                if message.type == "error"
                else None,
            )
            page.on("pageerror", lambda error: console_errors.append(("pageerror", str(error), "")))

            run_checks(args, report, page, base, env, args.binary, log_path)

            # Two failures are the documented behaviour, not defects: a
            # browser keeps no favicon for Cat Chat, and a refused sign-in is a
            # 401 on /auth — the browser logging that 401 IS the refusal the
            # probe just graded in the page above.
            bad = [
                f"[{kind}] {text} ({url or 'no url'})"
                for kind, text, url in console_errors
                if "favicon" not in url and not ("401" in text and url.endswith("/auth"))
            ]
            report.check("no console or page errors on the guest journey", not bad, "\n".join(bad))
            report.check(
                "a refused sign-in never raises a blocking browser dialog",
                not dialogs,
                f"dialogs={dialogs}",
            )
            browser.close()

        # --- The other half of the startup line: honest degradation --------
        # A host whose endpoint is not a SearXNG JSON API has to be told, rather
        # than left to discover it from a guest's failed search. A port with
        # nothing on it is the "could not reach it" shape.
        unavailable_log = serve_startup_search_line(
            args.binary,
            env,
            free_port(),
            f"http://127.0.0.1:{free_port()}",
            os.path.join(scratch, "search-unavailable.log"),
        )
        report.check(
            "an endpoint that is not usable search is reported, not advertised",
            "guest search:" in unavailable_log
            and "WARNING: unavailable" in unavailable_log
            and "CLAWDE_CATCHAT_SEARCH_ENDPOINT" in unavailable_log,
            f"log={unavailable_log[:400]!r}",
        )
    except Skip as skip:
        if args.require_browser:
            print(f"catchat-ux-probe: FAIL — {skip}")
            return EXIT_FAIL
        print(f"catchat-ux-probe: SKIPPED — {skip}")
        return EXIT_SKIP
    except Exception as error:  # noqa: BLE001
        print(f"FAIL: the probe aborted — {error}")
        print(tail(log_path))
        return EXIT_FAIL
    finally:
        if server is not None:
            server.kill()
            try:
                server.wait(timeout=10)
            except Exception:  # noqa: BLE001
                pass
        upstream.shutdown()
        searxng.shutdown()
        if args.keep:
            print(f"scratch dir kept: {scratch}")
        else:
            shutil.rmtree(scratch, ignore_errors=True)

    passed = report.checks - len(report.failures)
    print(f"\n{passed}/{report.checks} checks passed")
    if report.failures:
        print("failed checks:")
        for label in report.failures:
            print(f"- {label}")
        return EXIT_FAIL
    return EXIT_PASS


if __name__ == "__main__":
    sys.exit(main())
