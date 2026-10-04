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
(`time_extract.rs`), request rejections fall through to the next upstream
rather than failing the turn (`impls.rs`; a provider-specific 4xx must not kill
a chain another upstream can serve), and capability gating (vision / context
window) is done up-front. The two focus areas versus the industry consensus in 2026 are:

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

`should_fallback` (`impls.rs`) falls through on everything **except**
`ContentFiltered` and already-visible stream failures. A malformed/`InvalidRequest`
4xx also falls through: the classifier cannot distinguish a provider-specific
rejection from a genuinely bad request, so the next upstream is tried and the
last error surfaces if the request really is bad. Within a stream (`RetryingFreeStream`,
`impls.rs:852+`), errors re-dispatch via `start_next_plan_entry`; empty
completions (HTTP 200 + zero content) route through `advance_after_empty`
(`impls.rs:1078-1090`), which logs a placeholder notice as a `TextDelta`
(`impls.rs:1356-1363`).

A parallel first-byte watchdog (§6.5) fires at `first_byte_timeout_secs` on
auto routes: it launches a *second concurrent* request on the next non-cooled
plan entry (`impls.rs:1164-1256`) and switches to whichever returns first.

A rate limit (429) **fails over before it waits**: while another plan entry is
dispatchable, the throttled upstream is abandoned immediately so a healthy
provider answers the turn instead of the user waiting out the cooldown. Only
when the whole chain is rate-limited does the walk wait, for the shortest
recovery among the throttled upstreams (the `Retry-After` hint, or a 20s window
floor when none was sent; the live 5xx / empty-completion cooldown remaining,
for an upstream skipped as "in cooldown"), then retry the upstream that recovers
soonest. The recovery **repeats** while `turn_walk_budget_secs` allows, so a
wait that turns out too short self-corrects instead of failing the turn; a wait
that would overshoot the remaining budget is not started. Each upstream keeps a
single candidate entry holding its latest hint, so an upstream whose window
moved out cannot starve a sooner-recovering sibling. This replaces the old
per-upstream rate-limit wait, which paused on a still-throttled provider before
trying the next one. `fallback_retries: 0` disables the whole-chain recovery
too.

A **mid-stream** failure is handled differently from a pre-first-byte one,
because output has already reached the user. `RetryingFreeStream` *continues* the
response on the next upstream instead of replaying it: the committed text
(before the first tool call) is appended as an assistant turn plus a user
instruction to continue — the form Anthropic's 4.6+ docs recommend, since a
trailing assistant prefill is now rejected — and the next upstream resumes from
there. The seam is announced out-of-band as
`StreamEvent::UpstreamContinuation` (rendered as a brief "continuing on …"
note), never as content. This never duplicates or re-plays the visible text.

A tool block is **held** out of the consumer from its first event until it
stops. If the attempt is abandoned mid-call the held block is dropped with it,
so the query loop never sees — and never executes — a half-written call, and the
continuation resumes from the text prefix instead of re-issuing (and thereby
running twice) a call whose arguments were never complete. A stopped tool block
is flushed in order before any later event.

When there is nothing to continue from (no committed text), the failure falls
through to the normal replay-safe failover. When the interruption cannot be
recovered at all — continuation budget spent, or no route left — the error
surfaces and the query loop treats a visible-output stream failure as
non-retryable (`decide::classify_provider_error` → `Recovery::GiveUp`), so it is
not re-issued either; the text/thinking already streamed is committed to the
conversation (`partial_blocks_from_stream`, `query/src/lib.rs`) so history
matches the screen and the next turn has a record of what was said. Continuation
is capped at `MAX_CONTINUATION_ROUNDS` (`impls.rs`) and gated by the same route
availability as the whole-chain recovery.

Every walk is bounded by `turn_walk_budget_secs` (default 240s, `0` disables)
and every same-upstream retry wait reports its countdown as
`StreamEvent::UpstreamRetryProgress` — see §14.

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

