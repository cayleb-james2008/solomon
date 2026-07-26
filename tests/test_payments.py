"""Tests for the Stripe payment rail (solomon/payments.py).

Hermetic: Stripe HTTP is monkeypatched; the ledger is the real Ledger class
on a tmp_path. The money guard is exercised for real — poll_revenue must pass
through guard("collect") for every settled session.
"""
import json

import pytest

from solomon import payments
from solomon.ledger import Ledger


@pytest.fixture(autouse=True)
def _stripe_key(monkeypatch):
    monkeypatch.setenv("STRIPE_SECRET_KEY", "sk_test_fake")


@pytest.fixture
def ledger(tmp_path):
    return Ledger(tmp_path / "revenue.jsonl")


# --- form-encoding flattener -------------------------------------------------

def test_flatten_scalars():
    flat = dict(payments._flatten({"name": "X", "unit_amount": 500}))
    assert flat == {"name": "X", "unit_amount": "500"}


def test_flatten_nested_dict_and_list():
    flat = dict(payments._flatten({
        "metadata": {"solomon_tool": "summarizy"},
        "line_items": [{"price": "price_1", "quantity": 1}],
    }))
    assert flat["metadata[solomon_tool]"] == "summarizy"
    assert flat["line_items[0][price]"] == "price_1"
    assert flat["line_items[0][quantity]"] == "1"


def test_flatten_skips_none():
    flat = dict(payments._flatten({"a": None, "b": 1}))
    assert "a" not in flat and flat["b"] == "1"


# --- credential handling -----------------------------------------------------

def test_missing_key_fails_closed(monkeypatch):
    monkeypatch.delenv("STRIPE_SECRET_KEY", raising=False)
    with pytest.raises(payments.StripeError):
        payments._key()


# --- payment link creation ---------------------------------------------------

def _fake_stripe(calls):
    def fake(method, path, data=None):
        calls.append((method, path, data))
        if path == "/products":
            return {"id": "prod_1"}
        if path == "/prices":
            return {"id": "price_1"}
        if path == "/payment_links":
            return {"id": "plink_1", "url": "https://buy.stripe.com/test_abc"}
        raise AssertionError(f"unexpected path {path}")
    return fake


def test_ensure_payment_link_creates_chain(monkeypatch):
    calls = []
    monkeypatch.setattr(payments, "_req", _fake_stripe(calls))
    tool = {"name": "mood-analyzer", "description": "mood AI"}
    url = payments.ensure_payment_link(tool)
    assert url == "https://buy.stripe.com/test_abc"
    assert [p for _, p, _ in calls] == ["/products", "/prices", "/payment_links"]
    assert tool["stripe_link"] == url
    assert tool["stripe_link_id"] == "plink_1"
    assert tool["stripe_product_id"] == "prod_1"
    # price payload carries the $5 one-time amount
    _, _, price_data = calls[1]
    assert price_data["unit_amount"] == 500 and price_data["currency"] == "usd"
    _, _, link_data = calls[2]
    assert link_data["managed_payments"]["enabled"] is False


def test_ensure_payment_link_idempotent(monkeypatch):
    calls = []
    monkeypatch.setattr(payments, "_req", _fake_stripe(calls))
    tool = {"name": "t", "stripe_link": "https://buy.stripe.com/existing"}
    assert payments.ensure_payment_link(tool) == "https://buy.stripe.com/existing"
    assert calls == []  # no Stripe calls when a link already exists


def test_ensure_payment_link_failure_returns_none(monkeypatch):
    def boom(method, path, data=None):
        raise payments.StripeError("Stripe POST /products -> 401: nope")
    monkeypatch.setattr(payments, "_req", boom)
    tool = {"name": "t"}
    assert payments.ensure_payment_link(tool) is None
    assert "stripe_link" not in tool


def test_sync_links_only_fills_gaps(monkeypatch, tmp_path):
    calls = []
    monkeypatch.setattr(payments, "_req", _fake_stripe(calls))
    wrappers = tmp_path / "ai_wrappers"
    wrappers.mkdir()
    (wrappers / "tools.json").write_text(json.dumps([
        {"name": "a", "stripe_link": "https://buy.stripe.com/have"},
        {"name": "b"},
    ]))
    assert payments.sync_links(wrappers) == 1
    saved = json.loads((wrappers / "tools.json").read_text())
    assert saved[0]["stripe_link"] == "https://buy.stripe.com/have"
    assert saved[1]["stripe_link"] == "https://buy.stripe.com/test_abc"


# --- revenue polling ---------------------------------------------------------

def _sessions(*rows):
    return list(rows)


def test_poll_revenue_logs_and_dedupes(monkeypatch, tmp_path, ledger):
    sess = _sessions(
        {"id": "cs_1", "created": 100, "currency": "usd", "amount_total": 500,
         "payment_link": "plink_1", "customer_details": {"email": "a@b.c"}},
        {"id": "cs_2", "created": 200, "currency": "usd", "amount_total": 500,
         "payment_link": "plink_1", "customer_details": {"email": "d@e.f"}},
    )
    monkeypatch.setattr(payments, "fetch_completed_sessions", lambda since=0: sess)
    assert payments.poll_revenue(tmp_path, ledger) == 2
    assert ledger.get_total() == 10.0
    # Second poll with the same sessions: idempotent, nothing double-logged
    assert payments.poll_revenue(tmp_path, ledger) == 0
    assert ledger.get_total() == 10.0


def test_poll_revenue_skips_non_usd_and_zero(monkeypatch, tmp_path, ledger):
    sess = _sessions(
        {"id": "cs_eur", "created": 100, "currency": "eur", "amount_total": 500},
        {"id": "cs_zero", "created": 101, "currency": "usd", "amount_total": 0},
    )
    monkeypatch.setattr(payments, "fetch_completed_sessions", lambda since=0: sess)
    assert payments.poll_revenue(tmp_path, ledger) == 0
    assert ledger.get_total() == 0.0
    # skipped sessions are marked seen — never retried
    state = json.loads((tmp_path / payments.STATE_FILE).read_text())
    assert set(state["seen"]) == {"cs_eur", "cs_zero"}


def test_poll_revenue_cursor_advances(monkeypatch, tmp_path, ledger):
    monkeypatch.setattr(payments, "fetch_completed_sessions", lambda since=0: [
        {"id": "cs_9", "created": 999, "currency": "usd", "amount_total": 500},
    ])
    payments.poll_revenue(tmp_path, ledger)
    state = json.loads((tmp_path / payments.STATE_FILE).read_text())
    assert state["last_created"] == 999


def test_fetch_completed_sessions_query(monkeypatch):
    seen = {}

    def fake(method, path, data=None):
        seen["method"], seen["path"] = method, path
        return {"data": []}
    monkeypatch.setattr(payments, "_req", fake)
    payments.fetch_completed_sessions(since=123)
    assert seen["method"] == "GET"
    assert "status=complete" in seen["path"]
    assert "created" in seen["path"] and "123" in seen["path"]
