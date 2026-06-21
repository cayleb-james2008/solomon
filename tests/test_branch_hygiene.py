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


def test_wait_for_ci_stop_wins_over_green(tmp_path, monkeypatch):
    # halt-switch: a live operator STOP must leave a green PR OPEN, never auto-merge it to the
    # integration branch (SOLOMON_RSI: a mid-iteration stop does not ship). The STOP check must win
    # even when CI is already green.
    m = _load_runner()
    stop = tmp_path / "stop"; stop.write_text("", encoding="utf-8")
    monkeypatch.setattr(m, "STOP", stop)
    monkeypatch.setattr(m, "_pr_checks", lambda n: "success")   # CI is green this poll
    calls = _stub_gh(m, monkeypatch)
    out = m._wait_for_ci_then_merge({"number": 7, "state": "open"})
    assert out["state"] == "open (stopped before merge)"        # NOT merged
    assert not any("merge" in c and "--squash" in c for c in calls)   # no squash-merge was issued


def test_wait_for_ci_red_reverts_even_when_stopped(tmp_path, monkeypatch):
    # CI RED must auto-revert (close PR + delete branch) even with an operator STOP pending — a known-red
    # PR must never linger. The STOP halt only protects a green/pending PR from shipping, not from cleanup.
    m = _load_runner()
    stop = tmp_path / "stop"; stop.write_text("", encoding="utf-8")
    monkeypatch.setattr(m, "STOP", stop)
    monkeypatch.setattr(m, "_pr_checks", lambda n: "failure")   # CI is RED this poll
    calls = _stub_gh(m, monkeypatch)
    out = m._wait_for_ci_then_merge({"number": 7, "state": "open"})
    assert out["state"] == "reverted (CI red)"                  # reverted, NOT left stranded by the stop
    assert any("close" in c for c in calls)                     # the red PR was closed (auto-revert)


def test_wait_for_ci_transient_none_reconfirmed_before_merge(tmp_path, monkeypatch):
    # a transient gh None (a blip) must be RE-CONFIRMED via _await_pr_checks before merging — a single
    # None must not immediately squash-merge an unverified PR. Here the re-poll resolves to red -> revert.
    m = _load_runner()
    monkeypatch.setattr(m, "STOP", tmp_path / "nostop")         # no stop pending
    monkeypatch.setattr(m, "_pr_checks", lambda n: None)        # first poll: transient None
    monkeypatch.setattr(m, "_await_pr_checks", lambda n: "failure")  # re-confirm: actually red
    calls = _stub_gh(m, monkeypatch)
    out = m._wait_for_ci_then_merge({"number": 7, "state": "open"})
    assert out["state"] == "reverted (CI red)"                  # disambiguated -> NOT merged on the None
    assert not any("--squash" in c for c in calls)


def test_wait_for_ci_no_ci_configured_still_merges(tmp_path, monkeypatch):
    # genuine 'no CI configured' (None stays None after re-confirm) still squash-merges — no regression.
    m = _load_runner()
    monkeypatch.setattr(m, "STOP", tmp_path / "nostop")
    monkeypatch.setattr(m, "_pr_checks", lambda n: None)
    monkeypatch.setattr(m, "_await_pr_checks", lambda n: None)
    calls = _stub_gh(m, monkeypatch)                            # fake gh returns rc 0 -> merge succeeds
    out = m._wait_for_ci_then_merge({"number": 7, "state": "open"})
    assert out["state"] == "merged"
    assert any("merge" in c and "--squash" in c for c in calls)


# --------------------------------------------------------------------------- #
# control.branch_hygiene + control.clean_branch — the dashboard "Clean branch" tool
# (auto-detect a dirty managed repo left off its base branch / with stray rsi/* branches,
#  then clean it back to base). Real git temp repos, no network/gh/pi.
# --------------------------------------------------------------------------- #
import shutil  # noqa: E402

import pytest  # noqa: E402

requires_git = pytest.mark.skipif(not shutil.which("git"), reason="git not available")


