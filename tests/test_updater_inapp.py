"""In-app updater tests: control.update_status / apply_update + the app.py Api wiring.

All git/subprocess is mocked — no network, no real git, no real build. We drive the behaviour
by faking control._run (the only subprocess entry point these functions use) and the filesystem
probes (_solomon_repo, os.path.isfile).
"""
import os
import subprocess
import sys
import types

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import app  # noqa: E402
import control  # noqa: E402


def _cp(stdout="", returncode=0):
    return subprocess.CompletedProcess(args=[], returncode=returncode, stdout=stdout, stderr="")


def _fake_git(monkeypatch, *, behind="0", ahead="0", dirty="", has_origin=True,
              fetch_rc=0, sha="abc1234", branch="main"):
    """Patch control._run + _which_git + _solomon_repo to simulate a checkout state.
    `behind` = stdout of `git rev-list --count HEAD..origin/<branch>`; `ahead` = stdout of
    `git rev-list --count origin/<branch>..HEAD` (divergence); `fetch_rc` = the fetch exit code
    (non-zero simulates offline); `dirty` = porcelain stdout."""
    monkeypatch.setattr(control, "_which_git", lambda: "git")
    monkeypatch.setattr(control, "_solomon_repo", lambda: "C:/repo")

    def run(args, cwd=None, timeout=None):
        # control._run is called as [git, "-C", repo, *real_args]; strip the first three.
        a = args[3:] if len(args) > 2 and args[1] == "-C" else args[1:]
        if a[:1] == ["rev-parse"] and "--abbrev-ref" in a:
            return _cp(branch)
        if a[:1] == ["rev-parse"] and "--short" in a:
            return _cp(sha)
        if a[:2] == ["remote", "get-url"]:
            return _cp("git@x" if has_origin else "", 0 if has_origin else 1)
        if a[:1] == ["fetch"]:
            return _cp(returncode=fetch_rc)
        if a[:1] == ["status"]:
            return _cp(dirty)
        if a[:1] == ["rev-list"]:
            # update_status now does ONE call: rev-list --left-right --count origin/<b>...HEAD,
            # which prints "behind<TAB>ahead".
            return _cp(f"{behind}\t{ahead}")
        return _cp()

    monkeypatch.setattr(control, "_run", run)


# ── update_status ──────────────────────────────────────────────────────────────
def test_status_available_when_behind_and_clean(monkeypatch):
    _fake_git(monkeypatch, behind="3", dirty="")
    s = control.update_status()
    assert s["ok"] and s["available"] is True
    assert s["behind"] == 3 and s["dirty"] is False and s["currentSha"] == "abc1234"


def test_status_not_available_when_up_to_date(monkeypatch):
    _fake_git(monkeypatch, behind="0", dirty="")
    s = control.update_status()
    assert s["ok"] and s["available"] is False and s["behind"] == 0


def test_status_not_available_when_dirty_even_if_behind(monkeypatch):
    _fake_git(monkeypatch, behind="2", dirty=" M control.py")
    s = control.update_status()
    assert s["ok"] and s["available"] is False
    assert s["behind"] == 2 and s["dirty"] is True and "dirty" in s["reason"]


def test_status_no_origin(monkeypatch):
    _fake_git(monkeypatch, behind="5", has_origin=False)
    s = control.update_status()
    assert s["ok"] and s["available"] is False and "origin" in s["reason"]


def test_status_no_repo(monkeypatch):
    monkeypatch.setattr(control, "_solomon_repo", lambda: None)
    s = control.update_status()
    assert s["ok"] is False and s["available"] is False


def test_status_not_available_when_diverged(monkeypatch):
    # local is BOTH behind and ahead (diverged) -> _pull_latest would refuse --ff-only, so update_status
    # must NOT offer it; report unavailable with a divergence reason, mirroring the updater.
    _fake_git(monkeypatch, behind="1", ahead="1")
    s = control.update_status()
    assert s["ok"] and s["available"] is False and s["ahead"] == 1
    assert "diverged" in s["reason"]


def test_status_unavailable_when_fetch_fails(monkeypatch):
    # a failed fetch (offline) must not let a STALE origin ref report 'available' -> report unavailable.
    _fake_git(monkeypatch, behind="3", fetch_rc=1)
    s = control.update_status()
    assert s["ok"] and s["available"] is False
    assert "origin" in s["reason"]


