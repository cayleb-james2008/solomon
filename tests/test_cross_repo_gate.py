"""Tests for the cross-repo correlated test gate (user complaint: 'the loop misses correlated tests
or other things when making changes' — across repos that share a module).

A repo may declare `cross_repo_deps` in repos.json: a list of repo names whose gate should run when
THIS repo changes a shared module. After the primary gate passes (green + anti-gaming clean), for each
declared dep repo, run its gate in the dep repo's cwd. Any red = revert the branch (same as primary
gate red). Anti-gaming applies to cross-repo gates too (pass/collected counts must not drop vs the
dep repo's baseline). If `cross_repo_deps` is absent/empty, behavior is unchanged (backward compatible).
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


# ---- cross_repo_deps resolution from repos.json ----------------------------
def test_cross_repo_deps_missing_returns_empty(tmp_path, monkeypatch):
    """A repo row without `cross_repo_deps` yields [] (backward compatible — no cross-repo gate)."""
    m = _load_runner()
    repos_json = tmp_path / "repos.json"
    repos_json.write_text(json.dumps([{"name": "a", "path": str(tmp_path / "a")}]), encoding="utf-8")
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    assert m._cross_repo_deps("a") == []


def test_cross_repo_deps_resolves_names_to_paths(tmp_path, monkeypatch):
    """`cross_repo_deps: ["maki"]` resolves the named deps to their repo dicts (with paths) from
    repos.json so the runner can cd into them."""
    m = _load_runner()
    repos_json = tmp_path / "repos.json"
    a_path = tmp_path / "a"
    b_path = tmp_path / "b"
    a_path.mkdir()
    b_path.mkdir()
    repos_json.write_text(json.dumps([
        {"name": "a", "path": str(a_path), "cross_repo_deps": ["b"]},
        {"name": "b", "path": str(b_path), "gate": "echo ok"},
    ]), encoding="utf-8")
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    deps = m._cross_repo_deps("a")
    assert len(deps) == 1
    assert deps[0]["name"] == "b"
    assert deps[0]["path"] == str(b_path)


def test_cross_repo_deps_unknown_name_skipped(tmp_path, monkeypatch):
    """A dep name that doesn't exist in repos.json is silently skipped (no crash — best-effort)."""
    m = _load_runner()
    repos_json = tmp_path / "repos.json"
    a_path = tmp_path / "a"
    a_path.mkdir()
    repos_json.write_text(json.dumps([
        {"name": "a", "path": str(a_path), "cross_repo_deps": ["ghost", "b"]},
        {"name": "b", "path": str(tmp_path / "b")},
    ]), encoding="utf-8")
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    deps = m._cross_repo_deps("a")
    assert [d["name"] for d in deps] == ["b"]


def test_cross_repo_deps_self_excluded(tmp_path, monkeypatch):
    """A repo listing itself in cross_repo_deps is excluded (the primary gate already covers it)."""
    m = _load_runner()
    repos_json = tmp_path / "repos.json"
    a_path = tmp_path / "a"
    a_path.mkdir()
    repos_json.write_text(json.dumps([
        {"name": "a", "path": str(a_path), "cross_repo_deps": ["a", "b"]},
        {"name": "b", "path": str(tmp_path / "b")},
    ]), encoding="utf-8")
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    deps = m._cross_repo_deps("a")
    assert [d["name"] for d in deps] == ["b"]


# ---- cross-repo gate execution + anti-gaming -------------------------------
def _git(path, *a):
    return subprocess.run(["git", "-C", str(path), *a], capture_output=True, text=True)


def _mk_repo(path):
    """A real tiny git repo with a passing pytest gate (one trivial test)."""
    path.mkdir(parents=True, exist_ok=True)
    subprocess.run(["git", "init", str(path)], capture_output=True)
    _git(path, "config", "user.email", "t@t")
    _git(path, "config", "user.name", "t")
    _git(path, "checkout", "-b", "main")
    (path / "f.txt").write_text("1")
    _git(path, "add", "-A")
    _git(path, "commit", "-m", "init")
    return path


