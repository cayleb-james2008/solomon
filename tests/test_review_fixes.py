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
    # COLLECTED count drop is gamed even when 'passed' is held steady (tests deleted + a trivial one added)
    assert "collected count fell" in m._anti_gaming_reason(
        {"passed": 10, "collected": 10}, {"passed": 10, "collected": 7}, "")
    # collected held steady -> not gamed
    assert m._anti_gaming_reason({"passed": 10, "collected": 10},
                                 {"passed": 10, "collected": 10}, "") is None


def test_new_skip_markers_catches_non_decorator_forms():
    m = _load_runner()
    diff = ("+    pytest.skip('wip')\n"            # in-body call
            "+    pytest.xfail()\n"
            "+    @pytest.mark.skipif(True, reason='x')\n"
            "+        self.skipTest('nope')\n"
            "+    raise unittest.SkipTest\n"
            "+    raise SkipTest('bare')\n"
            "+    assert real()\n"                  # NOT a skip
            "+++ b/tests/new_test.py\n")            # file header, must be ignored
    skips = m._new_skip_markers(diff)
    assert len(skips) == 6                          # all six skip forms, header + real assertion excluded


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
    m.HEARTBEAT = tmp_path / "heartbeat.json"   # nonexistent by default -> _heartbeat_stale() is False
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
    assert m._read_lock_pid() == os.getpid()                  # lock now holds '<pid>\n<run_id>'


def test_acquire_lock_creates_with_pid(tmp_path):
    m = _runner_with_lock(tmp_path)
    assert not m.LOCK.exists()
    assert m.acquire_lock() is True
    assert m._read_lock_pid() == os.getpid()                               # pid on line 1, never empty
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


# --------------------------------------------------------------------------- #
# DUP-PR — _open_pr ADOPTS an existing PR on 'already exists' instead of falling to push-only.
# Regression for the live sover dup-PR spin: the agent opened the PR itself, the runner's
# 'gh pr create' then failed "already exists", the item never ticked, and the same backlog
# item re-shipped every iteration (PRs #18/#19 accumulated).
# --------------------------------------------------------------------------- #
def test_open_pr_adopts_existing_pr_on_already_exists(monkeypatch):
    m = _load_runner()
    monkeypatch.setattr(m, "gh_exe", lambda: "gh")
    monkeypatch.setattr(m, "BASE_BRANCH", "main")
    monkeypatch.setattr(m, "BEAUTIFY", False)

    def fake_run(args, **k):
        if "create" in args:
            return type("P", (), {"returncode": 1, "stdout": "",
                                  "stderr": 'a pull request for branch "rsi/iter-x" into "main" '
                                            'already exists:\nhttps://github.com/o/r/pull/18'})()
        if "list" in args:
            return type("P", (), {"returncode": 0, "stderr": "",
                                  "stdout": json.dumps([{"number": 18, "url": "https://github.com/o/r/pull/18"}])})()
        return type("P", (), {"returncode": 0, "stdout": "", "stderr": ""})()
    monkeypatch.setattr(m.subprocess, "run", fake_run)

    pr = m._open_pr("rsi/iter-x", "title", "summary", {"passed": 5, "failed": 0})
    assert pr["number"] == 18 and pr["state"] == "open"
    assert m._ship_succeeded(pr) is True              # an adopted PR ticks the backlog item (loop advances)


def test_open_pr_push_only_when_create_fails_without_existing_pr(monkeypatch):
    m = _load_runner()
    monkeypatch.setattr(m, "gh_exe", lambda: "gh")
    monkeypatch.setattr(m, "BASE_BRANCH", "main")
    monkeypatch.setattr(m, "BEAUTIFY", False)

    def fake_run(args, **k):
        if "create" in args:
            return type("P", (), {"returncode": 1, "stdout": "", "stderr": "some transient gh error"})()
        if "list" in args:
            return type("P", (), {"returncode": 0, "stdout": "[]", "stderr": ""})()
        return type("P", (), {"returncode": 0, "stdout": "", "stderr": ""})()
    monkeypatch.setattr(m.subprocess, "run", fake_run)

    pr = m._open_pr("rsi/iter-y", "title", "summary", None)
    assert pr["number"] is None and pr["state"] == "push-only"
    assert m._ship_succeeded(pr) is False             # a genuine create failure must NOT tick the item


