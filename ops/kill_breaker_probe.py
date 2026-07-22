#!/usr/bin/env python3
"""Asmodeus kill/breaker state probe for Solomon ops.

Returns:
    0 — normal (kill switch disengaged, breaker open)
    1 — deliberate operator freeze (breaker not-open WITH LIVE_TRADING_FROZEN)
    2 — engaged kill switch OR unexpected breaker trip (restart_forbidden)

Solomon reads stdout as a number, maps to:
    yellow_at=0.5, red_at=1.5
"""
import os
import sqlite3

local_app_data = os.environ["LOCALAPPDATA"]
db_path = os.path.join(local_app_data, "Asmodeus", "state", "asmodeus.db")
frozen_marker = os.path.join(local_app_data, "Asmodeus", "state", "LIVE_TRADING_FROZEN")

con = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
kill_engaged = con.execute("SELECT engaged FROM kill_switch WHERE id=1").fetchone()[0]
breaker_state = con.execute("SELECT state FROM breaker_state WHERE id=1").fetchone()[0]
frozen = os.path.exists(frozen_marker)

if kill_engaged or (breaker_state != "open" and not frozen):
    print(2)
elif breaker_state != "open":
    print(1)
else:
    print(0)
