"""Tests for control.py that don't require gh/network."""
import importlib.util
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import control  # noqa: E402


def _runtime(tmp_path, monkeypatch, name="x"):
    """Point control's runtime root at tmp_path and return the per-repo runtime dir."""
    monkeypatch.setattr(control, "HERE", str(tmp_path))
    rt = tmp_path / "runtime" / name
    rt.mkdir(parents=True)
    return rt


def test_read_heartbeat_missing(tmp_path, monkeypatch):
    _runtime(tmp_path, monkeypatch)
    assert control.read_heartbeat({"name": "x", "path": str(tmp_path)}) is None


def test_read_heartbeat_valid(tmp_path, monkeypatch):
    rt = _runtime(tmp_path, monkeypatch)
    (rt / "heartbeat.json").write_text(
        json.dumps({"repo": "x", "status": "iterating", "iteration": 3}), encoding="utf-8")
    hb = control.read_heartbeat({"name": "x", "path": str(tmp_path)})
    assert isinstance(hb, dict) and hb["status"] == "iterating" and hb["iteration"] == 3


def test_read_heartbeat_corrupt(tmp_path, monkeypatch):
    rt = _runtime(tmp_path, monkeypatch)
    (rt / "heartbeat.json").write_text("{not valid json", encoding="utf-8")
    assert control.read_heartbeat({"name": "x", "path": str(tmp_path)}) is None


def test_load_repos(tmp_path, monkeypatch):
    # isolate discovery so the live workspace/projects folder doesn't leak in
    monkeypatch.setattr(control, "PROJECTS_DIR", str(tmp_path / "noproj"))
    p = tmp_path / "repos.json"
    p.write_text(json.dumps([{"name": "maki", "path": "C:/x", "branch_prefix": "rsi/"}]),
                 encoding="utf-8")
    repos = control.load_repos(str(p))
    assert len(repos) == 1 and repos[0]["name"] == "maki" and repos[0]["branch_prefix"] == "rsi/"


def test_load_repos_missing(tmp_path, monkeypatch):
    monkeypatch.setattr(control, "PROJECTS_DIR", str(tmp_path / "noproj"))
    assert control.load_repos(str(tmp_path / "nope.json")) == []


def test_is_running_bogus_pid(tmp_path, monkeypatch):
    rt = _runtime(tmp_path, monkeypatch)
    (rt / "lock").write_text("999999", encoding="utf-8")  # a PID that is almost certainly dead
    assert control.is_running({"name": "x", "path": str(tmp_path)}) is False


def test_is_running_no_lock(tmp_path, monkeypatch):
    _runtime(tmp_path, monkeypatch)
    assert control.is_running({"name": "x", "path": str(tmp_path)}) is False


def test_empty_prefix_matches_no_prs():
    # an empty/missing branch_prefix must match NOTHING (never every PR)
    assert control.list_prs({"name": "x", "path": "C:/x"}) == []


def test_missing_path_is_safe():
    # a malformed registry entry must not crash the dashboard
    assert control.is_running({"name": "x"}) is False
    assert control.read_heartbeat({"name": "x"}) is None


# --------------------------------------------------------------------------- #
# auto-discovery + provider/model + keys
# --------------------------------------------------------------------------- #
def _projects(tmp_path, monkeypatch, *names):
    """Point control.PROJECTS_DIR at a tmp dir and create git-repo subdirs."""
    proj = tmp_path / "projects"
    proj.mkdir()
    for n in names:
        (proj / n / ".git").mkdir(parents=True)
    monkeypatch.setattr(control, "PROJECTS_DIR", str(proj))
    return proj


def _repos_json(tmp_path, monkeypatch, data):
    p = tmp_path / "repos.json"
    p.write_text(json.dumps(data), encoding="utf-8")
    monkeypatch.setattr(control, "REPOS_JSON", str(p))
    return p