# --------------------------------------------------------------------------- #
# NARRATE-1 — _narrated_without_writing flags a clean-tree no-op whose summary CLAIMS file
# writes (a weak model hallucinating its tool calls — the asmodeus/kimi no-op storm).
# --------------------------------------------------------------------------- #
def test_narrated_without_writing_flags_claimed_files():
    m = _load_runner()
    assert m._narrated_without_writing("Added `tests/test_subprocess_util.py` with 17 tests.") is True
    assert m._narrated_without_writing("Created backend/tvmaze.py and wired it in.") is True
    assert m._narrated_without_writing("I added tests in test_helpers.py for the loader.") is True
    # honest no-op (no claim of writing a file) -> not flagged
    assert m._narrated_without_writing("The item is already fully implemented; no change needed.") is False
    assert m._narrated_without_writing("(no summary returned)") is False
    assert m._narrated_without_writing("") is False
    assert m._narrated_without_writing(None) is False


# --------------------------------------------------------------------------- #
# CFG-1 — the loop re-reads repos.json each iteration so a dashboard model/goal/gate edit
# takes effect without a stop+restart (asmodeus stayed on kimi after a glm-5.2 switch).
# --------------------------------------------------------------------------- #
def test_refresh_config_picks_up_new_model_and_goal(tmp_path, monkeypatch):
    m = _load_runner()
    monkeypatch.setattr(m, "CONTROL", tmp_path)
    monkeypatch.setattr(m, "NAME", "asmodeus")
    m.PI_MODEL = "kimi-k2.7-code"; m.GOAL = ""; m.GATE_CMD = ""; m.REASONING = ""
    (tmp_path / "repos.json").write_text(json.dumps([
        {"name": "asmodeus", "provider": "ollama-cloud", "model": "glm-5.2",
         "gate": ".venv\\Scripts\\python -m pytest", "reasoning": "xhigh", "goal": "be excellent"},
        {"name": "other", "model": "should-not-pick"},
    ]), encoding="utf-8")
    m._refresh_config_from_registry()
    assert m.PI_MODEL == "glm-5.2"
    assert m._hb["model"] == "glm-5.2"
    assert m.GOAL == "be excellent"
    assert m.REASONING == "xhigh"
    assert m.GATE_CMD.endswith("pytest")


def test_refresh_config_tolerates_missing_or_torn_file(tmp_path, monkeypatch):
    m = _load_runner()
    monkeypatch.setattr(m, "CONTROL", tmp_path)        # no repos.json present
    monkeypatch.setattr(m, "NAME", "asmodeus")
    m.PI_MODEL = "keep-me"
    m._refresh_config_from_registry()                  # missing file -> no change, no raise
    assert m.PI_MODEL == "keep-me"
    (tmp_path / "repos.json").write_text("{ this is not json", encoding="utf-8")
    m._refresh_config_from_registry()                  # torn/invalid -> no change, no raise
    assert m.PI_MODEL == "keep-me"


# --------------------------------------------------------------------------- #
# PID-1 — recycled-PID-proof liveness (run-id identity token + heartbeat freshness).
# Windows recycles PIDs; the old _pid_alive-only check pinned a dead loop 'running' forever and
# let clear_lock refuse a recycled-pid lock. The runner now writes '<pid>\n<run_id>' and takes over
# a lock whose holder's heartbeat froze; control.is_running/clear_lock corroborate freshness+run-id.
# --------------------------------------------------------------------------- #
import datetime as _dt


def _iso(offset_s=0):
    return (_dt.datetime.now(_dt.timezone.utc) + _dt.timedelta(seconds=offset_s)).strftime("%Y-%m-%dT%H:%M:%SZ")


def test_acquire_lock_writes_pid_and_run_id(tmp_path):
    m = _runner_with_lock(tmp_path)
    assert m.acquire_lock() is True
    pid_line, run_line = m.LOCK.read_text(encoding="utf-8").splitlines()
    assert pid_line.strip() == str(os.getpid())
    assert run_line.strip() == m.RUN_ID                  # identity token persisted on the 2nd line
    assert m._read_lock_pid() == os.getpid()             # first line still parses as the pid


