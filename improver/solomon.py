"""Solomon supervisor — manages the per-repo RSI agents and recovers when one breaks something.

A deterministic watchdog is the default and the only thing that ever runs unattended:
  diagnose(repo)  — file-only health classification (safe to call from get_state every poll).
  recover(repo)   — a safe ladder:
      RUNG 0  deterministic, reversible recovery (clear stale lock/stop, reset base to origin) — auto.
      RUNG 1  an OPT-IN, PR-gated pi "Solomon fix-session" for a persistent gate-red streak.
      RUNG 2  escalate to the operator (write escalation.json with copy-paste steps; do nothing
              destructive).
Every action is appended to runtime/<name>/supervisor.jsonl. Nothing here force-kills a process,
discards un-pushed commits, pushes, or merges.
"""
import json
import os
import subprocess
import sys
import time
from datetime import datetime, timezone

import control

_AUTO_SAFE = {"stale_lock", "stop_lingering", "dirty_tree", "stuck"}


def _now():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _rt(repo):
    return control._runtime_dir(repo)


def _append_jsonl(repo, fname, rec):
    rt = _rt(repo)
    if not rt:
        return
    try:
        os.makedirs(rt, exist_ok=True)
        with open(os.path.join(rt, fname), "a", encoding="utf-8") as f:
            f.write(json.dumps(rec) + "\n")
    except OSError:
        pass


def _stale(hb, repo):
    ts = hb.get("updated_at")
    if not ts:
        return False
    try:
        last = datetime.strptime(ts, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)
    except (ValueError, TypeError):
        return False
    age = (datetime.now(timezone.utc) - last).total_seconds()
    return age > max(3 * control.project_interval(repo), 3600)


def diagnose(repo):
    """Deterministic, file-only health classification (no git shell-out — cheap on every poll).
    Returns {name, healthy, category, evidence, recommended:[...], auto_safe, running}."""
    name = control._repo_name(repo)
    hb = control.read_heartbeat(repo) or {}
    running = control.is_running(repo)
    rt = _rt(repo)
    has_lock = bool(rt and os.path.exists(os.path.join(rt, "lock")))
    has_stop = bool(rt and os.path.exists(os.path.join(rt, "stop")))
    status, phase = hb.get("status"), hb.get("phase")
    summary = hb.get("last_summary") or ""
    hist = control.read_history(repo, limit=20)

    cat, ev, rec, safe = "ok", (status or ("running" if running else "idle")), [], True
    if status == "error" and ("not set" in summary or "API key" in summary):
        cat, ev, rec, safe = "no_key", summary[:160], ["add the provider API key in Settings"], False
    elif status == "error" and "GitHub not ready" in summary:
        cat, ev, rec, safe = "gh_not_ready", summary[:160], ["connect GitHub (gh auth login)"], False
    elif status == "error" and phase == "reverted":
        cat, ev, rec, safe = ("revert_failed", (summary[:200] or "revert failed"),
                              ["attempt a reset of the base branch to origin", "else manual cleanup"], False)
    elif status == "error" and phase == "preflight" and "dirty" in summary.lower():
        cat, ev, rec, safe = "dirty_tree", summary[:160], ["reset the base branch to origin"], True
    elif status == "error" and phase == "preflight" and (
            "out-of-band" in summary.lower() or "refusing to hard-reset" in summary.lower()):
        # the never-hand-patched keystone refused to hard-reset a base ahead of origin (an operator
        # hand-patch or a dead run's local commit). The runner reports it but keeps spinning the same
        # refusal every iteration; without this branch diagnose() called it 'ok' (reported healthy
        # while wedged). Surface + escalate so the operator pushes or reverts.
        cat, ev, rec, safe = ("base_out_of_band", summary[:200],
                              ["base has un-pushed / out-of-band commits — push or revert them "
                               "(the loop changes a repo only via gated PRs)"], False)
    elif has_lock and not running:
        cat, ev, rec, safe = "stale_lock", "lock file present but no live improver PID", ["clear the stale lock"], True
    elif has_stop and not running:
        cat, ev, rec, safe = "stop_lingering", "stop sentinel present, no live loop", ["clear the stop sentinel"], True
    elif running and phase not in (None, "sleep") and _stale(hb, repo):
        # a hang can freeze the heartbeat in ANY active phase (test/commit/ship/pr/merge — e.g. a
        # gate or gh call that wedges), not only 'implement'. Any non-sleep phase that goes stale
        # while the process is still alive is stuck.
        cat, ev, rec, safe = ("stuck", f"no heartbeat update for a long time while in phase '{phase}'",
                              ["stop and restart the loop"], True)
    elif len(hist) >= 3 and all(r.get("status") in ("reverted", "error") for r in hist[-3:]):
        cat, ev, rec, safe = ("gate_red_streak", "last 3 iterations reverted/errored — the gate keeps failing",
                              ["run a Solomon fix-session (opt-in)"], False)
    elif (control.project_ship(repo) == "auto-merge" and len(hist) >= 3
          and all(r.get("status") == "blocked" for r in hist[-3:])):
        # auto-merge owns landing the PR; the runner now records 'blocked' (not 'shipped') for a PR
        # left un-merged on red/awaiting CI. A streak means they pile up open and NEVER merge with no
        # other signal (the gate is green locally, so gate_red_streak misses it). Surface it so the
        # operator fixes the CI cause instead of red PRs accumulating silently.
        cat, ev, rec, safe = ("ci_red_streak",
                              "last 3 auto-merge PRs did not land (CI-red / unmerged) — CI keeps failing",
                              ["review the failing CI on the open rsi/* PRs and fix the cause"], False)

    # anti-thrash: same auto category fixed >= 3 times recently -> escalate instead of looping forever
    if safe and cat != "ok":
        same = [s for s in control.read_supervisor_log(repo, limit=4)
                if s.get("category") == cat and s.get("rung") == 0 and not s.get("escalate")]
        if len(same) >= 3:
            ev += " (auto-fixed repeatedly — escalating instead of looping)"
            safe = False
    return {"name": name, "healthy": cat == "ok", "category": cat, "evidence": ev,
            "recommended": rec, "auto_safe": safe, "running": running}


