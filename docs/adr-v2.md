# ADRs — Solomon v2

## ADR-00: Architecture reset — Python monolith over Rust/Tauri
Decided: Python monolith (`solomon/` package) over refactoring the 71K-LOC Rust/Tauri RSI orchestrator. Reasoning: user said "simplify," browser-agent domain maps to Python (Playwright ecosystem, LLM SDKs, rich async), monolith is the Simplicity Budget default. The old Rust exe stays as-is for the RSI fleet; v2 is the profit engine. Revisit if user wants them unified.

## ADR-01: Browser layer — PatchRight (Playwright stealth fork)
Decided: PatchRight as primary browser automation layer over undetected-chromedriver and Camoufox. Reasoning: PatchRight is a drop-in replacement for Playwright Python with stealth patches built in (WebDriver detection bypass, navigator.webdriver removal, CDP leak fixes), maintained actively in 2025-2026, uses Chromium (most compatible with target sites), supports persistent profiles. Fallback: Camoufox (anti-detect Firefox) if PatchRight fails on a specific target. Revisit if detection rates increase.

## ADR-02: VLM fallback — MiniCPM-V-2.6 via llama.cpp local
Decided: MiniCPM-V-2.6 served via llama.cpp on localhost as the VLM fallback for text-only main LLMs. Reasoning (from research subagent): co-loads with a 20-30B dense LLM on the 96GB iGPU carve with ~37GB headroom spare — no unload/reload chore (unlike Ornith-35B on Lemonade which OOM-collides with Laguna). OpenAI-compatible /v1/chat/completions with image_url base64 is a ~20-line swap. Fallback: cloud vision API (Gemini Flash via OpenRouter) if local VLM is unavailable. Revisit if a smaller/faster VLM emerges.

## ADR-03: Income channel build order — freelance, content, microtask
Decided: Build freelance gig discovery first, then content publishing, then microtask/survey. Reasoning: freelance has the highest realistic revenue ceiling ($50-500/gig with low anti-bot difficulty for scraping public listings); content has passive potential but slow ramp; microtask is fastest to first dollar but lowest ceiling. All three are zero-cash-outlay (money guard preserved). Revisit based on actual channel performance.

## ADR-04: Operator approval gate before public submissions
Decided: No public submission (gig application, article publish, survey completion) happens without operator review of the draft in `runtime/`. Reasoning: platform ToS risk — freelance/content platforms may ban AI-generated submissions. Operator reviews the draft and flips the config gate to `auto_submit=true` per channel after trust is established. Revisit per channel once the operator is satisfied with draft quality.
