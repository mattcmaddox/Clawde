#!/usr/bin/env python3
"""Smoke-test the TUI permission status and settings surfaces.

The probe starts Clawde in a temporary HOME/CLAWDE_HOME, dismisses any startup
overlay, runs `/permissions`, then opens `/settings`. It does not connect to a
provider, edit project files, or write the user's real configuration.

Usage:
  python3 scripts/probes/tui-permissions-smoke.py \
      [--binary src-rust/target/debug/clawde] [--timeout-secs 15]
"""

import argparse
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import time


SESSION_PREFIX = "clawde-tui-permissions"


def tmux(args, check=True):
    return subprocess.run(
        ["tmux", *args],
        check=check,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )


def capture(session):
    result = tmux(["capture-pane", "-t", session, "-p"], check=False)
    return result.stdout if result.returncode == 0 else ""


def wait_for(session, text, timeout):
    deadline = time.monotonic() + timeout
    latest = ""
    while time.monotonic() < deadline:
        latest = capture(session)
        if text in latest:
            return latest
        time.sleep(0.25)
    return latest


def send(session, *keys):
    tmux(["send-keys", "-t", session, *keys])


def check(name, condition, detail=""):
    status = "PASS" if condition else "FAIL"
    suffix = f" — {detail}" if detail and not condition else ""
    print(f"[{status}] {name}{suffix}", flush=True)
    return condition


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--binary",
        default="src-rust/target/debug/clawde",
        help="path to the built clawde binary",
    )
    parser.add_argument("--timeout-secs", type=float, default=15.0)
    args = parser.parse_args()

    if sys.platform != "linux":
        print("tui-permissions-smoke: SKIPPED — Linux/tmux-only")
        return 2
    if shutil.which("tmux") is None:
        print("tui-permissions-smoke: SKIPPED — tmux is not installed")
        return 2

    binary = Path(args.binary).resolve()
    if not binary.exists():
        print(f"tui-permissions-smoke: binary not found: {binary} (build first)")
        return 1

    session = f"{SESSION_PREFIX}-{os.getpid()}"
    passed = True
    with tempfile.TemporaryDirectory(prefix="clawde-tui-smoke-") as home:
        command = "env HOME={home} CLAWDE_HOME={home} {binary}".format(
            home=shlex.quote(home),
            binary=shlex.quote(str(binary)),
        )
        try:
            tmux([
                "new-session",
                "-d",
                "-s",
                session,
                "-x",
                "100",
                "-y",
                "30",
                command,
            ])

            startup = wait_for(session, "Clawde", args.timeout_secs)
            passed &= check(
                "TUI starts and renders its header",
                "Clawde" in startup,
                "startup header was not rendered",
            )
            if not passed:
                return 1

            # Startup may leave the help/onboarding overlay active. Escape is
            # harmless when no modal is shown and makes this deterministic.
            send(session, "Escape")
            # Let the modal-close event repaint before entering a slash command;
            # tmux can otherwise deliver the text during the transition frame.
            time.sleep(0.4)
            send(session, "/permissions")
            send(session, "C-m")
            permissions = wait_for(session, "Permission Settings", args.timeout_secs)
            passed &= check(
                "permissions command renders effective mode",
                "Permission Settings" in permissions and "Mode:" in permissions,
            )
            passed &= check(
                "permissions command renders allow/deny state",
                "Allowed tools:" in permissions and "Denied tools:" in permissions,
            )

            send(session, "Escape")
            time.sleep(0.4)
            send(session, "/settings")
            send(session, "C-m")
            settings = wait_for(session, "Allowed tools", args.timeout_secs)
            passed &= check(
                "settings screen exposes allowed tools",
                "Allowed tools" in settings,
            )
            passed &= check(
                "settings screen exposes denied tools",
                "Denied tools" in settings,
            )
        finally:
            tmux(["kill-session", "-t", session], check=False)

    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
