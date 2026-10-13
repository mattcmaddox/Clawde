#!/usr/bin/env python3
"""Browser-driven UX probe for the Katban board web UI.

Why: the board UI is one embedded HTML+JS document inside
`crates/katban/src/board_server.rs` with no frontend build step, so nothing in
`cargo test` touches it. The Rust tests cover the routes, the origin rules and
the session store; this probe covers what a browser actually sees — that
signing in reveals the write toolbar, that a reload restores the session from
the HttpOnly cookie, that board/card/dependency actions work through the
rendered controls, and that signing out makes the board read-only again.

Two rules the assertions follow, so a stale expectation cannot masquerade as a
pass or a failure:

* Observe what the user can observe — element visibility, rendered text, the
  requests the page made. Never an internal page global: the UI keeps its state
  in `let` bindings on purpose, and a top-level `let` is NOT a `window`
  property, so a probe reading `window.<state>` would assert on `undefined`.
* Wait for the content you expect (`wait_text`) instead of waiting for an
  element to be non-empty — `#empty` and `#connection` already carry text from
  the first paint, so a truthiness wait returns immediately and grades the
  pre-action content.

Safety: the probe drives a scratch `CLAWDE_HOME` (never the real `~/.clawde`)
and starts the board WITHOUT `--run`. The `--run` flag spawns the card
executor, which would dispatch real agent runs at whatever cards the probe
creates.

The board's *exposed* path is covered without standing up caddy. The
`X-Forwarded-*` headers a proxy adds are replayed at the HTTP level — a read
without a session is refused, the session cookie comes back `Secure`, and the
same read succeeds with that cookie — and the browser half intercepts
`/api/projects` and `/api/runner` with the 401 the exposed board returns, so the
signed-out gate the page renders is graded too. The proxy leg itself (caddy
setting the headers) stays in the Rust tests, notably
`board_server.rs::exposed_reads_require_auth_but_loopback_stays_open`.

Prerequisites. The probe exits 2 (a skip, not a pass) when one is missing, so a
CI wiring that treats non-zero as failure can never read a skip as success.
`--require-browser` turns that skip into a failure instead, for CI — where a
silently skipped browser probe must not read as green:

    python3 -c "import playwright"       # pip install playwright
    playwright install chromium          # or pass --chromium /path/to/chrome

Usage:

    python3 scripts/probes/katban-board-ux-probe.py \
        [--binary src-rust/target/debug/clawde] [--chromium /usr/bin/chromium] \
        [--timeout-secs 120] [--keep] [--require-browser]
"""

import argparse
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

# Passes admin_auth::validate_password: >=16 chars, not all digits, not all
# alphanumeric (a space and punctuation), not a single repeated character.
ADMIN_PASSWORD = "katban probe password!"
EXIT_PASS = 0
EXIT_FAIL = 1
EXIT_SKIP = 2
POLL_SECS = 0.2
TOAST_TEXTS = "Array.from(document.querySelectorAll('.toast')).map(e => e.textContent)"


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


def visible(page, selector):
    """True only when the element is rendered — not merely present in the DOM."""
    return page.locator(selector).first.is_visible()


def free_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def http(method, url, payload=None, headers=None):
    """One cookie-free request. Returns (status, body_text, header_text)."""
    body = json.dumps(payload).encode() if payload is not None else None
    request = urllib.request.Request(url, data=body, method=method, headers=headers or {})
    if body:
        request.add_header("content-type", "application/json")
    try:
        with urllib.request.urlopen(request, timeout=10) as response:
            return response.status, response.read().decode(), "\n".join(
                f"{key}: {value}" for key, value in response.headers.items()
            )
    except urllib.error.HTTPError as error:
        return error.code, error.read().decode(), "\n".join(
            f"{key}: {value}" for key, value in error.headers.items()
        )


def session_cookie(headers_text):
    """The `katban_admin` value from a response's Set-Cookie line, if any."""
    for line in headers_text.splitlines():
        if line.lower().startswith("set-cookie:") and "katban_admin=" in line:
            return line.split("katban_admin=", 1)[1].split(";", 1)[0]
    return None


def js(page, expression):
    """Evaluate a JS expression in the page and return it JSON-normalized."""
    return json.loads(json.dumps(page.evaluate("() => (" + expression + ")")))


