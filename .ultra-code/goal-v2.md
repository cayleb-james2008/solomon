# Ultra-Code Goal — Solomon v2: Autonomous Profit CEO

## Finish-Line (observable)

**Done means:** Solomon v2 is a single Python process that, when given an LLM API key in `.env`, launches a stealth browser, picks an income channel, and autonomously executes revenue-generating actions across ≥3 channels (freelance bidding, content publishing, microtasks/surveys). A small VLM fallback converts screenshots to text descriptions for the text-only main LLM when visual context is needed. Revenue events are logged to `runtime/revenue.jsonl`. The money-out guard is preserved: Solomon never spends, only collects.

Observable gates:
1. `uv run python -m solomon --once` exits 0 (or runs one full loop cycle cleanly).
2. `uv run pytest` — all tests green.
3. LIVE: Solomon launches a stealth browser session, navigates to a target site, takes a screenshot, gets a VLM description (or main-model read if main is vision-capable), decides an action, and executes it — all observable in the console log.
4. The money guard (`solomon/guard.py`) refuses any action whose kind is in `MONEY_OUT_KINDS` — tested and green.
5. ≥3 income channel modules exist and each has a working `discover()` + `act()` skeleton that the CEO can route to.

## 96-line (Stage 1 scope — ship live)
- **Config layer** (`.env` → dataclass): API keys, browser stealth profile, channel toggles.
- **LLM client**: OpenAI-compatible client (works with cloud GLM-5.2, OpenRouter, or local Ornith via Lemonade :13305/api/v1). Auto-detects vision capability.
- **VLM fallback**: if main LLM is text-only, screenshots are sent to a lightweight VLM (Ornith :13305 if available, cloud vision API fallback) for description, fed back to main LLM as text.
- **Stealth browser pool**: PatchRight (Playwright stealth fork) or Camoufox — persistent profiles, human-like fingerprint, CDP-based.
- **CEO loop**: `observe → think → act → log` cycle. The CEO module selects which channel to run, dispatches the browser action, and records outcomes.
- **3 income channels** (skeleton + first one fully working):
  1. Freelance gig discovery (scrape Fiverr/Contra public listings, draft proposals via LLM, queue for operator approval before submit).
  2. Content publishing (Medium/Dev.to article draft + publish via API).
  3. Microtask/survey discovery (Prolific/connect availability check + auto-claim).
- **Revenue ledger**: `runtime/revenue.jsonl` — every inbound payment event logged.
- **Money guard**: fail-closed predicate, no money-out action ever dispatched.

## Seed of deferred (4%, Stage 2)
- Auto-follow-up cadence on freelance proposals (today: one-shot; Stage 2: reply-aware).
- Multi-account browser profiles (today: one persistent profile; Stage 2: per-channel fingerprints).
- Self-scaling: today 3 channels; Stage 2: CEO discovers and onboards new channels autonomously.
- Revenue attribution per-channel from email receipts (today: manual/source-tagged; Stage 2: IMAP auto-parse).
- Full anti-bot CAPTCHA solving (today: operator-alerted; Stage 2: 2captcha/anti-captcha integration).

## Risk Level: HIGH
- Anti-bot detection is an arms race — the browser layer must be stealth-first.
- Real money boundary — money_guard sacred, must not be weakened.
- Platform ToS — freelance/content platforms may ban AI-generated submissions. Mitigation: operator approval gate before any public submission.
- Credentials are operator-supplied secrets (API keys, platform logins via browser cookies/profile).

## Authority gaps (genuine stop conditions)
- Platform credentials (Fiverr/Medium/Prolific login) must be supplied by operator via persistent browser profile login — I cannot create accounts.
- The operator must approve the first public submission on each channel (ToS safety).

## ADR-00: Architecture reset — Python monolith over Rust/Tauri
Decided: Solomon v2 is a fresh Python package (`solomon/`), NOT a refactor of the 71K-LOC Rust/Tauri RSI orchestrator. Reasoning: the user said "simplify the architecture," the browser-agent domain maps cleanly to Python (Playwright/PatchRight ecosystem, LLM SDKs, rich async), and a monolith is the Simplicity Budget default. The old Rust exe stays as-is for the RSI fleet; v2 is the profit engine. Revisit if the user wants them unified.
