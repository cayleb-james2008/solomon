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

from winproc import hidden_subprocess_kwargs

_SECRET_WORDS = ("KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL", "AUTH")
_SAFE_INHERITED_ENV = {
    "PATH", "PATHEXT", "SYSTEMROOT", "WINDIR", "COMSPEC", "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE", "PROCESSOR_IDENTIFIER", "OS", "LANG", "TZ",
}


def _create_kill_on_close_job():
    """Create a Windows Job Object that owns the complete preview process tree."""
    if sys.platform != "win32":
        return None
    import ctypes
    from ctypes import wintypes

    class BasicLimit(ctypes.Structure):
        _fields_ = [
            ("PerProcessUserTimeLimit", ctypes.c_longlong),
            ("PerJobUserTimeLimit", ctypes.c_longlong),
            ("LimitFlags", wintypes.DWORD),
            ("MinimumWorkingSetSize", ctypes.c_size_t),
            ("MaximumWorkingSetSize", ctypes.c_size_t),
            ("ActiveProcessLimit", wintypes.DWORD),
            ("Affinity", ctypes.c_size_t),
            ("PriorityClass", wintypes.DWORD),
            ("SchedulingClass", wintypes.DWORD),
        ]

    class IoCounters(ctypes.Structure):
        _fields_ = [
            ("ReadOperationCount", ctypes.c_ulonglong),
            ("WriteOperationCount", ctypes.c_ulonglong),
            ("OtherOperationCount", ctypes.c_ulonglong),
            ("ReadTransferCount", ctypes.c_ulonglong),
            ("WriteTransferCount", ctypes.c_ulonglong),
            ("OtherTransferCount", ctypes.c_ulonglong),
        ]

    class ExtendedLimit(ctypes.Structure):
        _fields_ = [
            ("BasicLimitInformation", BasicLimit),
            ("IoInfo", IoCounters),
            ("ProcessMemoryLimit", ctypes.c_size_t),
            ("JobMemoryLimit", ctypes.c_size_t),
            ("PeakProcessMemoryUsed", ctypes.c_size_t),
            ("PeakJobMemoryUsed", ctypes.c_size_t),
        ]

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.CreateJobObjectW.argtypes = [wintypes.LPVOID, wintypes.LPCWSTR]
    kernel32.CreateJobObjectW.restype = wintypes.HANDLE
    kernel32.SetInformationJobObject.argtypes = [
        wintypes.HANDLE, ctypes.c_int, wintypes.LPVOID, wintypes.DWORD,
    ]
    kernel32.SetInformationJobObject.restype = wintypes.BOOL
    job = kernel32.CreateJobObjectW(None, None)
    if not job:
        return None
    limits = ExtendedLimit()
    limits.BasicLimitInformation.LimitFlags = 0x00002000  # KILL_ON_JOB_CLOSE
    if not kernel32.SetInformationJobObject(job, 9, ctypes.byref(limits), ctypes.sizeof(limits)):
        kernel32.CloseHandle(job)
        return None
    return job


def _assign_to_job(job, proc: subprocess.Popen) -> bool:
    if not job or sys.platform != "win32":
        return False
    import ctypes
    from ctypes import wintypes

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
    kernel32.AssignProcessToJobObject.restype = wintypes.BOOL
    try:
        process_handle = proc._handle
    except AttributeError:
        return False
    return bool(kernel32.AssignProcessToJobObject(job, wintypes.HANDLE(process_handle)))


def _close_job(job) -> None:
    if job and sys.platform == "win32":
        import ctypes
        ctypes.WinDLL("kernel32", use_last_error=True).CloseHandle(job)


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
        self._job_handle = None

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
        self._stdout = open(self.stdout_path, "w", encoding="utf-8")
        self._stderr = open(self.stderr_path, "w", encoding="utf-8")
        self.proc = subprocess.Popen(
            self._command(), shell=False, cwd=self.work_dir, env=env,
            stdout=self._stdout, stderr=self._stderr, stdin=subprocess.DEVNULL,
            close_fds=True, **hidden_subprocess_kwargs(new_group=True),
        )
        self._job_handle = _create_kill_on_close_job()
        if self._job_handle and not _assign_to_job(self._job_handle, self.proc):
            _close_job(self._job_handle)
            self._job_handle = None
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
            _close_job(self._job_handle)
            self._job_handle = None
            return
        try:
            if self._job_handle:
                _close_job(self._job_handle)
                self._job_handle = None
            elif sys.platform == "win32":
                subprocess.run(["taskkill", "/F", "/T", "/PID", str(self.proc.pid)],
                               capture_output=True, timeout=10, **hidden_subprocess_kwargs())
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
        self._job_handle = None
