# Verify — live Stripe revenue rail

## Credential and account gate
- `.env` contains a live-format Stripe key; the value was never printed.
- `.env` is Git-ignored.
- Read-only Stripe `/v1/account` authentication succeeded.
- `charges_enabled=True`; `payouts_enabled=True`.

## Live link gate
- Existing test link metadata was backed up locally and cleared from the runtime registry.
- Eight orphaned live products from failed link attempts were archived; no money movement occurred.
- `uv run python -m solomon.payments sync-links` → `links synced (4 created)`.
- Stripe API verification: all four links reported `livemode=True` and `active=True`.

## Landing gate
- Landing redeploy: `https://solomon-tools.solomontools.workers.dev`.
- Public response: 4,510 bytes, four `buy.stripe.com/` links, zero `/test_` links.

## Regression gate
- `uv run python -m pytest tests/ -q` → `51 passed in 0.66s`.

## Operational note
Managed Payments is disabled until the correct tax classification is selected. This is an intentional configuration boundary, not a claim that tax obligations are absent.