"""Tests for the Solomon supervisor: diagnose categories, reversible primitives, the recovery
ladder, supervisor.jsonl + escalation, anti-thrash. No network / gh / real pi."""
import json
import os
import shutil
import subprocess
import sys

import pytest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)
sys.path.insert(0, os.path.join(ROOT, "improver"))

import control  # noqa: E402
import solomon  # noqa: E402


def _rt(tmp_path, monkeypatch, name="x"):
    monkeypatch.setattr(control, "HERE", str(tmp_path))
    rt = tmp_path / "runtime" / name
    rt.mkdir(parents=True)
    return rt


def _repo(tmp_path, name="x", **extra):
    r = {"name": name, "path": str(tmp_path)}
    r.update(extra)
    return r


def _hb(rt, **fields):
    (rt / "heartbeat.json").write_text(json.dumps(fields), encoding="utf-8")


def _hist(rt, records):
    (rt / "history.jsonl").write_text("\n".join(json.dumps(x) for x in records) + "\n", encoding="utf-8")


# ---- diagnose --------------------------------------------------------------
def test_diagnose_ok(tmp_path, monkeypatch):
    rt = _rt(tmp_path, monkeypatch)
    _hb(rt, status="sleeping", phase="sleep")
    d = solomon.diagnose(_repo(tmp_path))
    assert d["category"] == "ok" and d["healthy"]


def test_diagnose_stale_lock(tmp_path, monkeypatch):
    rt = _rt(tmp_path, monkeypatch)
    (rt / "lock").write_text("999999", encoding="utf-8")
    _hb(rt, status="sleeping")
    monkeypatch.setattr(control, "_pid_alive", lambda pid: False)
    d = solomon.diagnose(_repo(tmp_path))
    assert d["category"] == "stale_lock" and d["auto_safe"]


def test_diagnose_revert_failed(tmp_path, monkeypatch):
    rt = _rt(tmp_path, monkeypatch)
    _hb(rt, status="error", phase="reverted", last_summary="REVERT FAILED — needs manual cleanup")
    d = solomon.diagnose(_repo(tmp_path))
    assert d["category"] == "revert_failed" and not d["auto_safe"]


def test_diagnose_dirty_tree(tmp_path, monkeypatch):
    rt = _rt(tmp_path, monkeypatch)
    _hb(rt, status="error", phase="preflight", last_summary="Working tree is dirty — commit or stash")
    d = solomon.diagnose(_repo(tmp_path))
    assert d["category"] == "dirty_tree" and d["auto_safe"]


def test_diagnose_gate_red_streak(tmp_path, monkeypatch):
    rt = _rt(tmp_path, monkeypatch)
    _hb(rt, status="sleeping")
    _hist(rt, [{"status": "reverted"}, {"status": "reverted"}, {"status": "error"}])
    d = solomon.diagnose(_repo(tmp_path))
    assert d["category"] == "gate_red_streak" and not d["auto_safe"]


def test_diagnose_ci_red_streak(tmp_path, monkeypatch):
    # auto-merge repo whose last 3 PRs all recorded 'blocked' (CI-red / unmerged) -> escalate.
    rt = _rt(tmp_path, monkeypatch)
    _hb(rt, status="sleeping")
    _hist(rt, [{"status": "blocked", "pr": {"number": 7, "state": "open (CI red — not merged)"}} for _ in range(3)])
    d = solomon.diagnose(_repo(tmp_path, ship="auto-merge"))
    assert d["category"] == "ci_red_streak" and not d["auto_safe"]


def test_diagnose_ci_red_not_flagged_in_pr_mode(tmp_path, monkeypatch):
    # In pr mode an open PR is the operator's to merge — Solomon does NOT escalate a blocked streak.
    rt = _rt(tmp_path, monkeypatch)
    _hb(rt, status="sleeping")
    _hist(rt, [{"status": "blocked", "pr": {"number": 7, "state": "open (CI red — not merged)"}} for _ in range(3)])
    d = solomon.diagnose(_repo(tmp_path, ship="pr"))
    assert d["category"] == "ok"


def test_diagnose_no_key(tmp_path, monkeypatch):
    rt = _rt(tmp_path, monkeypatch)
    _hb(rt, status="error", last_summary="OLLAMA_API_KEY not set — add it to Solomon/.env")
    d = solomon.diagnose(_repo(tmp_path))
    assert d["category"] == "no_key" and not d["auto_safe"]


