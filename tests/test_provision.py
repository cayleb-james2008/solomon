"""Tests for the deterministic provisioner, the start() provisioning interlock, and the
runner --provision parser. No network / gh / real pi."""
import importlib.util
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import control  # noqa: E402

RUNNER = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                      "improver", "run_improver.py")


def _load_runner():
    spec = importlib.util.spec_from_file_location("run_improver", RUNNER)
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


def _repo(tmp_path, monkeypatch, name="demo", **extra):
    monkeypatch.setattr(control, "HERE", str(tmp_path))   # contracts -> tmp/improver/<name>/
    r = {"name": name, "path": str(tmp_path)}
    r.update(extra)
    return r


# ---- stack detection -------------------------------------------------------
def test_detect_stack_python(tmp_path):
    open(os.path.join(tmp_path, "pyproject.toml"), "w").close()
    st = control._detect_stack({"name": "x", "path": str(tmp_path)})
    assert st["lang"] == "python" and "pytest" in st["test_cmd"]


def test_detect_stack_node(tmp_path):
    open(os.path.join(tmp_path, "package.json"), "w").close()
    st = control._detect_stack({"name": "x", "path": str(tmp_path)})
    assert st["lang"] == "node" and st["test_cmd"] == "npm test"


def _make_venv_and_tests(tmp_path):
    """Create an OS-appropriate .venv python stub + a tests/ dir, with NO manifest."""
    if os.name == "nt":
        vd = tmp_path / ".venv" / "Scripts"
        vd.mkdir(parents=True)
        (vd / "python.exe").write_text("", encoding="utf-8")
    else:
        vd = tmp_path / ".venv" / "bin"
        vd.mkdir(parents=True)
        (vd / "python").write_text("", encoding="utf-8")
    (tmp_path / "tests").mkdir()


def test_detect_stack_unittest_fallback(tmp_path):
    # venv + tests/ but no manifest (e.g. sover) -> python with unittest discovery
    _make_venv_and_tests(tmp_path)
    st = control._detect_stack({"name": "x", "path": str(tmp_path)})
    assert st["lang"] == "python"
    assert "unittest discover -s tests -t tests" in st["test_cmd"]


def test_detect_stack_pytest_when_conftest(tmp_path):
    open(os.path.join(tmp_path, "pyproject.toml"), "w").close()
    open(os.path.join(tmp_path, "conftest.py"), "w").close()
    st = control._detect_stack({"name": "x", "path": str(tmp_path)})
    assert st["lang"] == "python" and st["test_cmd"].endswith("-m pytest")


# ---- rendering -------------------------------------------------------------
def test_render_default_contract_sections_no_placeholders(tmp_path, monkeypatch):
    r = _repo(tmp_path, monkeypatch, name="alpha", has_remote=True)
    agent, backlog = control.render_default_contract(r)
    for h in ("# alpha self-improvement contract", "## Your job this run", "## Rules",
              "## Map of the code", "Do NOT run git"):
        assert h in agent
    assert "github_" in agent                      # github-tools paragraph present when has_remote
    assert "{" not in agent and "}" not in agent   # no unresolved format placeholders
    assert backlog.strip().startswith("# alpha backlog")


def test_render_no_github_para_when_local(tmp_path, monkeypatch):
    r = _repo(tmp_path, monkeypatch, name="loc", has_remote=False)
    agent, _ = control.render_default_contract(r)
    assert "github_" not in agent


def test_render_backlog_items_parseable(tmp_path, monkeypatch):
    _, backlog = control.render_default_contract(_repo(tmp_path, monkeypatch))
    items = [ln for ln in backlog.splitlines() if ln.strip().startswith("- [ ]")]
    assert len(items) >= 3


# ---- ensure_contracts ------------------------------------------------------
def test_contracts_present_and_ensure_roundtrip(tmp_path, monkeypatch):
    r = _repo(tmp_path, monkeypatch, name="x")
    assert control.contracts_present(r) == {"agent": False, "backlog": False}
    res = control.ensure_contracts(r)
    assert res["ok"] and set(res["created"]) == {"AGENT.md", "backlog.md"}
    assert control.contracts_present(r) == {"agent": True, "backlog": True}
    assert control.ensure_contracts(r) == {"ok": True, "created": []}   # idempotent


