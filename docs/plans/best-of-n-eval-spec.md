# Best-of-N eval probe — spec

Status: proposed (not implemented).
Scope: a new `scripts/eval/best_of_n.py` harness that runs the same task N
times in parallel Incus containers, each attempt pinned to a different free
upstream, applies the verify gate inside each container, and reports which
attempts produced passing code and what best-of-N selection would gain over a
single attempt.

Companion to the Incus investigation (docs/plans/katban-selfhost-spec.md
context; see also `docs/katban.md`). This probe is the smallest experiment that
tests the thesis: **container parallelism improves Clawde's code output by
making candidate generation + verification cheap, not by making the model
smarter per token.**

---

## 1. Hypothesis and what would falsify it

- H1: for an agentic coding task, the probability that at least one of N
  attempts (N different free upstreams) passes an independent verify gate is
  materially higher than the single-attempt pass rate of the best individual
  upstream.
- H2: upstreams differ on *which* tasks they pass (diversity), so selection
  value comes from the union, not from one "best" upstream.

Falsification looks like: one upstream dominates (its pass rate ≈ the union's),
or pass rates are so low that N attempts don't move P(≥1 pass), or container
overhead/unevaluable runs eat the signal. The report is designed to show all
three outcomes clearly.

## 2. Non-goals

- No Katban changes. This is harness-only; board-level `attempts: N` is a
  follow-up decision informed by the data.
- No paid providers. All attempts ride the free catalog (freebuff philosophy:
  diversity comes free from the multi-upstream chain).
- No host-mounted directories. Containers are hermetic: binary, auth, and
  fixture files are pushed in; artifacts are pulled out. The agent inside a
  container cannot see the host FS by construction.
- Not CI. Requires a working `incusd` and system containers; it is a
  dev-machine tier like `tui_probe.py` (manual-only).

## 3. Prerequisites

- Incus installed and initialized on the dev host (`incus list` must succeed;
  the probe hard-fails with exit 2 otherwise).
- A base image with python3 and git (default `images:ubuntu/24.04/cloud`;
  override with `--container-image`). The cloud variant ships cloud-init so
  `incus exec` works immediately.
- Built debug binary (`src-rust/target/debug/clawde`) and a seeded auth store
  (`~/.clawde/auth.json`), as with every other eval tier.

## 4. Architecture

```
best_of_n.py
  ├─ reuses run_eval.seed_home()      → fresh temp CLAWDE_HOME (sabotage-safe)
  ├─ reuses run_eval.run_headless()   → stream-json parsing, TTFT, attribution
  │     (invoked with binary/cwd/home INSIDE the container via incus exec)
  ├─ reuses drift_ab.write_pin_settings() + CATALOG_IDS
  │     → disable every other upstream; per-turn upstream_ids filter catches
  │       silent fallthrough, exactly like pinned drift_ab runs
  └─ new: container lifecycle + in-container verify + fs manifest + report
```

Per attempt (attempt = one upstream × one repeat):

1. `incus launch --ephemeral <image> clawde-eval-<runid>` (ephemeral:
   auto-destroy on stop; `finally` stops it regardless of outcome).
2. Seed a temp home on the host with `seed_home()` (copies keys, applies
   pin settings), tar it, `incus file push` into `/root/.clawde`.
3. `incus file push` the clawde binary to `/usr/local/bin/clawde`.
4. Materialize the fixture files into `/root/task/` (same shape as drift_ab's
   `SCENARIO_FILES`: plain file dict shipped in the script).
5. Run the agent: `incus exec <name> -- env CLAWDE_HOME=/root/.clawde clawde
   --print <prompt> --output-format stream-json ...` — the same argv
   `run_headless` builds today, executed through the exec wrapper. Permission
   mode `bypass-permissions` (same rationale as drift_ab: gated tools
   auto-deny in headless mode and the metrics would measure permission
   failures instead of code quality). The container IS the safety boundary
   that makes bypass acceptable.
6. Verify gate, independent of the agent's claims:
   `incus exec <name> -- bash -c 'cd /root/task && <verify_cmd>'` with the
   fixture's timeout. Record exit code + output tail. The agent does not
   choose the verify command; the fixture does.
7. Filesystem manifest: `find /root/task -type f -not -path '*/__pycache__/*'
   | sort | xargs sha256sum` (also pre-run). The report gets
   `fs_diff` = added/modified/deleted paths — what the agent *actually* did,
   including files it never mentioned.
8. Pull the artifacts worth keeping (diff, verify output) into
   `scripts/eval/results/best-of-n-<ts>/<attempt>/`, then stop the container.

Parallelism: attempts dispatch through a `ThreadPoolExecutor`
(max_workers = min(attempts, `--parallel`)). Threads are sufficient — the work
is subprocess I/O, matching the harness's sync style. Attempts on *different*
upstreams naturally spread rate limits; `--stagger-seconds` remains available
for repeats against the same upstream.

## 5. Fixture shape

A fixture is a directory (or inline dict) with:

```jsonc
{
  "name": "formatter-uppercase",
  "prompt": "format_name must uppercase the NAME only — 'Hello, ADA!'.
             Update formatter.py and test_formatter.py so all tests pass.
             You must not touch any other file.",
  "files": { "formatter.py": "...", "test_formatter.py": "..." },
  "verify_cmd": "python3 -m unittest test_formatter -v",
  "verify_timeout_secs": 120,
  "allowed_paths": ["formatter.py", "test_formatter.py"]  // scope check
}
```

Seed fixtures reuse drift_ab's three scenarios (constraint-pin, tangent-resist,
recall) — they already have metric-visible failure modes, pass-as-shipped
tests, and scope constraints. `allowed_paths` turns `fs_diff` into a
`scope_violations` list, the same drift signal but computed from the FS
instead of the transcript. First fixture set should be 3–5 tasks with a spread
of difficulty: one trivial (single-function edit), one multi-file, one
from-scratch module.