**Update 2026-10-02:** `should_fallback` no longer excludes `InvalidRequest`, so
this finding is closed — an overflow (and every other request rejection) falls
through to the next upstream.

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

**Update 2026-10-02:** implemented more broadly than proposed — every
`InvalidRequest` is fallbackable now (not only recognizable context overflows),
because the classifier cannot tell a provider-specific rejection from a bad
request. The credential is not cooled and the same provider is not retried.

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

---

## 8. Does rotating to a second key ever help? (MEASURED 2026-09-27)

`KeyRotatingProvider` responds to a rate limit by benching one key and
dispatching the request on the next. For several major providers the vendor docs
claim the quota is *not* per key:

| Provider | Docs say the quota is keyed to | Quote |
| --- | --- | --- |
| Groq | Organization | "Rate limits apply at the organization level, not individual users" |
| Google | Project | "Rate limits are applied per project, not per API key" |
| OpenRouter | Global | "Making additional accounts or API keys will not affect your rate limits, as we govern capacity globally" |
| Cerebras | Organization | "Rate limits apply at the organization level, not the user level" |
| Mistral | Organization | "API rate limits define how much traffic your Organization can send" |

If true, rotating spends one real request per stored key to rediscover that the
next key is throttled too, and adds load to an account already refusing work.

### The measurement refutes this, at least for Groq

`scripts/probes/limit-scope-probe.py` drives key A to a 429 on model A, then
immediately retries **the same model on key B**, then **a different model on
key A**. Run twice against the real configured Groq keys, identical both times:

```
groq: quota=DISTINCT (rotation works); limit=per-model
      took 30 reqs to 429
      keyB / same model  = success     <- independent quota
      keyA / other model = success     <- per-model, not cumulative
      x-ratelimit-limit-tokens 8000, x-ratelimit-limit-requests 1000
      x-ratelimit-reset-tokens 547ms
```

**Two of the stored Groq keys have independent quotas**, even though the docs
say limits apply "at the organization level". So on Groq the key ring is doing
real work: rotation genuinely escapes a rate limit. The doc claim does not hold
for this configuration, and it is the configuration the feature is used in.

This also confirms `limit_scope: per-model` for Groq independently of any doc
reading: a key exhausted on `gpt-oss-120b` served `gpt-oss-20b` immediately.

### Found while measuring: the NVIDIA catalog entry is dead

`FREE_CATALOG` names `openai/gpt-oss-120b` as NVIDIA's `default_model` and
`NVIDIA_PREFERRED_FREE` picks it first for discovery. NVIDIA's API answers:

```
HTTP 410 {"detail":"The model 'openai/gpt-oss-120b' has reached its end of
life on 2026-09-03T08:00:00Z and is no longer available."}
```

So chain position 3 currently dispatches to a model that has been gone for
about three weeks. `qwen/qwen3-next-80b-a3b-instruct` is 410 as well. Live and
responding on the configured key: `openai/gpt-oss-20b`,
`nvidia/nemotron-3.5-lightning-30b-a3b`, `nvidia/nemotron-3-ultra-550b-a55b`.

This is a separate bug from limit scoping and is **not fixed here** — fixing it
means picking NVIDIA's current free flagship, which needs the discovery path
that already exists (`NVIDIA_PREFERRED_FREE` in `free/discovery.rs`) to be
correct rather than hand-editing the catalog. It is called out because it was
found by the probe, not by reading the catalog.

### Providers that could not be measured

All inconclusive for a stated reason, not a guess:

| Provider | Why |
| --- | --- |
| NVIDIA | Catalog default is EOL (below). With a live model (`openai/gpt-oss-20b`) the drain completed 150 requests without a single 429, so the free-tier ceiling is above that. |
| Cloudflare | No 429 after 400 requests on `@cf/meta/llama-3.2-3b-instruct`, nor after 150 on the catalog probe model `@cf/qwen/qwen3-30b-a3b-fp8`. The documented 300 RPM text-generation ceiling was not reached. |
| Cline | HTTP 402 `insufficient_credits`, balance `-$0.00`. |
| Google, OpenRouter, Mistral, Cerebras, SambaNova, Poolside | Fewer than 2 stored keys, so there is nothing to compare. |

