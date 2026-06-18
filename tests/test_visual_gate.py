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