def test_discover_projects_finds_git_dir(tmp_path, monkeypatch):
    _projects(tmp_path, monkeypatch, "alpha", "beta")
    (tmp_path / "projects" / "plain").mkdir()        # no .git -> still listed, is_git=False
    (tmp_path / "projects" / ".hidden").mkdir()      # dot-name -> skipped
    found = control._discover_projects()
    by_name = {d["name"]: d for d in found}
    # every immediate (non-dot) subdir is returned, git or not
    assert set(by_name) == {"alpha", "beta", "plain"}
    for d in found:
        assert d["branch_prefix"] == "rsi/" and os.path.isabs(d["path"])
    # git dirs are flagged is_git; the plain folder is not
    assert by_name["alpha"]["is_git"] is True
    assert by_name["plain"]["is_git"] is False
    # has_remote is present for every entry (False here — no origin configured)
    assert by_name["alpha"]["has_remote"] is False
    assert by_name["plain"]["has_remote"] is False


def test_discover_projects_safe_on_missing(tmp_path, monkeypatch):
    monkeypatch.setattr(control, "PROJECTS_DIR", str(tmp_path / "nope"))
    assert control._discover_projects() == []


def test_load_repos_merges_discovery_and_json(tmp_path, monkeypatch):
    _projects(tmp_path, monkeypatch, "alpha", "beta")
    # repos.json overrides alpha with provider/model; gamma has no discovered dir
    _repos_json(tmp_path, monkeypatch, [
        {"name": "alpha", "provider": "openrouter", "model": "qwen/qwen3-coder"},
        {"name": "gamma", "path": "C:/g"},
        "bogus", {"no": "name"},  # non-dict / nameless -> skipped
    ])
    repos = control.load_repos()
    by_name = {r["name"]: r for r in repos}
    assert set(by_name) == {"alpha", "beta", "gamma"}
    # alpha: discovered path kept, json provider/model layered on top
    assert by_name["alpha"]["provider"] == "openrouter"
    assert by_name["alpha"]["model"] == "qwen/qwen3-coder"
    assert os.path.isabs(by_name["alpha"]["path"])
    # beta: discovered only, default branch_prefix
    assert by_name["beta"]["branch_prefix"] == "rsi/"
    assert "provider" not in by_name["beta"]
    # gamma: json-only entry still gets a default branch_prefix
    assert by_name["gamma"]["branch_prefix"] == "rsi/"


def test_project_provider_model_defaults():
    assert control.project_provider({}) == "ollama-cloud"
    assert control.project_model({}) == "glm-5.2"
    assert control.project_provider({"provider": "openrouter"}) == "openrouter"
    assert control.project_model({"provider": "openrouter"}) == "qwen/qwen3-coder"
    assert control.project_model({"provider": "openrouter", "model": "x/y"}) == "x/y"


def test_set_repo_config_roundtrip(tmp_path, monkeypatch):
    _projects(tmp_path, monkeypatch, "alpha")
    _repos_json(tmp_path, monkeypatch, [])
    r = control.set_repo_config("alpha", provider="openrouter", model="qwen/qwen3-coder")
    assert r["ok"] is True
    repos = control.load_repos()
    alpha = next(x for x in repos if x["name"] == "alpha")
    assert alpha["provider"] == "openrouter" and alpha["model"] == "qwen/qwen3-coder"
    # discovered path carried into the created entry
    assert os.path.isabs(alpha["path"])
    # update only the model, provider preserved
    control.set_repo_config("alpha", model="moonshot")
    alpha = next(x for x in control.load_repos() if x["name"] == "alpha")
    assert alpha["provider"] == "openrouter" and alpha["model"] == "moonshot"


def test_set_key_and_keys_status(tmp_path, monkeypatch):
    env = tmp_path / ".env"
    env.write_text("EXISTING=keep\n", encoding="utf-8")
    monkeypatch.setattr(control, "_ENV_FILE", str(env))

    assert control.keys_status() == {"ollama-cloud": False, "openrouter": False}

    assert control.set_key("ollama-cloud", "abc123")["ok"] is True
    assert control.keys_status() == {"ollama-cloud": True, "openrouter": False}

    assert control.set_key("openrouter", "def456")["ok"] is True
    assert control.keys_status() == {"ollama-cloud": True, "openrouter": True}

    # both keys + the unrelated line all coexist; keys_status never leaks values
    body = env.read_text(encoding="utf-8")
    assert "EXISTING=keep" in body
    assert "OLLAMA_API_KEY=abc123" in body
    assert "OPENROUTER_API_KEY=def456" in body

    # clearing one key flips only its flag
    assert control.set_key("ollama-cloud", "")["ok"] is True
    assert control.keys_status() == {"ollama-cloud": False, "openrouter": True}


