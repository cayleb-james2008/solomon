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

STALL_SWEEPS = 3   # consecutive error/preflight sweeps (incl. this one) that mark a lane STALLED


def _now() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _recent_snapshots(name: str, k: int) -> list:
    """The last `k` persisted _monitor.jsonl snapshots for repo `name` (oldest→newest), excluding the
    current sweep (it hasn't been written yet — the caller appends it). [] on missing/corrupt file."""
    try:
        with open(MON_LOG, "r", encoding="utf-8") as f:
            lines = f.read().splitlines()
    except OSError:
        return []
    out = []
    for line in lines:
        line = line.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(rec, dict) and rec.get("repo") == name:
            out.append(rec)
    return out[-k:]


def should_restart(running: bool, hb: dict, paused: bool, stop_pending: bool) -> bool:
    """Restart only a CRASHED loop. Pure (no IO) so the decision is unit-tested directly.

    True iff: not running, NOT explicitly paused, NO pending stop sentinel, and the repo has a
    heartbeat whose status is anything except a deliberate stop/halt. Restarted: a live-phase crash
    (iterating/sleeping/idle) AND a transient/retryable error (e.g. a flaky red base gate). Left alone:
      - ``"stopped"``                  — a clean exit (operator Stop / max-iterations);
      - ``"error"`` + ``phase=reverted`` — a revert-failure HALT that needs operator cleanup (a blind
                                          restart just re-hits the known-bad tree);
      - no heartbeat                   — a repo that never ran (the watchdog keeps enabled loops
                                          alive, it does not auto-enable new ones).
    A persistent error (no key, dirty base) restarts, re-errors immediately, and the supervisor's
    diagnose/anti-thrash escalates it — so it surfaces without the watchdog having to classify it."""
    if running or paused or stop_pending:
        return False
    status = (hb or {}).get("status")
    if not status or status == "stopped":
        return False
    if status == "error" and (hb or {}).get("phase") == "reverted":
        return False                     # the revert-failure HALT — operator cleanup, not a restart
    return True


def _auto_push() -> bool:
    try:
        with open(os.path.join(HERE, ".solomon.json"), "r", encoding="utf-8") as f:
            return bool(json.load(f).get("auto_push", True))
    except (OSError, json.JSONDecodeError):
        return True


def _base_is_clean(path) -> bool:
    """True iff the repo working tree is fully clean — no modified tracked files AND no untracked
    non-ignored files (`git status --porcelain` empty). Used to auto-recover a `dirty_base_persistent`
    self-stop ONLY once the operator has actually cleaned the tree, so clearing the stop can never
    thrash (a still-dirty base stays stopped)."""
    if not path or not os.path.isdir(path):
        return False
    try:
        r = control._run(["git", "-C", path, "status", "--porcelain"])   # windowless, guarded spawn
    except OSError:
        return False
    return r.returncode == 0 and not (r.stdout or "").strip()


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
        # Anti-wedge: the runner self-stops on a PERSISTENTLY dirty base (STOP sentinel +
        # reason="dirty_base_persistent") so it doesn't spin forever — but that sentinel otherwise pins
        # the lane DEAD until a human clicks Start (the "leave it on, come back to a dead lane" wedge).
        # If the base is now CLEAN again, clear the self-written sentinel so should_restart heals the
        # lane automatically. Safe: only fires on the runner's own reason marker AND a verified-clean
        # tree, so it never thrashes and never touches a true operator Stop (status="stopped", no reason).
        if (not running and not paused and stop_pending
                and hb.get("status") == "error"
                and hb.get("reason") == "dirty_base_persistent"
                and _base_is_clean(control._repo_path(r))):
            try:
                os.remove(os.path.join(rt, "stop"))
                stop_pending = False
                actions.append(f"{name} auto-recover: base clean again — cleared dirty_base_persistent stop")
            except OSError:
                pass
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
        # STALL DETECTOR: a lane that is STILL RUNNING but has repeated the SAME preflight refusal
        # (status=error, phase=preflight) every sweep is wedged — it spins forever re-hitting an
        # un-pushed base commit / untracked-file refusal, and a per-sweep snapshot alone reports it
        # "running" so the operator never sees it. Compare across sweeps: if this snap AND the prior
        # STALL_SWEEPS-1 persisted snaps are all error/preflight, escalate (do NOT auto-restart — a
        # restart just re-hits the refusal). Anti-thrash: escalate once (skip if an escalation.json
        # with this category already exists).
        if snap["running"] and snap["status"] == "error" and snap["phase"] == "preflight":
            window = _recent_snapshots(name, STALL_SWEEPS - 1) + [snap]
            if (len(window) >= STALL_SWEEPS
                    and all(s.get("status") == "error" and s.get("phase") == "preflight"
                            for s in window)):
                existing = solomon.read_escalation(r) or {}
                if existing.get("category") != "running_stalled":
                    actions.append(f"{name} STALLED: stuck in preflight for {STALL_SWEEPS} sweeps")
                    try:
                        solomon._write_escalation(r, {
                            "category": "running_stalled",
                            "evidence": (f"running but stuck in preflight for {STALL_SWEEPS} consecutive "
                                         f"sweeps — {(hb2.get('last_summary') or '')[:200]}")})
                    except Exception:  # noqa: BLE001 — a watchdog must never die on one bad repo
                        pass
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
