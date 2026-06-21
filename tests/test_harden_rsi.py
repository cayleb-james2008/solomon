"""Regression tests for the 2026-06-20 ultra-audit hardening pass (no network / gh / real pi).

Each test pins a previously-untested invariant the audit found broken. Grouped by the audit's
finding ids so a future reader can trace test -> finding.

  HARDEN-A (self-heal-1 / hygiene-1/2 / self-heal-2):
    heartbeat() must not let a non-status update (reflect's phase="reflect") clobber a terminal
    ERROR phase that solomon.diagnose() + monitor.should_restart() key on.
"""
import importlib.util
import os
import shutil
import subprocess
import sys

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


# --------------------------------------------------------------------------- #
# HARDEN-A — heartbeat() preserves a terminal error phase
# --------------------------------------------------------------------------- #
def test_heartbeat_preserves_error_phase_on_non_status_update(monkeypatch):
    """A preflight refusal parks status=error/phase=preflight; reflect() then calls
    heartbeat(phase="reflect"). The phase MUST survive so diagnose() can classify the wedge."""
    m = _load_runner()
    monkeypatch.setattr(m, "_runtime_atomic_write", lambda *a, **k: None)
    monkeypatch.setattr(m, "_hb", {"status": "error", "phase": "preflight",
                                   "last_summary": "Untracked non-ignored files ... would be deleted"})
    m.heartbeat(phase="reflect")
    assert m._hb["status"] == "error"
    assert m._hb["phase"] == "preflight"        # NOT clobbered to "reflect"


def test_heartbeat_preserves_reverted_phase(monkeypatch):
    """The revert-failure HALT parks status=error/phase=reverted; the watchdog refuses to restart
    only while phase=='reverted'. A stray phase update must not flip it to 'reflect'."""
    m = _load_runner()
    monkeypatch.setattr(m, "_runtime_atomic_write", lambda *a, **k: None)
    monkeypatch.setattr(m, "_hb", {"status": "error", "phase": "reverted"})
    m.heartbeat(phase="reflect")
    assert m._hb["phase"] == "reverted"


def test_heartbeat_allows_phase_change_when_status_set(monkeypatch):
    """A fresh iteration explicitly passes status -> the freeze releases and phase advances."""
    m = _load_runner()
    monkeypatch.setattr(m, "_runtime_atomic_write", lambda *a, **k: None)
    monkeypatch.setattr(m, "_hb", {"status": "error", "phase": "reverted"})
    m.heartbeat(status="iterating", phase="implement")
    assert m._hb["status"] == "iterating"
    assert m._hb["phase"] == "implement"


def test_heartbeat_normal_phase_update_unaffected(monkeypatch):
    """A non-error iteration updates phase freely (the guard only fires on status=='error')."""
    m = _load_runner()
    monkeypatch.setattr(m, "_runtime_atomic_write", lambda *a, **k: None)
    monkeypatch.setattr(m, "_hb", {"status": "iterating", "phase": "implement"})
    m.heartbeat(phase="gate")
    assert m._hb["phase"] == "gate"


# --------------------------------------------------------------------------- #
# HARDEN-B (gate-1) — the after-gate is never narrowed below the baseline scope
# --------------------------------------------------------------------------- #
def test_correlated_expansion_never_narrows_default_gate(tmp_path, monkeypatch):
    """gate-1: the default full-suite gate must not be narrowed to the correlated subset. With a
    full-suite baseline, a narrowed after-gate makes anti-gaming's pass/collected check fire every
    iteration (-> infinite revert/defer) and blinds the gate to non-correlated regressions. The
    expansion must be a no-op so after-gate scope == baseline scope."""
    m = _load_runner()
    test_dir = tmp_path / "tests"
    test_dir.mkdir()
    (test_dir / "test_config.py").write_text("import config\ndef test_x():\n    pass\n")
    monkeypatch.setattr(m, "REPO", tmp_path)
    # even though test_config.py correlates with the change, the default gate is returned UNCHANGED
    out = m._expand_gate_with_correlated("", ["scripts/config.py"])
    assert out == ""                              # full suite, not "pytest test_config.py"
    # a custom gate is likewise run verbatim (operator-owned), so baseline==after for it too
    assert m._expand_gate_with_correlated("mygate --x", ["scripts/config.py"]) == "mygate --x"


# --------------------------------------------------------------------------- #
# HARDEN-D (gate-2) — count-less custom gates still catch test deletion
# --------------------------------------------------------------------------- #
def test_removed_test_def_caught_when_counts_inactive():
    m = _load_runner()
    diff = "-def test_old_behaviour():\n-    assert thing()\n+CONST = 1\n"
    # gate emitted no parseable counts -> numeric rails inert -> the diff-based deletion rail fires
    reason = m._anti_gaming_reason({"passed": 0, "collected": 0},
                                   {"passed": 0, "collected": 0, "green": True}, diff)
    assert reason is not None and "removed" in reason and "test definition" in reason


