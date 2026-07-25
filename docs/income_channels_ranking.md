# Income Channels for a Solo AI Agent — Ranked by Pay-Within-30-Days Probability

**Constraints honored:** solo AI agent, browser + LLM API key, no credentials yet, no cash outlay, "Solomon only collects" (money-out guard). Today: 2026-07-25.

Primary-source checks (accessed 2026-07-25):
- Prolific: <https://www.prolific.com/pricing> — "Protocol" runs 40+ identity/behavioral checks on participants; platform is for *verified human participants*.
- CloudResearch: <https://www.cloudresearch.com/> — markets "Sentry" with "AI Content Detection: 97.3% confidence · Blocked" and "DigiPrint® Verified". Anti-AI by design.
- Amazon MTurk worker page: <https://www.mturk.com/worker> still up; US worker accounts widely suspended since 2021–22, near-zero new approvals.
- Fiverr ToS fetch: blocked by PerimeterX bot-challenge `PXCR10002539` ("It needs a human touch") — confirms aggressive anti-bot at signup.
- Rev freelancers: <https://www.rev.com/freelancers> — pay $0.40–$2.00+/audio-min; requires application + skills assessment + sample submission; "no minimum commitment".
- Medium Partner Program overview: <https://help.medium.com/hc/en-us/articles/360018970214> (linked from <https://help.medium.com/> sitemap). Medium pays on member read-time; no explicit AI ban, but stories are curator-reviewed for "Boost" (human-evaluated).
- YouTube Partner Program: <https://support.google.com/youtube/answer/72851> — 1,000 subs + 4,000 public watch-hours in 12 months (or 10M Shorts views) → monetization. Hard 30-day blocker.
- KDP: <https://kdp.amazon.com/> — account creation is free; KDP requires identity verification (accepted IDs page exists). Disclose AI on upload; royalties payout EFT 60+ days after month-end.
- Stripe Connect identity verification: <https://docs.stripe.com/connect/identity-verification> — KYC requires real government ID.
- Polar: <https://polar.sh/> — sign-in via Google/Apple/GitHub/email; bank-account payout page lives at `/finance/bank-account` behind login (KYC on payout, not signup).

---

## Ranking (1 = highest probability of paying a solo agent within 30 days)

### 1. AI-Wrapper App Sold via Polar / Stripe Checkout
- **Mechanism:** Agent builds a tiny single-purpose web tool (e.g., remarketing-blurb generator, JPEG-to-recipe-OCR, prompt-template pack). Deploys on Cloudflare Workers/Vercel free tier. Charges $3–$9 one-off or $5/mo via **Polar** (or Stripe Checkout). Polar signup = email/Google/GitHub, no upfront KYC; KYC only on bank-account linkage (payout) — and the "Solomon collects" guardrail is satisfied because money lands in the platform balance, not the agent's wallet.
- **Realistic income / first 30 days:** $0–$300. Expect $0 unless you ship a hookable demo to a niche subreddit/HN. Hits require distribution luck.
- **Setup hours:** 6–15 (build + deploy + pricing page). No cash outlay.
- **Difficulty:** Low-signup (Polar: email); medium-build; high-distribution risk.
- **Verdict:** Highest probability because the gate is code the agent writes, not a human-gated approval queue — the only blocker is product-market fit, not policy.

### 2. Amazon KDP Short-Form Non-Fiction / Low-Content Books
- **Mechanism:** Agent researches a thin-niche keyword ("prompt patterns for X", "checklist for Y"), drafts a 40–80 page book, AI must be **disclosed** on upload (KDP policy), generates a cover, uploads. KDP pays royalties EFT 60+ days after the close of the sales month → first payout likely **outside** the 30-day window, but sales register inside 30 days.
- **Realistic income / first 30 days:** $0–$40 in *registered* royalties (most low-content books sell ~0–3 copies; rare hits $100+).
- **Setup hours:** 3–8 per book. KDP account is free; requires **identity verification** (accepted government IDs page). Account creation is the human-credential bottlenecks.
- **Difficulty:** Medium signup (ID-verification gate is the hard part for an agent — needs an existing human KDP account the user has); low ongoing.
- **Verdict:** Pays eventually, scalable, but the ID-verification + ~60-day payout lag pushes realistic *cash-in-hand* past 30 days. Sales register within window.

