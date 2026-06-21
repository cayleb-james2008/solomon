"""Solomon — backend logic for the cross-repo auto-iterator dashboard.

Pure-ish, unit-testable functions over a registry of repos (`repos.json`).
Each repo runs a pi-powered "improver" loop that writes a heartbeat and a PID
lock under `<repo>/.rsi/`. This module reads those, controls start/stop, and
drives GitHub PR accept/deny via the `gh` CLI.

Heartbeat schema (written by the runner — read-only contract, do NOT change):
    {repo, status, phase, pid, iteration, goal, model,
     tests:{passed,failed,errors,skipped,collected,green}|null,
     last_pr:{number,url,branch,state}|null,
     last_summary, started_at, updated_at, log_tail:[...], run_id (optional identity token)}
"""
import json
import os
import shutil
import subprocess
import sys
import time
import uuid
from datetime import datetime, timezone
from pathlib import Path

from winproc import hidden_subprocess_kwargs, visible_console_kwargs

def _base_dir():
    """The operator data dir (repos.json, improver/, runtime/, .env). When frozen, the PyInstaller
    bundle does NOT contain improver/ — the operator data lives in the real Solomon folder. Resolution
    order, frozen: an explicit SOLOMON_HOME env var, then improver/ beside the exe, then a walk UP from
    the exe (so a dist/Solomon nested inside the source tree still resolves), else the exe's own dir —
    and in that last case warn to stderr so a relocated exe whose operator data is missing fails loudly
    rather than silently showing zero repos. Unfrozen: the directory of this source file."""
    if getattr(sys, "frozen", False):
        home = os.environ.get("SOLOMON_HOME")
        if home and os.path.isdir(os.path.join(home, "improver")):
            return home
        d = os.path.dirname(os.path.abspath(sys.executable))
        probe = d
        for _ in range(6):                       # first iteration probes beside the exe
            if os.path.isdir(os.path.join(probe, "improver")):
                return probe
            parent = os.path.dirname(probe)
            if parent == probe:
                break
            probe = parent
        if sys.stderr:                           # windowed frozen exe (console=False) has stderr == None
            sys.stderr.write(
                f"Solomon: operator data (improver/, repos.json) not found next to {d}, in any ancestor, "
                f"or via SOLOMON_HOME — the dashboard will show no repos and start/supervise will fail. "
                f"Keep Solomon.exe beside the operator data folder, or set SOLOMON_HOME to it.\n")
        return d
    return os.path.dirname(os.path.abspath(__file__))


HERE = _base_dir()
REPOS_JSON = os.path.join(HERE, "repos.json")
# Sibling projects folder (…/workspace/projects). Drop a git repo here and it auto-registers.
PROJECTS_DIR = os.path.join(os.path.dirname(HERE), "projects")

_GH_FALLBACK = r"C:\Program Files\GitHub CLI\gh.exe"

# provider defaults — keep in sync with improver/run_improver.py PROVIDERS
_PROVIDER_DEFAULT_MODEL = {
    "ollama-cloud": "glm-5.2",
    "openrouter": "qwen/qwen3-coder",
}


# --------------------------------------------------------------------------- #
# registry
# --------------------------------------------------------------------------- #
def _has_origin(path):
    """True if `git -C <path> remote get-url origin` returns 0. Safe (False) on error."""
    git = _which_git()
    if not git or not path:
        return False
    try:
        return _run([git, "-C", path, "remote", "get-url", "origin"]).returncode == 0
    except OSError:
        return False


def _discover_projects():
    """Yield {name, path, branch_prefix, is_git, has_remote} for EVERY immediate
    subdir of PROJECTS_DIR (skipping dot-names) — so local non-git folders also
    appear "available in Solomon".

    `is_git` = a `.git` entry (dir or file) exists; `has_remote` = an `origin`
    remote is configured. Safe ([]) on OSError so a missing/unreadable projects
    folder never crashes."""
    out = []
    try:
        entries = sorted(os.scandir(PROJECTS_DIR), key=lambda e: e.name)
    except OSError:
        return out
    for e in entries:
        try:
            if not e.is_dir() or e.name.startswith("."):
                continue
        except OSError:
            continue
        path = os.path.abspath(e.path)
        is_git = os.path.exists(os.path.join(path, ".git"))
        out.append({"name": e.name, "path": path, "branch_prefix": "rsi/",
                    "is_git": is_git, "has_remote": _has_origin(path) if is_git else False})
    return out


def _read_repos_json(path=REPOS_JSON):
    """Raw list read of repos.json. Returns [] on missing/corrupt/non-list."""
    try:
        with open(path, "r", encoding="utf-8") as f:
            data = json.load(f)
        return data if isinstance(data, list) else []
    except (OSError, json.JSONDecodeError):
        return []


def load_repos(path=None):
    """Merge auto-discovered projects with repos.json config.

    Discovered git repos under PROJECTS_DIR seed a name->entry map; repos.json
    entries (matched by `name`) are layered ON TOP — they carry per-project
    config (provider, model, optional path override). Every entry gets a default
    `branch_prefix` of "rsi/". Non-dict / nameless config entries are skipped."""
    if path is None:
        path = REPOS_JSON  # live module global (monkeypatch-friendly)
    merged = {}
    for d in _discover_projects():
        merged[d["name"]] = dict(d)
    for r in _read_repos_json(path):
        if not isinstance(r, dict):
            continue
        name = r.get("name")
        if not name:
            continue
        base = merged.get(name, {})
        base.update(r)            # config overrides/augments the discovered entry
        base["name"] = name
        if not base.get("branch_prefix"):
            base["branch_prefix"] = "rsi/"
        merged[name] = base
    return list(merged.values())


# --------------------------------------------------------------------------- #
# per-project provider + model
# --------------------------------------------------------------------------- #
def project_provider(repo):
    return (repo or {}).get("provider") or "ollama-cloud"


def project_model(repo):
    return (repo or {}).get("model") or _PROVIDER_DEFAULT_MODEL.get(
        project_provider(repo), _PROVIDER_DEFAULT_MODEL["ollama-cloud"])


def project_ship(repo):
    """Ship mode for the repo: one of local|push|pr|auto-merge (default 'pr')."""
    return (repo or {}).get("ship") or "pr"


def project_gate(repo):
    """Optional custom shell test-command for the repo's gate, or None (built-in pytest)."""
    return (repo or {}).get("gate") or None


def project_pr_target_branch(repo):
    """Branch PRs open against — and the loop integrates from. Default 'main'."""
    return (repo or {}).get("pr_target_branch") or "main"


def project_interval(repo):
    """Seconds between iterations (default 120; non-positive/invalid -> 120)."""
    try:
        v = int((repo or {}).get("interval") or 0)
    except (TypeError, ValueError):
        v = 0
    return v if v > 0 else 120


def project_max_iterations(repo):
    """Iterations before the loop self-stops; 0 = unlimited (invalid -> 0)."""
    try:
        v = int((repo or {}).get("max_iterations") or 0)
    except (TypeError, ValueError):
        v = 0
    return max(0, v)


def effective_ship(repo, auto_push=True):
    """Ship mode actually used for a run: the repo's ship mode when the global auto-push
    gate is ON, else 'local' (keep gate-green work on a local branch, never push/PR)."""
    return project_ship(repo) if auto_push else "local"


def project_reasoning(repo):
    """Agent thinking/reasoning level (pi --thinking): off|minimal|low|medium|high|xhigh.
    Default is 'xhigh' (max reasoning) — every repo runs the agent at full reasoning unless an
    operator explicitly lowers it."""
    return (repo or {}).get("reasoning") or "xhigh"


def project_goal(repo):
    """The operator's heavily-weighted north-star GOAL for this repo's RSI loop, or '' (unset).
    Baked into the contract (the per-iteration system prompt) + the provisioner so every iteration
    is steered by it."""
    return ((repo or {}).get("goal") or "").strip()


def project_sandbox(repo):
    """Per-repo sandbox config for visual E2E review, or None (review disabled).

    Schema (stored as the `sandbox` key in repos.json):
        enabled: bool — master switch; off => visual review is skipped
        launch: str — shell command to start the app in the branch cwd
        port_env: str — env var name to inject the ephemeral port as
        state_env: str — env var name to inject the temp state dir as
        extra_env: dict — sandbox-only env vars (disposable profile, auto-post off, etc.)
        health: str — path to poll for 200 (default "/")
        pages: list — page paths to capture (e.g. ["/", "/dashboard/cockpit/"])
        vision_model: str — vision-capable model id for the reviewer agent
    """
    sb = (repo or {}).get("sandbox")
    if not isinstance(sb, dict) or not sb.get("enabled"):
        return None
    return sb


def read_visual_review(repo):
    """Read the latest visual review report from runtime/<name>/visual_review/report.json.
    Returns the report dict (with screenshot paths) or None if no review has been run."""
    rt = _runtime_dir(repo)
    if not rt:
        return None
    p = os.path.join(rt, "visual_review", "report.json")
    try:
        with open(p, "r", encoding="utf-8") as f:
            report = json.load(f)
        # also list screenshot files so the UI can serve them
        shot_dir = os.path.join(rt, "visual_review")
        shots = []
        if os.path.isdir(shot_dir):
            for fn in sorted(os.listdir(shot_dir)):
                if fn.endswith(".png"):
                    shots.append(fn)
        if isinstance(report, dict):
            report["screenshot_files"] = shots
        return report
    except (OSError, json.JSONDecodeError):
        return None


