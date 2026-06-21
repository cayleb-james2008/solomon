"""Persistent, policy-bounded agent-browser bridge for monitored frontend tests.

The product owns an isolated named agent-browser session; the operator only observes
its structured state and latest JPEG through Solomon.  No raw JavaScript, file transfer,
clipboard, or personal Chrome profile is exposed by this adapter.
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import uuid
from datetime import datetime, timezone
from pathlib import Path
from urllib.parse import urlsplit

from winproc import hidden_subprocess_kwargs


def _now() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _clean_env() -> dict:
    env = dict(os.environ)
    for key in tuple(env):
        upper = key.upper()
        if key in ("PYTHONPATH", "PYTHONHOME", "GITHUB_TOKEN", "GH_TOKEN") or any(
            token in upper for token in ("PASSWORD", "SECRET", "CREDENTIAL")
        ):
            env.pop(key, None)
    env["AGENT_BROWSER_HEADED"] = "false"
    env["AGENT_BROWSER_SCREENSHOT_FORMAT"] = "jpeg"
    env["AGENT_BROWSER_SCREENSHOT_QUALITY"] = "70"
    return env


class AgentBrowser:
    """One persistent agent-browser session with monotonic observations."""

    def __init__(self, repo_path: str, runtime_dir: Path, viewport=(1280, 800),
                 allowed_origins: list[str] | None = None, session_id: str | None = None):
        self.repo_path = os.path.abspath(repo_path)
        self.runtime_dir = Path(runtime_dir)
        self.state_file = self.runtime_dir / "browser_state.json"
        self.frame_file = self.runtime_dir / "browser_frame.jpg"
        self.profile_dir = self.runtime_dir / "browser-profile"
        self.viewport = tuple(viewport)
        self.session_id = session_id or f"solomon-{uuid.uuid4().hex[:12]}"
        self.owner = {"app": "solomon", "projectId": Path(self.repo_path).name}
        self.allowed_origins = {self._origin(value) for value in (allowed_origins or []) if value}
        self._seq = 0
        self._last_url = ""
        self._last_title = ""
        self._last_refs: list[dict] = []
        self._closed = False
        self._started = False        # set True after the first successful CLI call (for _probe_alive)

    def __enter__(self):
        self.runtime_dir.mkdir(parents=True, exist_ok=True)
        self.profile_dir.mkdir(parents=True, exist_ok=True)
        self.write_state(ok=True, status="starting", phase="starting")
        return self

    def __exit__(self, *exc):
        self.close()
        return False

    @staticmethod
    def _origin(url: str) -> str:
        parsed = urlsplit(str(url))
        if parsed.scheme not in ("http", "https") or not parsed.hostname:
            return ""
        port = f":{parsed.port}" if parsed.port else ""
        return f"{parsed.scheme}://{parsed.hostname.lower()}{port}"

    def _url_allowed(self, url: str) -> bool:
        origin = self._origin(url)
        return bool(origin and (not self.allowed_origins or origin in self.allowed_origins))

    def _base_command(self) -> tuple[str | None, list[str]]:
        binary = os.environ.get("SOLOMON_AGENT_BROWSER") or shutil.which("agent-browser")
        if getattr(sys, "frozen", False):
            bundle = Path(getattr(sys, "_MEIPASS", Path(sys.executable).parent))
            packaged = bundle / "agent-browser" / "agent-browser-win32-x64.exe"
            if packaged.is_file():
                binary = str(packaged)
        if binary and sys.platform == "win32":
            native = Path(binary).parent / "node_modules" / "agent-browser" / "bin" / "agent-browser-win32-x64.exe"
            if native.is_file():
                binary = str(native)
        command = [
            binary or "agent-browser", "--session", self.session_id,
            "--profile", str(self.profile_dir), "--json",
            "--screenshot-format", "jpeg", "--screenshot-quality", "70",
        ]
        if self.allowed_origins:
            domains = sorted({urlsplit(origin).hostname or "" for origin in self.allowed_origins})
            command += ["--allowed-domains", ",".join(d for d in domains if d)]
        return binary, command

    def _run_cli(self, args: list[str], timeout: int = 30) -> dict:
        binary, command = self._base_command()
        if not binary:
            return {"ok": False, "error": "agent-browser 0.27.0 is not installed"}
        try:
            # agent-browser may spawn a persistent daemon. Regular temp files avoid the
            # daemon retaining a PIPE handle and making subprocess.communicate wait forever.
            with tempfile.TemporaryFile(mode="w+", encoding="utf-8") as stdout_file, \
                    tempfile.TemporaryFile(mode="w+", encoding="utf-8") as stderr_file:
                result = subprocess.run(
                    command + args, stdout=stdout_file, stderr=stderr_file, text=True,
                    stdin=subprocess.DEVNULL, timeout=timeout, cwd=self.repo_path,
                    env=_clean_env(), close_fds=True, **hidden_subprocess_kwargs(),
                )
                stdout_file.seek(0); stderr_file.seek(0)
                stdout = getattr(result, "stdout", None) or stdout_file.read()
                stderr = getattr(result, "stderr", None) or stderr_file.read()
        except (OSError, subprocess.TimeoutExpired) as exc:
            return {"ok": False, "error": str(exc)[:300]}
        payload = None
        for line in reversed((stdout or "").splitlines()):
            try:
                payload = json.loads(line)
                break
            except json.JSONDecodeError:
                continue
        if result.returncode != 0 or not isinstance(payload, dict) or not payload.get("success"):
            error = (payload or {}).get("error") if isinstance(payload, dict) else None
            return {"ok": False, "error": str(error or stderr or stdout or
                                                "agent-browser command failed")[:300]}
        data = payload.get("data")
        self._started = True   # a successful CLI call proves a live session exists (liveness probe)
        return {"ok": True, "data": data if isinstance(data, dict) else {"value": data}}

    def _fail(self, error: str, action: dict | None = None) -> dict:
        self._seq += 1
        state = self.write_state(ok=False, status="error", phase="error", error=error,
                                 current_action=action)
        return state

    # ---- process health (heartbeat / respawn) ----------------------------
    def _probe_alive(self) -> bool:
        """Lightweight liveness check of the underlying agent-browser session. Before the session is
        established (no successful CLI call yet) there is nothing to probe — return True (the action
        itself starts it) WITHOUT spawning a process. Once started, run a cheap ``get url`` and report
        whether the session/child answered. Never raises (delegates to _run_cli, which catches)."""
        if not self._started:
            return True
        return bool(self._run_cli(["get", "url"], timeout=10).get("ok"))

    def _reinit_session(self) -> dict:
        """Re-establish a crashed session exactly once: best-effort close the dead session, then reset
        the per-session counters so the NEXT action re-creates it. Returns a status dict; never raises."""
        self._run_cli(["close"], timeout=10)
        self._seq = 0
        self._last_refs = []
        self._started = False
        return {"ok": True, "reinit": True}

    def _guard_alive(self) -> None:
        """Before each action: if the session crashed, atomically record it ({ok:false,
        status:'crashed'} — so the dashboard panel shows it's dead, not a stale frame) and attempt
        EXACTLY ONE re-init; the upcoming action then re-creates the session. Synchronous per-action
        (no polling thread — keeps it simple). Never raises into the RSI loop (best-effort contract)."""
        if self._probe_alive():
            return
        self.write_state(ok=False, status="crashed", phase="error",
                         error="agent-browser session is not responding (process crashed)")
        self._reinit_session()

    def navigate(self, url: str) -> dict:
        if not self._url_allowed(url):
            return self._fail("navigation is outside the allowed sandbox origins",
                              {"kind": "navigate", "url": str(url)})
        self._guard_alive()         # respawn a crashed session before navigating
        result = self._run_cli(["open", str(url)])
        if not result.get("ok"):
            return self._fail(result.get("error", "navigation failed"), {"kind": "navigate"})
        return self.observe({"kind": "navigate", "url": str(url)})

    def observe(self, action: dict | None = None) -> dict:
        snapshot = self._run_cli(["snapshot", "-i", "-c"])
        if not snapshot.get("ok"):
            return self._fail(snapshot.get("error", "snapshot failed"), action or {"kind": "observe"})
        data = snapshot.get("data") or {}
        refs = data.get("refs") if isinstance(data, dict) else {}
        elements = []
        if isinstance(refs, dict):
            for ref, value in refs.items():
                item = {"ref": str(ref).removeprefix("@"), "role": "element", "name": "",
                        "observationSeq": self._seq + 1}
                if isinstance(value, dict):
                    item.update({k: value.get(k) for k in ("role", "name") if value.get(k) is not None})
                else:
                    item["name"] = str(value)
                elements.append(item)
        url_result = self._run_cli(["get", "url"])
        if url_result.get("ok"):
            self._last_url = str((url_result.get("data") or {}).get("url") or self._last_url)
        self._last_title = str(data.get("title") or self._last_title) if isinstance(data, dict) else self._last_title
        shot = self._run_cli(["screenshot", str(self.frame_file)])
        console_result = self._run_cli(["errors"])
        network_result = self._run_cli(["network", "requests"])
        console_errors = (console_result.get("data") or {}).get("errors") or [] \
            if console_result.get("ok") else []
        requests = (network_result.get("data") or {}).get("requests") or [] \
            if network_result.get("ok") else []
        network_errors = [request for request in requests if isinstance(request, dict) and
                          (request.get("failure") or int(request.get("status") or 0) >= 400)]
        cursor = {}
        if action and action.get("x") is not None and action.get("y") is not None:
            cursor = {"x": float(action["x"]), "y": float(action["y"]),
                      "kind": str(action.get("kind") or "move")}
        self._seq += 1
        self._last_refs = elements
        return self.write_state(
            ok=True, url=self._last_url, status="ready", phase="active",
            current_action=action or {"kind": "observe"}, elements=elements,
            frame_ok=bool(shot.get("ok")), cursor=cursor,
            console_errors=console_errors, network_errors=network_errors,
        )

    def act(self, action: dict) -> dict:
        if not isinstance(action, dict):
            return self._fail("action must be an object")
        kind = str(action.get("kind") or "")
        observation_seq = action.get("observation_seq")
        if observation_seq is not None and int(observation_seq) != self._seq:
            return self._fail(f"stale element reference: expected observation {self._seq}", action)
        # respawn a crashed session before acting — but navigate() self-guards, so skip the probe
        # here when delegating to it (avoids a redundant double-probe for an act(navigate)).
        if kind != "navigate":
            self._guard_alive()
        if kind == "navigate":
            return self.navigate(str(action.get("url") or ""))
        if kind == "observe":
            return self.observe(action)
        if kind == "click":
            ref = action.get("ref")
            if ref:
                target = str(ref) if str(ref).startswith("@") else f"@{ref}"
                result = self._run_cli(["click", target])
            elif action.get("x") is not None and action.get("y") is not None:
                result = self._run_cli(["mouse", "move", str(int(action["x"])), str(int(action["y"]))])
                if result.get("ok"):
                    result = self._run_cli(["mouse", "down"])
                if result.get("ok"):
                    result = self._run_cli(["mouse", "up"])
            else:
                return self._fail("click requires a ref or x/y coordinates", action)
        elif kind == "type":
            ref, text = action.get("ref"), str(action.get("text") or "")
            target = str(ref) if str(ref).startswith("@") else f"@{ref}"
            result = self._run_cli(["fill", target, text]) if ref else self._run_cli(["keyboard", "type", text])
        elif kind == "key":
            result = self._run_cli(["press", str(action.get("key") or "")])
        elif kind == "select":
            ref = str(action.get("ref") or "")
            target = ref if ref.startswith("@") else f"@{ref}"
            result = self._run_cli(["select", target, str(action.get("value") or "")])
        elif kind == "scroll":
            direction = str(action.get("direction") or "down")
            result = self._run_cli(["scroll", direction, str(int(action.get("pixels") or 300))])
        elif kind == "wait":
            result = self._run_cli(["wait", str(int(action.get("ms") or 500))])
        else:
            return self._fail(f"unsupported browser action: {kind or '(missing)'}", action)
        if not result.get("ok"):
            return self._fail(result.get("error", "browser action failed"), action)
        return self.observe(action)

    # Compatibility helpers for existing callers.
    def click(self, x_pct: float, y_pct: float) -> dict:
        return self.act({"kind": "click", "x": self.viewport[0] * float(x_pct) / 100,
                         "y": self.viewport[1] * float(y_pct) / 100})

    def type_text(self, text: str) -> dict:
        return self.act({"kind": "type", "text": text})

    def scroll(self, dx: int = 0, dy: int = 300) -> dict:
        return self.act({"kind": "scroll", "direction": "down" if dy >= 0 else "up",
                         "pixels": abs(int(dy))})

    def screenshot(self) -> dict:
        return self.observe({"kind": "observe"})

    def write_state(self, ok=True, url="", screenshot_b64="", cursor=None,
                    status="live", phase="active", error=None, current_action=None,
                    elements=None, frame_ok=False, console_errors=None, network_errors=None):
        state = {
            "schemaVersion": 1, "sessionId": self.session_id, "seq": self._seq,
            "ok": bool(ok), "url": url or self._last_url, "title": self._last_title,
            "viewport": {"width": self.viewport[0], "height": self.viewport[1]},
            "owner": self.owner,
            "page": {"url": url or self._last_url, "title": self._last_title,
                     "viewport": {"width": self.viewport[0], "height": self.viewport[1]}},
            "elements": elements if elements is not None else self._last_refs,
            "refs": [item.get("ref") for item in (elements if elements is not None else self._last_refs)],
            "cursor": cursor or {}, "currentAction": current_action or {},
            "consoleErrors": console_errors or [], "networkErrors": network_errors or [],
            "status": status, "phase": phase, "ts": _now(),
        }
        if frame_ok:
            state["frame"] = {"seq": self._seq, "mime": "image/jpeg", "available": True}
        if screenshot_b64:  # compatibility with older tests/state readers
            state["screenshot_b64"] = screenshot_b64
        if error:
            state["error"] = str(error)[:300]
        try:
            self.state_file.parent.mkdir(parents=True, exist_ok=True)
            tmp = self.state_file.with_suffix(".json.tmp")
            tmp.write_text(json.dumps(state), encoding="utf-8")
            os.replace(tmp, self.state_file)
        except OSError:
            pass
        return state

    def close(self):
        if self._closed:
            return
        self._closed = True
        if shutil.which("agent-browser"):
            self._run_cli(["close"], timeout=10)
        for path in (self.state_file, self.frame_file):
            try:
                path.unlink(missing_ok=True)
            except OSError:
                pass
        shutil.rmtree(self.profile_dir, ignore_errors=True)
