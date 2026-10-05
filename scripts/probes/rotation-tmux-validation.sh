#!/usr/bin/env bash
# End-to-end proof that a rate-limited API key is invisible to the user.
#
# Runs the REAL clawde TUI in tmux against the local mock upstream in
# `free-upstream-rotation-mock.py`, with two NVIDIA credentials of which the
# first always answers 429. Passes only when:
#
#   1. the turn completes and the mock's reply is on screen;
#   2. no rate-limit wording ever reaches the pane;
#   3. the request log shows 429 on key A then 200 on key B, same model string;
#   4. the benched key is tried exactly once (no per-turn re-pay of the 429);
#   5. the bench survives in the persisted key-ring state file, with the cooldown
#      SCOPE its researched profile calls for (per-model upstreams bench only
#      the model that 429'd; per-key ones bench the whole key).
#
# Everything runs against a scratch CLAWDE_HOME and a scratch HOME, so the
# developer's real config, sessions and auth store are never read or written.
#
# The mechanism under test is provider-agnostic: `build_free_provider` wraps ANY
# catalog upstream holding 2+ keys in the same KeyRotatingProvider. So the probe
# is not NVIDIA-specific — set UPSTREAM (and WIRE_MODEL for a non-NVIDIA id) to
# check any other upstream, including the per-model ones whose cooldown is
# scoped to a single model rather than the whole key. Three are exercised in CI-
# runnable form, covering both cooldown scopes and both probe shapes:
#   nvidia    per-key  bench, auth-lax probe (chat confirm only)
#   groq      per-model bench, auth-validating probe (models GET)
#   cerebras  per-model bench, auth-validating probe
#
# Usage: bash scripts/probes/rotation-tmux-validation.sh [path-to-binary]
#   UPSTREAM=groq     WIRE_MODEL=openai/gpt-oss-120b bash scripts/probes/rotation-tmux-validation.sh
#   UPSTREAM=cerebras WIRE_MODEL=gpt-oss-120b        bash scripts/probes/rotation-tmux-validation.sh

set -uo pipefail

BIN="${1:-src-rust/target/debug/clawde}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
MOCK="$REPO_ROOT/scripts/probes/free-upstream-rotation-mock.py"
# The researched per-upstream profiles: the persisted cooldown scope is asserted
# against the same file the runtime profile loader reads.
PROFILES="$REPO_ROOT/src-rust/crates/api/src/providers/free/provider-cooldown-profiles.json"

# The upstream under test. The pin the user types is `free/<upstream>/<wire>`;
# the mock sees the wire id, because the `free/<upstream>/` prefix is stripped
# before dispatch.
UPSTREAM="${UPSTREAM:-nvidia}"
WIRE_MODEL="${WIRE_MODEL:-nvidia/nemotron-3.5-lightning-30b-a3b}"
MODEL="free/${UPSTREAM}/${WIRE_MODEL}"
BASE_URL_VAR="CLAWDE_FREE_BASE_URL_$(printf '%s' "$UPSTREAM" | tr '[:lower:]-' '[:upper:]_')"
PROMPT="say hello"
REPLY_MARKER="ROTATED"
SESSION="clawde-rotation-probe"
PORT="${PORT:-18099}"

fail() { echo "FAIL: $*" >&2; exit 1; }
note() { echo "  $*"; }

[ -x "$BIN" ] || fail "no binary at $BIN (run: cargo build)"
[ -f "$MOCK" ] || fail "no mock at $MOCK"
command -v tmux >/dev/null || fail "tmux not installed"

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/clawde-rotation-probe.XXXXXX")"
LOG="$SCRATCH/requests.jsonl"
READY="$SCRATCH/mock.ready"
MOCK_OUT="$SCRATCH/mock.out"

cleanup() {
  tmux kill-session -t "$SESSION" 2>/dev/null
  [ -n "${MOCK_PID:-}" ] && kill "$MOCK_PID" 2>/dev/null
  wait "${MOCK_PID:-}" 2>/dev/null
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Credentials (fabricated; never real) and config
# ---------------------------------------------------------------------------

# Fabricated credentials in the shape each upstream issues, so a real endpoint
# answers them with an auth error rather than a protocol error if one is ever
# reached by accident.
case "$UPSTREAM" in
  nvidia) KEY_PREFIX="nvapi-" ;;
  groq) KEY_PREFIX="gsk_" ;;
  cerebras) KEY_PREFIX="csk-" ;;
  *) KEY_PREFIX="fake-" ;;
esac
KEY_A="${KEY_PREFIX}FAKEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA01"
KEY_B="${KEY_PREFIX}FAKEBBBBBBBBBBBBBBBBBBBBBBBBBBBB02"

python3 - "$SCRATCH" "$UPSTREAM" "$KEY_A" "$KEY_B" <<'PY'
import json, pathlib, sys

