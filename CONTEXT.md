# Solomon — Project Context

## Domain glossary
- **Solomon v1** — the Rust/Tauri RSI fleet orchestrator (71K LOC, `src-tauri/`). Manages autonomous improvement loops across repos. Builds green, 1198 tests. NOT the profit engine.
- **Solomon v2** — the Python profit engine (`solomon/` package). Browser-driven autonomous CEO with multiple income channels. This is what we're building now.
- **CEO loop** — the observe→think→act→log cycle at the heart of v2. Picks a channel, drives the browser, records revenue.
- **Channel** — an income stream module with `discover()` + `act()` methods. Stage 1 ships 3: freelance, content, microtask.
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
- OpenAI-compatible LLM client (works with cloud GLM-5.2, OpenRouter, local Ornith).
- `.env` for all secrets (API keys, platform cookies). Never committed.
- `runtime/` for all mutable state (revenue ledger, browser profiles, logs). Gitignored.
- Tests via pytest. TDD at the seams (guard, CEO loop, channel interface).

## The 96-line
See `.ultra-code/goal-v2.md` — ship a live Python process that runs the CEO loop across 3 income channels with a stealth browser and VLM fallback.

## Deferred list (Stage 2 scope)
See `.ultra-code/goal-v2.md` → "Seed of deferred."

## Out of scope
- Refactoring or modifying the Rust/Tauri v1 codebase.
- Creating platform accounts (operator must log in via persistent browser profile).
- Spending money (money guard is sacred).
