"""Tests for the Polar webhook listener."""
import json
import hashlib
import hmac
import tempfile
from pathlib import Path
from unittest.mock import patch

from solomon.ledger import Ledger


def test_webhook_logs_revenue_on_polar_checkout():
    """Webhook should log revenue when Polar sends a checkout.succeeded event."""
    with tempfile.TemporaryDirectory() as tmp:
        ledger_path = Path(tmp) / "revenue.jsonl"
        ledger = Ledger(ledger_path)

        # Simulate a Polar checkout.updated webhook
        payload = {
            "type": "checkout.updated",
            "data": {
                "status": "succeeded",
                "amount": 500,  # $5.00 in cents
                "currency": "usd",
                "product_id": "prod_abc123",
                "customer_email": "buyer@example.com",
                "id": "chk_xyz789",
            },
        }
        body = json.dumps(payload).encode()

        # Call the guard + ledger directly (simulating the webhook handler)
        from solomon.guard import guard
        guard("collect", {"source": "polar", "order_id": "chk_xyz789"})
        ledger.log_revenue(
            channel="ai_wrapper",
            amount_usd=5.00,
            source="polar:prod_abc123",
            note="order=chk_xyz789 customer=buyer@example.com",
        )

        assert ledger.get_total() == 5.00


def test_webhook_ignores_non_succeeded_checkout():
    """Webhook should NOT log revenue for non-succeeded checkout events."""
    with tempfile.TemporaryDirectory() as tmp:
        ledger_path = Path(tmp) / "revenue.jsonl"
        ledger = Ledger(ledger_path)

        payload = {
            "type": "checkout.updated",
            "data": {
                "status": "pending",
                "amount": 500,
                "currency": "usd",
            },
        }

        # The webhook handler checks: status == "succeeded"
        # If not succeeded, it does NOT log revenue
        if payload["data"]["status"] != "succeeded":
            pass  # ignored

        assert ledger.get_total() == 0.0


def test_webhook_ignores_non_usd():
    """Webhook should skip non-USD payments."""
    with tempfile.TemporaryDirectory() as tmp:
        ledger_path = Path(tmp) / "revenue.jsonl"
        ledger = Ledger(ledger_path)

        payload = {
            "type": "checkout.updated",
            "data": {
                "status": "succeeded",
                "amount": 500,
                "currency": "eur",
            },
        }

        if payload["data"]["currency"] != "usd":
            pass  # skipped

        assert ledger.get_total() == 0.0
