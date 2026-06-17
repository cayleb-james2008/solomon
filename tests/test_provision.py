"""Tests for the deterministic provisioner, the start() provisioning interlock, and the
runner --provision parser. No network / gh / real pi."""
import importlib.util
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


def test_runner_clean_env_strips_github_tokens(monkeypatch):
    m = _load_runner()
    monkeypatch.setenv("GITHUB_TOKEN", "bad")
    monkeypatch.setenv("GH_TOKEN", "bad2")
    env = m._clean_env()
    assert "GITHUB_TOKEN" not in env and "GH_TOKEN" not in env and "PYTHONPATH" not in env