The probe reports INCONCLUSIVE rather than a verdict whenever the key was never
actually seen to return 429, or when the comparison model turned out to be
unusable (404/410). A "success" on key B is only meaningful once key A has
provably been throttled.

### Conclusion

**Rotation is not wasteful. Do not add a per-provider `quota_scope` flag.**

A `quota_scope` field on the cooldown profile was implemented and then reverted.
Reverting was correct, and the measurement is why: the premise was not merely
unproven, it is false for the one provider measurable here. Two further reasons
would have applied regardless:

1. It was wrong for Mistral. Mistral was labelled `per-key` and used as the
   "independent keys" test exemplar while the same doc page said the opposite.
2. The axis is the user's key set, not the provider. Quota sharing depends on how
   many Organizations/Projects the stored keys span — and Google's docs note all
   keys created in AI Studio live in one project, so the shared case is the
   *default*. A per-provider constant would then bench a healthy second account.

Note the Cline 402 result validates an existing design decision: 402 is
deliberately not an exhaust signal (`classify_exhaust`), so a negative balance
does not bench a key or rotate. Without that carve-out this run would have
marked both Cline keys dead over a billing problem.

Re-run with `python3 scripts/probes/limit-scope-probe.py` (consumes free-tier
quota; needs 2+ keys for the provider). Add the provider's model pair to
`MODELS` after checking the live `/models` listing — hardcoded ids rot, which is
how the first groq attempt failed on a retired `llama-3.3-70b-versatile`.

### Found while measuring: Cline free models DO work at 0 credits — with a header

Measured 2026-09-27 against the configured Cline key, balance `-$0.00`.

`https://api.cline.bot/api/v1/ai/cline/recommended-models` returns a `free`
array of 6 models, which is what `fetch_cline_free_models` reads:

```
stealth/pixel-canary                 cline-free/mimo-v2.6-flash
stealth/space-bunny-alpha            cline-free/deepseek-v4.1-flash
cline-free/gemini-3.8-flash          cline-free/muse-spark-1.3-contributor
```

**All six are usable at zero credits**, provided the request carries
`X-CLIENT-TYPE: cline-sdk`. Clawde already sends this in production
(`CLINE_SDK_CLIENT_TYPE`, `openai_compat_providers.rs`). Without it every free
id returns:

```
HTTP 403  "cline-free/deepseek-v4.1-flash is only available via Cline product surfaces"
```

That 403 is an auth wall on *the request shape*, not a statement that free
models are API-unavailable. It is an easy and wrong conclusion to draw.

Measured outcomes, all at `$-0.00` balance, with the header:

| model id | result |
| --- | --- |
| `cline-free/deepseek-v4.1-flash` | 429 `INFERENCE_CAP_ERROR` — "Daily free limit reached ... Try again in 20h 54m" |
| `cline-free/mimo-v2.6-flash` | 429 `INFERENCE_CAP_ERROR` — daily free limit reached |
| `cline-free/gemini-3.8-flash` | 429 `INFERENCE_CAP_ERROR` — daily free limit reached |
| `cline-free/muse-spark-1.3-contributor` | 429 `INFERENCE_CAP_ERROR` — daily free limit reached |
| `deepseek/deepseek-v4.1-flash` (no prefix) | 402 `insufficient_credits` — this is a *paid* id |
| `cline-pass/deepseek-v4.1-flash` | 403 `ENTITLEMENT_ERROR` — not subscribed to ClinePass |
| `cline-cloud/glm-5.3` | 403 — not supported |
| `deepseek/deepseek-v4-flash` (the catalog default) | 500 `empty response content` — passes auth, returns nothing |