### 3. Medium Partner Program (坚持不懈 long-tail posting)
- **Mechanism:** Agent writes 4–8 narrowly-targeted, useful pieces/week ("tiny guides" for a niche: e.g., regex cookbooks, k8s gotchas). Enrolls in MPP (requires Stripe-connected payout, but payout setup happens **after** first reads). Medium's distribution guidelines grade stories for "Boost" via human curators — flagged AI boilerplate won't boost, but **disclosed, useful AI-edited** writing is not banned.
- **Realistic income / first 30 days:** $2–$60. Breakout post $50–$200; most posts $0.10–$2.
- **Setup hours:** 2–4 for signup + first 10 posts.
- **Difficulty:** Medium-high. Cloudflare at signup (we hit a "Just a moment" challenge), email/Social login. Distribution depends on Boost algorithm — pure informational "list" posts often get suppressed.
- **Verdict:** Real rev-share system, no upfront cash, but the agent has to write things **humans bookmark**. Possible; not fast.

### 4. Vocal.media Vocal+ Bonuses
- **Mechanism:** Agent posts long-form stories; Vocal pays per-read thresholds + Challenge bonuses. Vocal+ subscription boosts the per-read rate. A single winning Challenge entry (e.g. "$500 for best story on theme X") can pay Meaningfully.
- **Realistic income / first 30 days:** $0–$50 base; one-off Challenge wins $100–$1,000+ (low hit-rate).
- **Setup hours:** 2–5.
- **Difficulty:** Low signup; depends on winning curated Challenges against human competition.
- **Verdict:** Real money, but Challenge wins are judged — pure-AI submissions face editorial scrutiny. Bonus rate is sustainable only with Vocal+ ($5/mo; **skip that** under no-cash-outlay rule).

### 5. SaaS Affiliate / Referral Farming
- **Mechanism:** Agent signs up for referral programs that pay ** credits or cash** (Notion, Vercel, Hostinger, Fiverr Affiliate, ConvertKit, systeme.io, Gumroad). Writes niche comparison/review content (Medium/Dev.to/Hashnode or a free GitHub-Pages site) with its referral links. Earns on conversions.
- **Realistic income / first 30 days:** $0–$80. Realistic with one good "vs" article ranking for a long-tail keyword; usually $0 without distribution.
- **Setup hours:** 4–10 (signup + 5–10 pieces of content).
- **Difficulty:** Low signup — most SaaS affiliate programs are email-only approval. Income requires traffic.
- **Verdict:** Zero upfront cost, persistent once seeded, but slow and requires a *distribution* channel the agent doesn't yet have. Pairs well with ranking #3.

---

**CUT from top 5 — with reasons:**
- **Freelance gig scraping + auto-bidding (Fiverr/Upwork/Contra):** Fiverr served a PerimeterX bot-challenge at the ToS page; Upwork/Contra require verified identity + skill vetting + manual review on earnings withdrawal. Solo agent without identity docs → very low probability within 30 days.
- **Paid surveys (Prolific / CloudResearch Connect):** Both explicitly detect and block non-human respondents (Prolific "Protocol" 40+ checks; CloudResearch Sentry "AI Content Detection 97.3% · Blocked"). **Non-starter.**
- **Transcription (Rev / GoTranscript):** Rev requires a human-graded transcription sample + skills assessment. Pay-per-audio-min is real but the application gate is human-judged. Will reject AI-flagged samples. **Low probability solo.**
- **AI content on Adobe Stock / stock imagery:** Adobe Stock **does accept** AI-generated content (with disclosure + no real-person likeness / no trademark) — *however*, contributor payout requires ID-verified tax forms; $0.33–$3.06 royalties + a 50% min contribution likelihood of low sales. On-boarding mitigation medium. Could rank ~6 after the above if identity-docs path is unblocked.
- **YouTube:** 1,000 subs + 4,000 watch-hours in 12 months bar (verified from <https://support.google.com/youtube/answer/72851>). **Impossible within 30 days.**

---

## TOP 3 BUILD-ORDER

Wire up **(1) Polar-sold AI-wrapper** first — the only constraint the agent fully controls is its own code, and Polar doesn't ask for KYC at signup (we verified the bank-account page only triggers at payout, which routes through Solomon's "collects-only" guardrail). Build it as a single-tool demo: one input, one output, $5 one-time via Polar checkout, deploy on the Cloudflare Workers free tier. Run it for the 96-ship demo; even $0 of revenue proves the loop end-to-end without any human-approval gate. In parallel, seed **(5) affiliate farming** on top of a free GitHub-Pages mini-blog — costs nothing, gives every subsequent channel a distribution surface, and qualifies us for SaaS referral payouts (the highest-margin passive currency once traffic arrives). Third, queue **(3) Medium Partner Program** for slow-burn content compounding: posts the agent writes this week keep earning for ~12 months and last-hit quantities are uncapped — but skip it for the 96-ship slice specifically because MPP's *first* payout cycle runs at the end of the following month. Skip **Amazon KDP** for the same reason — real but payout lands at 60+ days; good Q2 channel, not a next-week channel. The combined play: ship product (today) → drive content (this week) → route every dollar through Solomon-collects-only Polar/Stripe; defer everything identity-gated until we secure one human-owned account under the user's name.
