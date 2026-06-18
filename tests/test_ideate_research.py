"""Tests for the ideate-lane external-research allowance (Video 1: 'agents hyperfocus, got stuck on
a local minimum, never looked up new ideas on the internet'). When a repo declares
`ideate_research: true` in repos.json AND auto_ai_fix is on, the ideate agent is PERMITTED (but not
required) to do web/docs lookup (Context7/webfetch-style) for novel ideas. The output is still
reviewable backlog items — the runner sorts + prepends, the agent never edits its own menu. Default
false (backward compatible; the agent reads only the local repo unless opted in).
"""
import importlib.util
import json
import os
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


# ---- ideate_research flag resolution from repos.json ----------------------
def test_ideate_research_missing_is_false(tmp_path, monkeypatch):
    m = _load_runner()
    repos_json = tmp_path / "repos.json"
    repos_json.write_text(json.dumps([{"name": "a", "path": str(tmp_path / "a")}]), encoding="utf-8")
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    assert m._ideate_research_enabled("a") is False


def test_ideate_research_true_is_true(tmp_path, monkeypatch):
    m = _load_runner()
    repos_json = tmp_path / "repos.json"
    repos_json.write_text(json.dumps([
        {"name": "a", "path": str(tmp_path / "a"), "ideate_research": True},
    ]), encoding="utf-8")
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    assert m._ideate_research_enabled("a") is True


def test_ideate_research_false_is_false(tmp_path, monkeypatch):
    m = _load_runner()
    repos_json = tmp_path / "repos.json"
    repos_json.write_text(json.dumps([
        {"name": "a", "path": str(tmp_path / "a"), "ideate_research": False},
    ]), encoding="utf-8")
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    assert m._ideate_research_enabled("a") is False


# ---- the ideate task includes the research allowance when the flag is set --
def test_ideate_task_without_research_flag_has_no_web_allowance(tmp_path, monkeypatch):
    """When ideate_research is OFF, the ideate task does NOT permit web lookup (the agent reads only
    the local repo — the existing behavior, preserved)."""
    m = _load_runner()
    repos_json = tmp_path / "repos.json"
    repos_json.write_text(json.dumps([{"name": "a", "path": str(tmp_path / "a")}]), encoding="utf-8")
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    monkeypatch.setattr(m, "NAME", "a")
    monkeypatch.setattr(m, "GOAL", "make it excellent")
    task = m._ideate_task()
    # no web/internet/research allowance when the flag is off
    assert "internet" not in task.lower()
    assert "external research" not in task.lower()
    # the task still points at ideate.md (the hard rules live there)
    assert "ideate.md" in task


def test_ideate_task_with_research_flag_includes_web_allowance(tmp_path, monkeypatch):
    """When ideate_research is ON, the ideate task PERMITS (but does not require) web/docs lookup for
    novel ideas — Video 1's 'look up new ideas on the internet' to escape local minima. The output is
    still reviewable backlog items (the runner sorts + prepends; the agent never edits its own menu)."""
    m = _load_runner()
    repos_json = tmp_path / "repos.json"
    repos_json.write_text(json.dumps([
        {"name": "a", "path": str(tmp_path / "a"), "ideate_research": True},
    ]), encoding="utf-8")
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    monkeypatch.setattr(m, "NAME", "a")
    monkeypatch.setattr(m, "GOAL", "make it excellent")
    task = m._ideate_task()
    assert "EXTERNAL RESEARCH ALLOWED" in task          # the allowance is present
    assert "internet" in task.lower()                    # the explicit 'look up new ideas on the internet'
    # the human-owned-menu guardrail is still stated
    assert "reviewable" in task.lower() or "never edit" in task.lower()
    # the task still points at ideate.md (the hard rules live there)
    assert "ideate.md" in task