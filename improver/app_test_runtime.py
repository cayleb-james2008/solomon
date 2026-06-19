"""Managed monitored frontend-test sessions for Solomon."""
from __future__ import annotations

import json
import os
import shutil
import sys
import threading
import time
import uuid
from datetime import datetime, timezone
from pathlib import Path

from .agent_browser import AgentBrowser
from .sandbox import Sandbox


def _now() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def detect_config(repo: dict) -> dict | None:
    configured = repo.get("sandbox")
    if isinstance(configured, dict) and configured.get("enabled") and configured.get("launch"):
        return dict(configured)
    root = Path(str(repo.get("path") or ""))
    python = shutil.which("python") or sys.executable
    for relative in ("web", "public", "."):
        directory = root / relative
        if (directory / "index.html").is_file():
            launch = [python, "-m", "http.server", "{port}", "--bind", "127.0.0.1"]
            if relative != ".":
                launch += ["--directory", relative]
            return {"enabled": True, "launch": launch, "health": "/index.html",
                    "pages": ["/index.html"], "boot_timeout": 20}
    package = root / "package.json"
    if package.is_file():
        try:
            data = json.loads(package.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            data = {}
        scripts = data.get("scripts") if isinstance(data, dict) else {}
        script = next((name for name in ("dev", "preview", "start")
                       if isinstance(scripts, dict) and name in scripts), None)
        if script:
            return {"enabled": True,
                    "launch": ["npm.cmd" if os.name == "nt" else "npm", "run", script, "--",
                               "--host", "127.0.0.1", "--port", "{port}"],
                    "health": "/", "pages": ["/"], "boot_timeout": 60}
    return None


class AppTestManager:
    def __init__(self):
        self._lock = threading.Lock()
        self._sessions: dict[str, dict] = {}

    def start(self, repo: dict, runtime_dir: Path) -> dict:
        name = str(repo.get("name") or "")
        config = detect_config(repo)
        if not name or not config:
            return {"ok": False, "error": "no runnable frontend configuration found"}
        with self._lock:
            active = self._sessions.get(name)
            if active and active["thread"].is_alive():
                return {"ok": True, "already": True, "sessionId": active["session_id"]}
            stop = threading.Event()
            session_id = f"{name}-{uuid.uuid4().hex[:12]}"
            thread = threading.Thread(
                target=self._run, args=(name, dict(repo), Path(runtime_dir), config, session_id, stop),
                name=f"app-test-{name}", daemon=True,
            )
            self._sessions[name] = {"thread": thread, "stop": stop, "session_id": session_id,
                                    "browser": None}
            thread.start()
        return {"ok": True, "sessionId": session_id, "status": "starting"}

    def _run(self, name: str, repo: dict, runtime_dir: Path, config: dict,
             session_id: str, stop: threading.Event) -> None:
        runtime_dir.mkdir(parents=True, exist_ok=True)
        report_path = runtime_dir / "app_test_report.json"
        started = _now()
        try:
            with Sandbox(config, str(repo["path"])) as sandbox:
                with AgentBrowser(str(repo["path"]), runtime_dir,
                                  allowed_origins=[sandbox.base_url], session_id=session_id) as browser:
                    with self._lock:
                        if name in self._sessions:
                            self._sessions[name]["browser"] = browser
                    first_page = (config.get("pages") or ["/"])[0]
                    result = browser.navigate(f"{sandbox.base_url}{first_page}")
                    if not result.get("ok"):
                        raise RuntimeError(result.get("error") or "browser navigation failed")
                    while not stop.wait(0.5):
                        pass
                    final = {"ok": True, "status": "stopped", "sessionId": session_id,
                             "startedAt": started, "finishedAt": _now(), "findings": []}
                    report_path.write_text(json.dumps(final, indent=2), encoding="utf-8")
        except Exception as exc:  # noqa: BLE001 - state is surfaced to the operator
            failed = {"ok": False, "status": "error", "sessionId": session_id,
                      "startedAt": started, "finishedAt": _now(), "error": str(exc)[:500],
                      "findings": [{"severity": "critical", "category": "infrastructure",
                                    "description": str(exc)[:300]}]}
            report_path.write_text(json.dumps(failed, indent=2), encoding="utf-8")
            state_path = runtime_dir / "browser_state.json"
            state_path.write_text(json.dumps({"schemaVersion": 1, "sessionId": session_id,
                                              "seq": 1, "ok": False, "status": "error",
                                              "phase": "error", "error": str(exc)[:300],
                                              "ts": _now()}), encoding="utf-8")
        finally:
            with self._lock:
                current = self._sessions.get(name)
                if current and current.get("session_id") == session_id:
                    current["browser"] = None

    def stop(self, name: str) -> dict:
        with self._lock:
            session = self._sessions.get(name)
        if not session:
            return {"ok": True, "already": True}
        session["stop"].set()
        browser = session.get("browser")
        if browser:
            browser.close()
        session["thread"].join(timeout=20)
        if session["thread"].is_alive():
            return {"ok": False, "error": "app test did not stop within 20 seconds"}
        with self._lock:
            self._sessions.pop(name, None)
        return {"ok": True, "sessionId": session["session_id"]}

    def act(self, name: str, action: dict) -> dict:
        with self._lock:
            browser = (self._sessions.get(name) or {}).get("browser")
        if not browser:
            return {"ok": False, "error": "no ready app-test browser session"}
        return browser.act(action)


MANAGER = AppTestManager()
