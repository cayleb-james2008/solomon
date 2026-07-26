# ADRs — Solomon v2

## ADR-00: Architecture reset — Python monolith over Rust/Tauri
Decided: Python monolith (`solomon/` package) over refactoring the 71K-LOC Rust/Tauri RSI orchestrator. Reasoning: user said "simplify," browser-agent domain maps to Python (Playwright ecosystem, LLM SDKs, rich async), monolith is the Simplicity Budget default. The old Rust exe stays as-is for the RSI fleet; v2 is the profit engine. Revisit if user wants them unified.

## ADR-01: Browser layer — PatchRight (stealth verified live)
Decided: PatchRight as primary browser layer. Confirmed by research subagent (2026-07-25 primary sources): Apache-2.0, actively maintained (last commit last week), explicitly passes Cloudflare/Akamai/Datadome/Kasada/Shape/F5, drop-in Playwright API, persistent profiles, correct CDP patches (Runtime.enable, Console.enable, command-flag leaks) shipped for you. Live-verified: `navigator.webdriver=False` against google.com. Pair with BrowserForge for fingerprint rotation when needed. Fallback: Camoufox (Firefox-based, C++-level fingerprint spoofing, 312 real-world fingerprint presets) if Chromium fingerprints get detected on a specific target — accept GPL-3.0 + "under development" risk. Revisit if detection rates increase.

## ADR-02: VLM fallback — MiniCPM-V-2.6 via llama.cpp local
Decided: MiniCPM-V-2.6 served via llama.cpp on localhost as the VLM fallback for text-only main LLMs. Reasoning (from research subagent): co-loads with a 20-30B dense LLM on the 96GB iGPU carve with ~37GB headroom spare — no unload/reload chore (unlike Ornith-35B on Lemonade which OOM-collides with Laguna). OpenAI-compatible /v1/chat/completions with image_url base64 is a ~20-line swap. Fallback: cloud vision API (Gemini Flash via OpenRouter) if local VLM is unavailable. Revisit if a smaller/faster VLM emerges.

## ADR-03: Income channel build order — content, AI-wrapper, freelance, microtask
Decided: Build content publishing first, then a Polar-sold AI-wrapper demo, then freelance gig discovery, then microtask/survey discovery. Reasoning (from research subagent): (1) Content via Medium/Dev.to has zero signup fee, no identity verification, and passive compounding — articles earn for 12 months. (2) A Polar-sold AI-wrapper ($5 one-time, Cloudflare Workers free tier) is the fastest end-to-end revenue loop because the agent controls the code and Polar doesn't require KYC at signup (payout triggers the bank-account page, which routes through Solomon's collect-only guardrail). (3) Freelance (Fiverr/Contra) is valuable but Fiverr served a bot-challenge during research and Contra requires identity verification — keep the discovery channel but don't expect first-dollar here. (4) Microtask/survey: Prolific explicitly detects and blocks non-human respondents (40+ Protocol checks) — keep the discovery/alert channel but it's not a reliable income source. All channels are zero-cash-outlay (money guard preserved). Revisit based on actual channel performance.

## ADR-04: Operator approval gate before public submissions
Decided: No public submission (gig application, article publish, survey completion) happens without operator review of the draft in `runtime/`. Reasoning: platform ToS risk — freelance/content platforms may ban AI-generated submissions. Operator reviews the draft and flips the config gate to `auto_submit=true` per channel after trust is established. Revisit per channel once the operator is satisfied with draft quality.

## ADR-05: GitHub Pages as the zero-credential distribution surface
Decided: GitHub Pages is the primary publishing channel because it requires zero new credentials (uses existing gh CLI auth), deploys instantly via git push, and is free. Reasoning: research subagent identified "a distribution surface the agent doesn't yet own" as the key missing piece for affiliate farming + content revenue. GitHub Pages solves this with zero friction. The blog repo (cayleb-james2008/solomon-blog) is auto-created on first run and Solomon pushes Jekyll-formatted articles to main, which Pages auto-builds. Revisit if GitHub changes Pages policy or if we need a custom domain.

## ADR-06: Local Ornith as the production brain with a local publish ledger
Decided: Run Solomon's CEO decisions and content generation on local Ornith via Lemonade, and persist normalized successful Dev.to titles in `runtime/content_published_titles.json` alongside the remote Dev.to check. Reasoning: this removes recurring LLM cost and keeps the profit loop standalone, while the tiny local ledger closes the API-intermittency duplicate-publish failure without adding a service or database. Revisit if local inference misses a full cycle or measured cloud quality materially improves conversion.

## ADR-07: Bottom-funnel tool spotlights before generic SEO topics
Decided: When a live Solomon tool lacks an “I built this” article, publish that bottom-funnel spotlight before generic trend content. Reasoning: it links directly to a working product and one-time checkout, so the same article has a shorter path to revenue than an untargeted SEO post. Revisit if measured spotlight conversion trails generic content after 10 published posts.

## ADR-08: Disable Stripe Managed Payments until tax classification is deliberate
Decided: Create Solomon's one-time payment links with `managed_payments[enabled]=false` until the operator selects and configures the correct Stripe tax code for these digital tools. Reasoning: the live account rejects links without an eligible tax code when Managed Payments is enabled; disabling the optional rail keeps checkout operational without guessing at a legal/tax classification. Revisit before scaling sales or entering jurisdictions where automated tax collection is required.

## ADR-09: Honest support CTA until paid entitlement exists
Decided: Describe the public $5 action as optional support instead of paid access because the current Cloudflare tools are publicly usable and Stripe polling records revenue but does not issue an entitlement. Reasoning: truthful copy is safer than claiming a license or gated feature that does not exist. Revisit when a minimal Stripe-session verification and delivery/access token path is implemented.

## ADR-10: Fail closed for Cloudflare worker inference
Decided: The AI-wrapper lane requires explicit `SOLOMON_WORKER_LLM_*` remote-provider settings and rejects localhost; it never reuses Solomon's local Ornith settings or silently falls back to OpenRouter. Reasoning: Cloudflare cannot reach the local model, and silent cloud use would violate standalone operation and cost control. Revisit only with an explicitly funded remote worker backend.
