#!/usr/bin/env python3
"""Solomon RSI improver — the continuous self-improvement loop (generic; targets any repo).

Each iteration:
  1. start from a clean ``main``; create branch ``rsi/iter-<ts>``
  2. run ONE Pi session (Kimi K2.7 via Ollama Cloud) against ``rsi/AGENT.md``; it makes a
     single small product improvement + a test (Pi is told NOT to touch git)
  3. **run the gate — pytest — enforced HERE, not trusted to the model**
  4. green + changed -> commit, push, open a PR for the operator to review/merge
     red            -> hard-reset + drop the branch (degradation safety)
     no change      -> drop the branch (no-op)
  5. write ``.rsi/heartbeat.json`` throughout (the Solomon dashboard reads it); sleep; repeat

Safety rails owned by THIS runner (not the model): single-flight lock, clean-tree
precondition, test-green gate, branch-per-iteration with auto-revert, and PR-only — it
never pushes to ``main``. Stop it via the dashboard's Stop button (writes ``.rsi/stop``)
or Ctrl-C.

Usage (generic; the Solomon dashboard supplies --repo/--name):
  python run_improver.py --repo <path> --name maki           # continuous loop
  python run_improver.py --repo <path> --name maki --once    # one iteration then exit
  python run_improver.py --repo <path> --name maki --smoke   # connectivity probe (no repo changes)
"""
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent            # Solomon/improver
CONTROL = HERE.parent                             # Solomon (operator infra, not published)

# Provider map — chosen by --provider; sets the pi extension, pi provider name, default model.
# Keep in sync with control.py _PROVIDER_DEFAULT_MODEL.
PROVIDERS = {
    "ollama-cloud": {"ext": "maki-cloud.ts", "pi_provider": "maki-cloud",
                     "default_model": "kimi-k2.7-code"},
    "openrouter": {"ext": "openrouter.ts", "pi_provider": "openrouter",
                   "default_model": "qwen/qwen3-coder"},
}

PI_EXT = HERE / "maki-cloud.ts"                   # shared pi provider (set by configure)

# Per-run config, set by configure() from --repo/--name. The runner is generic and lives
# OUTSIDE the target repo, so the product itself stays completely RSI-free.
REPO = Path(".")
NAME = "repo"
RUNTIME = CONTROL / "runtime" / NAME
HEARTBEAT = RUNTIME / "heartbeat.json"
LOCK = RUNTIME / "lock"
STOP = RUNTIME / "stop"
LOG = RUNTIME / "improver.log"
AGENT_MD = HERE / NAME / "AGENT.md"
BACKLOG = HERE / NAME / "backlog.md"
VENV_PY = REPO / ".venv" / "Scripts" / ("python.exe" if os.name == "nt" else "python")

PI_PROVIDER = "maki-cloud"
PI_MODEL = "kimi-k2.7-code"

SHIP = "pr"           # local|push|pr|auto-merge — set from --ship
GATE_CMD = ""         # optional custom shell test-command — set from --gate (empty = built-in pytest)
REASONING = ""        # pi --thinking level (off|minimal|low|medium|high|xhigh) — set from --reasoning
GOAL = ""             # operator north-star goal, weighted heavily into every task — set from --goal
BEAUTIFY = False      # one docs-only beautify pass — set from --beautify (skips the gate)
BEAUTIFY_MD = HERE / "beautify.md"   # the beautify system-prompt contract (provider-agnostic)
GITHUB_TOOLS = False                 # expose read-only github_* tools to the agent (remote repos only)
GITHUB_TOOLS_EXT = HERE / "github-tools.ts"  # gh-backed GitHub verification tools (pi extension)
SOLOMON = False                      # supervisor fix-session — set from --solomon (runs the gate + ships a PR)
PROVISION_MD = HERE / "provision.md" # one-shot contract-generation system prompt
SOLOMON_MD = HERE / "solomon.md"     # supervisor fix-session system prompt


def configure(repo: str, name: str, provider: str = "ollama-cloud",
              model: str | None = None) -> None:
    """Point the runner at a target repo with a chosen provider/model. Runtime
    (heartbeat/lock/stop/log) and the per-repo contract live under Solomon —
    never inside the target repo, which stays RSI-free."""
    global REPO, NAME, RUNTIME, HEARTBEAT, LOCK, STOP, LOG, AGENT_MD, BACKLOG, VENV_PY
    global PI_PROVIDER, PI_MODEL, PI_EXT
    REPO = Path(repo).resolve()
    NAME = name
    RUNTIME = CONTROL / "runtime" / name
    HEARTBEAT = RUNTIME / "heartbeat.json"
    LOCK = RUNTIME / "lock"
    STOP = RUNTIME / "stop"
    LOG = RUNTIME / "improver.log"
    AGENT_MD = HERE / name / "AGENT.md"
    BACKLOG = HERE / name / "backlog.md"
    VENV_PY = REPO / ".venv" / "Scripts" / ("python.exe" if os.name == "nt" else "python")
    prov = PROVIDERS.get(provider) or PROVIDERS["ollama-cloud"]
    PI_PROVIDER = prov["pi_provider"]
    PI_MODEL = model or prov["default_model"]
    PI_EXT = HERE / prov["ext"]
    _hb["repo"] = name
    _hb["model"] = PI_MODEL


def build_task(goal: str) -> str:
    """Per-iteration instruction for the Pi coder. The full operating contract is injected
    separately via --append-system-prompt (AGENT_MD); here we name the chosen item and, when set,
    the operator's north-star GOAL so the change is steered toward it (and a missing capability may
    be BUILT to serve it)."""
    north_star = (
        f'NORTH-STAR GOAL (weigh above all): {GOAL}\nChoose the change with the most leverage toward '
        f'that goal; if it needs a capability the project lacks, BUILD that capability as this one '
        f'increment.\n\n' if GOAL else ""
    )
    return (
        f'{north_star}Implement exactly ONE improvement in this repository: "{goal}". Make the smallest '
        "coherent change and add or update a pytest test for it, then run the test suite "
        "(`.venv/Scripts/python -m pytest`) yourself to confirm it is green. Do NOT run git or "
        "gh — the runner commits and opens the pull request. If that item is already done or "
        "unclear, instead fix one clear small bug or cleanup you find. End with a 2-4 sentence "
        "summary of what you changed."
    )
