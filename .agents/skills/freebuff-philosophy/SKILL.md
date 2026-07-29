# Freebuff Project Philosophy

## Core Principles

### 1. Free Only
Everything must be free. No features that require:
- Credit card signup
- Paywalls or subscriptions
- Paid API tiers
- Any form of payment

Free models, free tools, free MCP servers, free everything.

### 2. High Free Usage for Real Work
Many "free" options are severely limited — a few questions then exhausted. This project needs:
- Enough free usage for real coding projects (multi-hour agent sessions)
- Generous rate limits that support agentic workflows (tool calls, file edits, bash commands)
- Multiple fallback providers so when one free tier is exhausted, the next kicks in
- Aggressive aggregation of every viable free source

### 3. High Quality, Current Research
The LLM landscape changes rapidly (weekly). When researching:
- Look beyond advertisements and surface-level blog posts
- Seek technical data: actual API endpoints, rate limits, model capabilities
- Examine other projects' source code for real implementation patterns
- Verify information is current (not 6+ months old in this space)
- Prefer primary sources (provider docs, GitHub source code) over aggregator summaries

## Implementation Strategy

1. **Maximize upstream count** in FreeProvider's `FREE_CATALOG` — every additional free provider adds a fallback layer
2. **Per-upstream timeout** (30s default) prevents hanging providers from blocking the chain
3. **Multi-key rotation** (2+ keys per provider) adds another fallback dimension within each upstream
4. **Circuit breaker** skips repeatedly failing upstreams so the chain converges faster
5. **Per-provider defaults** — each upstream uses its own default model (no single-model bottleneck)