def test_ensure_contracts_does_not_overwrite(tmp_path, monkeypatch):
    r = _repo(tmp_path, monkeypatch, name="x")
    assert control.write_contract(r, "agent", "# custom contract\n")["ok"]
    res = control.ensure_contracts(r)
    assert res["created"] == ["backlog.md"]                              # only the missing one
    assert control.read_contract(r, "agent")["text"] == "# custom contract\n"   # preserved


def test_ensure_contracts_sets_gate_from_detection(tmp_path, monkeypatch):
    # provisioning must leave the loop with a runnable gate, not the failing pytest fallback
    monkeypatch.setattr(control, "HERE", str(tmp_path))
    repos = tmp_path / "repos.json"
    repos.write_text("[]", encoding="utf-8")
    monkeypatch.setattr(control, "REPOS_JSON", str(repos))
    monkeypatch.setattr(control, "PROJECTS_DIR", str(tmp_path / "projects"))
    _make_venv_and_tests(tmp_path)
    res = control.ensure_contracts({"name": "demo", "path": str(tmp_path)})
    assert res["ok"] and "unittest discover" in res.get("gate_set", "")
    saved = next(x for x in json.loads(repos.read_text(encoding="utf-8")) if x["name"] == "demo")
    assert "unittest discover" in (saved.get("gate") or "")


def test_ensure_contracts_keeps_operator_gate(tmp_path, monkeypatch):
    monkeypatch.setattr(control, "HERE", str(tmp_path))
    monkeypatch.setattr(control, "REPOS_JSON", str(tmp_path / "repos.json"))
    _make_venv_and_tests(tmp_path)
    res = control.ensure_contracts({"name": "demo", "path": str(tmp_path), "gate": "make test"})
    assert "gate_set" not in res                                         # operator gate untouched


# ---- start() provisions before spawning the loop ---------------------------
def test_start_provisions_before_spawn(tmp_path, monkeypatch):
    monkeypatch.setattr(control, "HERE", str(tmp_path))
    os.makedirs(os.path.join(tmp_path, "improver"), exist_ok=True)
    open(os.path.join(tmp_path, "improver", "run_improver.py"), "w").close()
    fake_py = os.path.join(tmp_path, "py.exe")
    open(fake_py, "w").close()
    monkeypatch.setattr(control, "_venv_python", lambda repo: fake_py)
    monkeypatch.setattr(control, "is_running", lambda repo: False)
    seen = {}

    class _Proc:
        pid = 4321

    def _popen(args, **kw):
        seen["agent_exists"] = os.path.exists(os.path.join(tmp_path, "improver", "x", "AGENT.md"))
        return _Proc()

    monkeypatch.setattr(control.subprocess, "Popen", _popen)
    res = control.start({"name": "x", "path": str(tmp_path)})
    assert res["ok"] and res["pid"] == 4321
    assert seen.get("agent_exists") is True            # contract existed at spawn time


# ---- runner --provision parser + writer ------------------------------------
def test_parse_provision_blocks():
    m = _load_runner()
    text = "junk\n===AGENT.md===\n# hi\nbody\n===backlog.md===\n- [ ] a\n- [ ] b\n"
    a, b = m._parse_provision(text)
    assert a.startswith("# hi") and "- [ ] a" in b
    assert m._parse_provision("no blocks here") == (None, None)


def test_provision_writes_files(tmp_path, monkeypatch):
    m = _load_runner()
    monkeypatch.setattr(m, "AGENT_MD", tmp_path / "AGENT.md")
    monkeypatch.setattr(m, "BACKLOG", tmp_path / "backlog.md")
    canned = "===AGENT.md===\n# generated\n===backlog.md===\n- [ ] do a thing\n"
    monkeypatch.setattr(m, "run_pi", lambda task, system_md=None, timeout=600: type("R", (), {"stdout": canned})())
    monkeypatch.setattr(m, "final_text", lambda s: s)
    assert m.provision() == 0
    assert (tmp_path / "AGENT.md").read_text(encoding="utf-8").startswith("# generated")
    assert "- [ ] do a thing" in (tmp_path / "backlog.md").read_text(encoding="utf-8")


