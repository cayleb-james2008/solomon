"""Tests for the per-repo EVAL_CMD eval needle (Video 1: 'metrics can be misleading, amount of code
merged = bloat/slop' — need richer needles). A repo may declare `EVAL_CMD` in repos.json: a
benchmark/visual/product-metric command run AFTER the test gate (if present), parsed by the RUNNER
(never the model). Anti-gaming: the eval score (a single float parsed from stdout) must not drop vs
the baseline. If it drops, revert the branch. If EVAL_CMD is absent, behavior is unchanged.

This is the spec's 'evals are everything' multiplier — richer per-repo needles beyond unit tests.
"""
import importlib.util
import json
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


# ---- float parsing from a command's stdout --------------------------------
def test_parse_eval_score_float():
    m = _load_runner()
    assert m._parse_eval_score("0.847") == 0.847
    assert m._parse_eval_score("score: 12.5") == 12.5
    assert m._parse_eval_score("coverage 92%\nother noise\n") == 92.0
    assert m._parse_eval_score(" Accuracy=0.91 \n") == 0.91
    assert m._parse_eval_score("latency_ms: 145.3 | p99=210") == 145.3   # first float wins


def test_parse_eval_score_none_when_unparseable():
    m = _load_runner()
    assert m._parse_eval_score("no numbers here") is None
    assert m._parse_eval_score("") is None
    assert m._parse_eval_score(None) is None


def test_parse_eval_score_negative_and_scientific():
    m = _load_runner()
    assert m._parse_eval_score("-3.5e2") == -350.0
    assert m._parse_eval_score("1.2e-3") == 1.2e-3


# ---- EVAL_CMD resolution from repos.json ----------------------------------
def test_eval_cmd_missing_returns_empty(tmp_path, monkeypatch):
    m = _load_runner()
    repos_json = tmp_path / "repos.json"
    repos_json.write_text(json.dumps([{"name": "a", "path": str(tmp_path / "a")}]), encoding="utf-8")
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    assert m._eval_cmd("a") == ""


def test_eval_cmd_resolved_from_repos_json(tmp_path, monkeypatch):
    m = _load_runner()
    repos_json = tmp_path / "repos.json"
    repos_json.write_text(json.dumps([
        {"name": "a", "path": str(tmp_path / "a"), "EVAL_CMD": ".venv\\Scripts\\python bench.py"},
    ]), encoding="utf-8")
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    assert m._eval_cmd("a") == ".venv\\Scripts\\python bench.py"


# ---- eval gate decision ---------------------------------------------------
def test_eval_gate_ok_when_score_rises_or_holds():
    """A score that rises or holds vs the baseline passes the eval gate."""
    m = _load_runner()
    assert m._eval_gate_reason(0.80, 0.85) is None     # rose -> ok
    assert m._eval_gate_reason(0.80, 0.80) is None     # held -> ok


def test_eval_gate_reverts_when_score_drops():
    """A score that drops vs the baseline reverts the branch (anti-gaming: the change made the
    product WORSE on the richer needle, even though the tests stayed green)."""
    m = _load_runner()
    reason = m._eval_gate_reason(0.80, 0.75)
    assert reason is not None
    assert "eval" in reason.lower() and "fell" in reason.lower()


def test_eval_gate_no_baseline_always_ok():
    """Without a baseline score (e.g. the eval command failed on the base, or printed no float),
    the eval gate does NOT block — there's nothing to compare against (a new eval needle)."""
    m = _load_runner()
    assert m._eval_gate_reason(None, 0.5) is None
    assert m._eval_gate_reason(None, None) is None


def test_eval_gate_no_after_score_ok():
    """If the eval command printed no float AFTER the change, the gate can't measure a drop — don't
    block (but the iteration should be flagged for a missing eval; the runner logs it)."""
    m = _load_runner()
    assert m._eval_gate_reason(0.8, None) is None


# ---- end-to-end eval gate execution against a real command -----------------
@pytest.mark.skipif(not shutil.which("git"), reason="git not available")
def test_run_eval_gate_green(tmp_path, monkeypatch):
    """A repo whose EVAL_CMD prints a float >= the baseline passes the eval gate."""
    m = _load_runner()
    a = tmp_path / "a"
    a.mkdir()
    subprocess.run(["git", "init", str(a)], capture_output=True)
    subprocess.run(["git", "-C", str(a), "checkout", "-b", "main"], capture_output=True)
    repos_json = tmp_path / "repos.json"
    repos_json.write_text(json.dumps([
        {"name": "a", "path": str(a), "EVAL_CMD": "echo 0.90"},
    ]), encoding="utf-8")
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    monkeypatch.setattr(m, "REPO", a)
    monkeypatch.setattr(m, "NAME", "a")
    res = m._run_eval_gate(0.85)
    assert res["ok"] is True
    assert res["score"] == 0.90


@pytest.mark.skipif(not shutil.which("git"), reason="git not available")
def test_run_eval_gate_red_reverts(tmp_path, monkeypatch):
    """A repo whose EVAL_CMD prints a float < the baseline fails the eval gate (ok=False)."""
    m = _load_runner()
    a = tmp_path / "a"
    a.mkdir()
    subprocess.run(["git", "init", str(a)], capture_output=True)
    subprocess.run(["git", "-C", str(a), "checkout", "-b", "main"], capture_output=True)
    repos_json = tmp_path / "repos.json"
    repos_json.write_text(json.dumps([
        {"name": "a", "path": str(a), "EVAL_CMD": "echo 0.70"},
    ]), encoding="utf-8")
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    monkeypatch.setattr(m, "REPO", a)
    monkeypatch.setattr(m, "NAME", "a")
    res = m._run_eval_gate(0.85)
    assert res["ok"] is False
    assert res["score"] == 0.70
    assert "drop" in (res.get("reason") or "").lower() or "eval" in (res.get("reason") or "").lower()


def test_run_eval_gate_no_cmd_is_noop(tmp_path, monkeypatch):
    """No EVAL_CMD declared -> _run_eval_gate returns ok=True (no gate; backward compatible)."""
    m = _load_runner()
    a = tmp_path / "a"
    a.mkdir()
    repos_json = tmp_path / "repos.json"
    repos_json.write_text(json.dumps([{"name": "a", "path": str(a)}]), encoding="utf-8")
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    monkeypatch.setattr(m, "REPO", a)
    monkeypatch.setattr(m, "NAME", "a")
    res = m._run_eval_gate(0.85)
    assert res["ok"] is True
    assert res.get("score") is None


@pytest.mark.skipif(not shutil.which("git"), reason="git not available")
def test_run_eval_gate_unparseable_stdout_ok(tmp_path, monkeypatch):
    """If the EVAL_CMD prints no float, the gate can't measure — ok=True (don't block on a missing
    needle; the runner logs it). This is the 'new eval needle' case."""
    m = _load_runner()
    a = tmp_path / "a"
    a.mkdir()
    subprocess.run(["git", "init", str(a)], capture_output=True)
    subprocess.run(["git", "-C", str(a), "checkout", "-b", "main"], capture_output=True)
    repos_json = tmp_path / "repos.json"
    repos_json.write_text(json.dumps([
        {"name": "a", "path": str(a), "EVAL_CMD": "echo no score here"},
    ]), encoding="utf-8")
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    monkeypatch.setattr(m, "REPO", a)
    monkeypatch.setattr(m, "NAME", "a")
    res = m._run_eval_gate(0.85)
    assert res["ok"] is True                 # no float -> can't measure a drop -> don't block
    assert res.get("score") is None