@pytest.mark.skipif(not shutil.which("git"), reason="git not available")
def test_run_cross_repo_gates_green_passes(tmp_path, monkeypatch):
    """When all declared dep gates are green, _run_cross_repo_gates returns ok=True with the per-repo
    results recorded (and nothing is reverted)."""
    m = _load_runner()
    a = _mk_repo(tmp_path / "a")
    b = _mk_repo(tmp_path / "b")
    repos_json = tmp_path / "repos.json"
    repos_json.write_text(json.dumps([
        {"name": "a", "path": str(a), "cross_repo_deps": ["b"]},
        {"name": "b", "path": str(b), "gate": "exit 0"},
    ]), encoding="utf-8")
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    monkeypatch.setattr(m, "REPO", a)
    monkeypatch.setattr(m, "NAME", "a")
    results = {}
    res = m._run_cross_repo_gates(results)
    assert res["ok"] is True
    assert "b" in res["results"]
    assert res["results"]["b"]["green"] is True


@pytest.mark.skipif(not shutil.which("git"), reason="git not available")
def test_run_cross_repo_gates_red_reverts(tmp_path, monkeypatch):
    """When a declared dep gate is red, _run_cross_repo_gates returns ok=False with the failing repo,
    so the caller reverts the branch (same as a primary gate red)."""
    m = _load_runner()
    a = _mk_repo(tmp_path / "a")
    b = _mk_repo(tmp_path / "b")
    repos_json = tmp_path / "repos.json"
    repos_json.write_text(json.dumps([
        {"name": "a", "path": str(a), "cross_repo_deps": ["b"]},
        {"name": "b", "path": str(b), "gate": "exit 1"},      # always red
    ]), encoding="utf-8")
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    monkeypatch.setattr(m, "REPO", a)
    monkeypatch.setattr(m, "NAME", "a")
    res = m._run_cross_repo_gates({})
    assert res["ok"] is False
    assert res["failed_repo"] == "b"
    assert res["results"]["b"]["green"] is False


def test_run_cross_repo_gates_no_deps_is_noop(tmp_path, monkeypatch):
    """No cross_repo_deps declared -> _run_cross_repo_gates returns ok=True with empty results
    (backward compatible — the primary gate alone decides)."""
    m = _load_runner()
    a = _mk_repo(tmp_path / "a")
    repos_json = tmp_path / "repos.json"
    repos_json.write_text(json.dumps([{"name": "a", "path": str(a)}]), encoding="utf-8")
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    monkeypatch.setattr(m, "REPO", a)
    monkeypatch.setattr(m, "NAME", "a")
    res = m._run_cross_repo_gates({})
    assert res["ok"] is True
    assert res["results"] == {}


def test_cross_repo_gate_uses_dep_gate_command(tmp_path, monkeypatch):
    """The cross-repo gate runs the DEP repo's own gate command (from its repos.json row), not the
    primary repo's gate — so a shared-module change is validated by the consumer's actual test suite."""
    m = _load_runner()
    a = _mk_repo(tmp_path / "a")
    b = _mk_repo(tmp_path / "b")
    repos_json = tmp_path / "repos.json"
    repos_json.write_text(json.dumps([
        {"name": "a", "path": str(a), "gate": "exit 1", "cross_repo_deps": ["b"]},   # a's own gate is red
        {"name": "b", "path": str(b), "gate": "exit 0"},                             # b's gate is green
    ]), encoding="utf-8")
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    monkeypatch.setattr(m, "REPO", a)
    monkeypatch.setattr(m, "NAME", "a")
    res = m._run_cross_repo_gates({})
    assert res["ok"] is True                       # the cross-repo gate used b's (green) gate, not a's


# ---- history records the cross-repo outcome on revert (MEDIUM review finding) --------------
# Bug: the cross-repo revert path built a `_hb_cross` dict (with cross_repo_gates results) but
# never passed it to _record_history, so the reverted history line lost the cross-repo diagnostic.
# Fix: _record_history gained an optional `extra` kwarg that MERGES into the record; the revert
# path threads the cross-repo results in via extra={"cross_repo_gates": ...}.