scratch, upstream, key_a, key_b = sys.argv[1:5]
home = pathlib.Path(scratch)

# auth.json: multi-key store is the only credential source the free chain reads.
# Keyed by the upstream UNDER TEST — hardcoding an id here would leave the chain
# without the pinned upstream, silently falling back to some other provider.
(home / "auth.json").write_text(
    json.dumps({"credentials": {}, "keys": {upstream: [key_a, key_b]}}, indent=2)
)

# The route pin is passed on the command line (`-m`), never here, so there is
# one source of truth for it. `importExternalSessionsOnStart` off keeps the
# probe from importing unrelated sessions out of the developer's real home.
(home / "settings.json").write_text(
    json.dumps(
        {
            "config": {"importExternalSessionsOnStart": False},
            # A fresh home would otherwise stop on the first-launch flow.
            "hasCompletedOnboarding": True,
        },
        indent=2,
    )
)
PY

# ---------------------------------------------------------------------------
# Mock upstream
# ---------------------------------------------------------------------------

python3 "$MOCK" \
  --port "$PORT" \
  --throttle-key "$KEY_A" \
  --reply "HELLO-FROM-THE-$REPLY_MARKER-KEY" \
  --log "$LOG" \
  --ready-file "$READY" >"$MOCK_OUT" 2>&1 &
MOCK_PID=$!

for _ in $(seq 1 50); do
  [ -f "$READY" ] && break
  sleep 0.1
done
[ -f "$READY" ] || fail "mock never came up (see $MOCK_OUT)"
note "mock upstream listening on 127.0.0.1:$PORT"

# ---------------------------------------------------------------------------
# Launcher: minimal env so no ambient provider key can join the chain, and
# HOME redirected so external-session import cannot see the real home.
# ---------------------------------------------------------------------------

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

# Wait for the input prompt box, not just the banner: keys sent before the
# box exists are dropped, and a dropped submit looks exactly like a slow turn.
for _ in $(seq 1 80); do
  tmux capture-pane -t "$SESSION" -p 2>/dev/null | grep -q '❯' && break
  sleep 0.5
done
sleep 2

marker_seen() {
  tmux capture-pane -t "$SESSION" -p 2>/dev/null | grep -q "$REPLY_MARKER"
}

# Text and the submit key MUST be separate invocations: bracketed paste turns a
# CR sent alongside the text into a literal newline (see AGENTS.md).
tmux send-keys -t "$SESSION" "$PROMPT"
sleep 4
tmux send-keys -t "$SESSION" C-m

# Poll for the mock's reply rather than sleeping a fixed amount. If the reply
# has not appeared after 20s, re-send the submit key: a C-m that lands while the
# app is still warming up is silently dropped, and re-sending is harmless once
# the turn has started.
for _ in $(seq 1 8); do
  for _ in $(seq 1 10); do
    sleep 2
    marker_seen && break 2
  done
  tmux send-keys -t "$SESSION" C-m
done
CLEAN_PANE="$(tmux capture-pane -t "$SESSION" -p 2>/dev/null)"

echo
echo "=== pane ==="
printf '%s\n' "$CLEAN_PANE"
echo "=== /keys health ==="
tmux send-keys -t "$SESSION" "/keys health"
sleep 2
tmux send-keys -t "$SESSION" C-m
sleep 3
HEALTH_PANE="$(tmux capture-pane -t "$SESSION" -p 2>/dev/null)"
printf '%s\n' "$HEALTH_PANE"
echo

# ---------------------------------------------------------------------------
# Assertions
# ---------------------------------------------------------------------------

echo "=== request log ==="
cat "$LOG" 2>/dev/null || fail "no request log — the turn never reached the mock"

grep -q "$REPLY_MARKER" <<<"$CLEAN_PANE" \
  || fail "the reply never reached the pane: the turn did not complete"

# A failed turn surfaces the upstream error text in the transcript. The
# key-ring footer indicator (`nvidia:1/2 (retry in 60s)`) is intentional UI and
# is NOT a failure, so this looks for error wording only — never the footer's
# own retry counter.
if grep -qiE 'too many requests|\b429\b|rate limit exceeded|rate_limit|\berror\b|request failed' <<<"$CLEAN_PANE"; then
  echo "$CLEAN_PANE" | grep -inE 'too many requests|\b429\b|rate limit exceeded|rate_limit|\berror\b|request failed'
  fail "rate-limit wording reached the user: the hand-off was not silent"
fi
note "reply rendered with no rate-limit error wording in the transcript"

THROTTLED_KEY_ID="..${KEY_A: -4}"
HEALTHY_KEY_ID="..${KEY_B: -4}"

python3 - "$LOG" "$THROTTLED_KEY_ID" "$HEALTHY_KEY_ID" "$WIRE_MODEL" <<'PY' || exit 1
import json, sys

