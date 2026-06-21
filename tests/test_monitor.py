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
    # not running, last status was a LIVE phase (finally never ran) -> crash -> restart
    assert monitor.should_restart(running=False, hb={"status": "sleeping"}, paused=False, stop_pending=False) is True
    assert monitor.should_restart(running=False, hb={"status": "iterating"}, paused=False, stop_pending=False) is True
    assert monitor.should_restart(running=False, hb={"status": "idle"}, paused=False, stop_pending=False) is True


def test_leave_clean_stop_alone():
    # a clean exit (operator Stop / max-iterations) sets status="stopped" -> do NOT restart
    assert monitor.should_restart(running=False, hb={"status": "stopped"}, paused=False, stop_pending=False) is False


def test_leave_revert_halt_for_operator():
    # the revert-failure HALT (status=error, phase=reverted) needs operator cleanup -> don't restart
    assert monitor.should_restart(running=False, hb={"status": "error", "phase": "reverted"},
                                  paused=False, stop_pending=False) is False


def test_restart_transient_error():
    # a transient/retryable error (e.g. a flaky red base gate, status=error/phase=preflight) that is
    # NOT the halt IS retried — otherwise a one-off blip would strand the loop down all night.
    assert monitor.should_restart(running=False, hb={"status": "error", "phase": "preflight"},
                                  paused=False, stop_pending=False) is True


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


# --- dirty_base_persistent self-stop auto-recovery (anti-wedge) ---------------------------------- #
import control  # noqa: E402
import solomon  # noqa: E402


def _stub_sweep_env(tmp_path, monkeypatch, *, hb, base_clean):
    """Wire a one-repo sweep() with a self-written stop sentinel present and a stubbed environment."""
    rt = tmp_path / "rt"; rt.mkdir()
    (rt / "stop").write_text("dirty_base_persistent\n", encoding="utf-8")
    repo = {"name": "demo", "path": str(tmp_path / "repo")}
    started = {"called": False}
    monkeypatch.setattr(control, "load_repos", lambda: [repo])
    monkeypatch.setattr(control, "_runtime_dir", lambda r: str(rt))
    monkeypatch.setattr(control, "_repo_path", lambda r: r.get("path"))
    monkeypatch.setattr(control, "is_running", lambda r: False)
    monkeypatch.setattr(control, "read_heartbeat", lambda r: dict(hb))
    monkeypatch.setattr(control, "read_history", lambda r, limit=1: [])
    monkeypatch.setattr(control, "start", lambda r, auto_push=True: started.update(called=True) or {"ok": True, "pid": 1})
    monkeypatch.setattr(monitor, "DISABLED", str(tmp_path / "_watchdog.disabled"))  # isolate from a real operator kill-switch
    monkeypatch.setattr(monitor, "_base_is_clean", lambda p: base_clean)
    monkeypatch.setattr(solomon, "recover", lambda *a, **k: {"actions_taken": [], "escalate": False})
    monkeypatch.setattr(solomon, "diagnose", lambda r: {"category": "ok"})
    return rt, started


def test_dirty_base_persistent_auto_recovers_when_base_clean(tmp_path, monkeypatch):
    # self-stopped on a persistently-dirty base; the operator cleaned the tree -> watchdog clears the
    # self-written stop sentinel and restarts, instead of leaving the lane dead until a human clicks Start.
    rt, started = _stub_sweep_env(
        tmp_path, monkeypatch,
        hb={"status": "error", "phase": "preflight", "reason": "dirty_base_persistent"},
        base_clean=True)
    out = monitor.sweep()
    assert not (rt / "stop").exists()                       # self-written sentinel cleared
    assert started["called"] is True                        # lane restarted
    assert any("auto-recover" in a for a in out["actions"])


def test_dirty_base_persistent_left_alone_when_still_dirty(tmp_path, monkeypatch):
    # base is STILL dirty -> do not clear, do not restart (no thrash; operator must clean it first).
    rt, started = _stub_sweep_env(
        tmp_path, monkeypatch,
        hb={"status": "error", "phase": "preflight", "reason": "dirty_base_persistent"},
        base_clean=False)
    monitor.sweep()
    assert (rt / "stop").exists()                           # sentinel preserved
    assert started["called"] is False                       # not restarted


def test_operator_stop_sentinel_never_auto_cleared(tmp_path, monkeypatch):
    # a stop with no dirty_base_persistent reason marker (e.g. an operator Stop in flight) is NEVER
    # auto-cleared, even with a clean base — only the runner's own self-stop reason qualifies.
    rt, started = _stub_sweep_env(
        tmp_path, monkeypatch,
        hb={"status": "stopped"},
        base_clean=True)
    monitor.sweep()
    assert (rt / "stop").exists()                           # operator intent preserved
    assert started["called"] is False