def test_set_key_unknown_provider(tmp_path, monkeypatch):
    monkeypatch.setattr(control, "_ENV_FILE", str(tmp_path / ".env"))
    assert control.set_key("nope", "x")["ok"] is False


# --------------------------------------------------------------------------- #
# ship mode + gate + publish
# --------------------------------------------------------------------------- #
def test_project_ship_defaults_and_override():
    assert control.project_ship({}) == "pr"            # default
    assert control.project_ship(None) == "pr"
    assert control.project_ship({"ship": "local"}) == "local"
    assert control.project_ship({"ship": "auto-merge"}) == "auto-merge"


def test_project_gate_defaults_and_override():
    assert control.project_gate({}) is None            # default = built-in pytest gate
    assert control.project_gate(None) is None
    assert control.project_gate({"gate": "npm test"}) == "npm test"


def test_set_repo_config_persists_ship_and_gate(tmp_path, monkeypatch):
    _projects(tmp_path, monkeypatch, "alpha")
    _repos_json(tmp_path, monkeypatch, [])
    r = control.set_repo_config("alpha", ship="auto-merge", gate="npm test")
    assert r["ok"] is True
    alpha = next(x for x in control.load_repos() if x["name"] == "alpha")
    assert control.project_ship(alpha) == "auto-merge"
    assert control.project_gate(alpha) == "npm test"
    # updating only ship preserves the gate
    control.set_repo_config("alpha", ship="push")
    alpha = next(x for x in control.load_repos() if x["name"] == "alpha")
    assert control.project_ship(alpha) == "push"
    assert control.project_gate(alpha) == "npm test"


def test_load_repos_carries_is_git_and_has_remote(tmp_path, monkeypatch):
    # one git dir, one plain dir; discovery flags propagate through load_repos
    proj = _projects(tmp_path, monkeypatch, "gitrepo")  # gitrepo gets a .git subdir
    (proj / "plainrepo").mkdir()
    _repos_json(tmp_path, monkeypatch, [])
    by_name = {r["name"]: r for r in control.load_repos()}
    assert by_name["gitrepo"]["is_git"] is True
    assert by_name["plainrepo"]["is_git"] is False
    assert by_name["gitrepo"]["has_remote"] is False   # no origin configured
    assert by_name["plainrepo"]["has_remote"] is False


def test_publish_to_github_not_connected(tmp_path, monkeypatch):
    _projects(tmp_path, monkeypatch, "alpha")
    _repos_json(tmp_path, monkeypatch, [])
    # github not connected -> early return, no git/gh/network side effects
    monkeypatch.setattr(control, "github_status", lambda: {"ready": False, "login": None})
    r = control.publish_to_github("alpha")
    assert r["ok"] is False
    assert "GitHub not connected" in r["error"]


def test_publish_to_github_unknown_repo(tmp_path, monkeypatch):
    monkeypatch.setattr(control, "PROJECTS_DIR", str(tmp_path / "noproj"))
    _repos_json(tmp_path, monkeypatch, [])
    monkeypatch.setattr(control, "github_status", lambda: {"ready": True, "login": "me"})
    r = control.publish_to_github("ghost")
    assert r["ok"] is False and "unknown repo" in r["error"]


# --------------------------------------------------------------------------- #
# beautify
# --------------------------------------------------------------------------- #
def test_beautify_unknown_repo():
    # a None/missing repo never spawns anything
    r = control.beautify(None)
    assert r["ok"] is False and "unknown repo" in r["error"]


def test_beautify_requires_git(tmp_path, monkeypatch):
    # a plain (non-git) discovered folder must be refused — no pi/git side effects
    proj = tmp_path / "projects"
    (proj / "plain").mkdir(parents=True)
    monkeypatch.setattr(control, "PROJECTS_DIR", str(proj))
    _repos_json(tmp_path, monkeypatch, [])
    repo = next(r for r in control.load_repos() if r["name"] == "plain")
    assert repo["is_git"] is False
    r = control.beautify(repo)
    assert r["ok"] is False and "git repo" in r["error"]


