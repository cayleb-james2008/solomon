"""Solomon — backend logic for the cross-repo auto-iterator dashboard.

Pure-ish, unit-testable functions over a registry of repos (`repos.json`).
Each repo runs a pi-powered "improver" loop that writes a heartbeat and a PID
lock under `<repo>/.rsi/`. This module reads those, controls start/stop, and
drives GitHub PR accept/deny via the `gh` CLI.

Heartbeat schema (written by the runner — read-only contract, do NOT change):
    {repo, status, phase, pid, iteration, goal, model,
     tests:{passed,failed,errors,green}|null,
     last_pr:{number,url,branch,state}|null,
     last_summary, started_at, updated_at, log_tail:[...]}
"""
import json
import os
import shutil
import subprocess
import sys

def _base_dir():
    """The operator data dir (repos.json, improver/, runtime/, .env). When frozen, the
    PyInstaller bundle does NOT contain improver/ — the operator data lives in the real
    Solomon folder — so walk up from the exe to find it (marked by improver/), falling
    back to the exe's own dir. Unfrozen: the directory of this source file."""
    if getattr(sys, "frozen", False):
        d = os.path.dirname(os.path.abspath(sys.executable))
        probe = d
        for _ in range(6):
            if os.path.isdir(os.path.join(probe, "improver")):
                return probe
            parent = os.path.dirname(probe)
            if parent == probe:
                break
            probe = parent
        return d
    return os.path.dirname(os.path.abspath(__file__))


HERE = _base_dir()
REPOS_JSON = os.path.join(HERE, "repos.json")
# Sibling projects folder (…/workspace/projects). Drop a git repo here and it auto-registers.
PROJECTS_DIR = os.path.join(os.path.dirname(HERE), "projects")

_GH_FALLBACK = r"C:\Program Files\GitHub CLI\gh.exe"
_NO_WINDOW = 0x08000000  # subprocess.CREATE_NO_WINDOW (win32)

