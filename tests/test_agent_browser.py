"""Tests for the agent-browser bridge (Feature 1b) — the Python side writes live state to
runtime/<name>/browser_state.json for the in-app panel; the node/Playwright driver is
exercised only when node + playwright are present (skipped otherwise)."""
import json
import os
import sys
from pathlib import Path

import pytest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "improver"))

import agent_browser  # noqa: E402


def test_agent_browser_write_state(tmp_path):
    """write_state atomically writes the live snapshot the panel reads."""
    ab = agent_browser.AgentBrowser(repo_path=str(tmp_path), runtime_dir=tmp_path / "rt")
    ab.write_state(ok=True, url="http://x/", screenshot_b64="abc",
                   cursor={"x": 42, "y": 58, "click": False}, status="live", phase="active")
    snap = json.loads((tmp_path / "rt" / "browser_state.json").read_text(encoding="utf-8"))
    assert snap["ok"] is True
    assert snap["url"] == "http://x/"
    assert snap["cursor"] == {"x": 42, "y": 58, "click": False}
    assert snap["phase"] == "active"
    assert "ts" in snap


def test_agent_browser_close_clears_state(tmp_path):
    """On close, the bridge clears browser_state.json so the panel returns to empty state."""
    ab = agent_browser.AgentBrowser(repo_path=str(tmp_path), runtime_dir=tmp_path / "rt")
    ab.write_state(ok=True, url="http://x/", screenshot_b64="abc")
    assert (tmp_path / "rt" / "browser_state.json").exists()
    ab.close()
    assert not (tmp_path / "rt" / "browser_state.json").exists()


def test_agent_browser_context_manager(tmp_path):
    """The context manager writes a starting state and clears it on exit."""
    rt = tmp_path / "rt"
    with agent_browser.AgentBrowser(repo_path=str(tmp_path), runtime_dir=rt) as ab:
        assert (rt / "browser_state.json").exists()
        snap = json.loads((rt / "browser_state.json").read_text(encoding="utf-8"))
        assert snap["ok"] is True and snap["status"] == "starting"
    # exited -> state cleared
    assert not (rt / "browser_state.json").exists()


def test_agent_browser_missing_cli_fails_closed(tmp_path, monkeypatch):
    """When node is not installed, _run_driver writes {ok:false} to the panel state and
    returns a clear error — never raises into the RSI loop."""
    monkeypatch.setattr(agent_browser.shutil, "which", lambda name: None)
    ab = agent_browser.AgentBrowser(repo_path=str(tmp_path), runtime_dir=tmp_path / "rt")
    out = ab.navigate("http://x/")
    assert out["ok"] is False
    assert "agent-browser" in out["error"]
    # the panel state reflects the failure
    snap = json.loads((tmp_path / "rt" / "browser_state.json").read_text(encoding="utf-8"))
    assert snap["ok"] is False


def test_agent_browser_reuses_named_session_and_sequences_observations(tmp_path, monkeypatch):
    calls = []

    class Result:
        returncode = 0
        stderr = ""
        stdout = json.dumps({"success": True, "data": {"url": "http://127.0.0.1:8000/",
                                                         "title": "Fixture", "refs": {},
                                                         "snapshot": "(no interactive elements)"}})

    monkeypatch.setattr(agent_browser.shutil, "which", lambda name: "agent-browser.exe")
    monkeypatch.setattr(agent_browser.subprocess, "run",
                        lambda args, **kwargs: calls.append(args) or Result())
    ab = agent_browser.AgentBrowser(str(tmp_path), tmp_path / "rt",
                                    allowed_origins=["http://127.0.0.1:8000"])
    first = ab.navigate("http://127.0.0.1:8000/")
    second = ab.observe()
    assert first["ok"] and second["ok"]
    assert second["seq"] > first["seq"]
    assert calls and all("--session" in call and ab.session_id in call for call in calls)


def test_agent_browser_rejects_external_navigation_without_spawning(tmp_path, monkeypatch):
    called = False
    monkeypatch.setattr(agent_browser.shutil, "which", lambda name: "agent-browser.exe")

    def run(*args, **kwargs):
        nonlocal called
        called = True

    monkeypatch.setattr(agent_browser.subprocess, "run", run)
    ab = agent_browser.AgentBrowser(str(tmp_path), tmp_path / "rt",
                                    allowed_origins=["http://127.0.0.1:8000"])
    out = ab.navigate("https://example.com/")
    assert out["ok"] is False and "allowed" in out["error"].lower()
    assert called is False


def test_agent_browser_rejects_stale_ref(tmp_path):
    ab = agent_browser.AgentBrowser(str(tmp_path), tmp_path / "rt")
    ab._seq = 7
    out = ab.act({"kind": "click", "ref": "@e1", "observation_seq": 6})
    assert out["ok"] is False and "stale" in out["error"].lower()