def test_record_history_merges_extra_dict(tmp_path, monkeypatch):
    """_record_history(status, branch, summary, extra={...}) MERGES the extra fields into the history
    line so the cross-repo gate results (and any other per-outcome diagnostic) land in history.jsonl.
    Existing callers that omit `extra` are unchanged."""
    m = _load_runner()
    rt = tmp_path / "rt"
    monkeypatch.setattr(m, "RUNTIME", rt)
    m._hb["iteration"] = 7
    m._hb["tests"] = {"passed": 5, "failed": 0}
    m._hb["last_pr"] = None

    cross = {"b": {"green": False, "tests": {"passed": 0, "failed": 1}, "tail": "boom"}}
    m._record_history("reverted", "rsi/iter-x", "Reverted — cross-repo gate RED on dep 'b'.",
                      extra={"cross_repo_gates": cross})

    import json as _json
    lines = (rt / "history.jsonl").read_text(encoding="utf-8").splitlines()
    assert len(lines) == 1
    rec = _json.loads(lines[0])
    assert rec["status"] == "reverted"
    assert rec["branch"] == "rsi/iter-x"
    assert rec["iteration"] == 7
    assert rec["tests"] == {"passed": 5, "failed": 0}
    # the diagnostic must be present on the reverted line (the fix — the bug dropped it)
    assert rec["cross_repo_gates"] == cross
    assert rec["cross_repo_gates"]["b"]["green"] is False


def test_record_history_unchanged_when_extra_omitted(tmp_path, monkeypatch):
    """Backward compatibility: callers that omit `extra` produce the same record shape as before
    (no extra key, no crash)."""
    m = _load_runner()
    rt = tmp_path / "rt"
    monkeypatch.setattr(m, "RUNTIME", rt)
    m._hb["iteration"] = 1

    m._record_history("shipped", "rsi/iter-y", "landed")

    import json as _json
    rec = _json.loads((rt / "history.jsonl").read_text(encoding="utf-8").strip())
    assert rec["status"] == "shipped"
    assert rec["branch"] == "rsi/iter-y"
    assert "cross_repo_gates" not in rec           # no extra -> no new key


def test_cross_repo_revert_records_gates_in_history(tmp_path, monkeypatch):
    """End-to-end of the revert path's history wiring: a cross-repo gate RED must record the per-dep
    results on the 'reverted' history line (the live bug — _hb_cross was built but never used)."""
    m = _load_runner()
    a = _mk_repo(tmp_path / "a")
    b = _mk_repo(tmp_path / "b")
    repos_json = tmp_path / "repos.json"
    repos_json.write_text(json.dumps([
        {"name": "a", "path": str(a), "cross_repo_deps": ["b"]},
        {"name": "b", "path": str(b), "gate": "exit 1"},      # dep always red -> revert path
    ]), encoding="utf-8")
    rt = tmp_path / "rt"
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    monkeypatch.setattr(m, "REPO", a)
    monkeypatch.setattr(m, "NAME", "a")
    monkeypatch.setattr(m, "RUNTIME", rt)

    # drive the exact revert path: run the cross-repo gates, then record history the way the
    # runner does on a RED dep (this mirrors lines ~1693-1704 of run_improver.py).
    xrec = {}
    xres = m._run_cross_repo_gates(xrec)
    assert xres["ok"] is False and xres["failed_repo"] == "b"
    summary = f"Reverted — cross-repo gate RED on dep 'b'. summary"
    m._record_history("reverted", "rsi/iter-z", summary,
                      extra={"cross_repo_gates": xrec.get("cross_repo_gates", {})})

    import json as _json
    rec = _json.loads((rt / "history.jsonl").read_text(encoding="utf-8").strip())
    assert rec["status"] == "reverted"
    assert "b" in rec["cross_repo_gates"]
    assert rec["cross_repo_gates"]["b"]["green"] is False   # the failing dep's outcome survived