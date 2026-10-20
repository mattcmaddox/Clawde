#!/usr/bin/env bash
# End-to-end proof that a provider tool-call cap resumes on another upstream.
#
# Runs the REAL clawde TUI in tmux against two local OpenAI-compatible mocks,
# one standing in for nvidia and one for groq. Each may stop its FIRST probe
# request with `finish_reason: "max_tool_calls"` (an upstream tool-call cap);
# whichever upstream the router leads with does, and the other must serve the
# resume. Passes only when:
#
#   1. the first turn completes and renders the second upstream's reply;
#   2. the capped upstream is asked exactly once — the resume moved elsewhere;
#   3. the resume request carries the transcript AND the cap resume prompt
#      (the note that says the previous provider capped, which is also what the
#      session transcript persists);
#   4. the NEXT user turn reaches the second upstream without touching the
#      capped one (the short-lived penalty demotes it in the free chain's plan);
#   5. no failure placeholder ("no response from …") reaches the pane.
#
# Why a probe: the Rust tests drive a mock `LlmProvider` and therefore skip the
# transport (SSE decoding of `finish_reason`, the TUI turn loop, the composite
# free provider's own plan ordering). Only the real binary proves the seam.
#
# Everything runs against a scratch CLAWDE_HOME and HOME, so the developer's
# config, sessions and auth store are never read or written. Debug builds only:
# `CLAWDE_FREE_BASE_URL_<UPSTREAM>` refuses non-loopback hosts and release
# binaries.
#
# Usage: bash scripts/probes/tool-cap-switch-probe.sh [path-to-binary]

set -uo pipefail

BIN="${1:-src-rust/target/debug/clawde}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
MOCK="$REPO_ROOT/scripts/probes/tool-cap-switch-mock.py"

CAPPED_UPSTREAM="nvidia"
RESUME_UPSTREAM="groq"
CAP_PORT="${CAP_PORT:-18110}"
RESUME_PORT="${RESUME_PORT:-18111}"
PROBE_MARKER="TOOLCAP-PROBE"
REPLY_MARKER="TOOLCAP-REPLY"
SESSION="clawde-tool-cap-probe"

fail() { echo "FAIL: $*" >&2; exit 1; }
note() { echo "  $*"; }

[ -x "$BIN" ] || fail "no binary at $BIN (run: cargo build)"
[ -f "$MOCK" ] || fail "no mock at $MOCK"
command -v tmux >/dev/null || fail "tmux not installed"

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/clawde-tool-cap-probe.XXXXXX")"
CAP_LOG="$SCRATCH/capped.jsonl"
RESUME_LOG="$SCRATCH/resume.jsonl"