def test_provision_parse_miss_returns_5(tmp_path, monkeypatch):
    m = _load_runner()
    monkeypatch.setattr(m, "AGENT_MD", tmp_path / "AGENT.md")
    monkeypatch.setattr(m, "BACKLOG", tmp_path / "backlog.md")
    monkeypatch.setattr(m, "run_pi", lambda task, system_md=None, timeout=600: type("R", (), {"stdout": "no blocks"})())
    monkeypatch.setattr(m, "final_text", lambda s: s)
    assert m.provision() == 5


def test_enrich_contract_background_spawn(tmp_path, monkeypatch):
    monkeypatch.setattr(control, "HERE", str(tmp_path))
    os.makedirs(os.path.join(tmp_path, "improver"), exist_ok=True)
    open(os.path.join(tmp_path, "improver", "run_improver.py"), "w").close()
    monkeypatch.setattr(control, "keys_status", lambda: {"ollama-cloud": True})
    monkeypatch.setattr(control, "_venv_python", lambda repo: sys.executable)
    calls = []
    monkeypatch.setattr(control.subprocess, "Popen",
                        lambda args, **kw: (calls.append(args), type("P", (), {"pid": 1})())[1])
    r = control.enrich_contract({"name": "x", "path": str(tmp_path), "provider": "ollama-cloud"}, background=True)
    assert r["ok"] and r.get("started") and calls and "--provision" in calls[0]


def test_mark_backlog_done_ticks_item_and_advances(tmp_path, monkeypatch):
    m = _load_runner()
    bl = tmp_path / "backlog.md"
    bl.write_text("# x backlog\n\n- [ ] first item\n- [ ] second item\n", encoding="utf-8")
    monkeypatch.setattr(m, "BACKLOG", bl)
    m._mark_backlog_done("first item")
    txt = bl.read_text(encoding="utf-8")
    assert "- [x] first item" in txt and "- [ ] second item" in txt
    assert m._top_backlog_item() == "second item"   # loop now advances
    m._mark_backlog_done("nonexistent")              # no-op, no crash
    assert bl.read_text(encoding="utf-8") == txt


def test_pr_title_prefers_goal_over_summary():
    m = _load_runner()
    # concise backlog goal wins, not the verbose summary
    assert m._pr_title("Add a roundtrip test for save_strategy()", "Added a TestSaveStrategy class with two tests that ...") \
        == "Add a roundtrip test for save_strategy()"
    # generic placeholder goal -> fall back to the summary's first line
    assert m._pr_title("model-chosen improvement", "Tightened error handling\nmore detail") == "Tightened error handling"
    assert len(m._pr_title("x" * 200)) == 72


def test_run_gate_parses_unittest_pass(monkeypatch):
    m = _load_runner()
    m.GATE_CMD = "dummy"  # take the custom-gate branch
    out = ".......\n----------\nRan 71 tests in 0.71s\n\nOK\n"
    monkeypatch.setattr(m.subprocess, "run",
                        lambda *a, **k: type("P", (), {"returncode": 0, "stdout": out, "stderr": ""})())
    green, tests, _ = m.run_gate()
    assert green and tests["passed"] == 71 and tests["failed"] == 0 and tests["errors"] == 0


def test_run_gate_parses_unittest_failures(monkeypatch):
    m = _load_runner()
    m.GATE_CMD = "dummy"
    out = "Ran 10 tests in 0.10s\n\nFAILED (failures=2, errors=1)\n"
    monkeypatch.setattr(m.subprocess, "run",
                        lambda *a, **k: type("P", (), {"returncode": 1, "stdout": out, "stderr": ""})())
    green, tests, _ = m.run_gate()
    assert not green and tests["passed"] == 7 and tests["failed"] == 2 and tests["errors"] == 1


def test_runner_clean_env_strips_github_tokens(monkeypatch):
    m = _load_runner()
    monkeypatch.setenv("GITHUB_TOKEN", "bad")
    monkeypatch.setenv("GH_TOKEN", "bad2")
    env = m._clean_env()
    assert "GITHUB_TOKEN" not in env and "GH_TOKEN" not in env and "PYTHONPATH" not in env