# provider defaults — keep in sync with improver/run_improver.py PROVIDERS
_PROVIDER_DEFAULT_MODEL = {
    "ollama-cloud": "kimi-k2.7-code",
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
    """Agent thinking/reasoning level (pi --thinking): off|minimal|low|medium|high|xhigh,
    or '' (unset -> the model/pi default)."""
    return (repo or {}).get("reasoning") or ""


def set_repo_config(name, provider=None, model=None, ship=None, gate=None,
                    pr_target_branch=None, interval=None, max_iterations=None,
                    reasoning=None):
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
    try:
        with open(REPOS_JSON, "w", encoding="utf-8") as f:
            json.dump(entries, f, indent=2)
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


def _clean_subenv():
    """os.environ minus GITHUB_TOKEN/GH_TOKEN. Solomon authenticates gh via `gh auth login` (keyring);
    a stale token env var would shadow it and make gh exit non-zero — which would block the
    GitHub-ready gate, blank the PR list, and break pushes. Stripping it forces the keyring path."""
    env = dict(os.environ)
    env.pop("GITHUB_TOKEN", None)
    env.pop("GH_TOKEN", None)
    return env


def _run(args, cwd=None):
    """Run a subprocess, capturing output. CREATE_NO_WINDOW on win32; broken gh token stripped."""
    kw = {"capture_output": True, "text": True, "cwd": cwd, "env": _clean_subenv()}
    if sys.platform == "win32":
        kw["creationflags"] = _NO_WINDOW
    return subprocess.run(args, **kw)


def _which_gh():
    return shutil.which("gh") or (_GH_FALLBACK if os.path.exists(_GH_FALLBACK) else None)


def _which_git():
    return shutil.which("git")


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
        r = _run(["tasklist", "/FI", f"PID eq {int(pid)}"])
        return str(pid) in (r.stdout or "")
    try:
        os.kill(int(pid), 0)
        return True
    except (OSError, ValueError):
        return False


def is_running(repo):
    """True if the PID in <path>/.rsi/lock is a live process."""
    rsi = _runtime_dir(repo)
    if not rsi:
        return False
    try:
        with open(os.path.join(rsi, "lock"), "r", encoding="utf-8") as f:
            pid = int((f.read() or "").strip())
    except (OSError, ValueError):
        return False
    return _pid_alive(pid)


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
    # Clear any stop sentinel FIRST (idempotent) so a Start ALWAYS means "do not stop" —
    # even if a Stop is still pending against a loop that's mid-iteration.
    os.makedirs(rsi, exist_ok=True)
    try:
        os.remove(os.path.join(rsi, "stop"))
    except OSError:
        pass

    prov = ensure_contracts(repo)          # auto-provision the agent contract before the first loop
    if not prov.get("ok"):
        return {"ok": False, "error": prov["error"]}

    if is_running(repo):
        hb = read_heartbeat(repo) or {}
        return {"ok": True, "pid": hb.get("pid"), "already": True}

    py = _venv_python(repo)
    runner = os.path.join(HERE, "improver", "run_improver.py")
    if not os.path.exists(py):
        return {"ok": False, "error": f"venv python not found: {py}"}
    if not os.path.exists(runner):
        return {"ok": False, "error": f"runner not found: {runner}"}

    flags = 0
    if sys.platform == "win32":
        flags = subprocess.CREATE_NO_WINDOW | subprocess.DETACHED_PROCESS
    args = [py, runner, "--repo", _repo_path(repo), "--name", _repo_name(repo),
            "--provider", project_provider(repo), "--model", project_model(repo),
            "--ship", effective_ship(repo, auto_push), "--gate", project_gate(repo) or "",
            "--pr-target-branch", project_pr_target_branch(repo),
            "--reasoning", project_reasoning(repo),
            "--interval", str(project_interval(repo)),
            "--max-iterations", str(project_max_iterations(repo))]
    if once:
        args.append("--once")
    try:
        proc = subprocess.Popen(
            args,
            cwd=_repo_path(repo),
            creationflags=flags,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            stdin=subprocess.DEVNULL,
            close_fds=True,
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

    flags = 0
    if sys.platform == "win32":
        flags = subprocess.CREATE_NO_WINDOW | subprocess.DETACHED_PROCESS
    try:
        proc = subprocess.Popen(
            [py, runner, "--repo", path, "--name", _repo_name(repo),
             "--provider", project_provider(repo), "--model", project_model(repo),
             "--ship", effective_ship(repo, auto_push),
             "--pr-target-branch", project_pr_target_branch(repo),
             "--reasoning", project_reasoning(repo),
             "--beautify", "--once"],
            cwd=path,
            creationflags=flags,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            stdin=subprocess.DEVNULL,
            close_fds=True,
        )
        return {"ok": True, "pid": proc.pid}
    except OSError as e:
        return {"ok": False, "error": str(e)}


def stop(repo):
    """Write an empty <path>/.rsi/stop sentinel; the runner polls it and exits."""
    rsi = _runtime_dir(repo)
    if not rsi:
        return {"ok": False, "error": "repo has no 'path'"}
    try:
        os.makedirs(rsi, exist_ok=True)
        with open(os.path.join(rsi, "stop"), "w", encoding="utf-8") as f:
            f.write("")
        return {"ok": True}
    except OSError as e:
        return {"ok": False, "error": str(e)}


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

## Your job this run (exactly one improvement)

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
    py = _venv_python(repo)
    if not py or not os.path.exists(py):
        py = sys.executable
    runner = os.path.join(HERE, "improver", "run_improver.py")
    if not os.path.exists(runner):
        return {"ok": False, "error": "runner not found"}
    args = [py, runner, "--repo", path, "--name", name, "--provider", prov,
            "--model", project_model(repo), "--provision"]
    if background:
        flags = 0
        if sys.platform == "win32":
            flags = subprocess.CREATE_NO_WINDOW | subprocess.DETACHED_PROCESS
        try:
            subprocess.Popen(args, cwd=path, creationflags=flags, stdout=subprocess.DEVNULL,
                             stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL, close_fds=True)
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


# --------------------------------------------------------------------------- #
# supervisor primitives — small, reversible recovery actions (used by improver/solomon.py)
# --------------------------------------------------------------------------- #
def clear_lock(repo):
    """Remove a STALE runtime/<name>/lock (its PID is dead). REFUSES if the PID is live.
    Returns {ok, removed:bool} / {ok:false, error}."""
    rt = _runtime_dir(repo)
    if not rt:
        return {"ok": False, "error": "repo has no 'path'"}
    lock = os.path.join(rt, "lock")
    try:
        with open(lock, "r", encoding="utf-8") as f:
            pid = int((f.read() or "0").strip() or "0")
    except (OSError, ValueError):
        return {"ok": True, "removed": False}   # no/invalid lock — nothing to clear
    if pid and _pid_alive(pid):
        return {"ok": False, "error": f"lock held by live pid {pid}"}
    try:
        os.remove(lock)
        return {"ok": True, "removed": True}
    except OSError as e:
        return {"ok": False, "error": str(e)}


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
        has_origin = g("remote", "get-url", "origin").returncode == 0
        if has_origin:
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
            g("fetch", "origin", "--quiet")
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