def set_repo_config(name, provider=None, model=None, ship=None, gate=None,
                    pr_target_branch=None, interval=None, max_iterations=None,
                    reasoning=None, goal=None, sandbox=None, phases=None):
    """Upsert the repos.json entry for `name`, setting any passed (non-None) keys.
    Creates the entry (carrying its discovered path) if it doesn't exist."""
    if not name:
        return {"ok": False, "error": "name required"}
    entries = _read_repos_json(REPOS_JSON)  # live module global (monkeypatch-friendly)
    entry = None
    for r in entries:
        if isinstance(r, dict) and r.get("name") == name:
            entry = r
            break
    if entry is None:
        entry = {"name": name, "branch_prefix": "rsi/"}
        disc = next((d for d in _discover_projects() if d["name"] == name), None)
        if disc:
            entry["path"] = disc["path"]
        entries.append(entry)
    if provider is not None:
        entry["provider"] = provider
    if model is not None:
        entry["model"] = model
    if ship is not None:
        entry["ship"] = ship
    if gate is not None:
        entry["gate"] = gate
    if pr_target_branch is not None:
        entry["pr_target_branch"] = pr_target_branch
    if interval is not None:
        entry["interval"] = interval
    if max_iterations is not None:
        entry["max_iterations"] = max_iterations
    if reasoning is not None:
        entry["reasoning"] = reasoning
    if goal is not None:
        entry["goal"] = goal
    if sandbox is not None:
        entry["sandbox"] = sandbox
    if phases is not None:
        # full-replace the per-phase model/provider/reasoning map (the UI sends the complete desired
        # state); drop empty per-phase entries so the row stays clean, and {} clears all overrides.
        entry["phases"] = {k: v for k, v in phases.items() if isinstance(v, dict) and v}
        if not entry["phases"]:
            entry.pop("phases", None)
    try:
        # atomic write (tmp + os.replace): a crash or a concurrent reader/writer must never see a
        # half-written repos.json — a truncate-in-place that fails mid-write would drop EVERY repo's
        # config. The operator may be editing this file in parallel, so never truncate in place.
        tmp = REPOS_JSON + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(entries, f, indent=2)
        os.replace(tmp, REPOS_JSON)
        return {"ok": True}
    except OSError as e:
        return {"ok": False, "error": str(e)}


# --------------------------------------------------------------------------- #
# helpers
# --------------------------------------------------------------------------- #
def _repo_path(repo):
    return (repo or {}).get("path") or ""


def _repo_name(repo):
    p = _repo_path(repo)
    return (repo or {}).get("name") or (os.path.basename(p) if p else "")


def _runtime_dir(repo):
    """Per-repo runtime (heartbeat/lock/stop/log) lives UNDER Solomon, not inside the
    target repo — so the product stays completely RSI-free."""
    name = _repo_name(repo)
    return os.path.join(HERE, "runtime", name) if name else None


def _venv_python(repo):
    p = _repo_path(repo)
    return os.path.join(p, ".venv", "Scripts", "python.exe") if p else None


def _runner_python(repo):
    """The interpreter to spawn run_improver.py with: the repo's own .venv python when it exists, else
    the current interpreter — but NEVER the frozen exe. When frozen, sys.executable is Solomon.exe, and
    spawning [Solomon.exe, run_improver.py, ...] would fall through to main() and launch a ghost
    dashboard window. In that case fall back to a DISCOVERED system Python host (run_improver.py is
    stdlib-only, so any Python >=3.11 can host it) — this makes onboarding a venv-less repo (e.g. a Node
    project) zero-touch. Returns a path, or None only when frozen AND no repo .venv AND no system Python
    is found (so the caller fails loudly with actionable guidance instead of spawning a stray GUI)."""
    py = _venv_python(repo)
    if py and os.path.exists(py):
        return py
    if not getattr(sys, "frozen", False):
        return sys.executable
    return _discover_host_python()


_HOST_PY_CACHE = None


def _discover_host_python():
    """A real Python >=3.11 to host run_improver.py when Solomon is frozen and the target repo has no
    .venv. Tries the `py` launcher (py -3), then common names on PATH. Verifies the candidate is a real
    Python >=3.11 and not Solomon.exe. Cached for the process. Returns a path or None.

    This is pure host discovery — it creates NO venv inside the target repo, so it cannot produce an
    untracked `.venv/` (the onboarding refusal we hit) or a locked-interpreter stash failure."""
    global _HOST_PY_CACHE
    if _HOST_PY_CACHE is not None:
        return _HOST_PY_CACHE or None
    candidates = []
    launcher = shutil.which("py")
    if launcher:
        try:
            out = _run([launcher, "-3", "-c", "import sys;print(sys.executable)"]).stdout.strip()
            if out:
                candidates.append(out)
        except OSError:
            pass
    for name in ("python3.12", "python3.11", "python3", "python"):
        w = shutil.which(name)
        if w:
            candidates.append(w)
    for c in candidates:
        try:
            if not c or not os.path.exists(c) or os.path.basename(c).lower().startswith("solomon"):
                continue
            ver = _run([c, "-c", "import sys;print('%d.%d' % sys.version_info[:2])"]).stdout.strip()
            maj, _, minr = ver.partition(".")
            if maj == "3" and minr.isdigit() and int(minr) >= 11:
                _HOST_PY_CACHE = c
                return c
        except (OSError, ValueError):
            continue
    _HOST_PY_CACHE = ""
    return None


def _clean_subenv():
    """os.environ minus GITHUB_TOKEN/GH_TOKEN and PYTHONPATH/PYTHONHOME.

    - GITHUB_TOKEN/GH_TOKEN: Solomon authenticates gh via `gh auth login` (keyring); a stale token
      env var would shadow it and make gh exit non-zero — blocking the GitHub-ready gate, blanking
      the PR list, and breaking pushes. Stripping it forces the keyring path.
    - PYTHONPATH/PYTHONHOME: when Solomon runs from one Python (e.g. 3.11) and spawns a repo's
      `.venv` python of a different minor (e.g. 3.12), a leaked PYTHONPATH/PYTHONHOME makes the
      child load the wrong stdlib and crash with `SRE module mismatch` on `import re`. Strip them
      so each venv python uses only its own stdlib."""
    env = dict(os.environ)
    for k in ("GITHUB_TOKEN", "GH_TOKEN", "PYTHONPATH", "PYTHONHOME"):
        env.pop(k, None)
    # Run spawned children in UTF-8 mode so the runner's stdout AND every subprocess.run(text=True) it
    # makes (git diffs, the test gate's output, gh) decode/encode as UTF-8 instead of the Windows
    # cp1252 locale — a non-cp1252 char (≥, em-dash, emoji) in agent output otherwise raises
    # Unicode{Encode,Decode}Error and CRASHES the iteration (observed: asmodeus died mid-iteration).
    env["PYTHONUTF8"] = "1"
    env["PYTHONIOENCODING"] = "utf-8"
    return env


def _run(args, cwd=None):
    """Run a subprocess, capturing output. Window-hidden on win32 (no console popup); broken gh token stripped."""
    kw = {"capture_output": True, "text": True, "cwd": cwd, "env": _clean_subenv()}
    return subprocess.run(args, **kw, **hidden_subprocess_kwargs())


def _which_gh():
    return shutil.which("gh") or (_GH_FALLBACK if os.path.exists(_GH_FALLBACK) else None)


def _which_git():
    return shutil.which("git")


# --------------------------------------------------------------------------- #
# watchdog self-rearm — the SolomonWatchdog scheduled task is the entire keep-alive
# layer; if it is deleted/disabled, crashed loops stay dead and repos go silent with
# no signal. Nothing previously noticed or re-created it. _ensure_watchdog_task makes
# opening the dashboard (app.main) idempotently re-arm a MISSING task.
# --------------------------------------------------------------------------- #
def _watchdog_python():
    """A WINDOWLESS python (pythonw.exe) that can import control/monitor for the scheduled sweep.
    Prefer the maki .venv (the established watchdog interpreter — scripts/watchdog.cmd uses it), else a
    pythonw beside the current interpreter when unfrozen. None when none is usable (caller no-ops)."""
    cand = os.path.join(PROJECTS_DIR, "maki", ".venv", "Scripts", "pythonw.exe")
    if os.path.isfile(cand):
        return cand
    if not getattr(sys, "frozen", False):
        pw = os.path.join(os.path.dirname(sys.executable), "pythonw.exe")
        return pw if os.path.isfile(pw) else sys.executable
    return None


def _ensure_watchdog_task():
    """Idempotently ensure the SolomonWatchdog scheduled task exists (windowless `pythonw monitor.py`,
    every 2 min). Query first; CREATE only if MISSING — never clobber an existing task or its operator-
    chosen schedule. Best-effort: any failure (no schtasks, locked-down host, elevation prompt) is
    swallowed so it never blocks startup. Windows-only. Returns a short status string for logging."""
    if sys.platform != "win32":
        return "skipped (not win32)"
    try:
        if _run(["schtasks", "/Query", "/TN", "SolomonWatchdog"]).returncode == 0:
            return "present"
        py = _watchdog_python()
        monitor = os.path.join(HERE, "monitor.py")
        if not py or not os.path.isfile(monitor):
            return "missing (no python/monitor to arm)"
        c = _run(["schtasks", "/Create", "/TN", "SolomonWatchdog", "/TR", f'"{py}" "{monitor}"',
                  "/SC", "MINUTE", "/MO", "2", "/F"])
        return "armed" if c.returncode == 0 else f"arm-failed: {(c.stderr or c.stdout or '').strip()[:120]}"
    except OSError as e:
        return f"arm-error: {e}"


# --------------------------------------------------------------------------- #
# in-app updater (source-rebuild model — see updater.py / SolomonUpdater.exe)
# --------------------------------------------------------------------------- #
def _solomon_repo():
    """Locate Solomon's own git checkout (the source-rebuild target). Resolution mirrors
    updater._find_solomon_repo: SOLOMON_HOME, then a walk UP from HERE/sys.executable looking
    for solomon.spec + control.py. Returns a path str, or None if not found."""
    def _is_repo(d):
        return os.path.isfile(os.path.join(d, "solomon.spec")) \
            and os.path.isfile(os.path.join(d, "control.py"))

    env_home = os.environ.get("SOLOMON_HOME")
    if env_home and _is_repo(env_home):
        return env_home
    starts = [HERE]
    if getattr(sys, "frozen", False):
        starts.append(os.path.dirname(os.path.abspath(sys.executable)))
    for start in starts:
        d = os.path.abspath(start)
        for _ in range(8):
            if _is_repo(d):
                return d
            parent = os.path.dirname(d)
            if parent == d:
                break
            d = parent
    return None


def current_sha():
    """Short commit sha of Solomon's own checkout (e.g. 'a1b2c3d'), or None when unavailable.
    Used for the version/identity shown in the UI."""
    repo = _solomon_repo()
    git = _which_git()
    if not repo or not git:
        return None
    try:
        r = _run([git, "-C", repo, "rev-parse", "--short", "HEAD"])
    except OSError:
        return None
    return (r.stdout or "").strip() or None if r.returncode == 0 else None