Two conclusions:

1. **The `402` carve-out in `classify_exhaust` is correct.** Its comment says a
   Cline negative-balance 402 "does not actually block free-model usage". That
   holds: 402 is a *paid-model* gate, and the free models work regardless of
   balance. An earlier note in this file claimed that carve-out was wrong; it is
   not, and the claim is withdrawn.

2. **The real Cline quota signal is `429 INFERENCE_CAP_ERROR`, and it is
   per-model with a retry time.** "Daily free limit reached on model
   deepseek/deepseek-v4.1-flash ... Try again in 20h 54m" names the model and
   gives an exact reset. This is precisely the model-dependent quota that
   `limit_scope: per-model` exists to express: bench the key for *that* model
   and leave the other five free models usable.

   The open question is whether Clawde currently classifies it that way.
   `classify_exhaust` matches on `ProviderError::RateLimited`, so the outcome
   depends on how the 429 is mapped in the provider adapter — that mapping is
   the thing to check next, and it is the highest-value part of the auto-mode
   work.

Separately, the catalog default `deepseek/deepseek-v4-flash` is unhealthy: it
returns `500 empty response content`, so it authenticates but produces nothing.

### Two concrete bugs this exposes in Clawde's handling

**Bug 1 — the reset time is discarded, so a daily cap retries every 60s.**

Cline returns `429` with a precise reset in the body:

```
"Daily free limit reached on model deepseek/deepseek-v4.1-flash.
 Try again in 20h 54m"
```

`time_extract::parse_time_after_keyword` *can* read that shape (`20h` → 72000s).
But it never sees the body, for two independent reasons:

- `error_handling.rs:154` maps `429 => ProviderError::RateLimited { provider,
  retry_after: None }` — the body is not stored on the variant at all.
- `key_rotating.rs:80` returns the literal string `"rate limited"` for that
  variant, and the `body_from_error` binding is populated only for
  `ProviderError::Other`.

So `estimate_cooldown` receives `"rate limited"` with no body, finds nothing,
and falls through to `default_cooldown_for_signal(RateLimit) = 60`. A cap that
resets in 20h54m is treated as a 60-second limit. The chain will re-hit the same
capped model roughly every minute for the next 21 hours.

Fix: carry the body (or a parsed `retry_after`) on the `RateLimited` variant, and
feed it to `estimate_cooldown` the way `Other` already is.

**Bug 2 — Cline's free models are per-model caps but the profile is per-key.**

Each of the six free models has its own daily counter, but `cline` has no
`limit_scope`, so it defaults to `per-key`. One model exhausting its daily cap
benches the key for *all six*, when five are still usable. Measured: all six are
simultaneously capped here only because the account burned through them; the
granularity is per model.

This is the "model-dependent quota" case from the top of this section, and the
`per-model` machinery already exists — `cline` simply is not opted into it.

Neither bug is fixed here. Bug 1 is a small, well-scoped change to
`parse_error_response` plus the cooldown path. Bug 2 is one JSON field, gated on
the same verification as the other `limit_scope` entries.

---

## 9. Catalog dispatch verification (measured 2026-09-27)

`scripts/probes/verify-catalog-models.py` sends one minimal request per
upstream using the real key, base url and headers, and reports the status. A
429 counts as healthy: the model exists and only the free quota is spent.

| upstream | status | verdict |
| --- | --- | --- |
| poolside | 200 | OK |
| nvidia | 200 | OK — the `nemotron-3.5-lightning` fix works |
| cerebras | 402 | **NO-CREDITS** — see below |
| google | 200 | OK |
| groq | 200 | OK |
| sambanova | 429 | OK (rate-limited = exists) |
| cline | 429 | OK — the `cline-free/` + `X-CLIENT-TYPE` fix works |
| mistral | 429 | OK (rate-limited = exists) |
| opencode-zen | 200 | OK |
| zai | 429 | OK (rate-limited = exists) |
| cloudflare | 200 | OK |
| openrouter | — | no key configured, not checked |

