"""Tests for the AI wrapper channel."""
import asyncio
import pytest
from unittest.mock import AsyncMock, MagicMock

from solomon.channels.ai_wrapper import AIWrapperChannel


def test_ai_wrapper_channel_name():
    ch = AIWrapperChannel()
    assert ch.name == "ai_wrapper"


def test_ai_wrapper_channel_inherits_channel():
    from solomon.channels import Channel
    assert isinstance(AIWrapperChannel(), Channel)


def test_ai_wrapper_discover_returns_dict():
    """discover should return a dict with summary and opportunities."""
    ch = AIWrapperChannel()
    llm = AsyncMock()
    # First call: tool name, second call: description
    llm.ask = AsyncMock(side_effect=["test-tool", "A test tool for developers"])
    browser = MagicMock()
    vlm = AsyncMock()
    result = asyncio.get_event_loop().run_until_complete(ch.discover(browser, llm, vlm))
    assert "summary" in result
    assert "opportunities" in result
    assert len(result["opportunities"]) == 1
    assert result["opportunities"][0]["tool_name"] == "test-tool"


def test_ai_wrapper_discover_fallback_on_error():
    """discover should return a fallback idea if LLM fails."""
    ch = AIWrapperChannel()
    llm = AsyncMock()
    llm.ask = AsyncMock(side_effect=Exception("API down"))
    browser = MagicMock()
    vlm = AsyncMock()
    result = asyncio.get_event_loop().run_until_complete(ch.discover(browser, llm, vlm))
    assert "opportunities" in result
    assert result["opportunities"][0]["tool_name"] == "commit-msg-ai"