def _control_repo(tmp_path, monkeypatch, work, name="w"):
    """Wire control's HERE at tmp_path (so runtime/<name> is isolated) and return the repo dict."""
    import control
    monkeypatch.setattr(control, "HERE", str(tmp_path))
    rt = tmp_path / "runtime" / name
    rt.mkdir(parents=True, exist_ok=True)
    return {"name": name, "path": str(work), "branch_prefix": "rsi/", "pr_target_branch": "main"}


@requires_git
def test_hygiene_clean_repo_on_base_not_dirty(tmp_path, monkeypatch):
    """(a) clean repo on base, no rsi/* branches → dirty False."""
    import control
    work = _mk_repo(tmp_path)
    repo = _control_repo(tmp_path, monkeypatch, work)
    monkeypatch.setattr(control, "is_running", lambda r: False)
    h = control.branch_hygiene(repo)
    assert h["dirty"] is False
    assert h["off_base"] is False
    assert h["stray"] == []
    assert h["current"] == "main"
    assert h["base"] == "main"


@requires_git
def test_hygiene_on_rsi_branch_is_dirty(tmp_path, monkeypatch):
    """(b) checked out on an rsi/* branch, no runner → dirty True & off_base True."""
    import control
    work = _mk_repo(tmp_path)
    _git(work, "checkout", "-b", "rsi/iter-cur")
    repo = _control_repo(tmp_path, monkeypatch, work)
    monkeypatch.setattr(control, "is_running", lambda r: False)
    h = control.branch_hygiene(repo)
    assert h["dirty"] is True
    assert h["off_base"] is True
    assert h["current"] == "rsi/iter-cur"
    assert "rsi/iter-cur" in h["reason"]


@requires_git
def test_hygiene_stray_rsi_branch_is_dirty(tmp_path, monkeypatch):
    """(c) on base with a stray rsi/* branch → dirty True & stray non-empty."""
    import control
    work = _mk_repo(tmp_path)
    _git(work, "branch", "rsi/iter-stray")     # create but stay on main
    repo = _control_repo(tmp_path, monkeypatch, work)
    monkeypatch.setattr(control, "is_running", lambda r: False)
    h = control.branch_hygiene(repo)
    assert h["dirty"] is True
    assert h["off_base"] is False              # we're on base, just have a stray branch
    assert "rsi/iter-stray" in h["stray"]
    assert "stray" in h["reason"]


@requires_git
def test_clean_branch_returns_to_base_and_removes_rsi(tmp_path, monkeypatch):
    """(d) clean_branch on a dirty repo returns to base + removes the rsi branch(es) + ok True;
    a follow-up branch_hygiene → dirty False."""
    import control
    work = _mk_repo(tmp_path)
    _git(work, "checkout", "-b", "rsi/iter-cur")     # off base, on an rsi branch
    _git(work, "branch", "rsi/iter-stray")           # plus a stray rsi branch
    repo = _control_repo(tmp_path, monkeypatch, work)
    monkeypatch.setattr(control, "is_running", lambda r: False)

    res = control.clean_branch(repo)
    assert res["ok"] is True
    assert res["from"] == "rsi/iter-cur"
    assert res["checked_out"] == "main"
    # both rsi/* branches removed (the one we left + the stray); back on main first so both are deletable
    assert sorted(res["removed"]) == ["rsi/iter-cur", "rsi/iter-stray"]
    assert _git(work, "rev-parse", "--abbrev-ref", "HEAD").stdout.strip() == "main"
    assert _git(work, "branch", "--list", "rsi/*").stdout.strip() == ""

    h = control.branch_hygiene(repo)
    assert h["dirty"] is False


