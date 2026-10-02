#!/usr/bin/env bash
# End-to-end proof that a free-mode retry wait is visible on screen, and that a
# chain which exhausts leaves a trace in the transcript.
#
# The routing audit measured two user-visible defects in auto mode, both of them
# silent: a rate-limit backoff waited 20-120s behind a bare spinner with no
# cause and no countdown, and a fully exhausted chain left only a 5s toast, so a
# turn that produced nothing appeared to vanish. Two surfaces fix that:
#
#   StreamEvent::UpstreamRetryProgress -> a live status-row countdown, and
#   SystemMessageStyle::Error          -> a transcript row that outlives the toast.
#
# This probe drives the REAL TUI in tmux against `free-upstream-rotation-mock.py`
# configured with a single key that 429s every chat request, and asserts:
#
#   1. during the wait the status row names the cause and counts down (at least
#      two distinct values, so it is a live indicator and not a frozen string);
#   2. the retry actually goes out, roughly one backoff later;
#   3. after the chain exhausts, the failure text is in the transcript, and is
#      still there well past the toast's 5s lifetime.
#
# Everything runs against a scratch CLAWDE_HOME and a scratch HOME, so the
# developer's real config, sessions and auth store are never read or written.
#
# Usage: bash scripts/probes/retry-visibility-probe.sh [path-to-binary]
#   KEEP_SCRATCH=1 to keep the captured panes and the mock log.

set -uo pipefail

BIN="${1:-src-rust/target/debug/clawde}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
MOCK="$REPO_ROOT/scripts/probes/free-upstream-rotation-mock.py"

UPSTREAM="nvidia"
WIRE_MODEL="nvidia/nemotron-3.5-lightning-30b-a3b"
MODEL="free/${UPSTREAM}/${WIRE_MODEL}"
BASE_URL_VAR="CLAWDE_FREE_BASE_URL_$(printf '%s' "$UPSTREAM" | tr '[:lower:]-' '[:upper:]_')"
PROMPT="say hello"
SESSION="clawde-retry-visibility-probe"
PORT="${PORT:-18081}"
KEY_A="nvapi-FAKEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA01"

fail() { PROBE_FAILED=1; echo "FAIL: $*" >&2; exit 1; }
note() { echo "  $*"; }

[ -x "$BIN" ] || fail "no binary at $BIN (run: cargo build)"
[ -f "$MOCK" ] || fail "no mock at $MOCK"
command -v tmux >/dev/null || fail "tmux not installed"

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/clawde-retry-visibility.XXXXXX")"
LOG="$SCRATCH/requests.jsonl"
READY="$SCRATCH/mock.ready"
MOCK_OUT="$SCRATCH/mock.out"
PANES="$SCRATCH/panes"
mkdir -p "$PANES"

