"""Tests for the untracked-agent-artifact recovery path (Video 1: 'agents go nuts in long-running
sessions, return to a complete mess'). Leftover agent files (AGENT_LOG.md, capabilities/*, …)
used to block the preflight clean FOREVER (the runner refused to `git clean -fd` them as operator-work
protection). Now the runner recognizes a conservative set of agent-artifact patterns and STAGES them
on the rsi branch (not the base), runs the gate, and ships or reverts — instead of wedging. Operator
work (anything NOT matching the heuristics) stays protected (refuse + escalate)."""
import importlib.util
import os
import sys

import pytest

RUNNER = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                      "improver", "run_improver.py")


def _load_runner():
    spec = importlib.util.spec_from_file_location("run_improver", RUNNER)
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


# ---- pattern matching ------------------------------------------------------
def test_agent_artifact_patterns_constant_exists():
    m = _load_runner()
    assert hasattr(m, "_AGENT_ARTIFACT_PATTERNS")
    assert isinstance(m._AGENT_ARTIFACT_PATTERNS, (list, tuple)) and m._AGENT_ARTIFACT_PATTERNS


def test_recognized_agent_artifact_files():
    """Files matching the conservative agent-artifact heuristics are recognized."""
    m = _load_runner()
    recognized = [
        "AGENT_LOG.md",
        "capabilities/exec_research/plan.md",
        "capabilities/exec_research/",
        "profiles/ggg/profile.json",
        "profiles/ggg/",
        "start_ggg.sh",
        "start_maki.sh",
        ".agent_artifacts/anything.txt",
        ".agent_artifacts/sub/deep/file.json",
    ]
    for f in recognized:
        assert m._is_agent_artifact(f), f"expected recognized: {f}"


def test_operator_work_not_recognized():
    """Files that could be real operator work are NOT classified as agent artifacts."""
    m = _load_runner()
    operator_work = [
        "README.md",
        "notes.txt",
        "new_module.py",
        "tests/test_new.py",
        "data/x.json",
        "config.yaml",
        "scripts/run.py",
        "src/app.py",
        "maki-notes.md",          # looks nothing like an agent pattern
        "start.sh",                # bare 'start.sh' without the prefix? matches start_*.sh? NO — pattern is start_*.sh
        "startfoo.sh",             # not start_ prefix
        "AGENT_LOG.txt",           # wrong extension (.md required)
        "capabilities/",           # bare dir — conservative: require a path under it
        "profiles/",               # bare dir — same
    ]
    for f in operator_work:
        assert not m._is_agent_artifact(f), f"should NOT be recognized as artifact: {f}"


def test_all_agent_artifacts_true_when_all_match():
    """Empty list is NOT 'all artifacts' (it's no files — the existing path stays)."""
    m = _load_runner()
    assert m._all_agent_artifacts(["AGENT_LOG.md", "capabilities/x/y.md"]) is True
    assert m._all_agent_artifacts(["AGENT_LOG.md", "README.md"]) is False
    assert m._all_agent_artifacts([]) is False     # empty != all-match (no recovery path)


def test_all_agent_artifacts_mixed():
    """One operator file among many artifacts -> still NOT all artifacts (protect operator work)."""
    m = _load_runner()
    files = ["AGENT_LOG.md", "capabilities/a/b.md", "profiles/ggg/x.json", "start_maki.sh",
             "my_real_work.py"]
    assert m._all_agent_artifacts(files) is False


# ---- recovery path (pure predicate: what would happen) ---------------------
def test_dirty_blocks_iteration_unchanged_for_base():
    """The existing dirty-tracked-file guard on the base branch is unchanged."""
    m = _load_runner()
    assert m._dirty_blocks_iteration(True, "main", "main") is True
    assert m._dirty_blocks_iteration(True, "rsi/x", "main") is False
    assert m._dirty_blocks_iteration(False, "main", "main") is False


def test_untracked_recovery_decision_helper():
    """The pure predicate that decides 'stage these on the branch' vs 'refuse+escalate'."""
    m = _load_runner()
    # all artifacts -> recover (stage)
    assert m._untracked_recovery_action(["AGENT_LOG.md", "capabilities/x/y.md"]) == "recover"
    # mixed -> refuse (operator work protected)
    assert m._untracked_recovery_action(["AGENT_LOG.md", "notes.txt"]) == "refuse"
    # all operator -> refuse
    assert m._untracked_recovery_action(["notes.txt", "new.py"]) == "refuse"
    # empty -> no-action (the clean path handles empty)
    assert m._untracked_recovery_action([]) == "none"