def update_status():
    """Check whether Solomon's own checkout is behind its remote default branch.

    Runs `git fetch` then counts origin/<branch>..HEAD (behind) and detects a dirty tree
    (uncommitted tracked changes block an ff pull). An update is 'available' only when behind>0
    AND not dirty (a dirty tree must be committed/stashed first — surfaced via reason).

    Returns a dict: {ok, available, behind, dirty, currentSha, branch, reason?, error?}.
    Pure-read + network fetch only; never modifies the tree. Safe (ok:False) on any error."""
    repo = _solomon_repo()
    git = _which_git()
    if not repo:
        return {"ok": False, "error": "Solomon source repo not found (needs solomon.spec + control.py)",
                "available": False, "behind": 0, "dirty": False, "currentSha": None}
    if not git:
        return {"ok": False, "error": "git not found on PATH",
                "available": False, "behind": 0, "dirty": False, "currentSha": None}

    def g(*args):
        return _run([git, "-C", repo, *args])

    try:
        branch = (g("rev-parse", "--abbrev-ref", "HEAD").stdout or "").strip() or "main"
        sha = (g("rev-parse", "--short", "HEAD").stdout or "").strip() or None
        if g("remote", "get-url", "origin").returncode != 0:
            return {"ok": True, "available": False, "behind": 0, "dirty": False,
                    "currentSha": sha, "branch": branch, "reason": "no 'origin' remote"}
        g("fetch", "origin", "--quiet")
        dirty = bool((g("status", "--porcelain", "--untracked-files=no").stdout or "").strip())
        cnt = g("rev-list", "--count", f"HEAD..origin/{branch}")
        behind = int((cnt.stdout or "0").strip() or "0") if cnt.returncode == 0 else 0
    except (OSError, ValueError) as e:
        return {"ok": False, "error": str(e), "available": False, "behind": 0,
                "dirty": False, "currentSha": None}

    available = behind > 0 and not dirty
    out = {"ok": True, "available": available, "behind": behind, "dirty": dirty,
           "currentSha": sha, "branch": branch}
    if behind > 0 and dirty:
        out["reason"] = "update available but working tree is dirty — commit or stash first"
    return out


def apply_update():
    """Spawn the source-rebuild updater detached, then signal the app to exit so the updater can
    rebuild + relaunch (it kills any running Solomon.exe itself). Prefers the prebuilt
    SolomonUpdater.exe; falls back to running updater.py with the maki build venv python.

    Returns {ok, started, mode?, error?}. Does NOT exit the process — the caller (app.py) decides
    when to close the window after this returns started:True."""
    repo = _solomon_repo()
    if not repo:
        return {"ok": False, "started": False, "error": "Solomon source repo not found"}
    env = dict(os.environ)
    env["SOLOMON_HOME"] = repo  # so the spawned updater resolves the same checkout
    exe = os.path.join(repo, "dist", "updater", "SolomonUpdater", "SolomonUpdater.exe")
    try:
        if os.path.isfile(exe):
            subprocess.Popen([exe], cwd=repo, close_fds=True, env=env,
                             **hidden_subprocess_kwargs(detached=True, new_group=True))
            return {"ok": True, "started": True, "mode": "exe"}
        # Fallback: run updater.py with the build venv python (it has pyinstaller + pywebview).
        from updater import _BUILD_PY_DEFAULT  # local import: avoids a hard dep at module load
        py = env.get("SOLOMON_BUILD_PY") or _BUILD_PY_DEFAULT
        if not os.path.isfile(py):
            return {"ok": False, "started": False,
                    "error": f"updater exe missing and build python not found: {py}"}
        subprocess.Popen([py, os.path.join(repo, "updater.py")], cwd=repo, close_fds=True, env=env,
                         **hidden_subprocess_kwargs(detached=True, new_group=True))
        return {"ok": True, "started": True, "mode": "source"}
    except OSError as e:
        return {"ok": False, "started": False, "error": str(e)}


def has_frontend(repo):
    """Return whether a repository exposes a user-facing web surface.

    Detection is deliberately cheap and root-scoped: it is used while loading the
    dashboard, so it must not recursively walk node_modules or large worktrees.
    """
    path = _repo_path(repo)
    if not path or not os.path.isdir(path):
        return False
    markers = (
        "index.html", "web/index.html", "public/index.html", "src/index.html",
        "templates", "web", "frontend", "client",
    )
    for marker in markers:
        candidate = os.path.join(path, *marker.split("/"))
        if os.path.isfile(candidate) or (marker in {"templates", "frontend", "client"}
                                         and os.path.isdir(candidate)):
            return True
    package_path = os.path.join(path, "package.json")
    try:
        with open(package_path, "r", encoding="utf-8") as f:
            package = json.load(f)
    except (OSError, json.JSONDecodeError):
        package = {}
    deps = {}
    for key in ("dependencies", "devDependencies"):
        value = package.get(key)
        if isinstance(value, dict):
            deps.update(value)
    frameworks = {
        "react", "react-dom", "vue", "@vue/cli-service", "svelte", "@sveltejs/kit",
        "next", "nuxt", "vite", "astro", "angular", "@angular/core",
    }
    if frameworks.intersection(deps):
        return True
    scripts = package.get("scripts") if isinstance(package, dict) else None
    return bool(isinstance(scripts, dict) and any(k in scripts for k in ("dev", "start", "preview")))


# --------------------------------------------------------------------------- #
# keys config (.env) — global per-provider API keys
# --------------------------------------------------------------------------- #
_ENV_FILE = os.path.join(HERE, ".env")
_PROVIDER_ENV_KEY = {"ollama-cloud": "OLLAMA_API_KEY", "openrouter": "OPENROUTER_API_KEY"}


def set_key(provider, value):
    """Upsert the provider's env line in <HERE>/.env, preserving other lines.
    Returns {ok} / {ok:false,error}."""
    key = _PROVIDER_ENV_KEY.get(provider)
    if not key:
        return {"ok": False, "error": f"unknown provider: {provider}"}
    try:
        lines = []
        if os.path.exists(_ENV_FILE):
            with open(_ENV_FILE, "r", encoding="utf-8") as f:
                lines = f.read().splitlines()
        new_line = f"{key}={value or ''}"
        found = False
        for i, line in enumerate(lines):
            if line.split("=", 1)[0].strip() == key:
                lines[i] = new_line
                found = True
                break
        if not found:
            lines.append(new_line)
        with open(_ENV_FILE, "w", encoding="utf-8") as f:
            f.write("\n".join(lines) + "\n")
        return {"ok": True}
    except OSError as e:
        return {"ok": False, "error": str(e)}


def keys_status():
    """{"ollama-cloud": bool, "openrouter": bool} — whether each provider's key
    line exists and is non-empty in .env. NEVER returns the key values."""
    present = {p: False for p in _PROVIDER_ENV_KEY}
    try:
        with open(_ENV_FILE, "r", encoding="utf-8") as f:
            content = f.read()
    except OSError:
        return present
    by_key = {}
    for line in content.splitlines():
        if "=" not in line:
            continue
        k, v = line.split("=", 1)
        by_key[k.strip()] = v.strip().strip('"').strip("'")
    for prov, key in _PROVIDER_ENV_KEY.items():
        present[prov] = bool(by_key.get(key))
    return present


# --------------------------------------------------------------------------- #
# GitHub connect + add project
# --------------------------------------------------------------------------- #
def github_status():
    """{"ready": gh_ready(), "login": <handle or None>}. login via gh api; None on failure."""
    ready = gh_ready()
    login = None
    gh = _which_gh()
    if gh:
        try:
            r = _run([gh, "api", "user", "--jq", ".login"])
            if r.returncode == 0:
                login = (r.stdout or "").strip() or None
        except OSError:
            login = None
    return {"ready": ready, "login": login}


def github_login_start():
    """Start GitHub CLI's browser-based login without blocking the dashboard."""
    gh = _which_gh()
    if not gh:
        return {"ok": False, "error": "gh not found"}
    if gh_ready():
        status = github_status()
        return {"ok": True, "already": True, "login": status.get("login")}
    try:
        # `gh auth login --web` is an INTERACTIVE device-code flow: it prints a one-time code and
        # waits for the operator to authorize in the browser. Spawn it in its OWN VISIBLE console
        # (no hidden window, no DEVNULL'd streams) so the operator can actually see the code and
        # complete login — hiding it made GitHub onboarding silently hang (onboarding-1).
        subprocess.Popen(
            [gh, "auth", "login", "--hostname", "github.com", "--git-protocol", "https", "--web"],
            cwd=HERE, env=_clean_subenv(), close_fds=True, **visible_console_kwargs(),
        )
        return {"ok": True, "started": True}
    except OSError as e:
        return {"ok": False, "error": str(e)}


def _gh_repo_visibility(path):
    """True / False if the repo at `path` has a PUBLIC / PRIVATE GitHub origin, else None (gh missing,
    not a GitHub remote, or the call failed). gh infers owner/repo from the origin remote in `path`.
    Used to default repos.json `public` on connect so the leak guard is active without a manual edit."""
    gh = _which_gh()
    if not gh:
        return None
    try:
        r = _run([gh, "repo", "view", "--json", "visibility", "--jq", ".visibility"], cwd=path)
    except OSError:
        return None
    if r.returncode != 0:
        return None
    v = (r.stdout or "").strip().upper()
    return True if v == "PUBLIC" else (False if v == "PRIVATE" else None)


def _parse_repo_spec(spec):
    """Return the bare repo name from a GitHub spec, or None if it's not one.
    Accepts https://github.com/owner/repo[.git] or owner/repo."""
    s = (spec or "").strip()
    if not s:
        return None
    if s.startswith("http://") or s.startswith("https://"):
        if "github.com/" not in s:
            return None
        s = s.split("github.com/", 1)[1]
    s = s.strip("/")
    if s.endswith(".git"):
        s = s[:-4]
    parts = [p for p in s.split("/") if p]
    if len(parts) != 2:
        return None
    return parts[1]


def add_project(spec):
    """Clone a GitHub repo into PROJECTS_DIR (so it auto-discovers).
    Returns {ok:true, name} or {ok:false, error}."""
    name = _parse_repo_spec(spec)
    if not name:
        return {"ok": False, "error": "not a GitHub repo spec (owner/repo or URL)"}
    gh = _which_gh()
    if not gh:
        return {"ok": False, "error": "gh not found"}
    try:
        os.makedirs(PROJECTS_DIR, exist_ok=True)
    except OSError as e:
        return {"ok": False, "error": str(e)}
    dest = os.path.join(PROJECTS_DIR, name)
    try:
        r = _run([gh, "repo", "clone", spec.strip(), dest], cwd=PROJECTS_DIR)
    except OSError as e:
        return {"ok": False, "error": str(e)}
    if r.returncode == 0:
        return {"ok": True, "name": name}
    return {"ok": False, "error": (r.stderr or r.stdout or "clone failed").strip()}