def test_solomon_fix_session_honors_auto_push(tmp_path, monkeypatch):
    # a fix-session must ship LOCAL-only when the global auto_push gate is off (no push/merge),
    # and the repo's configured ship mode when it's on.
    monkeypatch.setattr(control, "HERE", str(tmp_path))
    (tmp_path / "improver").mkdir()
    (tmp_path / "improver" / "run_improver.py").write_text("", encoding="utf-8")
    pyfake = tmp_path / "py.exe"
    pyfake.write_text("", encoding="utf-8")
    monkeypatch.setattr(control, "_venv_python", lambda repo: str(pyfake))
    captured = {}

    class _P:
        pid = 123

    monkeypatch.setattr(solomon.subprocess, "Popen", lambda args, **k: captured.update(args=args) or _P())
    repo = {"name": "x", "path": str(tmp_path), "ship": "auto-merge"}
    solomon.solomon_fix_session(repo, auto_push=False)
    a = captured["args"]
    assert a[a.index("--ship") + 1] == "local"        # gate off -> local, never push/merge
    solomon.solomon_fix_session(repo, auto_push=True)
    a = captured["args"]
    assert a[a.index("--ship") + 1] == "auto-merge"    # gate on -> the repo's configured ship mode


def test_diagnose_base_out_of_band(tmp_path, monkeypatch):
    # the never-hand-patched keystone refused to hard-reset an out-of-band base; must be surfaced
    # (escalate), not reported healthy while the loop spins the same refusal.
    rt = _rt(tmp_path, monkeypatch)
    _hb(rt, status="error", phase="preflight",
        last_summary="main has 2 commit(s) not on origin (out-of-band / un-pushed base change). "
                      "Refusing to hard-reset — push or revert them")
    d = solomon.diagnose(_repo(tmp_path))
    assert d["category"] == "base_out_of_band" and not d["auto_safe"]


def test_diagnose_stuck_covers_any_active_phase(tmp_path, monkeypatch):
    # a hang in test/commit/ship/pr/merge (not just implement) that goes stale while alive is stuck.
    rt = _rt(tmp_path, monkeypatch)
    monkeypatch.setattr(control, "is_running", lambda repo: True)
    (rt / "lock").write_text("1", encoding="utf-8")
    _hb(rt, status="iterating", phase="test", updated_at="2020-01-01T00:00:00Z")  # ancient -> stale
    d = solomon.diagnose(_repo(tmp_path))
    assert d["category"] == "stuck" and d["auto_safe"]


def test_anti_thrash_flips_to_escalate(tmp_path, monkeypatch):
    rt = _rt(tmp_path, monkeypatch)
    (rt / "lock").write_text("999999", encoding="utf-8")
    _hb(rt, status="sleeping")
    monkeypatch.setattr(control, "_pid_alive", lambda pid: False)
    (rt / "supervisor.jsonl").write_text(
        "\n".join(json.dumps({"category": "stale_lock", "rung": 0, "escalate": False}) for _ in range(3)) + "\n",
        encoding="utf-8")
    d = solomon.diagnose(_repo(tmp_path))
    assert d["category"] == "stale_lock" and not d["auto_safe"]   # repeated auto-fix -> escalate


# ---- reversible primitives -------------------------------------------------
def test_clear_lock_refuses_live(tmp_path, monkeypatch):
    rt = _rt(tmp_path, monkeypatch)
    (rt / "lock").write_text("4242", encoding="utf-8")
    monkeypatch.setattr(control, "_pid_alive", lambda pid: True)
    r = control.clear_lock(_repo(tmp_path))
    assert not r["ok"] and (rt / "lock").exists()


def test_clear_lock_removes_dead(tmp_path, monkeypatch):
    rt = _rt(tmp_path, monkeypatch)
    (rt / "lock").write_text("4242", encoding="utf-8")
    monkeypatch.setattr(control, "_pid_alive", lambda pid: False)
    r = control.clear_lock(_repo(tmp_path))
    assert r["ok"] and r["removed"] and not (rt / "lock").exists()


def _git(path, *a):
    return subprocess.run(["git", "-C", str(path), *a], capture_output=True, text=True)


