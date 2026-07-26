"""AI-wrapper product channel — Solomon builds and deploys a simple AI tool, collects payment via Polar.

This is the fastest first-dollar path per research (2026-07-25):
- Polar doesn't require KYC at signup (bank-account page triggers at payout)
- Cloudflare Workers free tier = zero hosting cost
- Solomon controls the code, the tool, and the checkout
- Money guard: Polar checkout is money-IN (collect), not money-OUT

The channel:
1. DISCOVER: LLM identifies a niche AI tool opportunity (something simple but useful)
2. ACT: Generate the tool code, package for Cloudflare Workers, create Polar checkout link
3. Revenue flows through Polar webhook → runtime/revenue.jsonl
"""
import json
import os
from datetime import datetime, timezone
from pathlib import Path

import httpx

from . import Channel


class AIWrapperChannel(Channel):
    name = "ai_wrapper"

    async def discover(self, browser, llm, vlm) -> dict:
        """Ask the LLM to identify a niche AI tool that could sell for $5."""
        system = """You are Solomon, a profit-focused AI CEO. Identify a simple, niche AI tool that:
- Can be built as a single-file Cloudflare Worker (JS/TS)
- Uses an LLM API for the core function
- Solves a specific problem people would pay $5 for
- Has low competition (no obvious free alternatives that are good enough)
- Can be described in one sentence

Respond with JSON:
{"tool_name": "<short name>", "description": "<one sentence>", "target_audience": "<who>", "api_type": "<what the tool does>", "price": 5}"""

        user = "What AI tool should we build and sell? Think of something practical that doesn't exist yet or is poorly served by free tools."

        # Discover what tools we've already built (dedup: never build the same tool twice)
        existing = set()
        try:
            wrappers_dir = browser.cfg.runtime_dir / "ai_wrappers"
            if wrappers_dir.exists():
                for d in wrappers_dir.iterdir():
                    if d.is_dir():
                        # Dir names look like: tool-name_20260726_031501
                        existing.add(d.name.rsplit("_", 2)[0].lower())
        except Exception:
            pass
        exclusion = ""
        if existing:
            exclusion = f" We ALREADY built these, suggest something DIFFERENT: {', '.join(sorted(existing))}."

        try:
            raw = await llm.ask(
                "You are a profit-focused AI CEO. Name ONE simple AI tool people would pay $5 for. Reply with ONLY the tool name (2-3 words, lowercase, hyphenated). Nothing else." + exclusion,
                "What tool should we build?" + exclusion,
                temperature=0.9,
                max_tokens=20,
            )
            tool_name = raw.strip().lower().replace(" ", "-")[:30]
            # Clean it for Cloudflare Workers naming
            tool_name = "".join(c for c in tool_name if c.isalnum() or c == "-")
            if not tool_name or len(tool_name) < 2 or tool_name in existing:
                tool_name = "ai-tool"
            # Absolute last-resort: if somehow still a dupe, suffix it
            if tool_name in existing:
                tool_name = f"{tool_name}-pro"

            desc_raw = await llm.ask(
                f"Write one sentence describing what '{tool_name}' does and who would pay $5 for it. Be specific.",
                "Describe the tool.",
                temperature=0.5,
                max_tokens=60,
            )
            description = desc_raw.strip().strip('"').strip("'")[:200]

            idea = {
                "tool_name": tool_name,
                "description": description or f"AI-powered {tool_name} tool",
                "target_audience": "developers and professionals",
                "api_type": "text_in→processed_out",
                "price": 5,
            }
            return {
                "summary": f"idea: {idea['tool_name']} — {idea['description'][:80]}",
                "opportunities": [idea],
            }
        except Exception as e:
            # Fallback: a useful default tool idea
            default = {
                "tool_name": "commit-msg-ai",
                "description": "Generate conventional commit messages from git diffs",
                "target_audience": " developers",
                "api_type": "text_in→commit_msg_out",
                "price": 5,
            }
            return {
                "summary": f"fallback idea: {default['tool_name']} (LLM error: {e})",
                "opportunities": [default],
            }

    async def act(self, browser, llm, vlm, decision: dict) -> dict:
        """Generate the AI-wrapper tool code and save it for deployment."""
        opportunities = decision.get("opportunities", [])
        if not opportunities:
            return {"summary": "no tool idea to act on", "revenue_usd": None}

        idea = opportunities[0] if isinstance(opportunities, list) else opportunities
        tool_name = idea.get("tool_name", "ai-tool")
        # Cloudflare Workers requires lowercase alphanumeric with dashes
        tool_slug = tool_name.lower().replace(" ", "-").replace("_", "-")
        tool_slug = "".join(c for c in tool_slug if c.isalnum() or c == "-")[:30]
        description = idea.get("description", "")
        api_type = idea.get("api_type", "")
        price = idea.get("price", 5)

        # Ask the LLM to generate the Cloudflare Worker code
        system = """You are Solomon, an autonomous AI engineer. Generate a complete, deployable Cloudflare Worker that implements the described AI tool.

Requirements:
- Single file: worker.js (ES module syntax, `export default { async fetch(request, env) { ... } }`)
- Frontend: a clean, minimal HTML page with an input form and results display
- Backend: call the LLM API using OpenAI-compatible format:
  - Endpoint: https://openrouter.ai/api/v1/chat/completions
  - Authorization: Bearer ${env.API_KEY}
  - Body: { "model": "meta-llama/llama-3.3-70b-instruct", "messages": [{"role":"system","content":"..."},{"role":"user","content":"..."}], "max_tokens": 200, "temperature": 0.3 }
  - Parse response: data.choices[0].message.content
- Frontend JS: POST to /api endpoint on the same worker, parse JSON response
- Include proper error handling for API failures
- Make it look professional (use inline CSS, no external deps)
- Handle GET / (serve HTML) and POST /api (process request) routes separately

Output ONLY the JavaScript code, no explanations."""

        user = f"""Build this AI tool as a Cloudflare Worker:
- Tool name: {tool_name}
- Description: {description}
- API type: {api_type}
- The LLM endpoint MUST be exactly: https://openrouter.ai/api/v1/chat/completions
- Model: meta-llama/llama-3.3-70b-instruct
- Use env.API_KEY for the LLM API key (never hardcode it)
- Route check for API calls: use request.url.endsWith('/api') — NEVER strict equality (url includes origin)
- Make it a single worker.js file

Generate the complete code."""

        try:
            worker_code = await llm.ask(system, user, temperature=0.3)
        except Exception as e:
            return {"summary": f"LLM code generation failed: {e}", "revenue_usd": None}

        # Save the generated tool
        ts = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
        tool_dir = browser.cfg.runtime_dir / "ai_wrappers" / f"{tool_name}_{ts}"
        tool_dir.mkdir(parents=True, exist_ok=True)

        worker_path = tool_dir / "worker.js"
        # Strip markdown fences if present
        code = worker_code.strip()
        if code.startswith("```"):
            lines = code.split("\n")
            lines = [l for l in lines if not l.strip().startswith("```")]
            code = "\n".join(lines)
        worker_path.write_text(code, encoding="utf-8")

        # Generate the wrangler.toml for Cloudflare Workers deployment
        wrangler_toml = f'''name = "{tool_name}"
main = "worker.js"
compatibility_date = "2024-01-01"

[vars]
# Set API_KEY via: wrangler secret put API_KEY
'''
        (tool_dir / "wrangler.toml").write_text(wrangler_toml, encoding="utf-8")

        # Generate a README with deployment instructions
        readme = f"""# {tool_name}

{description}

## Deploy

```sh
# 1. Install wrangler (Cloudflare Workers CLI)
npm install -g wrangler

# 2. Login to Cloudflare
wrangler login

# 3. Set the LLM API key as a secret
wrangler secret put API_KEY
# Paste your API key when prompted

# 4. Deploy
wrangler deploy
```

## Monetize via Polar

1. Create a Polar product at https://polar.sh (no KYC needed at signup)
2. Set the price to ${price}
3. Add the Polar checkout link to the worker's HTML
4. Gate the tool behind the checkout (or use Polar's usage-based billing)

## Revenue

Revenue from this tool flows through Polar webhook → Solomon's revenue ledger.
"""
        (tool_dir / "README.md").write_text(readme, encoding="utf-8")

        # Generate a Polar checkout page (inline HTML for the tool)
        polar_html = f"""<!DOCTYPE html>
<html>
<head>
  <title>{tool_name}</title>
  <style>
    body {{ font-family: -apple-system, sans-serif; max-width: 600px; margin: 50px auto; padding: 20px; }}
    .price {{ font-size: 3em; font-weight: bold; color: #2563eb; }}
    .checkout {{ display: inline-block; padding: 12px 32px; background: #2563eb; color: white;
                text-decoration: none; border-radius: 8px; font-weight: bold; margin: 20px 0; }}
    .checkout:hover {{ background: #1d4ed8; }}
  </style>
</head>
<body>
  <h1>{tool_name}</h1>
  <p>{description}</p>
  <div class="price">${price}</div>
  <div>
    <!-- Replace POLAR_CHECKOUT_URL with your Polar checkout link -->
    <a href="POLAR_CHECKOUT_URL" class="checkout">Buy Now →</a>
  </div>
  <p>After purchase, you'll get instant access to the tool.</p>
</body>
</html>"""
        (tool_dir / "checkout.html").write_text(polar_html, encoding="utf-8")

        # Auto-deploy to Cloudflare Workers if wrangler is available
        deployed_url = None
        try:
            import shutil
            # Resolve full path: on Windows, subprocess can't find npx.CMD by
            # bare name (CreateProcess doesn't use PATHEXT without shell=True)
            npx_path = shutil.which("npx")
            if npx_path:
                import subprocess
                # Set the API key as a wrangler secret first
                api_key = os.getenv("SOLOMON_LLM_API_KEY", "") or os.getenv("OPENROUTER_API_KEY", "")
                if api_key:
                    proc = subprocess.run(
                        [npx_path, "wrangler", "secret", "put", "API_KEY"],
                        input=api_key,
                        capture_output=True,
                        text=True,
                        cwd=str(tool_dir),
                        timeout=60,
                    )
                # Deploy
                proc = subprocess.run(
                    [npx_path, "wrangler", "deploy"],
                    capture_output=True,
                    text=True,
                    cwd=str(tool_dir),
                    timeout=120,
                )
                if proc.returncode == 0:
                    # Extract the workers.dev URL from output
                    for line in proc.stdout.split("\n"):
                        if "workers.dev" in line:
                            deployed_url = line.strip()
                            break
                    if not deployed_url:
                        subdomain = os.getenv("CF_SUBDOMAIN", "solomontools")
                        deployed_url = f"https://{tool_slug}.{subdomain}.workers.dev"

                    # Register tool + auto-update the landing page (self-scaling)
                    try:
                        from ..landing import register_tool, sync_landing_page
                        register_tool(browser.cfg.runtime_dir / "ai_wrappers", tool_slug, description, deployed_url)
                        # Auto-create Stripe payment links for any tool missing
                        # one BEFORE the landing rebuild, so Buy buttons go live
                        try:
                            from ..payments import sync_links
                            sync_links(browser.cfg.runtime_dir / "ai_wrappers")
                        except Exception:
                            pass  # rail offline (no key etc.) — tool still ships
                        landing_url = sync_landing_page(browser.cfg.runtime_dir / "ai_wrappers")
                        if landing_url:
                            deployed_url = deployed_url + f" | Landing: {landing_url}"
                    except Exception:
                        pass
        except Exception:
            pass

        return {
            "summary": f"AI tool '{tool_name}' generated at {tool_dir.name}" + (f" — DEPLOYED: {deployed_url}" if deployed_url else " — deploy with wrangler"),
            "revenue_usd": None,
            "source": "ai_wrapper",
            "note": f"Tool saved to {tool_dir}. Deploy: cd {tool_dir} && wrangler deploy" + (f" | Live: {deployed_url}" if deployed_url else ""),
            "deployed_url": deployed_url,
        }