def test_runner_argparse_accepts_beautify():
    # the generic runner must import cleanly and register the --beautify flag.
    # Import it as a module by path (proves it loads), then confirm both the CLI flag
    # and the BEAUTIFY plumbing are present. No git/gh/pi is run.
    runner_path = os.path.join(
        os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
        "improver", "run_improver.py")
    spec = importlib.util.spec_from_file_location("run_improver", runner_path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    assert hasattr(mod, "BEAUTIFY") and mod.BEAUTIFY is False  # module default
    assert mod.BEAUTIFY_MD.name == "beautify.md"              # contract path wired
    with open(runner_path, "r", encoding="utf-8") as f:
        src = f.read()
    assert '"--beautify"' in src                              # CLI flag registered


# --------------------------------------------------------------------------- #
# Solomon revamp: PR target / reasoning / auto-push gate / run controls
# --------------------------------------------------------------------------- #
def test_project_pr_target_branch_default_and_override():
    assert control.project_pr_target_branch({}) == "main"
    assert control.project_pr_target_branch(None) == "main"
    assert control.project_pr_target_branch({"pr_target_branch": "develop"}) == "develop"


def test_project_reasoning_default_and_override():
    assert control.project_reasoning({}) == "xhigh"          # max reasoning by default
    assert control.project_reasoning(None) == "xhigh"
    assert control.project_reasoning({"reasoning": "high"}) == "high"


def test_effective_ship_gate():
    assert control.effective_ship({"ship": "pr"}, True) == "pr"
    assert control.effective_ship({"ship": "auto-merge"}, True) == "auto-merge"
    assert control.effective_ship({"ship": "pr"}, False) == "local"
    assert control.effective_ship({}, False) == "local"


def test_project_interval_and_max_iterations():
    assert control.project_interval({}) == 120
    assert control.project_interval({"interval": 45}) == 45
    assert control.project_interval({"interval": 0}) == 120        # non-positive -> default
    assert control.project_interval({"interval": "x"}) == 120      # invalid -> default
    assert control.project_max_iterations({}) == 0
    assert control.project_max_iterations({"max_iterations": 7}) == 7
    assert control.project_max_iterations({"max_iterations": -3}) == 0


def test_set_repo_config_persists_new_fields(tmp_path, monkeypatch):
    _projects(tmp_path, monkeypatch, "alpha")
    _repos_json(tmp_path, monkeypatch, [])
    r = control.set_repo_config("alpha", pr_target_branch="develop", reasoning="high",
                                interval=300, max_iterations=10, goal="Grow the account autonomously")
    assert r["ok"] is True
    alpha = next(x for x in control.load_repos() if x["name"] == "alpha")
    assert control.project_pr_target_branch(alpha) == "develop"
    assert control.project_reasoning(alpha) == "high"
    assert control.project_interval(alpha) == 300
    assert control.project_max_iterations(alpha) == 10
    assert control.project_goal(alpha) == "Grow the account autonomously"
    # updating only reasoning preserves the rest (incl. the goal)
    control.set_repo_config("alpha", reasoning="low")
    alpha = next(x for x in control.load_repos() if x["name"] == "alpha")
    assert control.project_reasoning(alpha) == "low"
    assert control.project_pr_target_branch(alpha) == "develop"
    assert control.project_goal(alpha) == "Grow the account autonomously"
    assert control.project_max_iterations(alpha) == 10


def test_set_repo_config_persists_phases(tmp_path, monkeypatch):
    _projects(tmp_path, monkeypatch, "alpha")
    _repos_json(tmp_path, monkeypatch, [])
    phases = {"review": {"provider": "openrouter", "model": "qwen/qwen3-coder", "reasoning": "xhigh"},
              "beautify": {"reasoning": "low"}}
    r = control.set_repo_config("alpha", model="glm-5.2", phases=phases)
    assert r["ok"] is True
    alpha = next(x for x in control.load_repos() if x["name"] == "alpha")
    assert alpha["phases"]["review"]["provider"] == "openrouter"
    assert alpha["phases"]["beautify"]["reasoning"] == "low"
    # all-empty per-phase entries are dropped, clearing the key; the model is preserved
    control.set_repo_config("alpha", phases={"review": {}, "plan": {}})
    alpha = next(x for x in control.load_repos() if x["name"] == "alpha")
    assert "phases" not in alpha
    assert control.project_model(alpha) == "glm-5.2"


def test_rollup_state():
    assert control._rollup_state(None) is None
    assert control._rollup_state([]) is None
    assert control._rollup_state([{"conclusion": "SUCCESS"}]) == "success"
    assert control._rollup_state([{"state": "SUCCESS"}]) == "success"
    assert control._rollup_state([{"conclusion": "SUCCESS"}, {"conclusion": "FAILURE"}]) == "failure"
    assert control._rollup_state([{"status": "IN_PROGRESS"}]) == "pending"
    assert control._rollup_state([{"state": "PENDING"}]) == "pending"


def test_read_log_tail(tmp_path, monkeypatch):
    rt = _runtime(tmp_path, monkeypatch)
    (rt / "improver.log").write_text("line one\nline two\n", encoding="utf-8")
    out = control.read_log({"name": "x", "path": str(tmp_path)})
    assert out["ok"] is True and "line two" in out["log"]
    # a missing log is safe (empty string, never raises)
    monkeypatch.setattr(control, "HERE", str(tmp_path / "nope"))
    assert control.read_log({"name": "x", "path": str(tmp_path)})["log"] == ""


def test_read_history_and_metrics(tmp_path, monkeypatch):
    rt = _runtime(tmp_path, monkeypatch)
    recs = [
        {"status": "shipped", "iteration": 1, "tests": {"passed": 4, "failed": 0},
         "pr": {"state": "open"}},
        {"status": "reverted", "iteration": 2, "tests": {"passed": 1, "failed": 2}},
        {"status": "noop", "iteration": 3},
        {"status": "shipped", "iteration": 4, "tests": {"passed": 6, "failed": 0},
         "pr": {"state": "merged"}},
    ]
    body = "\n".join(json.dumps(r) for r in recs) + "\n{bad json\n\n"
    (rt / "history.jsonl").write_text(body, encoding="utf-8")
    hist = control.read_history({"name": "x", "path": str(tmp_path)})
    assert len(hist) == 4 and hist[-1]["status"] == "shipped"      # corrupt/blank lines skipped
    m = control.metrics({"name": "x", "path": str(tmp_path)})
    assert m["iterations"] == 4 and m["shipped"] == 2 and m["reverted"] == 1 and m["noop"] == 1
    assert m["merged"] == 1
    assert m["success_rate"] == round(2 / 4, 3)
    assert len(m["tests_series"]) == 3


def test_read_history_missing_safe(tmp_path, monkeypatch):
    _runtime(tmp_path, monkeypatch)
    assert control.read_history({"name": "x", "path": str(tmp_path)}) == []
    assert control.metrics({"name": "x", "path": str(tmp_path)})["iterations"] == 0


def test_contract_roundtrip(tmp_path, monkeypatch):
    monkeypatch.setattr(control, "HERE", str(tmp_path))
    repo = {"name": "x", "path": str(tmp_path)}
    assert control.read_contract(repo, "agent")["text"] == ""       # missing -> empty, ok
    assert control.write_contract(repo, "agent", "# Contract\n")["ok"] is True
    assert control.read_contract(repo, "agent")["text"] == "# Contract\n"
    assert control.write_contract(repo, "backlog", "- [ ] item")["ok"] is True
    assert control.read_contract(repo, "backlog")["text"] == "- [ ] item"
    assert control.read_contract(repo, "bogus")["ok"] is False      # unknown contract


def test_health_shape(tmp_path, monkeypatch):
    _projects(tmp_path, monkeypatch, "alpha")
    _repos_json(tmp_path, monkeypatch, [])
    monkeypatch.setattr(control, "_ENV_FILE", str(tmp_path / ".env"))
    monkeypatch.setattr(control, "gh_ready", lambda: False)
    h = control.health()
    assert set(h) == {"gh", "git", "keys", "repos"}
    assert h["gh"] is False and isinstance(h["repos"], list)
    assert any(r["name"] == "alpha" for r in h["repos"])


def test_runner_argparse_accepts_new_flags():
    runner_path = os.path.join(
        os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
        "improver", "run_improver.py")
    with open(runner_path, "r", encoding="utf-8") as f:
        src = f.read()
    for flag in ('"--pr-target-branch"', '"--max-iterations"', '"--reasoning"'):
        assert flag in src


def test_cleanup_worktrees_safe(tmp_path, monkeypatch):
    # never raises / never deletes when prerequisites are missing
    monkeypatch.setattr(control, "_which_git", lambda: None)
    assert control.cleanup_worktrees({"name": "x", "path": str(tmp_path)})["ok"] is False  # no git
    monkeypatch.setattr(control, "_which_git", lambda: "git")
    assert control.cleanup_worktrees({"name": "x"})["ok"] is False                          # no path


# --------------------------------------------------------------------------- #
# GitHub tools for the pi agent + end-of-loop verification
# --------------------------------------------------------------------------- #
def test_github_tools_extension_present():
    base = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    ext = os.path.join(base, "improver", "github-tools.ts")
    assert os.path.exists(ext)
    with open(ext, "r", encoding="utf-8") as f:
        src = f.read()
    for tool in ("github_status", "github_verify_push", "github_pr_status",
                 "github_ci_status", "github_list_prs"):
        assert tool in src


def test_runner_github_verification_wired():
    runner = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                          "improver", "run_improver.py")
    spec = importlib.util.spec_from_file_location("run_improver", runner)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    for attr in ("GITHUB_TOOLS", "GITHUB_TOOLS_EXT", "_github_ready", "_branch_on_remote", "_pr_checks"):
        assert hasattr(mod, attr), f"run_improver missing {attr}"
    with open(runner, "r", encoding="utf-8") as f:
        src = f.read()
    assert "github-tools.ts" in src                 # extension loaded by the runner
    assert "GitHub not ready" in src                # precondition gate present


