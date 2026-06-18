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


def test_render_contract_weights_goal_when_set(tmp_path, monkeypatch):
    r = _repo(tmp_path, monkeypatch, name="g", has_remote=True,
              goal="Grow followers, engagement, and revenue fully autonomously")
    agent, _ = control.render_default_contract(r)
    assert "## North-star goal" in agent and "Grow followers, engagement, and revenue" in agent
    assert agent.index("North-star goal") < agent.index("Your job this run")   # leads the contract
    assert control.project_goal(r) == "Grow followers, engagement, and revenue fully autonomously"
    # no goal -> no goal section
    agent2, _ = control.render_default_contract(_repo(tmp_path, monkeypatch, name="g2"))
    assert "North-star goal" not in agent2 and control.project_goal({"name": "x"}) == ""


def test_build_task_weaves_north_star_goal():
    m = _load_runner()
    m.GOAL = "More followers and creative monetization"
    t = m.build_task("Add a test for the strategy module")
    assert "NORTH-STAR GOAL" in t and "More followers and creative monetization" in t
    assert "Add a test for the strategy module" in t
    m.GOAL = ""
    assert "NORTH-STAR" not in m.build_task("Add a test")


def test_strip_tier_and_top_item_tier(tmp_path, monkeypatch):
    m = _load_runner()
    assert m._strip_tier("[architecture] Build the genesis API") == ("Build the genesis API", "architecture")
    assert m._strip_tier("Add a test") == ("Add a test", "chore")          # untagged -> chore (legacy-safe)
    bl = tmp_path / "backlog.md"
    bl.write_text("# b\n\n- [ ] [feature] Add live chat actions\n- [ ] Add a test\n", encoding="utf-8")
    monkeypatch.setattr(m, "BACKLOG", bl)
    assert m._top_backlog_item() == ("Add live chat actions", "feature")


def test_build_task_tier_lifts_smallest_change_ceiling():
    m = _load_runner()
    m.GOAL = ""
    chore = m.build_task("do x", "chore")
    arch = m.build_task("do x", "architecture")
    assert "SMALLEST coherent change" in chore and "regression" in chore
    assert "SIZE THE CHANGE TO THE OPPORTUNITY" in arch and "ambitious" in arch.lower()


def test_parse_ideas_sorts_by_leverage_and_drops_chores():
    m = _load_runner()
    out = ("[feature] | 3 | medium idea — why: ok\n"
           "[architecture] | 5 | big idea — why: unlocks the goal\n"
           "[chore] | 5 | add a test — why: nope\n"          # chore tier not allowed -> dropped
           "noise line\n"
           "[refactor] | 4 | refactor idea — why: cleaner\n")
    ideas = m._parse_ideas(out)
    assert [t for _l, t, _i in ideas] == ["architecture", "refactor", "feature"]   # sorted desc, no chore
    assert ideas[0][2].startswith("big idea")


def test_ideate_prepends_ambitious_items(tmp_path, monkeypatch):
    m = _load_runner()
    bl = tmp_path / "backlog.md"
    bl.write_text("# sover backlog\n\n- [ ] Add a test\n", encoding="utf-8")
    monkeypatch.setattr(m, "BACKLOG", bl)
    canned = "[architecture] | 5 | Build the genesis profile-create API — why: unlocks the goal\n[feature] | 3 | Add chat read-context — why: real numbers\n"
    monkeypatch.setattr(m, "run_pi", lambda task, system_md=None, timeout=600: type("R", (), {"stdout": canned})())
    monkeypatch.setattr(m, "final_text", lambda s: s)
    assert m.ideate() == 0
    txt = bl.read_text(encoding="utf-8")
    lines = [l for l in txt.splitlines() if l.startswith("- [ ]")]
    assert lines[0] == "- [ ] [architecture] Build the genesis profile-create API — why: unlocks the goal"  # highest leverage first
    assert lines[-1] == "- [ ] Add a test"                                          # legacy chore kept below
    assert txt.splitlines()[0] == "# sover backlog"                                 # header preserved


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
    assert m._top_backlog_item()[0] == "second item"   # loop now advances
    m._mark_backlog_done("nonexistent")              # no-op, no crash
    assert bl.read_text(encoding="utf-8") == txt


def test_note_deviation_defers_item_after_limit(tmp_path, monkeypatch):
    # the agent keeps shipping something OTHER than the named item -> after `limit` deviations the
    # item is deferred to the bottom so the loop advances (instead of re-shipping unrelated PRs).
    m = _load_runner()
    bl = tmp_path / "backlog.md"
    bl.write_text("# x backlog\n\n- [ ] target item\n- [ ] next item\n", encoding="utf-8")
    monkeypatch.setattr(m, "BACKLOG", bl)
    m._deviation_counts.clear()
    for _ in range(2):
        m._note_deviation("target item", limit=3)
    assert m._top_backlog_item()[0] == "target item"        # not deferred yet (< limit)
    m._note_deviation("target item", limit=3)               # 3rd deviation -> defer
    txt = bl.read_text(encoding="utf-8")
    assert "(deferred" in txt and m._top_backlog_item()[0] == "next item"