_NO_WINDOW = subprocess.CREATE_NO_WINDOW if sys.platform == "win32" else 0
BASE_BRANCH = "main"  # the repo's integration branch; re-resolved from the launch branch in main()

_hb = {
    "repo": "maki", "status": "starting", "phase": None, "pid": os.getpid(),
    "iteration": 0, "goal": None, "model": PI_MODEL, "tests": None, "last_pr": None,
    "last_summary": None, "started_at": None, "updated_at": None, "log_tail": [],
}


# ---- time / env -----------------------------------------------------------
def _now() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _stamp() -> str:
    return datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")


def _clean_env() -> dict:
    env = dict(os.environ)
    env.pop("PYTHONPATH", None)
    env.pop("PYTHONHOME", None)
    env.pop("GITHUB_TOKEN", None)   # a stale token would shadow the `gh auth login` keyring + break gh/git
    env.pop("GH_TOKEN", None)
    return env


_ENV_KEYS = ("OLLAMA_API_KEY", "OLLAMA_BASE_URL", "OPENROUTER_API_KEY")


def _load_env() -> None:
    """Load provider API keys (+ OLLAMA_BASE_URL) from Solomon/.env into os.environ.
    Dependency-free parser; existing environment values win."""
    for p in (CONTROL / ".env",):
        if not p.exists():
            continue
        try:
            for line in p.read_text(encoding="utf-8").splitlines():
                line = line.strip()
                if not line or line.startswith("#") or "=" not in line:
                    continue
                k, v = line.split("=", 1)
                k, v = k.strip(), v.strip().strip('"').strip("'")
                if k in _ENV_KEYS and not os.environ.get(k):
                    os.environ[k] = v
        except OSError:
            pass


def _required_key() -> str:
    """The env-var name of the API key the active provider needs."""
    return "OPENROUTER_API_KEY" if PI_PROVIDER == "openrouter" else "OLLAMA_API_KEY"


def _which(name: str, *extra: str) -> str:
    p = shutil.which(name)
    if p:
        return p
    for e in extra:
        if Path(e).exists():
            return e
    return name  # last resort; subprocess will surface a clear error


def pi_exe() -> str:
    return _which("pi")


def gh_exe() -> str:
    return _which("gh", r"C:\Program Files\GitHub CLI\gh.exe")


# ---- logging / heartbeat --------------------------------------------------
def log(msg: str) -> None:
    line = f"{_now()} {msg}"
    print(line, flush=True)
    RUNTIME.mkdir(parents=True, exist_ok=True)
    try:
        with open(LOG, "a", encoding="utf-8") as f:
            f.write(line + "\n")
    except OSError:
        pass
    _hb["log_tail"] = (_hb.get("log_tail") or [])[-19:] + [line]


def heartbeat(**fields) -> None:
    _hb.update(fields)
    _hb["updated_at"] = _now()
    RUNTIME.mkdir(parents=True, exist_ok=True)
    tmp = HEARTBEAT.with_suffix(".tmp")
    try:
        tmp.write_text(json.dumps(_hb, indent=2), encoding="utf-8")
        os.replace(tmp, HEARTBEAT)
    except OSError:
        pass


def _record_history(status: str, branch: str | None, summary: str) -> None:
    """Append one terminal-outcome line to runtime/<name>/history.jsonl (the dashboard's
    timeline + metrics read it). Best-effort; never raises."""
    rec = {"ts": _now(), "iteration": _hb.get("iteration"), "status": status,
           "branch": branch, "tests": _hb.get("tests"), "pr": _hb.get("last_pr"),
           "summary": (summary or "")[:500]}
    try:
        RUNTIME.mkdir(parents=True, exist_ok=True)
        with open(RUNTIME / "history.jsonl", "a", encoding="utf-8") as f:
            f.write(json.dumps(rec) + "\n")
    except OSError:
        pass


