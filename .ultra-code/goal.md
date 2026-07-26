# Ultra-Code Goal — Solomon Makes $50 Collected

## Finish-Line (observable)

**Done means:** Solomon's fleet ledger (`fleet_ledger.jsonl`) records ≥ $50.00 USD total revenue
across all projects, every line traceable to a real inbound collection event (money-in only;
money_guard stays sacred — Solomon never moves money OUT, only collects).

Observable gates:
1. `cargo build --release` in `src-tauri/` exits 0 (Solomon.exe rebuilt with any new code).
2. `cargo test` in `src-tauri/` — all tests green, money_guard closure test untouched.
3. LIVE: Solomon reads inbound email (IMAP) for payment receipts / deal confirmations,
   parses the $ amount, appends a `revenue_usd` line to `fleet_ledger.jsonl` per real event,
   and the rolling sum of `revenue_usd` across non-schema rows is ≥ $50.
4. money_guard.rs is byte-identical at the CLOSED-SET line (`MONEY_CAPABLE_KINDS` stays `&[]`);
   no money-OUT action is ever dispatched by Solomon itself.

## 96-line (Stage 1 scope — ship live)
- Solomon SENDS cold-outreach emails via curl-SMTP using operator-supplied `SOLOMON_SMTP_*` creds.
- Solomon READS its inbox via IMAP for replies, payment receipts (Stripe/PayPal/Coinbase/
  NowPayments/Gumroad/etc.), and deal confirmations.
- A new `ceo::revenue` module parses inbound mail, extracts `$` amounts, dedupes by
  message-id, and appends a real `revenue_usd` line to `fleet_ledger.jsonl` per event.
- The dashboard shows the rolling revenue total.
- Outreach targets are operator-supplied only (hard anti-scrape invariant preserved).

## Seed of deferred (4%, Stage 2)
- LLM-grade reply triage (today: deterministic parser; Stage 2: LLM classification of reply intent).
- Multi-account IMAP (today: single account; Stage 2: per-lane inboxes).
- Auto-follow-up cadence on cold replies (today: one-shot send; Stage 2: reply-aware follow-up).
- Revenue attribution per-lane from email headers (today: ledger tags `source`; Stage 2: per-lane).

## Risk Level: HIGH
- Real money boundary — money_guard sacred, must not be weakened.
- Email credentials are operator-supplied secrets I cannot create.
- Outreach recipients must be operator-vetted (anti-scrape invariant; I will not fabricate contacts).
- Inbound mail parsing must be fail-safe: a misparsed receipt must NOT inflate revenue (honest $ only).

## The ONE authority gap (genuine stop condition)
The ultra-code autonomy contract says: never block on the user for engineering decisions, but DO
stop when "credentials you can't create" are required. SMTP/IMAP credentials and outreach target
contacts are operator-supplied data dependencies. I will build ALL the code that consumes them, but
I cannot fabricate: (a) SMTP creds, (b) IMAP creds, (c) outreach target emails, (d) actual paying
customers. The user must supply (a)+(b), and ideally (c). The $50 itself arrives only when real
customers pay — which is the honest definition of "made $50."

## ADR-00: Interpreting "make $50, only collect, full email control"
Decided: Solomon must (1) actually SEND and (2) actually READ email autonomously, and (3) record
only real inbound revenue to the fleet ledger. "Only collect" = money_guard stays sacred (no money-
out capability ever added to `MONEY_CAPABLE_KINDS`). "Full control over email" = Solomon needs both
SMTP (send) AND IMAP (read) — today only SMTP is wired, and it's inert without creds. Adding IMAP
inbound is the missing half. Revisit if the operator wants a different revenue channel (e.g. webhook
from Stripe instead of email parsing).