def _write_repo_entries(entries):
    """Atomically persist a complete repos.json list."""
    try:
        os.makedirs(os.path.dirname(REPOS_JSON) or ".", exist_ok=True)
        tmp = REPOS_JSON + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(entries, f, indent=2)
        os.replace(tmp, REPOS_JSON)
        return {"ok": True}
    except OSError as e:
        return {"ok": False, "error": str(e)}


def connect_project(spec, goal=None, ship="pr", visual_gate=None, provider=None):
    """Register a local directory or clone/register a GitHub repository in one call.

    provider: which LLM provider the loop uses for this repo. None -> auto-pick whichever provider's
    key the operator has actually entered (so a 'successful' connect is immediately runnable), which
    fixes the OpenRouter-only operator getting a silently non-runnable repo (onboarding-3)."""
    raw = os.path.expandvars(os.path.expanduser((spec or "").strip()))
    if os.path.isdir(raw):
        path = os.path.abspath(raw)
        name = os.path.basename(os.path.normpath(path))
    else:
        cloned = add_project(raw)
        if not cloned.get("ok"):
            return cloned
        name = cloned["name"]
        path = os.path.abspath(os.path.join(PROJECTS_DIR, name))
    if not name or not os.path.isdir(path):
        return {"ok": False, "error": "project path not found"}

    git_dir = os.path.join(path, ".git")
    entry = {
        "name": name,
        "path": path,
        "branch_prefix": "rsi/",
        "is_git": os.path.exists(git_dir),
        "has_remote": _has_origin(path) if os.path.exists(git_dir) else False,
        "ship": ship or "pr",
    }
    if goal is not None:
        entry["goal"] = str(goal).strip()
    # Provider (onboarding-3): explicit arg wins; else the provider whose key is actually present;
    # else leave unset (project_provider() then falls back to the default). Written so the runner +
    # contract enrichment use a key the operator has, instead of silently defaulting to ollama-cloud.
    if provider is None:
        ks = keys_status()
        provider = next((p for p in ("ollama-cloud", "openrouter") if ks.get(p)), None)
    if provider:
        entry["provider"] = provider
    # Public visibility (onboarding-2): auto-detect from the origin so the leak guard
    # (deny_terms / private_paths / secret-shape block) is ACTIVE by default on a public remote,
    # instead of being silently off until the operator hand-edits repos.json.
    if entry["has_remote"]:
        vis = _gh_repo_visibility(path)
        if vis is not None:
            entry["public"] = vis
    resolved_visual_gate = bool(has_frontend(entry)) if visual_gate is None else bool(visual_gate)
    entry["visual_gate"] = resolved_visual_gate

    entries = _read_repos_json(REPOS_JSON)
    existing = next((r for r in entries if isinstance(r, dict) and r.get("name") == name), None)
    if existing is None:
        entries.append(entry)
    else:
        existing.update(entry)
        entry = existing
    written = _write_repo_entries(entries)
    if not written.get("ok"):
        return written

    contracts = ensure_contracts(entry)
    enriching = False
    try:
        enriching = bool(enrich_contract(entry, background=True).get("ok"))
    except Exception:  # noqa: BLE001 - registration remains useful if enrichment cannot start
        enriching = False
    return {
        "ok": True,
        "name": name,
        "path": path,
        "visual_gate": resolved_visual_gate,
        "contracts": contracts,
        "enriching": enriching,
        "provider_ready": bool(keys_status().get(project_provider(entry))),
        "sandbox_configured": bool(project_sandbox(entry)),
    }


def publish_to_github(name, private=True):
    """Publish a local project to GitHub: git-init if needed, ensure an initial
    commit, then `gh repo create <name> --source <path> --remote origin --push`.
    Returns {ok:true,url} or {ok:false,error}. Requires GitHub connected."""
    repo = next((r for r in load_repos() if r.get("name") == name), None)
    if not repo:
        return {"ok": False, "error": f"unknown repo: {name}"}
    path = _repo_path(repo)
    if not path or not os.path.isdir(path):
        return {"ok": False, "error": "repo has no valid 'path'"}
    gh_info = github_status()
    if not gh_info.get("ready"):
        return {"ok": False, "error": "GitHub not connected — run gh auth login (Connect GitHub)"}
    git = _which_git()
    if not git:
        return {"ok": False, "error": "git not found"}
    gh = _which_gh()
    if not gh:
        return {"ok": False, "error": "gh not found"}
    login = gh_info.get("login") or "rsi-control"

    def g(*args):
        return _run([git, "-C", path, *args])

    try:
        # 1. git init if needed
        if not os.path.exists(os.path.join(path, ".git")):
            init = g("init", "-b", "main")
            if init.returncode != 0:
                return {"ok": False, "error": (init.stderr or init.stdout or "git init failed").strip()}
        # 2. ensure a local identity (only set if missing)
        if not (g("config", "user.name").stdout or "").strip():
            g("config", "user.name", login)
            g("config", "user.email", f"{login}@users.noreply.github.com")
        # 3. ensure an initial commit (HEAD missing == empty repo)
        if g("rev-parse", "--verify", "HEAD").returncode != 0:
            g("add", "-A")
            commit = g("commit", "-m", "Initial commit")
            if commit.returncode != 0:
                return {"ok": False,
                        "error": (commit.stderr or commit.stdout or "initial commit failed").strip()}
        # 4. create the GitHub repo + push
        vis = "--private" if private else "--public"
        create = _run([gh, "repo", "create", name, "--source", path,
                       "--remote", "origin", "--push", vis], cwd=path)
    except OSError as e:
        return {"ok": False, "error": str(e)}
    if create.returncode != 0:
        return {"ok": False, "error": (create.stderr or create.stdout or "gh repo create failed").strip()}
    url = None
    for line in ((create.stdout or "") + "\n" + (create.stderr or "")).splitlines():
        line = line.strip()
        if "github.com/" in line and line.startswith("http"):
            url = line
            break
    return {"ok": True, "url": url}


# --------------------------------------------------------------------------- #
# heartbeat / running state
# --------------------------------------------------------------------------- #
def read_heartbeat(repo):
    """json.load of <repo.path>/.rsi/heartbeat.json, or None if missing/corrupt."""
    rsi = _runtime_dir(repo)
    if not rsi:
        return None
    try:
        with open(os.path.join(rsi, "heartbeat.json"), "r", encoding="utf-8") as f:
            return json.load(f)
    except (OSError, json.JSONDecodeError):
        return None


def read_log(repo, max_bytes=16384):
    """Tail (last max_bytes) of runtime/<name>/improver.log. {ok, log} — log is '' if none."""
    rt = _runtime_dir(repo)
    if not rt:
        return {"ok": False, "error": "repo has no 'path'"}
    try:
        with open(os.path.join(rt, "improver.log"), "rb") as f:
            f.seek(0, os.SEEK_END)
            size = f.tell()
            f.seek(max(0, size - max_bytes))
            data = f.read()
    except OSError:
        return {"ok": True, "log": ""}
    return {"ok": True, "log": data.decode("utf-8", "replace")}


def read_history(repo, limit=50):
    """Last `limit` iteration-outcome records from runtime/<name>/history.jsonl (newest last).
    Returns a list (oldest→newest); [] on missing/corrupt."""
    rt = _runtime_dir(repo)
    if not rt:
        return []
    try:
        with open(os.path.join(rt, "history.jsonl"), "r", encoding="utf-8") as f:
            lines = f.read().splitlines()
    except OSError:
        return []
    out = []
    for line in lines[-limit:]:
        line = line.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(rec, dict):
            out.append(rec)
    return out


def _pid_alive(pid):
    if not pid:
        return False
    if sys.platform == "win32":
        # /NH /FO CSV so the PID appears only as a quoted field — an exact match, not a substring of
        # some other column/PID in tasklist's formatted table (the substring form gave false 'alive',
        # wedging start()/clear_lock()). Mirrors run_improver._pid_alive.
        r = _run(["tasklist", "/FI", f"PID eq {int(pid)}", "/NH", "/FO", "CSV"])
        return f'"{int(pid)}"' in (r.stdout or "")
    try:
        os.kill(int(pid), 0)
        return True
    except (OSError, ValueError):
        return False


def _read_lock(rt):
    """Parse runtime/<name>/lock -> (pid:int, run_id:str|None). The lock is '<pid>' (legacy) or
    '<pid>\\n<run_id>' — a per-run identity token so a recycled OS PID can't masquerade as the live
    loop. Returns (0, None) on missing/empty/corrupt."""
    try:
        with open(os.path.join(rt, "lock"), "r", encoding="utf-8") as f:
            raw = (f.read() or "").strip()
    except OSError:
        return 0, None
    if not raw:
        return 0, None
    lines = raw.splitlines()
    try:
        pid = int(lines[0].strip())
    except (ValueError, IndexError):
        return 0, None
    run_id = lines[1].strip() if len(lines) > 1 and lines[1].strip() else None
    return pid, run_id


def _heartbeat_age(hb):
    """Seconds since the heartbeat's updated_at, or None if absent/unparseable."""
    ts = (hb or {}).get("updated_at")
    if not ts:
        return None
    try:
        last = datetime.strptime(ts, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)
    except (ValueError, TypeError):
        return None
    return (datetime.now(timezone.utc) - last).total_seconds()


def _lock_is_live(repo, rt=None):
    """Whether runtime/<name>/lock is held by a LIVE runner — not merely a process that happens to own
    the recorded PID. Windows recycles PIDs aggressively, so a dead loop's PID can be reused by an
    unrelated process and pin the loop 'running' forever — a wedge the supervisor's stale_lock recovery
    could never clear (clear_lock refused the 'live' PID). Liveness now requires, beyond a live PID:
      - if BOTH the lock and the heartbeat carry a run-id, they must MATCH (a fresh heartbeat under a
        different run-id means a newer runner owns the loop and this lock is orphaned); and
      - the heartbeat must not be STALE beyond a generous window (>= the longest agent session, so a
        runner mid-session is never mistaken for dead). A missing/unparseable heartbeat is NOT treated
        as dead — a just-spawned runner hasn't written one yet."""
    rt = rt or _runtime_dir(repo)
    if not rt:
        return False
    pid, run_id = _read_lock(rt)
    if not pid or not _pid_alive(pid):
        return False
    hb = read_heartbeat(repo)
    if not isinstance(hb, dict):
        return True                          # lock + live PID, no heartbeat yet -> just-started, treat as live
    if run_id and hb.get("run_id") and hb.get("run_id") != run_id:
        return False                         # a newer runner owns the heartbeat; this lock is orphaned
    if hb.get("status") == "stopped":
        return False                         # the lock's runner CLEANLY EXITED (status=stopped). Even if
                                             # its lock lingered (release_lock skipped it on a pid mismatch)
                                             # and the recorded PID was recycled by an unrelated process,
                                             # the runner is gone — else the loop could never be restarted.
                                             # A fresh runner overwrites this status ~1s after acquiring.
    age = _heartbeat_age(hb)
    if age is None:
        return True                          # no usable timestamp -> don't declare a live PID dead on that alone
    return age <= max(3 * project_interval(repo), 3600)