cleanup() {
  tmux kill-session -t "$SESSION" 2>/dev/null
  [ -n "${MOCK_PID:-}" ] && kill "$MOCK_PID" 2>/dev/null
  wait "${MOCK_PID:-}" 2>/dev/null
  if [ "${KEEP_SCRATCH:-0}" != "1" ] && [ "${PROBE_FAILED:-0}" != "1" ]; then
    rm -rf "$SCRATCH"
  fi
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Credential and config: exactly ONE key, so nothing can be rotated onto and
# the same-upstream retry path is the only way forward.
# ---------------------------------------------------------------------------

python3 - "$SCRATCH" "$UPSTREAM" "$KEY_A" <<'PY'
import json, pathlib, sys

scratch, upstream, key_a = sys.argv[1:4]
home = pathlib.Path(scratch)
(home / "auth.json").write_text(
    json.dumps({"credentials": {}, "keys": {upstream: [key_a]}}, indent=2)
)
(home / "settings.json").write_text(
    json.dumps(
        {
            "config": {"importExternalSessionsOnStart": False},
            "hasCompletedOnboarding": True,
        },
        indent=2,
    )
)
PY

# ---------------------------------------------------------------------------
# Mock upstream: every chat request answers 429.
# ---------------------------------------------------------------------------

python3 "$MOCK" \
  --port "$PORT" \
  --throttle-key "$KEY_A" \
  --log "$LOG" \
  --ready-file "$READY" >"$MOCK_OUT" 2>&1 &
MOCK_PID=$!

for _ in $(seq 1 50); do
  [ -f "$READY" ] && break
  sleep 0.1
done
[ -f "$READY" ] || fail "mock never came up (see $MOCK_OUT)"
note "mock upstream listening on 127.0.0.1:$PORT (every chat request 429s)"

cat >"$SCRATCH/launch.sh" <<EOF
#!/usr/bin/env bash
exec env -i \\
  PATH="$PATH" \\
  TERM="\${TERM:-dumb}" \\
  HOME="$SCRATCH" \\
  CLAWDE_HOME="$SCRATCH" \\
  "$BASE_URL_VAR=http://127.0.0.1:$PORT/v1" \\
  "$BIN" -m "$MODEL" "\$@"
EOF
chmod +x "$SCRATCH/launch.sh"

# ---------------------------------------------------------------------------
# Drive the TUI
# ---------------------------------------------------------------------------

tmux kill-session -t "$SESSION" 2>/dev/null
tmux new-session -d -s "$SESSION" -x 100 -y 30
tmux send-keys -t "$SESSION" "$SCRATCH/launch.sh" C-m

for _ in $(seq 1 80); do
  tmux capture-pane -t "$SESSION" -p 2>/dev/null | grep -q '❯' && break
  sleep 0.5
done
sleep 2

capture() { tmux capture-pane -t "$SESSION" -p 2>/dev/null; }

# Text and the submit key MUST be separate invocations (bracketed paste turns a
# CR sent alongside the text into a literal newline — see AGENTS.md).
tmux send-keys -t "$SESSION" "$PROMPT"
sleep 4
SUBMIT_AT="$(date +%s)"
tmux send-keys -t "$SESSION" C-m

# ---------------------------------------------------------------------------
# Assertion 1: the wait is on screen, named, and counting down
# ---------------------------------------------------------------------------

WAIT_SEEN_AT=""
declare -A REMAINING_SEEN=()
for i in $(seq 1 60); do
  sleep 1
  PANE="$(capture)"
  printf '%s\n' "$PANE" >"$PANES/t+${i}s.txt"
  if grep -qE 'retrying in [0-9]+s' <<<"$PANE"; then
    [ -n "$WAIT_SEEN_AT" ] || WAIT_SEEN_AT="$i"
    VALUE="$(grep -oE 'retrying in [0-9]+s' <<<"$PANE" | head -1)"
    REMAINING_SEEN["$VALUE"]=1
  fi
  grep -q 'free-mode upstreams' <<<"$PANE" && break
done

echo
echo "=== assertion 1: retry wait is visible ==="
[ -n "$WAIT_SEEN_AT" ] || {
  echo "--- last pane ---"
  sed -n '1,30p' "$PANES/t+60s.txt" 2>/dev/null || true
  fail "no retry indicator ever reached the pane"
}
note "indicator first on screen ${WAIT_SEEN_AT}s after submit"
for value in "${!REMAINING_SEEN[@]}"; do note "  saw: $value"; done

WAIT_PANE="$(cat "$PANES/t+${WAIT_SEEN_AT}s.txt")"
grep -q "$UPSTREAM" <<<"$WAIT_PANE" || fail "the indicator never names the upstream"
grep -qi 'rate limited' <<<"$WAIT_PANE" || fail "the indicator never names the cause"
[ "${#REMAINING_SEEN[@]}" -ge 2 ] \
  || fail "the indicator showed only one value: a frozen string, not a countdown"
note "cause named, and the countdown moved (${#REMAINING_SEEN[@]} distinct values)"

# ---------------------------------------------------------------------------
# Assertion 2: the retry actually went out, about one backoff later
# ---------------------------------------------------------------------------

RETRY_AT=""
for i in $(seq 1 90); do
  PANE="$(capture)"
  if grep -q 'free-mode upstreams' <<<"$PANE"; then
    RETRY_AT="$i"
    printf '%s\n' "$PANE" >"$PANES/exhausted.txt"
    break
  fi
  sleep 1
done
[ -n "$RETRY_AT" ] || fail "the chain never reported exhaustion (see $PANES)"

echo
echo "=== assertion 2: the walk ran and reported ==="
python3 - "$LOG" <<'PY' || fail "the mock log does not show the expected retry shape"
import json, sys

rows = [json.loads(line) for line in open(sys.argv[1]) if line.strip()]
chat = [r for r in rows if r["path"].endswith("/chat/completions")]
if len(chat) < 2:
    print(f"FAIL: expected a retry after the 429, got {len(chat)} chat request(s)")
    raise SystemExit(1)
turn = chat[-3:] if len(chat) >= 3 else chat
for r in turn:
    print(f"  t={r['ts']} {r['key_id']} {r['status']} {r['model']}")
gaps = [round(turn[i + 1]["ts"] - turn[i]["ts"], 2) for i in range(len(turn) - 1)]
print(f"  re-dispatch gap(s): {gaps}s")
if not any(gap >= 5 for gap in gaps):
    print("FAIL: no request was delayed by a backoff — the retry did not wait")
    raise SystemExit(1)
PY

# ---------------------------------------------------------------------------
# Assertion 3: the failure survives the toast
# ---------------------------------------------------------------------------

IMMEDIATE="$(capture)"
# The toast lives 5s by design for routine throttles; an exhausted chain is not
# one, so the transcript row has to outlast it.
note "waiting 12s (past the 5s toast lifetime) before re-capturing"
sleep 12
AFTER="$(capture)"
printf '%s\n' "$AFTER" >"$PANES/after-toast-expiry.txt"

echo
echo "=== assertion 3: the failed turn stays in the transcript ==="
grep -q 'free-mode upstreams' <<<"$IMMEDIATE" || fail "the failure was not on screen at all"
grep -q 'free-mode upstreams' <<<"$AFTER" \
  || fail "the failure vanished with the toast (see $PANES/after-toast-expiry.txt)"
note "exhaustion text present immediately and 12s later"

if grep -q 'free-mode upstreams' <<<"$AFTER"; then
  echo
  echo "=== pane after toast expiry ==="
  grep -n -B2 -A2 'free-mode upstreams' <<<"$AFTER"
fi

echo
echo "PASS: the retry wait was visible and counted down; the exhausted chain left a durable transcript row"
echo "artifacts: $SCRATCH"
[ "${KEEP_SCRATCH:-0}" = "1" ] || note "set KEEP_SCRATCH=1 to keep artifacts on success"
