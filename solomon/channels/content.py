"""Content publishing channel v2 — high-volume SEO articles with affiliate links.

This is Solomon's primary revenue channel. The strategy:
1. DISCOVER: Pull trending topics from multiple sources (HN, Reddit, Dev.to trending)
2. ACT: Generate SEO-optimized articles with:
   - Keyword-targeted titles and meta descriptions
   - Proper heading structure (H1, H2, H3)
   - Affiliate/referral links embedded naturally
   - 1000-2000 words, readable and shareable
3. PUBLISH: Push to Dev.to via their free API (auto-submit when enabled)

Revenue model:
- Dev.to ad revenue share (Partner Program)
- Affiliate commissions from embedded referral links
- Medium Partner Program (when API key added)
- Passive compounding — articles earn for 12+ months
"""
import json
import os
from datetime import datetime, timezone
from pathlib import Path

import httpx

from . import Channel


# SaaS referral programs that pay (zero signup cost, no identity verification)
AFFILIATE_LINKS = {
    "notion": {"url": "https://notion.so/signup", "text": "try Notion for free", "program": "Notion referral"},
    "vercel": {"url": "https://vercel.com/signup", "text": "deploy on Vercel", "program": "Vercel referral"},
    "github": {"url": "https://github.com", "text": "GitHub", "program": "none"},
    "cloudflare": {"url": "https://cloudflare.com", "text": "Cloudflare", "program": "none"},
    "openrouter": {"url": "https://openrouter.ai", "text": "OpenRouter", "program": "none"},
}

# Content categories that perform well on Dev.to
DEVTO_TAGS = ["programming", "ai", "productivity", "webdev", "python", "javascript", "devops", "machinelearning", "tutorial", "beginners", "security", "showdev", "tooling", "opensource"]

# Per-tool tags for spotlight ("I built this") posts — specific tags drive feed reach
SPOTLIGHT_TAGS = {
    "text-summarizer": ["showdev", "ai", "nlp", "productivity"],
    "password-generator": ["showdev", "security", "webdev", "javascript"],
    "summarizy": ["showdev", "ai", "productivity", "writing"],
    "mood-analyzer": ["showdev", "ai", "machinelearning", "webdev"],
}