def is_running(repo):
    """True if runtime/<name>/lock is held by a LIVE runner (PID alive + run-id/heartbeat-freshness
    corroborated, so a recycled OS PID can't pin a dead loop 'running' forever)."""
    return _lock_is_live(repo)


# --------------------------------------------------------------------------- #
# start / stop
# --------------------------------------------------------------------------- #
def start(repo, auto_push=True, once=False):
    """Spawn the improver detached if not already running. Returns {ok, pid|error}.

    `auto_push` is the global gate: when False the run ships 'local' (no push/PR) regardless
    of the repo's ship mode. `once` runs a single iteration then exits."""
    rsi = _runtime_dir(repo)
    if not rsi:
        return {"ok": False, "error": "repo has no 'path'"}
    os.makedirs(rsi, exist_ok=True)

    prov = ensure_contracts(repo)          # auto-provision the agent contract before the first loop
    if not prov.get("ok"):
        return {"ok": False, "error": prov["error"]}
    # ensure_contracts may have just auto-set the gate (via set_repo_config -> repos.json) when the
    # operator hadn't set one. The `repo` dict passed in is now STALE — project_gate(repo) would still
    # be '' and the runner would be spawned with the built-in pytest gate, which reds out every
    # iteration on a non-pytest project (e.g. unittest-only) until the operator stop+restarts. Re-read
    # the live config so the freshly-detected gate is used on the very first launch.
    if prov.get("gate_set"):
        for r in load_repos():
            if _repo_name(r) == _repo_name(repo):
                repo = r
                break

    if is_running(repo):
        hb = read_heartbeat(repo) or {}
        return {"ok": True, "pid": hb.get("pid"), "already": True}

    # Only NOW (we are about to spawn a fresh runner) clear any stop sentinel, so a Start that
    # actually starts means "run". Doing this BEFORE the is_running check — as the old code did —
    # silently revoked a pending Stop against a LIVE loop that hadn't yet polled the sentinel
    # (violating the halt-switch invariant "Start must not silently revoke a live stop").
    try:
        os.remove(os.path.join(rsi, "stop"))
    except OSError:
        pass

    py = _runner_python(repo)   # repo .venv python if present, else a discovered system Python host
    runner = os.path.join(HERE, "improver", "run_improver.py")
    if not py:
        return {"ok": False, "error": "no Python host found to run the improver — add a .venv to the "
                "repo or install Python 3.11+ (py launcher or on PATH)"}
    if not os.path.exists(runner):
        return {"ok": False, "error": f"runner not found: {runner}"}

    args = [py, runner, "--repo", _repo_path(repo), "--name", _repo_name(repo),
            "--provider", project_provider(repo), "--model", project_model(repo),
            "--ship", effective_ship(repo, auto_push), "--gate", project_gate(repo) or "",
            "--pr-target-branch", project_pr_target_branch(repo),
            "--reasoning", project_reasoning(repo),
            "--interval", str(project_interval(repo)),
            "--max-iterations", str(project_max_iterations(repo)),
            "--goal", project_goal(repo)]
    if once:
        args.append("--once")
    try:
        proc = subprocess.Popen(
            args,
            cwd=_repo_path(repo),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            stdin=subprocess.DEVNULL,
            close_fds=True,
            env=_clean_subenv(),   # strip stale gh token + PYTHONPATH so the repo venv python is clean
            **hidden_subprocess_kwargs(detached=True),
        )
        return {"ok": True, "pid": proc.pid}
    except OSError as e:
        return {"ok": False, "error": str(e)}


def beautify(repo, auto_push=True):
    """Spawn a one-shot, docs-only "beautify" run detached (README/banner/badges/Mermaid/
    GitHub About). Same launch pattern as start(), plus --beautify --once. Requires a git
    repo (the runner needs a remote/origin to ship a PR). Returns {ok, pid|error}.

    `auto_push` is the global gate (False -> ship 'local', no push/PR). Writes the same
    heartbeat as a normal run, so the workspace view shows its progress."""
    if not repo:
        return {"ok": False, "error": "unknown repo"}
    path = _repo_path(repo)
    if not path or not os.path.isdir(path):
        return {"ok": False, "error": "repo has no valid 'path'"}
    if not repo.get("is_git"):
        return {"ok": False, "error": f"{_repo_name(repo)} needs a git repo (publish it first)"}

    prov = ensure_contracts(repo)          # idempotent; guarantees the contract exists
    if not prov.get("ok"):
        return {"ok": False, "error": prov["error"]}

    py = _venv_python(repo)
    runner = os.path.join(HERE, "improver", "run_improver.py")
    if not py or not os.path.exists(py):
        return {"ok": False, "error": f"venv python not found: {py}"}
    if not os.path.exists(runner):
        return {"ok": False, "error": f"runner not found: {runner}"}

    try:
        proc = subprocess.Popen(
            [py, runner, "--repo", path, "--name", _repo_name(repo),
             "--provider", project_provider(repo), "--model", project_model(repo),
             "--ship", effective_ship(repo, auto_push),
             "--pr-target-branch", project_pr_target_branch(repo),
             "--reasoning", project_reasoning(repo),
             "--beautify", "--once"],
            cwd=path,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            stdin=subprocess.DEVNULL,
            close_fds=True,
            env=_clean_subenv(),
            **hidden_subprocess_kwargs(detached=True),
        )
        return {"ok": True, "pid": proc.pid}
    except OSError as e:
        return {"ok": False, "error": str(e)}


def stop(repo):
    """Write an empty <path>/.rsi/stop sentinel; the runner polls it and exits.

    After writing the sentinel, wait briefly for the loop to confirm stopped (is_running False), then
    auto-run cleanup_worktrees to delete lingering rsi/* branches left by local/push ship modes or dead
    runs (the observed rsi/iter-... leftover on sover). Cleanup is best-effort — a failure is logged but
    does not block the stop (the sentinel was already written). Never force-kills the process."""
    rsi = _runtime_dir(repo)
    if not rsi:
        return {"ok": False, "error": "repo has no 'path'"}
    try:
        os.makedirs(rsi, exist_ok=True)
        with open(os.path.join(rsi, "stop"), "w", encoding="utf-8") as f:
            f.write("")
    except OSError as e:
        return {"ok": False, "error": str(e)}
    # the loop polls the sentinel every second; give it a short grace window to exit cleanly before
    # cleaning branches (so we don't delete a branch a still-live runner is standing on). Best-effort:
    # if the loop is slow to exit, cleanup_worktrees' own guard (never delete the current branch) keeps
    # it safe, and a later stop/Supervise sweep will catch stragglers.
    stopped = False
    try:
        for _ in range(5):
            if not is_running(repo):
                stopped = True
                break
            time.sleep(1)
    except Exception:
        stopped = False
    # Only prune rsi/* branches once the loop has CONFIRMED stopped. A live runner oscillates HEAD
    # within a single iteration (checkout base -> rsi -> base), so a lock-free `git branch -D` here
    # could race it and delete the iteration branch out from under it (lifecycle-4) — the current-branch
    # guard inside cleanup_worktrees is TOCTOU against that. If the loop didn't exit in the grace
    # window, skip cleanup: the Supervise sweep's RUNG-0 recovery prunes stragglers while HOLDING the
    # single-flight lock, which is race-safe.
    if stopped:
        try:
            cleanup_worktrees(repo)   # best-effort; the stop itself already succeeded
        except Exception:
            pass
    return {"ok": True}


# --------------------------------------------------------------------------- #
# GitHub PRs
# --------------------------------------------------------------------------- #
def gh_ready():
    """True if `gh auth status` returns 0."""
    gh = _which_gh()
    if not gh:
        return False
    try:
        return _run([gh, "auth", "status"]).returncode == 0
    except OSError:
        return False


def _rollup_state(rollup):
    """Reduce a gh `statusCheckRollup` list to 'success'|'pending'|'failure'|None (no checks)."""
    if not rollup:
        return None
    bad = pending = False
    for c in rollup:
        st = (c.get("state") or "").upper()            # legacy status contexts
        status = (c.get("status") or "").upper()        # check runs: QUEUED/IN_PROGRESS/COMPLETED
        concl = (c.get("conclusion") or "").upper()      # SUCCESS/FAILURE/…
        if status and status != "COMPLETED":
            pending = True
        if st == "PENDING":
            pending = True
        if st in ("FAILURE", "ERROR") or concl in (
                "FAILURE", "TIMED_OUT", "CANCELLED", "ACTION_REQUIRED", "STARTUP_FAILURE"):
            bad = True
    return "failure" if bad else ("pending" if pending else "success")


def list_prs(repo):
    """Open PRs whose headRefName starts with the repo's branch_prefix. [] on error.
    Each PR carries a reduced `checks` field (success|pending|failure|None) for the UI."""
    gh = _which_gh()
    if not gh:
        return []
    prefix = repo.get("branch_prefix") or ""
    path = _repo_path(repo)
    if not prefix or not path:  # an empty prefix must match NOTHING, never every PR
        return []
    try:
        r = _run(
            [gh, "pr", "list", "--json",
             "number,title,headRefName,url,state,createdAt,statusCheckRollup"],
            cwd=path,
        )
        if r.returncode != 0:
            return []
        prs = json.loads(r.stdout or "[]")
    except (OSError, json.JSONDecodeError):
        return []
    out = []
    for p in prs:
        if not str(p.get("headRefName", "")).startswith(prefix):
            continue
        p["checks"] = _rollup_state(p.pop("statusCheckRollup", None))  # drop the heavy raw field
        out.append(p)
    return out


def merge_pr(repo, number):
    """gh pr merge <number> --squash --delete-branch. Returns {ok, error?}."""
    gh = _which_gh()
    if not gh:
        return {"ok": False, "error": "gh not found"}
    try:
        r = _run([gh, "pr", "merge", str(number), "--squash", "--delete-branch"],
                 cwd=_repo_path(repo))
    except OSError as e:
        return {"ok": False, "error": str(e)}
    if r.returncode == 0:
        return {"ok": True}
    return {"ok": False, "error": (r.stderr or r.stdout or "merge failed").strip()}