def test_clean_subenv_strips_github_tokens(monkeypatch):
    monkeypatch.setenv("GITHUB_TOKEN", "bad")
    monkeypatch.setenv("GH_TOKEN", "bad2")
    env = control._clean_subenv()
    assert "GITHUB_TOKEN" not in env and "GH_TOKEN" not in env


def test_clean_subenv_strips_pythonpath(monkeypatch):
    # leaked PYTHONPATH/PYTHONHOME crash a spawned repo .venv python of a different minor
    # version with "SRE module mismatch" — they must not reach the child.
    monkeypatch.setenv("PYTHONPATH", "C:/some/3.11/libs")
    monkeypatch.setenv("PYTHONHOME", "C:/some/3.11")
    env = control._clean_subenv()
    assert "PYTHONPATH" not in env and "PYTHONHOME" not in env


# --------------------------------------------------------------------------- #
# Feature 4: connect_project (one-call clone/register + config + provision)
# Feature 5: github_login_start (one-click login)
# Feature 2: list_worktrees (worktree visualization)
# --------------------------------------------------------------------------- #
def test_connect_project_registers_local_path(tmp_path, monkeypatch):
    """connect_project with a local path registers it in repos.json + sets the goal."""
    monkeypatch.setattr(control, "HERE", str(tmp_path))
    monkeypatch.setattr(control, "REPOS_JSON", str(tmp_path / "repos.json"))
    monkeypatch.setattr(control, "PROJECTS_DIR", str(tmp_path / "projects"))
    repo_dir = tmp_path / "myrepo"; repo_dir.mkdir()
    (repo_dir / "main.py").write_text("print('hi')", encoding="utf-8")
    # stub out the background enrichment + provision paths that need a venv/pi
    monkeypatch.setattr(control, "enrich_contract", lambda repo, background=False: {"ok": False})
    monkeypatch.setattr(control, "ensure_contracts", lambda repo: {"ok": True})
    monkeypatch.setattr(control, "keys_status", lambda: {"ollama-cloud": True})
    monkeypatch.setattr(control, "project_provider", lambda repo: "ollama-cloud")
    monkeypatch.setattr(control, "project_sandbox", lambda repo: None)
    monkeypatch.setattr(control, "has_frontend", lambda repo: False)
    out = control.connect_project(str(repo_dir), goal="make it fast", ship="pr")
    assert out["ok"] is True
    assert out["name"] == "myrepo"
    # the repos.json entry now exists with the goal + ship mode
    entries = control._read_repos_json(str(tmp_path / "repos.json"))
    row = next(r for r in entries if r["name"] == "myrepo")
    assert row["goal"] == "make it fast" and row["ship"] == "pr"


