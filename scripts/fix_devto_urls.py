"""One-shot maintenance: rewrite old-subdomain worker URLs in all Dev.to articles.

Uses curl (dev.to 401s urllib from this box intermittently).
Usage:  set -a; source .env; set +a; python scripts/fix_devto_urls.py
"""
import json, os, re, subprocess, sys, time

KEY = os.environ["DEVTO_API_KEY"].strip()
OLD, NEW = "caylebalvarezjames.workers.dev", "solomontools.workers.dev"


def call(method, url, payload=None):
    cmd = ["curl", "-s", "-X", method, "-H", f"api-key: {KEY}",
           "-A", "SolomonBot/1.0", "-H", "Content-Type: application/json"]
    if payload is not None:
        cmd += ["-d", json.dumps(payload)]
    cmd.append(url)
    out = subprocess.run(cmd, capture_output=True, text=True, timeout=60).stdout
    return json.loads(out) if out.strip() else {}


arts = call("GET", "https://dev.to/api/articles/me?per_page=30")  # per_page>30 -> 401 from dev.to
if not isinstance(arts, list):
    sys.exit(f"unexpected list response: {str(arts)[:200]}")
print(f"{len(arts)} articles")
for a in arts:
    full = call("GET", f"https://dev.to/api/articles/{a['id']}")
    body = full.get("body_markdown", "")
    if OLD not in body:
        print(f"  clean: {a['title'][:50]}")
        continue
    body = re.sub(r"^---\n.*?\n---\n", "", body, flags=re.S)  # strip frontmatter on re-PUT
    body = body.replace(OLD, NEW)
    call("PUT", f"https://dev.to/api/articles/{a['id']}", {"article": {"body_markdown": body}})
    print(f"  REWRITTEN: {a['title'][:50]}")
    time.sleep(20)
print("done")
