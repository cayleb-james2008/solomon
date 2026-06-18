#!/usr/bin/env python3
"""Solomon overnight watchdog + data collector.

Run periodically (a Windows scheduled task — see scripts/watchdog.cmd). Each sweep, for every
registered repo, it:

  1. **Restarts a CRASHED loop.** A loop that exits cleanly (operator Stop, or max-iterations)
     writes ``status="stopped"`` in its last heartbeat via the runner's ``finally`` block; a
     crash/kill leaves the last live status (``iterating``/``sleeping``/``error``). The watchdog
     restarts only the latter — so an operator Stop is left alone, but a crash is healed. The
     single-flight lock (fixed) makes a redundant start a no-op, so this is race-safe.
  2. **Runs the supervisor's RUNG-0 recovery** (deterministic + reversible only: stale lock,
     lingering stop, dirty-tree reset when quiesced). Never a pi fix, push, merge, or force-kill.
  3. **Appends a JSON snapshot per repo** to ``runtime/_monitor.jsonl`` for overnight trend
     analysis (status, iteration, last outcome, diagnosis, whether it was restarted).

Operator controls (honored, non-destructive):
  - ``runtime/_watchdog.disabled``  — global kill-switch; the sweep does nothing.
  - ``runtime/<name>/paused``       — never auto-restart this one repo.
A clean Stop from the GUI already sets ``status="stopped"`` and is respected automatically.
"""
import json
import os
import sys
from datetime import datetime, timezone

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.join(HERE, "improver"))

import control   # noqa: E402
import solomon   # noqa: E402

DISABLED = os.path.join(HERE, "runtime", "_watchdog.disabled")
MON_LOG = os.path.join(HERE, "runtime", "_monitor.jsonl")


def _now() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def should_restart(running: bool, hb: dict, paused: bool, stop_pending: bool) -> bool:
    """Restart only a CRASHED loop. Pure (no IO) so the decision is unit-tested directly.

    True iff: not running, NOT explicitly paused, NO pending stop sentinel, and the repo has a
    heartbeat whose status is a LIVE phase (iterating/sleeping/idle) — i.e. it died unexpectedly.
    Left alone:
      - ``"stopped"`` — a clean exit (operator Stop / max-iterations);
      - ``"error"``   — a state that needs operator/supervisor attention (a halt on revert failure,
                        a missing key, a dirty base, an out-of-band base move); a blind restart would
                        just re-hit the error, so the supervisor escalates it instead;
      - no heartbeat  — a repo that never ran (the watchdog keeps enabled loops alive, it does not
                        auto-enable new ones)."""
    if running or paused or stop_pending:
        return False
    status = (hb or {}).get("status")
    return bool(status) and status not in ("stopped", "error")


def _auto_push() -> bool:
    try:
        with open(os.path.join(HERE, ".solomon.json"), "r", encoding="utf-8") as f:
            return bool(json.load(f).get("auto_push", True))
    except (OSError, json.JSONDecodeError):
        return True


def sweep() -> dict:
    """One watchdog pass over all repos. Returns {ts, actions:[...], snapshots:[...]}."""
    if os.path.exists(DISABLED):
        return {"ts": _now(), "disabled": True, "actions": [], "snapshots": []}
    auto_push = _auto_push()
    actions, snapshots = [], []
    for r in control.load_repos():
        if not isinstance(r, dict) or not r.get("name"):
            continue
        name = r["name"]
        rt = control._runtime_dir(r)
        paused = bool(rt and os.path.exists(os.path.join(rt, "paused")))
        stop_pending = bool(rt and os.path.exists(os.path.join(rt, "stop")))
        running = control.is_running(r)
        hb = control.read_heartbeat(r) or {}
        restarted = False
        if should_restart(running, hb, paused, stop_pending):
            res = control.start(r, auto_push=auto_push)
            restarted = bool(res.get("ok") and not res.get("already"))
            actions.append(f"restarted {name} (pid {res.get('pid')})" if restarted
                           else f"restart {name} FAILED: {res.get('error')}")
        # RUNG-0 deterministic recovery (never a pi fix here: allow_pi=False)
        try:
            rec = solomon.recover(r, allow_pi=False, allow_restart=auto_push, auto_push=auto_push)
            if rec.get("actions_taken"):
                actions.append(f"{name} recover: {','.join(rec['actions_taken'])}")
            if rec.get("escalate"):
                actions.append(f"{name} ESCALATED: {rec.get('category')}")
        except Exception as e:  # noqa: BLE001 — a watchdog must never die on one bad repo
            actions.append(f"{name} recover error: {e}")
        hist = control.read_history(r, limit=1)
        last = hist[-1] if hist else {}
        hb2 = control.read_heartbeat(r) or {}
        try:
            diag = solomon.diagnose(r).get("category")
        except Exception:  # noqa: BLE001
            diag = "?"
        snap = {"ts": _now(), "repo": name, "running": control.is_running(r),
                "restarted": restarted, "paused": paused,
                "status": hb2.get("status"), "phase": hb2.get("phase"),
                "iteration": hb2.get("iteration"), "last_status": last.get("status"),
                "diagnosis": diag}
        snapshots.append(snap)
    return {"ts": _now(), "disabled": False, "actions": actions, "snapshots": snapshots}


def main() -> int:
    out = sweep()
    if out.get("disabled"):
        print(f"{out['ts']} watchdog disabled (kill-switch present) — no action")
        return 0
    try:
        os.makedirs(os.path.dirname(MON_LOG), exist_ok=True)
        with open(MON_LOG, "a", encoding="utf-8") as f:
            for snap in out["snapshots"]:
                f.write(json.dumps(snap) + "\n")
    except OSError:
        pass
    summary = "; ".join(out["actions"]) if out["actions"] else "all healthy, no action"
    running = sum(1 for s in out["snapshots"] if s["running"])
    line = f"{out['ts']} watchdog: {running}/{len(out['snapshots'])} running | {summary}"
    print(line)
    try:  # self-log so the task can run windowless via pythonw (no stdout redirection needed)
        with open(os.path.join(HERE, "runtime", "_watchdog.out.log"), "a", encoding="utf-8") as f:
            f.write(line + "\n")
    except OSError:
        pass
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
