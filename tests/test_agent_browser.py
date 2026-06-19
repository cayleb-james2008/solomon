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


def test_agent_browser_run_driver_missing_node(tmp_path, monkeypatch):
    """When node is not installed, _run_driver writes {ok:false} to the panel state and
    returns a clear error — never raises into the RSI loop."""
    monkeypatch.setattr(agent_browser.shutil, "which", lambda name: None)
    ab = agent_browser.AgentBrowser(repo_path=str(tmp_path), runtime_dir=tmp_path / "rt")
    out = ab.navigate("http://x/")
    assert out["ok"] is False
    assert "node" in out["error"]
    # the panel state reflects the failure
    snap = json.loads((tmp_path / "rt" / "browser_state.json").read_text(encoding="utf-8"))
    assert snap["ok"] is False


def test_agent_browser_run_driver_missing_driver_script(tmp_path, monkeypatch):
    """When the driver script is missing, the bridge writes {ok:false} and returns."""
    monkeypatch.setattr(agent_browser.shutil, "which", lambda name: "node" if name == "node" else None)
    ab = agent_browser.AgentBrowser(repo_path=str(tmp_path), runtime_dir=tmp_path / "rt")
    # point HERE at an empty dir so the driver script isn't found
    monkeypatch.setattr(agent_browser, "__file__", str(tmp_path / "agent_browser.py"))
    out = ab.navigate("http://x/")
    assert out["ok"] is False
    assert "driver" in out["error"].lower()