#!/usr/bin/env bash
# Orchestrates the ACP-over-TCP session MCP E2E validation:
#   MCP server -> clawde acp --listen -> acp_driver.py -> assertions
#
# Usage: scripts/probes/acp-session-mcp/run_e2e.sh
#   Requires a built clawde binary (cargo build -p clawde-cli) and a
#   provider key reachable from the ambient environment (GROQ_API_KEY or
#   the key ring) so the session/prompt model call can run.
#
# Checks: initialize advertises mcp http+sse; session/new with a localhost
# streamable-HTTP MCP server succeeds; the model calls the session MCP echo
# tool (verified via the MCP server's tool-call log); private / link-local
# URLs are rejected with an SSRF reason; loopback is allowed. The driver
# passes as soon as the tool call is observed and lets the run tear down,
# so it does not depend on free-model latency.
set -u

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT="${ACP_E2E_ROOT:-/tmp/acp-e2e}"
CLAWDE="${CLAWDE_BIN:-$SCRIPT_DIR/../../../src-rust/target/debug/clawde}"
MCP_PORT="${MCP_PORT:-19531}"
ACP_PORT="${ACP_PORT:-19532}"
CLIENT_HOME="$ROOT/clawde-home"
WORKDIR="$ROOT/workdir"
LOG="$ROOT/tool_calls.log"

mkdir -p "$CLIENT_HOME" "$WORKDIR" "$ROOT"
rm -f "$LOG" "$ROOT/acp.log" "$ROOT/mcp.log"

mkdir -p "$CLIENT_HOME" "$WORKDIR"
rm -f "$LOG"

cleanup() {
  [ -n "${ACP_PID:-}" ] && kill "$ACP_PID" 2>/dev/null
  [ -n "${MCP_PID:-}" ] && kill "$MCP_PID" 2>/dev/null
  wait 2>/dev/null
}
trap cleanup EXIT

# 1. Start the local streamable-HTTP MCP server
python3 "$SCRIPT_DIR/mcp_server.py" --port "$MCP_PORT" --log "$LOG" >"$ROOT/mcp.log" 2>&1 &
MCP_PID=$!
for _ in $(seq 1 50); do
  if (exec 3<>"/dev/tcp/127.0.0.1/$MCP_PORT") 2>/dev/null; then
    exec 3>&- 3<&-
    break
  fi
  kill -0 "$MCP_PID" 2>/dev/null || { echo "MCP server exited early"; cat "$ROOT/mcp.log"; exit 1; }
  sleep 0.2
done
echo "MCP server up on $MCP_PORT (pid $MCP_PID)"

# 2. Start clawde ACP over TCP with an isolated home
RUST_BACKTRACE=full CLAWDE_HOME="$CLIENT_HOME" "$CLAWDE" acp --listen "127.0.0.1:$ACP_PORT" >"$ROOT/acp.log" 2>&1 &
ACP_PID=$!
for _ in $(seq 1 100); do
  if (exec 3<>"/dev/tcp/127.0.0.1/$ACP_PORT") 2>/dev/null; then
    exec 3>&- 3<&-
    break
  fi
  kill -0 "$ACP_PID" 2>/dev/null || { echo "ACP server exited early"; cat "$ROOT/acp.log"; exit 1; }
  sleep 0.2
done
echo "ACP server up on $ACP_PORT (pid $ACP_PID)"

# 3. Drive the protocol
python3 "$SCRIPT_DIR/acp_driver.py" \
  --acp-port "$ACP_PORT" \
  --mcp-url "http://127.0.0.1:$MCP_PORT/mcp" \
  --workdir "$WORKDIR" \
  --log "$LOG"
DRIVER_RC=$?

echo "--- ACP server log tail ---"
tail -15 "$ROOT/acp.log"
exit $DRIVER_RC
