"""Regression tests for the ultra-review remediation (no network / gh / real pi).

Covers the invariants the review found untested plus the new behaviors:
  TEST-1  anti-gaming gate (pass-count drop + new skip/xfail markers)
  TEST-3  auto-revert / fail-closed (_abort_branch / _drop_branch)
  TEST-2  preflight refuses to hard-reset an un-pushed base commit (never-hand-patched keystone)
  DUP-1   runner _pr_checks reduction stays in parity with control._rollup_state
  SEC-1   _redact scrubs secret-shaped strings
  RSC-1   _await_pr_checks polls instead of treating a transient empty rollup as 'no CI'
  CTRL-1  _pid_alive exact CSV match (no substring false-positive)
  PKG-1   _runner_python never returns sys.executable when frozen
  SEC-2   open_url only opens http(s)
  TEST-4  get_state error-fallback dict carries the 'goal' key
  TEST-5  provider key value never reaches the get_state/health JSON bridge
"""
import importlib.util
import json
import os
import shutil
import subprocess
import sys

import pytest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import control  # noqa: E402

RUNNER = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                      "improver", "run_improver.py")


def _load_runner():
    spec = importlib.util.spec_from_file_location("run_improver", RUNNER)
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


def _git(path, *a):
    return subprocess.run(["git", "-C", str(path), *a], capture_output=True, text=True)


def _mk_origin_clone(tmp_path):
    origin = tmp_path / "origin.git"
    work = tmp_path / "work"
    subprocess.run(["git", "init", "--bare", str(origin)], capture_output=True)
    subprocess.run(["git", "clone", str(origin), str(work)], capture_output=True)
    _git(work, "config", "user.email", "t@t"); _git(work, "config", "user.name", "t")
    _git(work, "checkout", "-b", "main")
    (work / "f.txt").write_text("1"); _git(work, "add", "-A"); _git(work, "commit", "-m", "init")
    _git(work, "push", "-u", "origin", "main")
    return work


# --------------------------------------------------------------------------- #
# SEC-1 — secret redaction
# --------------------------------------------------------------------------- #
def test_redact_scrubs_secret_shapes():
    m = _load_runner()
    assert m._redact("see ghp_" + "A" * 40 + " here").count("ghp_") == 0
    assert "[REDACTED]" in m._redact("ghp_" + "A" * 40)
    assert "[REDACTED]" in m._redact("github_pat_" + "b" * 30)
    assert "[REDACTED]" in m._redact("key sk-" + "x" * 30)
    assert m._redact("OLLAMA_API_KEY=supersecretvalue123") == "OLLAMA_API_KEY=[REDACTED]"
    assert m._redact("OPENROUTER_API_KEY: abcd1234efgh5678") == "OPENROUTER_API_KEY: [REDACTED]"  # sep preserved
    # an UPPERCASE env-var-style name is redacted even when its value has no digit
    assert m._redact("GITHUB_TOKEN=ghaaaaaaaaaa") == "GITHUB_TOKEN=[REDACTED]"
    bearer = m._redact("Authorization: Bearer " + "z" * 30)
    assert "[REDACTED]" in bearer and "zzz" not in bearer


def test_redact_leaves_innocent_text_and_handles_empty():
    m = _load_runner()
    assert m._redact("Added a token bucket limiter; 4 tests pass.") == \
        "Added a token bucket limiter; 4 tests pass."
    # prose that uses 'token:'/'secret:'/'password:' must NOT be mangled (lowercase name, no digit value)
    assert m._redact("Implemented token: validation for the refresh flow.") == \
        "Implemented token: validation for the refresh flow."
    assert m._redact("The secret: management module now uses keyring.") == \
        "The secret: management module now uses keyring."
    assert m._redact("") == ""
    assert m._redact(None) is None


# --------------------------------------------------------------------------- #
# TEST-1 — anti-gaming rule (pure helpers)
# --------------------------------------------------------------------------- #
def test_new_skip_markers_detects_added_skips():
    m = _load_runner()
    diff = ("diff --git a/tests/t.py b/tests/t.py\n"
            "+    @pytest.mark.skip(reason='flaky')\n"
            "     def test_real():\n"
            "+@unittest.skip\n"
            "-    @pytest.mark.skip('was here before, removed line')\n")
    skips = m._new_skip_markers(diff)
    assert len(skips) == 2                                   # only the two ADDED markers
    assert m._new_skip_markers("") == []
    assert m._new_skip_markers("+    x = 1\n-    @pytest.mark.skip\n") == []   # removed != added


