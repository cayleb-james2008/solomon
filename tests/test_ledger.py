"""Tests for the revenue ledger."""
import json
import tempfile
from pathlib import Path

from solomon.ledger import Ledger


def test_ledger_creates_schema_row():
    """Ledger should create a schema row on init."""
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "revenue.jsonl"
        ledger = Ledger(path)
        assert path.exists()
        lines = path.read_text(encoding="utf-8").strip().split("\n")
        assert len(lines) == 1
        row = json.loads(lines[0])
        assert row["channel"] == "_schema"


def test_ledger_logs_revenue():
    """Ledger should append revenue entries."""
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "revenue.jsonl"
        ledger = Ledger(path)
        ledger.log_revenue("freelance", 25.00, "fiverr", "Test gig payment")
        lines = path.read_text(encoding="utf-8").strip().split("\n")
        # schema row + 1 revenue row
        assert len(lines) == 2
        row = json.loads(lines[1])
        assert row["channel"] == "freelance"
        assert row["amount_usd"] == 25.00
        assert row["source"] == "fiverr"


def test_ledger_total_excludes_schema():
    """Total should exclude the schema row."""
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "revenue.jsonl"
        ledger = Ledger(path)
        ledger.log_revenue("freelance", 50.00, "test")
        ledger.log_revenue("content", 15.50, "test")
        total = ledger.get_total()
        assert total == 65.50


def test_ledger_total_zero_when_empty():
    """Total should be 0 when only the schema row exists."""
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "revenue.jsonl"
        ledger = Ledger(path)
        assert ledger.get_total() == 0.0


def test_ledger_get_recent():
    """get_recent should return the N most recent entries."""
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "revenue.jsonl"
        ledger = Ledger(path)
        for i in range(5):
            ledger.log_revenue("test", float(i), "src")
        recent = ledger.get_recent(3)
        assert len(recent) == 3
        # Most recent first
        assert recent[0]["amount_usd"] == 4.0
        assert recent[1]["amount_usd"] == 3.0
