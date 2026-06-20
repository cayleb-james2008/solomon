"""Tests for dirty-repo self-recovery (review findings #2 + #3 + the sover leftover rsi/* branch).

1. RUNNER SELF-STOP on persistent dirty-BASE preflight bails (N=3 consecutive): instead of spinning
   forever on a dirty base tree (a dirty-tree+live-loop deadlock), write the STOP sentinel + record
   status=error/phase=preflight/reason=dirty_base_persistent so the watchdog leaves it alone + the
   operator is alerted.
2. solomon.recover() AUTO-RESET for revert_failed when the loop is NOT live: currently revert_failed
   only escalates. Now if diagnose()==revert_failed AND control.is_running(repo) is False, auto-run
   reset_to_base (holding the supervisor lock) + cleanup lingering rsi/* branches. Closes the
   revert-failure wedge (#2).
3. AUTO-CLEANUP of lingering rsi/* branches on a clean stop: extend control.cleanup_worktrees() to
   auto-run when a loop stops cleanly (status=stopped). Call from control.stop() after the stop
   sentinel is written + the loop confirms stopped. Removes the observed rsi/iter-... leftover on sover.
"""
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import time

import pytest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)
sys.path.insert(0, os.path.join(ROOT, "improver"))

RUNNER = os.path.join(ROOT, "improver", "run_improver.py")


def _load_runner():
    spec = importlib.util.spec_from_file_location("run_improver", RUNNER)
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


def _git(path, *a):
    return subprocess.run(["git", "-C", str(path), *a], capture_output=True, text=True)


def _mk_origin_clone(tmp_path):
    origin = tmp_path / "origin.git"
    work = tmp_path / "work"
    subprocess.run(["git", "init", "--bare", str(origin)], capture_output=True)
    subprocess.run(["git", "clone", str(origin), str(work)], capture_output=True)
    _git(work, "config", "user.email", "t@t")
    _git(work, "config", "user.name", "t")
    _git(work, "checkout", "-b", "main")
    (work / "f.txt").write_text("1")
    _git(work, "add", "-A")
    _git(work, "commit", "-m", "init")
    _git(work, "push", "-u", "origin", "main")
    return work


# ---- 1. runner self-stop on persistent dirty-base preflight bail ------------
def test_dirty_base_persistent_threshold_constant():
    m = _load_runner()
    assert hasattr(m, "_DIRTY_BASE_PERSISTENT_LIMIT")
    assert isinstance(m._DIRTY_BASE_PERSISTENT_LIMIT, int) and m._DIRTY_BASE_PERSISTENT_LIMIT >= 2


def test_dirty_base_persistent_counter_resets_on_non_dirty_iteration(monkeypatch):
    """The consecutive-dirty-base counter resets when a non-dirty iteration runs (so a transient
    dirty spell doesn't accumulate toward a false stop)."""
    m = _load_runner()
    # reset module-level state
    m._dirty_base_bail_count = 0
    # simulate a dirty-base bail: should increment
    m._note_dirty_base_bail(True, "main", "main")
    assert m._dirty_base_bail_count == 1
    m._note_dirty_base_bail(True, "main", "main")
    assert m._dirty_base_bail_count == 2
    # a clean iteration resets the counter
    m._note_dirty_base_bail(False, "main", "main")
    assert m._dirty_base_bail_count == 0