def test_connect_project_auto_enables_visual_gate_for_frontend(tmp_path, monkeypatch):
    """connect_project auto-enables visual_gate when the repo has a frontend and the operator
    didn't explicitly answer the visual-gate question (Feature 1a)."""
    monkeypatch.setattr(control, "HERE", str(tmp_path))
    monkeypatch.setattr(control, "REPOS_JSON", str(tmp_path / "repos.json"))
    monkeypatch.setattr(control, "PROJECTS_DIR", str(tmp_path / "projects"))
    repo_dir = tmp_path / "webrepo"; repo_dir.mkdir()
    (repo_dir / "index.html").write_text("<html></html>", encoding="utf-8")
    monkeypatch.setattr(control, "enrich_contract", lambda repo, background=False: {"ok": False})
    monkeypatch.setattr(control, "ensure_contracts", lambda repo: {"ok": True})
    monkeypatch.setattr(control, "keys_status", lambda: {"ollama-cloud": True})
    monkeypatch.setattr(control, "project_provider", lambda repo: "ollama-cloud")
    monkeypatch.setattr(control, "project_sandbox", lambda repo: None)
    monkeypatch.setattr(control, "has_frontend", lambda repo: True)
    out = control.connect_project(str(repo_dir), goal="ui", visual_gate=None)
    assert out["ok"] is True
    assert out.get("visual_gate") is True


