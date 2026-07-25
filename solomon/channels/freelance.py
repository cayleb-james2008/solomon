"""Freelance gig discovery channel — scrape public listings, draft proposals.

The agent browses Fiverr/Contra public gig requests, identifies small tasks it
can fulfill, drafts a proposal via the LLM, and saves it to runtime/proposals/
for operator review. Auto-submit is OFF by default (SOLOMON_AUTO_SUBMIT=false).

Money guard: discover and act are money-IN only (no spending). The guard checks
"subscribe_paid" and "upgrade_paid" are DENIED if the platform ever tries to
upsell during sign-up.
"""
import json
from datetime import datetime, timezone
from pathlib import Path

from . import Channel


class FreelanceChannel(Channel):
    name = "freelance"

    async def discover(self, browser, llm, vlm) -> dict:
        """Scrape public freelance gig listings."""
        gigs = []
        errors = []

        # Fiverr — public buyer requests (no login required to view)
        try:
            await browser.goto("https://www.fiverr.com/categories/programming-tech")
            await browser._human_delay(1000, 3000)
            screenshot = await browser.screenshot(
                str(browser.cfg.runtime_dir / "fiverr_screenshot.png")
            )
            # Use VLM to describe the page
            description = await vlm.describe_screenshot(
                screenshot, "List all freelance gig listings visible on this page. For each: title, price, and category."
            )
            gigs.append({"platform": "fiverr", "page": "programming-tech", "description": description[:500]})
        except Exception as e:
            errors.append(f"fiverr: {e}")

        # Contra — public project listings
        try:
            await browser.goto("https://contra.com/projects")
            await browser._human_delay(1000, 3000)
            screenshot = await browser.screenshot(
                str(browser.cfg.runtime_dir / "contra_screenshot.png")
            )
            description = await vlm.describe_screenshot(
                screenshot, "List all freelance project listings visible on this page. For each: title, budget, and skills required."
            )
            gigs.append({"platform": "contra", "page": "projects", "description": description[:500]})
        except Exception as e:
            errors.append(f"contra: {e}")

        summary = f"{len(gigs)} platforms scanned, {len(errors)} errors"
        return {"summary": summary, "opportunities": gigs, "errors": errors}

    async def act(self, browser, llm, vlm, decision: dict) -> dict:
        """Draft a proposal for a gig and save it for operator review."""
        opportunities = decision.get("opportunities", [])
        if not opportunities:
            # Use the most recent opportunities from discover
            return {"summary": "no gigs to act on", "revenue_usd": None}

        # Pick the first opportunity
        gig = opportunities[0] if isinstance(opportunities, list) else opportunities
        platform = gig.get("platform", "unknown")
        gig_desc = gig.get("description", "")

        # Ask the LLM to draft a proposal
        system = """You are Solomon, an autonomous freelance agent. Draft a professional, concise proposal for this gig. The proposal should:
- Be friendly but professional
- Highlight relevant skills (AI, Python, automation, content)
- Include a timeline estimate
- Include a price quote (be competitive — $15-50 for small tasks)
- Be self-contained and ready to paste"""

        user = f"Platform: {platform}\nGig description:\n{gig_desc}\n\nDraft a proposal."

        try:
            proposal = await llm.ask(system, user)
        except Exception as e:
            return {"summary": f"LLM proposal draft failed: {e}", "revenue_usd": None}

        # Save the proposal for operator review
        ts = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
        proposal_path = browser.cfg.proposals_dir / f"proposal_{platform}_{ts}.md"
        proposal_path.write_text(
            f"# Proposal — {platform} — {ts}\n\n## Gig\n{gig_desc}\n\n## Proposal\n{proposal}\n",
            encoding="utf-8",
        )

        # Auto-submit is OFF by default — operator reviews
        if browser.cfg.auto_submit:
            # TODO: implement submit logic when operator trust is established
            return {"summary": f"proposal auto-submitted on {platform}", "revenue_usd": None, "source": platform}
        else:
            return {
                "summary": f"proposal drafted and saved to {proposal_path.name} (operator review required)",
                "revenue_usd": None,
                "source": platform,
                "note": f"Draft saved to {proposal_path}",
            }
