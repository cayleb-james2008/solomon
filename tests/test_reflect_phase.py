"""Tests for the REFLECT pipeline phase + the shared novelty/dedup helpers, no pi/network.

The REFLECT phase runs AFTER each iteration (shipped OR failed/deferred) and distills one durable,
concrete lesson — what was attempted, the outcome, the root cause if it failed, and what to try or
avoid next — appending it to the TARGET repo's LESSONS store (improver/<name>/LESSONS.md, mirroring
the AGENT.md / backlog.md per-repo path resolution). Lessons are timestamped, concise, and
DEDUPLICATED (a near-duplicate of an existing lesson is dropped, not appended again).

  TOK-1  _tokenize lowercases + strips punctuation + drops stopwords
  JAC-1  _jaccard is 1.0 for identical token sets, 0.0 for disjoint, in-between for overlap
  NOV-1  _is_novel: an idea with high overlap against the corpus is NOT novel
  NOV-2  _is_novel: a genuinely different idea IS novel (and an empty corpus is always novel)
  RF-1   reflect() appends a lesson to improver/<name>/LESSONS.md for a SHIPPED iteration
  RF-2   reflect() appends a lesson for a FAILED/reverted iteration (records the outcome/root cause)
  RF-3   reflect() DEDUPES — a near-duplicate lesson is not appended a second time
  RF-4   reflect() is a no-op (no file, no pi) when there is no history to reflect on
  RF-5   _read_lessons returns the accumulated lesson text (used by ideate's novelty corpus)
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


# ---- novelty / dedup helpers ------------------------------------------------
def test_tokenize_normalizes_and_drops_stopwords():
    m = _load_runner()
    toks = m._tokenize("Add a Caching LAYER, for the API!!")
    assert "caching" in toks and "layer" in toks and "api" in toks
    # stopwords + punctuation gone
    assert "a" not in toks and "the" not in toks and "for" not in toks


def test_jaccard_bounds():
    m = _load_runner()
    a = {"caching", "layer", "api"}
    assert m._jaccard(a, a) == 1.0                      # identical
    assert m._jaccard(a, {"unrelated", "tokens"}) == 0.0  # disjoint
    j = m._jaccard(a, {"caching", "layer", "store"})    # 2 shared / 4 union
    assert 0.0 < j < 1.0
    assert m._jaccard(set(), set()) == 0.0              # empty/empty -> 0 (safe)


def test_is_novel_rejects_near_duplicate():
    m = _load_runner()
    corpus = ["Add a Redis caching layer in front of the API responses"]
    # a near-duplicate phrased slightly differently is NOT novel
    assert m._is_novel("Add a caching layer in front of API responses with Redis", corpus) is False


def test_is_novel_accepts_fresh_idea_and_empty_corpus():
    m = _load_runner()
    corpus = ["Add a Redis caching layer in front of the API responses"]
    assert m._is_novel("Replace the polling scheduler with an event-driven webhook bus", corpus) is True
    assert m._is_novel("anything at all", []) is True   # empty corpus -> always novel


# ---- reflect() writes a deduplicated, timestamped lesson --------------------
def _wire_reflect(m, monkeypatch, tmp_path, name="demo", history=None, lesson_out="LESSON: test"):
    """Point the runner's per-repo paths at tmp_path, stub history + the pi call + the clock."""
    runtime = tmp_path / "runtime" / name
    runtime.mkdir(parents=True, exist_ok=True)
    if history is not None:
        (runtime / "history.jsonl").write_text(
            "\n".join(json.dumps(h) for h in history) + "\n", encoding="utf-8")
    lessons = tmp_path / "improver" / name / "LESSONS.md"
    monkeypatch.setattr(m, "NAME", name)
    monkeypatch.setattr(m, "RUNTIME", runtime)
    monkeypatch.setattr(m, "LESSONS", lessons)
    monkeypatch.setattr(m, "BACKLOG", tmp_path / "improver" / name / "backlog.md")
    monkeypatch.setattr(m, "REFLECT_ENABLED", True)   # the phase is opt-in; enable it for the test
    monkeypatch.setattr(m, "log", lambda *a, **k: None)
    monkeypatch.setattr(m, "heartbeat", lambda *a, **k: None)
    monkeypatch.setattr(m, "_now", lambda: "2026-06-19T00:00:00Z")
    # the reflect agent (pi) returns one distilled lesson line
    monkeypatch.setattr(m, "_phase_run_pi", lambda *a, **k: _P(lesson_out))
    monkeypatch.setattr(m, "final_text", lambda s: s)
    return lessons


