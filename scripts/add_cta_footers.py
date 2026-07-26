"""One-off: retroactively add CTA footers to existing Dev.to articles."""
import json
import os
import time
import urllib.request

env = {}
with open(".env") as f:
    for line in f:
        if "=" in line and not line.startswith("#"):
            k, v = line.strip().split("=", 1)
            env[k] = v

KEY = env["DEVTO_API_KEY"]
CTA = (
    "\n\n---\n\n"
    "*Enjoyed this? I build simple, powerful AI tools — try the free "
    "[Text Summarizer](https://text-summarizer.caylebalvarezjames.workers.dev) "
    "or browse the full toolkit at "
    "[Solomon AI Tools](https://solomon-tools.caylebalvarezjames.workers.dev). "
    "No signup, no subscription.*"
)
MARKER = "solomon-tools.caylebalvarezjames.workers.dev"


def req(method, url, payload=None):
    data = json.dumps(payload).encode() if payload else None
    r = urllib.request.Request(url, data=data, method=method)
    r.add_header("api-key", KEY)
    r.add_header("Content-Type", "application/json")
    r.add_header("User-Agent", "SolomonBot/1.0 (content-automation)")
    with urllib.request.urlopen(r, timeout=30) as resp:
        return json.loads(resp.read())


articles = req("GET", "https://dev.to/api/articles/me/published?per_page=30")
print(f"Found {len(articles)} published articles")

for a in articles:
    aid = a["id"]
    full = req("GET", f"https://dev.to/api/articles/{aid}")
    body = full.get("body_markdown", "")
    if MARKER in body:
        print(f"  SKIP {aid} (already has CTA)")
        continue
    # Strip embedded YAML frontmatter (Dev.to already consumed it; re-sending
    # it leaks it into the description and can trigger 422s).
    # Some legacy articles have BROKEN frontmatter: opening --- with no closing
    # marker. In that case strip key:value lines until the first heading.
    if body.startswith("---"):
        parts = body.split("---", 2)
        if len(parts) >= 3:
            body = parts[2].strip()
        else:
            lines = body.split("\n")[1:]  # drop opening ---
            i = 0
            while i < len(lines):
                l = lines[i].strip()
                if l.startswith("#"):
                    break
                if l and ":" not in l and not l.startswith("["):
                    break
                i += 1
            body = "\n".join(lines[i:]).strip()
    new_body = body + CTA
    try:
        req("PUT", f"https://dev.to/api/articles/{aid}", {"article": {"body_markdown": new_body}})
        print(f"  OK   {aid} — CTA appended")
    except urllib.error.HTTPError as e:
        print(f"  FAIL {aid} — {e} — {e.read().decode()[:300]}")
    except Exception as e:
        print(f"  FAIL {aid} — {e}")
    time.sleep(12)  # respect Dev.to rate limits

print("Done.")
