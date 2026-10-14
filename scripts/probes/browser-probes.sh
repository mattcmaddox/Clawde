#!/usr/bin/env bash
# Set up and run the two browser probes.
#
# Why this exists: both probes need the `playwright` Python package, and on a
# distro whose python is externally managed (PEP 668) a bare
# `pip install playwright` is refused. They get their own venv, outside the
# repo, created once and reused. A system chromium is preferred over
# playwright's own download.
#
#     scripts/probes/browser-probes.sh setup     # create/refresh the venv
#     scripts/probes/browser-probes.sh board     # Katban board journey
#     scripts/probes/browser-probes.sh catchat   # Cat Chat guest journey
#     scripts/probes/browser-probes.sh all       # both (default)
#
# Override the venv location with CLAWDE_PROBE_VENV. Each probe exits 0 pass,
# 1 fail, 2 skip-with-a-missing-prerequisite.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
VENV="${CLAWDE_PROBE_VENV:-${XDG_DATA_HOME:-$HOME/.local/share}/clawde/probes-venv}"
REQS="$ROOT/scripts/probes/requirements.txt"
BINARY="$ROOT/src-rust/target/debug/clawde"

# A chromium from the distro is cheaper than playwright's download and is the
# same browser the probe would otherwise fetch.
system_chromium() {
    local candidate
    for candidate in /usr/bin/chromium /usr/bin/chromium-browser /usr/bin/google-chrome; do
        if [ -x "$candidate" ]; then
            printf '%s' "$candidate"
            return 0
        fi
    done
    return 1
}

setup() {
    if [ ! -x "$VENV/bin/python" ]; then
        echo "browser-probes: creating venv at $VENV"
        python3 -m venv "$VENV"
    fi
    "$VENV/bin/python" -m pip install --quiet --upgrade pip
    "$VENV/bin/python" -m pip install --quiet -r "$REQS"
    if ! system_chromium >/dev/null; then
        echo "browser-probes: no system chromium — downloading playwright's build"
        "$VENV/bin/python" -m playwright install --with-deps chromium
    fi
    "$VENV/bin/python" -c "import playwright" # fail here, not mid-probe
    echo "browser-probes: playwright ready in $VENV"
}

run() {
    local script="$1" chromium
    [ -x "$VENV/bin/python" ] || setup
    if [ ! -x "$BINARY" ]; then
        echo "browser-probes: binary not found: $BINARY" >&2
        echo "  build it first: cd src-rust && cargo build" >&2
        return 2
    fi
    chromium="$(system_chromium || true)"
    echo "browser-probes: running $script${chromium:+ (chromium $chromium)}"
    if [ -n "$chromium" ]; then
        "$VENV/bin/python" "$ROOT/scripts/probes/$script" \
            --binary "$BINARY" --chromium "$chromium"
    else
        "$VENV/bin/python" "$ROOT/scripts/probes/$script" --binary "$BINARY"
    fi
}

case "${1:-all}" in
    setup) setup ;;
    board) run katban-board-ux-probe.py ;;
    catchat) run catchat-ux-probe.py ;;
    all)
        run katban-board-ux-probe.py
        run catchat-ux-probe.py
        ;;
    *)
        echo "usage: scripts/probes/browser-probes.sh [setup|board|catchat|all]" >&2
        exit 64
        ;;
esac