**10/12 healthy.** Both catalog fixes made earlier are confirmed end to end.

### Cerebras is not a free tier

```
HTTP 402 {"message":"Payment required to access this resource. Visit your
billing tab.","code":"payment_required"}
```

Its docs answer the question directly: *"Is there a permanently free tier? No.
The Free Trial is time- and credit-bounded: $5 in credits that expire 30 days
after they're granted."* So the earlier `limit_scope: per-model` work for
cerebras is correct **for an account with credits** and irrelevant for one
without. The upstream will never answer again without payment.

### The gap this leaves

The 402 becomes `ProviderError::Other { status: 402 }`, whose
`recovery_class` is `QuotaExhausted`, so `may_fallback` is true and the chain
falls through correctly. But it is not a *decisive* liveness signal, so the
gate does not suppress it: **cerebras costs one failed request every turn
forever.**

Suppressing it needs the gate to treat "this account has no credits" as
terminal, and the current taxonomy cannot express that safely. Mapping
`QuotaExhausted` to `Unauthorized` would disable the whole upstream on a 402 —
which is right for cerebras and **wrong for Cline**, where a 402 means the
request was routed to a paid model id while the account's free models work
fine. The cline carve-out in `classify_exhaust` exists for exactly this
reason. Distinguishing them needs a per-upstream signal, not a global rule, so
this is left open rather than guessed at.

---

## 10. The 402 carve-out, quota switching, and surfacing it

### Per-upstream credit signal

`402` is ambiguous and the two live cases need opposite answers, so it is now a
per-provider field rather than a global rule:

| upstream | 402 means | `no_credits_is_terminal` |
| --- | --- | --- |
| cerebras | the $5 trial is gone and never refills | `true` — upstream is suppressed |
| cline | a *paid* model id was requested; free models still work | `false` — upstream stays |

Default is `false`: a wrongly-disabled upstream is worse than a wasted request.
Two tests pin the asymmetry, one on the profile data and one end to end
(a cerebras-shaped 402 removes the lane, a cline-shaped 402 does not).

`ProviderProfiles::load()` re-parses the embedded JSON on every call, which is
too hot for the liveness path, so `cooldown_profiles()` caches it in a
`OnceLock`.

### Proactive quota switching

A per-model cap is recorded against the exact `(upstream, model)` pair that hit
it. The plan builder demotes exactly those rows, so the upstream's *other*
models move up — a Cline daily cap on `deepseek-v4.1-flash` promotes
`gemini-3.8-flash` on the same account instead of waiting for the failure to
repeat. Rows are demoted, never dropped, because the cap resets.

This is a stable sort, so it biases ordering without disturbing task
preference or the capacity ranking within each tier.

### Liveness is instance-scoped

The store started life process-global, and the parallel test runner made that
actively harmful: one test's `Gone` silently reordered another test's plan,
producing four unrelated failures. It is now a `FreeProvider` field, like every
other routing signal. Production calls `share_liveness()` at registration so
the TUI and the startup log still see one shared view; a freshly constructed
provider starts empty. The `LIVENESS_LOCK` test mutex is gone as a result.

### Surfacing it in the TUI

`/ctx-viz` renders a tag beside the model for each upstream, next to the
existing cooldown annotations:

```
● Cerebras    gpt-oss-120b  (model blocked)
● Groq        openai/gpt-oss-120b
● Cline       cline-free/deepseek-v4.1-flash  (capped)
```

`retired` / `blocked` are red, `capped` is yellow, and a healthy chain renders
exactly as before. The point is that a chain which quietly gets shorter is
indistinguishable from a bug: "retired" separates *the provider is down* from
*our catalog is stale*.

`upstream_key_health` was not the right home for this — it is per-provider, so
in free mode it yields a single "free" row rather than one per upstream.

---

## 11. NVIDIA: two surfaces, and a correction

