"""Tests for ideate() novelty scoring + the ideate/reflect pipeline-phase wiring, no pi/network.

Video 1: 'agents hyperfocus, got stuck on a local minimum'. ideate() now scores each candidate idea
against (a) the current backlog, (b) recent run history (history.jsonl summaries), and (c) the
accumulated lessons (LESSONS.md), dropping near-duplicates (simple token/Jaccard overlap) so the
divergent lane yields genuinely FRESH directions instead of re-proposing what is already on the menu
or was just tried.

  NV-1  ideate drops a candidate that near-duplicates an existing backlog item
  NV-2  ideate drops a candidate that near-duplicates a recent history summary
  NV-3  ideate keeps genuinely novel candidates (prepended to the backlog)
  NV-4  ideate filters against accumulated lessons too
  WR-1  pipeline.ideate true -> IDEATE_ENABLED true; absent -> false (default off)
  WR-2  pipeline.reflect true -> REFLECT_ENABLED true; absent -> false (default off)
  WR-3  ideate_phase() is a no-op when IDEATE_ENABLED is off
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


class _P:
    def __init__(self, out):
        self.stdout = out
        self.returncode = 0
        self.stderr = ""


def _wire_ideate(m, monkeypatch, tmp_path, name, ideas_text,
                 backlog="# backlog\n", history=None, lessons=None):
    """Point ideate() at tmp_path files; stub the pi call to return `ideas_text`."""
    runtime = tmp_path / "runtime" / name
    runtime.mkdir(parents=True, exist_ok=True)
    if history is not None:
        (runtime / "history.jsonl").write_text(
            "\n".join(json.dumps(h) for h in history) + "\n", encoding="utf-8")
    backlog_p = tmp_path / "improver" / name / "backlog.md"
    backlog_p.parent.mkdir(parents=True, exist_ok=True)
    backlog_p.write_text(backlog, encoding="utf-8")
    lessons_p = tmp_path / "improver" / name / "LESSONS.md"
    if lessons is not None:
        lessons_p.write_text(lessons, encoding="utf-8")
    monkeypatch.setattr(m, "NAME", name)
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    monkeypatch.setattr(m, "RUNTIME", runtime)
    monkeypatch.setattr(m, "BACKLOG", backlog_p)
    monkeypatch.setattr(m, "LESSONS", lessons_p)
    monkeypatch.setattr(m, "GOAL", "make it excellent")
    monkeypatch.setattr(m, "log", lambda *a, **k: None)
    monkeypatch.setattr(m, "run_pi", lambda *a, **k: _P(ideas_text))
    monkeypatch.setattr(m, "final_text", lambda s: s)
    # repos.json so _ideate_research_enabled / _apply_phase_config don't blow up
    (tmp_path / "repos.json").write_text(json.dumps([{"name": name, "path": str(tmp_path)}]),
                                         encoding="utf-8")
    return backlog_p


def test_ideate_drops_backlog_duplicate(tmp_path, monkeypatch):
    m = _load_runner()
    ideas = ("[feature] | 5 | Add a Redis caching layer in front of the API responses — why: speed\n"
             "[architecture] | 4 | Replace polling with an event-driven webhook bus — why: latency\n")
    backlog_p = _wire_ideate(
        m, monkeypatch, tmp_path, "demo", ideas,
        backlog="# backlog\n- [ ] [feature] Add a caching layer in front of API responses with Redis\n")
    rc = m.ideate()
    assert rc == 0
    text = backlog_p.read_text(encoding="utf-8")
    # the webhook idea is novel and added; the caching idea duped the backlog and was dropped
    assert "webhook bus" in text
    assert text.count("caching layer") == 1   # only the pre-existing backlog line, no new dup


def test_ideate_drops_history_duplicate(tmp_path, monkeypatch):
    m = _load_runner()
    ideas = ("[feature] | 5 | Add a Redis caching layer in front of the API responses — why: speed\n"
             "[architecture] | 4 | Replace polling with an event-driven webhook bus — why: latency\n")
    backlog_p = _wire_ideate(
        m, monkeypatch, tmp_path, "demo", ideas,
        history=[{"status": "shipped",
                  "summary": "Added a caching layer in front of the API responses using Redis"}])
    rc = m.ideate()
    assert rc == 0
    text = backlog_p.read_text(encoding="utf-8")
    assert "webhook bus" in text
    assert "caching layer" not in text   # already tried (in history) -> dropped


def test_ideate_keeps_novel_ideas(tmp_path, monkeypatch):
    m = _load_runner()
    ideas = ("[feature] | 5 | Add a Redis caching layer in front of the API responses — why: speed\n"
             "[architecture] | 4 | Replace polling with an event-driven webhook bus — why: latency\n")
    backlog_p = _wire_ideate(m, monkeypatch, tmp_path, "demo", ideas, backlog="# backlog\n")
    rc = m.ideate()
    assert rc == 0
    text = backlog_p.read_text(encoding="utf-8")
    assert "caching layer" in text and "webhook bus" in text   # both novel -> both kept


def test_ideate_filters_against_lessons(tmp_path, monkeypatch):
    m = _load_runner()
    ideas = ("[feature] | 5 | Add a Redis caching layer in front of the API responses — why: speed\n"
             "[architecture] | 4 | Replace polling with an event-driven webhook bus — why: latency\n")
    backlog_p = _wire_ideate(
        m, monkeypatch, tmp_path, "demo", ideas,
        lessons="# Lessons\n\n- 2026-06-18T00:00:00Z — a Redis caching layer in front of the API "
                "responses was tried and reverted; avoid re-proposing it\n")
    rc = m.ideate()
    assert rc == 0
    text = backlog_p.read_text(encoding="utf-8")
    assert "webhook bus" in text
    assert "caching layer" not in text   # the lesson warned against it -> dropped


# ---- pipeline wiring: ideate + reflect booleans (default OFF) ---------------
def test_pipeline_ideate_flag(tmp_path, monkeypatch):
    m = _load_runner()
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    monkeypatch.setattr(m, "NAME", "demo")
    # absent -> default OFF
    (tmp_path / "repos.json").write_text(json.dumps([{"name": "demo", "path": str(tmp_path)}]),
                                         encoding="utf-8")
    m._refresh_config_from_registry()
    assert m.IDEATE_ENABLED is False
    # explicit true -> ON
    (tmp_path / "repos.json").write_text(json.dumps(
        [{"name": "demo", "path": str(tmp_path), "pipeline": {"ideate": True}}]), encoding="utf-8")
    m._refresh_config_from_registry()
    assert m.IDEATE_ENABLED is True


def test_pipeline_reflect_flag(tmp_path, monkeypatch):
    m = _load_runner()
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    monkeypatch.setattr(m, "NAME", "demo")
    (tmp_path / "repos.json").write_text(json.dumps([{"name": "demo", "path": str(tmp_path)}]),
                                         encoding="utf-8")
    m._refresh_config_from_registry()
    assert m.REFLECT_ENABLED is False
    (tmp_path / "repos.json").write_text(json.dumps(
        [{"name": "demo", "path": str(tmp_path), "pipeline": {"reflect": True}}]), encoding="utf-8")
    m._refresh_config_from_registry()
    assert m.REFLECT_ENABLED is True


def test_ideate_phase_noop_when_disabled(tmp_path, monkeypatch):
    m = _load_runner()
    called = {"pi": False}
    monkeypatch.setattr(m, "IDEATE_ENABLED", False)
    monkeypatch.setattr(m, "run_pi", lambda *a, **k: called.__setitem__("pi", True))
    monkeypatch.setattr(m, "log", lambda *a, **k: None)
    m.ideate_phase()
    assert called["pi"] is False
