# G5 live semantic fixer evidence

Status: implementation locally accepted; live fixer acceptance blocked by the
configured remote model. Do not report this run as a successful G5 acceptance.

Date: 2026-08-10.

## What was tested

- Endpoint: approved remote Ollama host `192.168.1.45:11434` only.
- Model: `qwen2.5-coder:7b`.
- Provider route: Clawde's explicit remote Ollama profile, with isolated mode.
- Smoke: `clawde diagnostics --live --json` against the disposable fixture.
- Request option probe: `tool_choice: "required"` was also tested through the
  existing Ollama provider-options path.

## Results

| Check | Result |
|---|---|
| CLI build | passed |
| Semantic verifier reached | yes |
| Semantic verifier verdict | `fixable` |
| Fresh fixer attempts | 2, bounded |
| Fixture changed | `false` |
| `fix_verified` | `false` |
| Fixer result | fail-closed `runner_error` (no-op) |
| Overall live G5 acceptance | **blocked** |

The same model was then tested directly, outside Clawde, through both Ollama
API routes using a harmless diagnostic tool definition and an explicit required
tool choice:

- OpenAI-compatible `/v1/chat/completions`: HTTP 200, valid JSON,
  `tool_calls=0`, `finish_reason=stop`.
- Native `/api/chat`: HTTP 200, valid JSON, `tool_calls=0`,
  `done_reason=stop`.

Therefore the no-op is not evidence of a Clawde tool-dispatch or permission
bug. The installed model/template returns ordinary text even when a tool call
is required. Prompt strengthening, model-registry metadata, and the existing
`tool_choice` option cannot make this model mutate files.

## Acceptance boundary

The production fixer intentionally rejects a text-only completion and requires
a real mutation in a scoped file before semantic re-verification. This is the
correct behavior: changing it to accept prose would turn a false no-op into a
false positive.

G5 live acceptance is complete only when a future run records all of:

- semantic verdict `fixable`;
- `file_changed=true`;
- `fix_verified=true`;
- `cargo_verified=true`;
- semantic re-verification pass; and
- goal continuation only after that acceptance.

## Next shortest path

Use a remote model that is known to emit OpenAI/Ollama tool calls (from a
configured free provider or a newly installed remote Ollama model), then rerun
the same disposable smoke. Do not use the local CPU Ollama daemon and do not
remove the fail-closed no-op check.

## Latest bounded smoke evidence (2026-08-10; explicit independent budgets)

The current production CLI was rebuilt and run against the approved remote GPU
Ollama endpoint only (`192.168.1.45:11434`); localhost was not used. The run
explicitly configured `semantic_max_attempts=1` and
`semantic_fix_max_attempts=2`.

| Check | Result |
|---|---|
| CLI build | passed |
| Remote semantic verifier reached | yes |
| Production verdict | `fixable` |
| Bounded semantic rounds | 1 configured |
| Bounded fresh-fixer retries | 2 configured |
| Smoke exit status | `1` (acceptance correctly failed closed) |
| `production.attempts` (semantic verifier) | `1` |
| `fix.attempts` (fresh fixer) | `2` |
| `file_changed` | `false` |
| `fix_verified` | `false` |
| `cargo_verified` | unavailable / not reached |
| Full G5 acceptance | **false / blocked** |

The model still produced no accepted mutation. This is retained as a fail-closed
result; no live fixer acceptance is claimed. The offline production-path test
proves that classified parser/application feedback is passed to a fresh second
attempt, without raw model/tool text, and that the retry budget is independent
from the semantic re-verification budget.

## Local validation for the current implementation

- `cargo fmt --all -- --check`: passed
- `cargo test -p clawde-query`: 250 passed, 0 failed
- `cargo check --workspace`: passed
- `cargo build -p clawde-cli`: passed
- `git diff --check`: passed
