"""Tests for the operator-approved ESCALATION LADDER (replaces silent defer).

Verifies the rungs that turn a stuck backlog item into adaptive progress instead of a dead-end:
  rung 0  — a failure injects a ONE-TIME corrective note into the next task (and is then cleared);
  finding #6 — a green-but-REVERTED iteration now escalates (it used to increment no counter and loop);
  rung 1  — a repeatedly-stuck item switches to the fallback model for its next attempt;
  cumulative — noop + deviation + revert all count toward the SAME per-item tally;
  success — a landed ship clears the item's escalation state;
  rung 2  — with decomposition enabled, a stuck item is split into sub-items instead of deferred.
"""
import importlib.util
import os
import sys
from types import SimpleNamespace

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

RUNNER = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                      "improver", "run_improver.py")


def _load_runner():
    spec = importlib.util.spec_from_file_location("run_improver", RUNNER)
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    m.BEAUTIFY = False
    m.SOLOMON = False
    m._fail_counts.clear()
    m._escalated_goals.clear()
    m.LAST_GATE_FEEDBACK = ""
    return m


def test_failure_feedback_injected_into_next_task_once():
    m = _load_runner()
    m._note_revert("item A", "the gate was GAMED (added 8 skip markers) — make the real tests pass")
    assert m.LAST_GATE_FEEDBACK                                   # captured on failure
    task = m.build_task("item A", "chore")
    assert "PREVIOUS ATTEMPT" in task and "skip markers" in task  # injected into the next task
    assert m.LAST_GATE_FEEDBACK == ""                             # consumed — one-time
    assert "PREVIOUS ATTEMPT" not in m.build_task("item A", "chore")   # not repeated


def test_revert_escalates_and_defers_after_limit(tmp_path, monkeypatch):
    # finding #6: a reverted iteration used to increment NO counter, so a gamed item looped forever.
    m = _load_runner()
    bl = tmp_path / "backlog.md"
    bl.write_text("# b\n\n- [ ] gamed item\n- [ ] next item\n", encoding="utf-8")
    monkeypatch.setattr(m, "BACKLOG", bl)
    monkeypatch.setattr(m, "log", lambda *a, **k: None)
    for _ in range(2):
        m._note_revert("gamed item", "gate gamed", limit=3)
    assert m._top_backlog_item()[0] == "gamed item"              # < limit -> still selected
    m._note_revert("gamed item", "gate gamed", limit=3)          # 3rd revert -> deferred
    assert "(deferred" in bl.read_text(encoding="utf-8")
    assert m._top_backlog_item()[0] == "next item"


def test_repeated_failure_escalates_to_fallback_model():
    m = _load_runner()
    m.PROVIDER_NAME = "ollama-cloud"
    m.PI_MODEL = "glm-5.2"
    m._note_noop("item A", limit=3)                              # n=1 — below fallback rung
    assert "item A" not in m._escalated_goals
    m._note_noop("item A", limit=3)                              # n=2 == _ESCALATE_TO_FALLBACK
    assert "item A" in m._escalated_goals
    m._apply_fallback_model("item A")
    assert m.PI_MODEL == m._FALLBACK_MODEL["ollama-cloud"]       # switched models for the retry


def test_fallback_model_not_applied_for_unescalated_goal():
    m = _load_runner()
    m.PROVIDER_NAME = "ollama-cloud"
    m.PI_MODEL = "glm-5.2"
    m._apply_fallback_model("fresh item")                       # not escalated
    assert m.PI_MODEL == "glm-5.2"                              # unchanged


def test_failure_count_is_cumulative_across_kinds(tmp_path, monkeypatch):
    m = _load_runner()
    bl = tmp_path / "backlog.md"
    bl.write_text("# b\n\n- [ ] A\n- [ ] B\n", encoding="utf-8")
    monkeypatch.setattr(m, "BACKLOG", bl)
    monkeypatch.setattr(m, "log", lambda *a, **k: None)
    m._note_noop("A", limit=3)                                  # n=1
    m._note_deviation("A", limit=3)                             # n=2 (same tally, different kind)
    assert m._top_backlog_item()[0] == "A"                      # not yet deferred
    m._note_revert("A", "failed gate", limit=3)                 # n=3 -> deferred
    assert "(deferred" in bl.read_text(encoding="utf-8")
    assert m._top_backlog_item()[0] == "B"


def test_success_clears_escalation_state():
    m = _load_runner()
    m._fail_counts["A"] = 2
    m._escalated_goals.add("A")
    m._clear_failure_state("A")
    assert "A" not in m._fail_counts and "A" not in m._escalated_goals


def test_decompose_replaces_item_with_subitems(tmp_path, monkeypatch):
    m = _load_runner()
    bl = tmp_path / "backlog.md"
    bl.write_text("# b\n\n- [ ] big item\n- [ ] other\n", encoding="utf-8")
    md = tmp_path / "decompose.md"
    md.write_text("decompose contract", encoding="utf-8")
    monkeypatch.setattr(m, "BACKLOG", bl)
    monkeypatch.setattr(m, "DECOMPOSE_MD", md)
    monkeypatch.setattr(m, "log", lambda *a, **k: None)
    monkeypatch.setattr(m, "run_pi",
                        lambda *a, **k: SimpleNamespace(stdout="- slice one\n- slice two\n- slice three"))
    monkeypatch.setattr(m, "final_text", lambda s: s)
    assert m._decompose_item("big item", "too big") is True
    txt = bl.read_text(encoding="utf-8")
    assert "- [ ] slice one" in txt and "- [ ] slice two" in txt and "- [ ] slice three" in txt
    assert "big item" not in txt                                # original replaced
    assert "- [ ] other" in txt                                # sibling untouched


def test_decompose_declines_when_too_few_subitems(tmp_path, monkeypatch):
    m = _load_runner()
    bl = tmp_path / "backlog.md"
    bl.write_text("# b\n\n- [ ] big item\n", encoding="utf-8")
    md = tmp_path / "decompose.md"
    md.write_text("x", encoding="utf-8")
    monkeypatch.setattr(m, "BACKLOG", bl)
    monkeypatch.setattr(m, "DECOMPOSE_MD", md)
    monkeypatch.setattr(m, "run_pi", lambda *a, **k: SimpleNamespace(stdout="- only one slice"))
    monkeypatch.setattr(m, "final_text", lambda s: s)
    assert m._decompose_item("big item", "") is False          # < 2 sub-items -> don't decompose
    assert "- [ ] big item" in bl.read_text(encoding="utf-8")   # backlog untouched


if __name__ == "__main__":
    import pytest
    pytest.main([__file__, "-v"])
