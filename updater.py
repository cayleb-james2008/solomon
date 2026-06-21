"""Solomon Updater — a small standalone exe that pulls the latest from the solomon git repo,
rebuilds the Solomon exe if anything changed, then launches it.

Designed to be built into its own exe (updater.spec) and placed next to Solomon.exe or run
from anywhere. It is the one-click "update + open" entry point: double-click the updater,
it brings solomon to the latest version and opens it.

Flow:
  1. Locate the solomon source repo (SOLOMON_HOME env, or walk up from the exe, or the repo
     beside the exe). Fails loudly if it can't find it.
  2. git fetch + check if origin/main is ahead of the local main. If so, pull.
  3. If the working tree changed (or the dist exe is missing), rebuild via PyInstaller using
     the maki venv python (the same one build.ps1 uses). The build is silent (CREATE_NO_WINDOW)
     and its stdout/stderr are streamed to this updater's console so the operator sees progress.
  4. If a stale Solomon.exe is running, stop it before rebuilding (a locked exe breaks the
     build), then relaunch the fresh one.
  5. Launch dist/Solomon/Solomon.exe and exit.

Safety:
  - Never force-pushes, never resets, never discards local commits. `git pull --ff-only` so a
    diverged tree fails loudly instead of merging surprise history.
  - Never touches uncommitted operator work: if `git status` is dirty, it skips the pull and
    rebuilds with the current tree (a dirty tree blocks a clean pull anyway), surfacing a
    warning. The operator can commit/stash and re-run.
  - The build uses the same venv python as build.ps1; if that venv is missing, it fails with a
    clear message instead of a cryptic PyInstaller error.
  - Best-effort: a build failure launches the existing exe (if present) so the operator isn't
    left without Solomon. A missing exe + failed build shows the error and exits non-zero.
"""
from __future__ import annotations

import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

from winproc import hidden_subprocess_kwargs

# The venv python used to build Solomon (same as build.ps1). Override with SOLOMON_BUILD_PY.
_BUILD_PY_DEFAULT = r"C:\Users\Cayleb\Desktop\workspace\projects\maki\.venv\Scripts\python.exe"


def _log(msg: str) -> None:
    """Print with a timestamp prefix so the console shows update progress."""
    t = time.strftime("%H:%M:%S")
    print(f"[{t}] {msg}", flush=True)


def _find_solomon_repo() -> Path | None:
    """Locate the solomon source repo. Resolution order:
      1. SOLOMON_HOME env var (must contain solomon.spec + control.py)
      2. walk UP from this exe's location (a dist/Solomon/updater.exe finds the source tree)
      3. the directory beside the exe
    Returns the repo path or None."""
    env_home = os.environ.get("SOLOMON_HOME")
    if env_home and os.path.isfile(os.path.join(env_home, "solomon.spec")) \
            and os.path.isfile(os.path.join(env_home, "control.py")):
        return Path(env_home)
    # frozen exe: walk up to find the source tree (dist/Solomon/updater.exe -> repo root)
    if getattr(sys, "frozen", False):
        d = Path(sys.executable).resolve().parent
        for _ in range(8):
            if (d / "solomon.spec").is_file() and (d / "control.py").is_file():
                return d
            parent = d.parent
            if parent == d:
                break
            d = parent
    return None


def _run(cmd: list[str], cwd: str | None = None,
         timeout: float | None = None) -> subprocess.CompletedProcess:
    """Run a subprocess, capturing output. CREATE_NO_WINDOW on win32 so the updater's own
    console is the only window the operator sees. `timeout` bounds network ops (fetch) so a stalled
    origin fails loudly instead of hanging; on expiry subprocess.run raises TimeoutExpired."""
    kw = {"capture_output": True, "text": True, "cwd": cwd}
    if timeout is not None:
        kw["timeout"] = timeout
    return subprocess.run(cmd, **kw, **hidden_subprocess_kwargs())


def _git(repo: Path, *args: str, timeout: float | None = None) -> subprocess.CompletedProcess:
    git = shutil.which("git")
    if not git:
        raise RuntimeError("git not found on PATH")
    return _run([git, "-C", str(repo), *args], timeout=timeout)


def _is_dirty(repo: Path) -> bool:
    """True if the working tree has uncommitted tracked changes (blocks a clean pull)."""
    r = _git(repo, "status", "--porcelain", "--untracked-files=no")
    return bool((r.stdout or "").strip())


def _has_origin(repo: Path) -> bool:
    return _git(repo, "remote", "get-url", "origin").returncode == 0