An earlier revision of this file concluded that NVIDIA's ceiling was a $30/hour
organization spend cap. **That was an over-generalisation and has been
reverted.** The $30.00/hour figure came from a **brev.nvidia.com** organization
dashboard. Clawde talks to **build.nvidia.com** / `integrate.api.nvidia.com`,
which is a different surface with different billing. Nothing established that
the brev cap applies here, and the catalog hint and `terminal_ttl_secs` that were
added on that assumption have both been removed.

What *is* established, measured 2026-09-27 against the endpoint Clawde uses:

**1. There is an authoritative free-endpoint API, and Clawde already uses it.**
`api.ngc.nvidia.com/v2/search/catalog/resources` marks each endpoint with
`PREVIEW == "true"` (the "Free Endpoint" badge) and `DEPRECATION` for retired
ones. `fetch_nvidia_catalog_free_models` filters on exactly that. A live sweep
returns **41 free endpoints, 0 deprecated**, all on page 0 — so
`NVIDIA_CATALOG_MAX_PAGES = 8` is more than enough and there is no pagination
bug. Free and live today includes:

```
nemotron-3.5-lightning-30b-a3b   (the catalog default — fix validated)
gpt-oss-20b                      (the fallback)
nemotron-3-ultra-550b-a55b   deepseek-v4.1-flash   kimi-k3   glm-5-3
```

This is the authoritative free-model signal. The `build.nvidia.com/models`
HTML filter in the `nimType=anim_type_preview` query string is a view of the
same data; the catalog API is better because it is structured, paginated, and
carries the deprecation flag.

**2. The chat API returns no limit headers at all.** A 200 carries only
`Nvcf-Reqid` and `Nvcf-Status: fulfilled`. There is no proactive signal to
read, which is why the liveness cache is dispatch-fed rather than probe-driven.

**3. Draining 1200+ requests at `max_tokens=2` never tripped anything.** Given
(2), the honest reading is that there may be no request-rate limit on this
endpoint to find — not that a ceiling sits above the probe cap, which is what
this file previously claimed.

**Still unverified:** whether `integrate.api.nvidia.com` has any rate limit at
all, and what status it returns if it does. The docs and forum pages tried are
stale or unrelated. `limit_scope` is left at the conservative `per-key`
default rather than guessed, and the generic 429 / 403 handling already applies
if the limit is ever hit.

---

## 12. NVIDIA does have a rate limit - the earlier probe could not find it

`scripts/probes/rate-burst-probe.py` exists because the sequential probe was
**methodologically incapable** of testing an RPM limit. It waits for each full
round-trip before sending the next, so 200 sequential requests take ~200 seconds
- about 6 requests per minute. It could never exceed a 40 RPM cap, so "no 429
after 200 requests" proved nothing. That was my error, and it is the reason the
profile earlier said "no rate limit found".

Bursting instead, on the same key:

| test | ok | 429 | achieved |
| --- | --- | --- | --- |
| A: 60x `openai/gpt-oss-20b` | 33 | 27 | 897 rpm |
| B: 60x `nvidia/nemotron-3.5-lightning-30b-a3b`, same key, immediately | 11 | 43 | 59 rpm |

**B also got rate-limited, so the limit is shared across models on one key.** A
~60-second sliding window, sized at roughly 31-36 requests:

| settle before burst | ok | 429 |
| --- | --- | --- |
| (cold) | 31 | 28 |
| 30s | 13 | 44 |
| 65s | 36 | 23 |

Full recovery by 65s and partial at 30s places the window at ~60s.

### Cross-checked against documentation - the scope holds, the number does not

A previous revision of this table over-corrected and declared 40 RPM
"unsupported". That was wrong in the other direction, and both halves are
retained here because they are not in conflict:

