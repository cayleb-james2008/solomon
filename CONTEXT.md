# Solomon — Project Context

## Domain glossary
- **Solomon v1** — the Rust/Tauri RSI fleet orchestrator (71K LOC, `src-tauri/`). Manages autonomous improvement loops across repos. Builds green, 1198 tests. NOT the profit engine.
- **Solomon v2** — the Python profit engine (`solomon/` package). Browser-driven autonomous CEO with multiple income channels. This is what we're building now.
- CEO loop — the observe→think→act→log cycle; a ready bottom-funnel spotlight deterministically outranks speculative channel expansion.
- Channel — an income stream module with `discover()` + `act()` methods.
- **VLM fallback** — when the main LLM is text-only, a small vision model converts screenshots to text descriptions so the main model can "see."
- **Money guard** — fail-closed predicate (`solomon/guard.py`): no money-out action is ever dispatched. Solomon only collects.
- **Stealth browser** — PatchRight or Camoufox with persistent profiles + human-like fingerprint. Anti-bot-first.

## ADR index
- ADR-00: Architecture reset — Python monolith over Rust/Tauri (see goal-v2.md)
- ADR-01: (pending — browser layer choice, post-research)
- ADR-02: (pending — VLM fallback choice, post-research)
- ADR-03: (pending — income channel build order, post-research)

## Decided conventions
- Python 3.11+ (host has python=3.11.15). Use `uv run` for execution (venv pip is missing).
- Async-first (asyncio) — browser automation is inherently async.
- OpenAI-compatible LLM client; production `.env` uses local Ornith on Lemonade `:13305/api/v1` for zero recurring inference cost, with cloud settings retained as an operator fallback.
- `.env` for all secrets (API keys, platform cookies). Never committed.
- `runtime/` for all mutable state (revenue ledger, browser profiles, logs). Gitignored.
- Tests via pytest. TDD at the seams (guard, CEO loop, channel interface, content dedup). Successful Dev.to titles are also recorded locally because the API can intermittently reject/omit authenticated reads.

## The 96-line
- See `.ultra-code/goal-v3.md` — keep the live Python profit loop simple, local-first, and biased toward bottom-funnel product traffic.

## Deferred list (Stage 2 scope)
See `.ultra-code/goal-v2.md` → "Seed of deferred."

## Out of scope
- Refactoring or modifying the Rust/Tauri v1 codebase.
- Creating platform accounts (operator must log in via persistent browser profile).
- Spending money (money guard is sacred).