def test_dirty_base_persistent_writes_stop_and_returns_true(monkeypatch, tmp_path):
    """After N consecutive dirty-base bails, _note_dirty_base_bail writes the STOP sentinel + records
    status=error/phase=preflight/reason=dirty_base_persistent + returns True (the caller should not
    spin again). Below the threshold it returns False (keep looping)."""
    m = _load_runner()
    # point the runner's runtime at a temp dir
    monkeypatch.setattr(m, "RUNTIME", tmp_path)
    monkeypatch.setattr(m, "STOP", tmp_path / "stop")
    monkeypatch.setattr(m, "HEARTBEAT", tmp_path / "heartbeat.json")
    m._dirty_base_bail_count = 0
    limit = m._DIRTY_BASE_PERSISTENT_LIMIT
    # below the threshold: no stop, returns False
    for _ in range(limit - 1):
        assert m._note_dirty_base_bail(True, "main", "main") is False
    assert not (tmp_path / "stop").exists()
    # the Nth consecutive dirty-base bail: writes STOP + returns True
    assert m._note_dirty_base_bail(True, "main", "main") is True
    assert (tmp_path / "stop").exists()
    # heartbeat records the reason
    hb = json.loads((tmp_path / "heartbeat.json").read_text(encoding="utf-8"))
    assert hb.get("status") == "error"
    assert hb.get("phase") == "preflight"
    assert hb.get("reason") == "dirty_base_persistent"


def test_dirty_base_non_base_branch_does_not_count():
    """A dirty non-base branch (rsi/*) is a dead-run leftover cleared by the forced preflight reset,
    NOT a persistent dirty-base problem — it must NOT accumulate toward the self-stop."""
    m = _load_runner()
    m._dirty_base_bail_count = 0
    m._note_dirty_base_bail(True, "rsi/iter-x", "main")
    assert m._dirty_base_bail_count == 0


# ---- 1b. non-destructive auto-stash recovery of a dirty BASE tree ----------
@pytest.mark.skipif(not shutil.which("git"), reason="git not available")
def test_auto_stash_base_recovers_dirty_tree_non_destructively(tmp_path, monkeypatch):
    """A dirty BASE tree (tracked change + an untracked operator-looking file) is STASHED, not
    deleted: after _auto_stash_base the working tree is verifiably clean (so the loop self-resumes)
    AND the work is fully recoverable from the stash list — nothing is destroyed and nothing is
    pushed (the stash is local)."""
    m = _load_runner()
    work = _mk_origin_clone(tmp_path)
    (work / "f.txt").write_text("CHANGED")              # tracked dirty
    (work / "operator_note.txt").write_text("keep me")  # untracked operator-looking file
    monkeypatch.setattr(m, "REPO", str(work))           # git() runs with cwd=REPO
    monkeypatch.setattr(m, "RUNTIME", tmp_path / "rt")
    assert m.tree_dirty() is True
    assert m._untracked_non_ignored_files()             # operator_note.txt present
    assert m._auto_stash_base("rsi/iter-test") is True
    # the base tree is now clean -> the iteration can proceed
    assert m.tree_dirty() is False
    assert m._untracked_non_ignored_files() == []
    # the work is preserved in the stash (recoverable, non-destructive)
    stashes = _git(work, "stash", "list").stdout
    assert "solomon-auto-preflight" in stashes
    _git(work, "stash", "pop")
    assert (work / "operator_note.txt").read_text() == "keep me"
    assert (work / "f.txt").read_text() == "CHANGED"


@pytest.mark.skipif(not shutil.which("git"), reason="git not available")
def test_auto_stash_leaves_ignored_files_untouched(tmp_path, monkeypatch):
    """Auto-stash uses `--include-untracked` (NOT `--all`), so .gitignore'd files (e.g. private
    profiles/ggg/) are left in place — never stashed, never exposed."""
    m = _load_runner()
    work = _mk_origin_clone(tmp_path)
    (work / ".gitignore").write_text("secret/\n")
    _git(work, "add", ".gitignore")
    _git(work, "commit", "-m", "ignore secret")
    (work / "secret").mkdir()
    (work / "secret" / "creds.txt").write_text("PRIVATE")
    (work / "f.txt").write_text("CHANGED")              # something to stash
    monkeypatch.setattr(m, "REPO", str(work))
    monkeypatch.setattr(m, "RUNTIME", tmp_path / "rt")
    assert m._auto_stash_base("rsi/iter-test") is True
    # the ignored private file is still present (not stashed away)
    assert (work / "secret" / "creds.txt").read_text() == "PRIVATE"