log_path, throttled, healthy, model = sys.argv[1:5]
rows = [json.loads(line) for line in open(log_path) if line.strip()]
chat = [r for r in rows if r["path"].endswith("/chat/completions")]

def die(msg):
    print(f"FAIL: {msg}", file=sys.stderr)
    raise SystemExit(1)

if not chat:
    die("no chat/completions request reached the mock")

# The startup health sweep issues its own 1-token chat probe per key, at
# `fallback_models.first()` (nvidia: openai/gpt-oss-20b), so the log can hold a
# 429 that is not the turn's. That probe never benches the key — a 429 is
# classified Transient, so the poller leaves ring state alone (see
# `crates/api/src/health_poller.rs`) — but it still is not the turn under test,
# so measure from the LAST throttled 429 onward.
exhausted_at = [
    i for i, r in enumerate(chat) if r["key_id"] == throttled and r["status"] == 429
]
if not exhausted_at:
    die("the throttled key never answered 429 on a chat request")
turn = chat[exhausted_at[-1]:]

if turn[0]["status"] != 429:
    die(f"the turn must open with the 429 it recovers from, got {turn[0]}")
after = turn[1:]
if not after:
    die("nothing was sent after the 429 — no hand-off happened")
if any(r["key_id"] != healthy for r in after):
    die(f"only the healthy key may serve after the hand-off, got {after}")
if any(r["status"] != 200 for r in after):
    die(f"no request after the hand-off may fail, got {after}")
if sum(1 for r in turn if r["key_id"] == throttled) != 1:
    die(f"a benched key must not be retried during the turn, got {turn}")

models = {r["model"] for r in turn}
if models != {model}:
    die(f"the model changed across the hand-off: {models} != {{{model}}}")

prior = chat[: exhausted_at[-1]]
if prior:
    probe_models = sorted({r["model"] for r in prior})
    print(
        f"  {len(prior)} startup health-sweep chat probe(s) preceded the turn "
        f"(models: {probe_models} — the sweep probes at fallback_models[0], not the pin)"
    )
print(f"  throttled key {throttled}: 1 request in the turn (the 429), then benched")
print(f"  healthy key   {healthy}: {len(after)} request(s), all 200")
print(f"  model on every turn attempt: {sorted(models)}")
print("  rotation was seamless and stayed on one model")
PY
ROTATION_OK=$?

# Persisted bench: the cooldown must outlive the process, or a restart
# resurrects the throttled key and re-pays the 429.
STATE="$SCRATCH/key-ring-state/$UPSTREAM.json"
if [ -f "$STATE" ]; then
  python3 - "$STATE" "$KEY_A" "$UPSTREAM" "$WIRE_MODEL" "$PROFILES" <<'PY' || ROTATION_OK=1
import json, sys

state_path, throttled, upstream, wire_model, profiles_path = sys.argv[1:6]
state = json.load(open(state_path))
entry = next((e for e in state.get("entries", []) if e.get("key") == throttled), None)
if entry is None:
    print("FAIL: throttled key missing from persisted ring state", file=sys.stderr)
    raise SystemExit(1)
if not entry.get("cooldown_remaining_secs"):
    print("FAIL: throttled key persisted without a cooldown", file=sys.stderr)
    raise SystemExit(1)

# Scope, not duration: a per-model upstream (groq, cerebras, sambanova,
# google, openrouter, cline) benches only the model it 429'd on and stays usable
# for every other model, while a per-key upstream (the default: nvidia, zai,
# opencode-zen) benches the key outright. Read from the same profile file the
# runtime loads, so re-scoping a profile cannot silently leave this probe
# asserting the old shape.
profiles = json.load(open(profiles_path))["profiles"]
limit_scope = profiles.get(upstream, {}).get("limit_scope", "per-key")
expected = wire_model if limit_scope == "per-model" else None
persisted = entry.get("cooldown_model")
if persisted != expected:
    print(
        f"FAIL: persisted cooldown_model={persisted!r} but {upstream}'s profile "
        f"says limit_scope={limit_scope!r}, which expects {expected!r}",
        file=sys.stderr,
    )
    raise SystemExit(1)

print(
    f"  persisted bench: {entry['cooldown_remaining_secs']}s remaining, "
    f"{upstream} limit_scope={limit_scope} -> cooldown_model={persisted!r}"
)
PY
  note "key-ring state persisted at $STATE"
else
  echo "FAIL: no key-ring state file at $STATE" >&2
  ROTATION_OK=1
fi

echo
if [ "$ROTATION_OK" -eq 0 ]; then
  echo "PASS: a 429 on one key was invisible, and the turn stayed on one model"
else
  echo "FAIL: see the assertions above"
fi
echo "artifacts: $SCRATCH"
[ "${KEEP_SCRATCH:-0}" = "1" ] || note "set KEEP_SCRATCH=1 to keep artifacts on success"
exit "$ROTATION_OK"