def test_anti_gaming_reason_pass_count_drop_and_skips():
    m = _load_runner()
    base, after = {"passed": 10}, {"passed": 8}
    assert "pass count fell" in m._anti_gaming_reason(base, after, "")
    # equal count but an added skip marker is still gamed
    assert "skip/xfail" in m._anti_gaming_reason({"passed": 10}, {"passed": 10},
                                                 "+    @pytest.mark.xfail\n")
    # a legitimate green iteration: count held/rose, no skips -> not gamed
    assert m._anti_gaming_reason({"passed": 10}, {"passed": 12}, "+    assert ok\n") is None
    # no baseline (e.g. nothing to compare) -> not gamed by count
    assert m._anti_gaming_reason(None, {"passed": 0}, "") is None


def test_solomon_fix_session_keeps_anti_gaming_baseline():
    """RSI-2: base_tests is measured for SOLOMON too, so the post-change pass-count check applies to a
    supervisor fix-session; only the base-RED ABORT (which a fix-session needs) is skipped for SOLOMON."""
    with open(RUNNER, encoding="utf-8") as f:
        src = f.read()
    assert "if not BEAUTIFY:" in src                         # base gate measured unless docs-only
    assert "if not bgreen and not SOLOMON:" in src           # only the red-base abort skips SOLOMON


# --------------------------------------------------------------------------- #
# RSC-1 — auto-merge polls CI instead of merging into an empty rollup window
# --------------------------------------------------------------------------- #
def test_await_pr_checks_returns_first_concrete_state(monkeypatch):
    m = _load_runner()
    seq = iter([None, None, "pending"])
    monkeypatch.setattr(m, "_pr_checks", lambda n: next(seq))
    monkeypatch.setattr(m.time, "sleep", lambda *_: None)
    monkeypatch.setattr(m, "STOP", m.RUNTIME / "no_such_stop")
    assert m._await_pr_checks(7, attempts=5, delay=0) == "pending"


def test_await_pr_checks_concludes_none_after_window(monkeypatch):
    m = _load_runner()
    calls = {"n": 0}
    def fake(_n):
        calls["n"] += 1
        return None
    monkeypatch.setattr(m, "_pr_checks", fake)
    monkeypatch.setattr(m.time, "sleep", lambda *_: None)
    monkeypatch.setattr(m, "STOP", m.RUNTIME / "no_such_stop")
    assert m._await_pr_checks(7, attempts=3, delay=0) is None
    assert calls["n"] == 3                                   # polled exactly 'attempts' times


# --------------------------------------------------------------------------- #
# DUP-1 — runner _pr_checks reduction matches control._rollup_state
# --------------------------------------------------------------------------- #
def test_pr_checks_parity_with_control_rollup_state(monkeypatch):
    m = _load_runner()
    monkeypatch.setattr(m, "gh_exe", lambda: "gh")
    rollups = [
        [{"conclusion": "SUCCESS"}],
        [{"state": "SUCCESS"}],
        [{"conclusion": "SUCCESS"}, {"conclusion": "FAILURE"}],
        [{"status": "IN_PROGRESS"}],
        [{"state": "PENDING"}],
        [{"conclusion": "ACTION_REQUIRED"}],
        [{"status": "COMPLETED", "conclusion": "SUCCESS"}, {"status": "IN_PROGRESS"}],
    ]
    for rollup in rollups:
        payload = json.dumps({"statusCheckRollup": rollup})
        monkeypatch.setattr(m.subprocess, "run",
                            lambda *a, _p=payload, **k: type("P", (), {"returncode": 0, "stdout": _p, "stderr": ""})())
        assert m._pr_checks(1) == control._rollup_state(rollup), rollup


# --------------------------------------------------------------------------- #
# TEST-3 — auto-revert / fail-closed
# --------------------------------------------------------------------------- #
@pytest.mark.skipif(not shutil.which("git"), reason="git not available")
def test_abort_branch_returns_to_base_and_deletes(tmp_path, monkeypatch):
    m = _load_runner()
    work = _mk_origin_clone(tmp_path)
    _git(work, "checkout", "-b", "rsi/iter-x")
    (work / "f.txt").write_text("2"); _git(work, "commit", "-am", "wip on branch")
    monkeypatch.setattr(m, "REPO", work)
    monkeypatch.setattr(m, "BASE_BRANCH", "main")
    assert m._abort_branch("rsi/iter-x") is True
    assert _git(work, "rev-parse", "--abbrev-ref", "HEAD").stdout.strip() == "main"
    branches = _git(work, "branch", "--list", "rsi/iter-x").stdout.strip()
    assert branches == ""                                    # branch deleted


