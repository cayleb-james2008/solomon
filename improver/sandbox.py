"""Sandbox — ephemeral, isolated app-booter for visual E2E review.

Boots a repo's app on a random free port with a throwaway state directory, polls a health
URL until 200, then yields a handle. On exit, terminates the process and cleans up the temp
dir. Never writes to the repo working tree — the sandbox runs in its own state dir.

Usage:
    with Sandbox(cfg, repo_path) as sb:
        print(sb.base_url)   # http://127.0.0.1:<free_port>
        # ... drive the app ...
    # process killed, temp dir removed
"""
from __future__ import annotations

import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

_NO_WINDOW = 0x08000000 if sys.platform == "win32" else 0


def _free_port() -> int:
    """Allocate a free TCP port on 127.0.0.1 using a bind-then-close trick."""
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]
    finally:
        s.close()


def _clean_sandbox_env(extra_env: dict | None = None) -> dict:
    """os.environ stripped of ALL secrets + PYTHONPATH/PYTHONHOME, plus sandbox-only vars.

    The sandbox must NEVER inherit real credentials — it runs with a disposable profile
    and no real tokens, so a leak in the ephemeral env can't reach a real account.
    """
    env = {}
    # keep only safe, non-secret env vars (PATH, SystemRoot, etc.)
    for k, v in os.environ.items():
        kl = k.upper()
        if any(x in kl for x in ("KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL")):
            continue
        if k in ("PYTHONPATH", "PYTHONHOME", "GITHUB_TOKEN", "GH_TOKEN"):
            continue
        env[k] = v
    if extra_env:
        env.update(extra_env)
    return env


class Sandbox:
    """Context manager that boots an app in an ephemeral sandbox.

    config keys (from repos.json `sandbox` object):
        launch:   shell command to start the app (run in the repo path cwd)
        port_env: env var name to inject the free port as (e.g. "SOVER_API_PORT")
        state_env: env var name to inject the temp state dir as (e.g. "SOVER_HOME")
        extra_env: dict of additional sandbox-only env vars
        health:   path to poll for 200 (default "/")
        boot_timeout: seconds to wait for health (default 30)
    """

    def __init__(self, config: dict, repo_path: str):
        self.config = config or {}
        self.repo_path = repo_path
        self.port = 0
        self.tmp_dir: str | None = None
        self.proc: subprocess.Popen | None = None
        self.base_url = ""

    def __enter__(self) -> "Sandbox":
        cfg = self.config
        launch = cfg.get("launch", "")
        if not launch:
            raise ValueError("sandbox config missing 'launch' command")

        self.port = _free_port()
        self.tmp_dir = tempfile.mkdtemp(prefix="solomon-sandbox-")

        extra = dict(cfg.get("extra_env") or {})
        port_env = cfg.get("port_env")
        if port_env:
            extra[port_env] = str(self.port)
        state_env = cfg.get("state_env")
        if state_env:
            extra[state_env] = self.tmp_dir

        env = _clean_sandbox_env(extra)
        self.base_url = f"http://127.0.0.1:{self.port}"

        flags = _NO_WINDOW | subprocess.CREATE_NEW_PROCESS_GROUP if sys.platform == "win32" else 0
        self.proc = subprocess.Popen(
            launch,
            shell=True,
            cwd=self.repo_path,
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            stdin=subprocess.DEVNULL,
            creationflags=flags,
            close_fds=True,
        )
        self._wait_health()
        return self

    def _wait_health(self) -> None:
        """Poll the health URL until 200 or timeout. Uses urllib (no dependency)."""
        import urllib.request
        import urllib.error

        health_path = self.config.get("health", "/")
        url = f"{self.base_url}{health_path}"
        timeout = int(self.config.get("boot_timeout", 30))
        deadline = time.time() + timeout
        last_err = ""
        while time.time() < deadline:
            if self.proc and self.proc.poll() is not None:
                raise RuntimeError(f"sandbox process exited early (code {self.proc.returncode})")
            try:
                req = urllib.request.Request(url, method="GET")
                with urllib.request.urlopen(req, timeout=3) as resp:
                    if resp.status == 200:
                        return
            except (urllib.error.URLError, ConnectionError, OSError) as e:
                last_err = str(e)[:200]
            time.sleep(0.5)
        raise RuntimeError(f"sandbox did not become healthy within {timeout}s — {last_err}")

    def __exit__(self, *exc):
        self._cleanup()
        return False

    def _cleanup(self) -> None:
        """Terminate the process tree and remove the temp dir."""
        if self.proc and self.proc.poll() is None:
            try:
                if sys.platform == "win32":
                    subprocess.run(["taskkill", "/F", "/T", "/PID", str(self.proc.pid)],
                                   capture_output=True, creationflags=_NO_WINDOW)
                else:
                    import signal
                    os.killpg(os.getpgid(self.proc.pid), signal.SIGKILL)
            except OSError:
                pass
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                try:
                    self.proc.kill()
                except OSError:
                    pass
        if self.tmp_dir and os.path.isdir(self.tmp_dir):
            shutil.rmtree(self.tmp_dir, ignore_errors=True)
        self.tmp_dir = None
        self.proc = None