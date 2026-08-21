# Agent Improvement Research — Clawde vs. Bleeding Edge

## Executive Summary

After researching Claude Code, Aider, OpenAI Codex, LangChain's latest patterns, and academic work on LLM agents, I've identified **12 high-impact improvements** Clawde could adopt. These fall into three categories: **context management**, **agent autonomy**, and **session intelligence**.

---

## 1. Context Management (Highest Impact)

### 1.1 Resume from Summary (Claude Code pattern)
**Source**: Claude Code sessions docs
**What**: When resuming a session inactive >1 hour and >100K tokens, offer to compact the full history into a summary before continuing. This prevents re-caching expensive full-history requests.
**Clawde gap**: Clawde has `/compact` but doesn't auto-suggest it on resume. Users must manually compact.
**Implementation**: Add a "resume hint" that detects stale large sessions and offers `/compact` on resume.

### 1.2 Path-Scoped Rules (Claude Code pattern)
**Source**: Claude Code memory docs
**What**: Rules in `.claude/rules/` can be scoped to specific file paths via YAML frontmatter. Rules only load when working with matching files.
```yaml
---
paths: ["src/api/**/*.ts"]
---
# API Development Rules
- All API endpoints must include input validation
```
**Clawde gap**: `AGENTS.md` is monolithic — all rules load every session regardless of what files are being edited.
**Implementation**: Add path-scoped rule loading to the system prompt builder.

### 1.3 Dynamic Repo Map (Aider pattern)
**Source**: Aider repomap docs
**What**: A graph-based ranking algorithm that includes only the most relevant class/function signatures from the repo, fitting within a token budget. Dynamically expands when no files are added to chat.
**Clawde gap**: Clawde reads files on demand but doesn't maintain a persistent repo map for context efficiency.
**Implementation**: Build a lightweight repo map that summarizes key symbols and fits in ~1K tokens.

### 1.4 Instruction Pin Enhancement
**Source**: Clawde's own compact.rs
**What**: Already implemented — extracts the most recent user instruction and pins it after compaction.
**Status**: DONE. Could enhance with automatic pin extraction on every turn (not just compact).

---

## 2. Agent Autonomy & Verification

### 2.1 Verification Loops (Claude Code best practices)
**Source**: Claude Code best practices
**What**: Give the agent a check it can run (tests, build, screenshot). Without verification, "looks done" is the only signal.
**Clawde gap**: Clawde has `/verify` but it's manual. No automatic verification loop.
**Implementation**: Add auto-verify mode that runs tests/build after code changes and iterates until pass.

### 2.2 Plan Mode with Goal Tracking
**Source**: Claude Code, LangChain ADLC
**What**: Separate exploration from execution. Plan mode reads files without changes. `/goal` tracks progress against a condition.
**Clawde gap**: No explicit plan mode or goal tracking.
**Implementation**: Add `--plan-mode` flag and `/goal` command that evaluates a condition each turn.

### 2.3 Evaluator-Optimizer Pattern
**Source**: Anthropic "Building Effective Agents"
**What**: One LLM generates, another evaluates. Iterative refinement loop.
**Clawde gap**: Single-model execution only.
**Implementation**: Add optional "reviewer" mode where a second model critiques the output.

### 2.4 Stop Hooks (Claude Code pattern)
**Source**: Claude Code hooks docs
**What**: Deterministic hooks that run after specific actions. Unlike CLAUDE.md instructions, hooks are enforced.
**Clawde gap**: No hook system.
**Implementation**: Add `.clawde/hooks.json` with pre/post tool-use hooks.

---

## 3. Session Intelligence

### 3.1 Session Naming & Search (Claude Code pattern)
**Source**: Claude Code sessions docs
**What**: Auto-generate session titles from first prompt. Search by name, git branch, or PR URL.
**Clawde gap**: Has `/rename` and `/search` but titles are manual.
**Implementation**: Auto-title sessions from first prompt using a small/fast model.

### 3.2 Cross-Session Messaging
**Source**: Claude Code agents docs
**What**: Sessions can message each other to pass findings and status.
**Clawde gap**: Sessions are isolated.
**Implementation**: Add inter-session message passing for coordinated work.

### 3.3 Worktree Isolation
**Source**: Claude Code agents docs
**What**: Each parallel session gets its own git worktree to avoid file conflicts.
**Clawde gap**: No worktree support.
**Implementation**: Auto-create worktrees for parallel sessions.

### 3.4 Auto Memory with Confidence Scoring
**Source**: Claude Code memory docs, Lilian Weng's agent survey
**What**: Claude writes learnings automatically from corrections. Memory has categories (preference, fact, pattern, decision, constraint) and confidence scores.
**Clawde gap**: Has `SessionMemoryExtractor` but doesn't do confidence-based filtering or category-aware merging.
**Implementation**: Add confidence thresholds and deduplication to memory extraction.

---

## 4. Advanced Patterns (Future)

### 4.1 Reflexion Pattern
**Source**: Lilian Weng's agent survey, Shinn & Labash 2023
**What**: Agent reflects on failed trajectories, stores self-critique, and uses it to guide future attempts.
**Implementation**: Add reflection loop when tool calls fail — analyze why, store lesson, retry with adjusted strategy.

### 4.2 Chain-of-Hindsight
**Source**: Liu et al. 2023
**What**: Present model with a sequence of past outputs annotated with feedback, so it learns from its own improving trajectory.
**Implementation**: When compacting, include not just what happened but what went wrong and how it was fixed.

### 4.3 Dynamic Tool Selection
**Source**: Voyager paper, LangChain ADLC
**What**: Agent learns which tools work best for which tasks and adapts its toolset.
**Implementation**: Track tool success rates per task type and dynamically adjust available tools.

---

## 5. Priority Matrix

| # | Improvement | Impact | Effort | Priority |
|---|---|---|---|---|
| 1 | Resume from Summary | High | Low | P0 |
| 2 | Verification Loops | High | Medium | P0 |
| 3 | Dynamic Repo Map | High | High | P1 |
| 4 | Path-Scoped Rules | Medium | Medium | P1 |
| 5 | Plan Mode + Goal Tracking | High | High | P1 |
| 6 | Auto Session Titles | Medium | Low | P1 |
| 7 | Stop Hooks | High | Medium | P1 |
| 8 | Evaluator-Optimizer | Medium | High | P2 |
| 9 | Cross-Session Messaging | Low | High | P2 |
| 10 | Worktree Isolation | Low | High | P2 |
| 11 | Reflexion Pattern | Medium | High | P2 |
| 12 | Dynamic Tool Selection | Medium | High | P3 |

---

## 6. Recommended Next Steps

1. **Implement P0 items** — Resume from Summary and Verification Loops are high-impact, low-effort
2. **Build Dynamic Repo Map** — This is the single biggest context efficiency win
3. **Add Plan Mode** — Separates exploration from execution, reduces wasted tokens
4. **Implement Stop Hooks** — Deterministic enforcement of checks

---

## Sources

- Anthropic. "Building Effective AI Agents" (2024)
- Claude Code Documentation — sessions, memory, hooks, best practices
- Aider — Repository Map architecture
- Lilian Weng. "LLM Powered Autonomous Agents" (2023)
- LangChain. "What is an AI Agent?" (2026)
- Shinn & Labash. "Reflexion: Language Agents with Verbal Reinforcement Learning" (2023)
- Liu et al. "Chain of Hindsight Aligns Language Models with Feedback" (2023)
- Yao et al. "Tree of Thoughts: Deliberate Problem Solving with Large Language Models" (2023)