| claim | source | verdict |
| --- | --- | --- |
| ~40 RPM is the default | a whole cluster of forum threads titled *"Request for NVIDIA Build API Rate Limit Increase (40 RPM → 200 RPM)"*, plus the ~35/window measured here | **supported** as a default, never as a contract |
| shared across models, per key/account | *"hosted NIM endpoints have per-account / per-API-key rate limits"* | **matches the measurement** |
| it is a constant you can rely on | - | **refuted** |

NVIDIA publishes no universal number:

> "The free tier is rate-limit based, with **no universal published RPM**"
>
> "model- and account-specific rate limits that **NVIDIA does not publish as
> one universal quota**"

And a moderator states the decisive part:

> "this usually involves a rate limit that is **dependent on model, use-case and
> the amount of current overall traffic using the same access**"

**So: 40 RPM is a real observed default that our measurement is consistent with,
and it is explicitly not a guarantee.** Two consequences:

- Never hard-code it. The allowance moves with model, use-case and current
  shared load.
- The *"current overall traffic"* clause means the ceiling is partly about
  **shared capacity, not a private per-account bucket**. Adding accounts
  therefore does not multiply headroom the way a per-account quota would - which
  is the strongest argument yet against building multi-account NVIDIA rotation.



### A documented behaviour that matters for routing

> "When free-trial credits on build.nvidia.com are exhausted, requests return an
> **authorization error rather than a throttle**."

Credit exhaustion on this surface can therefore surface as **401/403, not 429**.
The liveness gate reads 403 as `Unauthorized` and suppresses for
`terminal_ttl_secs` (default 6h) — which is too long for a budget that rotates.
That is a real risk, recorded in the profile, and it needs the observed status
before a per-provider TTL can be set honestly.

**Scope verdict: ~per key/account, shared across models.** The consequences:

- Switching between NVIDIA models does **not** escape a rate limit. Per-model
  scope is wrong here, so the profile keeps `limit_scope: per-key`.
- Rotating to a second NVIDIA key only helps if that key is in a **different
  organization**. Two keys in one org share the window, exactly as the OpenRouter
  and Groq cases did. This is the thing to check before adding the brev token
  as a second key - if it lands in the same org it adds nothing.
- The returned 429 is a normal `RateLimited`, so the existing per-key cooldown
  applies and no new error mapping is needed.

## 13. Cloudflare: no free models, and a stale exclusion list

The model listing Clawde needs is already implemented:
`fetch_cloudflare_available_free_models` queries
`/ai/models/search?per_page=100` and keeps models whose `source` is `"hosted"`
(neuron-billed, inside the 10K/day allocation), dropping `"proxied"` ones which
are billed separately and never free.

Probing confirmed the semantics the user described - there is no free/free-tier
model concept, only a unit budget:

```
@cf/qwen/qwen3-30b-a3b-fp8   200 OK
@cf/zai-org/glm-5.2          403 not available on the Workers Free plan
@cf/zai-org/glm-5.3          403 not available on the Workers Free plan
@cf/zai-org/glm-5.3-flash    403 not available on the Workers Free plan
```

**Bug found:** `CLOUDFLARE_PAID_REQUIRED` listed `glm-5.2` but **omitted
`glm-5.3` and `glm-5.3-flash`**, which the current pricing page names and which
both 403. Discovery could therefore pick a model that fails on every dispatch.
Fixed, with the live 403 recorded in the comment.

It is a genuine trap: `glm-5-3` **is** free on NVIDIA, so the cross-provider
name similarity makes it look safe. The list must be per-provider.

---

## 14. Auto mode's failure UX: the silent wait, and the turn that vanished

An audit of `RoutingStrategy::Auto` in a real TUI (100x30 tmux, scratch
`CLAWDE_HOME`, one key, a mock that 429s every chat request) measured the user
visible path end to end. Two defects were structural rather than cosmetic.

### The retry wait was invisible

