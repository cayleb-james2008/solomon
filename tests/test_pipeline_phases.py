"""Tests for the RSI role-agent pipeline phases (PLAN + adversarial REVIEW/JUDGE), no pi/network.

  RV-1  review REJECT -> branch reverted (_drop_branch), returns 'reject'
  RV-2  review APPROVE -> no revert, returns 'approve'
  RV-3  unparseable verdict -> 'skip' (fail-open: the objective gate already passed)
  PL-1  plan phase returns the planner's advisory text
"""
import importlib.util
import os
import sys

import pytest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

RUNNER = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                      "improver", "run_improver.py")


def _load_runner():
    spec = importlib.util.spec_from_file_location("run_improver", RUNNER)
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


class _P:
    def __init__(self, out):
        self.stdout = out
        self.returncode = 0
        self.stderr = ""


def _common(m, monkeypatch, out):
    monkeypatch.setattr(m, "_phase_run_pi", lambda *a, **k: _P(out))
    monkeypatch.setattr(m, "final_text", lambda s: s)
    monkeypatch.setattr(m, "git", lambda *a: _P(""))
    monkeypatch.setattr(m, "log", lambda *a, **k: None)
    m.BASE_BRANCH = "main"


def test_review_reject_reverts(monkeypatch):
    m = _load_runner()
    _common(m, monkeypatch, "analysis...\nREVIEW: reject - tests were weakened")
    dropped = {}
    monkeypatch.setattr(m, "_drop_branch", lambda b, phase, summary, **k: dropped.update(branch=b, phase=phase))
    assert m._run_review_phase("rsi/iter-1", "goal", "summary") == "reject"
    assert dropped.get("phase") == "reverted"
    assert dropped.get("branch") == "rsi/iter-1"


def test_review_approve_ships(monkeypatch):
    m = _load_runner()
    _common(m, monkeypatch, "looks correct + in scope\nREVIEW: approve - solid, minimal change")
    monkeypatch.setattr(m, "_drop_branch", lambda *a, **k: pytest.fail("approve must not revert"))
    assert m._run_review_phase("rsi/iter-1", "goal", "summary") == "approve"


def test_review_unparseable_fails_open(monkeypatch):
    m = _load_runner()
    _common(m, monkeypatch, "I could not decide.")
    monkeypatch.setattr(m, "_drop_branch", lambda *a, **k: pytest.fail("skip must not revert"))
    assert m._run_review_phase("rsi/iter-1", "g", "s") == "skip"


def test_plan_phase_returns_text(monkeypatch):
    m = _load_runner()
    monkeypatch.setattr(m, "_phase_run_pi", lambda *a, **k: _P("1. edit foo.py\n2. add a test\n3. run pytest"))
    monkeypatch.setattr(m, "final_text", lambda s: s)
    monkeypatch.setattr(m, "log", lambda *a, **k: None)
    plan = m._run_plan_phase("implement the thing")
    assert "edit foo.py" in plan and "run pytest" in plan
