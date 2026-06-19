"""Basic frontend-test isolation for Solomon.

This is deliberately named an isolated test runtime, not a hostile-code security
boundary.  It protects the source tree and ordinary user state by running from a
filtered temporary mirror with a re-rooted environment and bounded process tree.
"""
from __future__ import annotations

import os
import shlex
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

_NO_WINDOW = 0x08000000 if sys.platform == "win32" else 0
_SECRET_WORDS = ("KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL", "AUTH")
_SAFE_INHERITED_ENV = {
    "PATH", "PATHEXT", "SYSTEMROOT", "WINDIR", "COMSPEC", "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE", "PROCESSOR_IDENTIFIER", "OS", "LANG", "TZ",
}


def _free_port() -> int:
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]
    finally:
        sock.close()


def _secret_shaped(name: str) -> bool:
    upper = str(name).upper()
    return any(word in upper for word in _SECRET_WORDS)


def _clean_sandbox_env(extra_env: dict | None = None, state_root: str | Path | None = None) -> dict:
    extra_env = extra_env or {}
    rejected = [str(key) for key in extra_env if _secret_shaped(str(key))]
    if rejected:
        raise ValueError(f"secret-shaped sandbox env keys are forbidden: {', '.join(rejected)}")
    env = {key: value for key, value in os.environ.items() if key.upper() in _SAFE_INHERITED_ENV}
    root = Path(state_root or tempfile.mkdtemp(prefix="solomon-env-"))
    temp = root / "temp"
    appdata = root / "appdata"
    local = root / "localappdata"
    for directory in (root, temp, appdata, local):
        directory.mkdir(parents=True, exist_ok=True)
    env.update({
        "HOME": str(root), "USERPROFILE": str(root), "APPDATA": str(appdata),
        "LOCALAPPDATA": str(local), "TEMP": str(temp), "TMP": str(temp),
        "NO_PROXY": "127.0.0.1,localhost", "PYTHONNOUSERSITE": "1",
    })
    env.update({str(key): str(value) for key, value in extra_env.items()})
    return env


def _copy_ignore(_directory: str, names: list[str]) -> set[str]:
    ignored = set()
    fixed = {".git", ".venv", "venv", "node_modules", "__pycache__", ".pytest_cache",
             ".mypy_cache", ".ruff_cache", ".tox", "runtime", "build"}
    for name in names:
        lower = name.lower()
        if name in fixed or lower == ".env" or lower.startswith(".env.") or lower.endswith(".pem"):
            ignored.add(name)
    return ignored


