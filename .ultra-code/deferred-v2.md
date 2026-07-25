# Deferred — Solomon v2 Stage 2 Scope

Each item has a revisit trigger. Stage 2 works this list in user-value order.

## Auto-follow-up cadence on freelance proposals
- **Trigger**: First proposal gets a reply but no conversion after 7 days.
- **Plan**: Reply-aware follow-up — LLM reads the reply, drafts a follow-up, operator approves.

## Multi-account browser profiles (per-channel fingerprints)
- **Trigger**: Channel gets banned or rate-limited due to shared fingerprint.
- **Plan**: Per-channel persistent profiles with distinct fingerprints.

## Self-scaling: CEO discovers and onboards new channels autonomously
- **Trigger**: Current 3 channels are stable but revenue plateaus.
- **Plan**: CEO module that researches new income channels, evaluates them, and onboards the viable ones.

## Revenue attribution per-channel from email receipts (IMAP auto-parse)
- **Trigger**: Revenue events arrive but manual attribution is error-prone.
- **Plan**: IMAP inbox monitoring + receipt parsing (Stripe/PayPal/Coinbase/Gumroad) → auto-append to ledger with channel attribution.

## CAPTCHA solving (2captcha/anti-captcha integration)
- **Trigger**: Stealth browser hits a CAPTCHA on a target site.
- **Plan**: Integrate 2captcha or anti-captcha API. Operator-funded (money-OUT for the CAPTCHA service — requires operator approval per use).

## Full Medium/Dev.to API publishing pipeline
- **Trigger**: Operator trusts article draft quality after reviewing 5+ drafts.
- **Plan**: Medium/Dev.to API integration with auto-publish (per-channel `auto_submit` gate flip).

## KDP short book publishing
- **Trigger**: Content channel articles are performing well and could be compiled into short books.
- **Plan**: Amazon KDP integration — compile articles into short books, format, publish.

## Affiliate/referral farming
- **Trigger**: Content channel has traffic but no monetization beyond ad share.
- **Plan**: Sign up for SaaS referral programs (Notion, Vercel, etc.), embed affiliate links in content.

## AI-wrapper app + Stripe/Polar checkout
- **Trigger**: Solomon identifies a niche SaaS opportunity through content channel research.
- **Plan**: Build a simple AI-wrapper app, deploy, charge via Stripe/Polar. (Note: this involves money-OUT for hosting — operator approval required.)
