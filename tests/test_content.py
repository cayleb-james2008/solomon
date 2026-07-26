"""Tests for the content channel: spotlight rotation, title guard, CTA footer."""
import asyncio
from types import SimpleNamespace

from solomon.channels.content import ContentChannelV2


def _channel():
    return ContentChannelV2()


# --- spotlight rotation ---

def test_spotlight_picks_first_unfeatured_tool():
    tools = [{"name": "text-summarizer"}, {"name": "mood-analyzer"}]
    sp = _channel()._next_spotlight(tools, set())
    assert sp and sp["tool"]["name"] == "text-summarizer"
    assert "I Built a Free Text Summarizer" in sp["title"]


def test_spotlight_skips_tools_with_built_articles():
    tools = [{"name": "text-summarizer"}, {"name": "mood-analyzer"}]
    published = {"i built a free text summarizer no signup no subscription"}
    sp = _channel()._next_spotlight(tools, published)
    assert sp and sp["tool"]["name"] == "mood-analyzer"


def test_spotlight_none_when_all_featured():
    tools = [{"name": "mood-analyzer"}]
    published = {"i built a free mood analyzer no signup"}
    assert _channel()._next_spotlight(tools, published) is None


# --- publish path ---

class _FakeResp:
    status_code = 201

    def json(self):
        return {"url": "https://dev.to/solomon_dev/test"}


class _FakeClient:
    captured = None

    def __init__(self, *a, **k):
        pass

    async def __aenter__(self):
        return self

    async def __aexit__(self, *a):
        return False

    async def post(self, url, headers=None, json=None):
        _FakeClient.captured = json
        return _FakeResp()


def _browser():
    return SimpleNamespace(cfg=SimpleNamespace(auto_submit=True))


def test_publish_force_title_and_cta_footer(monkeypatch):
    monkeypatch.setattr("solomon.channels.content.httpx.AsyncClient", _FakeClient)
    monkeypatch.setenv("DEVTO_API_KEY", "test-key")
    monkeypatch.delenv("CF_SUBDOMAIN", raising=False)
    res = asyncio.run(_channel()._publish_devto(
        _browser(),
        "---\ntitle: Short\n---\n\nArticle body here.",
        topic_title="I Built a Free Mood Analyzer — No Signup, No Subscription",
        force_title=True,
        tags_override=["showdev", "ai"],
    ))
    art = _FakeClient.captured["article"]
    assert art["title"] == "I Built a Free Mood Analyzer — No Signup, No Subscription"
    assert art["tags"] == ["showdev", "ai"]
    # CTA footer regression: live subdomain, never the deleted one
    assert "solomontools.workers.dev" in art["body_markdown"]
    assert "caylebalvarezjames" not in art["body_markdown"]
    assert "PUBLISHED" in res["summary"]


def test_publish_short_title_falls_back_to_topic(monkeypatch):
    monkeypatch.setattr("solomon.channels.content.httpx.AsyncClient", _FakeClient)
    monkeypatch.setenv("DEVTO_API_KEY", "test-key")
    asyncio.run(_channel()._publish_devto(
        _browser(),
        "---\ntitle: SIMD for Collision\n---\n\nBody.",
        topic_title="SIMD for Collision Detection in Games: A Practical Guide",
    ))
    assert _FakeClient.captured["article"]["title"] == "SIMD for Collision Detection in Games: A Practical Guide"


def test_successful_title_is_recorded_locally(tmp_path):
    cfg = SimpleNamespace(runtime_dir=tmp_path)
    browser = SimpleNamespace(cfg=cfg)
    channel = _channel()
    channel._record_published_title(browser, "I Built a Free Text Summarizer — No Signup")
    channel._record_published_title(browser, "I Built a Free Text Summarizer — No Signup")
    saved = (tmp_path / "content_published_titles.json").read_text(encoding="utf-8")
    assert saved.count("i built a free text summarizer no signup") == 1
    assert channel._next_spotlight(
        [{"name": "text-summarizer"}],
        {"i built a free text summarizer no signup"},
    ) is None
