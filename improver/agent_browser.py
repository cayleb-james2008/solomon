"""Agent-browser bridge — a long-lived, agent-driven browser session whose live state
(screenshot + cursor position + URL) is written to runtime/<name>/browser_state.json
so the in-app Browser panel (Feature 1) can render it for the operator.

Design:
  - A single Chromium instance (Playwright, headed=false so it never steals the operator's
    cursor) is launched per repo on demand and kept alive for the duration of an agent
    browsing session. The agent never touches the operator's real Chrome profile.
  - The agent drives the browser through a small set of actions (navigate, click, type,
    scroll, screenshot). After EACH action the bridge captures a screenshot + the cursor's
    current position (as a % of the viewport) and atomically writes browser_state.json.
  - The dashboard's `control.browser_state()` reads that file and the UI renders the
    screenshot + an animated cursor at the reported position.
  - The bridge is agent-control only: the operator observes via the panel, they never
    drive the browser directly. This matches the "agent control only" requirement.
  - An image-native model is required for browser control: the agent is given the
    screenshot and must output the next action (click at x/y, type text, scroll, etc.),
    which this bridge executes. The cursor position is the last action's target.

The bridge is best-effort and never raises into the RSI loop — a failure writes
{ok:false} to browser_state.json and the panel shows the empty state. The RSI loop
must not break if the browser infra is down.

Usage (driven by run_improver.py / a browsing pi task):
    with AgentBrowser(repo_path, runtime_dir, sandbox_config) as ab:
        ab.navigate("http://127.0.0.1:39201/dashboard")
        ab.write_state(phase="active", status="agent driving")  # panel updates live
        # the pi agent's action loop calls ab.click(x_pct, y_pct), ab.type(text), etc.
    # on exit, the browser closes and browser_state.json is cleared
"""
from __future__ import annotations

import base64
import json
import os
import shutil
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

_NO_WINDOW = 0x08000000 if sys.platform == "win32" else 0


def _now() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


