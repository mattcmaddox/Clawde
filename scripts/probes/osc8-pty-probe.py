#!/usr/bin/env python3
"""Drive the real clawde binary through a pty and assert the live output
contains OSC 8 hyperlink escapes for a URL rendered on screen.

This is the closest automated stand-in for "clickable links in a real
terminal": it answers the binary's terminal capability queries (DA1 and the
kitty-keyboard probe) so it actually reaches the draw loop, types a URL into
the prompt, and greps the raw byte stream for the OSC 8 envelope
(`ESC ] 8 ; ; URL ESC \\`). It does not (and cannot) assert that a specific
emulator makes the link clickable — that is a terminal feature.

Usage:
    python3 scripts/probes/osc8-pty-probe.py [path-to-clawde-binary]
Exit code 0 on success, 1 on failure.
"""

import os
import pty
import select
import struct
import subprocess
import sys
import termios
import time
import fcntl

OSC8_OPEN = b"\x1b]8;;"


def main() -> int:
    binary = (
        sys.argv[1]
        if len(sys.argv) > 1
        else os.path.join(
            os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))),
            "src-rust",
            "target",
            "debug",
            "clawde",
        )
    )
    if not os.path.exists(binary):
        print(f"binary not found: {binary}", file=sys.stderr)
        return 1

    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))

    env = dict(os.environ)
    env["TERM"] = "xterm-256color"
    env.pop("CLAWDE_NO_HYPERLINKS", None)

    proc = subprocess.Popen(
        [binary],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env=env,
        preexec_fn=os.setsid,
        close_fds=True,
    )
    os.close(slave)

    buf = bytearray()
    url = b"https://example.com/path"
    start = time.time()
    typed = False
    try:
        while time.time() - start < 20:
            ready, _, _ = select.select([master], [], [], 0.2)
            if master in ready:
                try:
                    data = os.read(master, 65536)
                except OSError:
                    break
                if not data:
                    break
                buf += data
                # Answer terminal capability queries so the binary proceeds
                # past its startup probes into the draw loop.
                if b"\x1b[c" in data:
                    os.write(master, b"\x1b[?62;c")
                if b"\x1b[?u" in data:
                    os.write(master, b"\x1b[?0u")
            # Give it time to settle, then type the URL into the prompt.
            if not typed and time.time() - start > 6:
                os.write(master, url)
                typed = True
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            proc.kill()

    out = bytes(buf)
    closes = out.count(b"\x1b]8;;\x1b\\")
    # The close sequence also starts with the open prefix, so subtract it to
    # count real opens (an open carries the URL: `ESC ] 8 ; ; URL ESC \`).
    opens = out.count(OSC8_OPEN) - closes
    has_url = url in out
    print(f"captured bytes: {len(out)}")
    print(f"OSC8 open sequences: {opens}")
    print(f"OSC8 close sequences: {closes}")
    print(f"URL appears in stream: {has_url}")

    if opens > 0 and opens == closes and has_url:
        # Show one envelope for eyeballing.
        idx = out.find(OSC8_OPEN)
        print("sample:", out[idx : idx + len(OSC8_OPEN) + 40])
        return 0
    print("FAIL: no OSC 8 envelope emitted", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