def close_pr(repo, number):
    """gh pr close <number> --delete-branch. Returns {ok, error?}."""
    gh = _which_gh()
    if not gh:
        return {"ok": False, "error": "gh not found"}
    try:
        r = _run([gh, "pr", "close", str(number), "--delete-branch"],
                 cwd=_repo_path(repo))
    except OSError as e:
        return {"ok": False, "error": str(e)}
    if r.returncode == 0:
        return {"ok": True}
    return {"ok": False, "error": (r.stderr or r.stdout or "close failed").strip()}


def pr_diff(repo, number, max_bytes=200000):
    """`gh pr diff <number>` for the repo, capped at max_bytes. Returns
    {ok, diff, truncated} or {ok:false, error}."""
    gh = _which_gh()
    if not gh:
        return {"ok": False, "error": "gh not found"}
    try:
        r = _run([gh, "pr", "diff", str(number)], cwd=_repo_path(repo))
    except OSError as e:
        return {"ok": False, "error": str(e)}
    if r.returncode != 0:
        return {"ok": False, "error": (r.stderr or r.stdout or "diff failed").strip()}
    diff = r.stdout or ""
    truncated = len(diff) > max_bytes
    return {"ok": True, "diff": diff[:max_bytes], "truncated": truncated}


def local_rsi_branches(repo):
    """git branch --list "<prefix>*" -> list of branch names. [] on error."""
    git = _which_git()
    if not git:
        return []
    prefix = repo.get("branch_prefix") or ""
    path = _repo_path(repo)
    if not prefix or not path:  # an empty prefix must match NOTHING
        return []
    try:
        r = _run([git, "branch", "--list", f"{prefix}*"], cwd=path)
        if r.returncode != 0:
            return []
    except OSError:
        return []
    out = []
    for line in (r.stdout or "").splitlines():
        name = line.replace("*", "").strip()
        if name:
            out.append(name)
    return out


# --------------------------------------------------------------------------- #
# contracts (per-repo backlog.md / AGENT.md) + metrics + health
# --------------------------------------------------------------------------- #
_CONTRACT_FILES = {"backlog": "backlog.md", "agent": "AGENT.md"}


def _contract_path(repo, which):
    name = _repo_name(repo)
    fn = _CONTRACT_FILES.get(which)
    return os.path.join(HERE, "improver", name, fn) if (name and fn) else None


def read_contract(repo, which):
    """Read improver/<name>/{backlog.md|AGENT.md}. `which` in {'backlog','agent'}.
    {ok, text} (text '' if the file doesn't exist yet) or {ok:false, error}."""
    p = _contract_path(repo, which)
    if not p:
        return {"ok": False, "error": "unknown contract (use 'backlog' or 'agent')"}
    try:
        with open(p, "r", encoding="utf-8") as f:
            return {"ok": True, "text": f.read()}
    except FileNotFoundError:
        return {"ok": True, "text": ""}
    except OSError as e:
        return {"ok": False, "error": str(e)}


def write_contract(repo, which, text):
    """Write improver/<name>/{backlog.md|AGENT.md}, creating the dir. {ok} / {ok:false, error}."""
    p = _contract_path(repo, which)
    if not p:
        return {"ok": False, "error": "unknown contract (use 'backlog' or 'agent')"}
    try:
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "w", encoding="utf-8") as f:
            f.write(text if text is not None else "")
        return {"ok": True}
    except OSError as e:
        return {"ok": False, "error": str(e)}


# --------------------------------------------------------------------------- #
# provisioning — auto-create a project-specific agent contract before the first loop
# --------------------------------------------------------------------------- #
def _detect_stack(repo):
    """Cheap, read-only stdlib probe of the repo: {lang, test_cmd, entrypoints, top_dirs}.
    Never raises."""
    out = {"lang": "unknown", "test_cmd": "", "entrypoints": [], "top_dirs": []}
    path = _repo_path(repo)
    if not path or not os.path.isdir(path):
        return out
    here = lambda f: os.path.exists(os.path.join(path, f))
    isdir = lambda f: os.path.isdir(os.path.join(path, f))
    win = os.name == "nt"
    venv_rel = ".venv/Scripts/python.exe" if win else ".venv/bin/python"
    # The emitted command runs through cmd.exe (shell=True): a forward-slash exe path at the
    # start of the line ("'.venv' is not recognized") fails there, so use backslashes on Windows.
    py = (r".venv\Scripts\python" if win else ".venv/bin/python") if here(venv_rel) else "python"
    pytest_cfg = here("pytest.ini") or here("conftest.py") or here("tests/conftest.py")

    def _py_test_cmd():
        # pytest if the project configures it; else unittest discovery when a tests/ dir exists
        # (many app repos — e.g. sover — ship a tests/ dir + venv but no pytest config or manifest).
        if pytest_cfg or not isdir("tests"):
            return f"{py} -m pytest"
        return f"{py} -m unittest discover -s tests -t tests"

    has_manifest = any(here(f) for f in ("pyproject.toml", "requirements.txt", "requirements-dev.txt",
                                         "setup.py", "setup.cfg"))
    if has_manifest:
        out["lang"], out["test_cmd"] = "python", _py_test_cmd()
    elif here("package.json"):
        out["lang"], out["test_cmd"] = "node", "npm test"
    elif here("Cargo.toml"):
        out["lang"], out["test_cmd"] = "rust", "cargo test"
    elif here("go.mod"):
        out["lang"], out["test_cmd"] = "go", "go test ./..."
    elif here(venv_rel) and isdir("tests"):
        # Python project with a venv + tests but no manifest (e.g. sover): detect it anyway.
        out["lang"], out["test_cmd"] = "python", _py_test_cmd()
    for ep in ("app.py", "main.py", "__main__.py", "index.js", "src/main.py",
               "src/main.js", "src/main.ts"):
        if here(ep):
            out["entrypoints"].append(ep)
    try:
        for e in sorted(os.scandir(path), key=lambda x: x.name):
            if e.is_dir() and not e.name.startswith(".") and len(out["top_dirs"]) < 8:
                out["top_dirs"].append(e.name)
    except OSError:
        pass
    return out


def render_default_contract(repo):
    """Deterministic (no-LLM) AGENT.md + backlog.md tailored to the detected stack. Returns
    (agent_md, backlog_md). The backlog items use the exact `- [ ]` prefix _top_backlog_item parses."""
    name = _repo_name(repo) or "project"
    stack = _detect_stack(repo)
    gate = project_gate(repo) or stack["test_cmd"] or "(set a gate command in Config)"
    has_remote = bool((repo or {}).get("has_remote"))
    goal = project_goal(repo)
    # The operator's north-star GOAL is weighted heavily: it leads the contract (the per-iteration
    # system prompt) so EVERY improvement is chosen to advance it, and the agent may BUILD a missing
    # capability when the goal needs one (still one gated increment at a time).
    goal_block = (f"""## North-star goal (weigh this above all else)

> {goal}

Every iteration must move this goal forward — choose the single improvement with the most leverage
toward it. If achieving it needs a capability the project does not have yet, **build that capability**
(still as one small, tested, shippable increment). The backlog serves the goal; when the backlog and
the goal disagree, the goal wins.

""" if goal else "")
    if stack["entrypoints"]:
        code_map = ("- Entry points: " + ", ".join("`%s`" % e for e in stack["entrypoints"])
                    + ("\n- Detected stack: %s." % stack["lang"])
                    + "\n- Read these first to learn the codebase before changing anything.")
    elif stack["top_dirs"]:
        code_map = ("- Top-level directories: " + ", ".join("`%s`" % d for d in stack["top_dirs"])
                    + "\n- Read these first to learn the codebase before changing anything.")
    else:
        code_map = "- Read the README and the main entry point first to learn the codebase."
    github_para = ("\n- You MAY use the read-only `github_*` tools (`github_status`, "
                   "`github_verify_push`, `github_pr_status`, `github_ci_status`, `github_list_prs`) "
                   "to confirm the GitHub connection and check whether any open `rsi/*` PR is failing "
                   "CI — if a recent one is red, prefer a change that fixes it. These tools only read; "
                   "they never push, merge, or close.") if has_remote else ""
    agent_md = f"""# {name} self-improvement contract

You are the **{name} improver** — an autonomous coding agent running one iteration of a
continuous self-improvement loop on the {name} codebase. Each run, ship **one** small, real,
verified improvement.

{goal_block}## Your job this run (exactly one improvement)

1. **The improvement is named in your task message.** Implement that one item. If it is already
   done or unclear, instead fix one clear bug, missing test, rough edge, or simplification you
   find while reading the code. Either way, do exactly *one* thing.
2. **Implement it** with the smallest coherent change. Match the existing style; no new
   dependencies or frameworks unless truly required; no speculative abstraction. Doing more than
   the one item is a regression.
3. **Add or update a test** that covers the change. Never delete, weaken, `xfail`, or skip an
   existing test to "make it pass."
4. **Verify locally before you finish:** run the gate yourself — `{gate}` — it must be green. If
   your change can't go green, revert your own edits and pick something smaller.
5. **Summarize**: end with 2–4 sentences — what you changed, which file(s), and why. This becomes
   the pull-request description.

## Rules

- **Do NOT run git or `gh` directly, and never push or merge.** The runner owns version control:
  it created your branch, re-runs the gate authoritatively, and — only if green — commits and
  opens a pull request for the operator to review.{github_para}
- **Stay in the product.** Edit the application source and its tests/docs. Do NOT modify
  `.github/`, `.env` / secrets, or build/packaging files unless the task explicitly says so.
- **Keep tests portable.** The gate may run on Linux CI and installs only the repo's declared
  dependencies — tests must not require a GUI, the network, or any package not in the project's
  requirements. Guard OS-specific paths.
- **Keep it shippable.** No half-finished features behind the gate; scope down to a complete,
  tested slice and note the rest in your summary.

## Map of the code

{code_map}

_Auto-generated by Solomon. Refine it, or use “Enrich with AI” to make it project-specific._
"""
    backlog_md = f"""# {name} backlog

Improvements the loop pulls from, top first. Edit freely.

- [ ] add a test for the most-used module / entrypoint
- [ ] tighten error handling on the main entrypoint
- [ ] improve the README quickstart
"""
    return agent_md, backlog_md


