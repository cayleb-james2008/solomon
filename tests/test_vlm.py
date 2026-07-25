"""Tests for the VLM fallback module."""
from solomon.config import Config
from solomon.vlm import VLMFallback


def test_vlm_detects_vision_capable_models():
    """_is_vision_capable should detect vision-capable model names."""
    cfg = Config.load()
    vlm = VLMFallback(cfg)
    assert vlm._is_vision_capable("gpt-4o")
    assert vlm._is_vision_capable("gpt-4o-mini")
    assert vlm._is_vision_capable("claude-3-opus")
    assert vlm._is_vision_capable("gemini-1.5-pro")
    assert vlm._is_vision_capable("qwen2-vl-7b")
    assert vlm._is_vision_capable("MiniCPM-V-2.6")


def test_vlm_detects_text_only_models():
    """_is_vision_capable should return False for text-only models."""
    cfg = Config.load()
    vlm = VLMFallback(cfg)
    assert not vlm._is_vision_capable("glm-4-flash")
    assert not vlm._is_vision_capable("gpt-3.5-turbo")
    assert not vlm._is_vision_capable("llama-3.1-8b")
    assert not vlm._is_vision_capable("mistral-7b")


def test_vlm_returns_error_for_missing_screenshot():
    """describe_screenshot should return an error for a missing file."""
    import asyncio
    cfg = Config.load()
    vlm = VLMFallback(cfg)
    result = asyncio.get_event_loop().run_until_complete(
        vlm.describe_screenshot("/nonexistent/path.png", "test")
    )
    assert "ERROR" in result
