#!/usr/bin/env python3
"""Idle-CPU probe for the Clawde TUI.

Spawns the clawde binary in a pseudo-terminal, lets it sit idle, and fails if
it burns more CPU time than a healthy idle session should.

Why: an idle Clawde must NOT repaint at full rate. Since the idle-repaint
throttle (`App::needs_fast_repaint()` in crates/tui/src/app.rs + the poll
timeout in crates/cli/src/main.rs), an idle session burns well under ~0.5s of
CPU per 15s window. A regressed always-60fps repaint loop burns ~4s+ in the
same window, so the default bound is deliberately generous to stay
flake-resistant on loaded CI runners.

Usage:
  python3 scripts/probes/idle-cpu-probe.py \
      [--binary src-rust/target/debug/clawde] [--idle-secs 15] \
      [--warmup-secs 10] [--max-cpu-secs 2]

A warm-up phase skips the one-time startup cost (welcome render, models-cache
check, health probe) so the measured window reflects steady-state idle only.
Run manually as a health check (build first: cargo build), or wire into CI as a
Linux-only step.
"""

import argparse
import os
import signal
import sys
import time

if sys.platform != "linux":
    # Exit 2 (not 0) so a CI wiring that treats non-zero as failure never
    # gets a silent false pass on macOS/Windows.
    print("idle-cpu-probe: SKIPPED — Linux-only (reads /proc/<pid>/stat).")
    sys.exit(2)

import pty  # noqa: E402  (import guard for the platform check above)


def cpu_seconds(pid):
    """Sum utime+stime (fields 14/15) from /proc/<pid>/stat, in seconds."""
    try:
        with open("/proc/%d/stat" % pid, "r") as f:
            data = f.read()
        # The comm field may contain ')' (e.g. "(name)"), so split on the LAST one.
        body = data.rsplit(")", 1)[1].split()
        utime = int(body[11])  # field 14, zero-indexed within the post-comm list
        stime = int(body[12])  # field 15
    except (OSError, IndexError, ValueError):
        return None
    hz = float(os.sysconf("SC_CLK_TCK") or 100)
    return (utime + stime) / hz


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--binary",
        default="src-rust/target/debug/clawde",
        help="path to the clawde binary",
    )
    ap.add_argument("--idle-secs", type=int, default=15)
    ap.add_argument(
        "--warmup-secs",
        type=int,
        default=10,
        help="seconds to let startup settle before sampling begins",
    )
    ap.add_argument(
        "--max-cpu-secs",
        type=float,
        default=2.0,
        help="max CPU seconds allowed over the idle window",
    )
    args = ap.parse_args()

    if not os.path.exists(args.binary):
        print("idle-cpu-probe: binary not found: %s (build first: cargo build)" % args.binary)
        return 1

    pid, fd = pty.fork()
    if pid == 0:  # child
        os.execv(args.binary, [args.binary])
        os._exit(127)

    try:
        time.sleep(args.warmup_secs)  # skip one-time startup cost
        start = cpu_seconds(pid)
        time.sleep(args.idle_secs)
        end = cpu_seconds(pid)
    finally:
        os.close(fd)
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        try:
            os.waitpid(pid, 0)
        except ChildProcessError:
            pass

    if start is None or end is None:
        print("idle-cpu-probe: could not read /proc/%d/stat (process exited early?)" % pid)
        return 1

    delta = end - start
    ok = delta <= args.max_cpu_secs
    print(
        "idle-cpu-probe: %ds window (after %ds warmup), %.2fs CPU (limit %.2fs) -> %s"
        % (
            args.idle_secs,
            args.warmup_secs,
            delta,
            args.max_cpu_secs,
            "OK" if ok else "FAIL",
        )
    )
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
