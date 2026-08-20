# Free-mode routing: architecture analysis, prior art, and scoped improvements

Status: architecture audit and implementation record.
Date: 2026-08-10; implementation update: 2026-08-19.
Scope: `FreeProvider` auto-routing (`crates/api/src/providers/free/`), the
`KeyRotatingProvider` layer (`crates/api/src/providers/key_rotating.rs`), and
the header/quota plumbing that feeds both.

---

## 1. Executive summary

`free/auto` routes each request through an ordered chain of free-tier
upstreams (13 in `FREE_CATALOG`). The chain is **error-driven and reactive**:
on every fallbackable error (429 quota/rate, 401/403 auth, 5xx, timeout, empty
completion, concurrency) the *same request is re-dispatched verbatim* to the
next plan row (`crates/api/src/providers/free/impls.rs:1034-1076`). Within an
upstream, `KeyRotatingProvider` rotates exhausted keys.

The setup is thoughtfully built — Retry-After is honored as a cooldown floor
(`time_extract.rs`), validation errors are deliberately excluded from fallback
(`impls.rs:680-687`, matching the industry rule "never fall back on provider
validation errors"), and capability gating (vision / context window) is done
up-front. The two focus areas versus the industry consensus in 2026 are:

1. **Capacity awareness is deliberately conservative.** Fresh rate-limit
   headers are consulted at dispatch time as a soft demotion signal. Providers
   without usable headers use local sliding-window estimates only when an
   explicit catalog limit is known; ambiguous limits remain neutral, and
   estimates never hard-skip or invalidate a credential.
2. **Mid-stream failures duplicate output.** Content is forwarded as it
   arrives (`impls.rs:1312`). If an upstream emits text then dies, the partial
   text is already visible and the retry replays the whole prompt — the user
   sees the interrupted answer, then the whole answer again.

Both are known hard problems in this space; nobody in the ecosystem has solved
#2 cleanly, and #1 exists only as small-scope experiments and well-argued
feature requests. Nothing "greatly improves" on Clawde's overall design
(agent + multi-key rotation + streaming + failure telemetry all in one
binary); the improvements below would be differentiating rather than catch-up.

### Implementation update

The following audit fixes are now implemented: exact key-slot attribution under
concurrent rotation, bounded key-rotation health checks, persisted health-poller
exhaustion, typed valid/invalid/transient probe verdicts, empty/5xx probe-body
handling, bounded-concurrent health polling, private atomic key-ring snapshots, context-overflow fallback, no replay after visible stream output, and
out-of-band empty-completion retries (retry notices no longer become assistant
text). Conservative header-aware and locally estimated capacity routing is now
implemented; configurable hedge policy and telemetry retention remain open
design work.


---

## 2. Current architecture (what actually happens)

### 2.1 Dispatch plan

`attempt_plan` (`impls.rs:197-229`) builds an ordered list of `(chain_idx,
model)` rows, then applies:

- disabled-upstream filter (`is_disabled_upstream`)
- capability gate (`entry_fits_request`): drops non-vision upstreams for
  image-bearing requests, and any upstream whose documented context window is
  smaller than the request's estimated token count

The default `RoutingStrategy::Auto` orders rows task-first: task-preferred
upstreams lead (`attempt_plan_task`, `impls.rs:322-416`), then the rest of the
catalog in order. Each entry contributes its effective primary model then its
per-upstream `fallback_models` (`plan_rows_for_entry`, `impls.rs:450-457`).
Within the preferred group, ordering is by dispatch success rate then latency
(`preferred_order_key`, `impls.rs:427-443`); a success rate is trusted after
`MIN_SUCCESS_RATE_SAMPLES = 3` dispatches (`impls.rs:34-36`).

### 2.2 Fallback triggers

`should_fallback` (`impls.rs:680-687`) falls through on everything **except**
`InvalidRequest` and `ContentFiltered`. Within a stream (`RetryingFreeStream`,
`impls.rs:852+`), errors re-dispatch via `start_next_plan_entry`; empty
completions (HTTP 200 + zero content) route through `advance_after_empty`
(`impls.rs:1078-1090`), which logs a placeholder notice as a `TextDelta`
(`impls.rs:1356-1363`).

A parallel first-byte watchdog (§6.5) fires at `first_byte_timeout_secs` on
auto routes: it launches a *second concurrent* request on the next non-cooled
plan entry (`impls.rs:1164-1256`) and switches to whichever returns first.

### 2.3 Key rotation

`KeyRotatingProvider` rotates within an upstream when a key is exhausted
(429/401/403/5xx) and marks it cooled down via the `KeyRing` (`key_ring.rs`).
Cooldown is persisted to disk and adjusted across restarts
(`key_ring.rs:329-365`). Both the `KeyRing` cooldown tracks and the
router's 5xx / empty-completion cooldowns persist under
`~/.clawde/empty-cooldown-state/free.json` (`impls.rs:81-92`).

### 2.4 Rate-limit header collection (the unused half)

Three parallel paths already parse provider rate-limit state:

- `query_rate_limits` / `parse_rate_limit_headers`
  (`providers/free/mod.rs:1569,1661-1683`) reads `x-ratelimit-remaining/limit`
  for rpm, rpd and tpm — consumed only by `/keys health`
  (`crates/commands/src/keys.rs:1126`).
- OpenAI-compat streaming emits `StreamEvent::RateLimitHeaders`
  (`openai_compat.rs:986-1010`).
- Anthropic streaming emits the same via `anthropic-ratelimit-*` headers
  (`lib.rs:1240-1258`, `anthropic.rs:159-165`).

The query loop forwards these as `QueryEvent::RateLimitUpdate`
(`crates/query/src/lib.rs:1457-1464`) — used by the TUI for a usage display
and dropped by the runner (`runner/stream.rs:86`). **Nothing reads these
values back into the dispatch plan.** This is the single most valuable
untapped signal already in the codebase.

---

## 3. Prior-art research (August 2026)

### 3.1 Directly comparable free-tier aggregators

| Project | What it does | Relevance |
|---|---|---|
| **[QuotaRouter](https://github.com/Starland9/quotarouter)** — Starland9 | Python library that routes across Cerebras / Groq / Google AI Studio / Mistral / OpenRouter free tiers. Tracks **daily token quotas locally per provider with persistence and midnight reset**, applies rpm throttling, and falls back to the next provider when a daily quota is exhausted. Streaming supported; pluggable quota storage. | The user's exact mental model, implemented at small scope. Uses **declared daily caps + local token counting** rather than header parsing — the pragmatic answer for opaque providers. 2 stars / single maintainer; no agent context, no key rotation. |
| **[recompose #44](https://github.com/recomposesh/recompose/issues/44)** — "quota-aware routing mode" (Opened 2026-07-23) | Unimplemented feature spec: track per-account rpm/tpm headroom and *proactively* route away from accounts approaching limits instead of reacting to a 429. Signals: rate-limit headers, local sliding-window rpm/tpm counting, observed 429s as cooldown, optional user-declared limits. Open questions: selection policy (most-headroom / weighted / threshold-drain), persistence, opaque-provider handling. | Independent confirmation that "proactive quota routing" is the recognized *next* step in this niche, as of a month ago. The spec's design choices map 1:1 onto what Clawde would build. |
| **[litellm-local-config](https://github.com/gaiagent0/litellm-local-config)** | LiteLLM proxy config stacking free tiers (Groq, Gemini, OpenRouter) with a local Ollama/NPU fallback; 429/5xx → 60s cooldown → next provider. | Demonstrates the same two-level (cloud-free + local) fallback idea in a gateway config; not engine-level. |
| **[OmniRoute](https://arjavjain.org/posts/how-to-use-claude-code-with-omniroute-for-free)** (v3.8.49 reviewed 2026-08-03) | Local gateway for Claude Code with routing rules, fallback combos, health checks, quota/cooldown handling, usage analytics and a dashboard. Priority/fill-first routing; documents "fallback does not guarantee identical context / tool-call behavior / continuity across families". | Maturation of the gateway shape (health, cooldown, observability). Explicitly *reactive*: "when a provider fails, reaches a quota, or enters cooldown, route the next request" — no proactive headroom routing. |
| **[claude-code-router](https://musistudio.github.io/claude-code-router/)** (musistudio) | Popular scenario-based router (background/think/longContext/webSearch) with per-scenario fallback chains: `provider,model`. Sequential fallback on HTTP errors, no fallback on validation errors. | Confirms two Clawde design choices: validation errors must not trigger fallback; cheap fast lanes belong to background tasks. Purely reactive. |

### 3.2 The production-router / best-practice consensus

- **[LiteLLM Router](https://docs.litellm.ai/docs/routing)** is the reference:
  cooldowns after failed deployments, ordered fallback ladders, and
  **usage-based routing** (rpm/tpm-aware deployment selection; respecting caps
  during picking). This is the closest production-grade analogue to the
  proactive-routing improvement below.
- **[BEE-30039](https://alivedise.github.io/backend-engineering-essentials/ai-backend-patterns/llm-provider-rate-limiting-and-client-side-quota-management)**
  — client-side quota management "MUST" list: read rate-limit headers on
  *every* response (not just 429s); honor `retry-after` as a *floor*; full-jitter
  exponential backoff and never immediate 429 retry; a client-side token bucket
  mirroring provider enforcement with continuous refill; pre-flight token
  estimation (~4 chars/token heuristic, then precise counts for large requests);
  and distinguish RPM-triggered from TPM-triggered 429s because TPM needs longer
  waits. Clawde already does the Retry-After-floor part; it lacks the rest.
- **[TrueFoundry](https://www.truefoundry.com/blog/llm-failover-load-balancing-provider-outages)**,
  **[routeur.ai](https://routeur.ai/blog/llm-provider-failover-solution)**,
  **[apisrouter](https://apisrouter.com/llm-fallback-architecture-guide)** —
  all converge on the *streaming failover* problem: **you cannot fail over
  transparently once tokens are visible**. Accepted mitigations:
  (a) only re-dispatch *before first byte*, (b) buffer server-side and release
  on completion (gives up perceived latency), or (c) a non-streamed first-token
  liveness probe before committing to a stream. All three also note hedged
  requests *double-quota*: fire only near p95 latency, and cancel the loser.

### 3.3 Verdict

No existing project combines Clawde's total surface (agent loop + multi-key
rotation + streaming + per-task success telemetry). The two proposed
headline improvements would make Clawde an outlier rather than a follower:
QuotaRouter proves proactive local budget counting is tractable; recompose
#44 shows the feature is being requested but is not yet generally shipped;
no gateway in the survey solves mid-stream output duplication, and most
explicitly punt on it.

---

## 4. Flaws found (evidence, ordered by impact)

### F1. Dispatch-time quota awareness is intentionally bounded (RESOLVED)
The chain now consults fresh server observations before dispatch and keeps
local sliding-window estimates for the small set of explicit catalog limits:
Groq 1K requests/day, Cerebras 5 RPM/30K TPM, and SambaNova 20 RPM/200K TPD.
Providers with ranges, model-specific limits, or non-token units such as
Cloudflare neurons/day remain neutral until authoritative response metadata is
available. The estimate is a soft ordering signal, not a hard eligibility gate,
so stale local state cannot strand a provider permanently.

### F2. Mid-stream failure duplicates output (HIGH)
Events are forwarded as produced (`impls.rs:1312`). Any failure after the
first token re-dispatches the full request, so the user sees partial text
then the same answer again. This is the streaming-failover hard case the
entire industry calls unsolved (`section 3.2`).

### F3. Success-rate ordering self-reinforces and outranks quality (MEDIUM)
`MIN_SUCCESS_RATE_SAMPLES = 3` (`impls.rs:34-36`) is low, the rate is not
time-decayed, and ordering applies across all preferred upstreams
(`impls.rs:404-443`) regardless of quality tier. A flaky-but-best upstream
(the "crown jewel" gpt-4o tier, `catalog.rs:66`) is demoted after one bad
day and rarely probed enough to recover; the demotion is sticky because
cooled/failed upstreams dispatch less.

### F4. §6.5 parallel probe double-spends quota (MEDIUM)
Fires on every slow auto-route first byte (`impls.rs:1164-1256`), launching a
second concurrent request on another upstream. Two providers bill for one
answer; the loser is discarded (and its stream is not proactively cancelled),
so partial generations may still bill. The research consensus is to fire a
hedge only near p95 and cancel the loser.

### F5. Context overflow hard-fails instead of falling through (MEDIUM/LOW)
Overflow surfaces as `InvalidRequest`, which `should_fallback` excludes
(`impls.rs:680-687`). The pre-dispatch gate estimates tokens at ~4
chars/token (`impls.rs:284`), which under-counts code, so a request estimated
to fit Copilot's 16K but actually too big dies there instead of reaching a
128K upstream. Note the deliberate design tension: the exclusion is correct
for genuinely malformed requests; it is wrong for context-length overflows.

### F6. Empty-completion notice pollutes output/history (LOW)
"(no response from X — retrying…)" is emitted as a `TextDelta`
(`impls.rs:1356-1363`) — real assistant text that lands in the visible stream
and can enter the conversation history seen by the next turn.

### F7. Pinned requests silently change model mid-flight (LOW)
Once the pinned upstream dies, the remainder of the plan is *other* upstreams'
default models (`impls.rs:466-507`). "Something answered" is not "the selected
model answered". Becomes user-surprising for expensive pins.

### F8. Telemetry persisted indefinitely (LOW)
Success-rate + latency snapshots persist under
`~/.clawde/telemetry-state` without a documented purge, retention, or
opt-out for the routing history.

---

## 5. Scoped improvement proposals

Each proposal lists: what to build, files touched, effort, risk, why it
matters, and relevant prior art. Ordering inside each tier is by
value/effort.

### Tier 1 — the two headlines

#### P1. Dispatch-time quota-aware routing
**What:** Make `attempt_plan` consult per-key headroom before dispatching.

- **Reuse the existing parse.** `parse_rate_limit_headers` already reads
  rpm/rpd/tpm remaining+limit (`free/mod.rs:1661-1683`). Hoist the parsing
  out of the standalone probe into the normal response path and maintain a
  `RateLimitState` per ring slot (same shape as `KeyRing` cooldown, persisted
  like `empty-cooldown-state/free.json`).
- **Plan filter.** After `attempt_plan` builds its rows, deprioritize (or
  skip) any row whose upstream's cached headroom is below a configurable
  threshold or whose `retry_after` is still in the future — instead of
  dispatching and waiting for the 429. Update the cached state on every
  response, not just errors (`section 2.4` plumbing already carries it).
- **Opaque providers.** Free tiers that expose no headers (per recompose #44
  and QuotaRouter) get only explicitly declared request/token windows plus
  local accounting (~4 chars/token estimate, already used by the request
  planner). The estimate is deducted at dispatch, persisted with independent
  window resets, and remains neutral for providers whose limits are unknown or
  expressed in incompatible units.
- **Selection policy.** Start with threshold-drain (route normally until a
  key crosses e.g. 80% or a configurable floor, then let the next key /
  upstream take over), matching recompose #44's simplest policy. Make it
  configurable under `providers.free.options.routing`.
- **Design decisions to confirm before coding:** (a) put the state in
  `KeyRing` vs. a new `QuotaState` next to `LatencyState` — the key-rotation
  loop in `free/mod.rs` needs to stay ring-aligned with the poller
  (`resolve_free_upstream_keys`); (b) whether zero remaining means *skip the
  upstream entirely* or *demote it to last* — skipping risks wasted requests
  only if the cached state is stale, so prefer demotion first, skip as an
  option.
- **Effort:** large (touches core state machine + registry + plan building).
  **Risk:** medium — new persisted state, ring-alignment sensitivity.
  **Prior art:** QuotaRouter (budget model), recompose #44 (spec), LiteLLM
  usage-based routing (production reference).

#### P2. First-byte commit rule for streams
**What:** Bound re-dispatch to the pre-first-byte window so mid-stream
failure can never duplicate visible output.

- Adopt the industry rule: once the first byte has been delivered
  (`first_byte_received`, `impls.rs:1278`), a failure must not silently
  restart the same request. Options, in increasing invasiveness:
  1. **Best balance:** on post-first-byte failure, stop, emit a short
     out-of-band notice ("continued on <upstream>") and hand the user a
     retry affordance, rather than replaying the prompt.
  2. **Buffered first client:** hold the first ~one token / N ms so an
     immediate post-start error is caught before anything visible
     (cheap insurance; does not fix mid-answer failures).
  3. Full buffered release (hold the whole response, stream it only after
     completion) — rejects the point of streaming; do not do this except as
     an opt-in debug mode.
- **Effort:** medium — contained to `RetryingFreeStream::poll_next`.
  **Risk:** low. **Prior art:** TrueFoundry / routeur.ai / apisrouter —
  "failover only before first byte" is the shared conclusion.

### Tier 2 — cheaper, high-value fixes

#### P3. Decay and tier-aware success-rate ordering
Replace the persistent 3-sample rate with a time-decayed EWMA and only reorder
*within* a quality tier (tier order stays catalog-authoritative at
`catalog.rs:63-286`). Raise `MIN_SUCCESS_RATE_SAMPLES` or gate reordering on
recent-dispatch confidence. **Files:** `LatencyState` (defined in
`free/mod.rs`), `preferred_order_key` (`impls.rs:427-443`). **Effort:**
medium. **Risk:** low-medium (telemetry-format change; keep a migration).

#### P4. Restrain the §6.5 parallel probe
Two parts: (a) make it opt-in (default off) or require a configurable
latency baseline near p95 before it fires rather than firing on every slow
first byte; (b) when the parallel probe wins, actively
abort/cancel the abandoned leader stream instead of leaving it to drain.
**Files:** `routing.first_byte_timeout_secs` handling in `impls.rs:1164-1256`.
**Effort:** small-medium. **Risk:** low. **Agrees with:** TrueFoundry (hedge
only near p95, cancel the loser, track both attempts for billing).

#### P5. Fall back on context-length overflows
When an `InvalidRequest` is recognizable as a context-length overflow (provider
error body text) and a later plan row has a strictly larger documented context
window, treat it as fallbackable for that request instead of hard-failing.
Keep the exclusion for every other `InvalidRequest`. **Files:**
`should_fallback` (`impls.rs:680-687`) + error-text classifier (compare to
`time_extract.rs`'s body-scanning patterns). **Effort:** small. **Risk:** low —
misclassification only ever enables one extra dispatch attempt.

#### P6. Out-of-band empty-completion notices
Emit the "(no response…)" message via an `ProviderAttribution`-adjacent
channel (add a dedicated `StreamEvent`/status event) instead of as a
`TextDelta`. Keeps the conversation history clean. **Files:** `impls.rs:1356-1363`
+ query-loop event handling (`crates/query/src/lib.rs:1457` block).
**Effort:** small. **Risk:** low.

#### P7. Surface "model changed" on pinned fallback
When a pinned route falls through to a non-pinned model, emit a
`ProviderAttribution`-style notice so the user knows the selected model was
replaced (mirrors P2's out-of-band notice). **Files:** pinned arm of
`attempt_plan_task` / stream re-dispatch. **Effort:** small. **Risk:** low.

#### P8. Telemetry retention policy
Add a documented retention/TTL (e.g. prune per-upstream snapshots older than a
configurable window; clear on disable) for `telemetry-state` and an opt-out.
**Effort:** small. **Risk:** low.

---

## 6. Suggested sequencing

1. **P6 + P2 option 2** (small, self-contained, immediate UX win).
2. **P5 + P4** (small-medium; remove the two cheap failure modes).
3. **P3** (medium; makes telemetry-based ordering defensible).
4. **P1** (the headline; largest — do after the routing surface changes are
   settled so ring-alignment is only reworked once). P2 option 1 folded in
   here.
5. **P7 + P8** (polish, anytime).

---

## 7. References (accessed 2026-08-10)

- QuotaRouter — https://github.com/Starland9/quotarouter
- recompose #44 quota-aware routing — https://github.com/recomposesh/recompose/issues/44
- LiteLLM routing/load-balancing — https://docs.litellm.ai/docs/routing
- BEE-30039 client-side quota management —
  https://alivedise.github.io/backend-engineering-essentials/ai-backend-patterns/llm-provider-rate-limiting-and-client-side-quota-management
- TrueFoundry LLM failover & streaming failover —
  https://www.truefoundry.com/blog/llm-failover-load-balancing-provider-outages
- routeur.ai — failover and the first-byte boundary —
  https://routeur.ai/blog/llm-provider-failover-solution
- apisrouter fallback architecture — https://apisrouter.com/llm-fallback-architecture-guide
- claude-code-router (musistudio) — https://musistudio.github.io/claude-code-router/
- OmniRoute guide (v3.8.49) — https://arjavjain.org/posts/how-to-use-claude-code-with-omniroute-for-free
- litellm-local-config — https://github.com/gaiagent0/litellm-local-config
- LiteLLM cooldown internals — https://zread.ai/BerriAI/litellm/18-failover-and-cooldown-mechanisms