def test_connect_project_explicit_visual_gate_false_honored_for_frontend(tmp_path, monkeypatch):
    """An explicit visual_gate=False opt-out is honored even for a frontend repo — the flag
    is written to repos.json so the runner sees the explicit False and doesn't fall back to
    frontend auto-detection (the major bug the code review caught)."""
    monkeypatch.setattr(control, "HERE", str(tmp_path))
    monkeypatch.setattr(control, "REPOS_JSON", str(tmp_path / "repos.json"))
    monkeypatch.setattr(control, "PROJECTS_DIR", str(tmp_path / "projects"))
    repo_dir = tmp_path / "webrepo"; repo_dir.mkdir()
    (repo_dir / "index.html").write_text("<html></html>", encoding="utf-8")
    monkeypatch.setattr(control, "enrich_contract", lambda repo, background=False: {"ok": False})
    monkeypatch.setattr(control, "ensure_contracts", lambda repo: {"ok": True})
    monkeypatch.setattr(control, "keys_status", lambda: {"ollama-cloud": True})
    monkeypatch.setattr(control, "project_provider", lambda repo: "ollama-cloud")
    monkeypatch.setattr(control, "project_sandbox", lambda repo: None)
    monkeypatch.setattr(control, "has_frontend", lambda repo: True)
    out = control.connect_project(str(repo_dir), goal="ui", visual_gate=False)
    assert out["ok"] is True
    # the repos.json entry has visual_gate=False (explicit opt-out, not dropped)
    entries = control._read_repos_json(str(tmp_path / "repos.json"))
    row = next(r for r in entries if r["name"] == "webrepo")
    assert row.get("visual_gate") is False


def test_github_login_start_no_gh(monkeypatch):
    """github_login_start returns a clear error when gh is not installed."""
    monkeypatch.setattr(control, "_which_gh", lambda: None)
    out = control.github_login_start()
    assert out["ok"] is False and "gh not found" in out["error"]


def test_github_login_start_already_authed(monkeypatch):
    """github_login_start is idempotent — returns {already:true,login} when already authed."""
    monkeypatch.setattr(control, "_which_gh", lambda: "gh")
    monkeypatch.setattr(control, "gh_ready", lambda: True)
    monkeypatch.setattr(control, "github_status", lambda: {"ready": True, "login": "me"})
    out = control.github_login_start()
    assert out["ok"] is True and out["already"] is True and out["login"] == "me"


def test_github_login_start_uses_visible_console(monkeypatch):
    """onboarding-1: `gh auth login --web` is interactive (prints a one-time device code, waits). It
    must run in a VISIBLE console with std streams NOT redirected, or the operator never sees the code
    and login silently hangs."""
    monkeypatch.setattr(control, "_which_gh", lambda: "gh")
    monkeypatch.setattr(control, "gh_ready", lambda: False)
    monkeypatch.setattr(control, "_clean_subenv", lambda: {})
    captured = {}

    def _popen(args, **kwargs):
        captured["args"], captured["kwargs"] = args, kwargs
        return object()

    monkeypatch.setattr(control.subprocess, "Popen", _popen)
    out = control.github_login_start()
    assert out["ok"] and out.get("started")
    # streams must NOT be swallowed (no DEVNULL) — the device code goes to the console
    assert "stdout" not in captured["kwargs"] and "stderr" not in captured["kwargs"]
    assert "--web" in captured["args"]
    if sys.platform == "win32":  # own visible console window (CREATE_NEW_CONSOLE = 0x10)
        assert captured["kwargs"].get("creationflags", 0) & 0x00000010