@pytest.mark.skipif(not shutil.which("git"), reason="git not available")
def test_reset_to_base_guard_refuses_unpushed(tmp_path):
    origin = tmp_path / "origin.git"
    work = tmp_path / "work"
    subprocess.run(["git", "init", "--bare", str(origin)], capture_output=True)
    subprocess.run(["git", "clone", str(origin), str(work)], capture_output=True)
    _git(work, "config", "user.email", "t@t"); _git(work, "config", "user.name", "t")
    _git(work, "checkout", "-b", "main")
    (work / "f.txt").write_text("1"); _git(work, "add", "-A"); _git(work, "commit", "-m", "init")
    _git(work, "push", "-u", "origin", "main")
    (work / "f.txt").write_text("2"); _git(work, "commit", "-am", "local only")     # un-pushed
    r = control.reset_to_base({"name": "w", "path": str(work), "pr_target_branch": "main"})
    assert not r["ok"] and "un-pushed" in r["error"]


@pytest.mark.skipif(not shutil.which("git"), reason="git not available")
def test_reset_to_base_refuses_uncommitted_tracked(tmp_path):
    # reset_to_base must NOT discard uncommitted operator WIP — it refuses + escalates so the
    # operator commits/stashes first (the watchdog drives this unattended; eating WIP is unsafe).
    origin = tmp_path / "origin.git"
    work = tmp_path / "work"
    subprocess.run(["git", "init", "--bare", str(origin)], capture_output=True)
    subprocess.run(["git", "clone", str(origin), str(work)], capture_output=True)
    _git(work, "config", "user.email", "t@t"); _git(work, "config", "user.name", "t")
    _git(work, "checkout", "-b", "main")
    (work / "f.txt").write_text("1"); _git(work, "add", "-A"); _git(work, "commit", "-m", "init")
    _git(work, "push", "-u", "origin", "main")
    (work / "f.txt").write_text("operator WIP")                                      # uncommitted tracked edit
    r = control.reset_to_base({"name": "w", "path": str(work), "pr_target_branch": "main"})
    assert not r["ok"] and "uncommitted" in r["error"]
    assert (work / "f.txt").read_text() == "operator WIP"                            # WIP preserved


@pytest.mark.skipif(not shutil.which("git"), reason="git not available")
def test_reset_to_base_syncs_clean_tree(tmp_path):
    # a CLEAN tree behind origin is still resynced to origin truth (the legitimate use case).
    origin = tmp_path / "origin.git"
    work = tmp_path / "work"
    subprocess.run(["git", "init", "--bare", str(origin)], capture_output=True)
    subprocess.run(["git", "clone", str(origin), str(work)], capture_output=True)
    _git(work, "config", "user.email", "t@t"); _git(work, "config", "user.name", "t")
    _git(work, "checkout", "-b", "main")
    (work / "f.txt").write_text("1"); _git(work, "add", "-A"); _git(work, "commit", "-m", "init")
    _git(work, "push", "-u", "origin", "main")
    _git(work, "checkout", "-b", "rsi/stray")                                        # left on a stray branch, clean
    r = control.reset_to_base({"name": "w", "path": str(work), "pr_target_branch": "main"})
    assert r["ok"] and r["base"] == "main"
    assert _git(work, "rev-parse", "--abbrev-ref", "HEAD").stdout.strip() == "main"


def test_read_supervisor_log_tail(tmp_path, monkeypatch):
    rt = _rt(tmp_path, monkeypatch)
    (rt / "supervisor.jsonl").write_text("\n".join(json.dumps({"i": i}) for i in range(3)) + "\n", encoding="utf-8")
    recs = control.read_supervisor_log(_repo(tmp_path), limit=2)
    assert [r["i"] for r in recs] == [1, 2]


# ---- recovery ladder -------------------------------------------------------
def test_recover_rung0_stale_lock(tmp_path, monkeypatch):
    rt = _rt(tmp_path, monkeypatch)
    (rt / "lock").write_text("999999", encoding="utf-8")
    _hb(rt, status="sleeping")
    monkeypatch.setattr(control, "_pid_alive", lambda pid: False)
    res = solomon.recover(_repo(tmp_path), allow_pi=False)
    assert res["ok"] and not res["escalate"] and "clear_lock" in res["actions_taken"]
    assert (rt / "supervisor.jsonl").exists()