def test_read_lock_pid_parses_multiline_and_legacy(tmp_path):
    m = _runner_with_lock(tmp_path)
    m.LOCK.write_text("4242\nsomerunid", encoding="utf-8")
    assert m._read_lock_pid() == 4242                    # multi-line: pid from the first line
    m.LOCK.write_text("4242", encoding="utf-8")
    assert m._read_lock_pid() == 4242                    # legacy single-line still works
    m.LOCK.write_text("garbage\nx", encoding="utf-8")
    assert m._read_lock_pid() == 0                        # corrupt first line -> 0


def test_acquire_lock_takes_over_recycled_pid_with_stale_heartbeat(tmp_path, monkeypatch):
    m = _runner_with_lock(tmp_path)
    m.INTERVAL = 120
    monkeypatch.setattr(m, "_pid_alive", lambda pid: True)      # the recorded PID is ALIVE (recycled)
    m.LOCK.write_text("99999\noldrunid", encoding="utf-8")
    m.HEARTBEAT.write_text(json.dumps({"updated_at": _iso(-99999), "run_id": "oldrunid"}), encoding="utf-8")
    assert m.acquire_lock() is True                              # stale heartbeat -> holder is dead, take over
    assert m._read_lock_pid() == os.getpid()


def test_acquire_lock_refuses_live_holder_with_fresh_heartbeat(tmp_path, monkeypatch):
    m = _runner_with_lock(tmp_path)
    m.INTERVAL = 120
    monkeypatch.setattr(m, "_pid_alive", lambda pid: True)
    m.LOCK.write_text("99999\notherrun", encoding="utf-8")
    m.HEARTBEAT.write_text(json.dumps({"updated_at": _iso(0), "run_id": "otherrun"}), encoding="utf-8")
    assert m.acquire_lock() is False                            # genuinely live (fresh heartbeat) -> back off
    assert m.LOCK.read_text(encoding="utf-8").splitlines()[0] == "99999"


def _control_repo(tmp_path, monkeypatch):
    repo = {"name": "x", "path": str(tmp_path)}
    rt = tmp_path / "rt"; rt.mkdir()
    monkeypatch.setattr(control, "_runtime_dir", lambda r: str(rt))
    return repo, rt


def test_control_is_running_false_on_recycled_pid_stale_heartbeat(tmp_path, monkeypatch):
    repo, rt = _control_repo(tmp_path, monkeypatch)
    monkeypatch.setattr(control, "_pid_alive", lambda pid: True)   # PID alive (recycled by an unrelated proc)
    (rt / "lock").write_text("4242\nrunabc", encoding="utf-8")
    (rt / "heartbeat.json").write_text(json.dumps({"updated_at": _iso(-99999), "run_id": "runabc"}), encoding="utf-8")
    assert control.is_running(repo) is False                       # stale heartbeat -> dead despite a live PID
    assert control.clear_lock(repo).get("removed") is True         # and clear_lock CAN now remove it
    assert not (rt / "lock").exists()


def test_control_is_running_true_and_clear_lock_refuses_on_fresh(tmp_path, monkeypatch):
    repo, rt = _control_repo(tmp_path, monkeypatch)
    monkeypatch.setattr(control, "_pid_alive", lambda pid: True)
    (rt / "lock").write_text("4242\nrunabc", encoding="utf-8")
    (rt / "heartbeat.json").write_text(json.dumps({"updated_at": _iso(0), "run_id": "runabc"}), encoding="utf-8")
    assert control.is_running(repo) is True
    assert control.clear_lock(repo).get("ok") is False             # refuses to clear a live runner's lock
    assert (rt / "lock").exists()


def test_control_is_running_legacy_pid_only_lock(tmp_path, monkeypatch):
    repo, rt = _control_repo(tmp_path, monkeypatch)
    monkeypatch.setattr(control, "_pid_alive", lambda pid: True)
    (rt / "lock").write_text("4242", encoding="utf-8")             # legacy: no run-id, no heartbeat
    assert control.is_running(repo) is True                        # back-compat: a live PID is enough


def test_control_is_running_run_id_mismatch_is_orphaned(tmp_path, monkeypatch):
    repo, rt = _control_repo(tmp_path, monkeypatch)
    monkeypatch.setattr(control, "_pid_alive", lambda pid: True)
    (rt / "lock").write_text("4242\nOLDrun", encoding="utf-8")     # lock from a prior runner
    (rt / "heartbeat.json").write_text(json.dumps({"updated_at": _iso(0), "run_id": "NEWrun"}), encoding="utf-8")
    assert control.is_running(repo) is False                       # fresh heartbeat under a DIFFERENT run-id


