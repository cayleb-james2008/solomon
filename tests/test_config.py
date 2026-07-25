"""Tests for the config module."""
import os
from pathlib import Path
from unittest.mock import patch

from solomon.config import Config


def test_config_loads_defaults():
    """Config should load with defaults when env vars are missing."""
    # Clear any env vars that might be set
    for key in list(os.environ):
        if key.startswith("SOLOMON_"):
            del os.environ[key]
    # Mock dotenv to not load the actual .env file
    with patch("solomon.config.load_dotenv"):
        cfg = Config.load()
    assert cfg.channel_freelance is True
    assert cfg.channel_content is True
    assert cfg.channel_microtask is True
    assert cfg.channel_ai_wrapper is True
    assert cfg.auto_submit is False  # safety default
    assert cfg.max_cycles == 1


def test_config_parses_bools():
    """Config should parse bool env vars correctly."""
    os.environ["SOLOMON_CHANNEL_FREELANCE"] = "false"
    os.environ["SOLOMON_AUTO_SUBMIT"] = "true"
    os.environ["SOLOMON_BROWSER_HEADLESS"] = "true"
    try:
        cfg = Config.load()
        assert cfg.channel_freelance is False
        assert cfg.auto_submit is True
        assert cfg.browser_headless is True
    finally:
        del os.environ["SOLOMON_CHANNEL_FREELANCE"]
        del os.environ["SOLOMON_AUTO_SUBMIT"]
        del os.environ["SOLOMON_BROWSER_HEADLESS"]


def test_config_paths_exist():
    """Config should create runtime directories on demand."""
    cfg = Config.load()
    assert cfg.runtime_dir.exists()
    assert cfg.revenue_ledger_path.parent.exists()