def test_drop_branch_fail_closed_escalates(tmp_path, monkeypatch):
    m = _load_runner()
    monkeypatch.setattr(m, "RUNTIME", tmp_path)
    monkeypatch.setattr(m, "HEARTBEAT", tmp_path / "heartbeat.json")
    monkeypatch.setattr(m, "_abort_branch", lambda b: False)   # simulate a revert that could not complete
    monkeypatch.setattr(m, "_record_history", lambda *a, **k: None)
    m._drop_branch("rsi/iter-y", "reverted", "the summary")
    assert m._hb["status"] == "error"
    assert "REVERT FAILED" in m._hb["last_summary"]


# --------------------------------------------------------------------------- #
# TEST-2 — preflight refuses to hard-reset an un-pushed base commit
# --------------------------------------------------------------------------- #
@pytest.mark.skipif(not shutil.which("git"), reason="git not available")
def test_preflight_refuses_unpushed_base_commit(tmp_path, monkeypatch):
    m = _load_runner()
    work = _mk_origin_clone(tmp_path)
    (work / "f.txt").write_text("hand-patch"); _git(work, "commit", "-am", "operator hand-patch")  # un-pushed
    rt = tmp_path / "rt"; rt.mkdir()
    monkeypatch.setattr(m, "REPO", work)
    monkeypatch.setattr(m, "BASE_BRANCH", "main")
    monkeypatch.setattr(m, "RUNTIME", rt)
    monkeypatch.setattr(m, "HEARTBEAT", rt / "heartbeat.json")
    monkeypatch.setattr(m, "STOP", rt / "stop")
    monkeypatch.setattr(m, "LOG", rt / "improver.log")
    monkeypatch.setattr(m, "BACKLOG", tmp_path / "backlog.md")
    m._hb["iteration"] = 0
    ran = {"pi": False}
    monkeypatch.setattr(m, "run_pi", lambda *a, **k: ran.__setitem__("pi", True))

    m.one_iteration()

    assert m._hb["status"] == "error" and m._hb["phase"] == "preflight"
    assert "un-pushed" in m._hb["last_summary"]
    assert ran["pi"] is False                                # never reached the agent
    # the operator's un-pushed commit must survive (not hard-reset away)
    assert _git(work, "rev-list", "--count", "origin/main..main").stdout.strip() == "1"


# --------------------------------------------------------------------------- #
# CTRL-1 — _pid_alive exact CSV match
# --------------------------------------------------------------------------- #
@pytest.mark.skipif(sys.platform != "win32", reason="tasklist/CSV match is win32-only")
def test_pid_alive_exact_csv_match(monkeypatch):
    csv = '"python.exe","1234","Console","1","12,345 K"\n'
    monkeypatch.setattr(control, "_run",
                        lambda *a, **k: type("R", (), {"stdout": csv, "returncode": 0})())
    assert control._pid_alive(1234) is True
    assert control._pid_alive(12) is False                   # substring of 1234 but NOT a quoted field
    assert control._pid_alive(0) is False


# --------------------------------------------------------------------------- #
# PKG-1 — _runner_python never returns the frozen exe
# --------------------------------------------------------------------------- #
def test_runner_python_guards_frozen(tmp_path, monkeypatch):
    repo = {"name": "x", "path": str(tmp_path)}              # no .venv present
    monkeypatch.setattr(control.sys, "frozen", True, raising=False)
    assert control._runner_python(repo) is None              # frozen + no venv -> fail loudly
    monkeypatch.delattr(control.sys, "frozen", raising=False)
    assert control._runner_python(repo) == sys.executable    # unfrozen -> current interpreter


def test_runner_python_prefers_real_venv(tmp_path, monkeypatch):
    scripts = tmp_path / ".venv" / "Scripts"
    scripts.mkdir(parents=True)
    py = scripts / "python.exe"
    py.write_text("")
    repo = {"name": "x", "path": str(tmp_path)}
    monkeypatch.setattr(control.sys, "frozen", True, raising=False)
    assert control._runner_python(repo) == str(py)           # real venv wins even when frozen


