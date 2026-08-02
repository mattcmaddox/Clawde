# Free-Mode Workflow Spec — "Type `hi`, get an answer"

Status: **Mostly shipped** — core recovery + health poller + parallel probe done; remaining config keys pending.

## Implementation status (updated 2026-07-31, finalised 2026-08-01)

### Shipped

- **[§6.2] RetryingFreeStream** — Empty completions detected inside `FreeProvider`; placeholder emitted to transcript; identical `ProviderRequest` re-dispatched to next plan entry. The query loop (`query/src/lib.rs`) keeps its existing placeholder branch as a defensive last-resort path.
- **[§6.3] Empty-completion cooldown** — `CooldownState` tracks `consecutive_empties` per upstream; 3 consecutive empties arms a 60s cooldown (configurable via `EmptyCooldownConfig` in `RoutingConfig`). Cooldown gates dispatch (`is_in_empty_cooldown` checked at three call sites); `record_success` resets the counter. When all plan entries are in cooldown, `create_message_stream` returns `Err(ServerError)` with a message listing the skipped upstreams.
- **[§6.4] Health poller** — `crates/api/src/health_poller.rs`: async background task probes each configured free-upstream key via `validate_upstream_key()` (GET `/v1/models`, 5s timeout, `spawn_blocking` for blocking HTTP; upstreams whose models endpoint doesn't check auth — nvidia, huggingface, openrouter, sambanova, cline — get a 1-token `chat/completions` confirmation probe). Runs startup probe + every 5 min (configurable). Wired in `cli/src/main.rs` for both TUI (`tokio::spawn run_health_poller(300)`) and headless (`tokio::spawn run_health_poller(0)`) paths. Poller injects unhealthy keys into live key rings via `LlmProvider::mark_key_exhausted`. `classify_health_error()` maps probe errors to cooldown durations. `/health [<upstream>]` runs the same probe synchronously; `probe_sync_for()` limits a sweep to one upstream without clobbering the shared last-sweep slot.
- **[§6.5] First-byte watchdog / staggered probe** — `RetryingFreeStream` polls the current upstream; if no first byte arrives within `first_byte_timeout_secs` (configurable, default 0 = disabled), a parallel `create_message_stream` is spawned at the next plan entry via `tokio::spawn`. Both streams race — first to produce content wins, loser is dropped/cancelled. Only activates on `Route::Auto` and when `staggered_probe` is enabled (default `true`). Pinned routes stay strictly sequential. Plan entries consumed by the parallel probe are restored on all failure/cancel paths. Empty-cooldown arms after 3 consecutive parallel failures, triggering sequential fallback. Config keys: `first_byte_timeout_secs` (u64, default 0) and `staggered_probe` (bool, default true), both under `providers.free.options.routing`.
- **[§6.6] Shift+Enter manual override** — `crates/tui/src/app.rs` handle_keybinding_action: the `"newline"` action now branches on `is_streaming`. When streaming, clears UI state (spinner, texts, thinking, tool blocks) and shows "Aborted — retry or resubmit to try next upstream." The underlying stream future continues in the background; the user's next prompt triggers a fresh FreeProvider dispatch. Idle multi-line editing is unchanged.
- **[§6.7] empty_cooldown config** — `EmptyCooldownConfig { max_consecutive: 3, cooldown_secs: 60 }` serialized under `providers.free.options.routing.empty_cooldown` in `RoutingConfig`.
- **TUI visibility** — Status bar and `/keys health` show upstreams in empty cooldown with remaining retry time. `ProviderRegistry::empty_cooldown_summaries()` exposed for CLI/commands.
- **Persistence** — Empty-cooldown track persisted to `{clawde_home}/empty-cooldown-state/free.json` (atomic tmp+rename, id-keyed restore, silently ignores unknown/expired entries). Production call via `FreeProvider::with_routing(chain, routing, true)`; tests pass `false` so they never touch the real config dir.
- **Settings surface** — `first_byte_timeout_secs` (Number) and `staggered_probe` (Bool) appear in `/settings` → TUI settings screen. All routing fields merge (not overwrite) via `get_or_create_routing_json()` helper.
- **50 unit tests** in `free.rs` covering empty retry, cooldown arming/reset, all-empty/all-cooldown summaries, persistence round-trip, upstream independence, circuit-breaker integration, and 4 parallel probe scenarios (pinned-route exclusion, parallel-win, empty-cooldown fallback, fast-stream no-launch).

### Planned (per spec)

- **[§6.7] remaining config keys** — `upstream_5xx_cooldown_secs`, `health_poll_interval_secs`, `fallback_retries`.

---

## 1. Problem statement

The user has **10+ API keys** spread across the free upstreams (a mix: some
providers have 2–3 keys, most have 1). But the workflow is unreliable:
typing `hi` frequently produces:

```
(no response from groq/llama-3.3-70b-versatile — model ended the turn with stop_reason "end_turn")
```

instead of an answer. The app has 13 upstreams and 10+ keys, yet a single
empty response ends the turn — no key rotation, no upstream fallback, no
retry. The user wants a **basic working workflow**: type a prompt, reliably
get a non-empty text answer.

## 2. Root-cause analysis (verified in code)

The failure is a **classification gap**, not a network problem:

1. **`FreeProvider` only falls back on pre-stream errors.** In
   `src-rust/crates/api/src/providers/free.rs`
   (`create_message` / `create_message_stream`, ~lines 1700–1800), the
   fallback loop only continues to the next upstream when
   `create_message_stream()` returns `Err(ProviderError)` (and passes
   `should_fallback`), or when the `upstream_timeout_secs` (30s) timeout
   fires. An upstream that returns a **successful but empty stream**
   (`200 OK`, zero content, `stop_reason: "end_turn"`) is treated as
   **success** — the chain walk stops there.

2. **Key rotation has the same gap.** `KeyRotatingProvider`
   (`crates/api/src/providers/key_rotating.rs`) rotates keys only on
   `ExhaustSignal` (`QuotaExceeded`, `RateLimited`, `AuthFailed`, or
   `Other { status: 429 | 401 | 403 }`). Empty completions carry no error
   signal, so keys are never rotated and never marked exhausted on them.

3. **The query loop ends the turn on empty output.** In
   `src-rust/crates/query/src/lib.rs` (~line 1420–1440), after the provider
   stream finishes, if `combined_text.is_empty() && combined_thinking.is_empty()`
   the loop injects the `(no response from {provider}/{model} — …)`
   placeholder into the transcript and returns `EndTurn` via
   `continue_or_end!`. No retry, no fallback, no visibility.

4. **`should_fallback()`** (free.rs ~1536) already falls through on
   everything except `InvalidRequest` / `ContentFiltered` — so *error* paths
   are well covered. The empty-completion path simply never produces an
   error.

Net effect: one empty response from the first healthy-looking upstream kills
the turn even though 12 more upstreams and 10+ keys are configured.

## 3. Research findings

Web research on Groq, Cerebras, NVIDIA NIM, and Gemini free tiers:

| Signal | Meaning | How to classify |
|---|---|---|
| `429` + `retry-after` header | **Key** free-tier quota / rate limit exhausted (RPM/RPD/TPM/TPD sliding windows) | Cool down the **key** |
| `401` / `403` | **Key** invalid / revoked | Cool down the **key** (300s) |
| `5xx` (500/502/503) + Groq `498` | **Model/backend** overloaded or unavailable (Groq: "not charged for 5xx") | Cool down the **whole upstream** briefly |
| `200` + empty content + `finish_reason: stop` / `end_turn` | **Zero-token generation** — safety filter, prompt structure, or formatting constraints caused the model to emit nothing. A *third, distinct* signal: neither key nor model exhaustion. | Per-request failure; count toward an **empty-response cooldown** |

Empty-but-`200` completions are documented behaviour on Groq, Cerebras,
NVIDIA, and Gemini (Gemini reports `SAFETY`/`RECITATION` finish reasons).
They arrive **fast** (the user observed Groq returning the empty `end_turn`
in ~3s), whereas genuine generations take longer — an exploitable timing
signal.

## 4. Goals and non-goals

**Goals**

- Type `hi` (or any prompt) in free mode → reliably get a non-empty text
  answer, from whichever upstream/key works, without user intervention.
- Detect empty completions behind the scenes and automatically continue to
  the next upstream (identical re-send).
- Distinguish **key-level** failure (429/401/403 → cool the key), **model /
  backend-level** failure (5xx/498 → cool the upstream briefly), and
  **empty completion** (→ empty-cooldown counter).
- Proactively discover dead keys/providers via a **startup + periodic
  zero-token health poller** so the app never offers dead providers first.
- Self-heal silently: failures feed cooldowns and ordering; no new visible
  log UI (but failures stay visible *in the transcript* via the existing
  placeholder, per user choice).
- Manual escape hatch: a configurable keybinding (default **Shift+Enter**)
  to force-move off the current attempt mid-stream.

**Non-goals**

- No visible failure-log dashboard/panel (user chose self-healing-only).
- No empty-response retry for paid/non-free providers (e.g. pinned
  `anthropic/…`). Scope = free mode + pinned free upstreams.
- No parallel probing on every request (user chose staggered/sequential:
  "do what you think is best", with a first-byte stagger).
- No request simplification on retry (user chose **identical re-send**).

## 5. Interview decisions (summary)

| # | Question | Decision |
|---|---|---|
| 1 | Key layout | Mix: some providers have 2–3 keys, most have 1 |
| 2 | Empty-response recovery | Behind-the-scenes detection; auto-move to next upstream; don't hide failures; distinguish key vs model exhaustion (researched — §3) |
| 3 | Latency cap | No cap on attempts that are *working*; detect fast failures; be aware of the timing layout (see §6.5) |
| 4 | Probing strategy | Staggered/sequential (staggered probe after no first byte; first success wins). Consider a separate low-token poller — implemented as zero-token HEAD poller (§6.4) |
| 5 | Retry payload | **Identical re-send** |
| 6 | Failure trace | Failures logged; app self-aware and periodically checks; **self-healing only** surface (cooldowns + ordering) |
| 7 | Empty cooldown | Cooldown **after repeated empties** (threshold N=3, ~60s) |
| 8 | Poller cadence | **Startup + periodic** (re-ping every few minutes, live key-health updates) |
| 9 | Fix scope | **Free mode + pinned free upstreams** (any route that dispatches through `FreeProvider`) |
| 10 | History purity | **Keep the current placeholder** in the transcript, but **don't end the turn** — retry instead |
| 11 | All-fail UX | **Error + attempt summary** (one line per attempt: `groq: empty (3s); cerebras: rate limited; …`) + hint to run `/keys health` |
| 12 | 5xx handling | **Cool down the whole upstream briefly** (30–60s) |
| 13 | Success metric | **Always a text answer** (within the latency budget) |
| 14 | Manual override | Keybinding default **Shift+Enter** (see §6.6 for the binding conflict) |

## 6. Proposed design

### 6.1 Empty-completion classification

Define an **`EmptyCompletion`** classification. An attempt is an
empty completion when the consumed stream yields:

- zero text blocks (`TextDelta`s → empty joined text), **and**
- zero thinking/reasoning blocks, **and**
- zero tool-use blocks,
- and the stream terminates normally (`MessageStop` / `None`) with
  `stop_reason` of `end_turn` / `stop` / `content_filtered` / unknown.

`max_tokens` or `length` stops with *some* text remain successes (partial
output is real output). `stop_reason = tool_use` with tool blocks is a
success (tools are legitimate output).

**Where detection lives:** inside `FreeProvider`, via a self-retrying stream
wrapper (`RetryingFreeStream`, §6.2) that consumes each upstream's stream to
its terminal event and classifies it. `KeyRotatingProvider` passes streams
through unchanged (it only inspects errors); detection must therefore sit at
the FreeProvider layer, which sees the full event stream.

### 6.2 Fallback & retry semantics

**`RetryingFreeStream`** (new, in `free.rs`):

1. Wraps the request + the plan produced by `attempt_plan()`.
2. Consumes upstream[i]'s stream. Forwards every event (text, thinking,
   tool-use, usage, rate-limit headers) to the caller transparently.