## 6. Metrics per attempt

| field | source |
|---|---|
| `upstream_id` (every turn), `model` | stream events via `run_headless`; pinned filter requires ALL turns on the pinned upstream |
| `run_error`, `timed_out` | harness |
| `ttft_ms`, `total_ms`, `cost` | stream events |
| `tool_calls` | tool trajectory length |
| `verify_pass`, `verify_tail` | in-container verify exec |
| `fs_diff`, `scope_violations` | file manifests (before/after) |
| `judge_score` (optional) | `run_judge` median-of-3, `--judge`; advisory only, deterministic verify stays the gate |

A run counts as evaluable iff the agent finished without harness error AND the
verify gate executed (a verify timeout is an evaluable failure — "verify timed
out" is a quality datum, not missing data). Provider flakes/exclusions are
counted and reported separately, like drift_ab.

## 7. Report

`scripts/eval/results/best-of-n-<ts>.json`, schema
`clawde-best-of-n.v1`, same spirit as drift_ab's report:

```jsonc
{
  "schema_version": "clawde-best-of-n.v1",
  "tag": "...", "image": "...", "attempts": 3, "repeats": 2,
  "records": [ /* per-attempt records, artifacts dir referenced */ ],
  "per_task": {
    "formatter-uppercase": {
      "attempts": [ { "upstream": "groq", "verify_pass": true, "total_ms": 41000, ... } ],
      "any_pass": true,
      "picked": "groq",            // selection policy: first pass, fastest pass tie-break
      "passing_upstreams": ["groq", "cerebras"]
    }
  },
  "aggregate": {
    "single_attempt_pass_rate": { "groq": 0.5, "cerebras": 0.33, ... },
    "best_of_n_pass_rate": 0.83,     // union: task counts if ≥1 attempt passed
    "uplift": 0.33,                  // best_of_n − best single upstream
    "unevaluable": 1,
    "diversity": "per-task winners differ on 2/5 tasks"
  }
}
```

The three lines that decide the thesis: `best_of_n_pass_rate`,
`uplift`, and whether `passing_upstreams` varies per task. stdout prints a
compact table (task × upstream verify matrix) plus the aggregate block.

Exit codes follow drift_ab: `0` comparable data produced (read the report for
direction), `2` everything unevaluable (infrastructure). There is no exit 1 —
a low uplift is a *finding*, not a failure.

## 8. Budget and rate limits

N attempts × R repeats × T tasks headless runs. With defaults (N=3, R=2, T=4)
that is 24 agent runs per invocation. Each is pinned to one upstream, so
per-upstream burst exposure is R×T runs — modest, and `--stagger-seconds`
paces repeats. Multi-key rotation still applies per upstream. The report
records cost (free tiers: informational) and unevaluable counts so a
rate-limit-storm run is visible rather than silently skewing medians.

## 9. Failure modes and guards

- **Thinking-only completions (found in Phase 2 smoke, 2026-09-06)** —
  reasoning models on free tiers sometimes spend the whole reply in a
  thinking block: the stream shows attribution but zero `text_delta`, usage
  reports 0 output tokens, and the session JSON holds a thinking-only
  assistant message. The harness scores this as `run_error` (unevaluable
  flake) via `is_empty_completion` — never as a verify pass. The same
  prompt usually succeeds on retry; the same binary+home works on the host,
  so this is a model/stochasticity issue, not a container issue.
- **Fixtures must fail pristine** — the formatter-uppercase gate originally
  shipped passing tests, so a do-nothing agent scored a verify pass. Fixed:
  the shipped `test_content` now asserts the TARGET behavior, so only real
  work can turn the gate green. Rule for every new fixture: verify must
  fail on the untouched tree.

- **Silent pin fallthrough** — the drift_ab lesson: `Route::Pinned` falls
  through quietly. Guard: `disabled_upstreams` for all others AND per-turn
  `upstream_ids` filter; non-pinned runs are excluded and counted
  (`pin_excluded_runs`).
- **Container leak on crash** — ephemeral instances die on stop; `finally`
  always stops. Name prefix `clawde-eval-` makes orphans findable
  (`incus list clawde-eval-`).
- **Stale binary** — probe refuses to run if the debug binary is older than
  the newest source mtime? No — too clever; instead print the binary's
  `--version` build stamp in the report, matching how campaign.py treats
  baseline/candidate binaries explicitly.
- **Free-tier truncation quirks** (Groq max_total_tokens amputating
  `<task_context>`): same exposure as drift_ab; the pinned-filter +
  all-turns-served check covers it.
- **incusd unavailable** — exit 2 with the exact command that failed.

## 10. Build order

1. **Round trip** (Phase 1 — DONE, verified live 2026-09-06):
   `scripts/eval/best_of_n.py` + `test_best_of_n.py` (offline tests). Verdict:
   all phases pass on `images:ubuntu/24.04/cloud`; **stream-json line
   flushing survives `incus exec` with true interleaving** (timestamps prove
   per-line forwarding, not buffer-at-exit). Findings baked into the probe:
   - `incus file push` needs `--create-dirs` (no `--force` flag); the home is
     pushed as a tar stream piped through exec stdin instead.
   - The debug binary does NOT run on a clean cloud image: it needs
     `libasound.so.2` (voice stack's ALSA dep). The probe now has a
     `binary_deps` phase that installs distro-keyed packages into the
     container only (`libasound2t64` on ubuntu 24.04) and re-checks `ldd`.
     Clean-room release checks should test the binary BEFORE provisioning.
   - `--keep` leaves a normal (non-ephemeral) instance for hand inspection.
2. **Sequential best-of-N** (Phase 2): N pinned upstreams × R repeats ×
   fixture set, in-container verify, fs manifests, report. Answers H1/H2
   without parallelism.
3. **Parallel dispatch** (Phase 3): ThreadPoolExecutor + judge tier + optional
   `results.jsonl` append (`--results`) so summarize.py trends pick it up.

Phase 2 is the decision point: if uplift is real, promote to a Katban
per-card `attempts: N` design (container-backed) as a separate spec; if not,
the container round trip and verify-in-container machinery still serve the
multi-distro and clean-signal eval tiers already identified.

## 11. Results — first full runs (2026-09-06)

Three full 3-task × 3-upstream × 1-repeat runs (27 attempts), reports in
`scripts/eval/results/best-of-n-{1788710560,1788712657,1788714002}.json`.
Headline: **H1 (uplift) unmeasurable this round — 19/27 attempts (70%) died
to free-tier infrastructure, not model quality. H2 (diversity) confirmed.**

| run | upstreams | evaluable | passes | infra deaths |
|---|---|---|---|---|
| 1 | groq, google, zai | 3/9 | 2 | 30s timeout ×2 (gemini first byte), rate-limit ×3, empty-completion flake ×1 |
| 2 | groq, google, zai | 3/9 | 1 | rate-limit ×6 (back-to-back attempts, same accounts) |
| 3 | cerebras, nvidia, mistral | 2/9 | 2 | cerebras 402 quota ×3, mistral model-tier auth ×3, nvidia conn ×1 |

Findings that survive the noise:

- **Same model, different host, different outcome.** gpt-oss-120b via nvidia:
  2/2 verify passes (86s, 47s). Via groq: 0/3 evaluable (1 thinking-only
  flake, 2 verify fails — one broke `test_stats.py`'s import, one produced a
  near-miss uppercase). Serving stack (quantization, the documented groq
  max_total_tokens truncation quirk) matters as much as model choice.
- **Diversity confirmed**: per-task winners differ — formatter: zai
  (glm-4.7-flash, slow but correct: 66s TTFT, 314s total) and nvidia;
  quotes: groq, google, nvidia. stats-median: nobody (always ran last, after
  quota burned). No upstream got a full evaluable column, so the union beat
  every member — but n=1 repeats can't size the uplift.
- **Zero scope violations in 8/8 evaluable attempts.** Containment is not
  the bottleneck; task completion is.
- **Position-in-sequence correlated with quota death** — stats-median, always
  last, never got a fair shot. Future runs should interleave task order.
- **Gate integrity caveat**: fixtures let the agent edit its own tests, so a
  test-rewriting agent could game the gate. For selection integrity, hidden
  golden tests beat agent-editable ones.
- **Operational side-discoveries**: the user's cerebras key now returns 402
  (payment required) and the mistral key cannot serve `mistral-large-2512`
  (subscription tier) — both dead upstreams in the real free chain, found by
  the harness.

Protocol fixes before re-measuring H1: drop the two dead upstreams, raise
stagger to 60s+, repeats ≥ 3, interleave task order, and add a retry-once
pass for rate-limited attempts (counted honestly as retries).

## 12. Relationship to existing tiers

- Same isolation philosophy as every eval run: fresh temp `CLAWDE_HOME` from
  copied keys, no real-state pollution — pushed one level deeper (the whole
  filesystem is fresh, not just the home).
- `run_headless`/`seed_home`/`run_judge` keep this a thin script: the new code
  is container lifecycle + verify + manifests + report, not another stream
  parser.
- Offline tests (`test_eval.py` style) cover manifest diffing, scope
  violations, report aggregation, and the pin filter — no incus required, so
  `python3 -m unittest discover scripts/eval -p 'test_*.py'` stays green on
  machines without Incus.