def test_split_item_status_marks_deviation_and_strips_marker():
    m = _load_runner()
    s, dev = m._split_item_status("Implemented the named item.\nITEM-STATUS: done")
    assert not dev and "ITEM-STATUS" not in s and s == "Implemented the named item."
    s2, dev2 = m._split_item_status("Fixed an unrelated route bug instead.\nITEM-STATUS: deviated")
    assert dev2 and "ITEM-STATUS" not in s2
    s3, dev3 = m._split_item_status("No marker here.")
    assert not dev3 and s3 == "No marker here."          # missing marker -> assume done (progress-biased)


def test_note_noop_defers_stuck_item_after_three_tries(tmp_path, monkeypatch):
    m = _load_runner()
    bl = tmp_path / "backlog.md"
    bl.write_text("# b\n\n- [ ] hard item\n- [ ] easy item\n", encoding="utf-8")
    monkeypatch.setattr(m, "BACKLOG", bl)
    monkeypatch.setattr(m, "log", lambda *a, **k: None)
    m.BEAUTIFY = False
    m.SOLOMON = False
    m._noop_counts.clear()
    assert m._top_backlog_item()[0] == "hard item"
    m._note_noop("hard item")
    m._note_noop("hard item")
    assert m._top_backlog_item()[0] == "hard item"          # 2 noops — not deferred yet
    m._note_noop("hard item")                            # 3rd noop — deferred to the bottom
    assert m._top_backlog_item()[0] == "easy item"
    assert "deferred" in bl.read_text(encoding="utf-8")


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


def test_ship_succeeded_distinguishes_real_ship_from_failure():
    m = _load_runner()
    assert m._ship_succeeded({"number": 5})                                    # plain opened PR (pr-mode)
    assert m._ship_succeeded({"number": None, "state": "local branch (unshipped)"})
    assert m._ship_succeeded({"number": None, "state": "local (no remote)"})
    assert not m._ship_succeeded({"number": None, "state": "push-failed"})
    assert not m._ship_succeeded({"number": None, "state": "local (ship pending gh auth)"})
    # auto-merge: a LANDED/in-flight PR ticks; an un-merged red/awaiting/stopped PR does NOT
    assert m._ship_succeeded({"number": 7, "state": "merged"})
    assert m._ship_succeeded({"number": 7, "state": "auto-merge queued (awaiting CI)"})
    assert not m._ship_succeeded({"number": 7, "state": "open (CI red — not merged)"})
    assert not m._ship_succeeded({"number": 7, "state": "open (awaiting CI)"})
    assert not m._ship_succeeded({"number": 7, "state": "open (stopped before merge)"})
    # ship=push: only a VERIFIED push counts (else the loop re-pushes the same branch forever)
    assert m._ship_succeeded({"number": None, "state": "pushed (no PR)", "verified": True})
    assert not m._ship_succeeded({"number": None, "state": "pushed (unverified)", "verified": False})


def _fake_run(cmds):
    def run(args, **k):
        cmds.append(args)
        return type("P", (), {"returncode": 0, "stdout": "", "stderr": ""})()
    return run


def test_auto_merge_blocks_on_ci_red(monkeypatch):
    m = _load_runner()
    cmds = []
    monkeypatch.setattr(m.subprocess, "run", _fake_run(cmds))
    monkeypatch.setattr(m, "gh_exe", lambda: "gh")
    pr = m._auto_merge({"number": 7, "checks": "failure"})
    assert "CI red" in pr["state"] and cmds == []          # never invoked gh merge


def test_auto_merge_queues_on_pending(monkeypatch):
    m = _load_runner()
    cmds = []
    monkeypatch.setattr(m.subprocess, "run", _fake_run(cmds))
    monkeypatch.setattr(m, "gh_exe", lambda: "gh")
    pr = m._auto_merge({"number": 7, "checks": "pending"})
    assert "queued" in pr["state"] and any("--auto" in c for c in cmds)


def test_auto_merge_merges_on_success(monkeypatch):
    m = _load_runner()
    cmds = []
    monkeypatch.setattr(m.subprocess, "run", _fake_run(cmds))
    monkeypatch.setattr(m, "gh_exe", lambda: "gh")
    pr = m._auto_merge({"number": 7, "checks": "success"})
    assert pr["state"] == "merged" and any("--squash" in c and "--auto" not in c for c in cmds)


def test_runner_clean_env_strips_github_tokens(monkeypatch):
    m = _load_runner()
    monkeypatch.setenv("GITHUB_TOKEN", "bad")
    monkeypatch.setenv("GH_TOKEN", "bad2")
    env = m._clean_env()
    assert "GITHUB_TOKEN" not in env and "GH_TOKEN" not in env and "PYTHONPATH" not in env
