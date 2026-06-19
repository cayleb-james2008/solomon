"""Tests for the visual-review HARD gate (Video 1: 'agents can cheat, rewrite the evaluation
function' — the visual gate was advisory-only, a gap). When a repo declares `visual_gate: true` in
repos.json, a visual-review failure (a critical finding from visual_review.run) BLOCKS the ship —
the branch is reverted (same as a test-gate red). Off by default for non-UI repos (backward
compatible). The existing advisory-only path (VISUAL_REVIEW_ENABLED via sandbox config) is
unchanged; `visual_gate: true` upgrades it to blocking.
"""
import importlib.util
import json
import os
import sys
from pathlib import Path

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


# ---- visual_gate flag resolution from repos.json ---------------------------
def test_visual_gate_missing_is_false(tmp_path, monkeypatch):
    m = _load_runner()
    repos_json = tmp_path / "repos.json"
    repos_json.write_text(json.dumps([{"name": "a", "path": str(tmp_path / "a")}]), encoding="utf-8")
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    assert m._visual_gate_enabled("a") is False


def test_visual_gate_true_is_true(tmp_path, monkeypatch):
    m = _load_runner()
    repos_json = tmp_path / "repos.json"
    repos_json.write_text(json.dumps([
        {"name": "a", "path": str(tmp_path / "a"), "visual_gate": True},
    ]), encoding="utf-8")
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    assert m._visual_gate_enabled("a") is True


def test_visual_gate_false_is_false(tmp_path, monkeypatch):
    m = _load_runner()
    repos_json = tmp_path / "repos.json"
    repos_json.write_text(json.dumps([
        {"name": "a", "path": str(tmp_path / "a"), "visual_gate": False},
    ]), encoding="utf-8")
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    assert m._visual_gate_enabled("a") is False


# ---- the hard-gate decision over a visual_review.run result ---------------
def test_visual_gate_decision_blocks_on_critical():
    """A visual-review result with >=1 critical finding BLOCKS the ship when visual_gate is on."""
    m = _load_runner()
    res = {"ok": True, "findings": [
        {"severity": "critical", "category": "layout", "description": "broken"},
        {"severity": "warning", "category": "a11y", "description": "minor"},
    ]}
    assert m._visual_gate_reason(res) is not None
    assert "critical" in m._visual_gate_reason(res).lower()


def test_visual_gate_decision_allows_warnings_only():
    """Warnings alone (no critical) do NOT block — they become feedback for the next iteration."""
    m = _load_runner()
    res = {"ok": True, "findings": [
        {"severity": "warning", "category": "a11y", "description": "minor"},
    ]}
    assert m._visual_gate_reason(res) is None


def test_visual_gate_decision_allows_no_findings():
    m = _load_runner()
    assert m._visual_gate_reason({"ok": True, "findings": []}) is None


def test_visual_gate_decision_allows_failed_review():
    """A visual-review that itself FAILED to run (ok=False) does NOT block the ship — it's
    best-effort (the RSI loop must not break if the visual infra is down). The advisory path still
    applies (no feedback); the hard gate only fires on a SUCCESSFUL review WITH critical findings."""
    m = _load_runner()
    res = {"ok": False, "error": "sandbox failed to boot"}
    assert m._visual_gate_reason(res) is None


def test_visual_gate_decision_handles_missing_findings_key():
    m = _load_runner()
    assert m._visual_gate_reason({"ok": True}) is None
    assert m._visual_gate_reason({}) is None
    assert m._visual_gate_reason(None) is None


# ---- Feature 1a: mandatory visual testing for frontend repos -------------
def test_visual_gate_auto_on_for_frontend_repo(tmp_path, monkeypatch):
    """When visual_gate is ABSENT and the repo has a detected frontend (index.html), the gate
    is ON by default — mandatory visual testing phase for frontend repos."""
    m = _load_runner()
    repo_dir = tmp_path / "a"
    repo_dir.mkdir()
    (repo_dir / "index.html").write_text("<!DOCTYPE html><html></html>", encoding="utf-8")
    repos_json = tmp_path / "repos.json"
    repos_json.write_text(json.dumps([{"name": "a", "path": str(repo_dir)}]), encoding="utf-8")
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    assert m._visual_gate_enabled("a") is True


def test_visual_gate_explicit_false_overrides_frontend(tmp_path, monkeypatch):
    """visual_gate: false in repos.json is an explicit opt-out that wins over frontend detection
    — e.g. a headless API repo that happens to have a templates/ dir."""
    m = _load_runner()
    repo_dir = tmp_path / "a"
    repo_dir.mkdir()
    (repo_dir / "index.html").write_text("<!DOCTYPE html>", encoding="utf-8")
    repos_json = tmp_path / "repos.json"
    repos_json.write_text(json.dumps([
        {"name": "a", "path": str(repo_dir), "visual_gate": False},
    ]), encoding="utf-8")
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    assert m._visual_gate_enabled("a") is False


def test_visual_gate_off_for_non_frontend(tmp_path, monkeypatch):
    """A repo with no frontend markers and no visual_gate flag stays OFF — byte-identical to the
    legacy behavior for non-UI repos (libraries, CLIs, headless services)."""
    m = _load_runner()
    repo_dir = tmp_path / "a"
    repo_dir.mkdir()
    (repo_dir / "main.py").write_text("print('hi')", encoding="utf-8")
    repos_json = tmp_path / "repos.json"
    repos_json.write_text(json.dumps([{"name": "a", "path": str(repo_dir)}]), encoding="utf-8")
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    assert m._visual_gate_enabled("a") is False


# ---- control.has_frontend (Feature 1a frontend detector) -------------------
def test_has_frontend_detects_index_html(tmp_path):
    import control
    d = tmp_path / "repo"; d.mkdir()
    (d / "index.html").write_text("<html></html>", encoding="utf-8")
    assert control.has_frontend({"path": str(d)}) is True


def test_has_frontend_detects_react_in_package_json(tmp_path):
    import control
    d = tmp_path / "repo"; d.mkdir()
    (d / "package.json").write_text(json.dumps({"dependencies": {"react": "^18"}}), encoding="utf-8")
    assert control.has_frontend({"path": str(d)}) is True


def test_has_frontend_detects_templates_dir(tmp_path):
    import control
    d = tmp_path / "repo"; d.mkdir()
    (d / "templates").mkdir()
    assert control.has_frontend({"path": str(d)}) is True


def test_has_frontend_false_for_headless_repo(tmp_path):
    import control
    d = tmp_path / "repo"; d.mkdir()
    (d / "main.py").write_text("print('hi')", encoding="utf-8")
    assert control.has_frontend({"path": str(d)}) is False