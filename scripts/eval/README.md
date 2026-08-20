# Clawde eval harness

Headless evaluation for how well Clawde actually responds, designed to catch
regressions and drive improvements to the free-provider chain, routing, and
prompting. Runs the **real binary** (`--print --output-format stream-json`)
against the **real provider stack**, so it measures what users experience —
streaming latency, upstream attribution, fallback behavior, tool trajectory,
and answer quality — not a mocked adapter.

## Quick start

```bash
# Build once, then evaluate a fixture:
python3 scripts/eval/run_eval.py --fixture scripts/eval/fixtures/catalog-order
python3 scripts/eval/run_eval.py --fixture scripts/eval/fixtures/fallback-behavior

# Force the free chain to fall through (first configured upstream's keys are
# replaced with invalid placeholders; the chain must recover via a later one):
python3 scripts/eval/run_eval.py --fixture scripts/eval/fixtures/fallback-behavior \
    --sabotage huggingface

# Arbitrary prompt, pin a specific upstream for determinism:
python3 scripts/eval/run_eval.py --prompt "What is the catalog order?" \
    --model free/nvidia/meta/llama-3.3-70b-instruct

# Before/after comparison for a change:
python3 scripts/eval/run_eval.py --fixture scripts/eval/fixtures/catalog-order --tag before
python3 scripts/eval/run_eval.py --fixture scripts/eval/fixtures/catalog-order --tag after

# LLM-as-judge tier: grade the run's answer with a rubric via a second headless
# run (deterministic assertions stay the gate; judge score is advisory). Pin the
# judge to a strong upstream for stable scores:
python3 scripts/eval/run_eval.py --fixture scripts/eval/fixtures/fallback-behavior \
    --judge --judge-model free/groq/meta-llama/llama-4-scout-17b-16e-instruct

# Trend report over all recorded runs (TTFT, cost, score — by tag/fixture/upstream):
python3 scripts/eval/summarize.py

# TUI-tier probe (tmux): asserts the transcript renders, the streaming spinner
# shows, the key-ring footer appears, and the attribution badge renders:
python3 scripts/eval/tui_probe.py

# Offline harness tests (no binary, credentials, or provider calls):
python3 -m unittest discover scripts/eval -p 'test_*.py'

# Baseline-versus-candidate campaign (runs offline tests first, then each live
# fixture against both binaries and writes one campaign.json report). The
# default plan mode permits read-only inspection tools without allowing edits:
python3 scripts/eval/campaign.py \
    --baseline .eval-baseline/src-rust/target/debug/clawde \
    --candidate src-rust/target/debug/clawde \
    --baseline-cwd .eval-baseline/src-rust \
    --candidate-cwd src-rust \
    --permission-mode plan \
    --repeat 2
```

## Campaign runner

`campaign.py` is the controlled feedback loop for improving Clawde. It runs
local harness tests before provider calls, executes the same fixture suite
against a baseline and a candidate binary, supports repeated samples, and
writes all artifacts below one campaign directory. It returns:

- `0` when all fixtures have enough evaluable runs and the candidate stays
  within the configured pass-rate and score-drop gates;
- `1` for an evaluable quality regression;
- `2` when provider failures, timeouts, missing reports, or offline-test
  failures prevent a trustworthy comparison.

The default manifest is `scripts/eval/campaign.json`. Configure fixture-level
`repeats`/`judge`/`sabotage`, global `min_evaluable`, and gates such as:

```json
{
  "gates": {"max_pass_rate_drop": 0.0, "max_score_drop": 0.1}
}
```

Infrastructure failures are never silently treated as quality scores. Use
`--skip-offline-tests` only when the local harness has already been validated
separately. Campaign runs default to `--permission-mode plan`, which is
appropriate for read-only fixtures such as the bundled suite; explicitly opt
into another mode for a fixture that intentionally edits files. Baseline and
candidate workspaces are inferred from conventional `target/debug/clawde`
paths, or can be supplied with `--baseline-cwd` and `--candidate-cwd`. This is
important: each binary must inspect its own checkout, not the candidate's
source tree. The campaign runner does not apply patches or make commits; the
coding agent remains responsible for implementing a candidate change.

## Isolation (never touches real state)

Every run seeds a fresh temp `CLAWDE_HOME` from a copy of `~/.clawde/auth.json`.
Real key-ring cooldowns, sessions, and `free-state` are never read or written.
The copied keys are the same accounts, so tokens/cost accrue as usual — but no
state pollution leaks back into the user's real key ring.

