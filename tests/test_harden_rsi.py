"""Regression tests for the 2026-06-20 ultra-audit hardening pass (no network / gh / real pi).

Each test pins a previously-untested invariant the audit found broken. Grouped by the audit's
finding ids so a future reader can trace test -> finding.

  HARDEN-A (self-heal-1 / hygiene-1/2 / self-heal-2):
    heartbeat() must not let a non-status update (reflect's phase="reflect") clobber a terminal
    ERROR phase that solomon.diagnose() + monitor.should_restart() key on.
"""
import importlib.util
import os
import sys

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
    assert not m._item_demands_tests("Refactor the scheduler to use asyncio")
    assert not m._item_demands_tests("Fix the flaky retry in the fetcher")


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