@requires_git
def test_clean_branch_preserves_base_wip_when_only_stray(tmp_path, monkeypatch):
    """Cleaning a STRAY rsi/* branch while ALREADY on base must NOT discard unrelated uncommitted
    work on base (regression: the unconditional `checkout --force` was a data-loss footgun, found by
    dogfooding the cleaner). switched must be False and the base WIP must survive."""
    import control
    work = _mk_repo(tmp_path)                       # on main
    _git(work, "branch", "rsi/iter-stray")          # stray rsi branch; repo stays on main
    (work / "f.txt").write_text("LOCAL EDIT")       # uncommitted TRACKED change on base
    repo = _control_repo(tmp_path, monkeypatch, work)
    monkeypatch.setattr(control, "is_running", lambda r: False)

    res = control.clean_branch(repo)
    assert res["ok"] is True
    assert res["switched"] is False                 # never force-checked-out base
    assert res["removed"] == ["rsi/iter-stray"]     # stray pruned
    assert (work / "f.txt").read_text() == "LOCAL EDIT"   # base WIP preserved
    assert _git(work, "branch", "--list", "rsi/*").stdout.strip() == ""


@requires_git
def test_cleanup_worktrees_keeps_branch_under_detached_head(tmp_path, monkeypatch):
    # detached HEAD built on an rsi/* branch's commit: cleanup must NOT force-delete that branch
    # (abbrev-ref returns the literal "HEAD" so the name guard misses it), but still prune a stale rsi/*.
    import control
    work = _mk_repo(tmp_path)                                # on main
    _git(work, "checkout", "-b", "rsi/iter-keep")
    (work / "g.txt").write_text("wip"); _git(work, "add", "-A"); _git(work, "commit", "-m", "wip")
    keep_sha = _git(work, "rev-parse", "HEAD").stdout.strip()
    _git(work, "branch", "rsi/iter-stale", "main")          # stale rsi at the (different) base commit
    _git(work, "checkout", "--detach", keep_sha)            # detached HEAD on the iter-keep commit
    repo = _control_repo(tmp_path, monkeypatch, work)
    res = control.cleanup_worktrees(repo)
    assert res["ok"] is True
    branches = _git(work, "branch", "--list", "rsi/*").stdout
    assert "rsi/iter-keep" in branches                      # preserved (detached HEAD built on it)
    assert "rsi/iter-stale" not in branches                 # genuinely stale rsi pruned


@requires_git
def test_clean_branch_refuses_when_running(tmp_path, monkeypatch):
    """clean_branch refuses while a loop is live — it must not yank git out from under a runner."""
    import control
    work = _mk_repo(tmp_path)
    _git(work, "checkout", "-b", "rsi/iter-cur")
    repo = _control_repo(tmp_path, monkeypatch, work)
    monkeypatch.setattr(control, "is_running", lambda r: True)
    res = control.clean_branch(repo)
    assert res["ok"] is False
    assert "running" in res["error"]
    # still on the rsi branch — nothing was changed
    assert _git(work, "rev-parse", "--abbrev-ref", "HEAD").stdout.strip() == "rsi/iter-cur"


@requires_git
def test_hygiene_running_loop_on_rsi_branch_not_dirty(tmp_path, monkeypatch):
    """(e) a runner running on an rsi branch is NORMAL mid-iteration → dirty False
    (monkeypatch control.is_running True)."""
    import control
    work = _mk_repo(tmp_path)
    _git(work, "checkout", "-b", "rsi/iter-cur")
    repo = _control_repo(tmp_path, monkeypatch, work)
    monkeypatch.setattr(control, "is_running", lambda r: True)
    h = control.branch_hygiene(repo)
    assert h["dirty"] is False
    assert h["off_base"] is True               # it IS off base, but a live loop makes that normal
    assert h["running"] is True


def test_hygiene_non_git_repo_returns_clean_shape(tmp_path, monkeypatch):
    """Never raises: a non-git / pathless repo returns the not-dirty shape."""
    import control
    monkeypatch.setattr(control, "HERE", str(tmp_path))
    assert control.branch_hygiene({"name": "nope", "path": ""})["dirty"] is False
    # a path that exists but isn't a git repo
    plain = tmp_path / "plain"
    plain.mkdir()
    h = control.branch_hygiene({"name": "plain", "path": str(plain)})
    assert h["dirty"] is False