A pinned/single-entry chain that gets a 429 waits on the same upstream
(`schedule_same_upstream_retry` → `SameRetryDelay`, 20s doubling to 60s, or the
server's `Retry-After` capped at `MAX_RETRY_AFTER_WAIT_SECS` = 120s). Across a
20.5s wait the pane showed 70 distinct states and every one of them was spinner
animation only: the system knew the cause, the retry budget and the exact
window it was waiting on, and none of it reached the screen.

Two consequences, one of them a correctness bug rather than a UX one:

1. A `Retry-After` wait longer than the consumer's stream-stall watchdog (45s
   for `free` in `clawde-query`) produced **no events at all**, so the watchdog
   fired first and the stream was aborted and re-issued mid-wait. The same
   silent-window problem the refusal buffer already guards against
   (`BUFFER_CAP_SECS`), in a path nothing watched.
2. A user could not tell a rate-limit backoff from a hang.

`StreamEvent::UpstreamRetryProgress` is the fix: the provider re-arms its wait
timer in `RETRY_PROGRESS_TICK` (1s) slices, reports the cause and the countdown
each time, and reports `remaining_secs = 0` when the retry goes out so an
indicator clears. `clawde-query` forwards it as
`QueryEvent::UpstreamRetryProgress`; the TUI renders
`nvidia rate limited — retrying in 18s` in the status row (which the notice
alone keeps alive, `should_render_status_row`), and headless runs print the two
edges of a wait rather than a line per tick.

The *pre-stream* retry had the same gap, and it was the common case: when the
first dispatch of a turn fails with a retryable error, `create_message_stream`
slept the backoff out inside its own walk loop, before any stream existed. A
single-key upstream answering 429 — exactly the shape of the audit repro —
therefore waited 20s with a bare spinner and no event able to leave the loop.
That wait is now handed to the stream instead
(`RetryingFreeStream::new_retry_pending`): the returned stream owns the retry,
announces it on its first poll, and dispatches when the deadline passes. The
remaining silent wait is the non-streaming `create_message` walk, which returns
a `ProviderResponse` rather than a stream and so has no event surface to report
on; nothing interactive uses it.

### The exhausted chain left nothing behind

When every upstream fails, the terminal error is a `ServerError` with the joined
upstream failures. `outcome_notification_class` classified
`free-mode upstreams exhausted` as transient because a routine throttle should
not be a persistent red alarm — but this error only arrives after the chain has
spent every retry, so the turn produced nothing and the 5s toast was the only
trace of it. It is now a persistent `Error`, and the CLI pushes the same text
into the transcript as a `SystemMessageStyle::Error` annotation (a
*display-only* row: an annotation never enters `messages`, so an error block can
never be serialized into the next request).

### `turn_walk_budget_secs` bounds the walk

Bounds one dispatch's upstream walk — every attempt plus their same-upstream
retries across the whole chain. Default 240s, `0` disables. When it passes, the
walk stops and the exhaustion message names the budget
(`walk budget of 240s exhausted — stopped trying upstreams`), which
`join_capped_upstream_errors` always keeps because it preserves the last entry.
Enforced at every walk step: `walk_budget_note` gates `start_next_plan_entry`
(the stream and both dispatch loops) and `can_retry_same_upstream`.

The default is deliberately generous — a healthy first attempt never pays it,
and a single honored `Retry-After` is up to 120s — so it bounds the pathological
all-throttled walk (worst case previously ~4.3 minutes of silent spinner)
without cutting a recoverable one short.

### `/status` reports the running configuration

The configuration block printed `Routing strategy: Auto (task-based)` and
`Parallel attempts: 2 (enabled for prompts <50K tokens)` as fixed strings. The
second claim described a gate that does not exist anywhere in the code, and
hedging ships **disabled** (`provider-cooldown-profiles.json` →
`parallel.hedging.enabled: false`), so both lines described a configuration that
was not running. It now reads the same
`providers.free.options.routing` object `build_free_provider` reads and reports
the strategy, the same-upstream retry count, disabled upstreams, the real
hedging state and the walk budget. `resolve_routing_strategy_name` is shared
with `/routing` so the two surfaces cannot disagree.