def _suggested_steps(repo, cat):
    path = control._repo_path(repo) or "<repo>"
    base = control.project_pr_target_branch(repo)
    cd = f'cd "{path}"'
    if cat == "revert_failed":
        return [cd, f"git checkout --force {base}", "git reset --hard", f"git reset --hard origin/{base}", "git status"]
    if cat == "no_key":
        return ["Open Solomon → Settings and add the provider's API key, then retry"]
    if cat == "gh_not_ready":
        return ["gh auth login   # authenticate, then retry"]
    if cat == "stuck":
        return [cd, "# find the hung improver PID then stop it manually (Solomon will not force-kill):",
                "taskkill /F /T /PID <pid>   # Windows", "# or:  kill <pid>   # Unix"]
    if cat == "base_out_of_band":
        return [cd, f"git log origin/{base}..{base} --oneline   # the un-pushed / out-of-band commits",
                f"git push origin {base}                  # if they're wanted, OR (destructive):",
                f"git reset --hard origin/{base}           # discard them — the loop ships only via gated PRs"]
    if cat == "ci_red_streak":
        return [cd, "gh pr list --state open            # the CI-red rsi/* PRs that won't merge",
                "gh pr checks <number>                  # which check failed",
                "# fix the failing-CI cause (or close the bad PRs); tick 'Allow AI fix' for a fix-session"]
    return [cd, "git status"]


def _write_escalation(repo, d):
    rt = _rt(repo)
    if not rt:
        return
    rec = {"ts": _now(), "category": d["category"], "evidence": d["evidence"],
           "suggested_manual_steps": _suggested_steps(repo, d["category"])}
    try:
        os.makedirs(rt, exist_ok=True)
        with open(os.path.join(rt, "escalation.json"), "w", encoding="utf-8") as f:
            json.dump(rec, f, indent=2)
    except OSError:
        pass


def read_escalation(repo):
    rt = _rt(repo)
    if not rt:
        return None
    try:
        with open(os.path.join(rt, "escalation.json"), "r", encoding="utf-8") as f:
            return json.load(f)
    except (OSError, json.JSONDecodeError):
        return None


def clear_escalation(repo):
    rt = _rt(repo)
    if not rt:
        return {"ok": False, "error": "repo has no 'path'"}
    try:
        os.remove(os.path.join(rt, "escalation.json"))
    except OSError:
        pass
    return {"ok": True}


def _finish(repo, d, actions, escalate, msg):
    rung = 1 if d["category"] == "gate_red_streak" else (2 if escalate and not actions else 0)
    _append_jsonl(repo, "supervisor.jsonl",
                  {"ts": _now(), "category": d["category"], "rung": rung,
                   "actions": actions, "escalate": escalate, "message": msg})
    if escalate:
        _write_escalation(repo, d)
    return {"ok": not escalate, "category": d["category"], "actions_taken": actions,
            "escalate": escalate, "message": msg}