def test_reflect_appends_lesson_for_shipped(tmp_path, monkeypatch):
    m = _load_runner()
    lessons = _wire_reflect(
        m, monkeypatch, tmp_path,
        history=[{"status": "shipped", "summary": "added a caching layer", "tests": {"passed": 10}}],
        lesson_out="LESSON: caching layer shipped cleanly — keep cache keys versioned next time")
    m.reflect()
    assert lessons.exists()
    text = lessons.read_text(encoding="utf-8")
    assert "caching layer shipped" in text
    assert "2026-06-19T00:00:00Z" in text          # timestamped


def test_reflect_appends_lesson_for_failed(tmp_path, monkeypatch):
    m = _load_runner()
    lessons = _wire_reflect(
        m, monkeypatch, tmp_path,
        history=[{"status": "reverted", "summary": "tried X, gate failed", "tests": {"failed": 3}}],
        lesson_out="LESSON: X failed the gate because of a missing fixture — add the fixture first")
    m.reflect()
    text = lessons.read_text(encoding="utf-8")
    assert "missing fixture" in text


def test_reflect_dedupes_near_duplicate(tmp_path, monkeypatch):
    m = _load_runner()
    dup = "LESSON: the cache keys must be versioned or stale reads slip through after a deploy"
    lessons = _wire_reflect(
        m, monkeypatch, tmp_path,
        history=[{"status": "shipped", "summary": "caching", "tests": {"passed": 1}}],
        lesson_out=dup)
    # pre-seed an existing, near-identical lesson
    lessons.parent.mkdir(parents=True, exist_ok=True)
    lessons.write_text(
        "# Lessons\n\n- 2026-06-18T00:00:00Z — the cache keys MUST be versioned, "
        "or stale reads slip through after every deploy\n", encoding="utf-8")
    before = lessons.read_text(encoding="utf-8")
    m.reflect()
    after = lessons.read_text(encoding="utf-8")
    assert after == before          # near-duplicate not appended


def test_reflect_noop_without_history(tmp_path, monkeypatch):
    m = _load_runner()
    called = {"pi": False}

    def _boom(*a, **k):
        called["pi"] = True
        return _P("LESSON: should never run")

    lessons = _wire_reflect(m, monkeypatch, tmp_path, history=None)
    monkeypatch.setattr(m, "_phase_run_pi", _boom)
    m.reflect()
    assert called["pi"] is False         # no history -> no pi call
    assert not lessons.exists()          # and no lessons file created


def test_read_lessons_returns_accumulated_text(tmp_path, monkeypatch):
    m = _load_runner()
    lessons = tmp_path / "improver" / "demo" / "LESSONS.md"
    lessons.parent.mkdir(parents=True, exist_ok=True)
    lessons.write_text("# Lessons\n\n- 2026-06-18T00:00:00Z — versioned cache keys\n", encoding="utf-8")
    monkeypatch.setattr(m, "LESSONS", lessons)
    out = m._read_lessons()
    assert "versioned cache keys" in out
    # absent file -> empty string (best-effort)
    monkeypatch.setattr(m, "LESSONS", tmp_path / "nope" / "LESSONS.md")
    assert m._read_lessons() == ""