class ContentChannelV2(Channel):
    name = "content"

    def __init__(self):
        self._topics_cache = []

    async def discover(self, browser, llm, vlm) -> dict:
        """Pull trending topics from multiple sources."""
        topics = []
        errors = []

        # Check what we've already published (skip duplicates)
        already_published = set()
        try:
            api_key = os.getenv("DEVTO_API_KEY", "")
            # Keep a tiny local ledger as a fail-safe when Dev.to intermittently
            # returns 401 from this host. Remote state is still authoritative;
            # local state prevents Solomon from repeating its own last success.
            local_state = browser.cfg.runtime_dir / "content_published_titles.json"
            if local_state.exists():
                saved = json.loads(local_state.read_text(encoding="utf-8"))
                if isinstance(saved, list):
                    already_published.update(
                        self._normalize_title(t) for t in saved if isinstance(t, str)
                    )
            if api_key:
                async with httpx.AsyncClient(timeout=10) as client:
                    # Check BOTH published and unpublished (dupes may be unpublished)
                    for state in ("published", "unpublished"):
                        resp = await client.get(
                            f"https://dev.to/api/articles/me/{state}",
                            params={"per_page": 30},
                            headers={"api-key": api_key, "user-agent": "SolomonBot/1.0"},
                        )
                        if resp.status_code == 200:
                            for a in resp.json():
                                already_published.add(self._normalize_title(a.get("title", "")))
        except Exception:
            pass

        def _is_dupe(title: str, batch: list) -> bool:
            """Fuzzy dedup: normalized title match against published + current batch."""
            norm = self._normalize_title(title)
            if not norm:
                return True
            if norm in already_published:
                return True
            return any(self._normalize_title(t["title"]) == norm for t in batch)

        # Source 1: Hacker News top stories
        try:
            async with httpx.AsyncClient(timeout=15) as client:
                resp = await client.get("https://hacker-news.firebaseio.com/v0/topstories.json")
                resp.raise_for_status()
                for sid in resp.json()[:8]:
                    ir = await client.get(f"https://hacker-news.firebaseio.com/v0/item/{sid}.json")
                    if ir.status_code == 200 and ir.json():
                        item = ir.json()
                        if item.get("title") and item.get("score", 0) > 50:
                            title = item["title"]
                            if not _is_dupe(title, topics):
                                topics.append({
                                    "title": title,
                                    "source": "hackernews",
                                    "score": item.get("score", 0),
                                    "url": item.get("url", ""),
                                    "meta_keywords": self._extract_keywords(title),
                                })
        except Exception as e:
            errors.append(f"hackernews: {e}")

        # Source 2: Dev.to trending articles
        try:
            async with httpx.AsyncClient(timeout=15) as client:
                resp = await client.get("https://dev.to/api/articles?top=7&per_page=10")
                resp.raise_for_status()
                for article in resp.json()[:8]:
                    title = article.get("title", "")
                    if title and not _is_dupe(title, topics):
                        topics.append({
                            "title": title,
                            "source": "devto",
                            "score": article.get("positive_reactions_count", 0),
                            "url": article.get("url", ""),
                            "tags": article.get("tag_list", []),
                            "meta_keywords": self._extract_keywords(title),
                        })
        except Exception as e:
            errors.append(f"devto: {e}")

        # Source 3: tool spotlight — bottom-funnel "I built this" posts (highest conversion).
        # Emit one tool that has no spotlight article yet; rotation exhausts naturally.
        spotlight = None
        try:
            tools_path = browser.cfg.runtime_dir / "ai_wrappers" / "tools.json"
            tools = json.loads(tools_path.read_text(encoding="utf-8"))
            spotlight = self._next_spotlight(tools, already_published)
        except Exception:
            pass

        # Use LLM to pick the best topics for SEO content
        if topics:
            try:
                top_titles = "\n".join(f"- {t['title']} (source: {t['source']}, score: {t.get('score',0)})" for t in topics[:10])
                picks = await llm.ask(
                    """You are an SEO content strategist. Pick the 3 topics from this list that would generate the most organic search traffic if turned into a well-written tutorial/explainer article.

Rules:
- Prefer technical how-to topics over news/opinion
- Prefer topics with long-tail keyword potential
- Return only the 3 titles, one per line, no numbering or explanation.""",
                    f"Topics:\n{top_titles}",
                )
                picked_titles = [l.strip() for l in picks.strip().split("\n") if l.strip()][:3]
                # Match picked titles back to topic data
                picked_topics = []
                for pt in picked_titles:
                    for t in topics:
                        if pt.lower() in t["title"].lower() or t["title"].lower() in pt.lower():
                            picked_topics.append(t)
                            break
                # Intra-batch dedup (normalized titles, keep first occurrence)
                seen = set()
                unique_topics = []
                for t in picked_topics:
                    norm = self._normalize_title(t["title"])
                    if norm and norm not in seen and norm not in already_published:
                        seen.add(norm)
                        unique_topics.append(t)
                if unique_topics:
                    topics = unique_topics[:3]
            except Exception:
                pass  # Use raw topics if LLM fails

        summary = f"{len(topics)} SEO topics ready" + (f": {'; '.join(t['title'][:40] for t in topics[:3])}" if topics else "")
        if spotlight:
            summary += f" | spotlight: {spotlight['tool']['name']}"
        return {"summary": summary, "opportunities": [{"topics": topics, "spotlight": spotlight}], "errors": errors}

    async def act(self, browser, llm, vlm, decision: dict) -> dict:
        """Generate an SEO-optimized article and publish it."""
        # Get the topic
        opportunities = decision.get("opportunities", [])
        if not opportunities:
            return {"summary": "no topics to write about", "revenue_usd": None}

        opp = opportunities[0] if isinstance(opportunities, list) else opportunities
        spotlight = opp.get("spotlight")
        if spotlight:
            return await self._act_spotlight(browser, llm, spotlight["tool"])
        topics = opp.get("topics", [])
        if not topics:
            return {"summary": "no trending topics", "revenue_usd": None}

        topic = topics[0]
        topic_title = topic.get("title", "")
        keywords = topic.get("meta_keywords", [])

        # Generate the article with SEO + affiliate links
        keyword_str = ", ".join(keywords) if keywords else "technology, programming"
        affiliate_hint = "\n".join(f"- {v['text']}: {v['url']}" for v in AFFILIATE_LINKS.values())

        system = f"""You are Solomon, an autonomous content creator writing for Dev.to and Medium. Write a high-quality, SEO-optimized article about the given topic.

SEO Requirements:
- Title must include primary keyword naturally (not stuffed)
- Include a meta description (first 160 chars of intro)
- Use proper heading structure: H1 (title), H2 (sections), H3 (subsections)
- Target keywords: {keyword_str}
- Write 1200-2000 words
- Include code examples where relevant (use markdown code blocks)
- Make it genuinely useful — a real reader should learn something

Affiliate Integration:
- Naturally weave in 1-2 relevant tool mentions from this list:
{affiliate_hint}
- The mentions must be organic and relevant to the content, not forced
- Do NOT add a "sponsored" section — the mentions should be naturally part of the tutorial

Format:
- Start with `---` YAML frontmatter: title, published (bool), tags (pick 4-5 from: {', '.join(DEVTO_TAGS)} — mix 1-2 broad tags (programming/ai) with 2-3 specific/niche ones; NEVER use only programming+ai)
- Then the article body in markdown
- End with a brief author bio mentioning Solomon

Do NOT output markdown fences around the whole article. Start with the YAML frontmatter directly."""

        user = f"Write a Dev.to article about: {topic_title}\n\nSource for context: {topic.get('url', 'N/A')}\n\nMake it a practical, tutorial-style article that would rank well on Google."

        try:
            article = await llm.ask(system, user, temperature=0.8, max_tokens=4000)
        except Exception as e:
            return {"summary": f"LLM article failed: {e}", "revenue_usd": None}

        # Extract frontmatter for Dev.to API
        is_published = False
        if browser.cfg.auto_submit:
            result = await self._publish_devto(browser, article, topic_title=topic_title)
            return result
        else:
            # Save as draft
            ts = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
            slug = topic_title.lower().replace(" ", "-").replace("?", "").replace(":", "").replace("'", "")[:50]
            article_path = browser.cfg.articles_dir / f"article_{ts}_{slug}.md"
            article_path.write_text(article, encoding="utf-8")
            return {
                "summary": f"SEO article drafted: {topic_title[:50]} → {article_path.name}",
                "revenue_usd": None,
                "source": "content",
                "note": f"Draft at {article_path}. Set SOLOMON_AUTO_SUBMIT=true + DEVTO_API_KEY to publish.",
            }

    async def _act_spotlight(self, browser, llm, tool: dict) -> dict:
        """Write + publish an 'I built this' post for one of our live tools."""
        name = tool["name"]
        pretty = name.replace("-", " ").title()
        url = tool.get("url", f"https://{name}.{os.getenv('CF_SUBDOMAIN', 'solomontools')}.workers.dev")
        desc = tool.get("description", f"AI-powered {pretty}.")
        title = f"I Built a Free {pretty} — No Signup, No Subscription"
        tags = SPOTLIGHT_TAGS.get(name, ["showdev", "ai", "webdev", "tooling"])

        system = f"""You are Solomon, an autonomous AI CEO who builds and ships real software. Write a dev.to showdev post about a tool you built and operate.

Rules:
- First-person builder voice: what it does, why you built it, how it works under the hood (Cloudflare Workers edge + AI inference), what you learned shipping it
- Put the live link EARLY (right after the intro paragraph): {url} — readers must be able to try it instantly
- Honest monetization: the tool is free to use; mention ONCE at the end that it's a one-time $5 on the landing page if it saves people time. No fake urgency, no fake scarcity, no fake testimonials
- 700-1200 words, markdown, H2/H3 structure, include one small code or architecture snippet
- Start with --- YAML frontmatter: title: "{title}", published: true, tags: [{', '.join(tags)}]
- Do NOT wrap the whole article in markdown fences"""

        user = f"Tool: {pretty}\nWhat it does: {desc}\nLive URL: {url}\n\nWrite the post."
        try:
            article = await llm.ask(system, user, temperature=0.7, max_tokens=3500)
        except Exception as e:
            return {"summary": f"spotlight LLM failed: {e}", "revenue_usd": None}

        if browser.cfg.auto_submit:
            return await self._publish_devto(browser, article, topic_title=title, force_title=True, tags_override=tags)
        ts = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
        article_path = browser.cfg.articles_dir / f"spotlight_{ts}_{name}.md"
        article_path.write_text(article, encoding="utf-8")
        return {"summary": f"spotlight drafted: {pretty} → {article_path.name}", "revenue_usd": None, "source": "content"}

    async def _publish_devto(self, browser, article_markdown: str, topic_title: str = "", force_title: bool = False, tags_override: list | None = None) -> dict:
        """Publish an article to Dev.to via their free API."""
        api_key = os.getenv("DEVTO_API_KEY", "")
        if not api_key:
            return {
                "summary": "DEVTO_API_KEY not set — article saved as draft only",
                "revenue_usd": None,
                "source": "content",
                "note": "Set DEVTO_API_KEY in .env (get free token at dev.to/settings/extensions)",
            }

        # Parse frontmatter with YAML (handles titles with colons)
        frontmatter = {}
        body = article_markdown
        if article_markdown.startswith("---"):
            parts = article_markdown.split("---", 2)
            if len(parts) >= 3:
                import yaml
                fm_text = parts[1].strip()
                body = parts[2].strip()
                try:
                    frontmatter = yaml.safe_load(fm_text) or {}
                except Exception:
                    frontmatter = {}

        if force_title and topic_title:
            title = topic_title
        else:
            title = frontmatter.get("title", topic_title or "Untitled")
            if isinstance(title, str):
                title = title.strip().strip('"').strip("'")
            else:
                title = topic_title or "Untitled"
        # Guard: broken/truncated titles fall back to the topic title
        if len(title) < 20 and topic_title and len(topic_title) >= 20:
            title = topic_title
        if tags_override:
            tags = tags_override[:5]
        else:
            tags = frontmatter.get("tags", ["programming", "ai"])
            if isinstance(tags, str):
                tags = [t.strip() for t in tags.strip("[]").split(",") if t.strip()]
            tags = tags[:5]

        # Inject CTA footer linking to our AI tools (converts article traffic to funnel)
        sub = os.getenv("CF_SUBDOMAIN", "solomontools")
        if f"solomon-tools.{sub}.workers.dev" not in body:
            body += (
                "\n\n---\n\n"
                "*Enjoyed this? I build simple, powerful AI tools — try the free "
                f"[Text Summarizer](https://text-summarizer.{sub}.workers.dev) "
                "or browse the full toolkit at "
                f"[Solomon Tools](https://solomon-tools.{sub}.workers.dev). "
                "No signup, no subscription.*"
            )

        try:
            async with httpx.AsyncClient(timeout=30) as client:
                resp = await client.post(
                    "https://dev.to/api/articles",
                    headers={"api-key": api_key, "content-type": "application/json"},
                    json={
                        "article": {
                            "title": title,
                            "body_markdown": body,
                            "published": True,
                            "tags": tags,
                        }
                    },
                )
                if resp.status_code in (200, 201):
                    data = resp.json()
                    url = data.get("url", "")
                    self._record_published_title(browser, title)
                    return {
                        "summary": f"PUBLISHED on Dev.to: {url}",
                        "revenue_usd": None,
                        "source": "content",
                        "note": f"Live at {url}",
                    }
                else:
                    return {
                        "summary": f"Dev.to publish failed: HTTP {resp.status_code} — {resp.text[:200]}",
                        "revenue_usd": None,
                        "source": "content",
                    }
        except Exception as e:
            return {
                "summary": f"Dev.to publish error: {e}",
                "revenue_usd": None,
                "source": "content",
            }

    def _record_published_title(self, browser, title: str) -> None:
        """Persist successful titles so transient API failures cannot cause repeats."""
        runtime_dir = getattr(browser.cfg, "runtime_dir", None)
        if runtime_dir is None:
            return
        state_path = runtime_dir / "content_published_titles.json"
        try:
            saved = json.loads(state_path.read_text(encoding="utf-8")) if state_path.exists() else []
            if not isinstance(saved, list):
                saved = []
            normalized = self._normalize_title(title)
            if normalized and normalized not in saved:
                saved.append(normalized)
            state_path.write_text(json.dumps(saved[-500:], indent=2), encoding="utf-8")
        except Exception:
            # Publishing succeeded; state persistence is only a duplicate guard.
            pass

    def _next_spotlight(self, tools: list, already_published: set) -> dict | None:
        """First tool lacking an 'I built' article, else None (rotation exhausts)."""
        for t in tools:
            pretty_norm = self._normalize_title(t["name"].replace("-", " "))
            taken = any(pretty_norm in p and "built" in p for p in already_published)
            if not taken:
                return {
                    "title": f"I Built a Free {t['name'].replace('-', ' ').title()} — No Signup, No Subscription",
                    "tool": t,
                }
        return None

    @staticmethod
    def _normalize_title(title: str) -> str:
        """Normalize a title for fuzzy dedup: lowercase, alphanumeric words only."""
        import re
        words = re.sub(r"[^a-z0-9 ]", "", (title or "").lower()).split()
        return " ".join(words)

    def _extract_keywords(self, title: str) -> list[str]:
        """Extract potential SEO keywords from a title."""
        stop_words = {"the", "a", "an", "is", "are", "how", "why", "what", "to", "for", "in", "of", "with", "and", "or", "on", "at", "by"}
        words = [w.lower().strip(".,;:!?\"'()[]") for w in title.split()]
        keywords = [w for w in words if len(w) > 2 and w not in stop_words]
        return keywords[:5]
