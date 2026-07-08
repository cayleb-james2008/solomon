# kairos novel-signal hypothesis ledger

Durable state for the Solomon RSI novel-signal search lane (kairos = target #1).
Lifecycle per hypothesis: **proposed -> paper-screened -> pre-registered-gate -> arm/reject
-> settled-verdict**. Pre-registration and verdict live in the SAME row so the pass/fail
criteria cannot be edited after the outcome is seen (anti-gaming).

BLAST-RADIUS: every row starts as a ZERO-capital paper lane. Promotion to live requires the
untouched triple gate (--live-*-class flag + `.state/<LANE>_LIVE` + the in-file `_LIVE_DEFAULTS`
hard leash) AND promote.py's evidence ladder. This lane's search tool can propose, never arm.

FORBIDDEN: never propose a class marked DEAD / REAL-but-UNUSABLE / REFUTED in
`docs/STRATEGY_SCOREBOARD.md`. The search tool hard-rejects those before they reach this ledger.

| id | hypothesis (class) | proposed | pre-registered pass/fail gate | stage | verdict | mechanism | date |
|----|--------------------|----------|-------------------------------|-------|---------|-----------|------|
| H-52381add | order-book queue-imbalance microstructure (non-price-model) | The last genuinely-untested class per the scoreboard: pure order-flow / queue-imbalance dynamics, NOT the price-prediction model. Low probability (same 2c execution wall) but the only quick-untested crypto class left. | PAPER SCREEN (0 capital): capture >=200 settled paper observations of the signal; PASS iff sizing_source-truthful paper edge, AFTER the D1 adverse-selection haircut, is > 0 with a bootstrap CI low > 0 over market-window blocks AND the edge sign replicates out-of-sample (group k-fold by market). Any of: CI straddles 0, sign flips OOS, or < 200 obs => REJECT. No live capital is armed on a PASS without a separate operator-ratified pre-registered LIVE gate + the triple gate. | proposed->paper-screen | (pending) | a capture lane recording full order-book deltas over time (not just our own quotes), then a test of whether flow imbalance predicts short-horizon moves beyond the 2c spread. // fitness: promote_signal=False (truthful_pnl=-119.19, haircut_applied=-325.27, poison_excluded=176.56) | 2026-07-08T01:23:16Z |
