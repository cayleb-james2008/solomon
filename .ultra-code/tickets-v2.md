# Solomon v2 — Tracer-Bullet Tickets

## Ticket #0 — Walking Skeleton (96)
**Blocked by:** nothing
**Delivers:** A single `uv run python -m solomon --once` that loads config, initializes the LLM client, detects vision capability, launches a stealth browser, navigates to a test page, takes a screenshot, gets a VLM description (or main-model read if vision-capable), and logs the observed state. Money guard module exists and is tested. Revenue ledger file created.
**96-or-4:** 96

## Ticket #1 — Money Guard + CEO Loop (96)
**Blocked by:** #0
**Delivers:** `solomon/guard.py` with fail-closed predicate (MONEY_OUT_KINDS = closed set, deny by default). `solomon/ceo.py` with the observe→think→act→log cycle. CEO selects a channel, dispatches an action, checks the guard, and records the outcome. Tested: guard denies all money-out actions, CEO loop runs one cycle in dry mode.
**96-or-4:** 96

## Ticket #2 — Channel: Freelance Gig Discovery (96)
**Blocked by:** #1
**Delivers:** `solomon/channels/freelance.py` with `discover()` (scrape public Fiverr/Contra gig listings via stealth browser) and `act()` (LLM-drafts a proposal, saves to `runtime/proposals/` for operator approval before submit). Tests: discover returns gigs, act drafts a proposal, nothing is auto-submitted.
**96-or-4:** 96

## Ticket #3 — Channel: Content Publishing (96)
**Blocked by:** #1
**Delivers:** `solomon/channels/content.py` with `discover()` (find trending topics via Hacker News / Google Trends RSS) and `act()` (LLM drafts an article, saves to `runtime/articles/` as markdown for review). Medium/Dev.to publishing via their API is a config-gated submit (off by default). Tests: discover returns topics, act produces a draft article file.
**96-or-4:** 96

## Ticket #4 — Channel: Microtask/Survey Discovery (96)
**Blocked by:** #1
**Delivers:** `solomon/channels/microtask.py` with `discover()` (check Prolific/Connect availability via browser) and `act()` (alert operator to available tasks, auto-claim if logged in). Tests: discover returns task availability status, act logs the attempt.
**96-or-4:** 96

## Ticket #5 — Revenue Ledger + Dashboard (96)
**Blocked by:** #1
**Delivers:** `runtime/revenue.jsonl` schema + append logic in `solomon/ledger.py`. Simple console dashboard showing channel status, rolling revenue, last action. `solomon dashboard` command.
**96-or-4:** 96

## Ticket #6 — Runbook + Ship (96)
**Blocked by:** #2, #3, #4, #5
**Delivers:** `docs/runbook-v2.md` with setup instructions, `.env.example` updated for v2, ADR list finalized, deferred list written. Three-gate verify (uv run, pytest, live browser session).
**96-or-4:** 96

## Deferred (Stage 2)
- Auto-follow-up cadence on freelance proposals
- Multi-account browser profiles (per-channel fingerprints)
- Self-scaling: CEO discovers and onboards new channels autonomously
- Revenue attribution per-channel from email receipts (IMAP auto-parse)
- CAPTCHA solving (2captcha/anti-captcha integration)
- Full Medium/Dev.to API publishing pipeline
- KDP short book publishing
- Affiliate/referral farming
- AI-wrapper app + Stripe/Polar checkout
