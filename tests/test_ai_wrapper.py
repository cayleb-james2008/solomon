"""Tests for the AI wrapper channel."""
import asyncio
import pytest
from unittest.mock import AsyncMock, MagicMock

from solomon.channels.ai_wrapper import AIWrapperChannel


def _enable_remote_worker(monkeypatch):
    monkeypatch.setenv("SOLOMON_WORKER_LLM_BASE_URL", "https://worker-provider.example/v1")
    monkeypatch.setenv("SOLOMON_WORKER_LLM_MODEL", "worker-model")
    monkeypatch.setenv("SOLOMON_WORKER_LLM_API_KEY", "remote-worker-test-key")


def test_ai_wrapper_channel_name():
    ch = AIWrapperChannel()
    assert ch.name == "ai_wrapper"


def test_ai_wrapper_channel_inherits_channel():
    from solomon.channels import Channel
    assert isinstance(AIWrapperChannel(), Channel)


def test_ai_wrapper_discover_returns_dict(monkeypatch):
    """discover should return a dict with summary and opportunities."""
    _enable_remote_worker(monkeypatch)
    ch = AIWrapperChannel()
    llm = AsyncMock()
    # First call: tool name, second call: description
    llm.ask = AsyncMock(side_effect=["test-tool", "A test tool for developers"])
    browser = MagicMock()
    vlm = AsyncMock()
    result = asyncio.run(ch.discover(browser, llm, vlm))
    assert "summary" in result
    assert "opportunities" in result
    assert len(result["opportunities"]) == 1
    assert result["opportunities"][0]["tool_name"] == "test-tool"


def test_ai_wrapper_discover_fallback_on_error(monkeypatch):
    """discover should return a fallback idea if LLM fails."""
    _enable_remote_worker(monkeypatch)
    ch = AIWrapperChannel()
    llm = AsyncMock()
    llm.ask = AsyncMock(side_effect=Exception("API down"))
    browser = MagicMock()
    vlm = AsyncMock()
    result = asyncio.run(ch.discover(browser, llm, vlm))
    assert "opportunities" in result
    assert result["opportunities"][0]["tool_name"] == "commit-msg-ai"


def test_ai_wrapper_discover_dedupes_existing_tools(tmp_path, monkeypatch):
    """discover should exclude already-built tools and never re-suggest one."""
    _enable_remote_worker(monkeypatch)
    wrappers = tmp_path / "ai_wrappers"
    wrappers.mkdir()
    (wrappers / "test-tool_20260726_030000").mkdir()
    (wrappers / "other-tool_20260726_040000").mkdir()

    ch = AIWrapperChannel()
    llm = AsyncMock()
    # LLM stubbornly suggests an existing tool name — dedup must catch it
    llm.ask = AsyncMock(side_effect=["test-tool", "A duplicate tool"])
    browser = MagicMock()
    browser.cfg.runtime_dir = tmp_path
    vlm = AsyncMock()
    result = asyncio.run(ch.discover(browser, llm, vlm))
    name = result["opportunities"][0]["tool_name"]
    assert name not in ("test-tool", "other-tool")

    # The exclusion list should have been passed to the LLM
    first_call_system = llm.ask.call_args_list[0][0][0]
    assert "test-tool" in first_call_system and "other-tool" in first_call_system


def test_ai_wrapper_is_disabled_without_explicit_remote_provider(monkeypatch):
    for name in (
        "SOLOMON_WORKER_LLM_BASE_URL",
        "SOLOMON_WORKER_LLM_MODEL",
        "SOLOMON_WORKER_LLM_API_KEY",
    ):
        monkeypatch.delenv(name, raising=False)
    result = asyncio.run(AIWrapperChannel().discover(MagicMock(), AsyncMock(), AsyncMock()))
    assert result["opportunities"] == []
    assert "disabled" in result["summary"]


def test_local_worker_provider_is_rejected(monkeypatch):
    _enable_remote_worker(monkeypatch)
    monkeypatch.setenv("SOLOMON_WORKER_LLM_BASE_URL", "http://localhost:13305/api/v1")
    assert AIWrapperChannel._worker_provider() is None


def test_ai_wrapper_act_fails_closed_before_generation(monkeypatch):
    for name in (
        "SOLOMON_WORKER_LLM_BASE_URL",
        "SOLOMON_WORKER_LLM_MODEL",
        "SOLOMON_WORKER_LLM_API_KEY",
    ):
        monkeypatch.delenv(name, raising=False)
    llm = AsyncMock()
    result = asyncio.run(AIWrapperChannel().act(
        MagicMock(),
        llm,
        AsyncMock(),
        {"opportunities": [{"tool_name": "should-not-build"}]},
    ))
    assert "disabled" in result["summary"]
    llm.ask.assert_not_awaited()
