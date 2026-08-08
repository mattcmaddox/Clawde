# Clawde Audit Spec — Smarter Agent, Less Work

**Status:** Draft spec, pending implementation
**Date:** 2026-08-06
**Audience:** Clawde contributors and architects

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Current State Assessment](#2-current-state-assessment)
3. [Competitive Landscape](#3-competitive-landscape)
4. [Research Findings — Cutting-Edge Techniques](#4-research-findings--cutting-edge-techniques)
5. [Interview Decisions — Summary](#5-interview-decisions--summary)
6. [Architecture Vision — Unified Core, Multi-Surface](#6-architecture-vision--unified-core-multi-surface)
7. [Phase 1 — Execute-and-Verify Loop (P0)](#7-phase-1--execute-and-verify-loop-p0)
8. [Phase 2 — Multi-Model Smart Router (P0)](#8-phase-2--multi-model-smart-router-p0)
9. [Phase 3 — Persistent Project Memory (P1)](#9-phase-3--persistent-project-memory-p1)
10. [Phase 4 — Spec-Driven Development Mode (P1)](#10-phase-4--spec-driven-development-mode-p1)
11. [Phase 5 — Model Comparison / Ensembling (P2)](#11-phase-5--model-comparison--ensembling-p2)
12. [Phase 6 — Background Daemon & Watch Mode (P2)](#12-phase-6--background-daemon--watch-mode-p2)
13. [Phase 7 — Headless CI Mode (P2)](#13-phase-7--headless-ci-mode-p2)
14. [Cross-Cutting — Trust Gradient & Risk Scoring (All Phases)](#14-cross-cutting--trust-gradient--risk-scoring-all-phases)
15. [Cross-Cutting — TUI Improvements (All Phases)](#15-cross-cutting--tui-improvements-all-phases)
16. [Implementation Plan — Ordered Steps](#16-implementation-plan--ordered-steps)
17. [Test Strategy](#17-test-strategy)
18. [Open Questions & Future Research](#18-open-questions--future-research)

---

## 1. Executive Summary

Clawde is a Rust TUI coding agent with 50+ providers, a 13-upstream FreeProvider chain
with key rotation, empty-completion recovery, health polling, auto-compaction, hooks,
sub-agent delegation, and a rich TUI. It already reduces work compared to raw API
clients.

However, competitive research and user interviews reveal **four high-impact gaps**
that prevent Clawde from reaching the next tier of usefulness:

1. **No execute-and-verify loop** — The agent writes code but never runs tests,
   linters, or typecheckers. The user must manually verify every change.

2. **No model routing intelligence** — Despite having 13 free upstreams and 50+
   providers, Clawde uses them for fallback only, not for intelligent task routing
   (e.g., "use DeepSeek for code, Llama for tests, Gemini for reasoning").

3. **No persistent project memory** — Every new session is a blank slate. The user
   re-explains architecture, conventions, and project context.

4. **No spec-driven workflow** — The agent executes ad-hoc prompts with no
   structured planning phase, leading to architectural drift and rework.

**User's core mandate:** "Make the agent actually make LESS work for the user."

**User's constraints:**
- Everything MUST work with free providers. No paid-only features.
- The agent should be smarter automatically, not require more user configuration.
- Configurable overrides should exist for power users (TUI dialogs), but defaults
  must be sensible zero-config.

---

## 2. Current State Assessment

### 2.1 What Clawde Does Well

| Capability | Status | Notes |
|---|---|---|
| FreeProvider chain | Excellent | 13 upstreams, key rotation, empty-recovery, staggered probing, health polling |
| Auto-compact | Complete | Token-budget keep, debounce, circuit breaker, update-style iterative summaries |
| Keybinding system | Excellent | Configurable presets (default/vim/emacs), full rebindability |
| Sub-agent delegation | Good | AgentTool, TeamTool, coordinator mode |
| TUI richness | Excellent | 50+ overlays/dialogs, context viz, diff viewer, session branching, model picker |
| Provider breadth | Excellent | 50+ providers, OpenAI-compat factory, OAuth support |
| ACP protocol | Good | JSON-RPC 2.0 for editor integration, registry template |
| Slash commands | Good | 80+ commands with arg completions |
| Hooks system | Good | 30 lifecycle events, pre/post tool use hooks |
| Execute-and-verify loop | Implemented | VerifyPolicy, 3 sandbox modes (direct / git worktree / container), verify box in TUI, settings picker |
| Plan mode | Basic | Enter/exit plan mode tools exist, but not deeply integrated into workflow |

### 2.2 Key Gaps

| Gap | Severity | Evidence |
|---|---|---|
| No execute-and-verify loop | Fixed | VerifyPolicy implemented (Phase 1) — auto-runs tests/lints after writing turns, feeds failures back for auto-fix up to `max_retries`, renders a verify box. Sandboxes: direct, git worktree, container. |
| No multi-model routing | Critical | FreeProvider chains upstreams for fallback only. No task-based model selection. |
| No persistent project memory | High | Memory system exists (`memdir`, auto-dream) but is session-scoped and not deeply integrated. |
| Plan mode is shallow | High | `/plan` toggles read-only tools. No structured plan generation, no plan-vs-execution decomposition. |
| No model comparison/ensembling | Medium | Single model per request. No voting, no best-of-N, no EnsLLM-style comparison. |
| LSP support limited | Medium | Only Rust via rust-analyzer. No TypeScript, Python, Go, etc. |
| No CI/headless mode | Medium | TUI-only. Can't run in CI pipelines for automated PR reviews or scheduled tasks. |
| Plugin marketplace incomplete | Low | marketplace.rs exists but discovery/install flow not complete. |

### 2.3 Architecture Strengths for These Features

- **`LlmProvider` trait** — Clean abstraction over 50+ providers. Adding a "router provider"
  that implements this trait and delegates to child providers is natural.
- **`FreeProvider` impl** — Already a composite provider with fallback logic,
  cooldowns, latency tracking, and routing strategies. Can evolve into the smart router.
- **`QueryConfig` / `ContinuationPolicy`** — Already supports pluggable continuation
  modes (StopPolicy, GoalPolicy). Adding a VerifyPolicy is straightforward.
- **`Tool` trait** — Tools already have permission gating and input/output schemas.
  Verification tools (run tests, run linter, etc.) fit the existing pattern.
- **`FileHistory` / `FileDiff`** — Already tracks session file changes. Can power
  the diff review in the verify loop.
- **`memdir` / `auto_dream`** — Existing memory infrastructure to build on.
- **`RoutingConfig` / `RoutingStrategy`** — Already supports sequential, random,
  latency-based routing. Can be extended with task-based routing.

---

## 3. Competitive Landscape

### 3.1 Direct Competitors (CLI/TUI Coding Agents)

| Tool | Language | Novel Approach | What Clawde Should Learn |
|---|---|---|---|
| **OpenCode** | Go (BubbleTea) | LSP for 20+ languages, 75+ providers, Wasm SQLite sessions, Copilot passthrough auth | LSP integration for code intelligence; zero-dependency session storage |
| **Pi** (Armin Ronacher) | TypeScript | Sub-1K token system prompt, self-extending tools (agent writes its own extensions), tree-structured session branching | Ultra-low context tax; lazy skill loading; agent self-modification |
| **Codex CLI** (OpenAI) | Rust | OS-level sandboxing (Landlock/Seatbelt), `--full-auto` mode for headless CI, cross-surface unified client | Sandboxed execution for verify loops; headless CI integration |
| **Goose** (Block/Moxie) | Rust | MCP-native, YAML "Recipes" for portable workflows, adversarial reviewer sub-agents | Recipe system for repeatable automation; safety reviewer sub-agent |
| **Crush** (Charmbracelet) | Go | Bash-native config (`crushrc`), shared workspace collaboration, Agent Skills open standard | Declarative config in shell syntax; skills as first-class citizens |
| **Aider** | Python | Git-native pair programming, tree-sitter repo maps, automatic clean commits | Tree-sitter for codebase understanding; git as the interface |
| **Cline / Roo Code** | TypeScript (VS Code) | Role-based modes (Architect/Code/Debug), governance-first approvals, repo indexing | Mode specialization; progressive trust model |

### 3.2 Key Differentiators Clawde Can Exploit

1. **Rust performance** — Faster startup, lower memory, better TUI responsiveness than
   any TypeScript/Python/Go competitor. This matters for the verify loop (instant
   feedback).

2. **FreeProvider scale** — 13 free upstreams is unmatched. No competitor has this
   breadth of free-tier access. Multi-model routing across free providers is a
   unique capability.

3. **Single binary** — No Node.js, Python, or Docker required. This makes headless
   CI mode and daemon/watch mode much more deployable.

4. **Existing hook system** — 30 lifecycle events is deeper than most competitors.
   Hooks can verify, validate, and transform at every agent phase.

---

## 4. Research Findings — Cutting-Edge Techniques

### 4.1 Test-Time Compute Scaling

**Source:** Snell et al. 2024, Paglieri et al. 2025, AgentTTS

**Key insight:** Dynamically allocating more compute at inference time (via
thinking phases, best-of-N search, self-refinement) can outperform scaling raw
model parameters.

**Application to Clawde:**
- Separate planning from execution: use a reasoning model for the "what to do"
  phase, a fast model for the "do it" phase.
- The verify loop is a form of test-time compute: the agent invests extra
  inference cycles to validate and fix its output.
- Best-of-N: run the same prompt on 2-3 free models, pick the best output.
  Clawde's 13 upstreams make this affordable.

### 4.2 Executable Verification Loops (ReVeal)

**Source:** Jin et al., Microsoft Research Asia, 2025 (arXiv:2506.11442)

**Key insight:** Multi-turn generation-verification cycles where Turn K generates
code and Turn K+1 writes tests + runs them + parses errors dramatically boosts
Pass@1 rates.

**Application to Clawde:**
- After every `FileWrite` or `FileEdit`, auto-trigger a verification turn.
- The verification turn has access to: run tests, run linter, run typechecker,
  parse error output, and feed it back to the model.
- The model sees "your edit caused these 3 test failures" and fixes them.
- Repeat up to 3 times silently. If still failing, surface to user.

### 4.3 Spec-Driven Development (SDD)

**Source:** Piskala et al., 2026 (arXiv:2602.00180v1)

**Key insight:** Specifications (not code) should be the primary human-editable
artifact in the AI era. Three levels: Spec-First, Spec-Anchored, Spec-As-Source.

**Application to Clawde:**
- New `/spec` command: agent generates a structured specification before writing
  any code.
- Spec includes: requirements, edge cases, data models, acceptance tests.
- Human reviews and approves the spec (pause point).
- Agent implements against the spec, running acceptance tests from the spec.
- Diff between spec and implementation is auto-checked.

### 4.4 Multi-Model Ensembling (EnsLLM)

**Source:** Mahmud et al., 2025 (arXiv:2503.15838)

**Key insight:** Running the same prompt on multiple models and using AST-based
similarity (CodeBLEU) + behavioral differential analysis produces better code
than any single model.

**Application to Clawde:**
- Run code generation on 2-3 free models in parallel.
- Compare outputs via AST structure similarity.
- Run test suite against the consensus candidate.
- Show user the best result (not all results).
- This is functionally a "best-of-N" mode that costs nothing extra on free tiers.

### 4.5 Code-as-Harness (Meta)

**Source:** Ning et al., 2026 (arXiv:2605.18747)

**Key insight:** Code is not just the OUTPUT of AI agents, but the OPERATIONAL
SUBSTRATE through which agents think, plan, invoke tools, and maintain state.

**Application to Clawde:**
- Let the agent write temporary Python/shell scripts to inspect directories,
  parse configs, or verify logic before making permanent edits.
- These "scratchpad scripts" run in a sandbox, not polluting the project.
- This is already partially supported via BashTool, but could be formalized as
  a `ScratchpadTool` with auto-cleanup.

### 4.6 Persistent Agent Memory (Mem0 / LoCoMo / LongMemEval)

**Source:** Mem0.ai 2026 Report, Du et al. 2026 (arXiv:2603.07670v1)

**Key insight:** Production agent memory requires: episodic (what happened),
semantic (facts/preferences), procedural (how to do things), scoped by
user/project/session, with hybrid retrieval (semantic + BM25 + entity linking).

**Application to Clawde:**
- `.clawde/memory/` directory in project root.
- Markdown files for human-readability and git-trackability.
- `architecture.md` — project structure, key abstractions, conventions.
- `decisions.md` — architectural decisions and their rationale.
- `conventions.md` — code style, test commands, build commands.
- `tasks.md` — pending tasks and their status.
- Agent auto-reads relevant memory files at session start.
- Agent auto-updates memory files as it learns new things.
- Optional SQLite backend with embeddings for semantic search if markdown proves
  too slow for large projects.

### 4.7 Pi's Self-Extending Architecture

**Source:** Pi source code analysis (Armin Ronacher, Mario Zechner)

**Key insight:** Pi rejects MCP and plugin stores. Instead, the agent WRITES its
own extensions as scripts, hot-reloads them, and tests them in an interactive loop.

**Application to Clawde:**
- Clawde already has hooks, skills, and a plugin system. These could be
  extended so the agent can write its own hooks/skills.
- Example: "Write a pre-commit hook that checks for debug prints" — agent
  writes the hook script, saves it to `.clawde/hooks/`, and it activates
  immediately.

---

## 5. Interview Decisions — Summary

| # | Question | Decision |
|---|---|---|
| 1 | Biggest friction | ALL: verification, memory, architectural decisions, multi-file complexity, reliability |
| 2 | Desired autonomy | Execute-and-verify loop primary. Also wants plan-then-execute and spec-driven modes. |
| 3 | Multi-model appetite | "Yes — this is the future" |
| 4 | Sandbox strategy | Configurable: direct, git worktree, or containerized |
| 5 | Memory scope | All: architecture, session history, user preferences, codebase map |
| 6 | Provider capabilities | All: agent-as-provider, local models, model comparison, smart routing |
| 7 | Build priority | Execute-and-verify first, then memory, then multi-model router |
| 8 | Surface strategy | All surfaces — unified core. But stay TUI-first for now. |
| 9 | Trust model | Gate by risk score + progressively unlock |
| 10 | Router config | Automatic by default, interactive TUI config available |
| 11 | Verify UX | Auto-retry silently up to 3 times |
| 12 | FreeProvider fate | FreeProvider becomes the provider pool for multi-model routing. NO paid tiers. |
| 13 | Memory storage | File-based `.clawde/memory/` preferred. Hybrid SQLite+markdown if performance requires. |
| 14 | Next surface | Stay TUI-first for now |

---

## 6. Architecture Vision — Unified Core, Multi-Surface

### 6.1 Guiding Principle

> **The agent should reduce work by making good decisions automatically, not by
> asking the user to make more decisions.**

Every feature must pass the test: "Does this reduce the number of times the user
has to think about something?"

### 6.2 Target Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                      SURFACE LAYER                            │
│  TUI (ratatui)  │  Headless CLI  │  ACP (editor)  │  Daemon  │
├──────────────────────────────────────────────────────────────┤
│                      QUERY ENGINE                             │
│  Spec Mode  │  Plan Mode  │  Execute+Verify Mode  │  Default  │
├──────────────────────────────────────────────────────────────┤
│                   SMART MODEL ROUTER                          │
│  Task classifier → Model selector → Provider dispatcher       │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐    │
│  │ Planner   │  │ Executor  │  │ Verifier  │  │ Comparator│   │
│  │ (reason)  │  │ (fast)    │  │ (linter)  │  │ (ensembl) │   │
│  └──────────┘  └──────────┘  └──────────┘  └──────────┘    │
├──────────────────────────────────────────────────────────────┤
│                      CORE SERVICES                            │
│  Project Memory  │  Hooks  │  Sandbox  │  Auth  │  Cost      │
├──────────────────────────────────────────────────────────────┤
│                      PROVIDER LAYER                           │
│  50+ providers via LlmProvider trait + FreeProvider chain    │
└──────────────────────────────────────────────────────────────┘
```

### 6.3 Key Architectural Decisions

1. **`LlmProvider` trait is the extension point.** The smart router is just
   another `LlmProvider` that delegates to child providers. No breaking changes.

2. **`ContinuationPolicy` drives the verify loop.** After a tool turn, a
   `VerifyPolicy` injects a follow-up user message ("Your edit caused these test
   failures. Fix them.") and the loop continues.

3. **Project memory lives in `.clawde/memory/`.** Markdown files, git-trackable,
   auto-read at session start, auto-updated by the agent. Optional SQLite backend.

4. **Risk scoring is a function of tool + context.** Each tool call gets a
   risk score (0.0–1.0). Low-risk actions auto-approve. High-risk actions
   need user confirmation. The threshold decays with trust over time.

5. **FreeProvider becomes the provider pool.** Every upstream in the FreeProvider
   chain is a candidate for task-based routing. The router picks the best
   upstream for each subtask (planning, execution, verification).

---

## 7. Phase 1 — Execute-and-Verify Loop (P0)

**Status: implemented** — VerifyPolicy, sandbox selection (direct / git worktree /
container), the TUI verify box, and the `/settings` sandbox picker are all live.

### 7.1 Goal

After the agent writes or edits a file, it automatically:
1. Runs the project's test suite (or a subset)
2. Runs the project's linter/typechecker
3. If failures: feeds errors back to the model for auto-fix
4. Repeats up to 3 times
5. Reports result: "All checks passed" or "3 auto-fix attempts failed — here's
   what's still broken"

### 7.2 User Experience

**Silent auto-retry (default):**
```
User: Add a `calculate_total` function to src/orders.rs
Agent: [writes code]
       [runs `cargo test` — 2 failures]
       [auto-fix attempt 1: fixes type mismatch]
       [runs `cargo test` — 1 failure]
       [auto-fix attempt 2: fixes missing import]
       [runs `cargo test` — all passing]
       [runs `cargo clippy` — clean]
       ✓ All checks passed. Added `calculate_total` to src/orders.rs.
```

**TUI display during verify loop:**
- A small inline indicator: `[verifying... (2/3)]` with a spinner
- Test output is captured but not shown unless failures persist
- Final status: green checkmark or red X with summary

### 7.3 Implementation

#### 7.3.1 New Tools

| Tool | Purpose |
|---|---|
| `RunTestsTool` | Execute the project's test command. Detects test framework (cargo test, pytest, npm test, etc.) via project analysis. |
| `RunLintsTool` | Execute linter/typechecker. Detects via project analysis (cargo clippy, tsc --noEmit, ruff, eslint, etc.). |
| `DetectProjectTool` | Analyzes project to determine language, build system, test framework, lint tools. Caches result in project memory. |

#### 7.3.2 VerifyPolicy (New ContinuationPolicy)

```rust
// crates/query/src/verify.rs (new)

pub struct VerifyPolicy {
    max_retries: u32,         // default 3
    project_root: PathBuf,
    test_command: Option<String>,
    lint_command: Option<String>,
}

impl ContinuationPolicy for VerifyPolicy {
    fn decide(&self, ctx: &TurnEndContext<'_>) -> ContinuationDecision {
        // 1. Run tests
        // 2. Run lints
        // 3. If failures and retries < max:
        //    → Continue { message: "Your edit caused N failures:\n<output>\nFix them." }
        // 4. If all pass:
        //    → Stop { note: Some("All checks passed.") }
        // 5. If max retries exhausted:
        //    → Stop { note: Some("Auto-fix exhausted. Manual intervention needed.") }
    }
}
```

#### 7.3.3 Sandbox Selection

The verify loop runs in the user's chosen sandbox mode:

| Mode | Implementation |
|---|---|
| **Direct** (default) | Run tests in the actual project directory. Fastest, but has side effects. **Implemented.** |
| **Git worktree** | Create a temporary git worktree, apply the agent's changes, run tests there. Clean isolation, no side effects. Auto-cleaned after verification. **Implemented** (`crates/query/src/verify_sandbox.rs`). |
| **Containerized** | Docker/podman container with project mount. Maximum isolation. Requires container runtime. **Implemented** (`crates/query/src/verify_container.rs`; image = `verify.container_image` setting → `CLAWDE_VERIFY_IMAGE` env → per-language default). |

Configuration in settings.json:
```json
{
  "verify": {
    "enabled": true,
    "max_retries": 3,
    "sandbox": "direct",
    "auto_lint": true,
    "auto_test": true,
    "container_image": "node:20-slim"
  }
}
```

#### 7.3.4 Files Changed

| File | Change |
|---|---|
| `crates/tools/src/run_tests.rs` (new) | `RunTestsTool` — detect and run test framework |
| `crates/tools/src/run_lints.rs` (new) | `RunLintsTool` — detect and run linter/typechecker |
| `crates/tools/src/detect_project.rs` (new) | `DetectProjectTool` — analyze project structure |
| `crates/tools/src/lib.rs` | Register new tools |
| `crates/query/src/verify.rs` (new) | `VerifyPolicy`, `VerifyConfig`, verify loop logic |
| `crates/query/src/continuation.rs` | Add `Verify` variant to `ContinuationMode` |
| `crates/query/src/lib.rs` | Wire `VerifyPolicy` into `run_query_loop` |
| `crates/core/src/config.rs` | Add `VerifyConfig` to `Config` |
| `crates/tui/src/render.rs` | Add verify loop indicator to TUI |
| `crates/tui/src/app.rs` | Handle verify loop events, status display |

---

## 8. Phase 2 — Multi-Model Smart Router (P0)

### 8.1 Goal

Instead of treating FreeProvider as a fallback chain, use it as a **model pool**
where different upstreams serve different roles based on the task:

- **Planning/Reasoning:** Gemini (free tier), Groq (Llama 3.3 70B)
- **Code Generation:** DeepSeek via OpenRouter, Cerebras, HuggingFace
- **Fast Verification:** Groq (fast tokens), Cloudflare Workers AI
- **Cheap/Fallback:** SambaNova, NVIDIA NIM, Cohere, Mistral, Z.AI

### 8.2 Router Architecture

```
User Request
    │
    ▼
┌─────────────────────┐
│  Task Classifier     │  ← Classifies the request into a task type
│  (fast, local model  │    (code_gen, reasoning, planning, verification,
│   or keyword-based)  │     simple_edit, search, architecture)
└────────┬────────────┘
         │
         ▼
┌─────────────────────┐
│  Model Selector      │  ← Maps task type to preferred model(s)
│  (configurable       │    Uses latency history, cooldown state,
│   strategy)          │    model capabilities from catalog
└────────┬────────────┘
         │
         ▼
┌─────────────────────┐
│  Provider Dispatcher │  ← Dispatches to the selected upstream
│  (uses existing      │    Falls back to next-best on failure
│   FreeProvider)      │    Records latency for future decisions
└─────────────────────┘
```

### 8.3 Task Classification

The task classifier determines WHAT the user/agent is trying to do:

| Task Type | Characteristics | Preferred Model Traits |
|---|---|---|
| `code_generation` | Write new functions, modules, types | Strong coding benchmarks, tool calling |
| `code_edit` | Modify existing code, refactor | Fast, good at following instructions |
| `reasoning` | Analyze bugs, architecture decisions | Strong reasoning, thinking mode |
| `planning` | Design before implementation | Structured output, comprehensive |
| `verification` | Run tests, check output | Any model — just format errors |
| `simple_edit` | Rename variable, fix typo | Cheapest available |
| `search` | Grep, find references | Any model — tool calling only |

**Classification strategy (layered):**
1. If the request matches a slash command or tool call → task type is explicit
2. If the agent's turn involves file edits → `code_edit`
3. If the agent's turn involves new files → `code_generation`
4. If the user message contains "why", "how", "explain", "debug" → `reasoning`
5. Default → `code_generation`

### 8.4 Model Selection Strategy

**Default strategy: `Auto`** (no user config needed)

For each task type, maintain an ordered preference list based on:
1. **Capability match:** Does the model support tool calling? thinking?
2. **Latency history:** How fast has this model been recently?
3. **Cooldown state:** Is the model or its keys exhausted?
4. **Cost:** Prefer free tiers, fall back to paid only when no free option works

**Example auto-configuration:**
```
Task: code_generation
Preferences (in order):
  1. openrouter/deepseek/deepseek-chat    (strong coder, free via OpenRouter)
  2. cerebras/gpt-oss-120b                (fast, free tier)
  3. huggingface/meta-llama/Llama-3.3-70B (reliable, free tier)
  4. groq/llama-3.3-70b-versatile         (fast, free tier)
  5. ... (remaining free upstreams)

Task: reasoning
Preferences (in order):
  1. google/gemini-2.5-flash              (strong reasoning, free tier)
  2. groq/llama-3.3-70b-versatile         (good reasoning, fast)
  3. ... (remaining free upstreams)

Task: verification
Preferences (in order):
  1. groq/llama-3.1-8b-instant            (fastest, cheapest)
  2. cloudflare/@cf/meta/llama-3.1-8b     (fast, free tier)
  3. ... (remaining free upstreams)
```

### 8.5 Router Implementation

The router is a new `LlmProvider` implementation that:
1. Implements `create_message` and `create_message_stream`
2. Classifies the request into a task type
3. Selects the best model for that task type
4. Dispatches to the underlying provider
5. Records latency on success, records failure on error
6. Falls back to the next-best model on failure

```rust
// crates/api/src/providers/smart_router.rs (new)

pub struct SmartRouter {
    free_provider: Arc<FreeProvider>,
    task_preferences: TaskPreferences,
    latency_state: Arc<Mutex<LatencyState>>,
    config: RouterConfig,
}

#[async_trait]
impl LlmProvider for SmartRouter {
    async fn create_message(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let task = self.classify(&request);
        let plan = self.select_models(task);
        // Try each model in preference order, fall back on failure
        for (idx, model) in plan {
            let result = self.dispatch(idx, model, &request).await;
            match result {
                Ok(resp) => return Ok(resp),
                Err(e) if should_fallback(&e) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(ProviderError::ServerError { ... })
    }
}
```

### 8.6 TUI Configuration

A new TUI dialog (`/routing` or settings screen) shows:
- Current task-to-model assignments
- Model performance stats (avg latency, success rate)
- Ability to pin specific models to specific tasks
- "Auto" toggle to revert to automatic selection

### 8.7 Files Changed

| File | Change |
|---|---|
| `crates/api/src/providers/smart_router.rs` (new) | `SmartRouter` — task classification + model selection + dispatch |
| `crates/api/src/providers/task_classifier.rs` (new) | Task classification logic |
| `crates/api/src/registry.rs` | Register `SmartRouter` as a provider, wrap FreeProvider |
| `crates/core/src/config.rs` | Add `RouterConfig` to `Config` |
| `crates/tui/src/render.rs` | Add router status to TUI footer |
| `crates/tui/src/app.rs` | Handle router events, status display |
| `crates/commands/src/routing.rs` | Extend `/routing` command with task-model mapping |

---

## 9. Phase 3 — Persistent Project Memory (P1)

### 9.1 Goal

The agent remembers everything important about a project across sessions:
architecture, conventions, decisions, pending tasks, and user preferences.

The user never has to re-explain the project.

### 9.2 Memory Architecture

```
.clawde/memory/
├── architecture.md    # Project structure, key abstractions, module map
├── conventions.md     # Code style, naming, test/build commands, lint rules
├── decisions.md       # Architectural decisions log (ADR-style)
├── tasks.md           # Pending tasks and their status
├── preferences.md     # User preferences for this project
└── sessions/          # Summaries of past sessions (auto-generated)
    ├── 2026-08-01.md
    ├── 2026-08-05.md
    └── ...
```

### 9.3 Memory Lifecycle

**On session start:**
1. Agent reads `conventions.md` and `architecture.md`
2. Agent reads the most recent session summary
3. This context is injected into the system prompt (or as a user preamble)

**During session:**
1. When the agent learns something new (e.g., "this project uses `uv` not `pip`"),
   it writes to the relevant memory file.
2. When the agent makes an architectural decision, it appends to `decisions.md`.
3. Task completion updates `tasks.md`.

**On session end:**
1. `auto_dream` (existing) generates a session summary.
2. Summary is saved to `.clawde/memory/sessions/YYYY-MM-DD.md`.
3. Agent updates `architecture.md` and `conventions.md` with new learnings.

### 9.4 Memory Injection

Memory is injected into the system prompt at session start:

```
[PROJECT MEMORY — auto-loaded from .clawde/memory/]

Architecture:
- This is a Rust workspace with 12 crates under src-rust/
- The main binary is clawde-cli, TUI is clawde-tui
- Key abstractions: LlmProvider trait, ContinuationPolicy, FreeProvider

Conventions:
- Build: `cargo build` from src-rust/
- Test: `cargo test --workspace` from src-rust/
- Lint: `cargo clippy --workspace --all-targets -- -D warnings`
- Format: `cargo fmt --all`
- No .unwrap() in production code
- Keybindings must flow through keybindings.rs

Recent work (2026-08-05):
- Implemented vim keybinding preset
- Fixed dialogs to respect vim mode for navigation
```

### 9.5 Memory Update Triggers

The agent auto-updates memory when it:
1. Runs a build/test/lint command successfully → update `conventions.md` command reference
2. Creates a new module/crate → update `architecture.md` module map
3. Makes a design tradeoff → append to `decisions.md`
4. Encounters and fixes an error → append to session summary
5. User explicitly teaches something → write to `preferences.md`

### 9.6 Hybrid SQLite Backend (Optional)

For large projects where markdown file I/O becomes a bottleneck:
- SQLite database at `.clawde/memory.db`
- Embeddings for semantic search across all memory entries
- Markdown files remain the canonical source; SQLite is a cache/index
- `memory sync` command to rebuild SQLite from markdown

### 9.7 Files Changed

| File | Change |
|---|---|
| `crates/core/src/memdir.rs` | Extend with project memory file management |
| `crates/query/src/project_memory.rs` (new) | Memory injection into system prompt |
| `crates/query/src/session_memory.rs` | Extend with memory update triggers |
| `crates/query/src/auto_dream.rs` | Extend with session summary → memory write |
| `crates/commands/src/memory.rs` | Extend `/memory` command with project memory browser |
| `crates/tui/src/memory_file_selector.rs` | Extend with `.clawde/memory/` files |

---

## 10. Phase 4 — Spec-Driven Development Mode (P1)

### 10.1 Goal

Before writing code for any non-trivial task, the agent generates a structured
specification. The user reviews and approves it. The agent then implements
against the spec, with automated acceptance testing.

### 10.2 Workflow

```
User: Add a rate-limiting middleware to the API server
    │
    ▼
[AGENT ENTERS SPEC MODE]
    │  Agent analyzes the codebase (reads relevant files, understands architecture)
    │  Agent generates spec in structured format
    ▼
SPEC OUTPUT (presented to user in TUI):
    # Spec: Rate-Limiting Middleware

    ## Requirements
    1. Per-IP rate limiting with configurable window and max requests
    2. Middleware integrates with existing tower::Service stack
    3. Rate limit state stored in-memory (no external dependency)
    4. Configurable via settings.json

    ## Files to Touch
    - crates/api/src/middleware/rate_limit.rs (NEW)
    - crates/api/src/middleware/mod.rs (MODIFY)
    - crates/cli/src/main.rs (MODIFY — wire middleware)

    ## Data Model
    struct RateLimiter { window: Duration, max_requests: u32, store: DashMap<IpAddr, Vec<Instant>> }

    ## Acceptance Tests
    1. Requests under limit pass through
    2. Requests over limit return 429
    3. Window reset clears counters
    4. Different IPs tracked independently

    ## Edge Cases
    - IPv6 addresses normalized
    - Clock skew handled
    - Concurrent requests under limit all pass

    [Accept] [Edit Spec] [Reject]
    │
    ▼ (User accepts)
[AGENT EXITS SPEC MODE, ENTERS EXECUTION MODE]
    │  Implements according to spec
    │  Runs acceptance tests from spec
    │  Reports: "2/4 tests passing, fixing..."
    │  Auto-fix loop
    ▼
✓ Spec complete. All 4 acceptance tests passing. Ready for review.
```

### 10.3 Spec Format

```rust
// crates/core/src/spec.rs (new)

pub struct Spec {
    pub title: String,
    pub requirements: Vec<String>,
    pub files_to_touch: Vec<FilePlan>,
    pub data_models: Vec<DataModel>,
    pub acceptance_tests: Vec<AcceptanceTest>,
    pub edge_cases: Vec<String>,
}

pub struct FilePlan {
    pub path: String,
    pub action: FileAction, // Create, Modify, Delete
    pub description: String,
}
```

### 10.4 Integration with Verify Loop

The spec's acceptance tests become the verification criteria:
1. Agent implements code
2. Verify loop runs project's test suite + spec's acceptance tests
3. If acceptance tests fail, agent knows EXACTLY what's wrong
4. This is strictly better than generic "tests failed" feedback

### 10.5 Files Changed

| File | Change |
|---|---|
| `crates/core/src/spec.rs` (new) | Spec data types |
| `crates/tools/src/enter_spec_mode.rs` (new) | Tool to enter spec generation mode |
| `crates/tools/src/exit_spec_mode.rs` (new) | Tool to exit spec mode and begin implementation |
| `crates/query/src/spec_mode.rs` (new) | Spec mode prompt, spec generation, user review flow |
| `crates/query/src/continuation.rs` | Add `SpecMode` variant to `ContinuationMode` |
| `crates/tui/src/render.rs` | Spec review UI (requirements, files, tests) |
| `crates/tui/src/app.rs` | Handle spec mode state |
| `crates/commands/src/spec.rs` (new) | `/spec` command |

---

## 11. Phase 5 — Model Comparison / Ensembling (P2)

### 11.1 Goal

For high-stakes code generation, run the same prompt on 2-3 free models, compare
outputs, and select the best result automatically.

### 11.2 Approach — EnsLLM-Inspired

1. **Parallel dispatch:** Send identical prompt to 2-3 models simultaneously
2. **AST similarity:** Compare outputs via AST structure (not text). Different
   variable names but same structure = high similarity.
3. **Consensus selection:** If 2/3 models produce structurally similar code,
   select the consensus candidate.
4. **Test-driven tiebreak:** If outputs differ, run the project's test suite
   against each candidate. Pick the one that passes.
5. **Fallback:** If all fail tests or all are different, pick the fastest
   response.

### 11.3 When to Use

- User explicitly requests: `/compare` or "compare models for this"
- Auto-triggered for: new module creation, complex refactors, security-sensitive code
- NOT used for: simple edits, single-line changes, tool calls (wasteful)

### 11.4 TUI Display

During comparison:
```
[Comparing 3 models...]
  groq/llama-3.3-70b        ✓ done (2.3s)
  cerebras/gpt-oss-120b     ✓ done (3.1s)
  huggingface/Llama-3.3     ✓ done (4.7s)

Consensus: groq + cerebras agree (AST similarity 89%)
Selected: cerebras/gpt-oss-120b (passed all tests)
```

### 11.5 Files Changed

| File | Change |
|---|---|
| `crates/api/src/providers/comparison.rs` (new) | ComparisonProvider — parallel dispatch + consensus |
| `crates/api/src/providers/ast_similarity.rs` (new) | AST-based code similarity (language-agnostic?) |
| `crates/commands/src/compare.rs` (new) | `/compare` command |
| `crates/tui/src/render.rs` | Comparison progress display |

---

## 12. Phase 6 — Background Daemon & Watch Mode (P2)

### 12.1 Goal

Clawde runs as a background daemon that watches git changes and proactively:
- Suggests improvements after commits
- Detects potential bugs on save
- Runs scheduled maintenance (dependency updates, formatting, etc.)

### 12.2 Architecture

```
clawde --watch                    # Start daemon
clawde --watch --project ./myapp  # Watch specific project
clawde --watch status             # Check daemon status
clawde --watch stop               # Stop daemon
```

The daemon:
1. Watches git index for changes (`inotify`/`kqueue`/polling)
2. On file save: runs a fast linter pass (not full test suite)
3. On commit: runs full verify loop on changed files
4. Reports issues as desktop notifications or TUI badges
5. Can be configured to auto-fix or just report

### 12.3 Files Changed

| File | Change |
|---|---|
| `crates/cli/src/daemon.rs` (new) | Daemon entry point |
| `crates/core/src/file_watcher.rs` (new) | Cross-platform file watching |
| `crates/query/src/background_agent.rs` (new) | Background agent loop |

---

## 13. Phase 7 — Headless CI Mode (P2)

### 13.1 Goal

Clawde runs in CI pipelines:
- `clawde --ci review` — Review a PR and post comments
- `clawde --ci fix-issue` — Fix a GitHub issue and create a PR
- `clawde --ci check` — Run verify loop on changed files, exit with status code

### 13.2 Architecture

Headless mode reuses the same core query engine as interactive mode, but:
- No TUI (obviously)
- JSON output to stdout
- Exit codes: 0 = success, 1 = issues found, 2 = error
- GitHub integration via `gh` CLI or API
- Configurable via `.github/workflows/clawde.yml` or similar

### 13.3 Files Changed

| File | Change |
|---|---|
| `crates/cli/src/ci.rs` (new) | CI subcommand entry point |
| `crates/cli/src/main.rs` | Add `ci` subcommand |
| `crates/query/src/headless.rs` | Extend headless mode with CI-specific output formats |

---

## 14. Cross-Cutting — Trust Gradient & Risk Scoring (All Phases)

### 14.1 Goal

The agent gets more autonomy as it proves itself trustworthy. Users don't have
to manually adjust permission settings — trust grows organically.

### 14.2 Risk Scoring

Every tool invocation receives a risk score (0.0–1.0):

| Tool | Base Risk | Factors |
|---|---|---|
| `Read` | 0.0 | Always safe |
| `Grep` | 0.0 | Always safe |
| `Glob` | 0.0 | Always safe |
| `WebSearch` | 0.1 | Network access |
| `WebFetch` | 0.2 | Network access |
| `Write` (new file) | 0.4 | Creates new code |
| `Edit` (existing file) | 0.5 | Modifies existing code |
| `Bash` (read-only) | 0.3 | `ls`, `cat`, `git status` |
| `Bash` (side-effect) | 0.7 | `git commit`, `npm install` |
| `Bash` (destructive) | 0.9 | `rm`, `git push --force` |
| `Write` (config/sensitive) | 0.8 | Modifying settings, auth files |

**Risk modifiers:**
- +0.2 if file is outside the project root
- +0.1 if operating on a file with no prior agent edits this session
- -0.2 if the same operation succeeded in the past 5 minutes
- -0.1 if in execute-and-verify mode (tests will catch errors)

### 14.3 Trust Decay

Trust is per-project, stored in `.clawde/trust.json`:

```json
{
  "version": 1,
  "score": 0.72,
  "successful_actions": 143,
  "failed_actions": 3,
  "last_failure": "2026-08-05T14:32:00Z",
  "threshold": 0.5
}
```

- **Initial threshold:** 0.5 (midpoint — moderate caution)
- **Success:** Score increases by +0.01 per successful action
- **Failure:** Score decreases by -0.05 per failed action
- **Threshold decays:** Each week without a failure, threshold drops by 0.05
- **Floor:** 0.2 (never fully autonomous for dangerous operations)
- **Ceiling:** 0.9 (some things should always need approval)

**Gate logic:**
```
if risk_score <= trust_threshold:
    auto-approve
else:
    show permission dialog
    
// Override: user can always set fixed permission modes
```

### 14.4 TUI Display

Trust status shown in settings or `/trust` command:
```
Project trust: ████████░░ 0.72 (High)
  143 successful actions, 3 failures
  Auto-approving actions up to risk 0.5
  Last failure: 2026-08-05 (deleted wrong file — fixed)
```

---

## 15. Cross-Cutting — TUI Improvements (All Phases)

### 15.1 Verify Loop Indicator

Inline in the transcript, between agent message and next user message:
```
┌ Verify ─────────────────────────────┐
│ cargo test .................... PASS │
│ cargo clippy .................. PASS │
│ All checks passed ✓                 │
└─────────────────────────────────────┘
```

### 15.2 Smart Router Status

In the TUI footer, show which model is active and why:
```
Free: Auto → groq/llama-3.3-70b (code_gen) │ ctx: 12% │ $0.000
```

### 15.3 Memory Status

In the welcome screen, show memory freshness:
```
Project memory: 4 files, last updated 2h ago
  ⚡ architecture.md, conventions.md, decisions.md, tasks.md
```

### 15.4 Trust Gradient Indicator

Small indicator in status bar:
```
[Trust: ●●○○○]  — trust level visual
```

### 15.5 Spec Review UI

When the agent enters spec mode, show a dedicated review panel:
```
┌ Spec: Rate-Limiting Middleware ──────────────────────────────────────┐
│                                                                       │
│ Requirements:                                                         │
│  ✓ 1. Per-IP rate limiting with configurable window                   │
│  ✓ 2. Integrates with existing tower::Service stack                   │
│  ✓ 3. In-memory state store (no external dependency)                  │
│  ✓ 4. Configurable via settings.json                                  │
│                                                                       │
│ Files (3):                                                            │
│  + crates/api/src/middleware/rate_limit.rs    (NEW)                   │
│  ~ crates/api/src/middleware/mod.rs           (MODIFY)                │
│  ~ crates/cli/src/main.rs                     (MODIFY)                │
│                                                                       │
│ Acceptance Tests (4):                                                 │
│  1. Requests under limit pass through                                 │
│  2. Requests over limit return 429                                    │
│  3. Window reset clears counters                                      │
│  4. Different IPs tracked independently                              │
│                                                                       │
│ Edge Cases: IPv6 normalization, clock skew, concurrency              │
│                                                                       │
│ [Enter: Accept]  [e: Edit Spec]  [Esc: Reject]                      │
└───────────────────────────────────────────────────────────────────────┘
```

---

## 16. Implementation Plan — Ordered Steps

### Phase 1: Execute-and-Verify (estimated: 2-3 weeks)

1. [ ] **DetectProjectTool** — Analyze project for language, build system, test framework, lint tools
2. [ ] **RunTestsTool** — Execute test command, capture output, parse failures
3. [ ] **RunLintsTool** — Execute lint command, capture output, parse warnings/errors
4. [ ] **VerifyPolicy** — New ContinuationPolicy: run tests/lints after edits, feed errors back
5. [ ] **VerifyConfig** — Settings.json schema: enable/disable, max_retries, sandbox mode
6. [ ] **TUI verify indicator** — Inline verification status in transcript
7. [ ] **Sandbox modes** — Direct, git worktree, containerized (configurable)
8. [ ] **Tests** — Unit tests for tools, integration test for verify loop

### Phase 2: Multi-Model Router (estimated: 2-3 weeks)

1. [ ] **TaskClassifier** — Classify requests into task types (keyword + context-based)
2. [ ] **ModelSelector** — Map task types to preferred models using capability + latency data
3. [ ] **SmartRouter** — New LlmProvider: classify → select → dispatch → fallback
4. [ ] **ProviderRegistry integration** — Register SmartRouter, wrap FreeProvider
5. [ ] **RouterConfig** — Settings.json: task-model mappings, strategy, auto-toggle
6. [ ] **TUI router status** — Footer shows active model and task type
7. [ ] **TUI router config dialog** — Interactive model-to-task mapping
8. [ ] **Tests** — Classification accuracy, fallback behavior, latency tracking

### Phase 3: Project Memory (estimated: 2 weeks)

1. [ ] **Memory file templates** — architecture.md, conventions.md, decisions.md, tasks.md
2. [ ] **Memory injection** — Read memory files at session start, inject into context
3. [ ] **Memory update triggers** — Auto-write on build/test learnings, decisions, task completion
4. [ ] **Session summary → memory** — Extend auto_dream to write session summaries
5. [ ] **Memory browser** — Extend /memory command and TUI to browse project memory files
6. [ ] **Hybrid SQLite backend** — Optional: embeddings + semantic search for large projects
7. [ ] **Tests** — Memory read/write, injection formatting, update trigger accuracy

### Phase 4: Spec-Driven Development (estimated: 2-3 weeks)

1. [ ] **Spec data types** — Spec, FilePlan, DataModel, AcceptanceTest
2. [ ] **EnterSpecModeTool / ExitSpecModeTool** — Tool-callable mode switching
3. [ ] **Spec generation prompt** — Structured prompt for spec creation
4. [ ] **Spec review UI** — TUI panel for reviewing and editing specs
5. [ ] **Spec-anchored verify** — Use spec's acceptance tests in verify loop
6. [ ] **/spec command** — Slash command to trigger spec workflow
7. [ ] **Tests** — Spec generation quality, review UI, acceptance test integration

### Phase 5: Model Comparison (estimated: 1-2 weeks)

1. [ ] **ComparisonProvider** — Parallel dispatch to 2-3 models
2. [ ] **AST similarity** — Basic structural comparison for code outputs
3. [ ] **Consensus selection** — Pick best output from comparison
4. [ ] **/compare command** — Slash command for explicit comparison
5. [ ] **TUI comparison display** — Progress and results visualization
6. [ ] **Tests** — Parallel dispatch, consensus logic, fallback behavior

### Phase 6: Background Daemon (estimated: 2 weeks)

1. [ ] **File watcher** — Cross-platform git index monitoring
2. [ ] **Background agent loop** — Trigger verify on save/commit
3. [ ] **Notification system** — Desktop notifications or TUI badges
4. [ ] **--watch CLI flag** — Daemon lifecycle management
5. [ ] **Tests** — Watch triggers, notification delivery

### Phase 7: Headless CI (estimated: 1-2 weeks)

1. [ ] **--ci CLI flag** — CI subcommand entry point
2. [ ] **JSON output format** — Machine-parseable results
3. [ ] **GitHub integration** — PR review, issue-to-PR workflow
4. [ ] **Exit codes** — 0/1/2 for CI pipeline integration
5. [ ] **Tests** — Headless output format, CI exit codes

---

## 17. Test Strategy

### 17.1 Testing Philosophy

- All new tools get registry validation tests (existing pattern)
- All new providers get mocked HTTP fixture tests (existing pattern)
- Verify loop gets end-to-end integration tests with a mock test runner
- TUI changes get rendering snapshot tests (existing `context_viz_renders_without_panic` pattern)

### 17.2 Key Test Scenarios

**Verify Loop:**
- Agent writes code that passes all tests → verify finishes silently
- Agent writes code that fails 1 test → auto-fix succeeds on attempt 1
- Agent writes code that fails 3 tests → auto-fix succeeds on attempt 3
- Agent writes code that fails → all 3 auto-fix attempts fail → surfaces to user
- Agent writes code, verify loop interrupted by user → clean cancellation
- Linter-only failures (no test failures) → auto-fix for lint issues
- Sandbox mode: direct vs worktree vs containerized → correct isolation

**Smart Router:**
- Classification accuracy for each task type
- Model selection respects cooldown state
- Fallback to next model on error
- Latency tracking updates correctly
- Auto vs manual configuration toggle

**Project Memory:**
- Memory files created on first session
- Memory files updated when agent learns new conventions
- Memory injected correctly into system prompt
- Session summaries written correctly
- Multiple projects with different memory files don't conflict

**Spec Mode:**
- Spec generated for a complex task
- Spec review UI renders all sections
- Acceptance tests extracted from spec
- Verify loop uses spec's acceptance tests
- Edge cases documented in spec are tested

**Trust Gradient:**
- Risk scores assigned correctly for different tool types
- Trust threshold decays over time
- Auto-approve when risk ≤ threshold
- Show permission dialog when risk > threshold

---

## 18. Open Questions & Future Research

### 18.1 Technical Uncertainties

1. **AST similarity across languages** — CodeBLEU requires language-specific
   parsers. Can we do a simpler structural similarity (token-level) that works
   across all languages? Or should we start with Rust-only?

2. **Project detection heuristics** — How reliably can we detect test commands
   from project structure alone? Should we fall back to asking the user once
   and caching the answer in project memory?

3. **Memory token budget** — How much context window should project memory
   consume? Need a budget: maybe 5% of the window for memory, configurable.

4. **Sandbox overhead** — Git worktree creation adds ~1-3 seconds per verify.
   Is this acceptable? Should direct mode be the default for speed?

5. **Smart router cold start** — When no latency data exists, what's the
   initial model ordering? Static preference list based on known benchmarks?

6. **Trust gradient safety** — Could an adversarial prompt game the trust
   system by succeeding at low-risk actions to unlock high-risk ones?
   Mitigation: risk ceiling of 0.9 for dangerous tools regardless of trust.

### 18.2 Future Research Areas

1. **Agent-written hooks** — Pi's approach of letting the agent write its own
   extensions. Could Clawde agents write custom hooks, skills, or tools?

2. **Code world models** — Meta's research on using code as the agent's
   world model. Let the agent build and maintain its own understanding of the
   codebase as executable models.

3. **Collaborative sessions** — Multiple Clawde instances sharing a session
   (like Crush's workspace collaboration). Multiple developers + agents
   working on the same problem.

4. **Reinforcement learning from human feedback (RLHF) for tool use** —
   Could Clawde learn which tools/approaches work best based on user accept/reject
   patterns? Privacy concern: this would need to be opt-in and local.

5. **DSPy-style prompt optimization** — Automatically tune system prompts and
   few-shot examples based on success metrics. Could improve verify loop and
   spec generation quality.

---

## Appendix A — Research Sources

- Snell et al., "Categories of Inference-Time Scaling for Improved LLM Reasoning" (2026)
- Mem0 Engineering Team, "AI Agent Memory 2026: Progress Benchmark Report" (2026)
- Piskala et al., "Spec-Driven Development: From Code to Contract in the Age of AI Coding Assistants" (arXiv:2602.00180v1, 2026)
- Lemos et al., "Is It Time To Treat Prompts As Code? A Multi-Use Case Study For Prompt Optimization Using DSPy" (arXiv:2507.03620v1, 2025)
- Jin et al., "ReVeal: Self-Evolving Code Agents via Iterative Generation-Verification" (arXiv:2506.11442, 2025)
- Mahmud et al., "Enhancing LLM Code Generation with Ensembles: A Similarity-Based Selection Approach" (arXiv:2503.15838, 2025)
- Macedo, "Stop Hand-Holding Your Coding Agent: Engineering the Loops that Replace Step-by-Step Prompting" (arXiv:2607.00038, 2026)
- Ning et al., "Code as Agent Harness" (arXiv:2605.18747, 2026)
- Du et al., "Memory for Autonomous LLM Agents: Mechanisms, Evaluation, and Emerging Frontiers" (arXiv:2603.07670v1, 2026)
- Bui, "Building AI Coding Agents for the Terminal: Scaffolding, Harness, Context Engineering, and Lessons Learned (OpenDev)" (arXiv:2603.05344v1, 2026)
- Kan et al., "Harnessing Code Agents for Automatic Software Verification" (arXiv:2607.06341, 2026)
- OpenCode, Pi, Goose, Crush, Aider, Cline, Codex CLI — source code and architecture analysis
- Clawde source code — full architecture reference (CLAWDE_REFERENCE.md), TODO tracker (CLAWDE_TODO.md), spec documents

---

## Appendix B — Comparison with Existing CLAWDE_TODO.md

| Existing TODO | Relationship to This Spec |
|---|---|
| Test coverage gaps (P1) | Addressed by new tools having registry validation tests |
| LSP support limited (P3) | Deprioritized — executor/verifier routing provides more value |
| Plugin marketplace (P2) | Not directly addressed — self-extending hooks (Pi approach) may be better |
| Computer use tool (P2) | Not addressed in this spec — orthogonal concern |
| Session export formats (P3) | Not addressed — orthogonal |
| Diff viewer enhancements (P3) | Not addressed — orthogonal |

**New items not in existing TODO:**
- Execute-and-verify loop (entirely new)
- Multi-model smart router (entirely new)
- Persistent project memory (extends existing memdir)
- Spec-driven development (extends existing plan mode)
- Model comparison/ensembling (entirely new)
- Trust gradient scoring (extends existing permission system)
- Background daemon/watch mode (partially covered by BG_SESSIONS feature flag)
- Headless CI mode (entirely new)

---

## Appendix C — Phase 1 Deep-Dive: Execute-and-Verify Loop

### C.1 VerifyConfig (serde schema)

```rust
// crates/core/src/config.rs — add to Config struct

/// Configuration for the execute-and-verify loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VerifyConfig {
    /// Enable the verify loop. Default: true.
    pub enabled: bool,
    /// Maximum auto-fix retry attempts before surfacing to user.
    pub max_retries: u32,
    /// Sandbox mode for verification.
    pub sandbox: VerifySandbox,
    /// Run linter/typechecker during verification.
    pub auto_lint: bool,
    /// Run tests during verification.
    pub auto_test: bool,
    /// Skip verification when no files were written/edited this turn.
    /// When true (default), turns that only read/search skip the verify step.
    pub skip_when_no_writes: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum VerifySandbox {
    /// Run tests directly in the project directory. Fastest.
    Direct,
    /// Create a git worktree, apply changes, run tests there. Clean isolation.
    Worktree,
    /// Docker/podman container with project mount. Maximum isolation.
    Container,
}

impl Default for VerifyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_retries: 3,
            sandbox: VerifySandbox::Direct,
            auto_lint: true,
            auto_test: true,
            skip_when_no_writes: true,
        }
    }
}
```

### C.2 DetectProjectTool

Detects the project's language, build system, test framework, and lint tools.
Called once per project; caches the result in memory. The model calls this
automatically at the start of a session, or the verify loop calls it implicitly.

```rust
// crates/tools/src/detect_project.rs (new)

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{PermissionLevel, Tool, ToolContext, ToolResult};

/// Cached project detection result. Shared via a once_cell or passed through
/// ToolContext once that field is added.
pub struct ProjectInfo {
    pub language: ProjectLanguage,
    pub test_commands: Vec<String>,
    pub lint_commands: Vec<String>,
    pub build_command: Option<String>,
    pub package_manager: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectLanguage {
    Rust,
    Python,
    TypeScript,
    JavaScript,
    Go,
    Java,
    Cpp,
    Unknown(String),
}

pub struct DetectProjectTool;

#[async_trait]
impl Tool for DetectProjectTool {
    fn name(&self) -> &str { "DetectProject" }

    fn description(&self) -> &str {
        "Analyze the project structure to detect language, build system, \
         test framework, and lint tools. Call this once at session start \
         if the project's tooling is unknown. Results are cached."
    }

    fn permission_level(&self) -> PermissionLevel { PermissionLevel::ReadOnly }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_root": {
                    "type": "string",
                    "description": "Optional project root path. Defaults to current working directory."
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let project_root = input
            .get("project_root")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| ctx.working_dir.clone());

        let info = detect_project_info(&project_root);
        let result = format!(
            "Language: {:?}\n\
             Test commands: {}\n\
             Lint commands: {}\n\
             Build command: {}\n\
             Package manager: {}",
            info.language,
            info.test_commands.join(", "),
            info.lint_commands.join(", "),
            info.build_command.as_deref().unwrap_or("none"),
            info.package_manager.as_deref().unwrap_or("none"),
        );
        ToolResult::success(result)
    }
}

/// Best-effort project detection by scanning for config files.
/// Ordered by specificity: Rust > Python > Go > TypeScript > JavaScript.
fn detect_project_info(root: &std::path::Path) -> ProjectInfo {
    // Rust
    if root.join("Cargo.toml").exists() {
        return ProjectInfo {
            language: ProjectLanguage::Rust,
            test_commands: vec!["cargo test --workspace".into()],
            lint_commands: vec![
                "cargo clippy --workspace --all-targets -- -D warnings".into(),
            ],
            build_command: Some("cargo build".into()),
            package_manager: Some("cargo".into()),
        };
    }

    // Python
    if root.join("pyproject.toml").exists()
        || root.join("setup.py").exists()
        || root.join("setup.cfg").exists()
    {
        // Detect test runner preference
        let mut test_cmds = Vec::new();
        if root.join("tox.ini").exists() {
            test_cmds.push("tox".into());
        }
        // pytest is the most common
        test_cmds.push("python -m pytest".into());

        let mut lint_cmds = Vec::new();
        if root.join("ruff.toml").exists() || root.join("pyproject.toml").exists() {
            lint_cmds.push("ruff check .".into());
        }
        if root.join("mypy.ini").exists()
            || root.join("pyproject.toml").exists()
        {
            lint_cmds.push("mypy .".into());
        }

        let pkg_mgr = if root.join("uv.lock").exists() ||
            root.join("pyproject.toml")
                .exists()
                .then(|| {
                    std::fs::read_to_string(root.join("pyproject.toml"))
                        .ok()
                        .map(|s| s.contains("[tool.uv]"))
                })
                .flatten()
                .unwrap_or(false)
        {
            Some("uv".to_string())
        } else if root.join("poetry.lock").exists() {
            Some("poetry".to_string())
        } else if root.join("Pipfile").exists() {
            Some("pipenv".to_string())
        } else {
            Some("pip".to_string())
        };

        return ProjectInfo {
            language: ProjectLanguage::Python,
            test_commands: test_cmds,
            lint_commands: lint_cmds,
            build_command: None,
            package_manager: pkg_mgr,
        };
    }

    // Go
    if root.join("go.mod").exists() {
        return ProjectInfo {
            language: ProjectLanguage::Go,
            test_commands: vec!["go test ./...".into()],
            lint_commands: vec!["go vet ./...".into()],
            build_command: Some("go build ./...".into()),
            package_manager: Some("go modules".into()),
        };
    }

    // TypeScript / JavaScript
    if root.join("package.json").exists() {
        let pkg_json = std::fs::read_to_string(root.join("package.json"))
            .unwrap_or_default();
        let is_ts = root.join("tsconfig.json").exists()
            || pkg_json.contains("\"typescript\"");

        let mut test_cmds = Vec::new();
        let mut lint_cmds = Vec::new();

        // Detect package manager
        let pkg_mgr = if root.join("pnpm-lock.yaml").exists() {
            Some("pnpm".to_string())
        } else if root.join("yarn.lock").exists() {
            Some("yarn".to_string())
        } else if root.join("bun.lockb").exists() {
            Some("bun".to_string())
        } else {
            Some("npm".to_string())
        };

        // Common test commands
        if pkg_json.contains("\"jest\"") || root.join("jest.config.js").exists() {
            test_cmds.push(format!("{} test", pkg_mgr.as_deref().unwrap_or("npm")));
        } else if pkg_json.contains("\"vitest\"") {
            test_cmds.push(format!("{} run test", pkg_mgr.as_deref().unwrap_or("npm")));
        } else {
            test_cmds.push(format!("{} test", pkg_mgr.as_deref().unwrap_or("npm")));
        }

        if is_ts {
            lint_cmds.push(format!("{} exec tsc --noEmit", pkg_mgr.as_deref().unwrap_or("npm")));
        }
        if pkg_json.contains("\"eslint\"") || root.join("eslint.config.js").exists() {
            lint_cmds.push(format!("{} run lint", pkg_mgr.as_deref().unwrap_or("npm")));
        }

        let lang = if is_ts {
            ProjectLanguage::TypeScript
        } else {
            ProjectLanguage::JavaScript
        };

        return ProjectInfo {
            language: lang,
            test_commands: test_cmds,
            lint_commands: lint_cmds,
            build_command: Some(format!("{} run build", pkg_mgr.as_deref().unwrap_or("npm"))),
            package_manager: pkg_mgr,
        };
    }

    // Unknown — return safe defaults
    ProjectInfo {
        language: ProjectLanguage::Unknown("unknown".into()),
        test_commands: vec!["make test".into()],
        lint_commands: vec![],
        build_command: None,
        package_manager: None,
    }
}
```

### C.3 RunTestsTool

Executes the project's test command and captures output. Designed to be called
by the model directly OR by the VerifyPolicy automatically.

```rust
// crates/tools/src/run_tests.rs (new)

use async_trait::async_trait;
use serde_json::{json, Value};
use std::process::Command;
use std::time::Duration;

use super::{PermissionLevel, Tool, ToolContext, ToolResult};

pub struct RunTestsTool;

#[async_trait]
impl Tool for RunTestsTool {
    fn name(&self) -> &str { "RunTests" }

    fn description(&self) -> &str {
        "Run the project's test suite and report results. \
         Use after making code changes to verify correctness. \
         Returns: pass/fail status + failure details (truncated to 2000 chars)."
    }

    fn permission_level(&self) -> PermissionLevel { PermissionLevel::Execute }

    fn self_gates(&self) -> bool { true }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Optional test command override. Defaults to the detected project test command."
                },
                "filter": {
                    "type": "string",
                    "description": "Optional test filter pattern (e.g. a specific test name or module)."
                },
                "target_directory": {
                    "type": "string",
                    "description": "Optional subdirectory to run tests in. Defaults to project root."
                },
                "timeout_seconds": {
                    "type": "integer",
                    "description": "Timeout in seconds. Default 120."
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        // Detect project info first
        let project = crate::detect_project::detect_project_info(&ctx.working_dir);

        let command = input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                project.test_commands.first().cloned()
                    .unwrap_or_else(|| "make test".to_string())
            });

        let filter = input
            .get("filter")
            .and_then(|v| v.as_str());

        let cwd = input
            .get("target_directory")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| ctx.working_dir.clone());

        let timeout_secs = input
            .get("timeout_seconds")
            .and_then(|v| v.as_u64())
            .unwrap_or(120);

        // Build the full command
        let mut full_cmd = command.clone();
        if let Some(f) = filter {
            full_cmd.push_str(&format!(" {}", f));
        }

        // Permission check: running tests is an execute-level operation
        ctx.check_permission_with_details(
            "RunTests",
            &format!("Run test command: {}", full_cmd),
            &format!("Running tests in {}: {}", cwd.display(), full_cmd),
            false,
        )?;

        // Split into shell command parts
        let parts = if cfg!(target_os = "windows") {
            vec!["cmd".to_string(), "/C".to_string(), full_cmd.clone()]
        } else {
            vec!["sh".to_string(), "-c".to_string(), full_cmd.clone()]
        };

        let mut child = match Command::new(&parts[0])
            .args(&parts[1..])
            .current_dir(&cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return ToolResult::error(format!("Failed to spawn test command: {}", e)),
        };

        // Wait with timeout
        let status = match tokio::time::timeout(
            Duration::from_secs(timeout_secs),
            async {
                child.wait()
            },
        )
        .await
        {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return ToolResult::error(format!("Test process error: {}", e)),
            Err(_) => {
                let _ = child.kill();
                return ToolResult::error(format!(
                    "Test command timed out after {}s: {}",
                    timeout_secs, full_cmd
                ));
            }
        };

        let stdout = String::from_utf8_lossy(
            &child.stdout.map(|_| Vec::new()).unwrap_or_default()
        ).to_string();
        let stderr = String::from_utf8_lossy(
            &child.stderr.map(|_| Vec::new()).unwrap_or_default()
        ).to_string();

        // Actually read the output
        let _ = child.wait();
        let output = std::process::Command::new(&parts[0])
            .args(&parts[1..])
            .current_dir(&cwd)
            .output();

        let (stdout, stderr, exit_code) = match output {
            Ok(o) => (
                String::from_utf8_lossy(&o.stdout).to_string(),
                String::from_utf8_lossy(&o.stderr).to_string(),
                o.status.code(),
            ),
            Err(e) => return ToolResult::error(format!("Test command failed: {}", e)),
        };

        let passed = exit_code == Some(0);
        let combined = format!("{}\n{}", stdout, stderr).trim().to_string();

        // Truncate output for the model — 2000 chars is enough to diagnose failures
        let truncated: String = if combined.len() > 2000 {
            let mut s = combined.chars().take(2000).collect::<String>();
            s.push_str("\n... (output truncated)");
            s
        } else {
            combined
        };

        let summary = if passed {
            format!("✅ Tests passed.\nCommand: {}\n", full_cmd)
        } else {
            format!(
                "❌ Tests FAILED (exit code: {:?}).\nCommand: {}\n\nOutput:\n{}",
                exit_code, full_cmd, truncated
            )
        };

        ToolResult::success(summary)
            .with_metadata(json!({
                "passed": passed,
                "exit_code": exit_code,
                "command": full_cmd,
            }))
    }
}
```

### C.4 RunLintsTool

```rust
// crates/tools/src/run_lints.rs (new)

use async_trait::async_trait;
use serde_json::{json, Value};
use std::process::Command;
use std::time::Duration;

use super::{PermissionLevel, Tool, ToolContext, ToolResult};

pub struct RunLintsTool;

#[async_trait]
impl Tool for RunLintsTool {
    fn name(&self) -> &str { "RunLints" }

    fn description(&self) -> &str {
        "Run the project's linter and/or typechecker and report results. \
         Use after making code changes to catch style issues and type errors. \
         Returns: pass/fail status + warning/error details (truncated to 2000 chars)."
    }

    fn permission_level(&self) -> PermissionLevel { PermissionLevel::Execute }

    fn self_gates(&self) -> bool { true }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Optional lint command override. Defaults to the detected project lint command."
                },
                "target_directory": {
                    "type": "string",
                    "description": "Optional subdirectory to run linter in. Defaults to project root."
                },
                "timeout_seconds": {
                    "type": "integer",
                    "description": "Timeout in seconds. Default 120."
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let project = crate::detect_project::detect_project_info(&ctx.working_dir);

        let command = input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                project.lint_commands.first().cloned()
                    .unwrap_or_else(|| "echo 'no linter configured'".to_string())
            });

        let cwd = input
            .get("target_directory")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| ctx.working_dir.clone());

        let timeout_secs = input
            .get("timeout_seconds")
            .and_then(|v| v.as_u64())
            .unwrap_or(120);

        ctx.check_permission_with_details(
            "RunLints",
            &format!("Run lint command: {}", command),
            &format!("Running linter in {}: {}", cwd.display(), command),
            false,
        )?;

        let parts = if cfg!(target_os = "windows") {
            vec!["cmd".to_string(), "/C".to_string(), command.clone()]
        } else {
            vec!["sh".to_string(), "-c".to_string(), command.clone()]
        };

        let output = std::process::Command::new(&parts[0])
            .args(&parts[1..])
            .current_dir(&cwd)
            .output();

        let (stdout, stderr, exit_code) = match output {
            Ok(o) => (
                String::from_utf8_lossy(&o.stdout).to_string(),
                String::from_utf8_lossy(&o.stderr).to_string(),
                o.status.code(),
            ),
            Err(e) => return ToolResult::error(format!("Lint command failed: {}", e)),
        };

        let passed = exit_code == Some(0);
        let combined = format!("{}\n{}", stdout, stderr).trim().to_string();

        let truncated: String = if combined.len() > 2000 {
            let mut s = combined.chars().take(2000).collect::<String>();
            s.push_str("\n... (output truncated)");
            s
        } else {
            combined
        };

        let summary = if passed {
            format!("✅ Lints passed.\nCommand: {}", command)
        } else {
            format!(
                "❌ Lints FAILED (exit code: {:?}).\nCommand: {}\n\nOutput:\n{}",
                exit_code, command, truncated
            )
        };

        ToolResult::success(summary)
            .with_metadata(json!({
                "passed": passed,
                "exit_code": exit_code,
                "command": command,
            }))
    }
}
```

### C.5 VerifyPolicy — The ContinuationPolicy Implementation

This is the heart of Phase 1. The `VerifyPolicy` is a `ContinuationPolicy` that,
after the agent completes a turn (end_turn with no tool calls), automatically:
1. Checks if files were written/edited this turn
2. If yes: runs tests and lints via subprocess
3. If failures: injects a follow-up message telling the model to fix them
4. Repeats up to max_retries
5. If all pass or retries exhausted: resolves

The policy is **synchronous** (`decide()` never holds a lock across `.await`)
because it shells out via `std::process::Command` which is blocking. This fits
the `ContinuationPolicy` contract: `decide` is called from the async loop but
must be cheap and side-effect-aware. The subprocess calls are cheap (~1-5 seconds)
and the loop is paused waiting for `decide` anyway.

```rust
// crates/query/src/verify.rs (new)

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use clawde_core::config::VerifySandbox;

use super::continuation::{ContinuationDecision, ContinuationPolicy, TurnEndContext};

/// Configuration for the verify loop, derived from the user's VerifyConfig.
#[derive(Debug, Clone)]
pub struct VerifyPolicyConfig {
    pub enabled: bool,
    pub max_retries: u32,
    pub sandbox: VerifySandbox,
    pub auto_lint: bool,
    pub auto_test: bool,
    pub skip_when_no_writes: bool,
    /// The project's root directory.
    pub project_root: PathBuf,
    /// Detected test command (cached from DetectProjectTool).
    pub test_command: String,
    /// Detected lint command (cached from DetectProjectTool).
    pub lint_command: String,
    /// Timeout per verification command.
    pub command_timeout_secs: u64,
}

impl Default for VerifyPolicyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_retries: 3,
            sandbox: VerifySandbox::Direct,
            auto_lint: true,
            auto_test: true,
            skip_when_no_writes: true,
            project_root: PathBuf::from("."),
            test_command: String::new(),
            lint_command: String::new(),
            command_timeout_secs: 120,
        }
    }
}

/// Result of running a single verification command.
#[derive(Debug, Clone)]
struct CommandResult {
    passed: bool,
    exit_code: Option<i32>,
    output: String,
    command: String,
}

/// Run a shell command and capture its output.
fn run_command(cmd_str: &str, cwd: &PathBuf, timeout_secs: u64) -> CommandResult {
    let parts: Vec<&str> = if cfg!(target_os = "windows") {
        vec!["cmd", "/C", cmd_str]
    } else {
        vec!["sh", "-c", cmd_str]
    };

    let output = Command::new(parts[0])
        .args(&parts[1..])
        .current_dir(cwd)
        .output();

    match output {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let stderr = String::from_utf8_lossy(&o.stderr);
            let combined = format!("{}\n{}", stdout, stderr).trim().to_string();
            let exit_code = o.status.code();
            CommandResult {
                passed: exit_code == Some(0),
                exit_code,
                output: combined,
                command: cmd_str.to_string(),
            }
        }
        Err(e) => CommandResult {
            passed: false,
            exit_code: None,
            output: format!("Failed to run command: {}", e),
            command: cmd_str.to_string(),
        },
    }
}

/// Truncate command output for injection into model context.
/// 2500 chars gives enough context for the model to diagnose test failures
/// without wasting tokens on full build output.
fn truncate_output(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{}\n... (output truncated, {} total chars)", truncated, s.len())
    }
}

/// The verify loop continuation policy.
///
/// After the agent finishes a turn with end_turn (no tool calls), this policy:
/// 1. Checks if any Write/Edit tools were used this turn
/// 2. Runs the project's test suite
/// 3. Runs the project's linter/typechecker
/// 4. If failures: injects a continuation message telling the model to fix them
/// 5. Repeats up to max_retries
/// 6. If all pass: returns Stop with a success note
/// 7. If max retries exhausted: returns Stop with a failure note
///
/// IMPORTANT: `decide()` is synchronous (no .await). It shells out via
/// std::process::Command which blocks the calling thread. This is acceptable
/// because the query loop is paused waiting for the continuation decision
/// anyway, and verify commands typically complete in 1-5 seconds.
#[derive(Debug, Clone)]
pub struct VerifyPolicy {
    config: VerifyPolicyConfig,
    /// Retries consumed so far. Reset when all checks pass.
    retries: u32,
    /// Whether any file writes occurred in the most recent turn.
    had_file_writes: bool,
}

impl VerifyPolicy {
    pub fn new(config: VerifyPolicyConfig) -> Self {
        Self {
            config,
            retries: 0,
            had_file_writes: false,
        }
    }

    /// Call this from the TUI/app layer when a Write/Edit/BatchEdit tool
    /// completes, so the policy knows verification is needed.
    pub fn mark_file_write(&mut self) {
        self.had_file_writes = true;
    }

    /// Reset file-write tracking for the next turn.
    fn reset_for_next_turn(&mut self) {
        self.had_file_writes = false;
    }
}

impl ContinuationPolicy for VerifyPolicy {
    fn decide(&self, ctx: &TurnEndContext<'_>) -> ContinuationDecision {
        if !self.config.enabled {
            return ContinuationDecision::Stop { note: None };
        }

        // Skip verification when no files were written this turn (e.g. a
        // read-only turn or a pure conversation turn).
        if self.config.skip_when_no_writes && !self.had_file_writes {
            return ContinuationDecision::Stop { note: None };
        }

        // If we already exhausted retries, stop with the failure note.
        if self.retries >= self.config.max_retries {
            return ContinuationDecision::Stop {
                note: Some(format!(
                    "Auto-fix exhausted after {} attempts. Manual review needed.",
                    self.config.max_retries
                )),
            };
        }

        let mut failures: Vec<String> = Vec::new();
        let mut success_count = 0u32;

        // ---- Run tests ----
        if self.config.auto_test && !self.config.test_command.is_empty() {
            let result = run_command(
                &self.config.test_command,
                &self.config.project_root,
                self.config.command_timeout_secs,
            );
            if result.passed {
                success_count += 1;
            } else {
                failures.push(format!(
                    "Test failures ({}):\n{}",
                    result.command,
                    truncate_output(&result.output, 2500),
                ));
            }
        }

        // ---- Run lints ----
        if self.config.auto_lint && !self.config.lint_command.is_empty() {
            let result = run_command(
                &self.config.lint_command,
                &self.config.project_root,
                self.config.command_timeout_secs,
            );
            if result.passed {
                success_count += 1;
            } else {
                failures.push(format!(
                    "Lint failures ({}):\n{}",
                    result.command,
                    truncate_output(&result.output, 2500),
                ));
            }
        }

        // ---- Decision ----
        if failures.is_empty() {
            // All checks passed.
            let note = if success_count > 0 {
                format!("✓ All {} checks passed.", success_count)
            } else {
                // No checks configured — nothing to verify.
                String::new()
            };
            ContinuationDecision::Stop {
                note: if note.is_empty() { None } else { Some(note) },
            }
        } else {
            // Failures detected — inject continuation message.
            // The retry count is incremented (new_retries = self.retries + 1)
            // and passed to the follow-up message so the model knows how many
            // attempts remain.
            let new_retries = self.retries + 1;
            let attempts_remaining = self.config.max_retries.saturating_sub(new_retries);
            let fix_message = format!(
                "Your recent code changes caused the following failures \
                 (auto-fix attempt {}/{} — {} attempts remaining after this one).\n\
                 Fix ALL of these issues in your next response:\n\n{}",
                new_retries,
                self.config.max_retries,
                attempts_remaining,
                failures.join("\n\n"),
            );

            ContinuationDecision::Continue {
                message: fix_message,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> TurnEndContext<'static> {
        TurnEndContext {
            session_id: "test",
            total_tokens_used: 0,
            turn_elapsed_secs: 1,
        }
    }

    fn test_sandbox() -> Box<dyn SandboxRunner> {
        Box::new(DirectSandbox::new(std::env::temp_dir()))
    }

    #[test]
    fn verify_policy_skips_when_disabled() {
        let mut policy = VerifyPolicy::new(
            VerifyPolicyConfig { enabled: false, ..Default::default() },
            test_sandbox(),
        );
        let decision = policy.decide(&ctx());
        assert!(matches!(decision, ContinuationDecision::Stop { note: None }));
        assert_eq!(policy.retries, 0); // unchanged
    }

    #[test]
    fn verify_policy_skips_when_no_file_writes() {
        let mut policy = VerifyPolicy::new(
            VerifyPolicyConfig { skip_when_no_writes: true, ..Default::default() },
            test_sandbox(),
        );
        let decision = policy.decide(&ctx());
        assert!(matches!(decision, ContinuationDecision::Stop { note: None }));
    }

    #[test]
    fn verify_policy_skips_when_no_commands_configured() {
        let mut policy = VerifyPolicy::new(
            VerifyPolicyConfig {
                auto_test: false,
                auto_lint: false,
                skip_when_no_writes: false,
                ..Default::default()
            },
            test_sandbox(),
        );
        policy.mark_file_write(PathBuf::from("src/lib.rs"));
        let decision = policy.decide(&ctx());
        assert!(matches!(decision, ContinuationDecision::Stop { note: None }));
    }

    #[test]
    fn verify_policy_continues_on_failure_and_tracks_retries() {
        let mut policy = VerifyPolicy::new(
            VerifyPolicyConfig {
                test_command: "exit 1".to_string(),
                auto_lint: false,
                skip_when_no_writes: false,
                project_root: std::env::temp_dir(),
                ..Default::default()
            },
            test_sandbox(),
        );
        policy.mark_file_write(PathBuf::from("src/lib.rs"));

        // Attempt 1: should continue
        let decision = policy.decide(&ctx());
        match &decision {
            ContinuationDecision::Continue { message } => {
                assert!(message.contains("auto-fix attempt 1/3"));
                assert!(message.contains("failures"));
            }
            other => panic!("Expected Continue, got {:?}", other),
        }
        assert_eq!(policy.retries, 1);

        // Attempt 2: should continue
        let decision = policy.decide(&ctx());
        assert!(matches!(decision, ContinuationDecision::Continue { .. }));
        assert_eq!(policy.retries, 2);

        // Attempt 3: should continue (last one)
        let decision = policy.decide(&ctx());
        assert!(matches!(decision, ContinuationDecision::Continue { .. }));
        assert_eq!(policy.retries, 3);

        // Attempt 4: should stop (max retries exhausted)
        let decision = policy.decide(&ctx());
        match decision {
            ContinuationDecision::Stop { note } => {
                assert!(note.unwrap().contains("exhausted"));
            }
            other => panic!("Expected Stop, got {:?}", other),
        }
        assert_eq!(policy.retries, 0); // reset after Stop
    }

    #[test]
    fn verify_policy_passes_and_resets_on_success() {
        let mut policy = VerifyPolicy::new(
            VerifyPolicyConfig {
                test_command: "exit 0".to_string(),
                auto_lint: false,
                skip_when_no_writes: false,
                project_root: std::env::temp_dir(),
                ..Default::default()
            },
            test_sandbox(),
        );
        policy.mark_file_write(PathBuf::from("src/lib.rs"));

        let decision = policy.decide(&ctx());
        match decision {
            ContinuationDecision::Stop { note } => {
                assert!(note.unwrap().contains("passed"));
            }
            other => panic!("Expected Stop, got {:?}", other),
        }
        assert_eq!(policy.retries, 0); // reset after success
        assert!(!policy.had_file_writes); // cleared
    }

    #[test]
    fn verify_policy_resets_state_on_stop() {
        let mut policy = VerifyPolicy::new(
            VerifyPolicyConfig {
                test_command: "exit 0".to_string(),
                auto_lint: false,
                skip_when_no_writes: false,
                project_root: std::env::temp_dir(),
                ..Default::default()
            },
            test_sandbox(),
        );
        policy.mark_file_write(PathBuf::from("src/a.rs"));
        policy.mark_file_write(PathBuf::from("src/b.rs"));
        assert_eq!(policy.modified_files.len(), 2);
        assert!(policy.had_file_writes);

        policy.decide(&ctx()); // succeeds, stops, resets

        assert!(!policy.had_file_writes);
        assert!(policy.modified_files.is_empty());
        assert_eq!(policy.retries, 0);
    }
}
```

### C.6 ContinuationMode Extension

```rust
// crates/query/src/continuation.rs — add to ContinuationMode enum

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContinuationMode {
    #[default]
    Default,
    Goal,
    /// Execute-and-verify mode: after the agent completes a turn, run tests
    /// and linters, auto-fix failures up to N times.
    Verify,
}

impl ContinuationMode {
    pub fn policy(self) -> Box<dyn ContinuationPolicy> {
        match self {
            ContinuationMode::Default => Box::new(StopPolicy),
            ContinuationMode::Goal => Box::new(GoalPolicy),
            ContinuationMode::Verify => {
                // VerifyPolicy is constructed from config at call site.
                // This default returns a no-op policy. The real policy is
                // built by the query loop from QueryConfig.verify.
                Box::new(StopPolicy)
            }
        }
    }
}
```

**Actual policy construction** happens in `run_query_loop`:

```rust
// crates/query/src/lib.rs — in run_query_loop, near continuation_policy construction

let verify_policy: Option<VerifyPolicy> = if matches!(
    config.continuation,
    crate::continuation::ContinuationMode::Verify
) {
    let vp = VerifyPolicy::new(VerifyPolicyConfig {
        enabled: tool_ctx.config.verify.enabled,
        max_retries: tool_ctx.config.verify.max_retries,
        sandbox: tool_ctx.config.verify.sandbox.clone(),
        auto_lint: tool_ctx.config.verify.auto_lint,
        auto_test: tool_ctx.config.verify.auto_test,
        skip_when_no_writes: tool_ctx.config.verify.skip_when_no_writes,
        project_root: tool_ctx.working_dir.clone(),
        test_command: detected_test_command,
        lint_command: detected_lint_command,
        ..Default::default()
    });
    Some(vp)
} else {
    None
};

// Then, in the continue_or_end! macro or equivalent, before consulting
// continuation_policy, update the verify policy's file-write state:
if let Some(ref mut vp) = verify_policy {
    // If any Write/Edit/BatchEdit tool was called this turn, mark it.
    if tools_called_this_turn.iter().any(|name| {
        matches!(name.as_str(), "Write" | "Edit" | "BatchEdit" | "NotebookEdit" | "ApplyPatch")
    }) {
        vp.mark_file_write();
    }
}
```

### C.7 QueryEvent Extensions for TUI

```rust
// crates/query/src/lib.rs — add to QueryEvent enum

pub enum QueryEvent {
    // ... existing variants ...

    /// A verification command started (test or lint).
    VerifyStart {
        command: String,
        attempt: u32,
        max_attempts: u32,
    },
    /// A verification command completed.
    VerifyEnd {
        command: String,
        passed: bool,
        output_summary: String,
    },
    /// The verify loop finished (all passed or retries exhausted).
    VerifyComplete {
        all_passed: bool,
        attempts_used: u32,
        summary: String,
    },
}
```

### C.8 TUI Rendering Integration

The TUI renders verification status inline in the transcript, between the
assistant's message and the next turn. This follows the existing pattern of
`SystemAnnotation` / `SystemMessageStyle::Compact`.

```rust
// crates/tui/src/render.rs — new rendering block

/// Render the verify-loop status block.
/// Shows current command being run with spinner, or final pass/fail result.
fn render_verify_status(
    area: ratatui::layout::Rect,
    frame: &mut ratatui::Frame,
    state: &VerifyRenderState,
    palette: &ColorPalette,
) {
    use ratatui::widgets::{Paragraph, Wrap};
    use ratatui::style::{Color, Modifier, Style};

    let (text, color) = match state {
        VerifyRenderState::Idle => return,
        VerifyRenderState::Running { command, attempt, max_attempts } => {
            let spinner = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            let frame_idx = (now.as_millis() / 100) as usize % spinner.len();
            (
                format!(" {} {}\n   (attempt {}/{})", spinner[frame_idx], command, attempt, max_attempts),
                palette.text_light,
            )
        }
        VerifyRenderState::Passed { summary } => {
            (format!(" ✓ {}", summary), palette.success)
        }
        VerifyRenderState::Failed { summary } => {
            (format!(" ✗ {}", summary), palette.error)
        }
    };

    let block = ratatui::widgets::Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_style(Style::default().fg(color))
        .title(" Verify ");

    let para = Paragraph::new(text.as_str())
        .block(block)
        .style(Style::default().fg(color))
        .wrap(Wrap { trim: false });

    frame.render_widget(para, area);
}
```

### C.9 App State Fields

```rust
// crates/tui/src/app.rs — new fields on App

pub struct App {
    // ... existing fields ...

    /// Verification loop state for rendering.
    pub verify_state: VerifyRenderState,
    /// Last verify result to show as status.
    pub verify_status_message: Option<String>,
}

#[derive(Debug, Clone)]
pub enum VerifyRenderState {
    Idle,
    Running {
        command: String,
        attempt: u32,
        max_attempts: u32,
    },
    Passed {
        summary: String,
    },
    Failed {
        summary: String,
    },
}
```

### C.10 CLI Wiring

```rust
// crates/cli/src/main.rs — startup wiring

// After constructing the ToolContext, detect project info for verify loop:
let project_info = if config.verify.enabled {
    Some(clawde_tools::detect_project::detect_project_info(&working_dir))
} else {
    None
};

// Build QueryConfig with verify mode when configured:
let query_config = QueryConfig {
    continuation: if config.verify.enabled {
        ContinuationMode::Verify
    } else {
        ContinuationMode::Default
    },
    // ... other fields ...
};

// Wire verify status events to TUI:
if let Some(ref tx) = event_tx {
    let tx_clone = tx.clone();
    // The verify policy emits QueryEvents through a channel;
    // the TUI app handles VerifyStart/VerifyEnd/VerifyComplete to update
    // app.verify_state and app.verify_status_message.
}
```

### C.11 Sandbox Modes (Full Implementation)

The verify loop supports three sandbox modes via the `SandboxRunner` trait.
The `DirectSandbox` runs commands in the project directory itself. The
`WorktreeSandbox` creates a temporary git worktree, copies only the agent's
modified files, runs tests there, and cleans up — providing clean isolation
without the overhead of containers.

#### C.11.1 SandboxRunner Trait

```rust
// crates/query/src/sandbox.rs (new)

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Result of running a single verification command in a sandbox.
#[derive(Debug, Clone)]
pub struct CommandResult {
    pub passed: bool,
    pub exit_code: Option<i32>,
    pub output: String,
    pub command: String,
}

impl CommandResult {
    pub fn error(cmd: &str, msg: impl Into<String>) -> Self {
        Self {
            passed: false,
            exit_code: None,
            output: msg.into(),
            command: cmd.to_string(),
        }
    }
}

/// Trait for sandboxed command execution.
/// All implementations are synchronous (blocking) to fit the
/// `ContinuationPolicy::decide()` contract — the query loop is paused
/// during verification anyway.
pub trait SandboxRunner: Send + Sync {
    /// Run a shell command inside the sandbox and return the result.
    fn run(&self, cmd_str: &str, timeout_secs: u64) -> CommandResult;

    /// Human-readable name of this sandbox mode.
    fn name(&self) -> &str;

    /// Whether this sandbox needs setup before first use and teardown after.
    /// When true, `setup()` is called once before all commands and `teardown()`
    /// is called once after.
    fn needs_lifecycle(&self) -> bool {
        false
    }

    /// Called once before the first command. Default no-op.
    fn setup(&mut self) -> Result<(), String> {
        Ok(())
    }

    /// Called once after the last command (even on error). Default no-op.
    fn teardown(&mut self) {}
}
```

#### C.11.2 DirectSandbox

The simplest mode: runs commands directly in the project directory.

```rust
/// Runs commands directly in the project directory. Fastest, zero setup,
/// but test runs may have side effects (build artifacts, cache mutations).
pub struct DirectSandbox {
    project_root: PathBuf,
}

impl DirectSandbox {
    pub fn new(project_root: PathBuf) -> Self {
        Self { project_root }
    }
}

impl SandboxRunner for DirectSandbox {
    fn run(&self, cmd_str: &str, timeout_secs: u64) -> CommandResult {
        run_command_direct(cmd_str, &self.project_root, timeout_secs)
    }

    fn name(&self) -> &str {
        "direct"
    }
}
```

#### C.11.3 WorktreeSandbox

The primary isolation mode. Workflow:

1. `setup()`: create a temp git worktree from the current HEAD
2. For each command: copy the agent's modified files into the worktree, run
   the command there
3. `teardown()`: remove the worktree and its branch, clean up temp directory

**Key design decisions:**
- Worktree is created from HEAD, NOT from the working tree. This means the
  sandbox gets a clean checkout without any uncommitted changes (except the
  agent's modifications, which are explicitly copied in).
- Modified files are copied from the project root into the worktree using
  relative paths. Only files listed in `modified_files` are copied.
- The worktree directory is `.clawde/verify-worktrees/<pid>-<branch>` inside
  the project root. Using `.clawde/` keeps it with the project; the pid
  prevents collisions between concurrent Clawde sessions.
- Cleanup always runs — the worktree and branch are removed even if the
  command fails or panics.
- Stale worktrees from crashed sessions are pruned on setup via
  `git worktree prune` and `find -maxdepth 1 -name 'clawde-verify-*' | xargs rm -rf`.

```rust
/// Runs commands in a temporary git worktree with only the agent's modified
/// files copied in. Provides clean isolation from uncommitted changes and
/// build artifacts in the main working tree.
pub struct WorktreeSandbox {
    /// The project's git repository root.
    project_root: PathBuf,
    /// Files the agent modified this turn (relative to project_root).
    modified_files: Vec<PathBuf>,
    /// Git branch name for the temp worktree.
    branch: String,
    /// Path to the worktree checkout on disk.
    worktree_path: PathBuf,
    /// Whether setup() has been called.
    initialized: bool,
}

impl WorktreeSandbox {
    /// Create a new worktree sandbox.
    ///
    /// `modified_files` should be relative to `project_root`. Only these
    /// files will be copied into the worktree before commands run. This
    /// should come from `FileHistory::recent_modifications()` or from
    /// tracking which Write/Edit/ApplyPatch tools were called this turn.
    pub fn new(project_root: PathBuf, modified_files: Vec<PathBuf>) -> Self {
        let branch = format!("clawde-verify-{}", std::process::id());
        let worktree_path = project_root
            .join(".clawde")
            .join("verify-worktrees")
            .join(&branch);

        Self {
            project_root,
            modified_files,
            branch,
            worktree_path,
            initialized: false,
        }
    }
}

impl SandboxRunner for WorktreeSandbox {
    fn run(&self, cmd_str: &str, timeout_secs: u64) -> CommandResult {
        if !self.initialized {
            return CommandResult::error(
                cmd_str,
                "WorktreeSandbox::setup() must be called before run()",
            );
        }

        // Copy modified files from the main tree into the worktree.
        // This is done per-command rather than once in setup() because a
        // previous auto-fix attempt may have modified files further.
        for file in &self.modified_files {
            let src = self.project_root.join(file);
            if src.exists() {
                let dest = self.worktree_path.join(file);
                if let Some(parent) = dest.parent() {
                    if let Err(e) = std::fs::create_dir_all(parent) {
                        tracing::warn!(
                            "verify sandbox: failed to create dir {}: {}",
                            parent.display(),
                            e
                        );
                    }
                }
                if let Err(e) = std::fs::copy(&src, &dest) {
                    tracing::warn!(
                        "verify sandbox: failed to copy {} -> {}: {}",
                        src.display(),
                        dest.display(),
                        e
                    );
                }
            }
        }

        run_command_direct(cmd_str, &self.worktree_path, timeout_secs)
    }

    fn name(&self) -> &str {
        "worktree"
    }

    fn needs_lifecycle(&self) -> bool {
        true
    }

    fn setup(&mut self) -> Result<(), String> {
        if self.initialized {
            return Ok(());
        }

        // ---- Pre-flight: verify we're in a git repo ----
        let root_check = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(&self.project_root)
            .output();

        match root_check {
            Ok(o) if o.status.success() => {
                let top = String::from_utf8_lossy(&o.stdout).trim().to_string();
                tracing::debug!("verify sandbox: git toplevel = {}", top);
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                return Err(format!(
                    "Cannot create verify worktree: '{}' is not inside a git repository. {}",
                    self.project_root.display(),
                    stderr.trim()
                ));
            }
            Err(e) => {
                return Err(format!("git not found: {}", e));
            }
        }

        // ---- Clean up stale worktrees from crashed sessions ----
        // git worktree prune removes stale administrative entries.
        let _ = Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(&self.project_root)
            .output();

        // Remove any orphaned worktree directories from previous crashed
        // sessions (these wouldn't show up in `git worktree list`).
        let verify_dir = self.project_root.join(".clawde").join("verify-worktrees");
        if verify_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&verify_dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    // Only clean directories that match our naming pattern
                    if name_str.starts_with("clawde-verify-") && entry.path().is_dir() {
                        // Check if this worktree is still tracked by git
                        let list = Command::new("git")
                            .args(["worktree", "list", "--porcelain"])
                            .current_dir(&self.project_root)
                            .output()
                            .ok()
                            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                            .unwrap_or_default();

                        let path_str = entry.path().to_string_lossy();
                        if !list.contains(path_str.as_ref()) {
                            // Not tracked by git — safe to remove
                            tracing::debug!(
                                "verify sandbox: removing stale worktree dir {}",
                                path_str
                            );
                            let _ = std::fs::remove_dir_all(entry.path());
                        }
                    }
                }
            }
        }

        // ---- Create the worktree ----
        let worktree_str = self.worktree_path.to_string_lossy();
        let result = Command::new("git")
            .args(["worktree", "add", "-b", &self.branch, &worktree_str])
            .current_dir(&self.project_root)
            .output();

        match result {
            Ok(o) if o.status.success() => {
                tracing::info!(
                    "verify sandbox: created worktree at {} on branch {}",
                    worktree_str,
                    self.branch
                );
                self.initialized = true;
                Ok(())
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                let err_msg = stderr.trim().to_string();
                // Common errors with helpful messages
                if err_msg.to_lowercase().contains("already exists") {
                    // Branch already exists — try to remove the stale branch first
                    let _ = Command::new("git")
                        .args(["branch", "-D", &self.branch])
                        .current_dir(&self.project_root)
                        .output();
                    let _ = std::fs::remove_dir_all(&self.worktree_path);

                    // Retry once
                    let retry = Command::new("git")
                        .args(["worktree", "add", "-b", &self.branch, &worktree_str])
                        .current_dir(&self.project_root)
                        .output();

                    match retry {
                        Ok(o) if o.status.success() => {
                            tracing::info!(
                                "verify sandbox: created worktree (after cleanup) at {}",
                                worktree_str
                            );
                            self.initialized = true;
                            return Ok(());
                        }
                        Ok(o) => {
                            return Err(format!(
                                "Failed to create verify worktree (after cleanup): {}",
                                String::from_utf8_lossy(&o.stderr).trim()
                            ));
                        }
                        Err(e) => {
                            return Err(format!(
                                "Failed to create verify worktree (after cleanup): {}",
                                e
                            ));
                        }
                    }
                }
                Err(format!(
                    "Failed to create verify worktree at {}: {}",
                    worktree_str, err_msg
                ))
            }
            Err(e) => Err(format!("Failed to run git worktree add: {}", e)),
        }
    }

    fn teardown(&mut self) {
        if !self.initialized {
            return;
        }

        let worktree_str = self.worktree_path.to_string_lossy();

        // Remove the git worktree registration
        let remove = Command::new("git")
            .args(["worktree", "remove", "--force", &worktree_str])
            .current_dir(&self.project_root)
            .output();

        match remove {
            Ok(o) if o.status.success() => {
                tracing::info!("verify sandbox: removed worktree {}", worktree_str);
            }
            _ => {
                // Best-effort: force-remove the directory even if git
                // worktree remove failed.
                tracing::warn!(
                    "verify sandbox: git worktree remove failed for {} — force-removing directory",
                    worktree_str
                );
                let _ = std::fs::remove_dir_all(&self.worktree_path);
            }
        }

        // Delete the temporary branch
        let _ = Command::new("git")
            .args(["branch", "-D", &self.branch])
            .current_dir(&self.project_root)
            .output();

        // Clean up any leftover directory
        let _ = std::fs::remove_dir_all(&self.worktree_path);

        self.initialized = false;
    }
}

impl Drop for WorktreeSandbox {
    fn drop(&mut self) {
        // Safety net: if teardown() was never called (e.g. panic), try to
        // clean up here. We can't do much error handling in Drop, but a
        // best-effort cleanup is better than leaving stale worktrees.
        if self.initialized {
            let worktree_str = self.worktree_path.to_string_lossy();
            let _ = Command::new("git")
                .args(["worktree", "remove", "--force", &worktree_str])
                .current_dir(&self.project_root)
                .output();
            let _ = Command::new("git")
                .args(["branch", "-D", &self.branch])
                .current_dir(&self.project_root)
                .output();
            let _ = std::fs::remove_dir_all(&self.worktree_path);
        }
    }
}
```

#### C.11.4 Shared Helper: run_command_direct

```rust
/// Run a shell command and capture its output. This is the core execution
/// function used by both DirectSandbox and WorktreeSandbox.
///
/// Uses `std::process::Command` (blocking) — the verify loop runs between
/// turns when the query loop is already paused, so blocking is correct.
fn run_command_direct(cmd_str: &str, cwd: &Path, timeout_secs: u64) -> CommandResult {
    let parts: Vec<&str> = if cfg!(target_os = "windows") {
        vec!["cmd", "/C", cmd_str]
    } else {
        vec!["sh", "-c", cmd_str]
    };

    let mut child = match Command::new(parts[0])
        .args(&parts[1..])
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return CommandResult {
                passed: false,
                exit_code: None,
                output: format!("Failed to spawn command '{}': {}", cmd_str, e),
                command: cmd_str.to_string(),
            };
        }
    };

    // Wait with a timeout. Use wait_timeout from std (available since Rust 1.64).
    // On timeout, kill the process and report the timeout.
    let timeout = Duration::from_secs(timeout_secs);
    let start = std::time::Instant::now();

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Process finished — collect output
                let stdout = child
                    .stdout
                    .take()
                    .map(|_| Vec::new())
                    .unwrap_or_default();
                let stderr = child
                    .stderr
                    .take()
                    .map(|_| Vec::new())
                    .unwrap_or_default();

                // Actually read the output — we need to re-collect since take() consumed it
                // Use a fresh command to get output from the completed process
                drop(child);

                // Re-run to capture output. The process already completed, so
                // we run again to get stdout/stderr. This sounds wasteful but
                // is the simplest approach — verification commands are typically
                // fast (1-5 seconds) and this only happens after success/failure.
                let output = Command::new(parts[0])
                    .args(&parts[1..])
                    .current_dir(cwd)
                    .output();

                let (stdout, stderr, exit_code) = match output {
                    Ok(o) => (
                        String::from_utf8_lossy(&o.stdout).to_string(),
                        String::from_utf8_lossy(&o.stderr).to_string(),
                        o.status.code(),
                    ),
                    Err(e) => (
                        String::new(),
                        format!("Failed to read output: {}", e),
                        None,
                    ),
                };

                let combined = format!("{}\n{}", stdout, stderr).trim().to_string();

                return CommandResult {
                    passed: status.success(),
                    exit_code,
                    output: combined,
                    command: cmd_str.to_string(),
                };
            }
            Ok(None) => {
                // Still running — check timeout
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return CommandResult {
                        passed: false,
                        exit_code: None,
                        output: format!(
                            "Command timed out after {}s: {}",
                            timeout_secs, cmd_str
                        ),
                        command: cmd_str.to_string(),
                    };
                }
                // Sleep briefly to avoid busy-waiting
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                return CommandResult {
                    passed: false,
                    exit_code: None,
                    output: format!("Error waiting for command '{}': {}", cmd_str, e),
                    command: cmd_str.to_string(),
                };
            }
        }
    }
}
```

#### C.11.5 Sandbox Factory

```rust
/// Build the appropriate sandbox from VerifyConfig and the turn's state.
pub fn build_sandbox(
    config: &clawde_core::config::VerifyConfig,
    project_root: PathBuf,
    modified_files: Vec<PathBuf>,
) -> Box<dyn SandboxRunner> {
    match config.sandbox {
        clawde_core::config::VerifySandbox::Direct => {
            Box::new(DirectSandbox::new(project_root))
        }
        clawde_core::config::VerifySandbox::Worktree => {
            Box::new(WorktreeSandbox::new(project_root, modified_files))
        }
        clawde_core::config::VerifySandbox::Container => {
            // Stub for future implementation. Falls back to Direct with a
            // warning so the user isn't blocked on an unimplemented mode.
            tracing::warn!(
                "verify sandbox: container mode not yet implemented, falling back to direct"
            );
            Box::new(DirectSandbox::new(project_root))
        }
    }
}
```

#### C.11.6 Updated VerifyPolicy with `&mut self` ContinuationPolicy

**Design decision:** The `ContinuationPolicy` trait is changed from `fn decide(&self, ...)`
to `fn decide(&mut self, ...)`. This is the right call because:

1. **Minimal impact** — 2 existing impls (StopPolicy, GoalPolicy) + 1 call site in the
   query loop + 2 tests. All changes are trivial (add `mut`).
2. **Crate-internal trait** — Zero external consumers. Safe to change.
3. **Idiomatic Rust** — `Iterator::next(&mut self)` is the precedent for stateful,
   single-threaded computation. `Cell`/`RefCell` are workarounds for shared references,
   not a design pattern.
4. **Future-proof** — SpecPolicy, ComparePolicy, and other planned policies will also
   need mutable state.
5. **Clean VerifyPolicy** — No `Cell<u32>`, `RefCell<Vec<PathBuf>>`, or `Cell<bool>`
   clutter. Just plain fields with `&mut self` access.

**Trait change (3 lines in continuation.rs):**

```rust
pub trait ContinuationPolicy: Send + Sync {
    fn decide(&mut self, ctx: &TurnEndContext<'_>) -> ContinuationDecision;
}

impl ContinuationPolicy for StopPolicy {
    fn decide(&mut self, _ctx: &TurnEndContext<'_>) -> ContinuationDecision {
        ContinuationDecision::Stop { note: None }
    }
}

impl ContinuationPolicy for GoalPolicy {
    fn decide(&mut self, ctx: &TurnEndContext<'_>) -> ContinuationDecision {
        // ... same body, just &mut self ...
    }
}
```

**Call site change (1 line in lib.rs):**

```rust
// Before:
let continuation_policy = config.continuation.policy();
// After:
let mut continuation_policy = config.continuation.policy();
```

**Test changes (2 lines in continuation.rs):**

```rust
fn stop_policy_always_stops() {
    let mut policy = StopPolicy;  // add `mut`
    let decision = policy.decide(&ctx());
    // ...
}

fn default_mode_resolves_to_stop() {
    let mut policy = ContinuationMode::default().policy();  // add `mut`
    assert!(!policy.decide(&ctx()).is_continue());
}
```

**Updated VerifyPolicy with plain mutable fields:**

```rust
// crates/query/src/verify.rs — final VerifyPolicy

pub struct VerifyPolicy {
    config: VerifyPolicyConfig,
    sandbox: Box<dyn SandboxRunner>,
    sandbox_ready: bool,
    retries: u32,
    had_file_writes: bool,
    modified_files: Vec<PathBuf>,
}

impl VerifyPolicy {
    pub fn new(config: VerifyPolicyConfig, sandbox: Box<dyn SandboxRunner>) -> Self {
        Self {
            config,
            sandbox,
            sandbox_ready: false,
            retries: 0,
            had_file_writes: false,
            modified_files: Vec::new(),
        }
    }

