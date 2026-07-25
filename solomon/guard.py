"""Money guard — fail-closed predicate. Solomon NEVER moves money OUT.

This is the sacred chokepoint. Any action that could move money out (spending,
transferring, purchasing, paying, funding, etc.) is DENIED by default. The only
permitted money actions are INBOUND (collecting revenue, receiving payments).

This module is tested exhaustively. Do not weaken it.
"""

# Closed set of money-OUT action kinds. Any action whose kind matches one of these
# is DENIED. This set can only SHRINK, never grow — adding a kind here means
# "Solomon will refuse to do this," which is the correct default.
MONEY_OUT_KINDS = frozenset({
    "spend",
    "transfer",
    "withdraw",
    "purchase",
    "payment",
    "fund",
    "deposit_out",
    "ads_spend",
    "subscribe_paid",
    "upgrade_paid",
    "buy",
    "checkout",
    "pay_bill",
    "send_money",
    "tip",
    "donate",
    "invest_out",
    "place_order",
    "renew_paid",
})

# Permitted money-IN kinds (Solomon may collect, never disburse)
MONEY_IN_KINDS = frozenset({
    "collect",
    "receive",
    "log_revenue",
    "claim_payment",
    "invoice",
    "withdraw_to_self",  # collecting earned revenue to the operator's own account
})


class MoneyGuardError(PermissionError):
    """Raised when a money-OUT action is attempted."""


def check_action(action_kind: str, context: dict | None = None) -> bool:
    """Fail-closed predicate: returns True if the action is PERMITTED, False if DENIED.

    Rules:
    1. If action_kind is in MONEY_OUT_KINDS → DENY (return False).
    2. If action_kind is in MONEY_IN_KINDS → PERMIT (return True).
    3. If action_kind is unknown/ambiguous → DENY (return False) — fail-closed.
    """
    if action_kind in MONEY_OUT_KINDS:
        return False
    if action_kind in MONEY_IN_KINDS:
        return True
    # Unknown action — fail-closed
    return False


def guard(action_kind: str, context: dict | None = None) -> None:
    """Guard an action. Raises MoneyGuardError if the action is DENIED."""
    if not check_action(action_kind, context):
        raise MoneyGuardError(
            f"DENIED: action_kind='{action_kind}' is a money-OUT action or unknown. "
            f"Solomon never moves money out. (context: {context or {}})"
        )
