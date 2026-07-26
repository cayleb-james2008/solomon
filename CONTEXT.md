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
- ADR-01: PatchRight primary browser, Camoufox fallback
- ADR-02: MiniCPM-V local VLM fallback with cloud fallback
- ADR-03: Content → AI-wrapper → freelance → microtask channel order
- ADR-04: Operator approval gate before public submissions
- ADR-05: GitHub Pages zero-credential distribution surface
- ADR-06: Local Ornith plus local publish ledger
- ADR-07: Bottom-funnel tool spotlights before generic SEO
- ADR-08: Managed Payments disabled until tax classification is deliberate
- ADR-09: Optional support CTA until paid entitlement exists
- ADR-10: Cloudflare worker inference fails closed without explicit remote config

## Decided conventions
- Python 3.11+ (host has python=3.11.15). Use `uv run` for execution (venv pip is missing).
- Async-first (asyncio) — browser automation is inherently async.
- OpenAI-compatible LLM client; production `.env` uses local Ornith on Lemonade `:13305/api/v1` for zero recurring inference cost, with cloud settings retained as an operator fallback.
- `.env` for all secrets (API keys, platform cookies). Never committed.
- `runtime/` for all mutable state (revenue ledger, browser profiles, logs). Gitignored.
- Tests via pytest. TDD at the seams (guard, CEO loop, channel interface, content dedup). Successful Dev.to titles are also recorded locally because the API can intermittently reject/omit authenticated reads.
- Public tools are free to try; the live $5 links are optional support until a Stripe-session entitlement path exists.
- Cloudflare-generated wrapper workers require separate explicit remote provider settings; local Ornith is never sent to a worker and OpenRouter is never silently selected.

## The 96-line
- See `.ultra-code/goal-v3.md` — keep the live Python profit loop simple, local-first, and biased toward bottom-funnel product traffic.

## Deferred list (Stage 2 scope)
See `.ultra-code/deferred-v3.md` for paid entitlement, funnel measurement, distribution, and failover triggers.

## Out of scope
- Refactoring or modifying the Rust/Tauri v1 codebase.
- Creating platform accounts (operator must log in via persistent browser profile).
- Spending money (money guard is sacred).