def solomon_fix_session(repo, auto_push=True):
    """Spawn a one-shot, detached pi Solomon fix-session (run_improver.py --solomon). It runs through
    the normal branch + gate + PR path, so a Solomon fix is itself a reviewable PR — never a direct
    write to base, never bypassing the gate or the operator's merge. Honors the global auto_push gate
    (effective_ship): when auto_push is off the fix-session ships LOCAL only, never pushing/merging."""
    path, name = control._repo_path(repo), control._repo_name(repo)
    if not path or not name:
        return {"ok": False, "error": "repo has no name/path"}
    py = control._venv_python(repo)
    runner = os.path.join(control.HERE, "improver", "run_improver.py")
    if not py or not os.path.exists(py):
        return {"ok": False, "error": f"venv python not found: {py}"}
    if not os.path.exists(runner):
        return {"ok": False, "error": "runner not found"}
    flags = 0
    if sys.platform == "win32":
        flags = subprocess.CREATE_NO_WINDOW | subprocess.DETACHED_PROCESS
    args = [py, runner, "--repo", path, "--name", name,
            "--provider", control.project_provider(repo), "--model", control.project_model(repo),
            "--ship", control.effective_ship(repo, auto_push),
            "--pr-target-branch", control.project_pr_target_branch(repo),
            "--reasoning", control.project_reasoning(repo) or "", "--solomon"]
    try:
        # Match the other spawn sites (control.start/beautify/enrich_contract): strip the stale gh token
        # + PYTHONPATH/PYTHONHOME at the boundary so the child venv python uses its own stdlib + keyring.
        proc = subprocess.Popen(args, cwd=path, creationflags=flags, stdout=subprocess.DEVNULL,
                                stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL, close_fds=True,
                                env=control._clean_subenv())
        return {"ok": True, "pid": proc.pid}
    except OSError as e:
        return {"ok": False, "error": str(e)}


def recover(repo, allow_pi=False, allow_restart=True, auto_push=True):
    """Walk the recovery ladder for one repo. Returns
    {ok, category, actions_taken:[...], escalate:bool, message}. auto_push threads the global gate so
    a restart / fix-session ships LOCAL-only when pushing is disabled."""
    d = diagnose(repo)
    cat = d["category"]
    if cat == "ok":
        return {"ok": True, "category": "ok", "actions_taken": [], "escalate": False, "message": "healthy"}

    # RUNG 2 — not auto-safe and not a code-fix candidate -> escalate, do nothing destructive
    if not d["auto_safe"] and cat != "gate_red_streak":
        return _finish(repo, d, [], escalate=True, msg="escalated — operator action required")

    actions = []
    if cat == "stale_lock":
        r = control.clear_lock(repo)
        actions.append("clear_lock")
        if not r.get("ok"):
            return _finish(repo, d, actions, escalate=True, msg=r.get("error"))
    elif cat == "stop_lingering":
        rt = _rt(repo)
        try:
            os.remove(os.path.join(rt, "stop"))
        except OSError:
            pass
        actions.append("clear_stop")
    elif cat == "dirty_tree":
        # supervisor-authorized-recovery: never hard-reset git UNDER a live iteration (that dirty
        # tree may be the running loop's in-progress work). Refuse + escalate while it's alive.
        if control.is_running(repo):
            return _finish(repo, d, actions, escalate=True,
                           msg="loop is live — stop it before Solomon resets the working tree")
        r = control.reset_to_base(repo)
        actions.append("reset_to_base")
        if not r.get("ok"):
            return _finish(repo, d, actions, escalate=True, msg=r.get("error"))
    elif cat == "stuck":
        control.stop(repo)
        actions.append("stop")
        for _ in range(10):                      # grace window for the loop to exit cleanly
            if not control.is_running(repo):
                break
            time.sleep(1)
        if control.is_running(repo):
            return _finish(repo, d, actions, escalate=True,
                           msg="loop would not stop — manual kill required (Solomon will not force-kill)")
        if allow_restart:
            control.start(repo, auto_push=auto_push)
            actions.append("restart")
    elif cat == "gate_red_streak":
        if not (allow_pi and control.keys_status().get(control.project_provider(repo))):
            return _finish(repo, d, actions, escalate=True,
                           msg="persistent gate failure — tick 'Allow AI fix' to run a Solomon fix-session")
        # A fix-session takes the runner's single-flight lock; if the loop is still live the child
        # would race the lock and silently exit. Require a quiesced loop first.
        if control.is_running(repo):
            return _finish(repo, d, actions, escalate=True,
                           msg="loop is live — stop it before running a Solomon fix-session")
        r = solomon_fix_session(repo, auto_push=auto_push)
        actions.append("solomon_fix_session")
        return _finish(repo, d, actions, escalate=not r.get("ok"),
                       msg=("launched Solomon fix-session" if r.get("ok") else r.get("error")))
    return _finish(repo, d, actions, escalate=False, msg="recovered: " + ", ".join(actions))
