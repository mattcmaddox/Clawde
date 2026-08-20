#!/usr/bin/env python3
"""TUI-tier probe: assert streaming renders, the attribution badge, and the
key-ring footer in the real ratatui frontend.

The content tier (run_eval.py) proves answer quality headlessly; this probe
proves what only the TUI can show: the streaming spinner appears while the
free-provider chain works, the transcript grows, the per-turn attribution
badge (`⤷ upstream`) renders after completion, and the key-ring status footer
(`free:N/M` / `ollama:auto`) is on screen.

Drives the TUI in a throwaway 80x24 tmux session against an isolated
CLAWDE_HOME seeded with a copy of the real auth store — real key-ring state is
never touched. Runs the real binary; build it first with `cargo build`.

Exit codes: 0 = pass, 1 = assertion failed, 2 = could not run (no tmux /
non-Linux / missing binary).
"""

import argparse
import json
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
SRC_RUST = REPO_ROOT / "src-rust"
DEFAULT_BINARY = SRC_RUST / "target" / "debug" / "clawde"
DEFAULT_AUTH = Path(os.environ.get("HOME", "~")) / ".clawde" / "auth.json"
SESSION_PREFIX = "clawde-tui-probe"
DEFAULT_PROMPT = "Reply with exactly: PROBE-OK"
EXPECTED_TEXT = "PROBE-OK"


def tmux(*args: str, timeout: float = 10) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["tmux", *args],
        capture_output=True,
        text=True,
        timeout=timeout,
    )


def capture(session: str) -> str:
    return tmux("capture-pane", "-t", session, "-p").stdout


def wait_for(pred, what: str, session: str, timeout: float, interval: float = 0.1) -> bool:
    deadline = time.monotonic() + max(0.0, timeout)
    while time.monotonic() < deadline:
        if pred(capture(session)):
            return True
        time.sleep(interval)
    return False


def seed_home(home: Path, auth_file: Path) -> None:
    home.mkdir(parents=True, exist_ok=True)
    auth = {}
    if auth_file.exists():
        try:
            loaded = json.loads(auth_file.read_text())
            if isinstance(loaded, dict):
                auth = loaded
        except (json.JSONDecodeError, OSError):
            pass
    (home / "auth.json").write_text(json.dumps(auth, indent=2))
    (home / "settings.json").write_text(
        json.dumps({"auto_compact": False, "hasCompletedOnboarding": True, "hooks": {}}, indent=2)
    )
    for sub in ("sessions", "key-ring-state", "free-state", "projects"):
        (home / sub).mkdir(parents=True, exist_ok=True)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--binary", type=Path, default=DEFAULT_BINARY)
    ap.add_argument("--auth-file", type=Path, default=DEFAULT_AUTH)
    ap.add_argument("--timeout", type=float, default=240, help="Total budget seconds")
    ap.add_argument("--prompt", default=DEFAULT_PROMPT, help="Prompt sent to the TUI")
    ap.add_argument("--expected-text", default=EXPECTED_TEXT, help="Text required in the completed transcript")
    ap.add_argument("--keep-home", action="store_true")
    args = ap.parse_args()

    if sys.platform.startswith("win") or shutil.which("tmux") is None:
        print("tui-probe: skipped (requires tmux on a POSIX host)")
        return 2
    if not args.binary.exists():
        print("tui-probe: skipped — debug binary not built (run 'cargo build' in src-rust/)")
        return 2

    home = Path(tempfile.mkdtemp(prefix="clawde-tui-probe-"))
    session = f"{SESSION_PREFIX}-{os.getpid()}-{uuid.uuid4().hex[:6]}"
    started = time.monotonic()
    try:
        seed_home(home, args.auth_file)
        tmux("new-session", "-d", "-s", session, "-x", "80", "-y", "24")

        env_line = (
            f"cd {shlex.quote(str(SRC_RUST))} && "
            f"CLAWDE_HOME={shlex.quote(str(home))} exec {shlex.quote(str(args.binary.resolve()))}"
        )
        tmux("send-keys", "-t", session, env_line, "C-m")

        if not wait_for(
            lambda c: "Clawde v" in c or "🐾" in c or "Welcome" in c,
            "TUI welcome screen",
            session,
            20,
        ):
            print("tui-probe: FAIL — TUI did not render the welcome screen")
            print(capture(session)[-1200:])
            return 1

        # Ctrl-M is the submit key for the multiline prompt. Plain Enter is
        # intentionally a newline in the current keybinding behavior.
        tmux("send-keys", "-t", session, args.prompt)
        time.sleep(0.2)
        tmux("send-keys", "-t", session, "C-m")

        if not wait_for(
            lambda c: "Accomplishing" in c,
            "streaming spinner (Accomplishing)",
            session,
            args.timeout - (time.monotonic() - started),
        ):
            print("tui-probe: FAIL — no streaming spinner appeared")
            print(capture(session)[-1200:])
            return 1

        # Key-ring footer: the free-provider status chrome (`free:N/M`,
        # `ollama:auto`, or the `· free ·` provider badge).
        footer_seen = wait_for(
            lambda c: ("free:" in c and "/" in c) or "ollama:auto" in c or "· free ·" in c,
            "key-ring footer",
            session,
            15,
        )

        # Wait for completion: spinner gone and the prompt line is back.
        completed = wait_for(
            lambda c: "Accomplishing" not in c and "❯" in c and args.expected_text in c,
            "turn completion",
            session,
            max(10.0, args.timeout - (time.monotonic() - started)),
        )
        final = capture(session)

        checks = []
        checks.append(("transcript text", args.expected_text in final))
        checks.append(("attribution badge (⤷)", "⤷" in final))
        checks.append(("key-ring footer", footer_seen))
        checks.append(("completed", completed))
        checks.append(("no error banner", "⚠ Error" not in final))

        ok = all(passed for _, passed in checks)
        for name, passed in checks:
            print(f"  [{'PASS' if passed else 'FAIL'}] {name}")
        if not ok:
            print("tui-probe: FAIL — transcript tail:")
            print(final[-1600:])
        else:
            print(f"tui-probe: PASS in {time.monotonic() - started:.0f}s")
        return 0 if ok else 1
    finally:
        tmux("kill-session", "-t", session)
        if not args.keep_home:
            shutil.rmtree(home, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
