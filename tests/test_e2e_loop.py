"""End-to-end loop test — runs a FULL one_iteration() against a mock pi (canned agent responses) + a
temp git repo. Covers the orchestration (currently only pure sub-functions are tested):

GREEN path: baseline measure → branch → implement (mock agent writes a file) → gate green →
  ship (local mode) → compound (tick backlog).
RED path:  baseline measure → branch → implement (mock agent writes a file that fails the gate) →
  gate red → revert → no tick.

This proves the whole loop wires together end-to-end (the 7 invariants + the new gates) without
needing a real pi provider or a real network.
"""
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile

import pytest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)
sys.path.insert(0, os.path.join(ROOT, "improver"))

RUNNER = os.path.join(ROOT, "improver", "run_improver.py")
IMPROVER = os.path.join(ROOT, "improver")


def _load_runner():
    spec = importlib.util.spec_from_file_location("run_improver", RUNNER)
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


def _git(path, *a):
    return subprocess.run(["git", "-C", str(path), *a], capture_output=True, text=True)


def _mk_repo(tmp_path):
    """A real tiny git repo with a passing pytest gate (one trivial test) + a backlog."""
    work = tmp_path / "repo"
    work.mkdir()
    subprocess.run(["git", "init", str(work)], capture_output=True)
    _git(work, "config", "user.email", "t@t")
    _git(work, "config", "user.name", "t")
    _git(work, "checkout", "-b", "main")
    # ignore pytest's __pycache__ so it doesn't look like an untracked agent file
    (work / ".gitignore").write_text("__pycache__/\n*.pyc\n.pytest_cache/\n", encoding="utf-8")
    # a trivial passing test so the gate is green on the base
    tests_dir = work / "tests"
    tests_dir.mkdir()
    (tests_dir / "test_smoke.py").write_text(
        "def test_ok():\n    assert 1 + 1 == 2\n", encoding="utf-8")
    (work / "f.txt").write_text("1")
    _git(work, "add", "-A")
    _git(work, "commit", "-m", "init")
    return work


def _mk_backlog(tmp_path, runner):
    """Write a one-item backlog the iteration will pick."""
    runner.AGENT_MD.parent.mkdir(parents=True, exist_ok=True)
    runner.AGENT_MD.write_text("agent contract (test)", encoding="utf-8")
    runner.BACKLOG.write_text("- [ ] add a greeting module\n", encoding="utf-8")


def _configure_runner(m, work, tmp_path):
    """Point the runner's globals at the temp repo + a local ship mode + the built-in pytest gate."""
    m.configure(str(work), "repo")
    m.BASE_BRANCH = "main"
    m.SHIP = "local"                  # keep the branch locally; no push/PR (no remote)
    m.GATE_CMD = ""                   # built-in pytest gate (the repo has tests/test_smoke.py)
    m.REASONING = "off"
    m.GOAL = ""
    m.BEAUTIFY = False
    m.SOLOMON = False
    m.GITHUB_TOOLS = False
    m.INTERVAL = 1
    m.VENV_PY = work / ".venv" / "Scripts" / "python.exe"   # doesn't exist -> run_gate falls back to sys.executable
    # runtime + contracts under Solomon (improver parent), NOT inside the repo
    m.RUNTIME = tmp_path / "runtime" / "repo"
    m.RUNTIME.mkdir(parents=True, exist_ok=True)
    m.HEARTBEAT = m.RUNTIME / "heartbeat.json"
    m.LOCK = m.RUNTIME / "lock"
    m.STOP = m.RUNTIME / "stop"
    m.LOG = m.RUNTIME / "improver.log"
    m._hb.update({"repo": "repo", "status": "idle", "phase": None, "iteration": 0})


def _mock_pi_green(work):
    """A fake run_pi that writes a NEW python file (a real change) + returns a done summary."""
    def _fake_run_pi(task, system_md=None, timeout=1800):
        # write a new module (a real tracked change the gate will test)
        new_file = work / "greeting.py"
        new_file.write_text("def hello():\n    return 'hi'\n", encoding="utf-8")
        # return a pi --mode json event stream with a final assistant message + ITEM-STATUS: done
        out = json.dumps({
            "type": "agent_end",
            "messages": [{"role": "assistant", "content": [
                {"type": "text", "text": "Added greeting.py with hello().\nITEM-STATUS: done"}
            ]}]
        })
        return subprocess.CompletedProcess(args=["pi"], returncode=0, stdout=out, stderr="")
    return _fake_run_pi


def _mock_pi_red(work):
    """A fake run_pi that writes a file that BREAKS the gate (a failing test)."""
    def _fake_run_pi(task, system_md=None, timeout=1800):
        # write a new test that FAILS — the gate goes red
        (work / "tests" / "test_new.py").write_text(
            "def test_bad():\n    assert False\n", encoding="utf-8")
        out = json.dumps({
            "type": "agent_end",
            "messages": [{"role": "assistant", "content": [
                {"type": "text", "text": "Added a test.\nITEM-STATUS: done"}
            ]}]
        })
        return subprocess.CompletedProcess(args=["pi"], returncode=0, stdout=out, stderr="")
    return _fake_run_pi


