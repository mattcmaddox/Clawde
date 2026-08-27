#!/usr/bin/env bash
# gateway-sdk-smoke.sh — End-to-end SDK smoke test against a live gateway.
#
# Builds clawde-gateway if stale, starts it on a loopback port, waits for
# /healthz, runs scripts/gateway-sdk-smoke.py, and tears the gateway down.
#
# Usage:
#   ./scripts/gateway-sdk-smoke.sh
#
# Env overrides:
#   GATEWAY_PORT    listen port (default 8899)
#   GATEWAY_KEY     bearer key (default: random per run)
#   GATEWAY_PYTHON  python interpreter with openai + openai-agents installed
#                   (default: python3)
#   SKIP_BUILD=1    use the existing debug binary without rebuilding
#
# Requires python3 with `openai` and `openai-agents` installed, and at
# least one provider credential for the free cascade (e.g. GROQ_API_KEY) —
# either in the environment or stored in ~/.clawde/auth.json.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

BINARY="$REPO_ROOT/src-rust/target/debug/clawde-gateway"
SRC_DIR="$REPO_ROOT/src-rust"

PORT="${GATEWAY_PORT:-8899}"
KEY="${GATEWAY_KEY:-$(openssl rand -hex 16 2>/dev/null || echo "smoke-key")}"
PYTHON="${GATEWAY_PYTHON:-python3}"
LOG="$(mktemp /tmp/gateway-sdk-smoke.XXXXXX.log)"
PID=""

cleanup() {
    if [ -n "$PID" ] && kill -0 "$PID" 2>/dev/null; then
        kill "$PID" 2>/dev/null || true
        wait "$PID" 2>/dev/null || true
    fi
    rm -f "$LOG"
}
trap cleanup EXIT

# ---- 0. Provider credential sanity check (informational) ----
if [ -z "${GROQ_API_KEY:-}" ] && [ -z "${HUGGINGFACE_API_KEY:-}" ] \
    && [ -z "${NVIDIA_NIM_API_KEY:-}" ] && [ -z "${CEREBRAS_API_KEY:-}" ] \
    && [ -z "${GEMINI_API_KEY:-}" ] && [ -z "${OPENAI_API_KEY:-}" ]; then
    echo ":: warning: no provider API key in the environment; falling back to" >&2
    echo "   stored keys in ~/.clawde/auth.json (may still work)." >&2
fi

# ---- 1. Build if stale ----
if [ "${SKIP_BUILD:-0}" != "1" ]; then
    needs_build=0
    if [ ! -f "$BINARY" ]; then
        needs_build=1
    else
        # `|| true`: head -1 closes the pipe early, xargs gets SIGPIPE, and
        # pipefail would turn that into a shell-exiting failure.
        newest_src="$(find "$SRC_DIR/crates" "$SRC_DIR/Cargo.toml" "$SRC_DIR/Cargo.lock" \
            -type f \( -name '*.rs' -o -name '*.toml' \) \
            -print0 2>/dev/null | xargs -0 ls -t 2>/dev/null | head -1 || true)"
        if [ -n "$newest_src" ] && [ "$newest_src" -nt "$BINARY" ]; then
            needs_build=1
        fi
    fi
    if [ "$needs_build" = "1" ]; then
        echo ":: building clawde-gateway ..." >&2
        (cd "$SRC_DIR" && cargo build --package clawde-gateway) || {
            echo "build failed - aborting" >&2
            exit 1
        }
    fi
fi

# ---- 2. Start the gateway ----
echo ":: starting gateway on 127.0.0.1:$PORT ..." >&2
setsid nohup "$BINARY" --port "$PORT" --key "$KEY" >"$LOG" 2>&1 &
PID=$!

# Startup includes model discovery and can take several seconds.
for _ in $(seq 1 60); do
    if curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; then
        break
    fi
    if ! kill -0 "$PID" 2>/dev/null; then
        echo ":: gateway exited during startup:" >&2
        tail -20 "$LOG" >&2 || true
        exit 1
    fi
    sleep 1
done
if ! curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; then
    echo ":: gateway did not become healthy within 60s; last log lines:" >&2
    tail -20 "$LOG" >&2 || true
    exit 1
fi

# ---- 3. Run the SDK smoke tests ----
echo ":: running SDK smoke tests ..." >&2
# OPENAI_AGENTS_DISABLE_TRACING: the agents SDK otherwise prints a
# "OPENAI_API_KEY is not set, skipping trace export" notice to stdout.
GATEWAY_URL="http://127.0.0.1:$PORT/v1" GATEWAY_KEY="$KEY" \
    OPENAI_AGENTS_DISABLE_TRACING=1 \
    "$PYTHON" "$SCRIPT_DIR/gateway-sdk-smoke.py"