def contracts_present(repo):
    """{{agent:bool, backlog:bool}} — each True iff the contract file exists AND is non-empty."""
    out = {}
    for which in ("agent", "backlog"):
        p = _contract_path(repo, which)
        present = False
        if p:
            try:
                with open(p, "r", encoding="utf-8") as f:
                    present = bool(f.read().strip())
            except OSError:
                present = False
        out[which] = present
    return out


def ensure_contracts(repo):
    """Idempotent: guarantee improver/<name>/AGENT.md + backlog.md exist (deterministic template)
    BEFORE the first RSI loop, so the runner never hits a missing-contract crash. Never overwrites
    a present file. Also auto-sets the runner's gate from stack detection when the operator hasn't
    set one — otherwise the loop falls back to the built-in pytest gate, which fails (and reverts
    every iteration) on a non-pytest project like a unittest-only repo. Returns {ok, created:[...],
    gate_set?} or {ok:false, error}."""
    if not _repo_name(repo):
        return {"ok": False, "error": "repo has no name"}
    pres = contracts_present(repo)
    created = []
    if not (pres.get("agent") and pres.get("backlog")):
        agent_md, backlog_md = render_default_contract(repo)
        for which, text, label in (("agent", agent_md, "AGENT.md"),
                                   ("backlog", backlog_md, "backlog.md")):
            if not pres.get(which):
                r = write_contract(repo, which, text)
                if not r.get("ok"):
                    return {"ok": False, "error": r.get("error")}
                created.append(label)
    out = {"ok": True, "created": created}
    if not project_gate(repo):
        det = _detect_stack(repo).get("test_cmd")
        if det and set_repo_config(_repo_name(repo), gate=det).get("ok"):
            out["gate_set"] = det
    return out


def enrich_contract(repo, background=False):
    """Run a one-shot pi provisioner (run_improver.py --provision) to rewrite the contract tailored
    to the repo. Requires the active provider's key; degrades to the existing template on any failure.
    background=True spawns it detached and returns {ok, started} immediately (used on add); otherwise
    it blocks and returns the runner's JSON line {ok, agent_written, backlog_written, summary}."""
    name, path = _repo_name(repo), _repo_path(repo)
    if not name or not path:
        return {"ok": False, "error": "repo has no name/path"}
    prov = project_provider(repo)
    if not keys_status().get(prov):
        return {"ok": False, "error": f"{prov} API key not set (add it in Settings)"}
    py = _runner_python(repo)
    if not py:
        return {"ok": False, "error": f"{name} has no .venv python — create the repo's .venv first"}
    runner = os.path.join(HERE, "improver", "run_improver.py")
    if not os.path.exists(runner):
        return {"ok": False, "error": "runner not found"}
    args = [py, runner, "--repo", path, "--name", name, "--provider", prov,
            "--model", project_model(repo), "--goal", project_goal(repo), "--provision"]
    if background:
        try:
            subprocess.Popen(args, cwd=path, stdout=subprocess.DEVNULL,
                             stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL, close_fds=True,
                             env=_clean_subenv(), **hidden_subprocess_kwargs(detached=True))
            return {"ok": True, "started": True}
        except OSError as e:
            return {"ok": False, "error": str(e)}
    try:
        r = _run(args, cwd=path)
    except OSError as e:
        return {"ok": False, "error": str(e)}
    for line in reversed((r.stdout or "").strip().splitlines()):
        line = line.strip()
        if line.startswith("{"):
            try:
                return json.loads(line)
            except json.JSONDecodeError:
                break
    return {"ok": False, "error": (r.stderr or r.stdout or "provision failed").strip()[:300]}


def ideate(repo):
    """Run a one-shot divergent ideation pass (run_improver.py --ideate): pi proposes ambitious,
    leverage-ranked, tier-tagged improvements and the runner PREPENDS them to the repo's backlog.
    Operator-triggered (not auto-run in the loop) — the more-ambitious items still ship only through
    the gate-enforced PR loop. Blocks and returns the runner's JSON line {ok, added, top}."""
    name, path = _repo_name(repo), _repo_path(repo)
    if not name or not path:
        return {"ok": False, "error": "repo has no name/path"}
    prov = project_provider(repo)
    if not keys_status().get(prov):
        return {"ok": False, "error": f"{prov} API key not set (add it in Settings)"}
    py = _runner_python(repo)
    if not py:
        return {"ok": False, "error": f"{name} has no .venv python — create the repo's .venv first"}
    runner = os.path.join(HERE, "improver", "run_improver.py")
    if not os.path.exists(runner):
        return {"ok": False, "error": "runner not found"}
    args = [py, runner, "--repo", path, "--name", name, "--provider", prov,
            "--model", project_model(repo), "--goal", project_goal(repo), "--ideate"]
    try:
        r = _run(args, cwd=path)
    except OSError as e:
        return {"ok": False, "error": str(e)}
    for line in reversed((r.stdout or "").strip().splitlines()):
        line = line.strip()
        if line.startswith("{"):
            try:
                return json.loads(line)
            except json.JSONDecodeError:
                break
    return {"ok": False, "error": (r.stderr or r.stdout or "ideate failed").strip()[:300]}


def metrics(repo):
    """Aggregate runtime/<name>/history.jsonl into headline counters + a test-pass series."""
    hist = read_history(repo, limit=1000)
    out = {"iterations": len(hist), "shipped": 0, "merged": 0, "reverted": 0,
           "noop": 0, "error": 0, "stopped": 0, "tests_series": []}
    for rec in hist:
        st = rec.get("status")
        if st in ("shipped", "reverted", "noop", "error", "stopped"):
            out[st] += 1
        pr = rec.get("pr")
        if isinstance(pr, dict) and pr.get("state") == "merged":
            out["merged"] += 1
        tests = rec.get("tests")
        if isinstance(tests, dict) and tests.get("passed") is not None:
            out["tests_series"].append({"ts": rec.get("ts"),
                                        "passed": tests.get("passed") or 0,
                                        "failed": tests.get("failed") or 0})
    decided = out["shipped"] + out["reverted"] + out["noop"]
    out["success_rate"] = round(out["shipped"] / decided, 3) if decided else None
    return out


def health():
    """Operator-facing readiness snapshot — reuses gh/keys/git checks + per-repo venv presence."""
    repos = []
    for r in load_repos():
        if not isinstance(r, dict):
            continue
        py = _venv_python(r)
        repos.append({"name": r.get("name"),
                      "is_git": bool(r.get("is_git")),
                      "has_remote": bool(r.get("has_remote")),
                      "venv": bool(py and os.path.exists(py))})
    return {"gh": gh_ready(), "git": bool(_which_git()),
            "keys": keys_status(), "repos": repos}


def cleanup_worktrees(repo):
    """Prune git worktrees and delete leftover local <branch_prefix>* iteration branches
    (rsi/iter-* / rsi/beautify-* left by local/push ship modes or dead runs). Never touches
    the currently checked-out branch. Returns {ok, pruned, removed:[...]} / {ok:false, error}."""
    git = _which_git()
    if not git:
        return {"ok": False, "error": "git not found"}
    path = _repo_path(repo)
    if not path:
        return {"ok": False, "error": "repo has no 'path'"}
    try:
        pruned = _run([git, "-C", path, "worktree", "prune"]).returncode == 0
        cur = (_run([git, "-C", path, "rev-parse", "--abbrev-ref", "HEAD"]).stdout or "").strip()
    except OSError as e:
        return {"ok": False, "error": str(e)}
    removed = []
    for b in local_rsi_branches(repo):
        if b == cur:                       # never delete the branch we're standing on
            continue
        try:
            if _run([git, "-C", path, "branch", "-D", b]).returncode == 0:
                removed.append(b)
        except OSError:
            pass
    return {"ok": True, "pruned": pruned, "removed": removed}


def list_worktrees(repo):
    """Return parsed `git worktree list --porcelain` entries for visualization."""
    git = _which_git()
    path = _repo_path(repo)
    if not git or not path:
        return []
    try:
        result = _run([git, "-C", path, "worktree", "list", "--porcelain"])
    except OSError:
        return []
    if result.returncode != 0:
        return []
    rows, current = [], None
    for line in (result.stdout or "").splitlines() + [""]:
        if not line:
            if current:
                rows.append(current)
                current = None
            continue
        key, _, value = line.partition(" ")
        if key == "worktree":
            current = {"path": value, "branch": None, "head": None, "bare": False,
                       "detached": False, "locked": False, "prunable": False}
        elif current is not None:
            if key == "branch":
                current["branch"] = value.removeprefix("refs/heads/")
            elif key == "HEAD":
                current["head"] = value
            elif key in ("bare", "detached", "locked", "prunable"):
                current[key] = True
    return rows


def branch_hygiene(repo):
    """Read-only snapshot of whether the RSI loop left the repo's git state DIRTY — i.e. off its base
    branch (checked out on an rsi/* iteration branch) and/or with stray rsi/* branches lingering.

    A RUNNING loop sitting on an rsi/* branch mid-iteration is NORMAL, not dirty — so `dirty` is only
    True when no live runner holds the lock. Never raises: any error or non-git repo returns the
    not-dirty shape. Drives the dashboard's auto-surfaced "Clean branch" affordance.

    Returns {dirty, reason, current, base, off_base, stray, uncommitted, running}."""
    clean = {"dirty": False, "reason": "", "current": "", "base": "", "off_base": False,
             "stray": [], "uncommitted": 0, "running": False}
    git = _which_git()
    path = _repo_path(repo)
    if not git or not path:
        return clean
    try:
        base = project_pr_target_branch(repo)
        current = (_run([git, "-C", path, "rev-parse", "--abbrev-ref", "HEAD"]).stdout or "").strip()
        prefix = repo.get("branch_prefix") or "rsi/"
        stray = [b for b in local_rsi_branches(repo) if b != current]
        off_base = bool(current) and bool(base) and current != base and current.startswith(prefix)
        porcelain = _run([git, "-C", path, "status", "--porcelain"]).stdout or ""
        uncommitted = len([ln for ln in porcelain.splitlines() if ln.strip()])
        running = is_running(repo)
    except OSError:
        return clean
    dirty = (off_base or bool(stray)) and not running
    parts = []
    if off_base:
        parts.append(f"on {current} (base {base})")
    if stray:
        parts.append(f"{len(stray)} stray {prefix}* branch(es)")
    return {"dirty": dirty, "reason": "; ".join(parts), "current": current, "base": base,
            "off_base": off_base, "stray": stray, "uncommitted": uncommitted, "running": running}


