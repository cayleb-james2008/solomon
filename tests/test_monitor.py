"""Tests for the overnight watchdog's restart-decision logic (monitor.should_restart).

The watchdog must restart a CRASHED loop but leave a cleanly-stopped one alone — distinguished
by the last heartbeat status ("stopped" == clean exit via the runner's finally block)."""
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)
sys.path.insert(0, os.path.join(ROOT, "improver"))

import monitor  # noqa: E402


def test_restart_crashed_loop():
    # not running, last status was a live phase (finally never ran) -> crash -> restart
    assert monitor.should_restart(running=False, hb={"status": "sleeping"}, paused=False, stop_pending=False) is True
    assert monitor.should_restart(running=False, hb={"status": "iterating"}, paused=False, stop_pending=False) is True
    assert monitor.should_restart(running=False, hb={"status": "error"}, paused=False, stop_pending=False) is True


def test_leave_clean_stop_alone():
    # a clean exit (operator Stop / max-iterations) sets status="stopped" -> do NOT restart
    assert monitor.should_restart(running=False, hb={"status": "stopped"}, paused=False, stop_pending=False) is False


def test_never_restart_running():
    assert monitor.should_restart(running=True, hb={"status": "iterating"}, paused=False, stop_pending=False) is False


def test_paused_blocks_restart():
    assert monitor.should_restart(running=False, hb={"status": "sleeping"}, paused=True, stop_pending=False) is False


def test_pending_stop_blocks_restart():
    # a stop sentinel is present: the operator just asked it to stop -> don't fight it
    assert monitor.should_restart(running=False, hb={"status": "sleeping"}, paused=False, stop_pending=True) is False


def test_no_heartbeat_not_auto_enabled():
    # a never-run repo (no heartbeat) is left alone — the watchdog keeps enabled loops alive,
    # it does not auto-enable new ones
    assert monitor.should_restart(running=False, hb={}, paused=False, stop_pending=False) is False