def test_removed_test_def_not_double_flagged_when_counts_active():
    m = _load_runner()
    diff = "-def test_old_behaviour():\n-    assert thing()\n+CONST = 1\n"
    # with parseable, steady counts the numeric rails own deletion detection; the diff rail stays off
    reason = m._anti_gaming_reason({"passed": 5, "collected": 5},
                                   {"passed": 5, "collected": 5, "green": True}, diff)
    assert reason is None


def test_removed_test_defs_helper():
    m = _load_runner()
    assert m._removed_test_defs("-def test_x():\n-class TestY:\n-    pass") != []
    assert m._removed_test_defs("---  a/x.py\n-x = 1\n+def test_x():") == []   # header + add excluded


# --------------------------------------------------------------------------- #
# HARDEN-E (gate-3) — an "add tests" item that added no test is not ticked
# --------------------------------------------------------------------------- #
def test_item_demands_tests_detection():
    m = _load_runner()
    assert m._item_demands_tests("Add unit tests for `asmodeus.cli` covering version/kill (0% coverage)")
    assert m._item_demands_tests("Increase coverage of env.load_env to 80%")
    # broader real-world phrasings (verification finding): raise-coverage + restore/backfill/port tests
    assert m._item_demands_tests("Improve test coverage for the parser")
    assert m._item_demands_tests("Bump coverage of asmodeus.cli to 80%")
    assert m._item_demands_tests("Raise coverage to 80%")
    assert m._item_demands_tests("Backfill tests for env.load_env")
    assert m._item_demands_tests("Restore the deleted unit tests")
    assert m._item_demands_tests("Port tests from the legacy suite")
    # negatives — non-test work, and crucially test-INFRA work that legitimately adds no test
    assert not m._item_demands_tests("Refactor the scheduler to use asyncio")
    assert not m._item_demands_tests("Fix the flaky retry in the fetcher")
    assert not m._item_demands_tests("Improve the test runner's parallelism")
    assert not m._item_demands_tests("Speed up the test harness startup")


def test_added_test_defs_detection():
    m = _load_runner()
    assert m._added_test_defs("+def test_foo():\n+    assert 1") != []
    assert m._added_test_defs("+async def test_bar():\n+    pass") != []
    assert m._added_test_defs("+class TestBaz:\n+    pass") != []
    assert m._added_test_defs("+def helper():\n+    return 1") == []     # not a test def
    assert m._added_test_defs("+++ b/tests/test_x.py\n+x=1") == []       # header excluded


# --------------------------------------------------------------------------- #
# HARDEN-F (onboarding-4) — all-alpha credential values are still redacted
# --------------------------------------------------------------------------- #
def test_redact_scrubs_alpha_credential_values(monkeypatch):
    m = _load_runner()
    monkeypatch.setattr(m, "NAME", "demo")           # no deny_terms in play
    # a credential-NAMED value with NO digit used to slip through -> must now be redacted
    assert "[REDACTED]" in m._redact("password: SecretAlphaValueHere")
    assert "[REDACTED]" in m._redact("github_token=AbcDefGhiJklMnoPq")
    assert "[REDACTED]" in m._redact("client_secret: OnlyLettersNoDigitsHere")
    # but a BARE lowercase 'token:'/'secret:' prose word with an alpha value stays untouched
    assert m._redact("token: validation logic") == "token: validation logic"
    assert m._redact("secret: be consistent daily") == "secret: be consistent daily"


# --------------------------------------------------------------------------- #
# HARDEN-G (hygiene-4) — auto-merge records 'shipped' only on a confirmed merge
# --------------------------------------------------------------------------- #
def test_ship_outcome_auto_merge_only_shipped_on_confirmed_merge():
    m = _load_runner()
    assert m._ship_outcome({"number": 1, "state": "merged"}, "auto-merge") == "shipped"
    # a native-auto-merge HANDOFF has NOT landed -> 'blocked' so a never-merging streak is diagnosable
    assert m._ship_outcome({"number": 1, "state": "auto-merge queued (awaiting CI)"}, "auto-merge") == "blocked"
    assert m._ship_outcome({"number": 1, "state": "open (CI red — not merged)"}, "auto-merge") == "blocked"
    assert m._ship_outcome({"number": None, "state": "reverted (CI red)"}, "auto-merge") == "blocked"
    # pr-mode: an opened PR IS a successful ship (the human merges it later)
    assert m._ship_outcome({"number": 7, "state": "open"}, "pr") == "shipped"