# --- stall detector: a RUNNING lane stuck repeating the same preflight refusal ------------------- #
import json as _json  # noqa: E402


def _stall_env(tmp_path, monkeypatch, *, hb, prior_snaps, existing_escalation=None):
    """Wire a one-repo sweep() with a stubbed _monitor.jsonl tail and capture escalation writes."""
    rt = tmp_path / "rt"; rt.mkdir()
    mon = tmp_path / "_monitor.jsonl"
    if prior_snaps:
        mon.write_text("\n".join(_json.dumps(s) for s in prior_snaps) + "\n", encoding="utf-8")
    monkeypatch.setattr(monitor, "MON_LOG", str(mon))
    monkeypatch.setattr(monitor, "DISABLED", str(tmp_path / "_watchdog.disabled"))  # isolate from a real operator kill-switch
    repo = {"name": "demo", "path": str(tmp_path / "repo")}
    monkeypatch.setattr(control, "load_repos", lambda: [repo])
    monkeypatch.setattr(control, "_runtime_dir", lambda r: str(rt))
    monkeypatch.setattr(control, "_repo_path", lambda r: r.get("path"))
    monkeypatch.setattr(control, "is_running", lambda r: True)          # the lane IS still running
    monkeypatch.setattr(control, "read_heartbeat", lambda r: dict(hb))
    monkeypatch.setattr(control, "read_history", lambda r, limit=1: [])
    monkeypatch.setattr(control, "start", lambda r, auto_push=True: {"ok": True, "pid": 1})
    monkeypatch.setattr(solomon, "recover", lambda *a, **k: {"actions_taken": [], "escalate": False})
    monkeypatch.setattr(solomon, "diagnose", lambda r: {"category": "base_out_of_band"})
    written = {}
    monkeypatch.setattr(solomon, "_write_escalation", lambda r, d: written.update(d))
    monkeypatch.setattr(solomon, "read_escalation", lambda r: dict(existing_escalation) if existing_escalation else None)
    return rt, written


def _err_preflight(n):
    return [{"repo": "demo", "status": "error", "phase": "preflight"} for _ in range(n)]


def test_stall_escalates_after_three_error_preflight_sweeps(tmp_path, monkeypatch):
    # a running lane whose last 3 snapshots (2 prior + this one) are all error/preflight is wedged on
    # a repeated refusal -> escalate with category 'running_stalled' (and NEVER auto-restart).
    rt, written = _stall_env(
        tmp_path, monkeypatch,
        hb={"status": "error", "phase": "preflight", "last_summary": "out-of-band base commit"},
        prior_snaps=_err_preflight(2))
    out = monitor.sweep()
    assert written.get("category") == "running_stalled"
    assert any("STALLED" in a for a in out["actions"])


def test_stall_not_flagged_before_threshold(tmp_path, monkeypatch):
    # only 1 prior error/preflight snap + this one = 2 < 3 -> not yet a stall, no escalation.
    rt, written = _stall_env(
        tmp_path, monkeypatch,
        hb={"status": "error", "phase": "preflight"},
        prior_snaps=_err_preflight(1))
    out = monitor.sweep()
    assert written == {}
    assert not any("STALLED" in a for a in out["actions"])


def test_stall_not_flagged_when_phase_recovered(tmp_path, monkeypatch):
    # the window is broken by a non-error/preflight snap (the lane recovered between sweeps) -> no stall.
    rt, written = _stall_env(
        tmp_path, monkeypatch,
        hb={"status": "error", "phase": "preflight"},
        prior_snaps=[{"repo": "demo", "status": "error", "phase": "preflight"},
                     {"repo": "demo", "status": "iterating", "phase": "implement"}])
    monitor.sweep()
    assert written == {}


def test_stall_anti_thrash_escalates_once(tmp_path, monkeypatch):
    # an escalation.json with category 'running_stalled' already exists -> do NOT re-escalate (no thrash).
    rt, written = _stall_env(
        tmp_path, monkeypatch,
        hb={"status": "error", "phase": "preflight"},
        prior_snaps=_err_preflight(2),
        existing_escalation={"category": "running_stalled"})
    out = monitor.sweep()
    assert written == {}                                    # _write_escalation NOT called again
    assert not any("STALLED" in a for a in out["actions"])
