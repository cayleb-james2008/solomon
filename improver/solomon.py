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
import time
from datetime import datetime, timezone

import control
from winproc import hidden_subprocess_kwargs

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
    return age > max(3 * control.project_interval(repo), control.LOCK_LIVE_FLOOR_S)


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
    reason = hb.get("reason")
    summary = hb.get("last_summary") or ""
    hist = control.read_history(repo, limit=20)

    cat, ev, rec, safe = "ok", (status or ("running" if running else "idle")), [], True
    if status == "error" and reason == "needs_goal":
        # the empty-goal guard fired (run_improver): no north-star GOAL and only a placeholder/deferred
        # backlog, so the loop skips instead of fabricating work. NON-auto, escalate-only (like no_key) —
        # the operator must set a goal; Solomon must not auto-restart (a restart just re-hits the guard).
        cat, ev, rec, safe = ("needs_goal", summary[:200] or "no north-star GOAL and no actionable backlog",
                              ["set this repo's GOAL in Config so the loop has an objective"], False)
    elif status == "error" and ("not set" in summary or "API key" in summary):
        cat, ev, rec, safe = "no_key", summary[:160], ["add the provider API key in Settings"], False
    elif status == "error" and "GitHub not ready" in summary:
        cat, ev, rec, safe = "gh_not_ready", summary[:160], ["connect GitHub (gh auth login)"], False
    elif status == "error" and phase == "reverted":
        cat, ev, rec, safe = ("revert_failed", (summary[:200] or "revert failed"),
                              ["attempt a reset of the base branch to origin", "else manual cleanup"], False)
    elif (status == "error" and phase == "preflight" and "dirty" in summary.lower()
          and reason != "dirty_base_persistent"):
        # NOTE: the runner's OWN dirty_base_persistent self-stop also has status=error/phase=preflight and
        # 'dirty' in its summary, but it must NOT be treated as an auto-safe dirty_tree (which recover()
        # would hard-reset, destroying the operator's uncommitted base work the self-stop exists to
        # protect). Excluding reason here lets it fall through to stop_lingering (escalate-only while the
        # base stays dirty); monitor's auto-recover arm clears it once the base is verified clean.
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
    elif status == "error" and phase == "preflight" and "would be deleted by the preflight clean" in summary.lower():
        # The untracked-file refusal (hygiene-3): preflight refused `git clean -fd` because untracked
        # non-ignored files on the base would be destroyed (possible operator work). The runner's
        # auto-stash already failed or didn't fire (else there'd be no wedge), so this is operator-
        # gated — it must NOT be reported healthy. Escalate with steps; never auto-delete the files.
        cat, ev, rec, safe = ("untracked_refusal", summary[:200],
                              ["untracked files on the base block the preflight clean — review them, "
                               "then commit or remove them (the loop won't delete possible operator work)"], False)
    elif has_lock and not running:
        cat, ev, rec, safe = "stale_lock", "lock file present but no live improver PID", ["clear the stale lock"], True
    elif has_stop and not running:
        # Auto-clear a lingering stop ONLY for a CLEAN exit (status=='stopped'): the stop is then
        # vestigial. If the loop crashed/was killed mid-run with an operator Stop pending (the last
        # heartbeat is a live/error value, not 'stopped'), auto-clearing it + the next sweep's restart
        # would SILENTLY REVOKE the operator's Stop ("Start must not silently revoke a live stop",
        # SOLOMON_RSI halt switch). In that case escalate-only and leave the stop in place — the operator
        # presses Start to resume. (The runner's own dirty_base_persistent self-stop is separately
        # auto-recovered by the watchdog once the base is verified clean, so it doesn't need this arm.)
        clean_exit = status == "stopped"
        cat, ev, rec, safe = (
            "stop_lingering",
            "stop sentinel present after a clean exit" if clean_exit else
            "stop sentinel present but the loop did not exit cleanly (crash/kill mid-stop) — "
            "honoring the operator Stop, not auto-restarting",
            ["clear the vestigial stop sentinel"] if clean_exit else
            ["the loop was stopped but did not exit cleanly — press Start to resume, or investigate "
             "the crash; the watchdog will not auto-restart it"],
            clean_exit)
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
    elif len(hist) >= 5 and all(r.get("status") == "noop" for r in hist[-5:]):
        # The agent made NO change 5 iterations running: the backlog is exhausted or every remaining
        # item is too hard for the current model (the asmodeus 'deferred after repeated tries' state).
        # gate_red_streak only matches reverted/error, so a pure-noop churn was invisible — the loop
        # burned iterations forever with no alert (self-heal-4). Surface it so the menu gets refilled.
        cat, ev, rec, safe = ("noop_streak",
                              "5 iterations in a row made no change — the backlog looks exhausted or "
                              "too hard for the current model",
                              ["refill the backlog (run Ideate) or simplify/replace the deferred items, "
                               "or raise the repo's model"], False)
    elif status == "error":
        # Catch-all (lifecycle-3): an error we couldn't classify above — e.g. a base branch that does
        # not exist (no phase set) or a github/exec failure — must NOT fall through to 'ok'. That
        # reported a wedged repo as healthy and let the watchdog blind-restart it with no operator
        # alert. Surface it as a non-auto 'unknown_error' so recover() escalates (writes escalation.json).
        cat, ev, rec, safe = ("unknown_error", (summary[:200] or "unclassified loop error"),
                              ["review the loop's last error (dashboard / runtime log) and address the cause"], False)

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
    if cat == "needs_goal":
        return ["Open Solomon → this repo → Config and set a north-star GOAL (or add an actionable",
                "backlog item in improver/<name>/backlog.md); the loop resumes once it has an objective"]
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
    if cat == "untracked_refusal":
        return [cd, "git status --porcelain                          # the untracked files blocking preflight",
                "git stash push --include-untracked -m solomon       # if they're disposable, OR",
                'git add -A && git commit -m "operator work"         # if they are real work to keep']
    if cat == "noop_streak":
        return ["Open Solomon → this repo → Ideate to refill the backlog with fresh items,",
                "or edit improver/<name>/backlog.md to add/simplify items,",
                "or raise the repo's model in Config (the current one keeps failing to implement)"]
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
    args = [py, runner, "--repo", path, "--name", name,
            "--provider", control.project_provider(repo), "--model", control.project_model(repo),
            "--ship", control.effective_ship(repo, auto_push),
            "--pr-target-branch", control.project_pr_target_branch(repo),
            "--reasoning", control.project_reasoning(repo) or "", "--solomon"]
    try:
        # Match the other spawn sites (control.start/beautify/enrich_contract): strip the stale gh token
        # + PYTHONPATH/PYTHONHOME at the boundary so the child venv python uses its own stdlib + keyring.
        proc = subprocess.Popen(args, cwd=path, stdout=subprocess.DEVNULL,
                                stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL, close_fds=True,
                                env=control._clean_subenv(), **hidden_subprocess_kwargs(detached=True))
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
        # The repo is healthy — clear any STALE escalation.json a prior transient error left behind, so
        # a recovered repo stops showing an operator alert. This matters more now that the unknown_error
        # catch-all surfaces transient errors (e.g. a flaky base-gate-red) that then self-resolve.
        clear_escalation(repo)
        return {"ok": True, "category": "ok", "actions_taken": [], "escalate": False, "message": "healthy"}

    actions = []   # actions taken on this run (appended to supervisor.jsonl via _finish)

    # RUNG 2 — not auto-safe and not a code-fix candidate -> escalate, do nothing destructive.
    # EXCEPTION: revert_failed (review finding #2 — the revert-failure wedge). A dead iteration left
    # the repo on an un-revertable rsi/* branch and the loop halted (status=error/phase=reverted); the
    # runner will NEVER clear it itself. If the loop is confirmed NOT live, auto-run reset_to_base
    # (holding the supervisor lock so we never mutate git under a live iteration) + cleanup_worktrees
    # to drop the lingering rsi/* branches. This is RUNG-0 (reversible: checkout --force + reset --hard
    # origin/base — never force-push, never merge). If the loop IS live or the reset fails, escalate.
    if cat == "revert_failed":
        if control.is_running(repo):
            return _finish(repo, d, [], escalate=True,
                           msg="loop is live — stop it before Solomon resets the un-reverted base")
        # anti-thrash (self-heal-6): a deterministic revert-failure can recur after each successful
        # reset (an un-deletable path, a tree that re-wedges identically). The revert_failed rung is
        # NOT covered by the auto-safe anti-thrash below (safe=False), so without this it would
        # reset+restart forever. After N recent auto-resets of this category, escalate instead.
        prior_resets = sum(1 for s in control.read_supervisor_log(repo, limit=6)
                           if s.get("category") == "revert_failed"
                           and "reset_to_base" in (s.get("actions") or []))
        if prior_resets >= 3:
            return _finish(repo, d, [], escalate=True,
                           msg="revert-failure recurs after repeated auto-resets — escalating "
                               "(a deterministic cause keeps re-wedging the base)")
        ok, token = control.acquire_supervisor_lock(repo)
        if not ok:
            return _finish(repo, d, [], escalate=True,
                           msg="loop lock could not be acquired — stop it before Solomon resets the "
                               "un-reverted base")
        try:
            r = control.reset_to_base(repo)
        finally:
            control.release_supervisor_lock(repo, token)
        actions.append("reset_to_base")
        if not r.get("ok"):
            return _finish(repo, d, actions, escalate=True,
                          msg=(r.get("error") or "reset_to_base failed — escalate"))
        # the reset succeeded: clean up the lingering rsi/* branches the dead iteration left behind
        cw = control.cleanup_worktrees(repo)
        actions.append("cleanup_worktrees")
        if not cw.get("ok"):
            # the reset itself succeeded (the wedge is cleared); a cleanup failure is best-effort —
            # log it but don't re-escalate the already-recovered repo.
            msg = f"recovered: reset_to_base (cleanup_worktrees: {cw.get('error', '?')})"
        else:
            removed = cw.get("removed") or []
            msg = (f"recovered: reset_to_base, cleanup_worktrees (removed {len(removed)} rsi/* branch(es))")
        # The wedge is cleared and the base is clean + origin-synced — bring the loop back UP. Otherwise
        # the lingering status=error/phase=reverted heartbeat permanently blocks should_restart() and the
        # repo sits idle until a human presses Start (an overnight revert-failure = zero progress). Same
        # safe gated restart the 'stuck' path uses: only after the loop was confirmed not live and the
        # supervisor lock was released; anti-thrash caps a recurring cause to escalation, not an infinite
        # loop (each cycle is reversible — no force-push, no merge).
        if allow_restart:
            control.start(repo, auto_push=auto_push)
            actions.append("restart")
            msg += ", restart"
        return _finish(repo, d, actions, escalate=False, msg=msg)

    if not d["auto_safe"] and cat != "gate_red_streak":
        return _finish(repo, d, [], escalate=True, msg="escalated — operator action required")

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
        # supervisor-authorized-recovery: HOLD the runner's single-flight lock so we never hard-reset
        # git UNDER a live iteration (that dirty tree may be the running loop's in-progress work). The
        # atomic lock acquire replaces the old racy is_running() snapshot — a runner writes its lock
        # late in main(), a window where is_running() is False but a reset would still race it.
        ok, token = control.acquire_supervisor_lock(repo)
        if not ok:
            return _finish(repo, d, actions, escalate=True,
                           msg="loop is live — stop it before Solomon resets the working tree")
        try:
            r = control.reset_to_base(repo)
        finally:
            control.release_supervisor_lock(repo, token)
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
        # Restart UNCONDITIONALLY: a stuck loop must be re-spawned even with auto_push OFF. control.start
        # ships local-only when auto_push is False (effective_ship -> "local"; never pushes/merges), so a
        # restart is not a push-bearing action and must not be gated by allow_restart — gating it left
        # local-ship lanes silently dead (stopped, no restart, no escalation) until a human pressed Start.
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
