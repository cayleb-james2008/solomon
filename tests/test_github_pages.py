"""Tests for the GitHub Pages channel."""
import asyncio
from unittest.mock import AsyncMock, MagicMock, patch

from solomon.channels.github_pages import GithubPagesChannel


def test_github_pages_channel_name():
    ch = GithubPagesChannel()
    assert ch.name == "github_pages"


def test_github_pages_channel_inherits_channel():
    from solomon.channels import Channel
    assert isinstance(GithubPagesChannel(), Channel)
