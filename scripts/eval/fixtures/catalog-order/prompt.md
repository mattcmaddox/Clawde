Analyze the FreeProvider fallback chain in this codebase. Read crates/api/src/providers/free/catalog.rs, then answer concisely:

1. List EVERY upstream in the FREE_CATALOG in fallback priority order (the full ordered list of upstream ids, starting with the first tier).
2. For each of the first three upstreams, state its default model.
3. Which upstream is the last resort (Tier 4), and what does its note say about credits?
4. What do the `fallback_models` fields do, and which two catalog entries use them?

Be specific: enumerate the ids explicitly. Use the exact upstream ids (e.g. huggingface, nvidia) rather than display names.