# --------------------------------------------------------------------------- #
# SEC-3 — _redact scrubs the BARE loaded provider key (Ollama keys aren't sk-/gh-shaped, so the
# pattern rails miss a value the agent echoes without a NAME= prefix).
# --------------------------------------------------------------------------- #
def test_redact_scrubs_bare_loaded_provider_key(monkeypatch):
    m = _load_runner()
    monkeypatch.setenv("OLLAMA_API_KEY", "abcd1234efgh5678ijkl")    # bare value, not sk-/gh- shaped
    assert "[REDACTED]" in m._redact("the model echoed abcd1234efgh5678ijkl mid-summary")
    assert "abcd1234" not in m._redact("abcd1234efgh5678ijkl")
    monkeypatch.delenv("OLLAMA_API_KEY", raising=False)
    assert m._redact("ordinary summary text") == "ordinary summary text"   # no key set -> untouched


# --------------------------------------------------------------------------- #
# SHIP-1 — an UNVERIFIED push (rc=0 but the branch isn't on origin) blocks PR-open + records an error
# (SOLOMON_RSI step 7) instead of silently proceeding to a failing gh pr create.
# --------------------------------------------------------------------------- #
def test_ship_push_unverified_blocks_pr(monkeypatch):
    m = _load_runner()
    m.SHIP = "auto-merge"
    monkeypatch.setattr(m, "has_remote", lambda: True)
    monkeypatch.setattr(m, "_gh_ready", lambda: True)
    monkeypatch.setattr(m, "git", lambda *a: type("P", (), {"returncode": 0, "stdout": "", "stderr": ""})())
    monkeypatch.setattr(m, "_branch_on_remote", lambda b: False)   # push 'succeeded' but branch not on origin
    monkeypatch.setattr(m, "heartbeat", lambda **k: None)
    opened = []
    monkeypatch.setattr(m, "_open_pr", lambda *a, **k: opened.append(a) or {"number": 1, "state": "open"})
    pr = m._ship("rsi/iter-x", "t", "s", {"passed": 1})
    assert pr["state"] == "push-unverified (failed)" and opened == []   # never opened a PR
    assert m._ship_succeeded(pr) is False                              # and did not tick the item


# --------------------------------------------------------------------------- #
# AGENT-PATH — the agent subprocess gets a PATH that blocks gh (any) + git push/pull/merge/rebase,
# the operations a too-eager model uses to escape the runner's sandbox (sover dup-PRs, maki's
# conflicted main) despite the explicit contract. Read-only git + pi's own git pass through.
# --------------------------------------------------------------------------- #
def test_agent_shim_dir_blocks_gh_and_mutating_git(tmp_path):
    m = _load_runner()
    m.RUNTIME = tmp_path
    d = m._agent_shim_dir()
    assert d is not None
    gh = (d / "gh").read_text(encoding="utf-8")
    assert "exit 1" in gh and "github_*" in gh                 # gh fully blocked
    assert (d / "gh.cmd").exists()
    if (d / "git").exists():                                   # only when a real git is on PATH (dev/CI both)
        git = (d / "git").read_text(encoding="utf-8")
        assert "push|pull|merge|rebase" in git                 # only the escaping verbs are blocked
        assert "exec" in git                                   # everything else passes through to the real git
        assert (d / "git.cmd").exists()


def test_run_pi_prepends_agent_shims_to_path(tmp_path, monkeypatch):
    m = _load_runner()
    m.RUNTIME = tmp_path
    captured = {}

    class _Proc:
        returncode = 0
        def communicate(self, timeout=None):
            return ("", "")
    monkeypatch.setattr(m.subprocess, "Popen",
                        lambda args, **k: captured.update(env=k.get("env") or {}) or _Proc())
    monkeypatch.setattr(m, "pi_exe", lambda: "pi")
    m.run_pi("do one thing")
    first = captured["env"].get("PATH", "").split(os.pathsep)[0]
    assert first == str(tmp_path / "agent_shims")              # the shim dir is FIRST in the agent's PATH