def test_connect_project_auto_detects_public_remote(tmp_path, monkeypatch):
    """onboarding-2: a PUBLIC GitHub remote gets public=true on connect so the leak guard
    (deny_terms / private_paths / secret-shape block) is active without a manual repos.json edit."""
    monkeypatch.setattr(control, "HERE", str(tmp_path))
    monkeypatch.setattr(control, "REPOS_JSON", str(tmp_path / "repos.json"))
    monkeypatch.setattr(control, "PROJECTS_DIR", str(tmp_path / "projects"))
    repo_dir = tmp_path / "pub"; (repo_dir / ".git").mkdir(parents=True)
    monkeypatch.setattr(control, "_has_origin", lambda p: True)
    monkeypatch.setattr(control, "_gh_repo_visibility", lambda p: True)
    monkeypatch.setattr(control, "enrich_contract", lambda repo, background=False: {"ok": False})
    monkeypatch.setattr(control, "ensure_contracts", lambda repo: {"ok": True})
    monkeypatch.setattr(control, "keys_status", lambda: {"ollama-cloud": True})
    monkeypatch.setattr(control, "project_sandbox", lambda repo: None)
    monkeypatch.setattr(control, "has_frontend", lambda repo: False)
    out = control.connect_project(str(repo_dir), goal="x", ship="pr")
    assert out["ok"]
    row = next(r for r in control._read_repos_json(str(tmp_path / "repos.json")) if r["name"] == "pub")
    assert row.get("public") is True


def test_connect_project_auto_picks_present_provider(tmp_path, monkeypatch):
    """onboarding-3: connect picks the provider whose key the operator actually entered, so the repo
    is immediately runnable (an OpenRouter-only operator no longer gets a silently non-runnable repo)."""
    monkeypatch.setattr(control, "HERE", str(tmp_path))
    monkeypatch.setattr(control, "REPOS_JSON", str(tmp_path / "repos.json"))
    monkeypatch.setattr(control, "PROJECTS_DIR", str(tmp_path / "projects"))
    repo_dir = tmp_path / "loc"; repo_dir.mkdir()
    monkeypatch.setattr(control, "enrich_contract", lambda repo, background=False: {"ok": False})
    monkeypatch.setattr(control, "ensure_contracts", lambda repo: {"ok": True})
    monkeypatch.setattr(control, "keys_status", lambda: {"ollama-cloud": False, "openrouter": True})
    monkeypatch.setattr(control, "project_sandbox", lambda repo: None)
    monkeypatch.setattr(control, "has_frontend", lambda repo: False)
    out = control.connect_project(str(repo_dir), goal="x", ship="pr")
    assert out["ok"]
    row = next(r for r in control._read_repos_json(str(tmp_path / "repos.json")) if r["name"] == "loc")
    assert row.get("provider") == "openrouter"


def test_list_worktrees_empty_when_no_git(tmp_path, monkeypatch):
    monkeypatch.setattr(control, "_which_git", lambda: None)
    assert control.list_worktrees({"name": "x", "path": str(tmp_path)}) == []


def test_browser_state_missing_file_returns_not_ok(tmp_path, monkeypatch):
    """browser_state returns {ok:false} (not raises) when no session file exists — UI shows empty."""
    _runtime(tmp_path, monkeypatch, name="x")
    out = control.browser_state({"name": "x", "path": str(tmp_path)})
    assert out["ok"] is False


def test_browser_state_reads_snapshot(tmp_path, monkeypatch):
    """browser_state reads the live snapshot from runtime/<name>/browser_state.json."""
    rt = _runtime(tmp_path, monkeypatch, name="x")
    snap = {"ok": True, "url": "http://127.0.0.1:1/", "screenshot_b64": "abc",
            "cursor": {"x": 50, "y": 50, "click": False}, "status": "live", "phase": "active"}
    (rt / "browser_state.json").write_text(json.dumps(snap), encoding="utf-8")
    out = control.browser_state({"name": "x", "path": str(tmp_path)})
    assert out["ok"] is True and out["url"] == "http://127.0.0.1:1/"
    assert out["cursor"]["x"] == 50