cleanup() {
  tmux kill-session -t "$SESSION" 2>/dev/null
  for pid in "${CAP_PID:-}" "${RESUME_PID:-}"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
  done
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Credentials (fabricated; never real) and first-run config
# ---------------------------------------------------------------------------

CAP_KEY="nvapi-FAKETOOLCAPAAAAAAAAAAAAAAAAAAAAAA01"
RESUME_KEY="gsk_FAKETOOLCAPBBBBBBBBBBBBBBBBBBBBBBB02"

python3 - "$SCRATCH" "$CAPPED_UPSTREAM" "$CAP_KEY" "$RESUME_UPSTREAM" "$RESUME_KEY" <<'PY'
import json, pathlib, sys

scratch, capped_upstream, capped_key, resume_upstream, resume_key = sys.argv[1:6]
home = pathlib.Path(scratch)

# Only these two upstreams are keyed, so the free chain has no third lane it
# could reach instead of the one under test.
(home / "auth.json").write_text(
    json.dumps(
        {
            "credentials": {},
            "keys": {capped_upstream: [capped_key], resume_upstream: [resume_key]},
        },
        indent=2,
    )
)

# A fresh home would otherwise stop on the first-launch flow, and importing the
# developer's real sessions would make the probe read state it must not touch.
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
# Mock upstreams
# ---------------------------------------------------------------------------

python3 "$MOCK" \
  --port "$CAP_PORT" \
  --upstream "$CAPPED_UPSTREAM" \
  --reply "$REPLY_MARKER" \
  --marker "$PROBE_MARKER" \
  --cap-once \
  --log "$CAP_LOG" \
  --ready-file "$SCRATCH/capped.ready" >"$SCRATCH/capped.out" 2>&1 &
CAP_PID=$!

python3 "$MOCK" \
  --port "$RESUME_PORT" \
  --upstream "$RESUME_UPSTREAM" \
  --reply "$REPLY_MARKER" \
  --marker "$PROBE_MARKER" \
  --cap-once \
  --log "$RESUME_LOG" \
  --ready-file "$SCRATCH/resume.ready" >"$SCRATCH/resume.out" 2>&1 &
RESUME_PID=$!

for _ in $(seq 1 50); do
  [ -f "$SCRATCH/capped.ready" ] && [ -f "$SCRATCH/resume.ready" ] && break
  sleep 0.1
done
[ -f "$SCRATCH/capped.ready" ] || fail "capped mock never came up (see $SCRATCH/capped.out)"
[ -f "$SCRATCH/resume.ready" ] || fail "resume mock never came up (see $SCRATCH/resume.out)"
note "mocks listening on 127.0.0.1:$CAP_PORT ($CAPPED_UPSTREAM) and :$RESUME_PORT ($RESUME_UPSTREAM)"

# Minimal env: no ambient provider key can join the chain.
cat >"$SCRATCH/launch.sh" <<EOF
#!/usr/bin/env bash
exec env -i \\
  PATH="$PATH" \\
  TERM="\${TERM:-dumb}" \\
  HOME="$SCRATCH" \\
  CLAWDE_HOME="$SCRATCH" \\
  "CLAWDE_FREE_BASE_URL_$(printf '%s' "$CAPPED_UPSTREAM" | tr '[:lower:]-' '[:upper:]_')=http://127.0.0.1:$CAP_PORT/v1" \\
  "CLAWDE_FREE_BASE_URL_$(printf '%s' "$RESUME_UPSTREAM" | tr '[:lower:]-' '[:upper:]_')=http://127.0.0.1:$RESUME_PORT/v1" \\
  "$BIN" -m free/auto "\$@"
EOF
chmod +x "$SCRATCH/launch.sh"

# ---------------------------------------------------------------------------
# Drive the TUI
# ---------------------------------------------------------------------------

tmux kill-session -t "$SESSION" 2>/dev/null
tmux new-session -d -s "$SESSION" -x 120 -y 40
tmux send-keys -t "$SESSION" "$SCRATCH/launch.sh" C-m

# Wait for the input prompt box, not the banner: keys sent before it exists are
# dropped, and a dropped submit looks exactly like a slow turn.
for _ in $(seq 1 80); do
  tmux capture-pane -t "$SESSION" -p 2>/dev/null | grep -q '❯' && break
  sleep 0.5
done
sleep 2

pane_has() {
  tmux capture-pane -t "$SESSION" -p 2>/dev/null | grep -q "$1"
}

# Text and the submit key MUST be separate invocations (bracketed paste turns a
# CR sent alongside text into a literal newline; see AGENTS.md).
send_prompt() {
  tmux send-keys -t "$SESSION" "$1"
  sleep 4
  tmux send-keys -t "$SESSION" C-m
}

wait_for() {
  local marker="$1"
  for _ in $(seq 1 20); do
    sleep 1
    pane_has "$marker" && return 0
  done
  return 1
}

dump_and_fail() {
  echo "--- pane ---"
  tmux capture-pane -t "$SESSION" -p 2>/dev/null
  echo "--- capped upstream log ---"
  cat "$CAP_LOG" 2>/dev/null
  echo "--- resume upstream log ---"
  cat "$RESUME_LOG" 2>/dev/null
  fail "$1"
}

send_prompt "$PROBE_MARKER first turn"
wait_for "$REPLY_MARKER-1" \
  || dump_and_fail "the resumed turn never rendered the second upstream's reply"
note "turn 1: the cap was followed by a reply from $RESUME_UPSTREAM"

send_prompt "$PROBE_MARKER second turn"
wait_for "$REPLY_MARKER-2" \
  || dump_and_fail "the second turn never rendered the second upstream's reply"
note "turn 2: the next prompt reached $RESUME_UPSTREAM"

PANE="$(tmux capture-pane -t "$SESSION" -p 2>/dev/null)"
echo
echo "=== pane ==="
printf '%s\n' "$PANE"
echo "=== capped upstream log ==="
cat "$CAP_LOG" 2>/dev/null || fail "no request log for the capped upstream"
echo "=== resume upstream log ==="
cat "$RESUME_LOG" 2>/dev/null || fail "no request log for the resume upstream"
echo

if grep -qE '\(no response from|stream error|request failed' <<<"$PANE"; then
  fail "a failure placeholder reached the user"
fi

# ---------------------------------------------------------------------------
# Assertions
# ---------------------------------------------------------------------------

python3 - "$CAP_LOG" "$RESUME_LOG" <<'PY' || exit 1
import json, sys

cap_log, resume_log = sys.argv[1:3]


def chat_rows(path):
    rows = [json.loads(line) for line in open(path) if line.strip()]
    return [
        r
        for r in rows
        if r["path"].endswith("/chat/completions") and r["contains_probe"]
    ]


def die(msg):
    print(f"FAIL: {msg}", file=sys.stderr)
    raise SystemExit(1)


by_upstream = {}
for path in (cap_log, resume_log):
    rows = chat_rows(path)
    if rows:
        by_upstream[rows[0]["upstream"]] = rows

cap_rows = [(up, r) for up, rows in by_upstream.items() for r in rows if r["mode"] == "cap"]
if len(cap_rows) != 1:
    die(
        "exactly one upstream may stop the first probe turn at its tool-call "
        f"cap, got {cap_rows}"
    )
capped_upstream, cap_row = cap_rows[0]
resumed_upstream = next(
    (up for up, rows in by_upstream.items() if up != capped_upstream and rows),
    None,
)
if resumed_upstream is None:
    die("no upstream served the resume")
resumed = by_upstream[resumed_upstream]

# Turn 2 is a fresh prompt: the short-lived penalty must keep the capped
# upstream out of the plan, so all probe requests after the cap go elsewhere.
if len(by_upstream[capped_upstream]) != 1:
    die(
        "the capped upstream must not be asked again (turn 2 must skip it), "
        f"got {by_upstream[capped_upstream]}"
    )
if len(resumed) != 2:
    die(f"the resume upstream must serve exactly the resume and turn 2, got {resumed}")

first = resumed[0]
if first["mode"] != "reply":
    die(f"the resume request must be answered, got {first}")
if not first["contains_resume_prompt"]:
    die(
        "the resume request must carry the tool-call-cap prompt that tells the "
        f"new provider what remains, got {first}"
    )
if first["message_count"] <= cap_row["message_count"]:
    die(
        "the resume request must carry the capped turn's transcript "
        f"({first['message_count']} <= {cap_row['message_count']} messages)"
    )

print(f"  {capped_upstream}: 1 probe request (the cap), never retried")
print(
    f"  {resumed_upstream}: resume carried {first['message_count']} messages "
    f"with the cap prompt; turn 2 served as its reply #2"
)
print("  the cap resumed elsewhere and the next turn skipped the capped upstream")
PY
PROBE_OK=$?

echo
if [ "$PROBE_OK" -eq 0 ]; then
  echo "PASS: a provider tool-call cap resumed on another upstream and later turns skipped it"
else
  echo "FAIL: see the assertions above"
fi
echo "artifacts: $SCRATCH"
[ "${KEEP_SCRATCH:-0}" = "1" ] || note "set KEEP_SCRATCH=1 to keep artifacts on success"
exit "$PROBE_OK"
