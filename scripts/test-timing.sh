#!/usr/bin/env bash
# test-timing.sh — Measure per-crate lib test wall time on a warm build.
#
# Cold wall-clock for `cargo test --workspace` is dominated by compilation,
# not test execution, so a baseline must be taken on a build that is already
# compiled. This script warms each crate (cargo test --no-run) once, then
# times the real test run, and prints a machine-readable comparison line.
#
# Usage:
#   ./scripts/test-timing.sh                     # all workspace crates
#   ./scripts/test-timing.sh clawde-commands     # one crate
#   ./scripts/test-timing.sh clawde-core clawde-api
#
# Baseline to record (2026-08-06, cold first-run included compile):
#   core 15.7s · api 16.1s · query 19.3s · commands 30.2s · tools 11.3s ·
#   tui 47.1s · mcp 6.0s   — commands test EXECUTION alone: ~1.1s (163 tests)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SRC_DIR="$(cd "$SCRIPT_DIR/.." && pwd)/src-rust"

cd "$SRC_DIR"

if [ "$#" -eq 0 ]; then
    CRATES=(clawde-core clawde-api clawde-query clawde-commands clawde-tools clawde-tui clawde-mcp)
else
    CRATES=("$@")
fi

# Nanosecond timing needs GNU date (%N). macOS/BSD date lacks it (echoes the
# literal format or errors), so fall back to whole seconds — still fine for a
# comparative baseline. The digit check is robust: only accept the nanosecond
# branch when the probe output is exactly 13 digits.
if date +%s%N 2>/dev/null | grep -qE '^[0-9]{13}$'; then
    now_ms() { date +%s%N | head -c 13; }
else
    now_ms() { date +%s; }
fi

declare -a RESULTS
FAILED=0

for pkg in "${CRATES[@]}"; do
    # Warm build: compile the test binary once so the timed run measures
    # execution, not compilation.
    if ! cargo test --package "$pkg" --lib --no-run >/dev/null 2>&1; then
        echo "WARN: could not build $pkg — skipping" >&2
        continue
    fi

    start=$(now_ms)
    cargo test --package "$pkg" --lib --quiet >/dev/null 2>&1
    status=$?
    end=$(now_ms)

    # now_ms returns milliseconds when %N works, else seconds. Normalise.
    if [ "${#start}" -eq 13 ]; then
        elapsed_ms=$((end - start))
    else
        elapsed_ms=$(( (end - start) * 1000 ))
    fi

    if [ "$status" -ne 0 ]; then
        RESULTS+=("$pkg: ${elapsed_ms}ms  (FAILED — exit $status)")
        FAILED=1
    else
        RESULTS+=("$pkg: ${elapsed_ms}ms")
    fi
done

echo ""
echo "== test-timing summary (warm build) =="
for line in "${RESULTS[@]}"; do
    echo "  $line"
done

exit "$FAILED"
