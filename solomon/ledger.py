"""Revenue ledger — append-only JSONL log of all revenue events.

Schema: {"ts": ISO8601, "channel": str, "amount_usd": float, "source": str, "note": str}
Money-IN only. The money guard is sacred — Solomon never moves money out.
"""
import json
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional


class Ledger:
    """Append-only revenue ledger."""

    SCHEMA_ROW = {
        "ts": "2026-01-01T00:00:00Z",
        "channel": "_schema",
        "amount_usd": 0.0,
        "source": "_schema",
        "note": "Revenue ledger schema. Money-IN only.",
    }

    def __init__(self, path: Path):
        self.path = path
        if not self.path.exists():
            self._write_row(self.SCHEMA_ROW)

    def _write_row(self, row: dict):
        with open(self.path, "a", encoding="utf-8") as f:
            f.write(json.dumps(row, ensure_ascii=False) + "\n")

    def log_revenue(self, channel: str, amount_usd: float, source: str, note: str = ""):
        """Log a revenue event. Money-IN only — the caller must have passed the guard."""
        row = {
            "ts": datetime.now(timezone.utc).isoformat(),
            "channel": channel,
            "amount_usd": round(amount_usd, 2),
            "source": source,
            "note": note,
        }
        self._write_row(row)

    def get_total(self) -> float:
        """Get the rolling total of all revenue (excluding schema row)."""
        if not self.path.exists():
            return 0.0
        total = 0.0
        with open(self.path, "r", encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    row = json.loads(line)
                    if row.get("channel") == "_schema":
                        continue
                    total += float(row.get("amount_usd", 0))
                except (json.JSONDecodeError, ValueError):
                    continue
        return round(total, 2)

    def get_recent(self, n: int = 10) -> list[dict]:
        """Get the N most recent revenue entries."""
        if not self.path.exists():
            return []
        lines = []
        with open(self.path, "r", encoding="utf-8") as f:
            for line in f:
                lines.append(line.strip())
        rows = []
        for line in reversed(lines):
            if not line:
                continue
            try:
                row = json.loads(line)
                if row.get("channel") == "_schema":
                    continue
                rows.append(row)
                if len(rows) >= n:
                    break
            except json.JSONDecodeError:
                continue
        return rows
