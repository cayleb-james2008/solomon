"""Content publishing channel — find trending topics, draft articles.

The agent finds trending topics via Hacker News / RSS feeds, drafts articles via
the LLM, and saves them to runtime/articles/ as markdown for review. Medium/Dev.to
publishing via their API is a config-gated submit (off by default).
"""
import json
import os
from datetime import datetime, timezone
from pathlib import Path

import httpx

from . import Channel


class ContentChannel(Channel):
    name = "content"

    async def discover(self, browser, llm, vlm) -> dict:
        """Find trending topics from Hacker News."""
        topics = []
        errors = []

        # Hacker News — top stories (public API, no auth)
        try:
            async with httpx.AsyncClient(timeout=15) as client:
                resp = await client.get("https://hacker-news.firebaseio.com/v0/topstories.json")
                resp.raise_for_status()
                story_ids = resp.json()[:5]  # top 5

                for sid in story_ids:
                    item_resp = await client.get(f"https://hacker-news.firebaseio.com/v0/item/{sid}.json")
                    item_resp.raise_for_status()
                    item = item_resp.json()
                    if item and item.get("title"):
                        topics.append({
                            "title": item["title"],
                            "url": item.get("url", ""),
                            "score": item.get("score", 0),
                            "hn_url": f"https://news.ycombinator.com/item?id={sid}",
                        })
        except Exception as e:
            errors.append(f"hackernews: {e}")

        # Use LLM to pick the best topic for content generation
        if topics and not errors:
            try:
                topic_list = "\n".join(f"- {t['title']} (score: {t['score']})" for t in topics)
                topic = await llm.ask(
                    "You are a content strategist. Pick the ONE topic from this list that would make the best article for Medium/Dev.to. Respond with just the topic title.",
                    topic_list,
                )
            except Exception:
                topic = topics[0]["title"] if topics else ""
        else:
            topic = ""

        summary = f"{len(topics)} trending topics found" + (f", best: {topic[:50]}" if topic else "")
        return {"summary": summary, "opportunities": [{"topics": topics, "best_pick": topic}]}

    async def act(self, browser, llm, vlm, decision: dict) -> dict:
        """Draft an article based on the trending topic and save it."""
        # Get the topic from discover's opportunities or the decision
        opportunities = decision.get("opportunities", [])
        if opportunities:
            opp = opportunities[0] if isinstance(opportunities, list) else opportunities
            topic = opp.get("best_pick", "")
        else:
            topic = decision.get("topic", "")

        if not topic:
            return {"summary": "no topic to write about", "revenue_usd": None}

        # Ask the LLM to draft an article
        system = """You are Solomon, an autonomous content creator. Write a well-structured, engaging article suitable for Medium or Dev.to. The article should:
- Be 800-1500 words
- Have a clear introduction, body, and conclusion
- Use markdown formatting with headers, lists, and code blocks
- Be original and well-researched
- Include SEO-friendly title and meta description"""

        user = f"Write an article about: {topic}"

        try:
            article = await llm.ask(system, user, temperature=0.8)
        except Exception as e:
            return {"summary": f"LLM article draft failed: {e}", "revenue_usd": None}

        # Save the article for operator review
        ts = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
        article_path = browser.cfg.articles_dir / f"article_{ts}.md"
        article_path.write_text(
            f"---\ntitle: {topic}\ndate: {ts}\nstatus: draft\n---\n\n{article}\n",
            encoding="utf-8",
        )

        if browser.cfg.auto_submit:
            # Try to publish to Dev.to if we have an API token
            result = await self._publish_devto(browser, topic, article, ts)
            return result
        else:
            return {
                "summary": f"article drafted and saved to {article_path.name} (operator review required)",
                "revenue_usd": None,
                "source": "content",
                "note": f"Draft saved to {article_path}",
            }

    async def _publish_devto(self, browser, title: str, body_markdown: str, ts: str) -> dict:
        """Publish an article to Dev.to via their free API.

        Requires DEVTO_API_KEY in .env. Get one at https://dev.to/settings/extensions
        """
        api_key = os.getenv("DEVTO_API_KEY", "")
        if not api_key:
            return {
                "summary": "DEVTO_API_KEY not set — article draft saved, cannot auto-publish",
                "revenue_usd": None,
                "source": "content",
                "note": "Set DEVTO_API_KEY in .env to enable Dev.to publishing",
            }

        try:
            async with httpx.AsyncClient(timeout=30) as client:
                resp = await client.post(
                    "https://dev.to/api/articles",
                    headers={"api-key": api_key, "content-type": "application/json"},
                    json={
                        "article": {
                            "title": title,
                            "body_markdown": body_markdown,
                            "published": True,
                            "tags": ["ai", "automation"],
                        }
                    },
                )
                if resp.status_code in (200, 201):
                    data = resp.json()
                    url = data.get("url", "")
                    return {
                        "summary": f"article published to Dev.to: {url}",
                        "revenue_usd": None,
                        "source": "content",
                        "note": f"Published at {url}",
                    }
                else:
                    return {
                        "summary": f"Dev.to publish failed: HTTP {resp.status_code}",
                        "revenue_usd": None,
                        "source": "content",
                        "note": f"Error: {resp.text[:200]}",
                    }
        except Exception as e:
            return {
                "summary": f"Dev.to publish error: {e}",
                "revenue_usd": None,
                "source": "content",
            }