def test_recover_revert_failed_escalates_no_spawn(tmp_path, monkeypatch):
    rt = _rt(tmp_path, monkeypatch)
    _hb(rt, status="error", phase="reverted", last_summary="REVERT FAILED")
    spawned = []
    monkeypatch.setattr(solomon, "solomon_fix_session", lambda repo: spawned.append(repo) or {"ok": True})
    res = solomon.recover(_repo(tmp_path), allow_pi=True)
    assert res["escalate"] and not res["ok"]
    assert (rt / "escalation.json").exists()
    assert spawned == []                            # never spawns pi for a non-code-fix escalation


def test_recover_gate_red_streak_optin(tmp_path, monkeypatch):
    rt = _rt(tmp_path, monkeypatch)
    _hb(rt, status="sleeping")
    _hist(rt, [{"status": "reverted"}, {"status": "reverted"}, {"status": "reverted"}])
    calls = []
    monkeypatch.setattr(solomon, "solomon_fix_session",
                        lambda repo, auto_push=True: (calls.append(repo), {"ok": True, "pid": 1})[1])
    # without allow_pi -> escalate, no spawn
    res = solomon.recover(_repo(tmp_path), allow_pi=False)
    assert res["escalate"] and calls == []
    # with allow_pi + provider key present -> spawn the fix-session
    monkeypatch.setattr(control, "keys_status", lambda: {"ollama-cloud": True})
    res2 = solomon.recover(_repo(tmp_path, provider="ollama-cloud"), allow_pi=True)
    assert "solomon_fix_session" in res2["actions_taken"] and len(calls) == 1


def test_recover_dirty_tree_refuses_under_live_loop(tmp_path, monkeypatch):
    rt = _rt(tmp_path, monkeypatch)
    _hb(rt, status="error", phase="preflight", last_summary="Working tree is dirty — commit or stash")
    monkeypatch.setattr(control, "is_running", lambda repo: True)
    resets = []
    monkeypatch.setattr(control, "reset_to_base", lambda repo: resets.append(repo) or {"ok": True})
    res = solomon.recover(_repo(tmp_path), allow_pi=False)
    assert res["escalate"] and resets == []            # never hard-reset git under a live iteration


def test_recover_gate_red_streak_refuses_under_live_loop(tmp_path, monkeypatch):
    rt = _rt(tmp_path, monkeypatch)
    _hb(rt, status="sleeping")
    _hist(rt, [{"status": "reverted"}] * 3)
    monkeypatch.setattr(control, "is_running", lambda repo: True)
    monkeypatch.setattr(control, "keys_status", lambda: {"ollama-cloud": True})
    spawned = []
    monkeypatch.setattr(solomon, "solomon_fix_session", lambda repo: spawned.append(repo) or {"ok": True})
    res = solomon.recover(_repo(tmp_path, provider="ollama-cloud"), allow_pi=True)
    assert res["escalate"] and spawned == []           # never fix-session against a live lock


# ---- app bridges (safe: no live mutation) ----------------------------------
def test_app_bridges_unknown_repo_safe():
    import app
    api = app.Api()
    for m in ("ensure_contracts", "enrich_contract", "supervise", "read_supervisor_log",
              "read_escalation", "clear_escalation"):
        assert hasattr(api, m)
    assert api.supervise("definitely-not-a-repo")["ok"] is False    # empty target set, no recover
    assert api.read_supervisor_log("definitely-not-a-repo") == []
    assert api.read_escalation("definitely-not-a-repo") is None


def test_auto_ai_fix_only_on_unattended_sweep(monkeypatch):
    import app
    api = app.Api()
    api._state["auto_ai_fix"] = True
    fake = {"name": "z", "path": "C:/none", "provider": "ollama-cloud"}
    monkeypatch.setattr(app.control, "load_repos", lambda: [fake])
    seen = {}
    monkeypatch.setattr(solomon, "recover",
                        lambda repo, allow_pi=False, allow_restart=True, auto_push=True:
                        (seen.__setitem__("allow_pi", allow_pi),
                         {"ok": True, "category": "ok", "actions_taken": [], "escalate": False, "message": "x"})[1])
    api.supervise("z", allow_pi=False)                      # manual button: global must NOT auto-fire a pi fix
    assert seen["allow_pi"] is False
    api.supervise("z", allow_pi=False, unattended=True)     # unattended --supervise sweep: global applies
    assert seen["allow_pi"] is True
    api._state["auto_ai_fix"] = False                       # explicit tick always allows, regardless
    api.supervise("z", allow_pi=True)
    assert seen["allow_pi"] is True