def clean_branch(repo):
    """Action: clean a DIRTY repo back to its base branch + prune stray rsi/* branches.

    If the repo is OFF its base (checked out on an rsi/* iteration branch), force-checks-out the base
    (discarding that abandoned iteration's WIP — the intent of cleaning). If it is ALREADY on base
    (the only dirtiness is a stray rsi/* branch), it does NOT touch the working tree — so unrelated
    uncommitted work on the base branch is preserved. Then runs cleanup_worktrees to prune worktrees +
    delete the now-not-current rsi/* branches. Refuses while a loop is live (stop it first). Never raises.

    Returns {ok, from, checked_out, switched, removed, pruned} or {ok:false, error}."""
    if is_running(repo):
        return {"ok": False, "error": "loop is running — stop it first"}
    git = _which_git()
    path = _repo_path(repo)
    if not git:
        return {"ok": False, "error": "git not found"}
    if not path:
        return {"ok": False, "error": "repo has no 'path'"}
    base = project_pr_target_branch(repo)
    switched = False
    try:
        prev = (_run([git, "-C", path, "rev-parse", "--abbrev-ref", "HEAD"]).stdout or "").strip()
        # Only switch when actually OFF base. Force discards the abandoned rsi-branch WIP (intended).
        # When ALREADY on base we must NOT force-checkout — that would wipe unrelated uncommitted work
        # on the base branch even though the only dirtiness is a stray rsi/* branch. (Found by
        # dogfooding the cleaner: the unconditional force-checkout was a data-loss footgun.)
        if prev and base and prev != base:
            co = _run([git, "-C", path, "checkout", "--force", base])
            if co.returncode != 0:
                return {"ok": False,
                        "error": (co.stderr or co.stdout or f"checkout {base} failed").strip()[:200]}
            switched = True
    except OSError as e:
        return {"ok": False, "error": str(e)}
    cw = cleanup_worktrees(repo)
    return {"ok": True, "from": prev, "checked_out": base, "switched": switched,
            "removed": cw.get("removed", []), "pruned": cw.get("pruned")}


def browser_state(repo):
    """Read the latest monitored browser observation without raising into the UI."""
    rt = _runtime_dir(repo)
    if not rt:
        return {"ok": False, "error": "repo has no runtime"}
    path = os.path.join(rt, "browser_state.json")
    try:
        with open(path, "r", encoding="utf-8") as f:
            state = json.load(f)
        return state if isinstance(state, dict) else {"ok": False, "error": "invalid browser state"}
    except (OSError, json.JSONDecodeError) as e:
        return {"ok": False, "error": str(e)}


def start_app_test(repo):
    if not repo:
        return {"ok": False, "error": "unknown repo"}
    rt = _runtime_dir(repo)
    if not rt:
        return {"ok": False, "error": "repo has no runtime"}
    from improver.app_test_runtime import MANAGER
    return MANAGER.start(repo, Path(rt))


def stop_app_test(repo):
    if not repo:
        return {"ok": False, "error": "unknown repo"}
    from improver.app_test_runtime import MANAGER
    return MANAGER.stop(_repo_name(repo))


def app_test_state(repo, after_seq=0):
    state = browser_state(repo)
    try:
        seq = int(state.get("seq") or 0)
        after = int(after_seq or 0)
    except (TypeError, ValueError):
        seq, after = 0, 0
    if state.get("ok") and seq > 0 and seq <= after:
        return {"ok": True, "unchanged": True, "seq": seq}
    return state


def app_test_frame(repo, after_seq=0):
    import base64
    state = browser_state(repo)
    try:
        seq = int(state.get("seq") or 0)
    except (TypeError, ValueError):
        seq = 0
    if seq <= int(after_seq or 0):
        return {"ok": True, "unchanged": True, "seq": seq}
    rt = _runtime_dir(repo)
    frame = os.path.join(rt, "browser_frame.jpg") if rt else ""
    try:
        data = base64.b64encode(Path(frame).read_bytes()).decode("ascii")
        return {"ok": True, "seq": seq, "mime": "image/jpeg", "data": data}
    except OSError as e:
        return {"ok": False, "seq": seq, "error": str(e)}


def read_app_test_report(repo):
    rt = _runtime_dir(repo)
    path = os.path.join(rt, "app_test_report.json") if rt else ""
    try:
        with open(path, "r", encoding="utf-8") as f:
            report = json.load(f)
        return report if isinstance(report, dict) else {"ok": False, "error": "invalid report"}
    except (OSError, json.JSONDecodeError) as e:
        return {"ok": False, "error": str(e)}


# --------------------------------------------------------------------------- #
# supervisor primitives — small, reversible recovery actions (used by improver/solomon.py)
# --------------------------------------------------------------------------- #
def clear_lock(repo):
    """Remove a STALE runtime/<name>/lock (no LIVE runner holds it). REFUSES if a live runner holds it.
    Uses the same liveness test as is_running (PID alive + run-id/heartbeat-freshness), so a recycled-PID
    lock — which the old _pid_alive-only guard refused to clear, leaving the loop wedged forever — is now
    clearable. Returns {ok, removed:bool} / {ok:false, error}."""
    rt = _runtime_dir(repo)
    if not rt:
        return {"ok": False, "error": "repo has no 'path'"}
    lock = os.path.join(rt, "lock")
    if not os.path.exists(lock):
        return {"ok": True, "removed": False}   # nothing to clear
    if _lock_is_live(repo, rt):
        pid, _ = _read_lock(rt)
        return {"ok": False, "error": f"lock held by live runner (pid {pid})"}
    try:
        os.remove(lock)
        return {"ok": True, "removed": True}
    except OSError as e:
        return {"ok": False, "error": str(e)}


def acquire_supervisor_lock(repo):
    """Take the repo's single-flight runtime lock for a git-mutating recovery (reset_to_base), so the
    supervisor is mutually exclusive with a runner iteration — the SOLOMON_RSI 'supervisor holds the
    runner's lock during recovery' invariant. Replaces a racy is_running() snapshot (a runner writes its
    lock late in main(), a multi-second window where is_running is False). Returns (ok, token): ok False
    means a LIVE runner holds the lock (caller escalates). Mirrors run_improver.acquire_lock's atomic
    create + recycled/stale takeover. Release with release_supervisor_lock(repo, token)."""
    rt = _runtime_dir(repo)
    if not rt:
        return False, None
    os.makedirs(rt, exist_ok=True)
    lock = os.path.join(rt, "lock")
    token = "sup-" + uuid.uuid4().hex
    content = f"{os.getpid()}\n{token}"
    try:                                            # atomic exclusive create — no lock present at all
        fd = os.open(lock, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
        try:
            os.write(fd, content.encode("ascii"))
        finally:
            os.close(fd)
        return True, token
    except FileExistsError:
        pass
    if _lock_is_live(repo, rt):                     # a live runner holds it — do NOT mutate git under it
        return False, None
    try:                                            # stale/recycled lock — take it over, then verify we won
        tmp = os.path.join(rt, f"lock.sup.{os.getpid()}.tmp")
        with open(tmp, "w", encoding="utf-8") as f:
            f.write(content)
        os.replace(tmp, lock)
    except OSError:
        return False, None
    time.sleep(0.1)
    return (_read_lock(rt)[1] == token), token


def release_supervisor_lock(repo, token):
    """Release a supervisor lock only if WE still hold it (its run-id equals `token`)."""
    rt = _runtime_dir(repo)
    if not rt or not token:
        return
    if _read_lock(rt)[1] == token:
        try:
            os.remove(os.path.join(rt, "lock"))
        except OSError:
            pass


def reset_to_base(repo):
    """Reset the integration/base branch to origin truth — exactly the runner's own preflight —
    GUARDING against discarding un-pushed base commits (refuses + escalates instead). Never deletes
    feature branches (that's cleanup_worktrees). Returns {ok, base} / {ok:false, error}."""
    git = _which_git()
    if not git:
        return {"ok": False, "error": "git not found"}
    path = _repo_path(repo)
    if not path:
        return {"ok": False, "error": "repo has no 'path'"}
    base = project_pr_target_branch(repo)

    def g(*a):
        return _run([git, "-C", path, *a])

    try:
        # never destroy uncommitted operator work: a reset_to_base (checkout --force + reset --hard)
        # silently discards uncommitted TRACKED changes. The only guard used to be for un-pushed
        # COMMITS, leaving uncommitted WIP unprotected — so dirty_tree auto-recovery (and the
        # unattended watchdog that drives it every couple of minutes) could eat an operator's edits.
        # A dead run's own leftovers live on an rsi/* branch and are cleaned by the runner's preflight,
        # so by the time a dirty BASE tree reaches here it is operator work: refuse + escalate.
        dirty = g("status", "--porcelain", "--untracked-files=no")
        if (dirty.stdout or "").strip():
            return {"ok": False,
                    "error": "uncommitted tracked changes on the base tree — escalate "
                             "(won't auto-discard operator work; commit or stash first)"}
        has_origin = g("remote", "get-url", "origin").returncode == 0
        if has_origin:
            # fetch BEFORE the un-pushed check so origin/{base} is current truth — the runner's
            # preflight fetches before counting too; without this a stale local origin ref can make
            # reset_to_base spuriously refuse ("un-pushed") on a base that already matches origin,
            # or miss genuinely un-pushed commits if origin/{base} is stale-ahead.
            g("fetch", "origin", "--quiet")
            ahead = g("log", "--oneline", f"origin/{base}..{base}")
            if ahead.returncode == 0 and (ahead.stdout or "").strip():
                return {"ok": False,
                        "error": f"un-pushed commits on {base} — escalate (won't auto-discard)"}
        co = g("checkout", "--force", base)
        if co.returncode != 0:
            return {"ok": False,
                    "error": (co.stderr or co.stdout or f"checkout {base} failed").strip()[:200]}
        g("reset", "--hard")
        if has_origin:
            g("reset", "--hard", f"origin/{base}")
    except OSError as e:
        return {"ok": False, "error": str(e)}
    return {"ok": True, "base": base}


def read_supervisor_log(repo, limit=50):
    """Last `limit` records from runtime/<name>/supervisor.jsonl (oldest→newest). [] on missing."""
    rt = _runtime_dir(repo)
    if not rt:
        return []
    try:
        with open(os.path.join(rt, "supervisor.jsonl"), "r", encoding="utf-8") as f:
            lines = f.read().splitlines()
    except OSError:
        return []
    out = []
    for line in lines[-limit:]:
        line = line.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(rec, dict):
            out.append(rec)
    return out
