# Katban per-card `attempts: N` — design spec

Status: **Phases 1–3 implemented** (host tier, sequential ladder): structured
executor output (`runner.rs::AttemptOutput`, stream-json attribution), pin
filter + empty-completion guard, rotation (§6), early promotion, per-attempt
matrix on the card, CLI `board attempts [N]` / `board attempts-upstreams`,
`/katban` parity, web meta + card matrix. Phase 2's dead-upstream skip is in
(`cooling_free_upstreams()` reads the persisted cooldown snapshot the free
chain writes; `ladder_pins` ranks cooling upstreams after healthy ones).
Phase 3's container tier is in (`board.rs::ContainerRuntime`,
`container.rs` incus plumbing, `scope.rs` FS-manifest scope gate,
`runner.rs::run_one_card_container`): `board runtime incus` + `board card
scope <ID> [PATHS...]`, ephemeral container per attempt with in-container
verify, manifest-diff scope checking, artifact tar pull, env-error
provisioning semantics. Phase 4 (parallel racing) remains proposed.
Companions: [best-of-n-eval-spec.md](best-of-n-eval-spec.md) (the measured
experiment this promotes), [katban.md](../katban.md) (current board behavior),
[katban-selfhost-spec.md](katban-selfhost-spec.md) (§12 agent execution model).

Author: user + agent.

---

## 1. Why (the measured case)

The best-of-N eval (2026-09-06, 36 attempts, 3 repeats × 3 tasks × 4
upstreams, `results/best-of-n-1788718046.json`) measured what multiple
attempts on different free upstreams buy:

- **Best-of-N = 9/9 task-slots passed; best single upstream = 8/9** (zai,
  88.9%). The one miss was rescued by a different upstream. Uplift is small
  per-slot but structurally reliable: no single free upstream covers every
  task, and the union does.
- **Per-task winners differ on 3/3 tasks** — diversity is the mechanism, not
  a smarter model.

Measured upstream profiles (pass rate · median pass time · notes):

| upstream | pass | p50 time | notes |
|---|---|---|---|
| nvidia (gpt-oss-120b) | 7/9 | 71s | fast pick; 2 verify-FAILs |
| zai (glm-4.7-flash) | 8/9 | 166s | reliable pick; slow |
| groq (gpt-oss-120b) | 1/9 | 156s | 44% thinking-only empty-completion flake |
| google (gemini-2.5-flash) | 0/9 | — | rate-limited all run; fixed in harness, unmeasured |

Design implication: attempts should be **spread across model families**
(same model on different hosts behaves differently — groq vs nvidia proved
that), with selection by an **agent-independent gate**, and the picked
attempt chosen by a speed/reliability policy.

## 2. Principle: what changes and what must not

- **Opt-in per board.** `attempts` defaults to 1 = today's behavior exactly.
- **Selection is never the agent's opinion.** The existing verify gate
  (`verify.rs::run_gate`) decides which attempt passes. The gate runs per
  attempt, in that attempt's tree.
- **Host-exec default.** Attempts spawn the same headless clawde in worktrees
  as today. The container tier (§8) is opt-in and separate.
- **Slot accounting is honest.** An attempt is a real agent process; it
  consumes a `parallel_cap` slot. `attempts: 3` with `parallel_cap: 3` means
  one card at a time can run its three attempts — never 9 concurrent agents.
- **Free-first.** Attempt diversity comes from the free catalog's model
  families (freebuff philosophy). No paid-provider assumption anywhere.

## 3. Data model

`Board` gains (serde camelCase, defaults preserve current files):

```rust
/// Run each card N times (N different free-catalog model families) and
/// promote the first attempt that passes the verification gate — fastest
/// passing attempt wins review. 1 = today's single-attempt behavior.
#[serde(default = "default_attempts")]
pub attempts: u32,          // default 1, clamp 1..=5

/// Upstream ids attempts rotate over. Empty = derive from the free catalog
/// (first N keyed entries with distinct model_family, spec §6).
#[serde(default)]
pub attempt_upstreams: Vec<String>,
```

`Card` gains:

```rust
/// Per-attempt outcomes, order = attempt index. Recorded even when the
/// card overall fails, so `card show` explains what each model did.
#[serde(default)]
pub attempts: Vec<AttemptOutcome>,

#[serde(default)]
pub picked_attempt: Option<usize>,   // index into attempts; None = none passed
```