def wait_for(page, expression, timeout=10.0):
    """Poll a JS expression until it is truthy. Returns (ok, last_value)."""
    deadline = time.monotonic() + timeout
    value = None
    while time.monotonic() < deadline:
        try:
            value = js(page, expression)
        except Exception:  # noqa: BLE001 - page mid-navigation
            value = None
        if value:
            return True, value
        time.sleep(POLL_SECS)
    return False, value


def text_of(page, selector):
    return js(page, f"document.querySelector({json.dumps(selector)}).textContent") or ""


def wait_text(page, selector, needle, timeout=10.0):
    """Wait until `selector`'s text contains `needle`. Returns (ok, text).

    Predicate-based on purpose: `#empty` and `#connection` hold text from the
    first paint, so waiting for the element to be non-empty would grade the
    content that was there before the action under test.
    """
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


def prompts_of(page):
    return js(
        page,
        "Array.from(document.querySelectorAll('.card .prompt')).map(e => e.textContent)",
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


def unexpected_console_errors(console_errors, refused_routes=()):
    """Console noise that is NOT part of the board's documented behaviour.

    Three failures are expected and therefore not defects: a browser keeps no
    favicon for the board, `/api/me` answers 401 whenever the visitor is
    signed out (that 401 is exactly how the page decides to render the
    signed-out state), and the exposed-board leg refuses reads on purpose —
    the browser logging that 401 is the same thing an exposed board does to
    every signed-out visitor, and the leg grades the page's response to it.
    `refused_routes` carries only the routes the probe itself refuses, so a
    genuine failure anywhere else still counts. Anything else — including any
    `pageerror` — counts.
    """
    failures = []
    for kind, text, url in console_errors:
        if "favicon" in url:
            continue
        if url.endswith("/api/me") and "401" in text:
            continue
        if "401" in text and any(url.endswith(route) for route in refused_routes):
            continue
        failures.append(f"[{kind}] {text} ({url or 'no url'})")
    return failures


def tail(path, lines=40):
    if not os.path.exists(path):
        return "(no log)"
    with open(path, encoding="utf-8", errors="replace") as handle:
        return "\n".join(handle.read().splitlines()[-lines:])


def start_board(binary, env, cwd, port, log_path, timeout):
    """Start the board server and wait for /api/projects to answer 200."""
    log = open(log_path, "w", encoding="utf-8")
    process = subprocess.Popen(
        [binary, "katban", "board", "serve", "--host", "127.0.0.1", "--port", str(port)],
        cwd=cwd,
        env=env,
        stdout=log,
        stderr=subprocess.STDOUT,
        text=True,
    )
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(
                f"board server exited early with code {process.returncode}; log:\n{tail(log_path)}"
            )
        try:
            status, body, _ = http("GET", f"http://127.0.0.1:{port}/api/projects")
            if status == 200 and isinstance(json.loads(body), list):
                return process
        except Exception:  # noqa: BLE001 - not listening yet
            pass
        time.sleep(0.25)
    process.kill()
    raise RuntimeError(
        f"board server did not answer /api/projects within {timeout}s; log:\n{tail(log_path)}"
    )


def run_checks(args, report, page, context, base, env, project, log_path, refused):
    # ---- signed-out board: reads open, writes admin-gated -----------------
    status, body, headers = http("GET", base + "/api/me")
    report.check(
        "signed out: /api/me reports authed=false and sets no cookie",
        status == 401
        and json.loads(body).get("authed") is False
        and "set-cookie" not in headers.lower(),
        f"status={status} body={body.strip()}",
    )

    status, body, _ = http("POST", base + "/api/login", {"password": "wrong"})
    report.check(
        "no password configured: login answers 503 with the setup hint",
        status == 503 and "no admin password set" in body,
        f"status={status} body={body.strip()}",
    )

    page.goto(base + "/", wait_until="domcontentloaded", timeout=30000)
    report.check(
        "signed out: the password box stays hidden until Sign in is clicked",
        not visible(page, "#password"),
    )
    report.check("signed out: the write toolbar is hidden", not visible(page, "#toolbar"))
    report.check("signed out: a Sign in control is offered", visible(page, "#login"))
    ok, text = wait_text(page, "#empty", "No boards yet", 6)
    report.check(
        "signed out: an empty install says so instead of showing a blank board",
        ok,
        f"#empty={text!r}",
    )

    # ---- the CLI sets the password; the running server must accept it -----
    setup = subprocess.run(
        [args.binary, "katban", "board", "password", ADMIN_PASSWORD],
        env=env,
        cwd=project,
        capture_output=True,
        text=True,
    )
    report.check(
        "katban board password sets the admin password",
        setup.returncode == 0,
        (setup.stdout + setup.stderr).strip()[-400:],
    )

    status, body, _ = http("POST", base + "/api/login", {"password": "wrong password"})
    report.check(
        "wrong password is rejected",
        status == 401 and "wrong password" in body,
        f"status={status} body={body.strip()}",
    )

    status, body, headers = http("POST", base + "/api/login", {"password": ADMIN_PASSWORD})
    lowered = headers.lower()
    report.check(
        "a running server picks up the new password without a restart",
        status == 200 and json.loads(body).get("ok") is True,
        f"status={status} body={body.strip()}",
    )
    report.check(
        "the session cookie is HttpOnly, SameSite=Strict and site-wide",
        "set-cookie" in lowered
        and "httponly" in lowered
        and "samesite=strict" in lowered
        and "path=/" in lowered,
        headers.strip()[-400:],
    )

    # ---- exposed board: reads are admin-gated once a proxy is in front ---
    # The loopback dev board leaves reads open (spec §20.7); the moment the
    # request arrives through caddy the read tier needs the admin session like
    # every write. The probe's browser always talks to 127.0.0.1 directly, so
    # the proxy's headers are replayed over HTTP here.
    proxy = {"x-forwarded-for": "203.0.113.7", "x-forwarded-proto": "https"}
    status, body, _ = http("GET", base + "/api/projects", headers=proxy)
    report.check(
        "a proxied read without a session is refused",
        status == 401 and "login required" in body,
        f"status={status} body={body.strip()}",
    )
    status, body, headers = http(
        "POST", base + "/api/login", {"password": ADMIN_PASSWORD}, headers=proxy
    )
    token = session_cookie(headers)
    report.check(
        "a proxied sign-in marks the session cookie Secure",
        status == 200 and token is not None and "; secure" in headers.lower(),
        headers.strip()[-400:],
    )
    status, body, _ = http(
        "GET",
        base + "/api/projects",
        headers={**proxy, "cookie": f"katban_admin={token}"},
    )
    try:
        projects = json.loads(body)
    except ValueError:
        projects = None
    report.check(
        "the same proxied read is allowed with that cookie",
        status == 200 and isinstance(projects, list),
        f"status={status} token={token!r} body={body.strip()[:120]}",
    )

    # ---- sign in through the rendered controls ---------------------------
    page.goto(base + "/", wait_until="domcontentloaded", timeout=30000)
    page.locator("#login").click()
    report.check("Sign in reveals the password field", visible(page, "#password"))
    page.locator("#password").fill(ADMIN_PASSWORD)
    page.locator("#login").click()
    ok, _ = wait_for(page, "!document.querySelector('#toolbar').hidden", 10)
    report.check(
        "signing in reveals the write toolbar",
        ok
        and visible(page, "#toolbar")
        and visible(page, "#logout")
        and visible(page, "#authed")
        and not visible(page, "#password")
        and not visible(page, "#login"),
        f"toolbar={visible(page, '#toolbar')} logout={visible(page, '#logout')} "
        f"authed_badge={visible(page, '#authed')} login={visible(page, '#login')} "
        f"password={visible(page, '#password')}",
    )
    ok, text = wait_text(page, "#notice", "Signed in", 6)
    report.check("sign-in is confirmed in the status banner", ok, f"#notice={text!r}")
    ok, text = wait_text(page, "#connection", "connected", 8)
    report.check("live updates connect after signing in", ok, f"#connection={text!r}")

    # ---- the session survives a reload (HttpOnly cookie + /api/me) -------
    page.reload(wait_until="domcontentloaded", timeout=30000)
    ok, _ = wait_for(page, "!document.querySelector('#toolbar').hidden", 10)
    report.check(
        "a reload restores the session from the cookie",
        ok and visible(page, "#logout"),
        f"toolbar={visible(page, '#toolbar')}",
    )

    # ---- create a board through the New board dialog ---------------------
    board_name = f"probe-{int(time.time())}"
    page.locator("#newboard").click()
    report.check("New board opens its own dialog", visible(page, "#new-board-dialog"))
    report.check(
        "the New board dialog has a named project field",
        visible(page, "#newboard-name")
        and js(page, "document.querySelector('#newboard-name').getAttribute('required') !== null"),
    )
    page.locator("#newboard-name").fill(board_name)
    page.locator("#new-board-dialog button[type=submit]").click()
    ok, _ = wait_for(
        page, f"document.querySelector('#project').value === {json.dumps(board_name)}", 10
    )
    selection = js(page, "document.querySelector('#project').value")
    report.check(
        "creating a board selects it in the project picker",
        ok and not visible(page, "#new-board-dialog"),
        f"selection={selection!r}",
    )
    ok, _ = wait_for(
        page,
        "Array.from(document.querySelectorAll('.toast'))"
        ".some(e => e.textContent.includes('Board created'))",
        8,
    )
    report.check(
        "creating a board raises a toast confirmation",
        ok,
        f"toasts={js(page, TOAST_TEXTS)!r}",
    )
    ok, text = wait_text(page, "#empty", "This board is empty", 8)
    report.check(
        "a new board explains that cards stay in backlog",
        ok,
        f"#empty={text!r}",
    )

    # ---- add two cards ---------------------------------------------------
    page.locator("#newprompt").fill("probe card one")
    page.locator("#addcard").click()
    ok, _ = wait_for(page, "document.querySelectorAll('.card').length === 1", 10)
    report.check("Add card renders the new card", ok, f"cards={prompts_of(page)!r}")
    time.sleep(1.2)  # distinct created/updated timestamps for the sort checks
    page.locator("#newprompt").fill("probe card two")
    page.locator("#addcard").click()
    ok, _ = wait_for(page, "document.querySelectorAll('.card').length === 2", 10)
    report.check(
        "a second card joins the same board",
        ok and prompts_of(page) == ["probe card one", "probe card two"],
        f"cards={prompts_of(page)!r}",
    )

    # ---- sort: creation order vs recently updated ------------------------
    page.locator("#sort").select_option("updated")
    ok, _ = wait_for(
        page,
        "JSON.stringify(Array.from(document.querySelectorAll('.card .prompt')).map(e => e.textContent))"
        ' === JSON.stringify(["probe card two","probe card one"])',
        6,
    )
    report.check(
        "sort by recently updated puts the newest card first",
        ok,
        f"order={prompts_of(page)!r}",
    )
    page.locator("#sort").select_option("created")
    ok, _ = wait_for(
        page,
        "JSON.stringify(Array.from(document.querySelectorAll('.card .prompt')).map(e => e.textContent))"
        ' === JSON.stringify(["probe card one","probe card two"])',
        6,
    )
    report.check("sort by creation order restores the original order", ok, f"order={prompts_of(page)!r}")

    # ---- edit a card prompt ---------------------------------------------
    page.locator(".card").first.locator("button[data-edit]").click()
    report.check("edit opens the editor dialog", visible(page, "#editor"))
    page.locator("#editor-text").fill("probe card one edited")
    page.locator("#editor button[type=submit]").click()
    ok, _ = wait_for(
        page,
        "Array.from(document.querySelectorAll('.card .prompt'))"
        ".some(e => e.textContent === 'probe card one edited')",
        10,
    )
    report.check("saving the editor updates the card prompt", ok, f"cards={prompts_of(page)!r}")

    # ---- search + hide-done filters -------------------------------------
    page.locator("#search").fill("nothing-matches-this")
    ok, text = wait_text(page, "#empty", "No tasks match these filters", 6)
    report.check("a search with no matches shows the empty state", ok, f"#empty={text!r}")
    page.locator("#search").fill("")
    ok, _ = wait_for(page, "document.querySelectorAll('.card').length === 2", 6)
    report.check("clearing the search restores the cards", ok, f"cards={prompts_of(page)!r}")

    done_column = (
        "Array.from(document.querySelectorAll('.col h2')).some(h => h.textContent.startsWith('done'))"
    )
    before_hide = js(page, done_column)
    page.locator("#hide-done").check()
    ok, _ = wait_for(page, "!(" + done_column + ")", 6)
    report.check("Hide done removes the done column", before_hide and ok, f"before={before_hide}")
    page.locator("#hide-done").uncheck()

    # ---- dependency picker + toast --------------------------------------
    page.locator(".card").first.locator("button[data-link]").click()
    report.check("link opens the dependency picker", visible(page, "#dependency-dialog"))
    options = js(
        page,
        "Array.from(document.querySelectorAll('#dependency-target option')).map(o => o.textContent)",
    )
    report.check(
        "the picker lists the other card as a prerequisite",
        len(options) >= 1,
        f"options={options!r}",
    )
    page.locator("#dependency-dialog button[type=submit]").click()
    ok, _ = wait_for(
        page,
        "Array.from(document.querySelectorAll('.toast'))"
        ".some(e => e.textContent.includes('Dependency added'))",
        8,
    )
    report.check(
        "adding a dependency raises a toast confirmation",
        ok,
        f"toasts={js(page, TOAST_TEXTS)!r}",
    )
    ok, _ = wait_for(
        page,
        "Array.from(document.querySelectorAll('.card'))"
        ".some(c => c.textContent.includes('waits for: probe card two'))",
        10,
    )
    report.check("the board shows what the card waits for", ok)

    # ---- refresh + runner indicator -------------------------------------
    with page.expect_response(lambda response: response.url.endswith("/api/projects")) as refreshed:
        page.locator("#refresh").click()
    report.check(
        "Refresh re-fetches the project list",
        refreshed.value.status == 200,
        f"status={refreshed.value.status}",
    )
    ok, text = wait_text(page, "#runner", "board only", 6)
    report.check(
        "the runner indicator reports that the scheduler is off",
        ok,
        f"#runner={text!r}",
    )

    # ---- sign out makes the board read-only again -----------------------
    page.locator("#logout").click()
    ok, _ = wait_for(page, "document.querySelector('#toolbar').hidden", 10)
    report.check(
        "signing out hides the write toolbar and restores Sign in",
        ok
        and not visible(page, "#toolbar")
        and not visible(page, "#logout")
        and visible(page, "#login"),
        f"toolbar={visible(page, '#toolbar')} logout={visible(page, '#logout')} "
        f"login={visible(page, '#login')}",
    )
    report.check(
        "signing out clears the session cookie",
        not [c for c in context.cookies() if c.get("name") == "katban_admin"],
        f"cookies={[c.get('name') for c in context.cookies()]}",
    )
    ok, _ = wait_for(page, "document.querySelectorAll('.card').length === 2", 10)
    report.check("the signed-out board is still readable", ok, f"cards={prompts_of(page)!r}")

    # ---- exposed board: what the page shows a signed-out visitor ---------
    # The 401 the exposed board answers reads with, replayed into the browser
    # so the gate the page renders (instead of a misleading "no boards") is
    # graded too. The route is a page-level intercept, so the server's own
    # loopback behaviour is untouched once it is removed.
    def refuse(route):
        route.fulfill(
            status=401, content_type="application/json", body='{"error":"login required"}'
        )

    page.route("**/api/projects", refuse)
    page.route("**/api/runner", refuse)
    refused.update(("/api/projects", "/api/runner"))
    page.reload(wait_until="domcontentloaded", timeout=30000)
    ok, text = wait_text(page, "#empty", "Sign in above to view projects", 10)
    report.check(
        "a refused read shows the sign-in gate, not an empty board",
        ok
        and not visible(page, "#board")
        and js(page, "document.querySelector('#project').disabled === true"),
        f"#empty={text!r} board_visible={visible(page, '#board')}",
    )
    ok, text = wait_text(page, "#runner", "Sign in to see runner status", 8)
    report.check("the runner indicator asks for a session", ok, f"#runner={text!r}")
    page.unroute("**/api/projects")
    page.unroute("**/api/runner")
    page.reload(wait_until="domcontentloaded", timeout=30000)
    ok, _ = wait_for(page, "document.querySelectorAll('.card').length === 2", 10)
    report.check(
        "the loopback board reads again once the proxy answer stops",
        ok,
        f"cards={prompts_of(page)!r}",
    )

    # ---- a write on a dropped session reports, it does not alert ----------
    # Every write funnels through one notice-based path; this drives its 401
    # arm the way an admin really hits it — signed in, then the cookie is gone
    # (server restart, session TTL, another tab signing out), and the page
    # still shows the toolbar because the session only lives in that cookie.
    page.locator("#login").click()
    page.locator("#password").fill(ADMIN_PASSWORD)
    page.locator("#login").click()
    ok, _ = wait_for(page, "!document.querySelector('#toolbar').hidden", 10)
    report.check("signing back in restores the toolbar", ok)
    context.clear_cookies()
    draft = "probe write without a session"
    # The write route's own 401 is expected from here on: the probe just
    # dropped the session, and the browser logging that refusal is the app
    # behaving correctly. Only this route on this board is exempted.
    refused.add(f"/api/board/{board_name}/cards")
    page.locator("#newprompt").fill(draft)
    page.locator("#addcard").click()
    ok, text = wait_text(page, "#notice", "Could not add the task", 10)
    report.check(
        "a refused write reports in the notice area and drops the session",
        ok and not visible(page, "#toolbar") and not visible(page, "#logout"),
        f"#notice={text!r} toolbar={visible(page, '#toolbar')}",
    )
    remaining = js(page, "document.querySelector('#newprompt').value")
    report.check(
        "the refused write leaves the draft in the box",
        remaining == draft,
        f"#newprompt={remaining!r}",
    )


def main():
    parser = argparse.ArgumentParser(description="Katban board UX probe")
    parser.add_argument("--binary", default="src-rust/target/debug/clawde")
    parser.add_argument("--chromium", default=None, help="chrome/chromium to drive")
    parser.add_argument("--timeout-secs", type=int, default=120)
    parser.add_argument("--keep", action="store_true", help="keep the scratch dir")
    parser.add_argument(
        "--require-browser",
        action="store_true",
        help="fail (1) instead of skipping (2) when playwright/chromium is missing",
    )
    args = parser.parse_args()

    # Both subprocesses run with cwd set to the scratch project dir, so a
    # repo-relative default has to be resolved against this process's cwd.
    args.binary = os.path.abspath(args.binary)
    if not os.path.exists(args.binary):
        print(
            f"katban-board-ux-probe: binary not found: {args.binary} (build first: cargo build)"
        )
        return EXIT_FAIL
    try:
        from playwright.sync_api import sync_playwright
    except Exception as error:  # noqa: BLE001
        reason = (
            f"playwright is not installed — {error}\n"
            "  pip install playwright && playwright install chromium"
        )
        if args.require_browser:
            print(f"katban-board-ux-probe: FAIL — {reason}")
            return EXIT_FAIL
        print(f"katban-board-ux-probe: SKIPPED — {reason}")
        return EXIT_SKIP

    refused = set()

    scratch = tempfile.mkdtemp(prefix="katban-ux-probe-")
    home = os.path.join(scratch, "home")
    project = os.path.join(scratch, "project")
    log_path = os.path.join(scratch, "server.log")
    os.makedirs(os.path.join(project, ".git"))

    env = os.environ.copy()
    env["CLAWDE_HOME"] = home
    env["NO_COLOR"] = "1"
    env["CI"] = "1"

    port = free_port()
    base = f"http://127.0.0.1:{port}"
    server = None
    report = Report()
    try:
        try:
            server = start_board(args.binary, env, project, port, log_path, args.timeout_secs)
        except RuntimeError as error:
            print(f"FAIL: the board server did not come up\n  {error}")
            return EXIT_FAIL

        with sync_playwright() as play:
            browser = launch_browser(play, args.chromium)
            context = browser.new_context(viewport={"width": 1200, "height": 950}, locale="en-US")
            page = context.new_page()
            dialogs_seen = []
            page.on(
                "dialog",
                lambda dialog: (dialogs_seen.append(dialog.type), dialog.accept())[1],
            )
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

            run_checks(args, report, page, context, base, env, project, log_path, refused)

            bad = unexpected_console_errors(console_errors, refused)
            report.check("no console or page errors on the verified routes", not bad, "\n".join(bad))
            report.check(
                "no confirm dialog was triggered unexpectedly", not dialogs_seen, f"dialogs={dialogs_seen}"
            )
            browser.close()
    except Skip as skip:
        if args.require_browser:
            print(f"katban-board-ux-probe: FAIL — {skip}")
            return EXIT_FAIL
        print(f"katban-board-ux-probe: SKIPPED — {skip}")
        return EXIT_SKIP
    except Exception as error:  # noqa: BLE001
        print(f"FAIL: the probe aborted — {error}")
        print(tail(log_path, 30))
        return EXIT_FAIL
    finally:
        if server is not None:
            server.kill()
            try:
                server.wait(timeout=10)
            except Exception:  # noqa: BLE001
                pass
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
