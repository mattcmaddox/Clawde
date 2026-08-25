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
      [--warmup-secs 30] [--max-cpu-secs 2]

The warmup phase adaptively waits for the one-time startup cost (welcome
render, health-key probe sweep, model discovery) to finish — sampling CPU
once per second until the process goes genuinely idle — so the measured
window reflects steady-state idle only. On a healthy network the sweep
finishes in a couple of seconds and the warmup returns early; on a slow or
unreachable network it may take the full warmup budget.
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


def wait_for_quiescence(pid, max_wait, quiet=False):
    """Wait for the child to stop burning CPU.

    Startup sweeps (health-key probes, model discovery) can run well past a
    fixed warmup on a slow/unreachable network, and the probe must not measure
    that one-time work.  We sample CPU once per second and require three
    consecutive samples each burning under 0.05s (i.e. the process is
    genuinely idle — the tail of a staggered probe sweep burns ~0.08s/s,
    which stays above the 0.05s bar, while a healthy idle session at the
    250ms repaint throttle burns well under 0.05s/s).  Returns the elapsed
    settle time, or None if the process never settled within `max_wait`.
    """
    start = time.monotonic()
    deadline = start + max_wait
    prev = cpu_seconds(pid)
    quiet_streak = 0
    while time.monotonic() < deadline:
        time.sleep(1.0)
        cur = cpu_seconds(pid)
        if cur is None:
            return None
        if prev is not None and (cur - prev) < 0.05:
            quiet_streak += 1
            if quiet_streak >= 3:
                elapsed = time.monotonic() - start
                if not quiet:
                    print("idle-cpu-probe: settled after %.1fs" % elapsed)
                return elapsed
        else:
            quiet_streak = 0
        prev = cur
    return None


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
        default=30,
        help="max seconds to wait for the startup sweep to finish before "
        "sampling begins (returns early once the process goes idle)",
    )
    ap.add_argument(
        "--max-cpu-secs",
        type=float,
        default=2.0,
        help="max CPU seconds allowed over the idle window",
    )
    ap.add_argument(
        "--quiet",
        action="store_true",
        help="suppress the settle-status line (for pre-commit hook output)",
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
        settle = wait_for_quiescence(pid, args.warmup_secs, args.quiet)
        if settle is None:
            print(
                "idle-cpu-probe: FAIL — CPU never settled within %ds warmup; "
                "the binary is doing sustained background work (startup "
                "health probes / model discovery on a slow network?)"
                % args.warmup_secs
            )
            return 1
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