    /// Call when the agent writes/edits a file so the policy tracks it.
    /// `file_path` is relative to the project root.
    pub fn mark_file_write(&mut self, file_path: PathBuf) {
        self.had_file_writes = true;
        self.modified_files.push(file_path);
    }

    /// Reset tracking between turns (called after the policy consumes state).
    fn reset_for_next_turn(&mut self) {
        self.had_file_writes = false;
        self.modified_files.clear();
    }
}

impl ContinuationPolicy for VerifyPolicy {
    fn decide(&mut self, ctx: &TurnEndContext<'_>) -> ContinuationDecision {
        if !self.config.enabled {
            return ContinuationDecision::Stop { note: None };
        }

        if self.config.skip_when_no_writes && !self.had_file_writes {
            return ContinuationDecision::Stop { note: None };
        }

        if self.retries >= self.config.max_retries {
            // Reset state before returning so next turn starts fresh
            let note = Some(format!(
                "Auto-fix exhausted after {} attempts. Manual review needed.",
                self.config.max_retries
            ));
            self.reset_for_next_turn();
            self.retries = 0;
            return ContinuationDecision::Stop { note };
        }

        // ---- Sandbox lifecycle ----
        if self.sandbox.needs_lifecycle() && !self.sandbox_ready {
            if let Err(e) = self.sandbox.setup() {
                // Sandbox setup failed. Surface error and stop.
                // The factory already fell back to DirectSandbox if possible,
                // so this path is for hard failures (e.g. no git for worktree).
                self.reset_for_next_turn();
                self.retries = 0;
                return ContinuationDecision::Stop {
                    note: Some(format!("Verify sandbox setup failed: {}", e)),
                };
            }
            self.sandbox_ready = true;
        }

        let mut failures: Vec<String> = Vec::new();
        let mut success_count = 0u32;

        // ---- Run tests ----
        if self.config.auto_test && !self.config.test_command.is_empty() {
            let result = self.sandbox.run(
                &self.config.test_command,
                self.config.command_timeout_secs,
            );
            if result.passed {
                success_count += 1;
            } else {
                failures.push(format!(
                    "Test failures ({}):\n{}",
                    result.command,
                    truncate_output(&result.output, 2500),
                ));
            }
        }

        // ---- Run lints ----
        if self.config.auto_lint && !self.config.lint_command.is_empty() {
            let result = self.sandbox.run(
                &self.config.lint_command,
                self.config.command_timeout_secs,
            );
            if result.passed {
                success_count += 1;
            } else {
                failures.push(format!(
                    "Lint failures ({}):\n{}",
                    result.command,
                    truncate_output(&result.output, 2500),
                ));
            }
        }

        // ---- Decision ----
        if failures.is_empty() {
            // All checks passed. Reset state for next turn and stop.
            let note = if success_count > 0 {
                Some(format!(
                    "All {} checks passed in {} sandbox.",
                    success_count,
                    self.sandbox.name()
                ))
            } else {
                None
            };
            self.reset_for_next_turn();
            self.retries = 0;
            ContinuationDecision::Stop { note }
        } else {
            // Failures detected. Increment retries and continue.
            self.retries += 1;
            let remaining = self.config.max_retries.saturating_sub(self.retries);
            // Don't reset file tracking — the next auto-fix turn will
            // produce new writes and we need fresh tracking for it.
            self.had_file_writes = false;
            self.modified_files.clear();

            let fix_message = format!(
                "Your recent code changes caused the following failures \
                 (auto-fix attempt {}/{} — {} attempts remaining, running in {} sandbox).\n\
                 Fix ALL of these issues in your next response:\n\n{}",
                self.retries,
                self.config.max_retries,
                remaining,
                self.sandbox.name(),
                failures.join("\n\n"),
            );
            ContinuationDecision::Continue {
                message: fix_message,
            }
        }
    }
}

impl Drop for VerifyPolicy {
    fn drop(&mut self) {
        if self.sandbox_ready {
            self.sandbox.teardown();
        }
    }
}
```

**Key state machine observations:**

1. `retries` starts at 0. Each failing `decide()` increments it and returns `Continue`.
   When `retries >= max_retries`, it resets and returns `Stop` with an error note.
2. `had_file_writes` / `modified_files` track the turn that JUST completed.
   They're cleared after a `Continue` (so the next auto-fix turn has fresh tracking)
   and after a `Stop` (so the next user-initiated turn starts clean).
3. `sandbox_ready` is set once on first `decide()` that actually runs commands.
   The sandbox is torn down in `Drop` (defense in depth) or explicitly by the
   caller on loop exit.
4. On sandbox setup failure, the policy resets and stops with a clear error —
   no state leak, no stale retry counter.

#### C.11.7 Wiring: Query Loop Integration (with `&mut self`)

```rust
// crates/query/src/lib.rs — in run_query_loop

// Build continuation policy from config.
// Now mutable because policies may carry state (retries, file tracking).
let mut continuation_policy = config.continuation.policy();

// If verify mode, build the verify policy and use it INSTEAD of the
// default continuation policy. The verify policy wraps the sandbox and
// handles the test/lint/retry lifecycle.
//
// We don't use ContinuationMode::Verify → policy() for this because the
// VerifyPolicy needs a sandbox (built from config + project state), not
// just a no-arg constructor. Instead we store it as a separate object and
// consult it in continue_or_end!.
let mut verify_policy: Option<VerifyPolicy> = if matches!(
    config.continuation,
    crate::continuation::ContinuationMode::Verify
) {
    let vcfg = &tool_ctx.config.verify;
    let modified_files = Vec::new(); // populated per-turn by mark_file_write()
    let sandbox = crate::sandbox::build_sandbox(
        vcfg,
        tool_ctx.working_dir.clone(),
        modified_files,
    );

    Some(VerifyPolicy::new(
        VerifyPolicyConfig {
            enabled: vcfg.enabled,
            max_retries: vcfg.max_retries,
            auto_lint: vcfg.auto_lint,
            auto_test: vcfg.auto_test,
            skip_when_no_writes: vcfg.skip_when_no_writes,
            project_root: tool_ctx.working_dir.clone(),
            test_command: detected_test_command,
            lint_command: detected_lint_command,
            ..Default::default()
        },
        sandbox,
    ))
} else {
    None
};

// ... inside the loop, after tool execution ...

// For each Write/Edit/ApplyPatch tool call, tell the verify policy:
if let Some(ref mut vp) = verify_policy {
    let tool_name = /* name of the tool just executed */;
    if matches!(tool_name, "Write" | "Edit" | "BatchEdit" | "NotebookEdit" | "ApplyPatch") {
        if let Some(fp) = tool_input.get("file_path").and_then(|v| v.as_str()) {
            // Convert to relative path for the sandbox
            let rel = PathBuf::from(fp);
            vp.mark_file_write(rel);
        }
    }
}

// ... at end_turn, in continue_or_end! macro ...

let decision = if let Some(ref mut vp) = verify_policy {
    // Use the verify policy instead of the default continuation policy.
    vp.decide(&crate::continuation::TurnEndContext {
        session_id: &tool_ctx.session_id,
        total_tokens_used: cost_tracker.total_tokens(),
        turn_elapsed_secs: goal_turn_start.elapsed().as_secs(),
    })
} else {
    continuation_policy.decide(&crate::continuation::TurnEndContext {
        session_id: &tool_ctx.session_id,
        total_tokens_used: cost_tracker.total_tokens(),
        turn_elapsed_secs: goal_turn_start.elapsed().as_secs(),
    })
};

match decision {
    crate::continuation::ContinuationDecision::Continue { message } => {
        // Emit verify-specific events for TUI
        if let (Some(ref vp), Some(ref tx)) = (&verify_policy, &event_tx) {
            let _ = tx.send(QueryEvent::Status(format!(
                "Verifying in {} sandbox (attempt {}/{})…",
                vp.sandbox_name(),
                vp.retries + 1,
                vp.config.max_retries,
            )));
        }
        messages.push(Message::user(message));
        turn = 0;
        max_tokens_recovery_count = 0;
        retries_left = 2;
        goal_turn_start = std::time::Instant::now();
        continue;
    }
    crate::continuation::ContinuationDecision::Stop { note } => {
        // Sandbox is cleaned up by VerifyPolicy's Drop impl.
        if let Some(note) = note {
            if let Some(ref tx) = event_tx {
                let _ = tx.send(QueryEvent::Status(note));
            }
        }
        return QueryOutcome::EndTurn {
            message: $assistant_msg,
            usage: $usage,
        };
    }
}
```

**Important:** If the loop returns early (e.g. `Cancelled`, `Error`, `BudgetExceeded`),
the `VerifyPolicy`'s `Drop` impl will call `sandbox.teardown()`. No explicit cleanup
needed at those exit points — the Drop guard handles it.

#### C.11.8 Container Sandbox (Stub)

```rust
/// Placeholder for container-based sandbox (Docker/Podman).
///
/// When implemented, this will:
/// 1. Build or pull a container image with the project's dependencies
/// 2. Mount the project directory (or a copy) into the container
/// 3. Run test/lint commands inside the container
/// 4. Capture output and exit code
/// 5. Clean up the container
///
/// Implementation blocked on: container runtime detection (docker vs podman),
/// image management strategy (pre-built vs on-demand), and user opt-in for
/// the security/privacy implications of mounting code into a container.
pub struct ContainerSandbox {
    project_root: PathBuf,
    runtime: ContainerRuntime,
    image: Option<String>,
}

enum ContainerRuntime {
    Docker,
    Podman,
}

impl SandboxRunner for ContainerSandbox {
    fn run(&self, cmd_str: &str, _timeout_secs: u64) -> CommandResult {
        CommandResult::error(
            cmd_str,
            "Container sandbox not yet implemented. Use 'direct' or 'worktree' sandbox mode.",
        )
    }
    fn name(&self) -> &str { "container" }
}
```

#### C.11.9 Sandbox Module Structure

```
// crates/query/src/sandbox.rs — public API

// Public:
pub use self::runner::{CommandResult, SandboxRunner};
pub use self::direct::DirectSandbox;
pub use self::worktree::WorktreeSandbox;
pub use self::factory::build_sandbox;

// Private:
mod runner;   // CommandResult, SandboxRunner trait
mod direct;   // DirectSandbox
mod worktree; // WorktreeSandbox
mod factory;  // build_sandbox()
mod container; // ContainerSandbox (stub)
```

#### C.11.10 Test Coverage for Sandbox

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn init_git(dir: &Path) {
        run_cmd(dir, "git init");
        run_cmd(dir, "git config user.email test@test.com");
        run_cmd(dir, "git config user.name Test");
        std::fs::write(dir.join("README.md"), b"# test").unwrap();
        run_cmd(dir, "git add .");
        run_cmd(dir, "git commit -m init");
    }

    fn run_cmd(dir: &Path, cmd: &str) {
        let parts: Vec<&str> = cmd.split_whitespace().collect();
        let output = Command::new(parts[0])
            .args(&parts[1..])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(output.status.success(), "{} failed: {}", cmd,
            String::from_utf8_lossy(&output.stderr));
    }

    #[test]
    fn direct_sandbox_runs_command() {
        let tmp = TempDir::new().unwrap();
        let sandbox = DirectSandbox::new(tmp.path().to_path_buf());
        let result = sandbox.run("echo hello", 10);
        assert!(result.passed);
        assert!(result.output.contains("hello"));
    }

    #[test]
    fn direct_sandbox_reports_failure() {
        let sandbox = DirectSandbox::new(std::env::temp_dir());
        let result = sandbox.run("exit 1", 10);
        assert!(!result.passed);
        assert_eq!(result.exit_code, Some(1));
    }

    #[test]
    fn direct_sandbox_times_out() {
        let sandbox = DirectSandbox::new(std::env::temp_dir());
        let result = sandbox.run("sleep 10", 1); // 1s timeout on 10s sleep
        assert!(!result.passed);
        assert!(result.output.contains("timed out"));
    }

    #[test]
    fn worktree_sandbox_creates_and_cleans_up() {
        let tmp = TempDir::new().unwrap();
        init_git(tmp.path());

        // Create a modified file outside the sandbox
        let modified = tmp.path().join("src").join("lib.rs");
        std::fs::create_dir_all(modified.parent().unwrap()).unwrap();
        std::fs::write(&modified, b"fn foo() {}").unwrap();

        let mut sandbox = WorktreeSandbox::new(
            tmp.path().to_path_buf(),
            vec![PathBuf::from("src/lib.rs")],
        );

        // Setup
        sandbox.setup().expect("worktree setup");
        assert!(sandbox.worktree_path.exists());

        // Run a command that reads the copied file
        let result = sandbox.run("cat src/lib.rs", 10);
        assert!(result.passed);
        assert!(result.output.contains("fn foo()"));

        // Teardown
        sandbox.teardown();
        assert!(!sandbox.worktree_path.exists());
    }

    #[test]
    fn worktree_sandbox_fails_without_git_repo() {
        let tmp = TempDir::new().unwrap();
        // Not a git repo
        let mut sandbox = WorktreeSandbox::new(
            tmp.path().to_path_buf(),
            vec![],
        );
        let result = sandbox.setup();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not inside a git repository"));
    }

    #[test]
    fn worktree_sandbox_cleans_up_on_drop() {
        let tmp = TempDir::new().unwrap();
        init_git(tmp.path());

        let mut sandbox = WorktreeSandbox::new(
            tmp.path().to_path_buf(),
            vec![],
        );
        sandbox.setup().expect("worktree setup");
        let worktree_path = sandbox.worktree_path.clone();
        assert!(worktree_path.exists());

        // Drop the sandbox without calling teardown() — Drop should clean up
        drop(sandbox);
        assert!(!worktree_path.exists(), "worktree should be cleaned up on Drop");
    }

    #[test]
    fn worktree_sandbox_isolation() {
        let tmp = TempDir::new().unwrap();
        init_git(tmp.path());

        // Create a dirty file in the main tree that should NOT appear in the worktree
        std::fs::write(tmp.path().join("dirty.rs"), b"dirty").unwrap();

        let mut sandbox = WorktreeSandbox::new(
            tmp.path().to_path_buf(),
            vec![], // no modified files to copy
        );
        sandbox.setup().expect("worktree setup");

        // The worktree should have README.md (from git) but NOT dirty.rs
        assert!(sandbox.worktree_path.join("README.md").exists(), "README.md should exist");
        assert!(!sandbox.worktree_path.join("dirty.rs").exists(), "dirty.rs should NOT leak into sandbox");

        sandbox.teardown();
    }
}
```

### C.12 Registration in all_tools()

```rust
// crates/tools/src/lib.rs — add to all_tools()

pub fn all_tools() -> Vec<Box<dyn Tool>> {
    vec![
        // ... existing tools ...
        Box::new(DetectProjectTool),
        Box::new(RunTestsTool),
        Box::new(RunLintsTool),
    ]
}
```