```rust
pub struct AttemptOutcome {
    pub upstream: String,      // free-catalog id the attempt was pinned to
    pub model: String,         // full model id actually requested
    pub served_upstream: Option<String>, // attribution: what actually served
    pub verify_passed: Option<bool>,     // None = attempt never reached gate
    pub elapsed_ms: Option<u64>,
    pub scope_violations: u32, // FS-diff out-of-scope count (container tier)
    pub error: Option<String>, // rate-limit / empty-completion / crash
}
```

## 4. Runner changes (`runner.rs`)

1. **Executor contract**: `CardExecutor::execute` gains the model pin and
   returns structured output:

   ```rust
   fn execute(&self, work_dir: &Path, prompt: &str, model: Option<&str>)
       -> Result<AttemptOutput, String>;
   ```

   `AttemptOutput` carries the response digest plus the parsed
   `--output-format stream-json` attribution (`upstream_id`, `model`,
   `retries`, `cost_usd`, token usage). The runner switches from plain
   `--print` to stream-json parsing — reusing the event shapes the eval
   harness already validates (`run_eval.parse_stream_events` is the reference
   implementation; the Rust side reads the same events natively).

2. **Attempt dispatch**: for a card with `board.attempts = N`, the runner
   spawns up to N attempts **sequentially per card** (v1 — see §10 for
   parallel). Each attempt k uses upstream `attempt_upstreams[k %
   len]`. A card occupies one `parallel_cap` slot while ANY of its attempts
   runs; total concurrent agents across the board is still ≤ parallel_cap.
   (v1 keeps it simple and quota-friendly; per-card parallel attempts is a
   later, opt-in knob.)

3. **Early promotion**: after each attempt finishes, run the verify gate in
   that attempt's worktree. First pass wins:
   - promote the card to Review immediately (current finalize path),
   - tear down the remaining attempt worktrees (they are per-attempt
     worktrees, same `.worktrees/` machinery, named
     `katban/<slug>/attempt<k>` … resolved via the existing per-card branch
     naming plus an attempt suffix),
   - record all completed outcomes + `picked_attempt` on the card.

4. **All-attempts-failed**: the card goes to Failed with a composed result
   listing each attempt's error — the existing `auto_retry` machinery then
   applies unchanged (transient errors requeue, user errors don't; a requeue
   re-runs the full attempts:N ladder).

5. **Pin honesty** (the eval's hardest-won lesson): `Route::Pinned` falls
   through silently. After each attempt, compare attribution `upstream_id`
   against the pin. A mismatch marks the attempt `error: "pin fell through:
   served by X"` and it does not count as a pass — even if its verify
   happened to pass, its identity is wrong for the diversity policy.
   One bounded retry of a rate-limited attempt (classify via error text:
   `rate_limited` / `retry after`), then move to the next attempt.

6. **Empty completions**: attribution present but zero text + zero tool
   calls + zero output tokens = provider flake (`is_empty_completion`
   semantics from the harness). Mark the attempt failed, don't feed it to
   the gate — a no-op must never pass a pass-as-shipped fixture, and real
   cards have those too (unchanged trees skip the gate today).

## 5. Selection policy

- **Gate**: verify pass AND zero scope violations (when the container tier
  provides FS manifests; host tier records git-diff scope only, see §8).
- **Pick**: among passing attempts, the **fastest** (min `elapsed_ms`).
  Rationale from the measured data: nvidia's 71s vs zai's 166s is a real
  user-visible difference, and both are reliable on their winning tasks.
- **Record, don't discard**: every attempt's outcome stays on the card. The
  web UI shows the per-attempt matrix (same shape as the eval's stdout
  table), and `picked` is visually marked. A reviewer can override the pick
  by promoting a different attempt's tree before merge (the picked attempt's
  worktree is the one finalize would commit; override = re-point to another
  attempt's tree — only possible pre-merge, worktrees of unpicked attempts
  are kept until card merge/trash).

## 6. Upstream rotation defaults

When `attempt_upstreams` is empty, derive N upstreams at spawn time:

1. Free-catalog order, filtered to upstreams with keys (the auth-store probe
   the registry already does).
2. **Distinct `model_family`** first: the measured diversity is across
   families (glm vs gpt-oss vs gemini), not just hosts. Fill remaining slots
   with distinct hosts of the same family if needed (groq+nvidia gpt-oss is
   still real diversity — serving stacks differ measurably).
3. Dead upstreams (auth-failed / quota-402 / tier-403 classifications from
   the key-ring state) are skipped — the eval's cerebras/mistral findings.
4. Fewer available upstreams than N: run min(N, available) attempts; never
   reuse the same upstream twice within one card's ladder unless families
   are exhausted.

