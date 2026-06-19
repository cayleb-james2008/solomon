"""Tests for the RSI branch-hygiene + auto-condense-to-main overhaul (no network/gh/pi).

  HYG-1  _prune_stale_rsi_branches deletes leftover rsi/* but keeps the base
  HYG-2  _prune_stale_rsi_branches never deletes the current branch
  CI-1   _wait_for_ci_then_merge merges on green
  CI-2   _wait_for_ci_then_merge reverts (closes PR + deletes branch) on red; not a landed ship
  CI-3   _wait_for_ci_then_merge hands off to native auto-merge when CI stays pending past the cap
"""
import importlib.util
import os
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

RUNNER = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                      "improver", "run_improver.py")


def _load_runner():
    spec = importlib.util.spec_from_file_location("run_improver", RUNNER)
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


def _git(path, *a):
    return subprocess.run(["git", "-C", str(path), *a], capture_output=True, text=True)


def _mk_repo(tmp_path):
    work = tmp_path / "work"
    work.mkdir()
    subprocess.run(["git", "init", str(work)], capture_output=True)
    _git(work, "config", "user.email", "t@t")
    _git(work, "config", "user.name", "t")
    _git(work, "checkout", "-b", "main")
    (work / "f.txt").write_text("1")
    _git(work, "add", "-A")
    _git(work, "commit", "-m", "init")
    return work


class _R:
    def __init__(self, rc=0, out="", err=""):
        self.returncode = rc
        self.stdout = out
        self.stderr = err


def _stub_gh(m, monkeypatch):
    calls = []

    def fake_run(args, **kw):
        calls.append(list(args))
        return _R(0)
    monkeypatch.setattr(m.subprocess, "run", fake_run)
    monkeypatch.setattr(m, "gh_exe", lambda: "gh")
    monkeypatch.setattr(m, "log", lambda *a, **k: None)
    monkeypatch.setattr(m, "heartbeat", lambda **k: None)
    return calls


def test_prune_stale_rsi_branches_keeps_base(tmp_path):
    m = _load_runner()
    work = _mk_repo(tmp_path)
    m.REPO = work
    _git(work, "branch", "rsi/iter-OLD1")
    _git(work, "branch", "rsi/iter-OLD2")
    _git(work, "branch", "rsi/beautify-X")
    _git(work, "checkout", "main")
    pruned = m._prune_stale_rsi_branches()
    assert pruned == 3
    assert _git(work, "branch", "--list", "rsi/*").stdout.strip() == ""
    assert "main" in _git(work, "branch", "--show-current").stdout


def test_prune_never_deletes_current_branch(tmp_path):
    m = _load_runner()
    work = _mk_repo(tmp_path)
    m.REPO = work
    _git(work, "checkout", "-b", "rsi/iter-CUR")
    pruned = m._prune_stale_rsi_branches()
    assert pruned == 0
    assert "rsi/iter-CUR" in _git(work, "branch", "--show-current").stdout


def test_wait_for_ci_merges_on_green(monkeypatch):
    m = _load_runner()
    monkeypatch.setattr(m, "_pr_checks", lambda n: "success")
    calls = _stub_gh(m, monkeypatch)
    out = m._wait_for_ci_then_merge({"number": 7, "state": "open"})
    assert out["state"] == "merged"
    assert any("merge" in c and "--squash" in c for c in calls)


def test_wait_for_ci_reverts_on_red(monkeypatch):
    m = _load_runner()
    monkeypatch.setattr(m, "_pr_checks", lambda n: "failure")
    calls = _stub_gh(m, monkeypatch)
    out = m._wait_for_ci_then_merge({"number": 7, "state": "open"})
    assert out["state"] == "reverted (CI red)"
    assert any("close" in c for c in calls)
    assert m._ship_succeeded(out) is False  # a reverted PR did NOT land — item stays open


def test_wait_for_ci_queues_auto_when_pending_past_cap(monkeypatch):
    m = _load_runner()
    monkeypatch.setattr(m, "_pr_checks", lambda n: "pending")
    monkeypatch.setattr(m, "CI_WAIT_CEILING_S", 0)  # already past the cap on first check
    calls = _stub_gh(m, monkeypatch)
    out = m._wait_for_ci_then_merge({"number": 7, "state": "open"})
    assert "auto-merge queued" in out["state"]
    assert any("--auto" in c for c in calls)
