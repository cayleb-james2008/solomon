"""Tests for the money guard — the most critical safety module."""
import pytest
from solomon.guard import check_action, guard, MoneyGuardError, MONEY_OUT_KINDS, MONEY_IN_KINDS


class TestMoneyGuard:
    """The money guard is SACRED. Solomon never moves money OUT."""

    def test_all_money_out_kinds_denied(self):
        """Every money-OUT action kind must be DENIED."""
        for kind in MONEY_OUT_KINDS:
            assert check_action(kind) is False, f"{kind} should be DENIED"

    def test_all_money_in_kinds_permitted(self):
        """Every money-IN action kind must be PERMITTED."""
        for kind in MONEY_IN_KINDS:
            assert check_action(kind) is True, f"{kind} should be PERMITTED"

    def test_unknown_action_denied_fail_closed(self):
        """Unknown/ambiguous actions must be DENIED (fail-closed)."""
        assert check_action("some_unknown") is False
        assert check_action("") is False
        assert check_action("buy_crypto") is False
        assert check_action("wire_transfer") is False

    def test_guard_raises_on_money_out(self):
        """The guard() function must raise MoneyGuardError on money-OUT."""
        with pytest.raises(MoneyGuardError):
            guard("spend")
        with pytest.raises(MoneyGuardError):
            guard("withdraw")
        with pytest.raises(MoneyGuardError):
            guard("purchase")

    def test_guard_passes_on_money_in(self):
        """The guard() function must not raise on money-IN."""
        guard("collect")  # should not raise
        guard("log_revenue")  # should not raise

    def test_guard_raises_on_unknown(self):
        """The guard() function must raise on unknown actions (fail-closed)."""
        with pytest.raises(MoneyGuardError):
            guard("some_unknown_action")

    def test_money_out_kinds_is_frozen(self):
        """The MONEY_OUT_KINDS set must be frozen (immutable)."""
        with pytest.raises(AttributeError):
            MONEY_OUT_KINDS.add("new_kind")

    def test_no_overlap(self):
        """No action kind should be in both MONEY_OUT and MONEY_IN sets."""
        assert MONEY_OUT_KINDS.isdisjoint(MONEY_IN_KINDS)