# ---- git ------------------------------------------------------------------
def git(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run(["git", *args], cwd=REPO, capture_output=True, text=True,
                          env=_clean_env(), creationflags=_NO_WINDOW)


def has_remote() -> bool:
    return git("remote", "get-url", "origin").returncode == 0


def _branch_on_remote(branch: str) -> bool:
    """True if `branch` is visible on origin — confirms a push actually landed."""
    r = git("ls-remote", "--heads", "origin", branch)
    return r.returncode == 0 and ("refs/heads/" + branch) in (r.stdout or "")


def _github_ready() -> tuple:
    """(ok, reason): gh authenticated AND origin reachable. Precondition before iterating a
    GitHub-based repo, so the loop never burns iterations it can't ship."""
    if not _gh_ready():
        return False, "gh not authenticated (run `gh auth login`)"
    if git("ls-remote", "--heads", "origin").returncode != 0:
        return False, "origin remote not reachable"
    return True, ""


def tree_dirty() -> bool:
    return bool(git("status", "--porcelain").stdout.strip())


def head_sha() -> str:
    return git("rev-parse", "HEAD").stdout.strip()


def _abort_branch(branch: str) -> bool:
    """Revert the working tree and delete ``branch``, fail-closed. Returns True only when we
    are verifiably back on BASE_BRANCH with the branch removed; logs + returns False otherwise
    so the caller surfaces an error instead of letting the loop branch off un-reverted code."""
    git("reset", "--hard")
    co = git("checkout", "--force", BASE_BRANCH)
    if co.returncode != 0:
        log(f"CRITICAL: could not return to {BASE_BRANCH}: {(co.stderr or '').strip()[:200]}")
        return False
    if git("rev-parse", "--abbrev-ref", "HEAD").stdout.strip() != BASE_BRANCH:
        log(f"CRITICAL: not on {BASE_BRANCH} after checkout; refusing to delete {branch}")
        return False
    d = git("branch", "-D", branch)
    if d.returncode != 0:
        log(f"branch -D {branch} failed: {(d.stderr or '').strip()[:200]}")
    return True


def _drop_branch(branch: str, phase: str, summary: str, status: str = "sleeping") -> None:
    """Abort a branch and write the matching heartbeat — escalating to status=error if the
    revert could not complete, so the loop never silently keeps branching off poisoned state."""
    if _abort_branch(branch):
        heartbeat(status=status, phase=phase, last_summary=summary)
        _record_history(phase, branch, summary)
    else:
        heartbeat(status="error", phase="reverted",
                  last_summary=f"REVERT FAILED — {branch} needs manual cleanup before the loop "
                               f"can safely continue. {summary}")
        _record_history("error", branch, summary)


# ---- pi -------------------------------------------------------------------
def _kill_tree(pid: int) -> None:
    """Force-kill a process and ALL its children (pi spawns a node child)."""
    if sys.platform == "win32":
        subprocess.run(["taskkill", "/F", "/T", "/PID", str(pid)],
                       capture_output=True, creationflags=_NO_WINDOW)
    else:
        import os as _os
        import signal as _sig
        try:
            _os.killpg(_os.getpgid(pid), _sig.SIGKILL)
        except OSError:
            pass


def run_pi(task: str, timeout: int = 1800, system_md: Path | None = None) -> subprocess.CompletedProcess:
    args = [pi_exe(), "--print", "--mode", "json",
            "--provider", PI_PROVIDER, "--model", PI_MODEL]
    if REASONING:
        args += ["--thinking", REASONING]
    args += ["-e", str(PI_EXT)]
    if GITHUB_TOOLS:
        args += ["-e", str(GITHUB_TOOLS_EXT)]   # read-only github_* tools for remote repos
    args += ["--append-system-prompt", str(system_md or AGENT_MD), task]
    env = _clean_env()
    env["RSI_MODEL"] = PI_MODEL  # the extension registers exactly this model id
    env["RSI_REASONING"] = REASONING  # extension flips model.reasoning on when a level is set
    # Popen (not subprocess.run): subprocess.run's timeout only kills the direct child, and
    # pi's node grandchild holding the stdout pipe makes the read block forever — that froze a
    # run for 5h. We force-kill the whole tree on timeout, then re-raise so the caller reverts.
    flags = _NO_WINDOW | (subprocess.CREATE_NEW_PROCESS_GROUP if sys.platform == "win32" else 0)
    # encoding="utf-8": pi emits UTF-8 (em-dashes, smart quotes). Without this, text=True
    # decodes with the platform default (cp1252 on Windows) and mangles non-ASCII into mojibake
    # — which then gets written verbatim into the provisioned AGENT.md/backlog.md.
    proc = subprocess.Popen(args, cwd=REPO, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            text=True, encoding="utf-8", errors="replace", env=env,
                            creationflags=flags)
    try:
        out, err = proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        _kill_tree(proc.pid)
        try:
            out, err = proc.communicate(timeout=20)
        except subprocess.TimeoutExpired:
            out, err = "", ""
        raise subprocess.TimeoutExpired(args, timeout, output=out, stderr=err)
    return subprocess.CompletedProcess(args, proc.returncode, out, err)


def final_text(stdout: str) -> str:
    """Extract the last assistant text from a ``pi --mode json`` event stream."""
    final = ""
    for line in (stdout or "").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if ev.get("type") == "agent_end":
            msgs = ev.get("messages") or []
        elif isinstance(ev.get("message"), dict):
            msgs = [ev["message"]]
        else:
            continue
        for m in msgs:
            if isinstance(m, dict) and m.get("role") == "assistant":
                t = "".join(part.get("text", "") for part in (m.get("content") or [])
                            if isinstance(part, dict) and part.get("type") == "text")
                if t:
                    final = t
    return final.strip()


# ---- gate -----------------------------------------------------------------
def run_gate() -> tuple:
    """Authoritative test gate. Returns (green, {passed,failed,errors,green}, tail).

    If a custom GATE_CMD was supplied (--gate), run THAT via the shell in REPO;
    green = returncode 0 (pytest-style N passed/failed parsed when present, else
    passed/failed=0). Otherwise run the built-in pytest gate."""
    if GATE_CMD:
        p = subprocess.run(GATE_CMD, shell=True, cwd=REPO, capture_output=True,
                           text=True, env=_clean_env(), creationflags=_NO_WINDOW)
    else:
        py = str(VENV_PY) if VENV_PY.exists() else sys.executable
        # `-o addopts=` clears any repo ini addopts (e.g. a stray `-q`, which combined with
        # our own would become `-qq` and SUPPRESS the "N passed" summary line we parse below).
        p = subprocess.run([py, "-m", "pytest", "-o", "addopts="], cwd=REPO, capture_output=True,
                           text=True, env=_clean_env(), creationflags=_NO_WINDOW)
    out = (p.stdout or "") + (p.stderr or "")

    def _n(pat):
        m = re.search(pat, out)
        return int(m.group(1)) if m else 0

    passed, failed, errors = _n(r"(\d+) passed"), _n(r"(\d+) failed"), _n(r"(\d+) error")
    if not passed and not failed and not errors:
        # unittest doesn't print pytest-style "N passed"; parse its own summary so unittest gates
        # (e.g. `python -m unittest discover`) report real counts in the heartbeat + PR body.
        ran = re.search(r"Ran (\d+) tests?", out)
        if ran:
            failed, errors = _n(r"failures=(\d+)"), _n(r"errors=(\d+)")
            passed = max(int(ran.group(1)) - failed - errors, 0)

    tests = {"passed": passed, "failed": failed, "errors": errors, "green": p.returncode == 0}
    return p.returncode == 0, tests, out[-1500:]


# ---- pr -------------------------------------------------------------------
def _top_backlog_item() -> str:
    try:
        for line in BACKLOG.read_text(encoding="utf-8").splitlines():
            s = line.strip()
            if s.startswith("- [ ]"):
                return s[5:].strip()
    except OSError:
        pass
    return "model-chosen improvement"


def _mark_backlog_done(goal: str) -> None:
    """After a successful ship, tick the backlog item we just implemented (`- [ ]` -> `- [x]`) so a
    continuous loop advances to the NEXT item instead of re-shipping the same one each iteration
    (every iteration bases off the integration branch, which doesn't yet have the in-flight PRs)."""
    if not goal:
        return
    try:
        lines = BACKLOG.read_text(encoding="utf-8").splitlines()
    except OSError:
        return
    for i, ln in enumerate(lines):
        s = ln.strip()
        if s.startswith("- [ ]") and s[5:].strip() == goal.strip():
            lines[i] = ln.replace("- [ ]", "- [x]", 1)
            try:
                BACKLOG.write_text("\n".join(lines) + "\n", encoding="utf-8")
            except OSError:
                pass
            return


_noop_counts: dict = {}   # per-goal consecutive-noop tally, for this loop process's lifetime


def _note_noop(goal: str, limit: int = 3) -> None:
    """Track consecutive no-change iterations on a backlog goal. After `limit` of them — the agent
    can't implement this item right now — defer it to the bottom of the backlog so the loop ADVANCES
    instead of spinning forever on a too-hard item (the supervisor's gate_red_streak only catches
    reverts/errors, not noops)."""
    if not goal or BEAUTIFY or SOLOMON or goal.lower() == "model-chosen improvement":
        return
    _noop_counts[goal] = _noop_counts.get(goal, 0) + 1
    if _noop_counts[goal] >= limit:
        if _defer_backlog_item(goal):
            log(f"item noop'd {limit}x — deferred to bottom of backlog: {goal[:60]}")
        _noop_counts[goal] = 0


def _defer_backlog_item(goal: str) -> bool:
    """Move a stuck `- [ ]` item to the BOTTOM of the backlog (with a note) so `_top_backlog_item`
    returns the next item. Returns True if it moved one."""
    if not goal:
        return False
    try:
        lines = BACKLOG.read_text(encoding="utf-8").splitlines()
    except OSError:
        return False
    for i, ln in enumerate(lines):
        s = ln.strip()
        if s.startswith("- [ ]") and s[5:].strip() == goal.strip():
            item = lines.pop(i).rstrip()
            if "(deferred" not in item:
                item += "  (deferred: agent could not implement after repeated tries)"
            lines.append(item)
            try:
                BACKLOG.write_text("\n".join(lines) + "\n", encoding="utf-8")
                return True
            except OSError:
                return False
    return False


def _pr_title(goal: str, summary: str = "") -> str:
    """Concise PR/commit title from the backlog goal; fall back to the summary's first line
    when the goal is the generic placeholder (so the title isn't a truncated paragraph)."""
    g = (goal or "").strip()
    if g and g.lower() != "model-chosen improvement":
        return g[:72]
    first = next((ln.strip() for ln in (summary or "").splitlines() if ln.strip()), "improvement")
    return first[:72]


def _gh_ready() -> bool:
    p = subprocess.run([gh_exe(), "auth", "status"], capture_output=True, text=True,
                       env=_clean_env(), creationflags=_NO_WINDOW)
    return p.returncode == 0


def _pr_checks(number) -> str | None:
    """Reduce a PR's statusCheckRollup to 'success'|'pending'|'failure'|None (no checks) via gh."""
    if not number:
        return None
    p = subprocess.run([gh_exe(), "pr", "view", str(number), "--json", "statusCheckRollup"],
                       cwd=REPO, capture_output=True, text=True, env=_clean_env(), creationflags=_NO_WINDOW)
    if p.returncode != 0:
        return None
    try:
        rollup = (json.loads(p.stdout or "{}") or {}).get("statusCheckRollup") or []
    except json.JSONDecodeError:
        return None
    if not rollup:
        return None
    bad = pend = False
    for c in rollup:
        st = (c.get("state") or "").upper()
        status = (c.get("status") or "").upper()
        concl = (c.get("conclusion") or "").upper()
        if (status and status != "COMPLETED") or st == "PENDING":
            pend = True
        if st in ("FAILURE", "ERROR") or concl in (
                "FAILURE", "TIMED_OUT", "CANCELLED", "ACTION_REQUIRED", "STARTUP_FAILURE"):
            bad = True
    return "failure" if bad else ("pending" if pend else "success")


def _open_pr(branch: str, title: str, summary: str, tests: dict | None) -> dict:
    if BEAUTIFY:
        body = (f"Repo beautification (docs/presentation only — no code changes).\n\n{summary}\n\n"
                f"_Opened by the Solomon beautify pass on branch `{branch}` — review and "
                f"merge or close._")
        pr_title = f"docs: {title}"
    else:
        gate = (f"**Gate:** {tests['passed']} passed / {tests['failed']} failed on branch "
                f"`{branch}`.\n\n") if tests else ""
        body = (f"Autonomous improvement (RSI loop).\n\n{summary}\n\n"
                f"{gate}_Opened by the Solomon RSI loop — review and merge or close._")
        pr_title = f"rsi: {title}"
    p = subprocess.run([gh_exe(), "pr", "create", "--base", BASE_BRANCH, "--head", branch,
                        "--title", pr_title, "--body", body],
                       cwd=REPO, capture_output=True, text=True, env=_clean_env(), creationflags=_NO_WINDOW)
    if p.returncode != 0:
        log(f"gh pr create failed: {(p.stderr or '').strip()[:200]}")
        return {"number": None, "url": None, "branch": branch, "state": "push-only"}
    url = (p.stdout or "").strip().splitlines()[-1] if p.stdout.strip() else None
    num = None
    if url and "/pull/" in url:
        try:
            num = int(url.rsplit("/", 1)[1])
        except ValueError:
            pass
    return {"number": num, "url": url, "branch": branch, "state": "open"}


def _auto_merge(pr: dict) -> dict:
    """Squash-merge an open PR — but NEVER on CI-red (pr-only-shipping-with-auto-revert applies to CI
    too). checks: 'failure' -> leave open, do not merge; 'pending' -> queue GitHub native auto-merge
    (merges only after required checks pass); 'success'/None(no CI) -> the gate(s) we have are green,
    merge now. On success state='merged'; otherwise the PR is left open + logged."""
    num = pr.get("number")
    if not num:
        return pr
    checks = pr.get("checks")
    if checks == "failure":
        log(f"auto-merge: CI FAILING on PR {num} — leaving open, NOT merging")
        return {**pr, "state": "open (CI red — not merged)"}
    if checks == "pending":
        am = subprocess.run([gh_exe(), "pr", "merge", str(num), "--auto", "--squash", "--delete-branch"],
                            cwd=REPO, capture_output=True, text=True, env=_clean_env(), creationflags=_NO_WINDOW)
        if am.returncode == 0:
            return {**pr, "state": "auto-merge queued (awaiting CI)"}
        log(f"auto-merge: CI pending and native --auto unavailable on PR {num} — leaving open until CI resolves")
        return {**pr, "state": "open (awaiting CI)"}
    m = subprocess.run([gh_exe(), "pr", "merge", str(num), "--squash", "--delete-branch"],
                       cwd=REPO, capture_output=True, text=True, env=_clean_env(), creationflags=_NO_WINDOW)
    if m.returncode == 0:
        return {**pr, "state": "merged"}
    log(f"gh pr merge {num} failed: {(m.stderr or '').strip()[:200]} — PR left open")
    return pr


# ---- iteration ------------------------------------------------------------
def one_iteration() -> None:
    _hb["iteration"] += 1
    n = _hb["iteration"]
    branch = (f"rsi/beautify-{_stamp()}" if BEAUTIFY
              else f"rsi/solomon-{_stamp()}" if SOLOMON
              else f"rsi/iter-{_stamp()}")

    if tree_dirty():
        heartbeat(status="error", phase="preflight",
                  last_summary="Working tree is dirty — commit or stash your changes; "
                               "the loop resumes once it's clean.")
        log("SKIP iteration: working tree dirty")
        return

    # Robust preflight: a previous run that died mid-iteration can leave the repo on a stray
    # rsi/* branch with the base diverged from origin. FORCE back to the base branch and
    # hard-sync it to origin so every iteration starts from a clean, current base (the runner
    # never keeps local-only commits on the base — origin is the source of truth).
    co = git("checkout", "--force", BASE_BRANCH)
    if co.returncode != 0:
        heartbeat(status="error", phase="preflight",
                  last_summary=f"Could not checkout {BASE_BRANCH}: {(co.stderr or '').strip()[:200]}")
        log(f"checkout {BASE_BRANCH} failed — skipping iteration")
        return
    git("reset", "--hard")  # drop any leftover working-tree changes from a dead run
    if has_remote():
        git("fetch", "origin", "--quiet")
        # never-hand-patched keystone (enforced, not prose): REFUSE to adopt a base that moved
        # without a gated iteration. Committed-but-un-pushed commits on BASE_BRANCH — an operator
        # hand-patch, or a dead run's local commit — must NOT be silently hard-reset away. Surface
        # them and skip; the loop changes a repo only through gated PRs.
        ahead = git("rev-list", "--count", f"origin/{BASE_BRANCH}..{BASE_BRANCH}")
        n_ahead = int((ahead.stdout or "0").strip() or "0") if ahead.returncode == 0 else 0
        if n_ahead > 0:
            shas = git("log", f"origin/{BASE_BRANCH}..{BASE_BRANCH}", "--oneline").stdout.strip()
            heartbeat(status="error", phase="preflight",
                      last_summary=f"{BASE_BRANCH} has {n_ahead} commit(s) not on origin "
                                   f"(out-of-band / un-pushed base change). Refusing to hard-reset — "
                                   f"push or revert them; managed repos change only via gated PRs. "
                                   f"Commits: {shas[:300]}")
            log(f"REFUSE preflight reset: {n_ahead} un-pushed base commit(s) on {BASE_BRANCH}")
            return
        rs = git("reset", "--hard", f"origin/{BASE_BRANCH}")
        if rs.returncode != 0:
            log(f"reset to origin/{BASE_BRANCH} failed: {(rs.stderr or '').strip()[:160]} — using local {BASE_BRANCH}")
    if git("checkout", "-B", branch).returncode != 0:
        heartbeat(status="error", phase="preflight", last_summary=f"Could not create branch {branch}")
        return
    base = head_sha()

    # Anti-gaming baseline (gate-enforced-by-runner): measure the gate on the CLEAN base before Pi
    # touches anything, so a post-change "green" that actually dropped the pass count — deleted,
    # weakened, or skipped tests — is caught and reverted. Skipped for the docs-only/supervisor modes.
    base_tests = None
    if not BEAUTIFY and not SOLOMON:
        bgreen, base_tests, _ = run_gate()
        if not bgreen:
            heartbeat(status="error", phase="preflight",
                      last_summary=f"Base gate is RED before any change ({base_tests}). Fix the gate "
                                   f"command or the base; the loop can't measure a gain from a red base.")
            log(f"base gate RED — skipping iteration {n}")
            git("checkout", "--force", BASE_BRANCH)
            git("branch", "-D", branch)
            return

    if SOLOMON:
        goal = "supervise: diagnose and fix the persistent gate failure"
        task = _solomon_task()
        system_md = SOLOMON_MD
    elif BEAUTIFY:
        goal = "Beautify this repository (README/banner/badges/Mermaid/About, docs only)"
        task = ("Beautify this repository per beautify.md — rewrite the README to modern OSS "
                "standards (centered banner, shields.io badges, a Mermaid architecture diagram, "
                "full sections), create assets/banner.svg, set the GitHub About (description + "
                "topics), and add LICENSE/CONTRIBUTING only if missing. Documentation and "
                "presentation only — do NOT change any source code or behavior. Then stop.")
        system_md = BEAUTIFY_MD
    else:
        goal = _top_backlog_item()
        task = build_task(goal)
        system_md = None
    heartbeat(status="iterating", phase="implement", goal=goal,
              last_pr=None, tests=None)
    log(f"iteration {n}: branch {branch} — Pi ({PI_MODEL}) working"
        + (" (beautify, docs-only)" if BEAUTIFY else ""))
    try:
        p = run_pi(task, system_md=system_md, timeout=900 if BEAUTIFY else 1800)
    except subprocess.TimeoutExpired:
        log("Pi session timed out")
        _drop_branch(branch, "noop", "Pi session timed out.")
        return
    summary = final_text(p.stdout) or "(no summary returned)"
    log(f"Pi rc={p.returncode}: {summary[:200]}")

    if not tree_dirty() and head_sha() == base:
        log("Pi made no changes — dropping branch")
        _note_noop(goal)          # defer this item if the agent keeps failing to implement it
        _drop_branch(branch, "noop", summary)
        return

    if BEAUTIFY:
        # docs-only pass — there is nothing to test; do NOT run the pytest/custom gate.
        log("beautify: gate skipped (docs-only)")
        tests = None
    else:
        heartbeat(phase="test", last_summary=summary)
        green, tests, tail = run_gate()
        heartbeat(tests=tests)
        log(f"gate: {'GREEN' if green else 'RED'} {tests}")
        if not green:
            log(f"gate tail: {tail[-400:]}")
            _drop_branch(branch, "reverted",
                         f"Reverted — tests failed ({tests['failed']} failed). {summary}")
            return
        # Anti-gaming: a GREEN gate that DROPPED the pass count means tests were removed/weakened/
        # skipped — the gate was gamed. Revert even though it is "green".
        if base_tests and tests["passed"] < base_tests["passed"]:
            log(f"anti-gaming: pass count fell {base_tests['passed']} -> {tests['passed']} — reverting")
            _drop_branch(branch, "reverted",
                         f"Reverted — anti-gaming: pass count fell {base_tests['passed']}→"
                         f"{tests['passed']} (tests removed/weakened/skipped). {summary}")
            return
        # ...and catch newly-introduced skip/xfail markers (weakening that needn't drop the count).
        diff = git("diff", BASE_BRANCH).stdout or ""
        new_skips = [ln for ln in diff.splitlines() if ln.startswith("+")
                     and re.search(r"@\s*(pytest\.mark\.(skip|xfail)|unittest\.skip)", ln)]
        if new_skips:
            log(f"anti-gaming: {len(new_skips)} new skip/xfail marker(s) — reverting")
            _drop_branch(branch, "reverted",
                         f"Reverted — anti-gaming: introduced {len(new_skips)} skip/xfail "
                         f"marker(s). {summary}")
            return

    # commit anything Pi left uncommitted (it shouldn't commit, but be robust)
    heartbeat(phase="commit")
    git("add", "-A")
    if git("diff", "--cached", "--quiet").returncode != 0:
        title = "beautify repo" if BEAUTIFY else _pr_title(goal, summary)
        prefix = "docs" if BEAUTIFY else "rsi"
        git("commit", "-m", f"{prefix}: {title}\n\n{summary}")
    rl = git("rev-list", "--count", f"{BASE_BRANCH}..HEAD")
    if rl.returncode != 0:
        log(f"rev-list failed: {(rl.stderr or '').strip()[:160]} — keeping {branch} for inspection")
        git("checkout", BASE_BRANCH)
        heartbeat(status="error", phase="commit",
                  last_summary=f"Gate passed but commit count couldn't be verified; {branch} kept. {summary}")
        return
    if rl.stdout.strip() == "0":
        log("no commits ahead after gate — dropping branch")
        _drop_branch(branch, "noop", summary)
        return

    # honor an operator Stop that arrived during the (possibly long) iteration: keep the
    # gate-green work on its local branch, but do NOT open a PR after a stop was requested.
    if STOP.exists():
        log("stop requested during iteration — committed locally, skipping PR")
        git("checkout", BASE_BRANCH)
        heartbeat(status="stopped", phase="sleep",
                  last_pr={"number": None, "url": None, "branch": branch,
                           "state": "local (stopped before PR)"},
                  last_summary=summary)
        _record_history("stopped", branch, summary)
        return

    title = "beautify repo" if BEAUTIFY else _pr_title(goal, summary)
    pr = _ship(branch, title, summary, tests)
    git("checkout", BASE_BRANCH)
    # Advance the backlog ONLY on a real ship — a PR was opened/merged, or the work was deliberately
    # kept local (ship=local) — never on a push/auth FAILURE (else the item is lost without landing).
    if not BEAUTIFY and not SOLOMON and _ship_succeeded(pr):
        _mark_backlog_done(goal)
    heartbeat(status="sleeping", phase="sleep", last_pr=pr, last_summary=summary)
    _record_history("shipped", branch, summary)


def _ship_succeeded(pr: dict) -> bool:
    """A terminal, non-failed ship: a PR was opened (has a number) OR the branch was deliberately
    kept local (ship=local). NOT a push/auth failure (those didn't land and must be retried)."""
    if pr.get("number"):
        return True
    state = (pr.get("state") or "").lower()
    return "local" in state and "fail" not in state and "pending" not in state


def _ship(branch: str, title: str, summary: str, tests: dict) -> dict:
    """Branch on SHIP to ship the gate-green committed branch.

    local      -> keep the local branch; no push/PR.
    push       -> push the branch; no PR.
    pr         -> push + open PR (default).
    auto-merge -> push + open PR, then squash-merge + delete branch.

    For push/pr/auto-merge without a remote, gracefully degrade to local."""
    if SHIP in ("push", "pr", "auto-merge") and not has_remote():
        log(f"ship={SHIP} but no remote — kept local")
        return {"number": None, "url": None, "branch": branch,
                "state": "local (no remote)"}

    if SHIP == "local":
        log(f"ship=local — kept committed branch {branch} locally (unshipped)")
        return {"number": None, "url": None, "branch": branch,
                "state": "local branch (unshipped)"}

    # push / pr / auto-merge all need gh + a successful push first
    if not _gh_ready():
        log("gh not ready — branch committed locally; ship pending `gh auth login`")
        return {"number": None, "url": None, "branch": branch,
                "state": "local (ship pending gh auth)"}
    heartbeat(phase="ship")
    push = git("push", "-u", "origin", branch)
    if push.returncode != 0:
        log(f"git push failed: {(push.stderr or '').strip()[:200]}")
        return {"number": None, "url": None, "branch": branch, "state": "push-failed", "verified": False}

    # verify the push actually landed on origin — not just a zero return code
    verified = _branch_on_remote(branch)
    if not verified:
        log(f"WARNING: push reported success but '{branch}' is not visible on origin")

    if SHIP == "push":
        log(f"ship=push — pushed {branch} (no PR){'' if verified else ' [UNVERIFIED]'}")
        return {"number": None, "url": None, "branch": branch,
                "state": "pushed (no PR)" if verified else "pushed (unverified)", "verified": verified}

    heartbeat(phase="pr")
    pr = _open_pr(branch, title, summary, tests)
    pr["verified"] = verified
    pr["checks"] = _pr_checks(pr.get("number"))
    log(f"opened PR: {pr.get('url')} (push {'verified' if verified else 'UNVERIFIED'}, CI {pr.get('checks') or 'none'})")
    if SHIP == "auto-merge":
        heartbeat(phase="merge")
        pr = _auto_merge(pr)
        log(f"ship=auto-merge — {pr.get('state')}: {pr.get('url')}")
    return pr


# ---- lock -----------------------------------------------------------------
def _pid_alive(pid: int) -> bool:
    if sys.platform == "win32":
        out = subprocess.run(["tasklist", "/FI", f"PID eq {pid}", "/NH", "/FO", "CSV"],
                             capture_output=True, text=True, creationflags=_NO_WINDOW).stdout
        return f'"{pid}"' in (out or "")  # CSV quotes the PID field — exact, no substring FP
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


def acquire_lock() -> bool:
    RUNTIME.mkdir(parents=True, exist_ok=True)
    try:  # atomic create — only one racer can ever win the lock (closes the TOCTOU window)
        with open(LOCK, "x", encoding="utf-8") as f:
            f.write(str(os.getpid()))
        return True
    except FileExistsError:
        pass
    try:
        pid = int((LOCK.read_text(encoding="utf-8") or "0").strip() or "0")
    except (ValueError, OSError):
        pid = 0
    if pid and pid != os.getpid() and _pid_alive(pid):
        return False
    # lock is stale (its recorded pid is dead) — take it over atomically
    try:
        tmp = RUNTIME / "lock.tmp"
        tmp.write_text(str(os.getpid()), encoding="utf-8")
        os.replace(tmp, LOCK)
        return True
    except OSError:
        return False


def release_lock() -> None:
    try:  # only unlink if the lock is still ours (don't delete one another process took)
        if int((LOCK.read_text(encoding="utf-8") or "0").strip() or "0") == os.getpid():
            LOCK.unlink()
    except (OSError, ValueError):
        try:
            LOCK.unlink()
        except OSError:
            pass


# ---- smoke ----------------------------------------------------------------
def smoke() -> int:
    _load_env()
    key = _required_key()
    if not os.environ.get(key):
        print(f"SMOKE: FAIL — {key} not set (put it in Solomon/.env)")
        return 1
    args = [pi_exe(), "--print", "--mode", "json", "--provider", PI_PROVIDER,
            "--model", PI_MODEL, "-e", str(PI_EXT), "--no-tools",
            "--system-prompt", "Connectivity smoke test. Output exactly the single word READY.",
            "READY?"]
    env = _clean_env()
    env["RSI_MODEL"] = PI_MODEL  # the extension registers exactly this model id
    try:
        p = subprocess.run(args, cwd=REPO, capture_output=True, text=True,
                           env=env, timeout=120, creationflags=_NO_WINDOW)
    except subprocess.TimeoutExpired:
        print("SMOKE: FAIL — timed out")
        return 1
    t = final_text(p.stdout)
    if "READY" in t.upper():
        print("SMOKE: PASS")
        return 0
    print(f"SMOKE: FAIL — {t or (p.stderr or '').strip()[:300]}")
    return 1


# ---- provision (one-shot contract generation) -----------------------------
def _parse_provision(text: str):
    """Pull the two ===AGENT.md=== / ===backlog.md=== blocks out of the provisioner's output."""
    if "===AGENT.md===" not in text or "===backlog.md===" not in text:
        return None, None
    after = text.split("===AGENT.md===", 1)[1]
    agent_part, backlog_part = after.split("===backlog.md===", 1)
    agent_md = agent_part.strip().strip("`").strip()
    backlog_md = backlog_part.split("===")[0].strip().strip("`").strip()
    return (agent_md or None), (backlog_md or None)


def provision() -> int:
    """One-shot: pi reads the repo and emits the two contract blocks; the runner writes them
    (Python owns the filesystem — pi never writes contract files). Prints one JSON line. No gate,
    no git, no loop."""
    goal_line = (f"\n\nThe operator's NORTH-STAR GOAL for this project (weigh it heavily — the "
                 f"contract must lead with it and the backlog must be ordered to advance it, "
                 f"including building any capability the goal needs that the project lacks):\n"
                 f"{GOAL}\n") if GOAL else ""
    task = ("Read this repository and generate its Solomon improver contract and backlog per "
            "provision.md. Output ONLY the two fenced blocks." + goal_line)
    try:
        p = run_pi(task, system_md=PROVISION_MD, timeout=600)
    except subprocess.TimeoutExpired:
        print(json.dumps({"ok": False, "error": "provisioner timed out"}))
        return 5
    agent_md, backlog_md = _parse_provision(final_text(p.stdout) or "")
    if not agent_md or not backlog_md:
        print(json.dumps({"ok": False, "error": "provisioner did not emit both blocks"}))
        return 5
    try:
        AGENT_MD.parent.mkdir(parents=True, exist_ok=True)
        AGENT_MD.write_text(agent_md, encoding="utf-8")
        BACKLOG.write_text(backlog_md, encoding="utf-8")
    except OSError as e:
        print(json.dumps({"ok": False, "error": str(e)}))
        return 5
    first = next((ln.strip() for ln in agent_md.splitlines() if ln.strip()), "")
    print(json.dumps({"ok": True, "agent_written": len(agent_md),
                      "backlog_written": len(backlog_md), "summary": first[:80]}))
    return 0


def _solomon_task() -> str:
    """Build the supervisor fix-session prompt from the recent (failing) iteration history."""
    hist = []
    try:
        for line in (RUNTIME / "history.jsonl").read_text(encoding="utf-8").splitlines()[-6:]:
            line = line.strip()
            if line:
                try:
                    hist.append(json.loads(line))
                except json.JSONDecodeError:
                    pass
    except OSError:
        pass
    outcomes = "; ".join(
        f"{h.get('status')} ({(h.get('tests') or {}).get('failed', '?')} failed): {(h.get('summary') or '')[:80]}"
        for h in hist) or "no recorded history"
    return ("You are Solomon, supervising this repository's RSI loop, which keeps FAILING its test "
            f"gate. Recent outcomes: {outcomes}. Read the failing test(s) and the code they guard, "
            "find the ROOT CAUSE (a flaky or incorrect test, an unmet dependency, or a wrong "
            "instruction in the agent contract), and make the SMALLEST fix — to the one offending "
            "test or the code it covers — so the gate goes green. Do NOT run git or gh. If the cause "
            "is genuinely ambiguous, write a 2-4 sentence diagnosis and make no code change.")


# ---- main -----------------------------------------------------------------
def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description="Generic Pi RSI improver loop (targets a repo by path)")
    ap.add_argument("--repo", required=True, help="path to the target git repository")
    ap.add_argument("--name", default=None, help="repo name for runtime/contract namespacing")
    ap.add_argument("--provider", default="ollama-cloud", choices=list(PROVIDERS),
                    help="pi provider (ollama-cloud or openrouter)")
    ap.add_argument("--model", default=None, help="model id (defaults to the provider's default)")
    ap.add_argument("--ship", default="pr", choices=["local", "push", "pr", "auto-merge"],
                    help="how to ship a gate-green branch (default: pr)")
    ap.add_argument("--gate", default="", help="custom shell test-command (default: built-in pytest)")
    ap.add_argument("--pr-target-branch", dest="pr_target_branch", default="",
                    help="branch PRs target / the loop integrates from (default: current HEAD or main)")
    ap.add_argument("--max-iterations", dest="max_iterations", type=int, default=0,
                    help="stop after N iterations (0 = unlimited)")
    ap.add_argument("--reasoning", default="",
                    choices=["", "off", "minimal", "low", "medium", "high", "xhigh"],
                    help="agent thinking level passed to pi --thinking (default: model default)")
    ap.add_argument("--goal", default="",
                    help="operator north-star goal, weighted heavily into every task + the provisioner")
    ap.add_argument("--once", action="store_true", help="run one iteration then exit")
    ap.add_argument("--interval", type=int, default=120, help="seconds between iterations")
    ap.add_argument("--smoke", action="store_true", help="connectivity probe; no repo changes")
    ap.add_argument("--beautify", action="store_true",
                    help="one docs-only pass (README/banner/badges/Mermaid/About); skips the gate")
    ap.add_argument("--provision", action="store_true",
                    help="one-shot: generate this repo's AGENT.md + backlog.md, then exit (no loop)")
    ap.add_argument("--solomon", action="store_true",
                    help="supervisor fix-session: diagnose + fix a persistent gate failure (one iteration)")
    a = ap.parse_args(argv)
    configure(a.repo, a.name or Path(a.repo).name, a.provider, a.model)
    global SHIP, GATE_CMD, REASONING, GOAL, BEAUTIFY, SOLOMON
    SHIP = a.ship
    GATE_CMD = a.gate or ""
    REASONING = a.reasoning or ""
    GOAL = (a.goal or "").strip()
    BEAUTIFY = a.beautify
    SOLOMON = a.solomon
    if BEAUTIFY or SOLOMON:
        a.once = True  # beautify + the supervisor fix-session are single-shot

    if a.smoke:
        return smoke()

    _load_env()
    if a.provision:                        # generate the contract files, then exit (no git/loop needed)
        return provision()
    if git("rev-parse", "--is-inside-work-tree").returncode != 0:
        print("ERROR: not a git repository.")
        return 2
    global BASE_BRANCH
    BASE_BRANCH = (a.pr_target_branch or "").strip() or \
        (git("rev-parse", "--abbrev-ref", "HEAD").stdout.strip() or "main")
    # Ensure the base branch actually exists before the loop. Otherwise every iteration's preflight
    # `git checkout BASE_BRANCH` fails and the loop spins forever with zero progress. If it's missing
    # locally but present on a (freshly-wired) origin, create it tracking origin/<base>.
    if git("rev-parse", "--verify", "--quiet", BASE_BRANCH).returncode != 0:
        created = False
        if has_remote():
            git("fetch", "origin", "--quiet")
            if git("rev-parse", "--verify", "--quiet", f"origin/{BASE_BRANCH}").returncode == 0:
                created = git("checkout", "-B", BASE_BRANCH, f"origin/{BASE_BRANCH}").returncode == 0
        if not created:
            heartbeat(status="error",
                      last_summary=f"Base branch '{BASE_BRANCH}' does not exist locally or on origin — "
                                   f"set this repo's PR-target branch to a real branch in Config.")
            print(f"ERROR: base branch '{BASE_BRANCH}' not found (local or origin).")
            return 2
    key = _required_key()
    if not os.environ.get(key):
        heartbeat(status="error", last_summary=f"{key} not set — add it to Solomon/.env")
        print(f"ERROR: {key} not set (Solomon/.env or environment).")
        return 2
    # GitHub-based repo: expose the read-only github_* tools to the agent, and verify the
    # connection works BEFORE iterating when we intend to push — never burn iterations we can't ship.
    global GITHUB_TOOLS
    GITHUB_TOOLS = has_remote()
    if GITHUB_TOOLS and SHIP in ("push", "pr", "auto-merge"):
        ok, why = _github_ready()
        if not ok:
            heartbeat(status="error",
                      last_summary=f"GitHub not ready — {why}. Connect GitHub before Solomon iterates this remote repo.")
            print(f"ERROR: GitHub not ready — {why}")
            return 4
        log("GitHub connection verified — github_* tools enabled for the agent")
    if not acquire_lock():
        print(f"Another improver is already running for {NAME} (runtime lock held).")
        return 3
    if STOP.exists():
        STOP.unlink()

    _hb["started_at"] = _now()
    heartbeat(status="idle", phase=None)
    log(f"Solomon RSI improver started for {NAME}")
    try:
        while True:
            if STOP.exists():
                log("stop flag set — exiting")
                break
            one_iteration()
            if a.once:
                break
            if a.max_iterations and _hb["iteration"] >= a.max_iterations:
                log(f"reached max iterations ({a.max_iterations}) — exiting")
                break
            for _ in range(max(1, a.interval)):  # interruptible cooldown
                if STOP.exists():
                    break
                time.sleep(1)
    except KeyboardInterrupt:
        log("interrupted")
    finally:
        heartbeat(status="stopped", phase=None)
        release_lock()
        try:
            STOP.unlink()
        except OSError:
            pass
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