# ---- 2. solomon.recover() auto-reset for revert_failed when loop not live ---
def _rt(tmp_path, monkeypatch, name="x"):
    import control
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


def test_recover_revert_failed_auto_resets_when_loop_not_live(tmp_path, monkeypatch):
    """When diagnose()==revert_failed AND the loop is NOT live, recover() auto-runs reset_to_base
    (holding the supervisor lock) + cleanup_worktrees, instead of only escalating. Closes #2."""
    import control
    import solomon
    rt = _rt(tmp_path, monkeypatch)
    _hb(rt, status="error", phase="reverted", last_summary="REVERT FAILED")
    monkeypatch.setattr(control, "is_running", lambda repo: False)          # loop NOT live
    monkeypatch.setattr(control, "acquire_supervisor_lock", lambda repo: (True, "sup-tok"))
    released = []
    monkeypatch.setattr(control, "release_supervisor_lock",
                        lambda repo, token: released.append(token))
    resets = []
    monkeypatch.setattr(control, "reset_to_base", lambda repo: resets.append(repo) or {"ok": True, "base": "main"})
    cleanups = []
    monkeypatch.setattr(control, "cleanup_worktrees", lambda repo: cleanups.append(repo) or {"ok": True})
    res = solomon.recover(_repo(tmp_path), allow_pi=False)
    assert not res["escalate"]
    assert "reset_to_base" in res["actions_taken"]
    assert "cleanup_worktrees" in res["actions_taken"]
    assert len(resets) == 1 and len(cleanups) == 1
    assert released == ["sup-tok"]          # lock released after the reset


def test_recover_revert_failed_still_escalates_when_loop_live(tmp_path, monkeypatch):
    """When the loop IS live, recover() must NOT hard-reset git under a running iteration — it still
    escalates (the existing behavior is preserved)."""
    import control
    import solomon
    rt = _rt(tmp_path, monkeypatch)
    _hb(rt, status="error", phase="reverted", last_summary="REVERT FAILED")
    monkeypatch.setattr(control, "is_running", lambda repo: True)           # loop IS live
    resets = []
    monkeypatch.setattr(control, "reset_to_base", lambda repo: resets.append(repo) or {"ok": True})
    res = solomon.recover(_repo(tmp_path), allow_pi=False)
    assert res["escalate"]
    assert resets == []                     # never hard-reset git under a live iteration
    assert (rt / "escalation.json").exists()


def test_recover_revert_failed_escalates_when_reset_fails(tmp_path, monkeypatch):
    """If the auto-reset itself fails (e.g. un-pushed commits, uncommitted WIP), recover() escalates
    with the error — it does NOT claim recovery succeeded."""
    import control
    import solomon
    rt = _rt(tmp_path, monkeypatch)
    _hb(rt, status="error", phase="reverted", last_summary="REVERT FAILED")
    monkeypatch.setattr(control, "is_running", lambda repo: False)
    monkeypatch.setattr(control, "acquire_supervisor_lock", lambda repo: (True, "sup-tok"))
    monkeypatch.setattr(control, "release_supervisor_lock", lambda repo, token: None)
    monkeypatch.setattr(control, "reset_to_base",
                         lambda repo: {"ok": False, "error": "un-pushed commits — escalate"})
    monkeypatch.setattr(control, "cleanup_worktrees", lambda repo: {"ok": True})
    res = solomon.recover(_repo(tmp_path), allow_pi=False)
    assert res["escalate"]
    assert "reset_to_base" in res["actions_taken"]      # it tried, then escalated on failure
    assert "un-pushed" in (res.get("message") or "") or (rt / "escalation.json").exists()


