# Seamless Per-Key Rotation — Session Goals (2026-09-27)

Dated record of what this session was actually trying to achieve, including the
distinctions that made it non-obvious. Written so the reasoning survives the
code, which will move.

## The request

> key1 runs out of RPM and gets a message about waiting or too many requests. I
> want that request to be silent to the user and immediately use key2 until it
> triggers the same rate limit, and seamlessly continue working with ANY other
> nvidia key.

(45 seconds was offered as an illustration, not a constant. See
[The 45-second number](#the-45-second-number) — this matters.)

Stated that plainly it sounds like a retry loop. It is not, and the difference
is where all the nuance lives.

## Goal 1 — "silent" means the failure never becomes visible content

A rate limit can surface two ways, and only one of them is acceptable:

- **Rejected before the response** — `do_streaming` sees a non-2xx and returns
  `Err` *before* handing back a response object
  (`crates/api/src/providers/openai_compat.rs`, the status check that precedes
  the stream). The caller never receives a stream, so there is nothing to
  render.
- **An error event mid-stream** — the response opened at 200 and the failure
  arrives as an item in the stream. By then the UI has already committed to
  that turn: text may be on screen, and the user reads a raw "too many
  requests" regardless of how gracefully the fallback chain above it handles
  the failure afterwards.

The first is the whole requirement. This is a property of the *transport layer*,
not of the fallback chain above it: no amount of cleverness in `FreeProvider`
can un-print a stream that already started. Any new provider adapter added to
the free chain must reject non-2xx before returning the response, or it quietly
breaks the user-visible promise this session was making.

## Goal 2 — same model, not just same provider

The free chain's ordinary fallback **changes the model**: pin
`nvidia/nemotron-...`, watch it fail, and the chain moves to groq's default,
then sambanova's. That is the right behaviour when a model is genuinely
unavailable, and it is the wrong behaviour here.

The user asked for one model and holds several NVIDIA accounts precisely to get
more of *that* model. Falling through to groq when a third nvidia key is still
healthy would be a different kind of lie than showing an error: the answer would
come back fine, and the user would never learn they got someone else's model.

So rotation has to happen **below the plan**. `KeyRotatingProvider`
re-dispatches the identical `request.model` on the next credential and only
surfaces an error when the entire pool is spent — at which point the chain above
is free to do what it normally does. The hand-off is invisible; the exhaustion
is not.

The test that pins this is
`pinned_nvidia_route_rotates_keys_without_switching_model`: a real
`FreeProvider`, a pinned nvidia route, key0 rate-limited, key1 healthy, and a
second healthy upstream whose attempt log must stay **empty**. The assertion that
matters most is the one about groq never being consulted.


## Goal 3 — the pool's throughput is the sum of its keys

Three accounts at 40 requests/minute each should buy roughly 120 requests per
minute on a single model. That is the entire point of holding several
credentials, and it is a claim about a *session*, not about a single request —
so the test drives a session.

`sustained_load_cycles_through_every_key_without_a_failure` gives the three keys
deliberately unequal budgets (`1, 2, 3`) so they dry up in sequence, then asserts:

- every turn within the combined budget is served, with no error reaching the
  caller;
- the model string is identical on every dispatch, including the ones that
  followed a hand-off;
- load actually spread — no key sat idle while another was hammered;
- **benched keys are not retried while cooling.** Cheap to skip and expensive in
  practice: re-trying a cooled key makes every turn pay for a 429 it cannot win,
  turning a silent hand-off into per-turn latency for nothing;
- only a fully exhausted pool surfaces `RateLimited`, carrying the earliest
  retry hint, and it returns *immediately* rather than sleeping out the window
  (`skip_recovery_loop`, because the chain above handles retry at its own level).

## Goal 4 — get the cooldown *scope* right, not just the cooldown

Rate limits differ in what they actually constrain, and clawde models that as
`limit_scope` (`"per-key"` | `"per-model"`, `crates/api/src/providers/free/mod.rs`).

NVIDIA is `per-key` — it inherits the default. That is deliberate, not an
oversight: the limit is an account-level ceiling and the chat API returns no
limit headers at all, so a 429 tells you about the *account*, and benching that
key for every model is the honest reading. Cerebras, by contrast, is
`per-model`: one model's bucket says nothing about another's.

The direction of the error is worth remembering because it is asymmetric.
Over-scoping (treating a per-model limit as per-key) benches a healthy key for
everything over one model's complaint — throughput lost for no reason.
Under-scoping re-tries a limit that will refuse again. When adding an upstream,
the safe default is `per-key`, and moving to `per-model` needs evidence.

## The 45-second number

The mock in `key_rotating.rs` uses `RPM_WINDOW_SECS = 45`. **That number is
illustrative and exists nowhere in production.** It came from the phrasing of
the original request, which explicitly used 45 seconds as an example.

The real cooldown comes from, in order: the error's parsed `retry_after`, the
HTTP `Retry-After` header, text extracted from the response body, then the
per-signal default (60s rate limit, 3600s quota, 300s auth).

This distinction is load-bearing here, because this repo has already been burned
on exactly it. The catalog carries the honest position — NVIDIA publishes no
per-model rate numbers for this endpoint, so a printed request count would be a
guess dressed up as a spec. `40 RPM` is a plausible-looking figure that keeps
creeping back into docs and comments as though it were fact (see the sequence
`drop the 40 RPM claim` -> `restore the 40 RPM figure with its proper caveats`).
Keep it in prose as an illustration of *why* rotation matters; never let it

## Goal 5 — a bug found on the way, and why it was fixed where it was

`tool_routing_gate_demotes_prose_prone_upstream` was failing in `cargo test`,
unrelated to any of the above, and it failed identically on a clean tree.

`ToolDialectState::load()` reads a persisted tally from the developer's real
`~/.clawde/free-state/tool-dialect.json`. That file held 21 structured samples
for groq from actual usage. The test recorded 3 prose samples and expected
demotion; 3 of 24 is 12%, the gate wants 60%, so the routing gate correctly did
nothing and the test blamed the routing gate.

Two things made this worth fixing properly rather than locally:

1. **The test was reading the developer's live routing history into its
   assertions.** Same class of defect AGENTS.md already calls out.
2. **It was overwriting it on the way out.** `cargo test` was destroying real
   user state, silently, on every run.

The tempting fix — give that one test a `TestHome` guard — is the wrong shape.
The existing guards work by setting `CLAWDE_HOME`, which is process-global, so
they only protect the tests that remember to opt in, and they race the ones that
don't on the parallel runner. A guard you have to remember is not a guard.

So the fix moved down a level: `Settings::state_dir()` is now the single redirect
for mutable runtime state (`free-state/`, `empty-cooldown-state/`,
`telemetry-state/`, `capacity-state/`), reusing the same harness detection the
`settings.json` guard already used, so those redirects cannot drift apart. It
resolves to `config_dir()` in production — **no migration, no data movement.**
Every reader and writer was routed through it, including the two in
`crates/commands/src/status.rs` that would otherwise have read a different
directory than the writers.

`tests_resolve_runtime_state_to_a_scratch_directory` is the regression guard.

## What was deliberately not done

- **No hardcoded 45s, no hardcoded 40 RPM.** Nothing in production was tuned to
  a number that came from the prompt.
- **No model switch on key exhaustion.** Deliberately the opposite: a key
  hand-off keeps the model, and only a spent pool tells the chain above.
- **Nothing deleted.** The prose-demotion gate, the multi-upstream fallback and
  the recovery loop all still work as before; this session made them provable
  and left them in place.
- **No attempt to make NVIDIA's limit knowable.** It publishes nothing and sends
  no headers. The design accepts a 429 as the only oracle and reacts to it,
  instead of pretending to schedule around a number nobody has.

become an input to a timer.
