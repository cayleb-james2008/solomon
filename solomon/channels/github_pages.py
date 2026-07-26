"""GitHub Pages publishing channel — Solomon publishes articles to a GH Pages blog.

This channel gives Solomon a free distribution surface (GitHub Pages) that:
- Requires no new credentials (uses the existing gh CLI auth)
- Deploys instantly via git push
- Can embed affiliate/referral links for passive income
- Can host the AI-wrapper tool demos + Polar checkout links

The channel:
1. DISCOVER: Check the existing blog repo for articles, find trending topics
2. ACT: Write a new article, format as a Jekyll/Hugo post, commit + push to the
   GitHub Pages repo, which auto-deploys.

The blog repo is created on first run if it doesn't exist: <github_user>/solomon-blog
"""
import json
import os
import subprocess
from datetime import datetime, timezone
from pathlib import Path

import httpx

from . import Channel


class GithubPagesChannel(Channel):
    name = "github_pages"

    def __init__(self):
        self._blog_repo = None  # detected/set on first use

    async def discover(self, browser, llm, vlm) -> dict:
        """Check the blog repo status and find trending topics."""
        # Get the GitHub username from gh CLI
        try:
            result = subprocess.run(
                ["gh", "api", "user", "--jq", ".login"],
                capture_output=True, text=True, timeout=10,
            )
            gh_user = result.stdout.strip()
        except Exception:
            gh_user = ""

        if not gh_user:
            return {"summary": "gh CLI not available", "opportunities": []}

        blog_repo = f"{gh_user}/solomon-blog"
        self._blog_repo = blog_repo
        self._gh_user = gh_user

        # Check if the blog repo exists
        try:
            result = subprocess.run(
                ["gh", "repo", "view", blog_repo, "--json", "name,url"],
                capture_output=True, text=True, timeout=10,
            )
            if result.returncode == 0:
                repo_info = json.loads(result.stdout)
                repo_url = repo_info.get("url", "")
                blog_url = f"https://{gh_user}.github.io/solomon-blog/"
                # Count existing posts
                posts_dir = Path(browser.cfg.runtime_dir) / "solomon-blog" / "_posts"
                post_count = len(list(posts_dir.glob("*.md"))) if posts_dir.exists() else 0
                # Fetch trending topics for the summary
                topics_short = []
                try:
                    async with httpx.AsyncClient(timeout=10) as client:
                        resp = await client.get("https://hacker-news.firebaseio.com/v0/topstories.json")
                        resp.raise_for_status()
                        for sid in resp.json()[:3]:
                            ir = await client.get(f"https://hacker-news.firebaseio.com/v0/item/{sid}.json")
                            if ir.status_code == 200 and ir.json():
                                topics_short.append(ir.json().get("title", "")[:60])
                except Exception:
                    pass
                topic_str = "; ".join(topics_short) if topics_short else "none"
                summary = f"blog at {blog_url} — {post_count} posts — trending: {topic_str}"
            else:
                summary = "blog repo does not exist yet — will create on first act"
                blog_url = None
        except Exception as e:
            summary = f"error checking blog: {e}"
            blog_url = None

        # Find trending topics from HN (same as content channel)
        topics = []
        try:
            async with httpx.AsyncClient(timeout=15) as client:
                resp = await client.get("https://hacker-news.firebaseio.com/v0/topstories.json")
                resp.raise_for_status()
                story_ids = resp.json()[:5]
                for sid in story_ids:
                    item_resp = await client.get(f"https://hacker-news.firebaseio.com/v0/item/{sid}.json")
                    item_resp.raise_for_status()
                    item = item_resp.json()
                    if item and item.get("title"):
                        topics.append(item["title"])
        except Exception:
            pass

        return {
            "summary": summary,
            "opportunities": [{"topics": topics, "blog_url": blog_url}],
            "blog_repo": blog_repo,
            "gh_user": gh_user,
        }

    async def act(self, browser, llm, vlm, decision: dict) -> dict:
        """Write an article and publish it to the GitHub Pages blog."""
        opportunities = decision.get("opportunities", [])
        if not opportunities:
            return {"summary": "no topics to publish", "revenue_usd": None}

        opp = opportunities[0] if isinstance(opportunities, list) else opportunities
        topics = opp.get("topics", [])
        if not topics:
            return {"summary": "no trending topics", "revenue_usd": None}

        # Pick the first topic
        topic = topics[0]

        # Ensure the blog repo exists
        blog_dir = Path(browser.cfg.runtime_dir) / "solomon-blog"
        if not blog_dir.exists():
            created = self._create_blog_repo(browser.cfg)
            if not created:
                return {"summary": "failed to create blog repo", "revenue_usd": None}
            # Clone it
            repo_name = self._blog_repo.split("/")[-1] if "/" in self._blog_repo else self._blog_repo
            full_name = f"{self._gh_user}/{repo_name}"
            subprocess.run(
                ["gh", "repo", "clone", full_name, str(blog_dir)],
                capture_output=True, text=True, timeout=30,
            )

        # Ask the LLM to write the article
        system = """You are Solomon, an autonomous content creator. Write a well-structured, engaging blog post suitable for a tech blog. The post should:
- Be 800-1500 words
- Use markdown formatting with headers, lists, and code blocks
- Include a catchy title
- Have a clear intro, body, and conclusion
- Be SEO-friendly and shareable
- Include a subtle call-to-action at the end (but NO spammy links)
- NEVER include any real person's name, email, GitHub username, or personal info
- The author is "Solomon" (an autonomous AI agent) — do not attribute to any human
"""

        user = f"Write a blog post about: {topic}"

        try:
            article = await llm.ask(system, user, temperature=0.8)
        except Exception as e:
            return {"summary": f"LLM article failed: {e}", "revenue_usd": None}

        # Save as a Jekyll post (YYYY-MM-DD-title.md)
        date_str = datetime.now(timezone.utc).strftime("%Y-%m-%d")
        time_str = datetime.now(timezone.utc).strftime("%Y-%m-%d-%H%M%S")
        slug = topic.lower().replace(" ", "-").replace(":", "").replace("'", "").replace("?", "").replace("!", "").replace("/", "-").replace("\\", "-").replace('"', "").replace("<", "").replace(">", "").replace("|", "").replace("*", "").replace("&", "and")[:50]
        post_filename = f"{date_str}-{slug}.md"
        posts_dir = blog_dir / "_posts"
        posts_dir.mkdir(parents=True, exist_ok=True)
        post_path = posts_dir / post_filename

        post_content = f"""---
layout: post
title: "{topic}"
date: {datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M:%S")} +0000
categories: ai tech
author: Solomon
---

{article}
"""
        post_path.write_text(post_content, encoding="utf-8")

        # Git commit + push
        try:
            subprocess.run(["git", "config", "user.email", f"{self._gh_user}@users.noreply.github.com"], cwd=str(blog_dir), capture_output=True, timeout=5)
            subprocess.run(["git", "config", "user.name", "Solomon"], cwd=str(blog_dir), capture_output=True, timeout=5)
            subprocess.run(["git", "add", "."], cwd=str(blog_dir), capture_output=True, timeout=10)
            subprocess.run(
                ["git", "commit", "-m", f"post: {topic}"],
                cwd=str(blog_dir), capture_output=True, text=True, timeout=10,
            )
            subprocess.run(
                ["git", "push"],
                cwd=str(blog_dir), capture_output=True, text=True, timeout=30,
            )
        except Exception as e:
            return {"summary": f"git push failed: {e}", "revenue_usd": None}

        blog_url = f"https://{self._gh_user}.github.io/solomon-blog/{date_str}/{slug}.html"

        return {
            "summary": f"published '{topic}' to GitHub Pages blog",
            "revenue_usd": None,
            "source": "github_pages",
            "note": f"Live at {blog_url} (may take 1-2 min for GH Pages build)",
        }

    def _create_blog_repo(self, cfg) -> bool:
        """Create the GitHub Pages blog repo with Jekyll config."""
        # repo_name is just the bare name (e.g. "solomon-blog"), not "user/repo"
        repo_name = self._blog_repo.split("/")[-1] if "/" in self._blog_repo else self._blog_repo
        try:
            # Create the repo (just the name, gh defaults to the authed user)
            result = subprocess.run(
                ["gh", "repo", "create", repo_name, "--public", "--description", "Solomon's autonomous tech blog"],
                capture_output=True, text=True, timeout=30,
            )
            full_name = f"{self._gh_user}/{repo_name}"
            if result.returncode != 0:
                # Repo might already exist
                pass

            # Clone it to add Jekyll config
            blog_dir = Path(cfg.runtime_dir) / "solomon-blog"
            subprocess.run(
                ["gh", "repo", "clone", full_name, str(blog_dir)],
                capture_output=True, text=True, timeout=30,
            )

            # Create _config.yml for Jekyll
            config = """title: Solomon's Tech Blog
description: Autonomous tech insights from an AI agent
author: Solomon
theme: minima
url: ""
github_username: ""
twitter_username: ""
"""
            (blog_dir / "_config.yml").write_text(config, encoding="utf-8")

            # Create _posts directory
            (blog_dir / "_posts").mkdir(exist_ok=True)

            # Create an index.html that redirects to the latest posts
            index = """<!DOCTYPE html>
<html>
<head>
<meta http-equiv="refresh" content="0; url=/solomon-blog/">
<title>Solomon's Tech Blog</title>
</head>
<body>Redirecting...</body>
</html>
"""
            (blog_dir / "index.html").write_text(index, encoding="utf-8")

            # Commit and push
            subprocess.run(["git", "config", "user.email", f"{self._gh_user}@users.noreply.github.com"], cwd=str(blog_dir), capture_output=True, timeout=5)
            subprocess.run(["git", "config", "user.name", "Solomon"], cwd=str(blog_dir), capture_output=True, timeout=5)
            subprocess.run(["git", "add", "."], cwd=str(blog_dir), capture_output=True, timeout=10)
            subprocess.run(["git", "commit", "-m", "initial jekyll setup"], cwd=str(blog_dir), capture_output=True, text=True, timeout=10)
            subprocess.run(["git", "push", "-u", "origin", "main"], cwd=str(blog_dir), capture_output=True, text=True, timeout=30)

            # Enable GitHub Pages
            subprocess.run(
                ["gh", "api", f"repos/{full_name}/pages", "-X", "POST",
                 "-f", "build_type=legacy", "-f", "source[branch]=main", "-f", "source[path]=/"],
                capture_output=True, text=True, timeout=10,
            )

            return True
        except Exception:
            return False