3. On terminal event: classify via §6.1.
   - **Non-empty** → emit `MessageStop`, finish. Success.
   - **Empty** → record the attempt (`provider`, `model`, `stop_reason`,
     elapsed). Emit a **text delta containing the placeholder** (same
     format as today: `(no response from groq/… — model ended the turn with
     stop_reason "end_turn")`), then **re-dispatch the identical request**
     (same `ProviderRequest`, same `ProviderRequest.clone()`) against
     upstream[i+1] with its `default_model`/`effective_model` substituted.
     (Pinned routes continue with the remaining plan entries, mirroring
     today's pinned-then-rest behaviour.)
   - If every attempt in the plan is empty/fails → emit the **all-fail
     attempt summary** (§6.6) as final text, then finish. (The summary text
     is non-empty, so the query loop's own placeholder branch won't
     double-fire.)
4. Mid-switch event ordering: the wrapper must emit the placeholder *before*
   the next upstream's first event so the transcript reads chronologically.
   Tool-call/usage state must not leak between attempts — the next attempt
   starts from a clean `ProviderRequest` clone.

**Query loop impact** (`query/src/lib.rs`): mostly none for mid-chain
failures (the wrapper hides them). The existing placeholder branch becomes
the *last-resort* path for a stream that still ends empty (defensive). The
loop's `retries_left` / stall machinery is unchanged.

**Why the wrapper (not query-loop re-dispatch):** the plan lives inside
`FreeProvider`; the query loop cannot order the chain. Keeping retry inside
the provider means pinned `groq/model` requests (which also route through
`FreeProvider` as `Route::Pinned`) get the same behaviour for free, and the
TUI/CLI code paths stay untouched.

### 6.3 Cooldown model (key vs upstream vs empty)

Three independent cooldown tracks, all reusing/extending existing state:

| Trigger | Level | Action | Default |
|---|---|---|---|
| `429` + `retry-after` | **Key** | `KeyRing.mark_exhausted(idx, retry_after, msg)` (existing) | 3600s quota / 60s rate-limit |
| `401`/`403` | **Key** | `KeyRing.mark_exhausted(idx, 300, msg)` (existing) | 300s |
| `5xx` / Groq `498` | **Upstream** | `CooldownState.record_failure(idx)` — NEW: also count as failure for the FreeProvider circuit breaker | upstream cooldown 45s |
| Empty completion | **Upstream + key** | NEW empty-counter: N=3 consecutive empties → cooldown 60s | 3 × 60s |
| Stream stall (no first byte) | **Upstream** | count as failure toward circuit breaker | 30s timeout (existing) |

Implementation notes:

- The empty counter lives alongside `CooldownState` in `FreeProvider`
  (per-upstream `consecutive_empties: Vec<u32>`; reset on any non-empty
  success). Optionally mirror a per-key empty count into `KeyRing` entries
  (see §6.9) so `/keys health` can show it.
- **Self-healing ordering:** the attempt-plan builders (§existing
  `attempt_plan_*` functions) must skip upstreams in cooldown *before*
  building the plan (today cooldown is checked inside the loop — fine, but
  the empty-cooldown track must be consulted in the same place).
- 5xx upstream cooldown is **brief** by design (user: "cool down the whole
  upstream briefly") so a recovering backend returns to rotation quickly.

### 6.4 Health poller (startup + periodic, zero-token)

New background task (e.g. `crates/core/src/health_poller.rs`, spawned from
`cli/main.rs` / `tui/app.rs` startup; cancels via the existing shutdown
`CancellationToken`):

- **When:** once at startup, then every `health_poll_interval_secs`
  (default **300s**; `0` disables).
- **What:** for each configured free upstream with ≥1 key, probe every key
  using the **existing** `validate_upstream_key()` /
  `query_rate_limits()` helpers (GET to `/v1/models`, 5s timeout; a 1-token
  `chat/completions` confirmation for upstreams whose models endpoint
  doesn't check auth — nvidia, huggingface, openrouter, sambanova, cline).
- **How results are applied:**
  - `401`/`403` → `KeyRing.mark_exhausted(idx, 300, "health check: invalid key")`
  - `429` with `retry-after` → mark exhausted per header
  - `5xx` → upstream-level cooldown (brief)
  - `200` → clear key cooldown / reset failure counters
- **Safety:** stagger probes (don't hammer all providers at once), respect
  `retry-after`, skip providers already in cooldown, never probe providers
  without keys, and abort promptly on app shutdown. Persist results through
  the existing key-ring snapshot persistence so state survives restarts.
- The TUI status bar (`provider:active/total (retry in Xs)`) then reflects
  *live* health instead of only request-driven state.

**Wiring needs:** a public API on `KeyRotatingProvider` to inject external
health results into its `KeyRing` (e.g.
`mark_key_exhausted(index, cooldown_secs, reason)`) or, simpler, the poller
persists `ProviderKeyRingSnapshot`s that the ring already re-reads. Decide
at implementation; both fit existing patterns.

### 6.5 Timing model (the "actually trying vs failing" problem)

The user's constraint: *don't time-cap an attempt that is working; detect
quick failures fast.* The research gives the lever: **empty completions
arrive in ~3s; real generations take longer.**

- **Fast-failure path (empty):** attempts fail in ~3s; the chain walk over
  N upstreams costs ~N×3s worst case. Since success usually lands within the
  first few upstreams, typical total latency stays in the 3–10s band.
- **Slow-path (no first byte):** keep today's `upstream_timeout_secs` (30s)
  hard cap per upstream, and add a **first-byte watchdog**:
  - If an upstream yields **no event within `first_byte_timeout_secs`
    (default 5s)**, fire a **staggered parallel probe** at the next plan
    entry (only for `Route::Auto`; pinned routes stay strictly sequential
    and deterministic).
  - First stream to produce content wins; the loser is cancelled. If the
    original produces content within the window, the probe is cancelled and
    never cost anything.
  - Worst-case latency for the whole chain ≈ 5s + one generation time, not
    `plan_len × timeout`.
- **No global hard cap on a working generation** (per user choice); the
  budget only bounds the fallback walk. If every attempt fails fast, the
  all-fail summary (§6.6) arrives within `~N × (fast-fail time)`.

Config: `first_byte_timeout_secs` (0 = pure sequential, today's behaviour).

### 6.6 All-fail UX + manual override

**All-fail (every upstream/key empty or errored):**

- The `RetryingFreeStream` emits a final **attempt summary** text block:
  `(all free upstreams failed: groq: empty (3s); cerebras: rate limited (429, retry in 45s); nvidia: empty (2s))` + a hint: `run /keys health for key status`.
- Optionally also emit `QueryEvent::Error` / `Status` so the TUI can style
  it as an error, but the transcript must contain the summary (debugging).
- After the whole-chain failure, per decision #11, **do not** silently end:
  surface the summary and (per #10's "retry instead of ending") allow one
  whole-chain retry via the existing `retries_left` mechanism before
  surfacing the summary as final.

**Manual override — `Shift+Enter` keybinding (VERIFIED FEASIBLE):**

- **Verification result (checked `crates/core/src/keybindings.rs` +
  `crates/tui/src/app.rs` + `prompt_input.rs`):** `shift+enter` is already
  a default binding → action `"newline"` (Chat context). **While streaming,
  `"newline"` is a no-op**: both the `handle_keybinding_action("newline")`
  arm (app.rs ~6511) and the hardcoded Shift/Alt/Ctrl+Enter fallback
  (app.rs ~5620) are guarded by `if !self.is_streaming`. So during an
  in-flight request Shift+Enter is currently a **dead keystroke** — a free
  slot with zero conflict.
- **Plan:** keep the `shift+enter` → `newline` binding unchanged (so idle
  multi-line editing is untouched), and make the `"newline"` action branch
  on `self.is_streaming`:
  - **Idle** (`!is_streaming`): `prompt_input.insert_newline()` (today's
    behaviour, preserved).
  - **Streaming**: abort the current attempt and advance to the next
    upstream in the plan (the manual "force-move" override). The abort
    flows through the existing cancellation machinery; the re-dispatch is
    handed to the running query loop (via the shared command queue or a
    dedicated flag the loop drains between turns — see §6.2).
- **Why this works:** `enter`/`submit` is already blocked while streaming
  (same `!is_streaming` guard), so streaming is a distinct, already-gated
  key-handling state. `alt+u` → `cycleFreeUpstream` (app.rs ~6449) is the
  precedent for a Chat-context action that deliberately has **no**
  streaming guard and runs mid-flight.
- **Configurability:** the binding stays in the `keybindings.rs` table, so
  users can rebind/unbind it via `keybindings.json` (AGENTS.md
  compliance — no inline key checks). The new action label
  (e.g. `"retryNextProvider"`) is added to the keybinding cheat-sheet
  action labels in `tui/src/overlays.rs` (~2295).
- **Fallback (not needed, documented):** `Ctrl+Shift+R` is **unbound** in
  the defaults and **not** in `NON_REBINDABLE` (`["ctrl+c", "ctrl+d",
  "ctrl+m"]`), so it is available and user-rebindable if a distinct key is
  ever preferred. The `parse_keystroke` path already handles
  `ctrl+shift+enter`-style chords (tested).

### 6.7 Configuration surface

New options under `settings.json` →
`providers.free.options.routing` (extending the existing `RoutingConfig`):

| Key | Default | Meaning |
|---|---|---|
| `empty_cooldown.max_consecutive` | `3` | Consecutive empty completions before cooldown |
| `empty_cooldown.cooldown_secs` | `60` | Cooldown after the threshold |
| `upstream_5xx_cooldown_secs` | `45` | Upstream-level cooldown after a 5xx/498 |
| `health_poll_interval_secs` | `300` | Poller cadence (`0` = off) |
| `first_byte_timeout_secs` | `5` | Staggered-probe trigger (`0` = pure sequential) |
| `staggered_probe` | `true` | Enable the first-byte staggered probe (auto routes) |
| `fallback_retries` | `1` | Whole-chain retries after all upstreams fail |

All must serialize/deserialize via `serde` with defaults (existing pattern
in `RoutingConfig`), and surface in the settings screen / free-mode dialog
only where cheap (JSON-only is acceptable initially).

### 6.8 File-by-file change plan

| File | Change |
|---|---|
| `crates/api/src/providers/free.rs` | `RetryingFreeStream` wrapper; empty-completion classification; empty counter + cooldown track; 5xx upstream cooldown hook; first-byte staggered probe; all-fail attempt summary; plan builders skip cooled upstreams |
| `crates/api/src/providers/key_rotating.rs` | Optional: public `mark_key_exhausted(index, secs, reason)` for the poller; ensure empty completions pass through untouched |
| `crates/core/src/key_ring.rs` | Optional: per-key empty-count field + persistence; poller-facing API (or reuse snapshot persistence) |
| `crates/core/src/health_poller.rs` (new) | Startup + periodic zero-token poller task; wiring + cancel |
| `crates/core/src/keybindings.rs` | `Shift+Enter` default binding: "retry/advance provider while streaming" |
| `crates/query/src/lib.rs` | Defensive last-resort path only; whole-chain retry on all-empty (respect `fallback_retries`); ensure placeholder/summary text flows to transcript |
| `crates/tui/src/app.rs` | Wire the new keybinding action; status-bar stays as-is (already reads key health) |
| `crates/commands/src/keys.rs` | `/keys health` reflects poller + empty counters; optionally add `/keys probe` (on-demand sweep) |
| `docs/configuration.md`, `docs/commands.md` | Document new settings + Shift+Enter + behaviour |
| `AGENTS.md`/`spec/13_rust_codebase.md` | Update FreeProvider architecture section (routing, cooldowns, poller) |

### 6.9 Test plan

Follow existing fixture patterns (mocked HTTP/streams, **no live API calls**):

1. **free.rs unit tests**
   - Empty stream (zero text/tools, `end_turn`) classified as failure;
     identical re-send goes to the next plan entry.
   - Pinned route (`groq/…`): empty → falls through to remaining entries.
   - Placeholder text emitted before the next upstream's first event.
   - Non-empty `max_tokens` partial output treated as success (no retry).
   - Tool-use turn with tool blocks treated as success.
   - N=3 consecutive empties → upstream cooled for 60s; a success resets the
     counter.
   - `5xx`/`498` → upstream briefly cooled; `429` still key-level.
   - First-byte watchdog: slow stream (delayed first event) triggers a
     staggered probe; fast first byte cancels it.
   - All-fail → attempt-summary text contains one line per attempt.
2. **key_rotating.rs tests**: empty completion does **not** rotate keys
   (but increments the FreeProvider empty counter — tested via free.rs);
   new `mark_key_exhausted` API updates ring state.
3. **health_poller tests**: mocked `validate_upstream_key` results →
   key/upstream cooldown state updated correctly; no probes for providers
   without keys; respects `retry-after` and shutdown token.
4. **query loop integration**: all-empty chain surfaces summary instead of
   silently ending; `fallback_retries=1` re-runs the chain once.
5. **Keybinding test**: Shift+Enter action dispatched only while streaming.

Validation commands: `cargo test --package clawde-api`, `cargo test
--package clawde-core`, `cargo test --package clawde-query`, then
`cargo clippy --workspace --all-targets -- -D warnings` and
`cargo fmt --all`. Manual: `cargo run -- "hi"` (interactive) and
`cargo run -- --print "hi"` (headless); tmux harness per AGENTS.md for
Shift+Enter.

## 7. Open questions / risks

1. **Shift+Enter binding conflict** (§6.6): verify against the prompt-input
   newline binding in `keybindings.rs`; gate to in-flight-only or pick
   `Ctrl+Shift+R` if unusable.
2. **Placeholder-in-transcript clutter:** every failed attempt leaves a
   placeholder line in the transcript (user's explicit choice). Long
   sessions with many failures could get noisy; mitigate by keeping the
   format compact and the summary one-line-per-attempt.
3. **Poller politeness:** zero-token HEAD checks still hit provider APIs;
   must stagger, respect 429 `retry-after`, and never probe during
   cooldown. Worst case it should add no observable latency to requests.
4. **`content_filtered` empty completions:** treated as failures (move on)
   since free tiers filter aggressively — confirm this doesn't mask
   legitimate refusals the user wants to see. (Note `should_fallback` today
   *excludes* `ContentFiltered` errors from falling back; the empty path
   deliberately differs.)
5. **Mid-switch state leaks** in `RetryingFreeStream`: usage/tool-call state
   must reset per attempt; the assistant message assembled by the query loop
   must contain placeholder + final answer as separate blocks.
6. **Interaction with `RandomFailover` / `LatencyBased` strategies:** the
   staggered probe and empty cooldown must compose with non-sequential
   plan orderings (cooldown skips should apply to whichever plan order is
   active).

## 8. Acceptance criteria

1. In free mode, typing `hi` returns a **non-empty text answer** without
   user intervention (verified via `cargo run -- --print "hi"`), even when
   the first upstream returns an empty `end_turn`.
2. Failures are visible in the transcript as placeholders/summary lines but
   never silently end the turn.
3. `/keys health` and the status bar reflect live health (poller results +
   empty/5xx cooldowns) within one poll interval.
4. After ~3 consecutive empty responses from one upstream, subsequent
   requests skip it (60s) and the app prefers healthy upstreams.
5. If every upstream fails, the user sees a one-line-per-attempt summary
   plus the `/keys health` hint — not a bare placeholder.
6. A pinned `groq/…` request that returns empty falls through to the other
   free upstreams instead of dead-ending.
7. All new behaviour is covered by the tests in §6.9; workspace compiles
   clean with `cargo clippy --workspace --all-targets -- -D warnings`.

## 9. Key code locations (reference)

- `crates/api/src/providers/free.rs` — `FreeProvider`, `attempt_plan*`,
  `should_fallback` (~1536), fallback loops (~1700–1800), `CooldownState`,
  `RoutingConfig`/`RoutingStrategy`
- `crates/api/src/providers/key_rotating.rs` — `ExhaustSignal`,
  `KeyRotatingProvider`
- `crates/core/src/key_ring.rs` — `KeyRing`, `mark_exhausted`,
  `next_available`, `statuses`, snapshot persistence
- `crates/query/src/lib.rs` — provider dispatch, empty-placeholder branch
  (~1420–1440), `retries_left`, `continue_or_end!`
- `crates/core/src/keybindings.rs` — keybinding registry (Shift+Enter)
- `crates/tui/src/app.rs` — keybinding dispatch; status bar key health in
  `tui/src/render.rs` (~2510–2613)
- `crates/commands/src/keys.rs` — `/keys` command + `cmd_health`
- `crates/tui/src/free_mode_dialog.rs`, `settings_screen.rs` — routing /
  `disabled_upstreams` UI
- `crates/api/src/providers/free.rs` — `validate_upstream_key`,
  `query_rate_limits` (reused by the poller)