def test_base_dir_frozen_no_stderr_does_not_crash(tmp_path, monkeypatch):
    """PKG-2 regression: a windowed frozen exe (console=False) has sys.stderr == None; the fail-loud
    warning branch must not raise AttributeError trying to write to it."""
    exe = tmp_path / "Solomon.exe"
    monkeypatch.setattr(control.sys, "frozen", True, raising=False)
    monkeypatch.setattr(control.sys, "executable", str(exe))
    monkeypatch.setattr(control.sys, "stderr", None)         # the crash condition
    monkeypatch.delenv("SOLOMON_HOME", raising=False)
    monkeypatch.setattr(control.os.path, "isdir", lambda p: False)   # no improver/ anywhere -> fail-loud branch
    assert control._base_dir() == os.path.dirname(os.path.abspath(str(exe)))   # returns exe dir, no crash


# --------------------------------------------------------------------------- #
# SEC-2 — open_url only opens http(s)
# --------------------------------------------------------------------------- #
def test_open_url_only_allows_http(monkeypatch):
    import app
    opened = []
    monkeypatch.setattr(app.webbrowser, "open", lambda u: opened.append(u) or True)
    api = app.Api()
    assert api.open_url("http://example.com")["ok"] is True
    assert api.open_url("https://example.com/x")["ok"] is True
    assert opened == ["http://example.com", "https://example.com/x"]
    for bad in ("file:///C:/Windows/System32/calc.exe", r"C:\evil.exe",
                "javascript:alert(1)", r"\\host\share\x", None, 123):
        assert api.open_url(bad)["ok"] is False
    assert len(opened) == 2                                   # never launched for the bad inputs


# --------------------------------------------------------------------------- #
# TEST-4 — get_state error-fallback carries the 'goal' key
# --------------------------------------------------------------------------- #
def test_get_state_fallback_includes_goal(monkeypatch):
    import app
    monkeypatch.setattr(app.control, "load_repos",
                        lambda: [{"name": "x", "path": "p", "is_git": True, "has_remote": False}])
    monkeypatch.setattr(app.control, "gh_ready", lambda: False)
    monkeypatch.setattr(app.control, "github_status", lambda: {"ready": False, "login": None})

    def boom(_r):
        raise RuntimeError("repo blew up while building state")
    monkeypatch.setattr(app.control, "project_provider", boom)   # first call in the try -> except branch
    st = app.Api().get_state()
    assert st["repos"][0]["goal"] == ""                      # fallback dict has the same keys the UI reads
    assert "error" in st["repos"][0]


# --------------------------------------------------------------------------- #
# TEST-5 — provider key value never reaches the bridge payload
# --------------------------------------------------------------------------- #
def test_secret_value_never_in_state_or_health(tmp_path, monkeypatch):
    import app
    SENTINEL = "SENTINEL_SECRET_VALUE_zzz999"
    env = tmp_path / ".env"
    env.write_text(f"OLLAMA_API_KEY={SENTINEL}\n", encoding="utf-8")
    monkeypatch.setattr(control, "_ENV_FILE", str(env))
    monkeypatch.setattr(control, "REPOS_JSON", str(tmp_path / "none.json"))
    monkeypatch.setattr(control, "PROJECTS_DIR", str(tmp_path / "noproj"))
    monkeypatch.setattr(control, "gh_ready", lambda: False)
    monkeypatch.setattr(control, "github_status", lambda: {"ready": False, "login": None})
    assert control.keys_status().get("ollama-cloud") is True   # key IS present (bool only)
    state_blob = json.dumps(app.Api().get_state())
    assert SENTINEL not in state_blob
    health_blob = json.dumps(control.health())
    assert SENTINEL not in health_blob


# --------------------------------------------------------------------------- #
# LOCK-1 — acquire_lock single-flight: an EMPTY existing lock is HELD (not stolen).
# Regression for the live double-runner: two improvers ran on sover because the old
# create-then-write left a window where a racer read the empty lock as a stale pid 0
# and stole it. acquire_lock must refuse an empty lock, a live-pid lock, and ours; and
# take over ONLY a confirmed-dead-pid lock.
# --------------------------------------------------------------------------- #
def _runner_with_lock(tmp_path):
    m = _load_runner()
    m.RUNTIME = tmp_path
    m.LOCK = tmp_path / "lock"
    return m


