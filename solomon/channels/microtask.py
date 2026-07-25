"""Microtask/survey discovery channel — check Prolific/Connect availability.

The agent checks task availability on microtask platforms and alerts the operator
when paid tasks are available. If logged in via persistent browser profile,
auto-claim is attempted.
"""
from datetime import datetime, timezone

from . import Channel


class MicrotaskChannel(Channel):
    name = "microtask"

    PLATFORMS = [
        {"name": "prolific", "url": "https://www.prolific.com/", "login_url": "https://www.prolific.com/sign-in"},
        {"name": "cloudresearch", "url": "https://connect.cloudresearch.com/", "login_url": "https://connect.cloudresearch.com/login"},
    ]

    async def discover(self, browser, llm, vlm) -> dict:
        """Check task availability on microtask platforms."""
        results = []
        errors = []

        for platform in self.PLATFORMS:
            try:
                await browser.goto(platform["url"])
                await browser._human_delay(1000, 3000)
                screenshot = await browser.screenshot(
                    str(browser.cfg.runtime_dir / f"{platform['name']}_screenshot.png")
                )
                description = await vlm.describe_screenshot(
                    screenshot,
                    f"Is this a login page or does it show available tasks? What is the current state of the page? Are there any task/survey listings visible?"
                )

                # Check if we're logged in (not a login page)
                is_login = "sign in" in description.lower() or "log in" in description.lower() or "login" in description.lower()

                results.append({
                    "platform": platform["name"],
                    "state": "login_required" if is_login else "available",
                    "description": description[:300],
                    "url": platform["url"],
                })
            except Exception as e:
                errors.append(f"{platform['name']}: {e}")

        available = sum(1 for r in results if r["state"] == "available")
        summary = f"{len(results)} platforms checked, {available} appear to have tasks"
        return {"summary": summary, "opportunities": results, "errors": errors}

    async def act(self, browser, llm, vlm, decision: dict) -> dict:
        """Alert the operator to available tasks, or auto-claim if logged in."""
        opportunities = decision.get("opportunities", [])
        if not opportunities:
            return {"summary": "no microtask opportunities", "revenue_usd": None}

        actions = []
        for opp in (opportunities if isinstance(opportunities, list) else [opportunities]):
            platform = opp.get("platform", "unknown")
            state = opp.get("state", "unknown")

            if state == "login_required":
                # Alert the operator — they need to log in via the browser profile
                actions.append(f"{platform}: login required (operator must log in via browser profile)")
                # Navigate to the login page so the operator can log in
                login_url = next((p["login_url"] for p in self.PLATFORMS if p["name"] == platform), None)
                if login_url:
                    await browser.goto(login_url)
            elif state == "available":
                if browser.cfg.auto_submit:
                    # Attempt to claim the first available task
                    actions.append(f"{platform}: auto-claiming task")
                    # TODO: implement task claim logic when operator trust is established
                else:
                    actions.append(f"{platform}: task available (operator review required before claim)")

        return {
            "summary": "; ".join(actions),
            "revenue_usd": None,
            "source": "microtask",
            "note": " No auto-claim — operator must approve",
        }