# --------------------------------------------------------------------------- #
# HARDEN-H — _git_add_all skips ignored paths without erroring (the sover wedge)
# --------------------------------------------------------------------------- #
@pytest.mark.skipif(not shutil.which("git"), reason="git not available")
def test_git_add_all_skips_ignored_without_error(tmp_path, monkeypatch):
    """A public repo with private_paths AND other gitignored dirs (sover: .runtime/data/profiles/ggg)
    used to fail `git add -A -- . :(exclude)priv` with 'paths are ignored ... Use -f', erroring the
    whole iteration. Bare `git add -A` must skip ignored files silently, stage the normal change, and
    leave private paths unstaged."""
    m = _load_runner()

    def g(*a):
        return subprocess.run(["git", "-C", str(tmp_path), *a], capture_output=True, text=True)

    subprocess.run(["git", "init", str(tmp_path)], capture_output=True)
    g("config", "user.email", "t@t"); g("config", "user.name", "t")
    (tmp_path / ".gitignore").write_text("ignored_dir/\n", encoding="utf-8")
    (tmp_path / "ignored_dir").mkdir(); (tmp_path / "ignored_dir" / "s.txt").write_text("x", encoding="utf-8")
    (tmp_path / "normal.py").write_text("y = 1\n", encoding="utf-8")
    (tmp_path / "priv").mkdir(); (tmp_path / "priv" / "brand.json").write_text("{}", encoding="utf-8")
    monkeypatch.setattr(m, "REPO", str(tmp_path))
    monkeypatch.setattr(m, "NAME", "x")
    monkeypatch.setattr(m, "_repo_is_public", lambda name: True)
    monkeypatch.setattr(m, "_repo_private_paths", lambda name: ["priv/"])

    r = m._git_add_all()
    assert r.returncode == 0                                   # no 'paths are ignored' failure
    staged = g("diff", "--cached", "--name-only").stdout
    assert "normal.py" in staged                              # the real change is staged
    assert "priv/brand.json" not in staged                   # private path kept out of the commit
    assert "ignored_dir" not in staged                       # ignored files skipped silently


# --------------------------------------------------------------------------- #
# HARDEN-I — a crashed loop is restarted (not mistaken for a clean stop)
# --------------------------------------------------------------------------- #
def test_watchdog_restarts_a_crashed_loop():
    """Bug B: an unhandled-exception crash now records status=error/phase='crashed' (not 'stopped'),
    so monitor.should_restart RESTARTS it instead of mistaking the crash for a deliberate operator
    stop and leaving the loop dead. A real clean stop and the revert HALT are still left alone."""
    spec = importlib.util.spec_from_file_location("monitor", os.path.join(ROOT, "monitor.py"))
    mon = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mon)
    crashed = {"status": "error", "phase": "crashed"}
    assert mon.should_restart(running=False, hb=crashed, paused=False, stop_pending=False) is True
    assert mon.should_restart(running=False, hb={"status": "stopped"}, paused=False, stop_pending=False) is False
    halt = {"status": "error", "phase": "reverted"}
    assert mon.should_restart(running=False, hb=halt, paused=False, stop_pending=False) is False
    # a crashed loop the operator paused stays paused (no surprise restart)
    assert mon.should_restart(running=False, hb=crashed, paused=True, stop_pending=False) is False


# --------------------------------------------------------------------------- #
# HARDEN-J — the empty-goal guard skips a dead iteration instead of fabricating work
# --------------------------------------------------------------------------- #
def test_needs_goal_skip_fires_on_empty_goal_plus_placeholder(monkeypatch):
    """No north-star GOAL + the generic placeholder item -> skip (the asmodeus no-objective wedge)."""
    m = _load_runner()
    monkeypatch.setattr(m, "GOAL", "   ")          # whitespace-only counts as empty
    assert m._needs_goal_skip("model-chosen improvement") is True


def test_needs_goal_skip_fires_on_empty_goal_plus_deferred(monkeypatch):
    """No GOAL + an already-deferred item (every real item exhausted) -> skip, don't loop on it."""
    m = _load_runner()
    monkeypatch.setattr(m, "GOAL", "")
    assert m._needs_goal_skip("rewrite the parser  (deferred: agent could not implement)") is True


def test_needs_goal_skip_not_fired_when_real_backlog_item(monkeypatch):
    """A repo with a REAL backlog item but no GOAL still runs (the conjunction is required)."""
    m = _load_runner()
    monkeypatch.setattr(m, "GOAL", "")
    assert m._needs_goal_skip("add a /metrics endpoint") is False


def test_needs_goal_skip_not_fired_when_goal_set(monkeypatch):
    """A real north-star GOAL is enough — even the placeholder item runs (the goal steers it)."""
    m = _load_runner()
    monkeypatch.setattr(m, "GOAL", "make the API 2x faster")
    assert m._needs_goal_skip("model-chosen improvement") is False