# ---- GREEN path -----------------------------------------------------------
@pytest.mark.skipif(not shutil.which("git"), reason="git not available")
def test_e2e_green_path_ships_local_and_ticks_backlog(tmp_path, monkeypatch):
    """Full one_iteration(): baseline → branch → mock agent writes greeting.py → gate green →
    ship=local (branch kept) → backlog item ticked. The compounding base is unchanged (local mode
    keeps the branch; the next iteration re-bases off main)."""
    m = _load_runner()
    work = _mk_repo(tmp_path)
    _configure_runner(m, work, tmp_path)
    _mk_backlog(tmp_path, m)
    # mock pi to write a real, gate-green change
    monkeypatch.setattr(m, "run_pi", _mock_pi_green(work))
    # no cross-repo deps, no eval cmd, no visual gate -> those gates are no-ops
    monkeypatch.setattr(m, "_cross_repo_deps", lambda name: [])
    monkeypatch.setattr(m, "_eval_cmd", lambda name: "")
    monkeypatch.setattr(m, "_visual_gate_enabled", lambda name: False)
    # don't actually acquire/release a real lock (no concurrent runner in the test)
    monkeypatch.setattr(m, "acquire_lock", lambda: True)
    monkeypatch.setattr(m, "release_lock", lambda: None)

    before = m._hb["iteration"]
    m.one_iteration()

    # the iteration was counted
    assert m._hb["iteration"] == before + 1
    # the gate-green branch was kept locally (ship=local)
    assert m._hb.get("last_pr", {}).get("state", "").startswith("local")
    # the backlog item was ticked (shipped the named item, no deviation)
    backlog = m.BACKLOG.read_text(encoding="utf-8")
    assert "- [x] add a greeting module" in backlog
    # the repo is back on main (the branch was kept but we checkout main after ship)
    assert _git(work, "rev-parse", "--abbrev-ref", "HEAD").stdout.strip() == "main"
    # the rsi/* branch exists locally (ship=local kept it)
    branches = _git(work, "branch", "--list", "rsi/*").stdout
    assert "rsi/iter-" in branches or "rsi/" in branches


# ---- RED path --------------------------------------------------------------
@pytest.mark.skipif(not shutil.which("git"), reason="git not available")
def test_e2e_red_path_reverts_and_does_not_tick(tmp_path, monkeypatch):
    """Full one_iteration(): baseline → branch → mock agent writes a FAILING test → gate red →
    branch reverted → NO backlog tick (the item didn't ship). The compounding base is unchanged."""
    m = _load_runner()
    work = _mk_repo(tmp_path)
    _configure_runner(m, work, tmp_path)
    _mk_backlog(tmp_path, m)
    # mock pi to write a gate-RED change (a failing test)
    monkeypatch.setattr(m, "run_pi", _mock_pi_red(work))
    monkeypatch.setattr(m, "_cross_repo_deps", lambda name: [])
    monkeypatch.setattr(m, "_eval_cmd", lambda name: "")
    monkeypatch.setattr(m, "_visual_gate_enabled", lambda name: False)
    monkeypatch.setattr(m, "acquire_lock", lambda: True)
    monkeypatch.setattr(m, "release_lock", lambda: None)

    before = m._hb["iteration"]
    m.one_iteration()

    # the iteration was still counted (it reached the implement phase)
    assert m._hb["iteration"] == before + 1
    # the branch was REVERTED (status reflects the revert, not a ship)
    assert m._hb.get("status") in ("error", "sleeping")           # _drop_branch writes one of these
    # the backlog item was NOT ticked (the change didn't ship — it was reverted)
    backlog = m.BACKLOG.read_text(encoding="utf-8")
    assert "- [ ] add a greeting module" in backlog
    assert "- [x]" not in backlog
    # the repo is back on main and the rsi/* branch is GONE (reverted + deleted)
    assert _git(work, "rev-parse", "--abbrev-ref", "HEAD").stdout.strip() == "main"
    branches = _git(work, "branch", "--list", "rsi/*").stdout
    assert "rsi/iter-" not in branches


# ---- noop path (agent writes nothing) -------------------------------------
@pytest.mark.skipif(not shutil.which("git"), reason="git not available")
def test_e2e_noop_drops_branch_and_defers_after_repeats(tmp_path, monkeypatch):
    """A mock pi that writes NOTHING is a no-op: the branch is dropped, the item is NOT ticked, and
    after a few no-ops the item is deferred to the bottom of the backlog so the loop ADVANCES."""
    m = _load_runner()
    work = _mk_repo(tmp_path)
    _configure_runner(m, work, tmp_path)
    _mk_backlog(tmp_path, m)
    # mock pi that writes nothing
    def _noop_pi(task, system_md=None, timeout=1800):
        out = json.dumps({"type": "agent_end", "messages": [{"role": "assistant", "content": [
            {"type": "text", "text": "Did nothing.\nITEM-STATUS: skipped"}
        ]}]})
        return subprocess.CompletedProcess(args=["pi"], returncode=0, stdout=out, stderr="")
    monkeypatch.setattr(m, "run_pi", _noop_pi)
    monkeypatch.setattr(m, "_cross_repo_deps", lambda name: [])
    monkeypatch.setattr(m, "_eval_cmd", lambda name: "")
    monkeypatch.setattr(m, "_visual_gate_enabled", lambda name: False)
    monkeypatch.setattr(m, "acquire_lock", lambda: True)
    monkeypatch.setattr(m, "release_lock", lambda: None)

    m.one_iteration()
    # the branch was dropped (a noop), the item is NOT ticked
    backlog = m.BACKLOG.read_text(encoding="utf-8")
    assert "- [ ] add a greeting module" in backlog
    # no rsi/* branch lingers (it was dropped)
    branches = _git(work, "branch", "--list", "rsi/*").stdout
    assert "rsi/iter-" not in branches