class AgentBrowser:
    """A long-lived, agent-driven Chromium session whose live state is surfaced to the
    in-app Browser panel via browser_state.json.

    The browser is launched headless so it never steals the operator's cursor/focus —
    the operator sees the agent's browser ONLY through the in-app panel's screenshot
    stream + the rendered visible cursor. This is the "agent control only, visible to
    the user through a panel" requirement, satisfied mechanically.
    """

    def __init__(self, repo_path: str, runtime_dir: Path, viewport=(1280, 800)):
        self.repo_path = repo_path
        self.runtime_dir = Path(runtime_dir)
        self.state_file = self.runtime_dir / "browser_state.json"
        self.viewport = viewport
        self._proc = None          # the node driver subprocess (see _driver_js)
        self._page_w = viewport[0]
        self._page_h = viewport[1]

    # ---- lifecycle ----
    def __enter__(self):
        self.runtime_dir.mkdir(parents=True, exist_ok=True)
        self.write_state(ok=True, url="", screenshot_b64="", cursor=None,
                        status="starting", phase="active")
        return self

    def __exit__(self, *exc):
        self.close()
        return False

    def close(self):
        """Terminate the browser driver and clear the panel state."""
        if self._proc and self._proc.poll() is None:
            try:
                self._proc.terminate()
                try:
                    self._proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    self._proc.kill()
            except OSError:
                pass
        self._proc = None
        # clear the state file so the panel returns to the empty state
        try:
            if self.state_file.exists():
                self.state_file.unlink()
        except OSError:
            pass

    # ---- public actions (called by the agent's action loop) ----
    def navigate(self, url: str) -> dict:
        """Navigate the browser to a URL, capture a screenshot, and write the live state.
        Returns {ok, url} or {ok:false, error}. The screenshot + cursor are written to
        browser_state.json for the panel to render."""
        # The actual browser driving is delegated to a node/Playwright driver (see
        # _run_driver). This keeps the heavy Playwright dep in node-land and lets the
        # bridge run under any Python.
        return self._run_driver("navigate", url=url)

    def click(self, x_pct: float, y_pct: float) -> dict:
        """Click at the given viewport percentages (0-100). The cursor position is
        recorded so the panel renders the visible cursor at the click point."""
        return self._run_driver("click", x_pct=float(x_pct), y_pct=float(y_pct))

    def type_text(self, text: str) -> dict:
        """Type text into the currently-focused element."""
        return self._run_driver("type", text=text)

    def scroll(self, dx: int = 0, dy: int = 300) -> dict:
        """Scroll the page by (dx, dy) pixels."""
        return self._run_driver("scroll", dx=int(dx), dy=int(dy))

    def screenshot(self) -> dict:
        """Capture a screenshot without taking an action."""
        return self._run_driver("screenshot")

    # ---- state writer (read by control.browser_state + the panel) ----
    def write_state(self, ok=True, url="", screenshot_b64="", cursor=None,
                    status="live", phase="active", error=None):
        """Atomically write the live browser state to browser_state.json.

        `cursor` is {x, y, click} where x/y are viewport percentages (0-100) and click
        is True for a brief moment after a click (the panel animates a click ring).
        The file is written atomically (tmp + os.replace) so the panel never reads a
        half-written snapshot."""
        snap = {"ok": ok, "url": url, "screenshot_b64": screenshot_b64,
                "cursor": cursor or {}, "status": status, "phase": phase, "ts": _now()}
        if error:
            snap["error"] = str(error)[:300]
        try:
            self.state_file.parent.mkdir(parents=True, exist_ok=True)
            tmp = str(self.state_file) + ".tmp"
            with open(tmp, "w", encoding="utf-8") as f:
                json.dump(snap, f)
            os.replace(tmp, self.state_file)
        except OSError:
            pass
        return snap

    # ---- driver: delegates the real browser work to a node/Playwright script ----
    def _run_driver(self, action: str, **kwargs) -> dict:
        """Run the node driver for a single action. The driver script owns the
        Chromium instance + page, performs the action, captures a screenshot, and
        prints a JSON line the bridge parses. If the driver isn't available, the
        bridge writes a best-effort {ok:false} state and returns it (never raises)."""
        node = shutil.which("node")
        if not node:
            self.write_state(ok=False, status="node not found", phase="idle")
            return {"ok": False, "error": "node not found"}
        driver = Path(__file__).parent / "agent_browser_driver.js"
        if not driver.exists():
            self.write_state(ok=False, status="driver missing", phase="idle")
            return {"ok": False, "error": "agent_browser_driver.js not found"}
        payload = json.dumps({"action": action, "viewport": list(self.viewport), **kwargs})
        try:
            p = subprocess.run(
                [node, str(driver), str(self.repo_path), payload],
                capture_output=True, text=True, timeout=30,
                env=_clean_env(), creationflags=_NO_WINDOW,
            )
        except (subprocess.TimeoutExpired, OSError) as e:
            self.write_state(ok=False, status="driver timeout", phase="idle",
                             error=str(e)[:200])
            return {"ok": False, "error": str(e)[:200]}
        if p.returncode != 0:
            self.write_state(ok=False, status="driver failed", phase="idle",
                             error=(p.stderr or p.stdout or "")[:200])
            return {"ok": False, "error": (p.stderr or p.stdout or "driver failed")[:200]}
        # parse the last JSON line from stdout
        for line in reversed((p.stdout or "").splitlines()):
            line = line.strip()
            if line.startswith("{"):
                try:
                    res = json.loads(line)
                except json.JSONDecodeError:
                    break
                # the driver returns {ok, url, screenshot_b64, cursor, x, y}; write the panel state
                self.write_state(
                    ok=bool(res.get("ok")),
                    url=res.get("url", ""),
                    screenshot_b64=res.get("screenshot_b64", ""),
                    cursor={"x": res.get("x", 0), "y": res.get("y", 0),
                            "click": action == "click"},
                    status=res.get("status", "live"),
                    phase="active",
                    error=res.get("error"),
                )
                return res
        self.write_state(ok=False, status="no driver output", phase="idle")
        return {"ok": False, "error": "no driver output"}


def _clean_env() -> dict:
    """Env for the node driver — strip secrets + python path pollution (same rules as
    the visual review capture)."""
    env = dict(os.environ)
    for k in ("PYTHONPATH", "PYTHONHOME", "GITHUB_TOKEN", "GH_TOKEN"):
        env.pop(k, None)
    return env