- `--auth-file PATH` — seed from a different auth store (e.g. a CI test store).
- `--keep-home` — keep the temp home for inspection.
- `--results PATH` — write the trend index somewhere other than the default
  `scripts/eval/results/results.jsonl`; useful for CI and parallel jobs.
- `--no-results` — write the per-run report but do not append a trend index.
- `--sabotage <upstream>` — replace that upstream's keys with invalid
  placeholders (>= 8 chars so the resolver's placeholder guard lets them
  through) and auto-pin the run to that upstream's default model. The pinned
  route tries the dead upstream first, fails, and the chain must fall through
  to a later one — so `upstream_id != sabotaged` on every run is the
  deterministic proof of a working fallback (asserted via the auto-appended
  `not-upstream` check).

## Assertions (`expected.json`)

promptfoo-style weighted assertions; a run passes when the weighted score is
>= 1.0 (every assertion passes) unless the fixture sets `threshold` (e.g.
0.85 for the catalog-order fixture, so a single omitted entry doesn't fail
the gate while a collapsed enumeration does).

| type | checks |
|---|---|
| `contains` / `icontains` | substring in the answer (case-insensitive variant) |
| `not-contains` | substring absent |
| `regex` | pattern matches (case-insensitive) |
| `starts-with` | answer starts with value |
| `min-length` | answer length >= value |
| `mentions-upstreams` | counts catalog upstream ids in the answer (`min` hits) — facts source-derived from `catalog.rs` |
| `tool-used` / `not-tool-used` | whether the agent invoked a tool by name |
| `tool-sequence` | ordered tool trajectory: `value` is a list of tool names; default asserts it appears as an ordered subsequence (the canonical "locate then read" pattern), `mode: "exact"` asserts the full sequence incl. repeats |
| `tool-order` | `value` is a `[A, B]` pair — A must fire before B somewhere in the trajectory |
| `max-tool-calls` / `min-tool-calls` | step-count gate on the total number of tool rounds (repeats included) — catches a run that flails in a loop |
| `tool-count` | `value` is a tool name; `min`/`max` bound how many times it fired |
| `upstream-present` | `provider_attribution` recorded a composite upstream |
| `not-upstream` | run was served by an upstream other than `value` — auto-appended with weight 3 when `--sabotage` is used, the deterministic proof the chain fell through |
| `similar` | cosine similarity between the answer and a golden text >= `min` (default 0.5), via `scripts/eval/embeddings.py` — tries the free Hugging Face inference API, falls back to deterministic token-overlap similarity when the embedding endpoint is unreachable (sandbox DNS whitelists often block it) |
| `no-error` | run completed without a provider/agent error |

Each entry: `{"type": ..., "value"?: ..., "weight"?: 1, "min"?: ...}`.

## What a run records (report.json + results.jsonl)

Per run: time-to-first-token (ms, from the first `text_delta`), total wall time,
response chars, the upstream that actually served (`upstream_id`), model,
`retries`, `fallback_used` (the model-switch signal, not the upstream-fallback
signal — use `not-upstream` for that), cost, the tool trajectory, verify
results, and the assertion-by-assertion score. With `--judge`, the report and
`results.jsonl` also carry `judge_score` (0-1, rubric-graded by a second
headless run against the fixture's rubric; weak judge models sometimes emit
malformed scores, so `run_judge` retries with a correction hint and the parser
accepts `0-10`, percentage, and bare-decimal forms).

`results.jsonl` is append-only for trend tracking — `summarize.py` turns it
into a per-tag / per-fixture / per-upstream trend report with a per-run tail,
so a release that silently degrades TTFT or answer quality trips a visible
signal. It also records `judge_model` (so judge scores are comparable across
runs) and `tool_calls` (step-count trend).

`summarize.py --regression` compares the last N runs of each fixture against
the older baseline and flags judge-score / assertion-score drops beyond a
margin (default 0.15); exit 1 when a regression is detected.

Exit codes: `0` pass, `1` ran but assertions failed (blocks the pre-commit
gate), `2` could not be evaluated (provider error, timeout, empty completion —
warns but does not block). `--threshold` on a fixture sets the weighted-score
bar (catalog-order uses 0.85).

## Where the data comes from

Before a live run, the harness validates fixture assertion schemas and
checks that `catalog_facts.json` matches the current `FREE_CATALOG` source
hash and parsed IDs/models. Regenerate facts after catalog edits with
`derive_catalog_facts.py`; stale facts fail clearly instead of silently
scoring against an obsolete provider order.