def _pull_latest(repo: Path) -> dict:
    """Fetch + ff-only pull from origin/main. Returns {updated:bool, error?}.
    Never merges surprise history; never discards local commits."""
    out = {"updated": False}
    if not _has_origin(repo):
        out["error"] = "no 'origin' remote — set one, or run the updater from inside the repo"
        return out
    branch = (_git(repo, "rev-parse", "--abbrev-ref", "HEAD").stdout or "").strip() or "main"
    try:
        fetch = _git(repo, "fetch", "origin", "--quiet", timeout=25)
    except subprocess.TimeoutExpired:
        out["error"] = "fetch from origin timed out (offline?) — re-run the updater when connected"
        return out
    if fetch.returncode != 0:
        out["error"] = "could not reach origin (offline?) — re-run the updater when connected"
        return out
    ahead = _git(repo, "log", "--oneline", f"origin/{branch}..{branch}")
    behind = _git(repo, "log", "--oneline", f"{branch}..origin/{branch}")
    if not (behind.stdout or "").strip():
        out["message"] = f"already on latest {branch}"
        return out
    if (ahead.stdout or "").strip():
        out["error"] = (f"local {branch} has diverged from origin/{branch} — "
                        "merge or rebase manually, then re-run the updater")
        return out
    r = _git(repo, "pull", "--ff-only", "origin", branch)
    if r.returncode != 0:
        out["error"] = (r.stderr or r.stdout or "git pull failed").strip()[:300]
        return out
    out["updated"] = True
    out["message"] = f"pulled latest {branch}"
    return out


def _exe_path(repo: Path) -> Path:
    return repo / "dist" / "Solomon" / "Solomon.exe"


def _stop_running_solomon() -> None:
    """If a stale Solomon.exe is running, stop it so the build can overwrite the file.
    Best-effort: a failure here doesn't block the update (the build will fail loudly on a
    locked file, which is the clearer error)."""
    if sys.platform != "win32":
        return
    try:
        r = _run(["tasklist", "/FI", "IMAGENAME eq Solomon.exe", "/FO", "CSV", "/NH"])
        if r.returncode == 0 and "Solomon.exe" in (r.stdout or ""):
            _log("Stopping running Solomon.exe so the build can replace it...")
            _run(["taskkill", "/F", "/IM", "Solomon.exe"])
            time.sleep(1.5)
    except OSError:
        pass


def _build(repo: Path) -> bool:
    """Run PyInstaller via the build venv python. Returns True on success.
    Mirrors build.ps1: --noconfirm, distpath=dist, workpath=build, solomon.spec."""
    py = os.environ.get("SOLOMON_BUILD_PY") or _BUILD_PY_DEFAULT
    if not os.path.isfile(py):
        _log(f"Build python not found: {py}")
        _log("Set SOLOMON_BUILD_PY to a venv python that has pyinstaller + pywebview installed.")
        return False
    _log("Building Solomon.exe with PyInstaller (this takes ~30s)...")
    r = _run([py, "-m", "PyInstaller", "--noconfirm",
              "--distpath", str(repo / "dist"),
              "--workpath", str(repo / "build"),
              str(repo / "solomon.spec")],
             cwd=str(repo))
    if r.returncode != 0:
        _log("Build failed:")
        if r.stderr:
            print(r.stderr[-1500:], flush=True)
        elif r.stdout:
            print(r.stdout[-1500:], flush=True)
        return False
    _log("Build OK.")
    return True


def _launch(repo: Path) -> bool:
    """Launch the freshly built (or existing) Solomon.exe. Returns True if launched."""
    exe = _exe_path(repo)
    if not exe.is_file():
        _log(f"Solomon.exe not found at {exe}")
        return False
    try:
        subprocess.Popen([str(exe)], close_fds=True,
                         **hidden_subprocess_kwargs(detached=True))
        _log(f"Launched Solomon: {exe}")
        return True
    except OSError as e:
        _log(f"Failed to launch Solomon: {e}")
        return False


def main() -> int:
    _log("Solomon Updater")
    repo = _find_solomon_repo()
    if not repo:
        _log("Could not find the Solomon source repo (needs solomon.spec + control.py).")
        _log("Set SOLOMON_HOME to the repo folder, or place this updater inside dist/Solomon/.")
        if not sys.stdin.isatty():
            input("\nPress Enter to exit...")
        return 2

    _log(f"Source repo: {repo}")
    exe = _exe_path(repo)
    had_exe = exe.is_file()

    # 1. Pull latest (best-effort — a dirty tree or no origin skips the pull, not a hard fail).
    if _is_dirty(repo):
        _log("Warning: working tree has uncommitted changes — skipping git pull.")
        _log("Commit or stash, then re-run the updater for a clean pull. Rebuilding with current tree.")
        pull = {"updated": False, "message": "dirty tree — pull skipped"}
    else:
        pull = _pull_latest(repo)
        if pull.get("error"):
            _log(f"Pull failed: {pull['error']}")
        elif pull.get("updated"):
            _log(f"Updated: {pull.get('message')}")
        else:
            _log(f"Up to date: {pull.get('message', 'no change')}")

    # 2. Rebuild if the pull updated the tree, or if the exe is missing.
    need_build = pull.get("updated") or not had_exe
    if need_build:
        _stop_running_solomon()
        if not _build(repo):
            # Build failed — fall back to the existing exe if present so the operator isn't stranded.
            if had_exe:
                _log("Build failed — launching the previous Solomon.exe instead.")
                return 0 if _launch(repo) else 1
            _log("Build failed and no previous exe exists. Fix the errors above and re-run.")
            if not sys.stdin.isatty():
                input("\nPress Enter to exit...")
            return 1
    else:
        _log("No changes — skipping rebuild.")

    # 3. Launch (or relaunch) Solomon.
    ok = _launch(repo)
    if not ok:
        if not sys.stdin.isatty():
            input("\nPress Enter to exit...")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())