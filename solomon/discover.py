"""Channel discovery module — Solomon autonomously researches and evaluates new
income methods. This is the 'self-scaling' seed: the CEO uses this to find new
channels beyond the built-in 4.

The discovery module:
1. Asks the LLM to brainstorm new income methods based on Solomon's capabilities
2. Evaluates each method (feasibility, revenue potential, startup cost)
3. Returns a ranked list that can be added to the CEO's channel rotation
"""
import json

from .config import Config
from .llm import LLMClient


async def discover_new_channels(llm: LLMClient) -> list[dict]:
    """Ask the LLM to brainstorm and rank new income channels.

    Returns a list of {"name": str, "mechanism": str, "revenue_potential": str,
    "startup_cost": str, "feasibility": float, "notes": str}
    """
    system = """You are Solomon, an autonomous profit-focused CEO. Your job is to discover NEW income methods that you (an AI agent with a browser and an LLM API) could pursue.

For each method, evaluate:
- mechanism: what action does the agent perform?
- revenue_potential: realistic monthly income range
- startup_cost: cash required upfront (must be $0 — Solomon only collects)
- feasibility: 0-1 probability of working for a solo AI agent within 30 days
- notes: any blockers or requirements

Constraints:
- No cash outlay upfront (money guard)
- No identity verification (we don't have human ID docs yet)
- Must use only a browser + LLM API
- No money-OUT actions (no spending, no paid subscriptions)

Respond with a JSON array of 5 methods, ranked by feasibility (highest first).
[{"name": "...", "mechanism": "...", "revenue_potential": "$X-$Y/mo", "startup_cost": "$0", "feasibility": 0.X, "notes": "..."}]"""

    user = "What 5 new income methods should Solomon pursue? Think creatively — beyond the standard freelance/content/survey channels. Consider: API-as-a-service, code generation, data scraping-as-a-service, documentation generation, bug bounty triage, code review, etc."

    try:
        result = await llm.ask_json(system, user)

        # Handle both {"methods": [...]} and [...] response shapes
        if isinstance(result, dict) and "methods" in result:
            return result["methods"]
        elif isinstance(result, list):
            return result
        else:
            return [result] if isinstance(result, dict) else []
    except Exception as e:
        return [{"name": "discovery_error", "mechanism": f"LLM error: {e}", "revenue_potential": "$0", "startup_cost": "$0", "feasibility": 0, "notes": "retry needed"}]