def test_recover_revert_failed_escalates_when_lock_unavailable(tmp_path, monkeypatch):
    """If the supervisor lock can't be acquired (a live runner holds it — defensive even though
    is_running is False), escalate instead of mutating git under a possibly-live iteration."""
    import control
    import solomon
    rt = _rt(tmp_path, monkeypatch)
    _hb(rt, status="error", phase="reverted", last_summary="REVERT FAILED")
    monkeypatch.setattr(control, "is_running", lambda repo: False)
    monkeypatch.setattr(control, "acquire_supervisor_lock", lambda repo: (False, None))  # live holder
    resets = []
    monkeypatch.setattr(control, "reset_to_base", lambda repo: resets.append(repo) or {"ok": True})
    res = solomon.recover(_repo(tmp_path), allow_pi=False)
    assert res["escalate"]
    assert resets == []


# ---- 3. cleanup_worktrees auto-run on a clean stop -------------------------
@pytest.mark.skipif(not shutil.which("git"), reason="git not available")
def test_stop_auto_cleans_rsi_branches(tmp_path, monkeypatch):
    """control.stop() writes the stop sentinel, waits briefly for the loop to exit, then auto-runs
    cleanup_worktrees to delete lingering rsi/* branches (the observed rsi/iter-... leftover on sover).
    The currently-checked-out branch is never deleted (cleanup_worktrees guards that)."""
    import control
    work = _mk_origin_clone(tmp_path)
    # leave a stray rsi/* branch behind
    _git(work, "checkout", "-b", "rsi/iter-stray")
    _git(work, "checkout", "main")             # back on main so the stray is deletable
    repo = {"name": "w", "path": str(work), "branch_prefix": "rsi/", "pr_target_branch": "main"}
    monkeypatch.setattr(control, "HERE", str(tmp_path))
    rt = tmp_path / "runtime" / "w"
    rt.mkdir(parents=True)
    # is_running False (loop not live) so stop() can proceed to cleanup
    monkeypatch.setattr(control, "is_running", lambda repo: False)
    res = control.stop(repo)
    assert res["ok"]
    # the stray rsi/* branch is gone (cleanup ran)
    branches = _git(work, "branch", "--list", "rsi/*").stdout
    assert "rsi/iter-stray" not in branches


def test_stop_does_not_force_delete_current_branch(tmp_path, monkeypatch):
    """cleanup_worktrees (called from stop) never deletes the branch we're standing on — even if
    it's an rsi/* branch (a loop that stopped mid-iteration on its rsi branch)."""
    import control
    work = _mk_origin_clone(tmp_path)
    _git(work, "checkout", "-b", "rsi/iter-current")          # standing on an rsi branch
    repo = {"name": "w", "path": str(work), "branch_prefix": "rsi/", "pr_target_branch": "main"}
    monkeypatch.setattr(control, "HERE", str(tmp_path))
    rt = tmp_path / "runtime" / "w"
    rt.mkdir(parents=True)
    monkeypatch.setattr(control, "is_running", lambda repo: False)
    control.stop(repo)
    # we're still on rsi/iter-current (it was NOT deleted)
    assert _git(work, "rev-parse", "--abbrev-ref", "HEAD").stdout.strip() == "rsi/iter-current"


def test_stop_still_works_when_cleanup_fails(tmp_path, monkeypatch):
    """stop() must still return ok=True (the sentinel was written) even if cleanup_worktrees fails —
    cleanup is best-effort, the stop itself must not be blocked by it."""
    import control
    work = _mk_origin_clone(tmp_path)
    repo = {"name": "w", "path": str(work), "branch_prefix": "rsi/", "pr_target_branch": "main"}
    monkeypatch.setattr(control, "HERE", str(tmp_path))
    rt = tmp_path / "runtime" / "w"
    rt.mkdir(parents=True)
    monkeypatch.setattr(control, "is_running", lambda repo: False)
    monkeypatch.setattr(control, "cleanup_worktrees", lambda repo: {"ok": False, "error": "boom"})
    res = control.stop(repo)
    assert res["ok"]                        # the stop sentinel still wrote; cleanup failure is logged
    assert (rt / "stop").exists()