class Sandbox:
    """Launch an application from a disposable mirror on an ephemeral loopback port."""

    def __init__(self, config: dict, repo_path: str):
        self.config = config or {}
        self.repo_path = os.path.abspath(repo_path)
        self.port = 0
        self.tmp_dir: str | None = None
        self.work_dir: str | None = None
        self.state_dir: str | None = None
        self.proc: subprocess.Popen | None = None
        self.base_url = ""
        self.stdout_path: str | None = None
        self.stderr_path: str | None = None
        self._stdout = None
        self._stderr = None

    def _create_isolation_root(self) -> None:
        self.tmp_dir = tempfile.mkdtemp(prefix="solomon-sandbox-")
        root = Path(self.tmp_dir)
        work = root / "worktree"
        state = root / "state"
        shutil.copytree(self.repo_path, work, ignore=_copy_ignore, dirs_exist_ok=False)
        state.mkdir(parents=True, exist_ok=True)
        self.work_dir = str(work)
        self.state_dir = str(state)
        self.stdout_path = str(root / "stdout.log")
        self.stderr_path = str(root / "stderr.log")

    def _command(self) -> list[str]:
        launch = self.config.get("launch")
        if isinstance(launch, list):
            args = [str(item) for item in launch]
        elif isinstance(launch, str) and launch.strip():
            args = shlex.split(launch, posix=sys.platform != "win32")
        else:
            raise ValueError("sandbox config missing 'launch' command")
        values = {"port": str(self.port), "state": str(self.state_dir),
                  "workdir": str(self.work_dir)}
        args = [arg.format(**values) for arg in args]
        if args:
            first = args[0].replace("/", os.sep).replace("\\", os.sep)
            candidate = os.path.abspath(os.path.join(self.repo_path, first))
            if (first.startswith(f".venv{os.sep}") or first.startswith(f"venv{os.sep}")) \
                    and os.path.exists(candidate):
                args[0] = candidate
        return args

    def __enter__(self) -> "Sandbox":
        self.port = _free_port()
        self.base_url = f"http://127.0.0.1:{self.port}"
        self._create_isolation_root()
        extra = dict(self.config.get("extra_env") or {})
        port_env = self.config.get("port_env")
        state_env = self.config.get("state_env")
        if port_env:
            extra[str(port_env)] = str(self.port)
        if state_env:
            extra[str(state_env)] = str(self.state_dir)
        env = _clean_sandbox_env(extra, self.state_dir)
        env["HOST"] = "127.0.0.1"
        flags = _NO_WINDOW | subprocess.CREATE_NEW_PROCESS_GROUP if sys.platform == "win32" else 0
        self._stdout = open(self.stdout_path, "w", encoding="utf-8")
        self._stderr = open(self.stderr_path, "w", encoding="utf-8")
        self.proc = subprocess.Popen(
            self._command(), shell=False, cwd=self.work_dir, env=env,
            stdout=self._stdout, stderr=self._stderr, stdin=subprocess.DEVNULL,
            creationflags=flags, close_fds=True,
        )
        try:
            self._wait_health()
        except Exception:
            self._cleanup()
            raise
        return self

    def _wait_health(self) -> None:
        import urllib.error
        import urllib.request

        url = f"{self.base_url}{self.config.get('health', '/')}"
        timeout = max(1, int(self.config.get("boot_timeout", 30)))
        deadline = time.time() + timeout
        last_error = ""
        while time.time() < deadline:
            if self.proc and self.proc.poll() is not None:
                detail = self._read_log_tail(self.stderr_path)
                raise RuntimeError(f"sandbox process exited early (code {self.proc.returncode}): {detail}")
            try:
                with urllib.request.urlopen(url, timeout=2) as response:
                    if response.status == 200:
                        return
            except (urllib.error.URLError, ConnectionError, OSError) as exc:
                last_error = str(exc)[:200]
            time.sleep(0.25)
        raise RuntimeError(f"sandbox did not become healthy within {timeout}s: {last_error}")

    @staticmethod
    def _read_log_tail(path: str | None, limit: int = 800) -> str:
        if not path:
            return ""
        try:
            text = Path(path).read_text(encoding="utf-8", errors="replace")
            return text[-limit:]
        except OSError:
            return ""

    def _terminate_process_tree(self) -> None:
        if not self.proc or self.proc.poll() is not None:
            return
        try:
            if sys.platform == "win32":
                subprocess.run(["taskkill", "/F", "/T", "/PID", str(self.proc.pid)],
                               capture_output=True, creationflags=_NO_WINDOW, timeout=10)
            else:
                import signal
                os.killpg(os.getpgid(self.proc.pid), signal.SIGKILL)
        except (OSError, subprocess.TimeoutExpired):
            try:
                self.proc.kill()
            except OSError:
                pass

    def __exit__(self, *exc):
        self._cleanup()
        return False

    def _cleanup(self) -> None:
        self._terminate_process_tree()
        if self.proc:
            try:
                self.proc.wait(timeout=5)
            except (OSError, subprocess.TimeoutExpired):
                try:
                    self.proc.kill()
                except OSError:
                    pass
        for handle in (self._stdout, self._stderr):
            try:
                if handle:
                    handle.close()
            except OSError:
                pass
        if self.tmp_dir:
            shutil.rmtree(self.tmp_dir, ignore_errors=True)
        self.proc = None
        self._stdout = None
        self._stderr = None
        self.tmp_dir = None
        self.work_dir = None
        self.state_dir = None