## 7. Settings surface

- CLI: `clawde katban board attempts <N>` (1..=5, default 1) and
  `clawde katban board attempts-upstreams <id> [<id>...]` (empty = auto).
- `/katban board attempts <N>` slash parity; Alt+G menu row.
- Web meta line gains `attempts N` next to `cap`/`retry` (the
  `#meta` template already renders these board knobs).
- Card JSON and the board API include `attempts`/`pickedAttempt` so the web
  UI can render the matrix without new endpoints.

## 8. Container tier (opt-in, phase 3)

The eval harness (`scripts/eval/best_of_n.py`) already proves the mechanics:
hermetic container, seeded home, binary push, task push, in-container verify,
sha256 FS manifests, tar artifact pull, ephemeral teardown. Phase 3 wires the
same primitives behind a board-level flag:

- `clawde katban board runtime incus` (default `host`).
- With it, each attempt runs inside an ephemeral `images:ubuntu/24.04/cloud`
  container: worktree pushed in, agent runs with `bypass-permissions`
  (the container is the safety boundary), verify gate runs in-container,
  FS manifest diff gives real `scope_violations`, artifact tar attached to
  the attempt record.
- The binary-deps lesson applies: the probe must provision the container
  (distro-keyed packages) or the attempt dies on `libasound.so.2`. The
  dependency phase is part of attempt setup, and its failure is an
  environment error, not a card failure (mirrors the verify gate's
  install-failure skip semantics).
- Host tier keeps git-diff-based scope only; FS-manifest scope is the
  container tier's advantage.

## 9. Interplay with existing features

- **`parallel_cap`**: unchanged semantics — counts concurrent agent
  processes. An attempts:N card holds one slot for its whole ladder.
- **`auto_retry`**: retries a fully-failed card (all attempts failed with
  transient errors); each retry re-runs the ladder.
- **Feedback loop**: review comments requeue via `send_feedback_to_agent`;
  the follow-up run re-runs attempts:N with the composed feedback prompt.
  Attempts are per-run state: `card.attempts` resets each run.
- **auto-review**: runs once, on the picked attempt's diff only.
- **Verify master switch** (`board verify off`): with the gate off there is
  no selector, so attempts:N degrades to "first attempt wins" and the card
  carries the full matrix for human review. Documented, not an error.

## 10. Phases

1. **Phase 1 — host tier, sequential ladder** (§4.1–6, §7): structured
   executor output, pin filter, empty-completion guard, selection, records,
   CLI/web surface. Tests: executor trait fakes for attribution shapes,
   early-promotion ordering, slot accounting with attempts:N, pin-fallthrough
   exclusion, dead-upstream skip.
2. **Phase 2 — measured knobs**: latency-aware pick already in §5; add
   optional per-board `attempt_upstreams`; key-ring health integration for
   dead-upstream skipping (the registry's `key_ring_summaries` already
   exposes this).
3. **Phase 3 — container tier** (§8) behind the runtime flag, reusing the
   best_of_n probe's container primitives as a Rust module in `clawde-katban`
   (the Python harness remains the eval-side reference). **Implemented:**
   `crates/katban/src/container.rs` (launch/push/exec/manifest/deps/teardown
   + pin-settings hardening), `crates/katban/src/scope.rs` (allowlist
   matcher), `Board::runtime` + `Card::scope_paths`, and
   `runner.rs::run_one_card_container` wiring the ladder to the container
   flow with the scope gate in the selection order.
4. **Phase 4 — per-card parallel attempts** (opt-in): attempts race in
   parallel containers, first verify-pass cancels the rest. Only after the
   sequential ladder is proven; multiplies quota pressure.

## 11. Risks / anti-goals

- **Quota multiplication**: attempts:N multiplies free-tier token burn by N
  on every card. Mitigated by family rotation + dead-upstream skip +
  early promotion (stop as soon as one passes). Admin tier has no budget
  caps by decision, but the board meta line must make the multiplier visible.
- **Gate gaming**: the agent can edit its own tests in two of the eval
  fixtures — the same is true of real cards. Selection integrity ultimately
  wants hidden golden tests; until then the picked attempt is reviewable and
  override-able (§5), and auto-review reads the picked diff.
- **Not for every card**: trivial cards don't need N attempts; the knob is
  per board, and per-card override (`--attempts` on card add) can come later.
- **Never** make attempts:N the default, never let the container tier become
  required, and never select by model self-report — only the gate decides.
