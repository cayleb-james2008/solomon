import os
from pathlib import Path

import pytest

from improver import sandbox


def test_clean_env_rejects_secret_extra_env(monkeypatch):
    monkeypatch.setenv("OPENROUTER_API_KEY", "real-secret")
    with pytest.raises(ValueError, match="secret-shaped"):
        sandbox._clean_sandbox_env({"MY_TOKEN": "oops"})


def test_clean_env_reroots_user_state(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", "C:/real-home")
    env = sandbox._clean_sandbox_env({"SAFE_FLAG": "1"}, state_root=tmp_path)
    assert env["HOME"] == str(tmp_path)
    assert env["USERPROFILE"] == str(tmp_path)
    assert env["TEMP"].startswith(str(tmp_path))
    assert "OPENROUTER_API_KEY" not in env


def test_materialized_source_keeps_writes_out_of_repo(tmp_path):
    repo = tmp_path / "repo"
    repo.mkdir()
    (repo / "app.py").write_text("print('ok')", encoding="utf-8")
    (repo / ".env").write_text("SECRET=yes", encoding="utf-8")
    sb = sandbox.Sandbox({"launch": ["python", "app.py"]}, str(repo))
    sb._create_isolation_root()
    try:
        assert sb.work_dir != str(repo)
        assert (Path(sb.work_dir) / "app.py").exists()
        assert not (Path(sb.work_dir) / ".env").exists()
        (Path(sb.work_dir) / "created.txt").write_text("sandbox", encoding="utf-8")
        assert not (repo / "created.txt").exists()
    finally:
        sb._cleanup()


def test_launch_command_is_argv_not_shell(tmp_path, monkeypatch):
    repo = tmp_path / "repo"
    repo.mkdir()
    (repo / "index.html").write_text("ok", encoding="utf-8")
    captured = {}

    class Proc:
        pid = 123
        returncode = None

        def poll(self): return None
        def wait(self, timeout=None): return 0
        def kill(self): pass

    monkeypatch.setattr(sandbox.subprocess, "Popen",
                        lambda args, **kwargs: captured.update(args=args, kwargs=kwargs) or Proc())
    monkeypatch.setattr(sandbox.Sandbox, "_wait_health", lambda self: None)
    monkeypatch.setattr(sandbox.Sandbox, "_terminate_process_tree", lambda self: None)
    sb = sandbox.Sandbox({"launch": ["python", "-m", "http.server", "{port}"]}, str(repo))
    with sb:
        assert isinstance(captured["args"], list)
        assert captured["kwargs"].get("shell") is False
        assert captured["kwargs"]["cwd"] != str(repo)


def test_command_passes_literal_braces_through(tmp_path):
    # a launch arg with literal { } (JSON blob / brace-glob / {0}) must NOT crash _command via str.format
    repo = tmp_path / "repo"; repo.mkdir()
    sb = sandbox.Sandbox(
        {"launch": ["node", "server.js", '--cfg={"host":"127.0.0.1"}', "src/**/*.{js,ts}", "{port}"]},
        str(repo))
    sb.port = 5173
    sb.state_dir = str(tmp_path / "s")
    sb.work_dir = str(repo)
    args = sb._command()
    assert '--cfg={"host":"127.0.0.1"}' in args             # literal braces preserved (no KeyError)
    assert "src/**/*.{js,ts}" in args
    assert "5173" in args                                   # {port} still substituted


def test_enter_cleans_up_on_boot_failure(tmp_path):
    # a missing launch binary (Popen FileNotFoundError) before _wait_health must NOT leak the repo-mirror
    # temp dir — __exit__ never runs when __enter__ raises, so the boot must self-clean.
    import glob, tempfile
    repo = tmp_path / "repo"; repo.mkdir()
    (repo / "app.py").write_text("print(1)", encoding="utf-8")
    before = set(glob.glob(os.path.join(tempfile.gettempdir(), "solomon-sandbox-*")))
    sb = sandbox.Sandbox({"launch": ["this_binary_does_not_exist_xyz_123", "app.py"]}, str(repo))
    with pytest.raises(Exception):
        sb.__enter__()
    after = set(glob.glob(os.path.join(tempfile.gettempdir(), "solomon-sandbox-*")))
    assert after == before                                  # no leaked sandbox temp dir