def test_acquire_lock_refuses_empty_lock(tmp_path, monkeypatch):
    m = _runner_with_lock(tmp_path)
    monkeypatch.setattr(m, "_pid_alive", lambda pid: False)   # even with "dead" pids, empty != stale
    m.LOCK.write_text("", encoding="utf-8")                   # a racer's just-created, not-yet-written lock
    assert m.acquire_lock() is False                          # MUST back off, not steal
    assert m.LOCK.read_text(encoding="utf-8") == ""           # and must not overwrite it


def test_acquire_lock_refuses_live_holder(tmp_path, monkeypatch):
    m = _runner_with_lock(tmp_path)
    monkeypatch.setattr(m, "_pid_alive", lambda pid: True)
    m.LOCK.write_text("99999", encoding="utf-8")
    assert m.acquire_lock() is False
    assert m.LOCK.read_text(encoding="utf-8") == "99999"      # untouched


def test_acquire_lock_takes_over_dead_holder(tmp_path, monkeypatch):
    m = _runner_with_lock(tmp_path)
    monkeypatch.setattr(m, "_pid_alive", lambda pid: False)   # recorded pid is dead
    m.LOCK.write_text("99999", encoding="utf-8")
    assert m.acquire_lock() is True
    assert m.LOCK.read_text(encoding="utf-8").strip() == str(os.getpid())


def test_acquire_lock_creates_with_pid(tmp_path):
    m = _runner_with_lock(tmp_path)
    assert not m.LOCK.exists()
    assert m.acquire_lock() is True
    assert m.LOCK.read_text(encoding="utf-8").strip() == str(os.getpid())  # never created empty
    assert m.acquire_lock() is True                                        # idempotent for our own pid


# --------------------------------------------------------------------------- #
# WEDGE-1 — a dirty tree skips the iteration ONLY on the base branch. A dead run that died
# mid-iteration leaves the tree dirty on an rsi/* branch; that must NOT wedge the loop forever
# (it must fall through to the forced preflight reset that clears it).
# --------------------------------------------------------------------------- #
def test_dirty_blocks_only_on_base_branch():
    m = _load_runner()
    assert m._dirty_blocks_iteration(True, "main", "main") is True          # dirty base -> protect, skip
    assert m._dirty_blocks_iteration(True, "rsi/iter-123", "main") is False  # dead-run leftover -> reset
    assert m._dirty_blocks_iteration(True, "HEAD", "main") is False          # detached -> reset, don't wedge
    assert m._dirty_blocks_iteration(False, "main", "main") is False         # clean -> never blocks


# --------------------------------------------------------------------------- #
# LOCK-2 — release_lock must only delete OUR lock; an unreadable/foreign lock is left alone
# (the old except-branch unlinked unconditionally, which could steal another runner's lock).
# --------------------------------------------------------------------------- #
def test_release_lock_keeps_foreign_lock(tmp_path):
    m = _runner_with_lock(tmp_path)
    m.LOCK.write_text("999999", encoding="utf-8")        # another runner's pid (not ours)
    m.release_lock()
    assert m.LOCK.exists()                                # must NOT delete a lock we don't own
    m.LOCK.write_text("not-a-pid", encoding="utf-8")     # corrupt/unparseable
    m.release_lock()
    assert m.LOCK.exists()                                # unreadable lock is left alone, not stolen
    m.LOCK.write_text(str(os.getpid()), encoding="utf-8")
    m.release_lock()
    assert not m.LOCK.exists()                            # our own lock IS released


# --------------------------------------------------------------------------- #
# STOP-1 — start() on a LIVE loop must not revoke a pending Stop (halt-switch invariant).
# --------------------------------------------------------------------------- #
def test_start_does_not_revoke_pending_stop_on_live_loop(tmp_path, monkeypatch):
    monkeypatch.setattr(control, "HERE", str(tmp_path))
    rt = tmp_path / "runtime" / "x"
    rt.mkdir(parents=True)
    (rt / "stop").write_text("", encoding="utf-8")           # operator Stop pending
    monkeypatch.setattr(control, "is_running", lambda repo: True)
    monkeypatch.setattr(control, "ensure_contracts", lambda repo: {"ok": True, "created": []})
    monkeypatch.setattr(control, "read_heartbeat", lambda repo: {"pid": 4242})
    r = control.start({"name": "x", "path": str(tmp_path)})
    assert r.get("already") and r.get("pid") == 4242
    assert (rt / "stop").exists()                            # the live Stop was NOT silently revoked