The binary's `--output-format stream-json` already emits `text_delta`,
`tool_start`/`tool_end`, `provider_attribution` (upstream id, model, retries,
fallback_used), `verify`, and `result` (cost, usage) events. This harness only
timestamps each line and parses them; no new Rust is needed for the content
tier. Rust-side observability added alongside this harness:

- `Message.turn_meta` (`upstream_id`, `started_at`, `completed_at`) persisted on
  assistant messages — visible in JSONL sessions and indexed into `sessions.db`.
- Per-message `cost` (`MessageCost`) now populated by the query loop from the
  turn's cost delta; the SQLite index stores it (previously always NULL).
- Stop hooks receive `upstream_id`, `model`, `elapsed_ms`, `cost_usd`,
  `fallback_used`, `retries` on `HookContext` — a ready-made feedback channel
  (e.g. append every turn to an eval log).

## Implemented tiers

- **LLM-as-judge tier** (`--judge`): G-Eval-style rubric grading with the free
  provider as a second headless run. Deterministic assertions stay the
  authoritative gate (per `live_smoke.rs` philosophy); the judge score is
  recorded in the report and `results.jsonl`. The judge is pinned to
  `DEFAULT_JUDGE_MODEL` (groq `openai/gpt-oss-120b`) so scores are comparable
  run-to-run; override with `--judge-model`. The score is the **median of up
  to 3 parses** (the free judge is noisy — a single run can score an identical
  answer 0.0 to 0.85), with corrective retries for unparseable output. A
  fixture can opt into a hard gate via `judge.min_score` — when the median
  score is below that floor the run fails even if deterministic assertions
  pass (never trips when the judge could not parse, `score: null`). A truly
  reliable judge needs a paid provider; free models are flaky (empty
  completions / unparseable scores are recorded as `judge_score: null` and
  never block).

  Calibrate `judge.min_score` with `calibrate_judge.py`, which grades stored
  good vs degraded responses (from `results/*/report.json`) and reports the
  two bands. The `catalog-order` fixture is calibrated: good enumerations
  median 0.2–0.35, refusals 0.0 and 8/14 collapses 0.1, so `min_score: 0.15`
  sits between them — it catches a 12/14 answer that scores 0.917 on the
  deterministic assertions (above the 0.85 threshold) but 0.06 on the judge.
- **Trajectory assertions** (`tool-sequence`, `tool-order`, `max-tool-calls`,
  `min-tool-calls`, `tool-count`): gates on *how* the agent got there — the
  ordered tool trajectory and step count, not just the final text. The
  `trajectory` fixture exercises the canonical Grep-then-Read pattern.
- **Semantic similarity** (`similar` assertion): embedding cosine similarity
  against golden text, via `embeddings.py` — free Hugging Face inference API
  first, deterministic token-overlap fallback when the sandbox DNS blocks
  the embedding host.
- **TUI tier** (`tui_probe.py`): tmux-driven probe asserting the transcript
  text renders, the streaming spinner appears, the key-ring footer shows, the
  attribution badge (`⤷ groq · $0.00`) renders under completed turns, and no
  error banner appears. Requires `tmux` and a built binary.
- **Feedback capture** (`hooks/record_feedback.py`): a Stop hook appending each
  turn's `HookContext` (upstream, model, latency, cost, fallback, response
  excerpt) to `scripts/eval/results/hooks.jsonl`. Wire it in settings.json
  under `config.hooks.Stop` (see the script header). Stop hooks fire on
  streaming (free-provider) turns too — not just the Anthropic accumulator
  path — so per-turn evidence works for every provider.
- **Trend tracking** (`summarize.py`): pass rate, TTFT median/max, cost,
  score, and judge means — overall and by tag / fixture / upstream, plus a
  per-run tail. The JSONL index uses file locking so concurrent eval jobs do
  not interleave records.
- **Offline harness tests** (`test_eval.py`, `test_campaign.py`): validate
  fixture schemas, source parsing, identifier matching, event parsing, judge
  parsing, JSONL persistence, campaign aggregation, regression gates, and the
  real subprocess deadline/pipe-draining behavior without making provider
  calls.

## Reliability notes

The content runner drains stdout and stderr concurrently, enforces the timeout
while the process is still running, and kills the child process group on
expiry. Judge retries use separate session IDs so each score is independent.
The TUI probe uses a unique tmux session per process and submits prompts with
Ctrl-M, matching Clawde's multiline prompt behavior.

## Roadmap (not yet implemented)

- **Pre-commit gate for TUI tier**: wire `tui_probe.py` into `.githooks/pre-commit`
  (it is currently manual-only because it needs a display + tmux).
