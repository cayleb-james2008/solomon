"""Stealth browser pool — PatchRight (Playwright stealth fork) with persistent profiles.

Anti-bot-first design:
- Persistent profile (survives restarts, keeps login sessions)
- Human-like fingerprint (navigator.webdriver removal, CDP leak fixes via PatchRight)
- Non-headless by default (visible browser is less suspicious)
- Random mouse movements and delays (configured via humanize param)
"""
import asyncio
import random
from pathlib import Path
from typing import Optional

from .config import Config


class StealthBrowser:
    """Wraps a PatchRight browser context with stealth defaults."""

    def __init__(self, cfg: Config):
        self.cfg = cfg
        self._playwright = None
        self._browser = None
        self._context = None
        self._page = None

    async def launch(self):
        """Launch the browser with a persistent profile."""
        from patchright.async_api import async_playwright

        self._playwright = await async_playwright().start()

        profile_path = Path(self.cfg.browser_profile)
        profile_path.mkdir(parents=True, exist_ok=True)

        # Persistent context = keeps cookies/login across sessions
        self._context = await self._playwright.chromium.launch_persistent_context(
            user_data_dir=str(profile_path),
            headless=self.cfg.browser_headless,
            viewport={"width": 1366, "height": 768},
            user_agent=self.cfg.browser_ua or None,
            locale="en-US",
            timezone_id="America/New_York",
            # Stealth: these are handled by PatchRight automatically, but we set
            # reasonable values for anything PatchRight doesn't override
            args=[
                "--disable-blink-features=AutomationControlled",
                "--no-first-run",
                "--no-default-browser-check",
            ],
        )

        # Use the first page or create one
        if self._context.pages:
            self._page = self._context.pages[0]
        else:
            self._page = await self._context.new_page()

    async def goto(self, url: str, wait: bool = True):
        """Navigate to a URL with a human-like delay."""
        await self._page.goto(url, wait_until="domcontentloaded" if wait else "commit")
        await self._human_delay()

    async def screenshot(self, path: str | None = None) -> str:
        """Take a screenshot. Returns the path to the screenshot file."""
        if path is None:
            path = str(self.cfg.runtime_dir / "screenshot.png")
        await self._page.screenshot(path=path, full_page=False)
        return path

    async def get_text(self) -> str:
        """Get the visible text content of the current page."""
        return await self._page.inner_text("body")

    async def get_url(self) -> str:
        return self._page.url

    async def get_title(self) -> str:
        return await self._page.title()

    async def click(self, selector: str):
        """Click an element with a human-like delay."""
        await self._page.wait_for_selector(selector, timeout=10000)
        await self._human_delay()
        await self._page.click(selector)

    async def type(self, selector: str, text: str, delay: int = 50):
        """Type text into an input field with human-like per-character delay."""
        await self._page.wait_for_selector(selector, timeout=10000)
        await self._page.click(selector)
        await self._page.type(selector, text, delay=delay + random.randint(0, 50))
        await self._human_delay()

    async def fill(self, selector: str, text: str):
        """Fill a field (instant, no per-char delay)."""
        await self._page.wait_for_selector(selector, timeout=10000)
        await self._page.fill(selector, text)

    async def wait_for(self, selector: str, timeout: int = 10000):
        await self._page.wait_for_selector(selector, timeout=timeout)

    async def query_selector_all(self, selector: str):
        return await self._page.query_selector_all(selector)

    async def query_selector(self, selector: str):
        return await self._page.query_selector(selector)

    async def _human_delay(self, min_ms: int = 300, max_ms: int = 1500):
        """Random human-like delay between actions."""
        await asyncio.sleep(random.uniform(min_ms / 1000, max_ms / 1000))

    async def close(self):
        """Close the browser and save the profile."""
        if self._context:
            await self._context.close()
        if self._playwright:
            await self._playwright.stop()
        self._page = None
        self._context = None
        self._browser = None
        self._playwright = None

    @property
    def page(self):
        return self._page

    @property
    def context(self):
        return self._context
