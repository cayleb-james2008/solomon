# Solomon v2 — Runbook

## Quickstart

```sh
# 1. Install dependencies (first time only)
uv pip install -e ".[dev]"
uv pip install patchright
uv run patchright install chromium

# 2. Configure — copy .env.example.v2 to .env and edit API keys
cp .env.example.v2 .env
# Edit .env: set SOLOMON_LLM_API_KEY (required for live mode)

# 3. Run the money guard check (verify safety)
uv run python -m solomon guard-check

# 4. Test the stealth browser
uv run python -m solomon test-browser

# 5. Dry run (no browser, no network — verifies config)
uv run python -m solomon run --dry --once

# 6. Live run (one CEO cycle — launches browser, scans channels)
uv run python -m solomon run --once

# 7. Revenue dashboard
uv run python -m solomon dashboard
```

## Architecture (simplified)

```
solomon/
├── __init__.py        — package marker
├── __main__.py        — CLI entrypoint (python -m solomon)
├── cli.py             — CLI dispatcher (run, dashboard, test-*)
├── config.py          — .env → typed Config dataclass
├── llm.py             — OpenAI-compatible LLM client (cloud GLM / OpenRouter / local Ornith)
├── vlm.py             — VLM fallback: screenshot → text description (MiniCPM-V-2.6 / cloud vision)
├── browser.py         — Stealth browser pool (PatchRight, persistent profiles, anti-bot)
├── guard.py           — Money guard (fail-closed: no money-OUT, only collect)
├── ceo.py             — CEO loop: observe → think → act → log
├── ledger.py          — Revenue ledger (append-only JSONL)
└── channels/
    ├── __init__.py    — Channel abstract base class
    ├── freelance.py   — Fiverr/Contra gig discovery + proposal drafting
    ├── content.py      — Hacker News trending → article drafting
    └── microtask.py    — Prolific/CloudResearch task discovery
```

## How it works

1. **Config** loads `.env` with all settings (API keys, browser profile, channel toggles).
2. **CEO loop** runs each cycle: observes all active channels, asks the LLM which to prioritize, executes the chosen action, and logs any revenue.
3. **Stealth browser** (PatchRight) drives real browser sessions with anti-bot stealth (navigator.webdriver=False, persistent profiles).
4. **VLM fallback** converts screenshots to text descriptions when the main LLM is text-only (e.g., glm-4-flash). If the main LLM is vision-capable (e.g., gpt-4o), it handles images directly.
5. **Money guard** is sacred — Solomon never moves money out. All spending/transfer/withdraw actions are DENIED by default (fail-closed).
6. **Revenue ledger** logs every inbound payment event to `runtime/revenue.jsonl`.

## Safety

- **Money guard**: `solomon/guard.py` — 19 money-OUT action kinds are DENIED, 6 money-IN kinds are PERMITTED, and all unknown actions are DENIED (fail-closed).
- **Operator approval gate**: `SOLOMON_AUTO_SUBMIT=false` (default) means all proposals, articles, and task claims are saved as drafts in `runtime/` for operator review before any public submission.
- **No background processes**: Solomon runs as a foreground process. `--once` runs a single cycle and exits.

## VLM setup (optional — for text-only main LLMs)

If your main LLM is text-only (e.g., glm-4-flash, llama-3.1), set up a VLM for screenshot descriptions:

### Option A: MiniCPM-V-2.6 via llama.cpp (local, recommended)
```sh
# Start llama.cpp server with MiniCPM-V-2.6 on port 8012
# (download GGUF from HuggingFace, run llama-server with --port 8012)
# Then in .env:
SOLOMON_VLM_BASE_URL=http://localhost:8012/v1
SOLOMON_VLM_MODEL=MiniCPM-V-2.6
```

### Option B: Cloud vision via OpenRouter (fallback)
```sh
SOLOMON_VLM_BASE_URL=https://openrouter.ai/api/v1
SOLOMON_VLM_API_KEY=your-openrouter-key
SOLOMON_VLM_MODEL=google/gemini-flash-1.5
```

## Income channels

| Channel    | What it does                          | Revenue potential        | Status       |
|------------|---------------------------------------|--------------------------|--------------|
| Freelance  | Scrape Fiverr/Contra, draft proposals | $15-500/gig              | Draft mode   |
| Content    | Find trending topics, draft articles  | $0-50/article (passive)  | Draft mode   |
| Microtask  | Check Prolific/CloudResearch tasks     | $2-20/task               | Discovery    |

All channels save drafts to `runtime/` for operator review. Flip `SOLOMON_AUTO_SUBMIT=true` per-channel after trust is established.

## Tests

```sh
uv run python -m pytest tests/ -v
# 21 tests — guard, config, ledger, VLM, channels
```

## Commands

| Command                    | Description                              |
|----------------------------|------------------------------------------|
| `solomon run --once`       | Run one CEO cycle (live browser)         |
| `solomon run --dry --once` | Dry run (no browser, config check)       |
| `solomon run`              | Continuous loop (SOLOMON_MAX_CYCLES=0)  |
| `solomon dashboard`        | Show revenue dashboard                   |
| `solomon guard-check`      | Verify money guard is intact             |
| `solomon test-browser`     | Test stealth browser launch              |
| `solomon test-vlm`         | Test VLM fallback                        |
| `solomon test-llm`         | Test LLM connection                      |