def test_apply_update_strips_pythonhome(monkeypatch, tmp_path):
    # apply_update must spawn the updater with _clean_subenv (no leaked PYTHONHOME/PYTHONPATH that would
    # crash the maki-venv 3.12 updater python / its PyInstaller rebuild at interpreter startup).
    monkeypatch.setenv("PYTHONHOME", "C:/other/python")
    monkeypatch.setenv("PYTHONPATH", "C:/junk")
    monkeypatch.setattr(control, "_solomon_repo", lambda: str(tmp_path))
    monkeypatch.setattr(control.os.path, "isfile", lambda p: True)   # updater exe "present" -> exe branch
    captured = {}
    monkeypatch.setattr(control.subprocess, "Popen",
                        lambda args, **kw: captured.update(env=kw.get("env")) or types.SimpleNamespace())
    res = control.apply_update()
    assert res["ok"] and res["started"]
    assert "PYTHONHOME" not in captured["env"] and "PYTHONPATH" not in captured["env"]
    assert captured["env"].get("SOLOMON_HOME") == str(tmp_path)


def test_current_sha(monkeypatch):
    _fake_git(monkeypatch, sha="deadbee")
    assert control.current_sha() == "deadbee"


# ── apply_update ────────────────────────────────────────────────────────────────
def test_apply_spawns_exe_when_present(monkeypatch):
    monkeypatch.setattr(control, "_solomon_repo", lambda: "C:/repo")
    exe = os.path.join("C:/repo", "dist", "updater", "SolomonUpdater", "SolomonUpdater.exe")
    monkeypatch.setattr(control.os.path, "isfile", lambda p: p == exe)
    spawned = {}

    def popen(args, **kw):
        spawned["args"] = args
        spawned["kw"] = kw
        return types.SimpleNamespace(pid=4321)

    monkeypatch.setattr(control.subprocess, "Popen", popen)
    res = control.apply_update()
    assert res["ok"] and res["started"] and res["mode"] == "exe"
    assert spawned["args"] == [exe]


def test_apply_falls_back_to_source_when_exe_absent(monkeypatch):
    monkeypatch.setattr(control, "_solomon_repo", lambda: "C:/repo")
    build_py = "C:/venv/python.exe"
    # exe absent; build python present
    monkeypatch.setattr(control.os.path, "isfile", lambda p: p == build_py)
    monkeypatch.setenv("SOLOMON_BUILD_PY", build_py)
    spawned = {}

    def popen(args, **kw):
        spawned["args"] = args
        return types.SimpleNamespace(pid=99)

    monkeypatch.setattr(control.subprocess, "Popen", popen)
    res = control.apply_update()
    assert res["ok"] and res["started"] and res["mode"] == "source"
    assert spawned["args"][0] == build_py and spawned["args"][1].endswith("updater.py")


def test_apply_errors_when_no_repo(monkeypatch):
    monkeypatch.setattr(control, "_solomon_repo", lambda: None)
    res = control.apply_update()
    assert res["ok"] is False and res["started"] is False


# ── app.py Api wiring ────────────────────────────────────────────────────────────
def test_api_update_status_caches(monkeypatch):
    monkeypatch.setattr(control, "update_status",
                        lambda: {"ok": True, "available": True, "behind": 1, "dirty": False})
    api = app.Api()
    assert api.cached_update_status() is None         # nothing yet
    out = api.update_status()
    assert out["available"] is True
    assert api.cached_update_status() == out          # now cached


def test_api_bg_check_populates_cache(monkeypatch):
    monkeypatch.setattr(control, "update_status",
                        lambda: {"ok": True, "available": False, "behind": 0})
    api = app.Api()
    api._bg_update_check()
    assert api._update == {"ok": True, "available": False, "behind": 0}


def test_api_apply_signals_exit(monkeypatch):
    monkeypatch.setattr(control, "apply_update", lambda: {"ok": True, "started": True, "mode": "exe"})
    destroyed = {"calls": 0}

    class FakeTimer:
        """Run the callback synchronously so the test asserts the exit was scheduled."""
        def __init__(self, _delay, fn):
            self.fn = fn

        def start(self):
            self.fn()

    monkeypatch.setattr("threading.Timer", FakeTimer)
    api = app.Api()
    api._window = types.SimpleNamespace(destroy=lambda: destroyed.__setitem__("calls", destroyed["calls"] + 1))
    res = api.apply_update()
    assert res["started"] is True
    assert destroyed["calls"] == 1                    # window close was scheduled + fired


def test_api_apply_no_exit_when_not_started(monkeypatch):
    monkeypatch.setattr(control, "apply_update", lambda: {"ok": False, "started": False, "error": "x"})
    api = app.Api()
    api._window = types.SimpleNamespace(destroy=lambda: (_ for _ in ()).throw(AssertionError("must not close")))
    res = api.apply_update()
    assert res["started"] is False                    # destroy never called (would raise)
