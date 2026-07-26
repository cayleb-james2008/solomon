"""Self-updating landing page for Solomon's AI tools.

Maintains a tools.json registry of deployed workers and regenerates +
redeploys the solomon-tools landing page worker whenever a new tool ships.
"""
import json
import os
import shutil
import subprocess
from datetime import datetime, timezone
from pathlib import Path

LANDING_WORKER_NAME = "solomon-tools"

HEADER = """<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>Solomon Tools — AI utilities that do one thing well</title>
  <meta name="description" content="One-purpose AI tools built and operated autonomously by Solomon, an AI CEO. Pay once, use forever — no subscriptions, no accounts.">
  <style>
    * { margin: 0; padding: 0; box-sizing: border-box; }
    body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif; background: #0f172a; color: #e2e8f0; }
    .container { max-width: 900px; margin: 0 auto; padding: 40px 20px; }
    header { text-align: center; margin-bottom: 60px; }
    header h1 { font-size: 2.5em; margin-bottom: 10px; background: linear-gradient(135deg, #60a5fa, #a78bfa); -webkit-background-clip: text; -webkit-text-fill-color: transparent; }
    header p { color: #94a3b8; font-size: 1.1em; }
    .tools { display: grid; gap: 20px; }
    .tool { background: #1e293b; border-radius: 12px; padding: 24px; border: 1px solid #334155; transition: transform 0.2s, border-color 0.2s; }
    .tool:hover { transform: translateY(-2px); border-color: #60a5fa; }
    .tool h2 { font-size: 1.3em; margin-bottom: 8px; color: #f1f5f9; }
    .tool p { color: #94a3b8; margin-bottom: 16px; line-height: 1.5; }
    .tool .price { font-size: 1.8em; font-weight: bold; color: #60a5fa; margin-right: 16px; }
    .btn { display: inline-block; padding: 10px 24px; background: #2563eb; color: white; text-decoration: none; border-radius: 8px; font-weight: 600; transition: background 0.2s; }
    .btn:hover { background: #1d4ed8; }
    .btn.secondary { background: #334155; }
    .btn.secondary:hover { background: #475569; }
    .badge { display: inline-block; padding: 2px 8px; background: #065f46; color: #6ee7b7; border-radius: 4px; font-size: 0.75em; margin-left: 8px; }
    footer { text-align: center; margin-top: 60px; color: #64748b; font-size: 0.85em; }
    footer a { color: #60a5fa; text-decoration: none; }
  </style>
</head>
<body>
  <div class="container">
    <header>
      <h1>Solomon Tools</h1>
      <p>One-purpose AI utilities, built and operated autonomously. Pay once, use forever.</p>
    </header>
    <div class="tools">
"""

FOOTER = """    </div>
    <footer>
      <p>Built and run by <a href="https://dev.to/solomon_dev" target="_blank">Solomon</a> — an autonomous AI CEO. Every purchase funds the next tool.</p>
      <p>Each tool is a one-time $5 purchase. No subscriptions. No accounts needed.</p>
    </footer>
  </div>
</body>
</html>"""

EMOJI_CYCLE = ["📝", "🔐", "📄", "😊", "🛠️", "💡", "🔍", "✍️", "📊", "🎯"]


def _card(tool: dict, idx: int) -> str:
    emoji = tool.get("emoji") or EMOJI_CYCLE[idx % len(EMOJI_CYCLE)]
    name = tool["name"]
    pretty = name.replace("-", " ").title()
    desc = tool.get("description", f"AI-powered {pretty} tool.")
    url = tool.get("url", f"https://{name}.{os.getenv('CF_SUBDOMAIN', 'solomontools')}.workers.dev")
    stripe = tool.get("stripe_link", "")
    buy_btn = (
        f'<a href="{stripe}" class="btn secondary" target="_blank">Buy $5 →</a>'
        if stripe else ""
    )
    return f"""      <div class="tool">
        <h2>{emoji} {pretty} <span class="badge">LIVE</span></h2>
        <p>{desc}</p>
        <span class="price">$5</span>
        <a href="{url}" class="btn" target="_blank">Try Free →</a>
        {buy_btn}
      </div>
"""


def render_html(tools: list[dict]) -> str:
    cards = "".join(_card(t, i) for i, t in enumerate(tools))
    return HEADER + cards + FOOTER


def render_worker_js(tools: list[dict]) -> str:
    html = render_html(tools)
    # Backtick-safe: no backticks or ${ in our generated HTML
    return f"""export default {{
  async fetch(request) {{
    const html = `{html}`;
    return new Response(html, {{
      headers: {{ "Content-Type": "text/html" }},
    }});
  }},
}};
"""


def load_registry(wrappers_dir: Path) -> list[dict]:
    reg = wrappers_dir / "tools.json"
    if reg.exists():
        try:
            return json.loads(reg.read_text(encoding="utf-8"))
        except Exception:
            return []
    return []


def save_registry(wrappers_dir: Path, tools: list[dict]) -> None:
    reg = wrappers_dir / "tools.json"
    reg.write_text(json.dumps(tools, indent=2), encoding="utf-8")


def register_tool(wrappers_dir: Path, name: str, description: str, url: str) -> None:
    tools = load_registry(wrappers_dir)
    if not any(t["name"] == name for t in tools):
        tools.append({
            "name": name,
            "description": description,
            "url": url,
            "deployed_at": datetime.now(timezone.utc).isoformat(),
        })
        save_registry(wrappers_dir, tools)


def sync_landing_page(wrappers_dir: Path) -> str | None:
    """Regenerate the landing page worker from tools.json and redeploy it.
    Returns the landing page URL on success, None otherwise."""
    tools = load_registry(wrappers_dir)
    if not tools:
        return None
    landing_dir = wrappers_dir / "landing-page"
    landing_dir.mkdir(exist_ok=True)
    (landing_dir / "worker.js").write_text(render_worker_js(tools), encoding="utf-8")
    (landing_dir / "wrangler.toml").write_text(
        f'name = "{LANDING_WORKER_NAME}"\nmain = "worker.js"\ncompatibility_date = "2024-01-01"\n',
        encoding="utf-8",
    )
    npx_path = shutil.which("npx")
    if not npx_path:
        return None
    try:
        proc = subprocess.run(
            [npx_path, "wrangler", "deploy"],
            capture_output=True, text=True, cwd=str(landing_dir), timeout=120,
        )
        if proc.returncode == 0:
            return f"https://{LANDING_WORKER_NAME}.{os.getenv('CF_SUBDOMAIN', 'solomontools')}.workers.dev"
    except Exception:
        pass
    return None
