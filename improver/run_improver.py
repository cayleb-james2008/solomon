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
import uuid
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent            # Solomon/improver
CONTROL = HERE.parent                             # Solomon (operator infra, not published)

# Provider map — chosen by --provider; sets the pi extension, pi provider name, default model.
# Keep in sync with control.py _PROVIDER_DEFAULT_MODEL.
PROVIDERS = {
    "ollama-cloud": {"ext": "maki-cloud.ts", "pi_provider": "maki-cloud",
                     "default_model": "glm-5.2"},
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
PI_MODEL = "glm-5.2"

# Per-process identity token written into the lock file's 2nd line + the heartbeat, so a recycled OS
# PID can't masquerade as this live runner (Windows reuses PIDs aggressively). See acquire_lock /
# control._lock_is_live. INTERVAL (set from --interval in main) is the cooldown AND the lock-staleness base.
RUN_ID = uuid.uuid4().hex
INTERVAL = 120

SHIP = "pr"           # local|push|pr|auto-merge — set from --ship
GATE_CMD = ""         # optional custom shell test-command — set from --gate (empty = built-in pytest)
GATE_TIMEOUT = 900    # seconds before a hung gate is force-failed (so it can't freeze the loop)
REASONING = ""        # pi --thinking level (off|minimal|low|medium|high|xhigh) — set from --reasoning
GOAL = ""             # operator north-star goal, weighted heavily into every task — set from --goal
BEAUTIFY = False      # one docs-only beautify pass — set from --beautify (skips the gate)
BEAUTIFY_MD = HERE / "beautify.md"   # the beautify system-prompt contract (provider-agnostic)
GITHUB_TOOLS = False                 # expose read-only github_* tools to the agent (remote repos only)
GITHUB_TOOLS_EXT = HERE / "github-tools.ts"  # gh-backed GitHub verification tools (pi extension)
SOLOMON = False                      # supervisor fix-session — set from --solomon (runs the gate + ships a PR)
PROVISION_MD = HERE / "provision.md" # one-shot contract-generation system prompt
IDEATE_MD = HERE / "ideate.md"       # divergent ideation system prompt (anti-shallowness lane)
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


def _refresh_config_from_registry() -> None:
    """Re-read THIS repo's row from repos.json at the top of each iteration so an operator edit to
    model/gate/reasoning/goal (via the dashboard) takes effect WITHOUT a stop+restart. Config was
    captured once into globals at launch and never re-read, so a live loop silently steered by stale
    values — e.g. asmodeus kept running kimi after the operator switched it to glm-5.2 in the registry.
    SHIP is deliberately NOT refreshed here: the launcher passes control.effective_ship(), which already
    applied the global auto_push gate, so re-reading the raw 'ship' field would bypass it. BASE_BRANCH /
    interval / max_iterations are launch-owned too. Best-effort: any read/parse error keeps the current
    config (writes to repos.json are atomic, so a torn read is transient)."""
    global PI_PROVIDER, PI_MODEL, PI_EXT, GATE_CMD, REASONING, GOAL
    try:
        rows = json.loads((CONTROL / "repos.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):       # ValueError covers json.JSONDecodeError
        return
    row = (next((r for r in rows if isinstance(r, dict) and r.get("name") == NAME), None)
           if isinstance(rows, list) else None)
    if not isinstance(row, dict):
        return
    prov = PROVIDERS.get(row.get("provider") or "ollama-cloud") or PROVIDERS["ollama-cloud"]
    PI_PROVIDER = prov["pi_provider"]
    PI_EXT = HERE / prov["ext"]
    PI_MODEL = row.get("model") or prov["default_model"]
    GATE_CMD = (row.get("gate") or "").strip()
    REASONING = row.get("reasoning") or "xhigh"   # max reasoning by default
    GOAL = (row.get("goal") or "").strip()
    _hb["model"] = PI_MODEL


def build_task(goal: str, tier: str = "chore") -> str:
    """Per-iteration instruction for the Pi coder. The full operating contract is injected
    separately via --append-system-prompt (AGENT_MD); here we name the chosen item, its ambition
    TIER (sizing the change to the opportunity), and the operator's north-star GOAL."""
    north_star = (
        f'NORTH-STAR GOAL (weigh above all): {GOAL}\nChoose the change with the most leverage toward '
        f'that goal; if it needs a capability the project lacks, BUILD that capability as this one '
        f'increment.\n\n' if GOAL else ""
    )
    if tier == "chore":
        sizing = ("Make the SMALLEST coherent change and add or update a pytest test for it; doing more "
                  "than this one item is a regression.")
    else:
        sizing = (f"This is a {tier.upper()}-tier item — SIZE THE CHANGE TO THE OPPORTUNITY: a "
                  "substantive, possibly multi-file change is expected and welcome; be ambitious and "
                  "creative toward the goal, not minimal. It must still be ONE coherent, shippable "
                  "improvement that passes the gate, with tests covering it. If it's genuinely too big "
                  "for one iteration, implement the largest coherent first slice that's shippable now "
                  "and note the rest in your summary.")
    return (
        f'{north_star}Implement exactly ONE improvement in this repository: "{goal}". {sizing} Then run '
        "the test suite (`.venv/Scripts/python -m pytest`) yourself to confirm it is green. Do NOT run "
        "git or gh — the runner commits and opens the pull request. If that item is already done or "
        "unclear, instead fix one clear small bug or cleanup you find. End with a 2-4 sentence summary "
        "of what you changed, then a FINAL line that is exactly `ITEM-STATUS: done` if you implemented "
        "(or it was already fully done) the named item above, or `ITEM-STATUS: deviated` if you instead "
        "changed something else."
    )


def _split_item_status(summary: str):
    """Pull the trailing `ITEM-STATUS: done|deviated` marker off the agent summary. Returns
    (clean_summary, deviated: bool). Used to tick the backlog item ONLY when the agent actually
    implemented it — not when it deviated to some other change (which would silently skip the item)."""
    deviated = False
    lines = (summary or "").splitlines()
    kept = []
    for ln in lines:
        m = re.match(r"\s*ITEM-STATUS:\s*(done|deviated|skipped)\b", ln, re.I)
        if m:
            deviated = m.group(1).lower() != "done"
            continue   # strip the marker line from the PR/commit body
        kept.append(ln)
    return ("\n".join(kept).strip() or summary), deviated


# Files a backlog item explicitly mandates EDITING — a backticked code/config path that DIRECTLY follows
# an edit verb (edit/change/rewrite/modify/replace/create) within a few chars. This deliberately EXCLUDES
# files named only as inputs/context/examples ("using `data/x.json`", "see `README.md`", a "(a/b/c)"
# candidate list, a generated artifact) — the dominant item shape, which must NOT trigger a false
# deviation. Bounded quantifiers (no catastrophic backtracking). Used to catch an agent that reports
# ITEM-STATUS: done while shipping UNRELATED work (touching none of the files it was told to edit).
_EDIT_MANDATE_RE = re.compile(
    r"\b(?:edit|edits|editing|change|changes|changed|rewrite|rewrites|rewriting|modif\w+|"
    r"replace|replaces|recreate|create|creates)\b"
    r"[^.\n`]{0,15}?`([^`]{1,80}?\.(?:py|ts|tsx|js|jsx|json|toml|md|ya?ml|cfg|ini|txt|html|css|svg|rs|go|sh))`",
    re.I)


def _norm_path(p: str) -> str:
    return re.sub(r"^[./\\]+", "", (p or "").strip().replace("\\", "/")).lower()


def _edit_mandated_files(text: str) -> set:
    """Normalized relative paths the item explicitly mandates EDITING (a backticked code/config file just
    after an edit verb). Empty when the item gives no explicit edit mandate — then the agent's
    self-reported ITEM-STATUS stands, exactly as before (no false positives on data-driven items)."""
    return {_norm_path(m.group(1)) for m in _EDIT_MANDATE_RE.finditer(text or "")}


def _deviated_from_named_files(goal: str, changed_files: str) -> bool:
    """True when the item explicitly mandates editing >=1 file but the committed diff touched NONE of them
    (matched by path SUFFIX so a named `scripts/scheduler.py` isn't satisfied by a decoy `docs/scheduler.py`)
    — the agent shipped something other than the named edit, whatever its ITEM-STATUS said. Conservative:
    fires ONLY on an explicit edit mandate, never on files named as inputs/context, so honest data-driven
    iterations are never mis-flagged."""
    named = _edit_mandated_files(goal)
    if not named:
        return False
    touched = [_norm_path(ln) for ln in (changed_files or "").splitlines() if ln.strip()]
    for n in named:
        if any(t == n or t.endswith("/" + n) for t in touched):
            return False                         # touched at least one mandated file -> not a deviation
    return True                                  # mandated files exist but the diff touched none of them


_NO_WINDOW = subprocess.CREATE_NO_WINDOW if sys.platform == "win32" else 0
BASE_BRANCH = "main"  # the repo's integration branch; re-resolved from the launch branch in main()

_hb = {
    "repo": "maki", "status": "starting", "phase": None, "pid": os.getpid(), "run_id": RUN_ID,
    "iteration": 0, "goal": None, "model": PI_MODEL, "tests": None, "last_pr": None,
    "last_summary": None, "started_at": None, "updated_at": None, "log_tail": [],
}

# Set True when an iteration could not revert its branch (a known-bad tree): the loop HALTS rather
# than letting the next preflight bulldoze it, and keeps its status=error so the supervisor escalates
# (revert_failed) and the watchdog does not blindly restart it.
_HALTED = False


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


# Secret-shaped patterns scrubbed from agent free-text before it reaches a commit, PR body,
# history.jsonl, or the log. The pi agent runs with the provider key in its env and has read access
# to the target repo, so a model that echoes a secret (its own key, a committed .env, a failing test's
# token) must not leak it into a pushed PR or onto disk. Best-effort defense-in-depth, not a guarantee.
_SECRET_TOKEN_PATTERNS = [
    re.compile(r"\bgh[pousr]_[A-Za-z0-9]{20,}\b"),          # GitHub PAT/OAuth/server/refresh tokens
    re.compile(r"\bgithub_pat_[A-Za-z0-9_]{20,}\b"),        # fine-grained PAT
    re.compile(r"\bsk-[A-Za-z0-9_-]{20,}\b"),               # OpenAI/Anthropic-style keys
    re.compile(r"(?i)\bBearer\s+[A-Za-z0-9._\-]{20,}"),     # Authorization: Bearer <token>
]
# NAME<sep>value where NAME looks like a credential and value is secret-length. The separator is
# captured (group 2) and preserved so legitimate text isn't punctuation-rewritten.
_SECRET_KEYVAL_PATTERN = re.compile(
    r"(?i)\b([A-Za-z0-9_]*(?:API_?KEY|ACCESS_TOKEN|AUTH_TOKEN|SECRET|PASSWORD|TOKEN))\b(\s*[=:]\s*)"
    r"([A-Za-z0-9_\-\.]{8,})")


def _redact_keyval(m) -> str:
    """Redact the value of a NAME<sep>value match ONLY when it's a real credential assignment — the
    value looks token-like (contains a digit) OR the NAME is an UPPERCASE env-var-style identifier.
    Otherwise it's prose (e.g. 'token: validation logic') — return it untouched."""
    name, sep, value = m.group(1), m.group(2), m.group(3)
    looks_secret = any(c.isdigit() for c in value) or (name.isupper() and "_" in name)
    return f"{name}{sep}[REDACTED]" if looks_secret else m.group(0)


def _redact(text: str) -> str:
    """Scrub secret-shaped strings, keeping any credential's NAME + separator but replacing its value.
    Returns the input unchanged when there is nothing to redact; None/empty pass through."""
    if not text:
        return text
    out = text
    for pat in _SECRET_TOKEN_PATTERNS:
        out = pat.sub("[REDACTED]", out)
    out = _SECRET_KEYVAL_PATTERN.sub(_redact_keyval, out)
    # Exact-value pass: the agent runs with the active provider key in its env and could echo it BARE
    # (no NAME= prefix). Ollama keys aren't sk-/gh-shaped, so the shape patterns above miss them — scrub
    # the literal loaded value so the agent's OWN key can't leak into a commit/PR/log regardless of shape.
    for _k in ("OLLAMA_API_KEY", "OPENROUTER_API_KEY"):
        _v = os.environ.get(_k)
        if _v and len(_v) >= 8:
            out = out.replace(_v, "[REDACTED]")
    return out


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
    """True if the operator has uncommitted changes to TRACKED files — work the loop must not clobber.
    Untracked files are NOT counted here (a separate _untracked_non_ignored_files() guard protects them
    from the preflight clean — see one_iteration)."""
    return bool(git("status", "--porcelain", "--untracked-files=no").stdout.strip())


def _untracked_non_ignored_files() -> list:
    """Non-ignored UNTRACKED files in the working tree (git '??' entries). These are either operator
    scratch files (a new test, a not-yet-added module) or a dead run's leftovers — indistinguishable
    after a checkout, and ALL of them are potential operator work the loop must not silently delete.
    Pure (delegates to git()) so the guard rule is unit-testable without a real repo."""
    out = []
    for line in (git("status", "--porcelain", "--untracked-files=normal").stdout or "").splitlines():
        if line.startswith("?? "):
            out.append(line[3:].strip())
    return out


def _dirty_blocks_iteration(dirty: bool, cur_branch: str, base_branch: str) -> bool:
    """Whether a dirty tree must SKIP the iteration. ONLY a dirty BASE branch is protected operator
    work the loop must never clobber; a dirty ``rsi/*`` (or any non-base / detached) branch is a
    previous run's mid-iteration leftover (a killed/crashed runner) that the forced preflight reset
    clears — it must NOT wedge the loop forever (which it did: a dead run left the tree dirty on an
    rsi/* branch and every subsequent start skipped on `tree_dirty()`). Pure so the rule is
    unit-tested without a real repo."""
    return dirty and cur_branch == base_branch


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
        global _HALTED
        _HALTED = True                     # halt the loop — don't bulldoze a known-bad tree next preflight
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


def _write_shim(path: Path, content: str) -> None:
    try:
        path.write_text(content, encoding="utf-8")
        os.chmod(path, 0o755)
    except OSError:
        pass


def _agent_shim_dir() -> "Path | None":
    """Create (idempotently) a dir of PATH shims that block the AGENT from the version-control
    operations that escape the runner's branch-per-iteration sandbox — DESPITE the AGENT.md 'do NOT run
    git or gh' rule that a capable model (glm-5.2) ignores: ANY `gh` (it must use the read-only
    github_* tools, never the gh CLI), and `git push|pull|merge|rebase` (which push a branch to origin
    or conflict the base — the cause of the sover dup-PRs and maki's conflicted main). Prepended to the
    agent's PATH so its `git`/`gh` resolve here first; read-only git and pi's OWN internal git use PASS
    THROUGH to the real binary, so this can only ever REFUSE the four named git verbs + gh. The runner's
    own git/gh use the real PATH (_clean_env), unaffected. Returns the dir, or None if it can't be made."""
    try:
        d = RUNTIME / "agent_shims"
        d.mkdir(parents=True, exist_ok=True)
    except OSError:
        return None
    _write_shim(d / "gh", '#!/bin/sh\necho "blocked by Solomon: use the read-only github_* tools, '
                          'not the gh CLI (the runner owns GitHub)" >&2\nexit 1\n')
    _write_shim(d / "gh.cmd", '@echo off\r\necho blocked by Solomon: use the github_* tools, not gh 1>&2'
                              '\r\nexit /b 1\r\n')
    real_git = shutil.which("git")
    if real_git and os.path.isabs(real_git):
        _write_shim(d / "git",
                    '#!/bin/sh\ncase "$1" in\n'
                    '  push|pull|merge|rebase) echo "blocked by Solomon: the runner owns version control '
                    '(no git $1 in the agent)" >&2; exit 1;;\n'
                    '  *) exec "' + real_git + '" "$@";;\nesac\n')
        _write_shim(d / "git.cmd",
                    '@echo off\r\n'
                    'if /I "%~1"=="push" goto blk\r\n'
                    'if /I "%~1"=="pull" goto blk\r\n'
                    'if /I "%~1"=="merge" goto blk\r\n'
                    'if /I "%~1"=="rebase" goto blk\r\n'
                    '"' + real_git + '" %*\r\n'
                    'goto :eof\r\n'
                    ':blk\r\necho blocked by Solomon: the runner owns version control 1>&2\r\nexit /b 1\r\n')
    return d


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
    shim = _agent_shim_dir()    # block the agent from gh + git push/merge/etc. (it ignores the contract)
    if shim:
        env["PATH"] = str(shim) + os.pathsep + env.get("PATH", "")
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


def _narrated_without_writing(summary: str) -> bool:
    """Heuristic: did the agent CLAIM it wrote/added code while the tree is actually clean? A weak model
    sometimes hallucinates its tool calls — it narrates 'Added tests/test_x.py …' but made no file edits,
    so the iteration is a no-op. Surfacing this distinguishes 'did nothing' from 'claimed work it didn't
    do' (a model-quality signal) without changing control flow. Pure over the summary text."""
    if not summary:
        return False
    s = summary.lower()
    claims_work = any(w in s for w in ("added ", "created ", "i add", "implement", "wrote ", "new file"))
    mentions_file = bool(re.search(r"`[^`]+\.[A-Za-z]{1,4}`", summary)) or ".py" in s
    return claims_work and mentions_file


# ---- gate -----------------------------------------------------------------
def run_gate() -> tuple:
    """Authoritative test gate. Returns (green, {passed,failed,errors,green}, tail).

    If a custom GATE_CMD was supplied (--gate), run THAT via the shell in REPO;
    green = returncode 0 (pytest-style N passed/failed parsed when present, else
    passed/failed=0). Otherwise run the built-in pytest gate.

    A hung gate (an infinite loop in a test, a test that waits on input) would otherwise
    freeze the iteration forever with the lock held; GATE_TIMEOUT bounds it — on timeout the
    gate is reported RED so the iteration reverts instead of hanging."""
    try:
        if GATE_CMD:
            # GATE_CMD is a TRUSTED, operator-only shell command (set via repos.json / the Config UI).
            # It is intentionally run with shell=True because real gates use compound syntax (`a && b`,
            # pipes, `-s tests -t tests`). It is never agent- or PR-derived; do not feed untrusted --gate.
            p = subprocess.run(GATE_CMD, shell=True, cwd=REPO, capture_output=True,
                               text=True, env=_clean_env(), creationflags=_NO_WINDOW, timeout=GATE_TIMEOUT)
        else:
            py = str(VENV_PY) if VENV_PY.exists() else sys.executable
            # `-o addopts=` clears any repo ini addopts (e.g. a stray `-q`, which combined with
            # our own would become `-qq` and SUPPRESS the "N passed" summary line we parse below).
            p = subprocess.run([py, "-m", "pytest", "-o", "addopts="], cwd=REPO, capture_output=True,
                               text=True, env=_clean_env(), creationflags=_NO_WINDOW, timeout=GATE_TIMEOUT)
    except subprocess.TimeoutExpired as e:
        partial = ((e.stdout or "") if isinstance(e.stdout, str) else "") + \
                  ((e.stderr or "") if isinstance(e.stderr, str) else "")
        log(f"gate TIMED OUT after {GATE_TIMEOUT}s — reporting RED so the iteration reverts")
        tests = {"passed": 0, "failed": 0, "errors": 1, "skipped": 0, "collected": 0,
                 "green": False, "timeout": True}
        return False, tests, (partial + f"\n[gate timed out after {GATE_TIMEOUT}s]")[-1500:]
    out = (p.stdout or "") + (p.stderr or "")

    def _n(pat):
        m = re.search(pat, out)
        return int(m.group(1)) if m else 0

    passed, failed, errors = _n(r"(\d+) passed"), _n(r"(\d+) failed"), _n(r"(\d+) error")
    skipped = _n(r"(\d+) skipped")
    collected = _n(r"collected (\d+) item")
    if not passed and not failed and not errors:
        # unittest doesn't print pytest-style "N passed"; parse its own summary so unittest gates
        # (e.g. `python -m unittest discover`) report real counts in the heartbeat + PR body.
        ran = re.search(r"Ran (\d+) tests?", out)
        if ran:
            failed, errors = _n(r"failures=(\d+)"), _n(r"errors=(\d+)")
            skipped = skipped or _n(r"skipped=(\d+)")
            ran_n = int(ran.group(1))
            passed = max(ran_n - failed - errors, 0)
            collected = collected or ran_n
    if not collected:                      # fall back to the sum so the anti-gaming collected-rail works
        collected = passed + failed + errors + skipped

    # collected = the spec-mandated discovered-test count (step 2); anti-gaming reverts a green change
    # that dropped it (tests deleted) even when 'passed' is held steady by adding a trivial test.
    tests = {"passed": passed, "failed": failed, "errors": errors,
             "skipped": skipped, "collected": collected, "green": p.returncode == 0}
    return p.returncode == 0, tests, out[-1500:]


# Every way a test can be neutered by a skip/xfail — not just the decorator form: the marker
# decorators (skip/skipif/xfail), in-body pytest.skip()/xfail() calls, and unittest's skipTest /
# raise SkipTest. A "green" gate that simply skipped the failing tests is gamed, so anti-gaming
# watches for any of these being ADDED.
_SKIP_MARKER_RE = re.compile(
    r"pytest\.mark\.(?:skip|skipif|xfail)\b"
    r"|pytest\.(?:skip|xfail)\s*\("
    r"|unittest\.skip"
    r"|\.skipTest\s*\("
    r"|raise\s+(?:unittest\.)?SkipTest")


def _new_skip_markers(diff_text: str) -> list:
    """Added (+) lines in a unified diff that introduce a pytest/unittest skip or xfail in ANY form
    (decorator, in-body call, skipTest, raise SkipTest). Anti-gaming: a 'green' gate that weakened
    tests by skipping them is reverted even though it passed. Pure over diff text so it's testable
    without a real git repo. The '+++ ' file-header line is excluded (it is not added content)."""
    return [ln for ln in (diff_text or "").splitlines()
            if ln.startswith("+") and not ln.startswith("+++") and _SKIP_MARKER_RE.search(ln)]


def _anti_gaming_reason(base_tests, tests, diff_text: str):
    """Why a GREEN gate should still be reverted as gamed, or None. Pure (no git/IO) so the rule is
    unit-tested directly: a dropped pass count (tests removed/weakened/skipped) or newly-introduced
    skip/xfail markers (weakening that need not drop the count)."""
    if base_tests and tests:
        if tests.get("passed", 0) < base_tests.get("passed", 0):
            return (f"pass count fell {base_tests['passed']}→{tests['passed']} "
                    f"(tests removed/weakened/skipped)")
        # spec: the COLLECTED count must not drop either — deleting real tests and adding one
        # trivially-passing test can hold 'passed' steady while true coverage shrinks.
        base_c, c = base_tests.get("collected"), tests.get("collected")
        if base_c and c is not None and c < base_c:
            return f"collected count fell {base_c}→{c} (tests removed)"
    skips = _new_skip_markers(diff_text)
    if skips:
        return f"introduced {len(skips)} skip/xfail marker(s)"
    return None


# ---- pr -------------------------------------------------------------------
_TIERS = ("chore", "feature", "refactor", "architecture")


def _strip_tier(text: str):
    """Split a leading [chore|feature|refactor|architecture] ambition tag off a backlog item.
    Returns (text_without_tag, tier). Default 'chore' = today's safe, smallest-change behavior, so an
    untagged (legacy) backlog behaves byte-identically; higher tiers lift the smallest-change ceiling."""
    m = re.match(r"\[(chore|feature|refactor|architecture)\]\s*", (text or "").strip(), re.I)
    if m:
        return ((text or "").strip()[m.end():].strip(), m.group(1).lower())
    return ((text or "").strip(), "chore")


def _top_backlog_item():
    """(text, tier) of the first unchecked `- [ ]` item; tier from its leading tag, default 'chore'."""
    try:
        for line in BACKLOG.read_text(encoding="utf-8").splitlines():
            s = line.strip()
            if s.startswith("- [ ]"):
                return _strip_tier(s[5:].strip())
    except OSError:
        pass
    return "model-chosen improvement", "chore"


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
        if s.startswith("- [ ]") and _strip_tier(s[5:].strip())[0] == goal.strip():
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


_deviation_counts: dict = {}   # per-goal consecutive-deviation tally, for this loop process's lifetime


def _note_deviation(goal: str, limit: int = 3) -> None:
    """Track consecutive iterations where the agent DEVIATED from a backlog goal (shipped a real
    change, but to something OTHER than the named item). Deviations don't tick the item and aren't
    noops, so without this the loop re-selects the same item forever and keeps shipping unrelated PRs
    under its name. After `limit` deviations, defer the item so the loop ADVANCES."""
    if not goal or BEAUTIFY or SOLOMON or goal.lower() == "model-chosen improvement":
        return
    _deviation_counts[goal] = _deviation_counts.get(goal, 0) + 1
    if _deviation_counts[goal] >= limit:
        if _defer_backlog_item(goal):
            log(f"item deviated {limit}x — deferred to bottom of backlog: {goal[:60]}")
        _deviation_counts[goal] = 0


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
        if s.startswith("- [ ]") and _strip_tier(s[5:].strip())[0] == goal.strip():
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


def _await_pr_checks(number, attempts: int = 6, delay: float = 8.0) -> str | None:
    """Poll a just-opened PR's checks so auto-merge does NOT merge in the empty-rollup window. A freshly
    created PR usually reports an empty statusCheckRollup ('None') for several seconds before CI registers
    its check runs; treating that transient None as 'no CI configured' and merging immediately would
    bypass CI (defeating pr-only-shipping-with-auto-revert for the auto-merge path). Retry until a
    concrete state appears, only concluding 'no CI configured' if it stays None for the whole window.
    Returns 'success'|'pending'|'failure'|None. Only used on ship=auto-merge."""
    last = None
    for i in range(max(1, attempts)):
        last = _pr_checks(number)
        if last is not None:
            return last
        if STOP.exists() or i >= attempts - 1:
            break
        time.sleep(delay)
    return last


def _existing_open_pr(branch: str) -> tuple:
    """(number, url) of the OPEN PR whose head is ``branch``, or (None, None). Used to ADOPT a PR the
    agent opened itself (it is told NOT to run gh, but an over-eager model sometimes does) so the runner
    doesn't fall to 'push-only' on a 'gh pr create … already exists' error — which never ticks the item,
    so the loop re-ships the same backlog item forever (the live sover dup-PR spin)."""
    p = subprocess.run([gh_exe(), "pr", "list", "--head", branch, "--state", "open",
                        "--json", "number,url"],
                       cwd=REPO, capture_output=True, text=True, env=_clean_env(), creationflags=_NO_WINDOW)
    if p.returncode != 0:
        return None, None
    try:
        arr = json.loads(p.stdout or "[]")
    except (ValueError, TypeError):
        return None, None
    if isinstance(arr, list) and arr and isinstance(arr[0], dict):
        return arr[0].get("number"), arr[0].get("url")
    return None, None


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
        stderr = (p.stderr or "").strip()
        # The agent may have already opened a PR for this branch (it's told NOT to run gh, but a
        # too-eager model sometimes does) — 'gh pr create' then fails "already exists". Adopt that PR
        # instead of returning push-only (which never ticks the item, so the loop re-ships it forever).
        if "already exists" in stderr.lower():
            num, url = _existing_open_pr(branch)
            if num:
                log(f"adopted existing PR #{num} for {branch} (gh pr create: already exists)")
                return {"number": num, "url": url, "branch": branch, "state": "open"}
        log(f"gh pr create failed: {stderr[:200]}")
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

    cur_branch = git("rev-parse", "--abbrev-ref", "HEAD").stdout.strip()
    if _dirty_blocks_iteration(tree_dirty(), cur_branch, BASE_BRANCH):
        heartbeat(status="error", phase="preflight",
                  last_summary=f"Working tree is dirty on the base branch '{BASE_BRANCH}' — commit or "
                               "stash your changes; the loop won't clobber base-branch work.")
        log(f"SKIP iteration: working tree dirty on base branch '{BASE_BRANCH}'")
        return
    # (a dirty rsi/* or detached/other branch is a dead run's mid-iteration leftover — the forced
    #  preflight reset below discards it, so a killed runner can't wedge the loop forever)

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
    git("reset", "--hard")    # drop tracked changes from a dead run
    # `git clean -fd` deletes ALL non-ignored untracked files — but on the base branch those files are
    # either operator scratch work (a new test, a not-yet-added module) or a dead run's leftovers, and
    # the two are indistinguishable after the checkout. Silently deleting operator files every iteration
    # is a data-loss path that violates the never-discard-operator-work keystone. GUARD: if there are
    # untracked non-ignored files, skip+escalate instead of cleaning (the operator commits/stashes/removes
    # them; the loop self-resumes once the tree is clean). A dead run's leftovers are surfaced the same
    # way — preserved, not destroyed. Only clean when there is nothing to destroy (a safe no-op).
    untracked = _untracked_non_ignored_files()
    if untracked:
        heartbeat(status="error", phase="preflight",
                  last_summary=f"Untracked non-ignored files on '{BASE_BRANCH}' would be deleted by "
                               f"the preflight clean — the loop won't destroy possible operator work. "
                               f"Commit, stash, or remove them: {', '.join(untracked[:8])}"
                               + (f" (+{len(untracked)-8} more)" if len(untracked) > 8 else ""))
        log(f"REFUSE preflight clean: {len(untracked)} untracked non-ignored file(s) on {BASE_BRANCH} "
            f"— skip+escalate (won't destroy operator work)")
        return
    git("clean", "-fd")       # safe now: no untracked non-ignored files to destroy
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
    if not BEAUTIFY:
        bgreen, base_tests, _ = run_gate()
        # A normal iteration needs a GREEN base to measure a gain. A SOLOMON fix-session runs ON a red
        # base by definition (it is invoked precisely to fix the failing gate), so do NOT abort it on a
        # red base — but still keep base_tests so the post-change pass-count anti-gaming check (below)
        # applies to the fix-session too (a recovery path that edits managed code must not game the gate).
        if not bgreen and not SOLOMON:
            heartbeat(status="error", phase="preflight",
                      last_summary=f"Base gate is RED before any change ({base_tests}). Fix the gate "
                                   f"command or the base; the loop can't measure a gain from a red base.")
            log(f"base gate RED — skipping iteration {n}")
            git("checkout", "--force", BASE_BRANCH)
            git("branch", "-D", branch)
            return
        # A custom gate that exits 0 but prints no parseable counts yields passed=0, making the
        # pass-count anti-gaming check (base 0 vs after 0) a silent no-op. Surface that the numeric rail
        # is inactive for this gate (returncode + the skip/xfail-marker diff check still apply).
        if base_tests and GATE_CMD and not any(base_tests.get(k) for k in ("passed", "failed", "errors")):
            log("WARNING: custom gate emitted no parseable test counts — the pass-count anti-gaming "
                "check is INACTIVE for this gate (only returncode + skip/xfail-marker detection apply). "
                "Have the gate print a pytest-style 'N passed' or unittest 'Ran N tests' summary.")

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
        goal, tier = _top_backlog_item()
        task = build_task(goal, tier)
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
    # Redact secret-shaped strings at the single source the commit body, PR body, history.jsonl, log,
    # and heartbeat all derive from — so a model that echoed a secret can't leak it downstream (SEC-1).
    summary = _redact(final_text(p.stdout)) or "(no summary returned)"
    summary, item_deviated = _split_item_status(summary)   # don't tick the item if the agent deviated
    log(f"Pi rc={p.returncode}: {summary[:200]}")

    if not tree_dirty() and head_sha() == base:
        if _narrated_without_writing(summary):
            log("WARNING: Pi narrated a change but wrote nothing to a clean tree — the model likely "
                "hallucinated its file edits; counting as a no-op")
            summary = "[narrated-but-unwritten] " + summary
        else:
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
            log(f"gate tail: {_redact(tail[-400:])}")   # SEC-5: failing test output can carry secrets
            _drop_branch(branch, "reverted",
                         f"Reverted — tests failed ({tests['failed']} failed). {summary}")
            return
        # Anti-gaming: a GREEN gate that dropped the pass/collected count or added skip/xfail markers
        # means tests were removed/weakened — revert even though it is "green". Stage first (git add -A)
        # so the diff includes NEW UNTRACKED test files — `git diff <commit>` omits untracked files, so a
        # skip/xfail added inside a brand-new test file (the common case: Pi writes new tests) would
        # otherwise be invisible — then diff the staged tree against base.
        git("add", "-A")
        gamed = _anti_gaming_reason(base_tests, tests, git("diff", "--cached", BASE_BRANCH).stdout or "")
        if gamed:
            log(f"anti-gaming: {gamed} — reverting")
            _drop_branch(branch, "reverted", f"Reverted — anti-gaming: {gamed}. {summary}")
            return

    # commit anything Pi left uncommitted (it shouldn't commit, but be robust)
    heartbeat(phase="commit")
    add = git("add", "-A")
    if add.returncode != 0:
        # `git add` can fail outright — e.g. an untracked Windows reserved-name file (`nul`, `con`, …)
        # that core.protectNTFS refuses — and then NOTHING stages: the iteration silently looks like a
        # no-op while the agent's real work is dropped (observed: asmodeus no-op-spun for hours on a
        # stray `nul` at its repo root). Surface it as an ERROR with the cause, not a silent no-op.
        err = (add.stderr or "").strip()[:200]
        log(f"git add -A failed: {err}")
        _abort_branch(branch)
        heartbeat(status="error", phase="commit",
                  last_summary=f"git add failed — the agent's change could not be staged ({err}). If an "
                               f"untracked Windows reserved-name file (e.g. `nul`) is in the tree, add it "
                               f"to .git/info/exclude.")
        _record_history("error", branch, summary)
        return
    if git("diff", "--cached", "--quiet").returncode != 0:
        title = "beautify repo" if BEAUTIFY else _pr_title("" if item_deviated else goal, summary)
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

    # Catch a DEVIATING agent that reports ITEM-STATUS: done while shipping UNRELATED work: when the
    # backlog item names concrete files and the committed diff touched NONE of them, the agent did
    # something other than the named item — don't trust its self-report (live: sover's privacy item
    # named pyproject.toml and the beautify item named README.md/banner.svg/CONTRIBUTING.md, yet both
    # shipped capability_plan/chat changes and claimed done, silently consuming the item). Mark it
    # deviated so it is NOT ticked — it defers after a few tries instead of being lost as 'done'.
    if not BEAUTIFY and not SOLOMON and not item_deviated:
        if _deviated_from_named_files(goal, git("diff", "--name-only", "--no-renames", f"{BASE_BRANCH}..{branch}").stdout):
            log(f"DEVIATION: the item names file(s) the committed diff never touched — agent shipped "
                f"unrelated work; not ticking '{goal[:60]}'")
            item_deviated = True

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

    title = "beautify repo" if BEAUTIFY else _pr_title("" if item_deviated else goal, summary)
    pr = _ship(branch, title, summary, tests)
    git("checkout", BASE_BRANCH)
    # Advance the backlog ONLY on a real LANDED ship of the NAMED item — a PR opened (pr-mode) /
    # merged or queued (auto-merge) / a verified push / a kept-local branch. Never on a push/auth
    # FAILURE or an auto-merge PR left un-merged on red CI (item lost without landing), and never
    # when the agent DEVIATED to a different change (the item didn't ship under its own name).
    landed = _ship_succeeded(pr)
    if not BEAUTIFY and not SOLOMON and landed:
        if item_deviated:
            _note_deviation(goal)         # defer the item if the agent keeps shipping something ELSE
        else:
            _mark_backlog_done(goal)
    heartbeat(status="sleeping", phase="sleep", last_pr=pr, last_summary=summary)
    # 'shipped' only when the change actually landed (or is in-flight to merge); an auto-merge PR
    # left un-merged on red/awaiting CI, or a failed push, records 'blocked' so it is not counted as
    # a success and a streak of them is diagnosable (ci_red_streak) instead of silently 'shipped'.
    _record_history("shipped" if landed else "blocked", branch, summary)


def _ship_succeeded(pr: dict) -> bool:
    """A terminal ship that LANDED (and so should advance the backlog item):
      - an opened PR (pr/auto-merge) — EXCEPT an auto-merge PR left un-merged on red/awaiting CI
        ('open (CI red ...)' / 'open (awaiting CI)' / 'open (stopped...)'); those did NOT land, so
        the item stays open and the supervisor's ci_red_streak surfaces them instead of the item
        being silently consumed while the red PR rots;
      - a VERIFIED push (ship=push) — without this the loop re-pushes the same branch every iteration;
      - a deliberately kept-local branch (ship=local).
    NOT a push/auth failure (those didn't land and must be retried)."""
    state = (pr.get("state") or "").lower()
    if "fail" in state:
        return False
    if pr.get("number"):
        # un-landed auto-merge states all read 'open (CI red ...)' / 'open (awaiting CI)' /
        # 'open (stopped ...)'; the landed/in-flight ones ('merged', 'auto-merge queued (...)') and a
        # plain pr-mode 'open' do not contain the '(' marker. (Substring-safe: 'not merged' contains
        # 'merged', so we must NOT test for 'merged' directly.)
        return "open (" not in state
    if "local" in state and "pending" not in state:
        return True
    return state.startswith("pushed") and bool(pr.get("verified"))


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
        if SHIP in ("pr", "auto-merge"):
            # SOLOMON_RSI step 7: verify the branch landed BEFORE opening a PR. An unverified push
            # means gh pr create will fail anyway; record a non-landed ERROR state (don't tick the
            # item, don't open a PR) so the supervisor surfaces it instead of a benign-looking state.
            heartbeat(status="error", phase="ship")
            return {"number": None, "url": None, "branch": branch,
                    "state": "push-unverified (failed)", "verified": False}

    if SHIP == "push":
        log(f"ship=push — pushed {branch} (no PR){'' if verified else ' [UNVERIFIED]'}")
        return {"number": None, "url": None, "branch": branch,
                "state": "pushed (no PR)" if verified else "pushed (unverified)", "verified": verified}

    heartbeat(phase="pr")
    pr = _open_pr(branch, title, summary, tests)
    pr["verified"] = verified
    # auto-merge must not merge before CI registers: poll so an empty rollup right after PR-create is
    # treated as 'CI not reported yet' (retry), not 'no CI configured' (merge now). Other ship modes
    # only display checks, so a single read is fine.
    pr["checks"] = (_await_pr_checks(pr.get("number")) if SHIP == "auto-merge"
                    else _pr_checks(pr.get("number")))
    log(f"opened PR: {pr.get('url')} (push {'verified' if verified else 'UNVERIFIED'}, CI {pr.get('checks') or 'none'})")
    if SHIP == "auto-merge":
        if STOP.exists():
            # a Stop arrived during the iteration (e.g. while _await_pr_checks was polling, which
            # returns None on a STOP-break): do NOT auto-merge — merging on a None rollup would land
            # an un-CI'd PR. Leave it open for review (honors the halt switch).
            log("stop requested — not auto-merging; PR left open for review")
            pr = {**pr, "state": "open (stopped before merge)"}
        else:
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


def _read_lock_pid() -> int:
    """The pid on the FIRST line of LOCK ('<pid>' or '<pid>\\n<run_id>'): a positive int, or 0 when the
    lock is missing, empty (a racer created it but has not written its pid yet), or corrupt. Never raises."""
    try:
        raw = LOCK.read_text(encoding="utf-8")
    except OSError:
        return 0
    first = raw.splitlines()[0].strip() if raw.strip() else ""
    try:
        return int(first) if first else 0
    except ValueError:
        return 0


def _heartbeat_stale(window: float) -> bool:
    """True only with POSITIVE evidence the lock holder is dead: its heartbeat's updated_at is older than
    `window` seconds (>= the longest agent session, so a runner mid-session is never mistaken for dead).
    A missing/unparseable heartbeat returns False — no evidence, so we never steal a possibly-live lock."""
    try:
        hb = json.loads(HEARTBEAT.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return False
    ts = hb.get("updated_at") if isinstance(hb, dict) else None
    if not ts:
        return False
    try:
        last = datetime.strptime(ts, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)
    except (ValueError, TypeError):
        return False
    return (datetime.now(timezone.utc) - last).total_seconds() > window


def _heartbeat_is_stopped() -> bool:
    """True if the lock holder's heartbeat reports a clean stop (status='stopped'). Such a runner has
    EXITED even if its lock lingered with a since-recycled PID, so a restart may take the lock over."""
    try:
        hb = json.loads(HEARTBEAT.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return False
    return isinstance(hb, dict) and hb.get("status") == "stopped"


def acquire_lock() -> bool:
    """Single-flight: at most one improver per repo. Returns True iff WE now hold the lock.

    Two improvers were once observed running on one repo (sover) because the old code
    created the lock file EMPTY (``open(LOCK, "x")``) and wrote the pid in a SECOND step:
    a racer that hit FileExistsError inside that window read the still-empty file as pid 0,
    mistook it for a stale lock, and stole it via ``os.replace``. The fix has three parts:
      1. the create writes the pid in the SAME exclusive ``os.open`` (O_CREAT|O_EXCL), so the
         file is never observed empty by our own create;
      2. an EXISTING lock that reads empty is treated as HELD by a mid-write racer — we back
         off, never treat empty as stale;
      3. only a lock whose recorded pid is a confirmed-DEAD process is taken over, and the
         takeover is verified by reading our pid back, so two racers cannot both adopt the
         same stale lock."""
    RUNTIME.mkdir(parents=True, exist_ok=True)
    mypid = os.getpid()
    try:  # atomic exclusive create WITH the pid written before the handle closes (no empty window)
        fd = os.open(str(LOCK), os.O_CREAT | os.O_EXCL | os.O_WRONLY)
        try:
            os.write(fd, f"{mypid}\n{RUN_ID}".encode("ascii"))   # pid + identity token (no empty window)
        finally:
            os.close(fd)
        return True
    except FileExistsError:
        pass
    # The lock exists. Resolve who holds it, tolerating a racer's just-created-but-empty file.
    pid = 0
    for _ in range(10):                  # ~0.5s grace for a racing creator to write its pid
        pid = _read_lock_pid()
        if pid:
            break
        time.sleep(0.05)
    if pid == mypid:
        return True                      # already ours
    if pid == 0:
        return False                     # still empty after the grace window — a racer holds it
    if _pid_alive(pid) and not _heartbeat_stale(max(3 * INTERVAL, 3600)) and not _heartbeat_is_stopped():
        return False                     # held by a live improver (PID alive, heartbeat fresh, not stopped)
    # Recorded pid is dead, OR alive-but-its-heartbeat-froze (a recycled PID whose original runner is
    # gone), OR the heartbeat says the runner cleanly stopped (lingering lock) -> take over, then VERIFY
    # we won (last os.replace wins; the loser must back off so a dead lock can't be adopted by two racers).
    try:
        tmp = RUNTIME / f"lock.{mypid}.tmp"
        tmp.write_text(f"{mypid}\n{RUN_ID}", encoding="utf-8")
        os.replace(tmp, LOCK)
    except OSError:
        return False
    time.sleep(0.1)                      # let any co-racer's replace land before we read back
    return _read_lock_pid() == mypid


def release_lock() -> None:
    # Only unlink the lock if it is still OURS. A read error / unparseable content must NOT trigger a
    # delete — the old except-branch unlinked unconditionally, which could remove a lock another runner
    # legitimately holds (re-opening two-runners-on-one-repo). A genuinely stale lock is reclaimed by
    # acquire_lock's dead-pid takeover path instead. _read_lock_pid returns 0 on missing/empty/corrupt,
    # so an unreadable lock is left untouched.
    try:
        if _read_lock_pid() == os.getpid():
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
def _strip_code_fence(text: str) -> str:
    """Strip a SINGLE leading/trailing ``` code fence if present — NOT all backtick characters.
    str.strip('`') removes every leading/trailing backtick, mangling content whose first/last line
    is an inline-code span (e.g. an AGENT.md gate line starting with `pytest -q`). Only a complete
    fence (a line that is just ``` optionally followed by a language tag) should be removed."""
    lines = text.splitlines()
    # strip a leading fence line
    if lines and re.match(r"^```\w*\s*$", lines[0].strip()):
        lines = lines[1:]
    # strip a trailing fence line
    if lines and re.match(r"^```\s*$", lines[-1].strip()):
        lines = lines[:-1]
    return "\n".join(lines).strip()


def _parse_provision(text: str):
    """Pull the two ===AGENT.md=== / ===backlog.md=== blocks out of the provisioner's output."""
    if "===AGENT.md===" not in text or "===backlog.md===" not in text:
        return None, None
    after = text.split("===AGENT.md===", 1)[1]
    agent_part, backlog_part = after.split("===backlog.md===", 1)
    # The old code did backlog_part.split("===")[0] which truncated at the FIRST '===' substring —
    # a markdown horizontal rule or table inside the backlog would silently drop the rest. The
    # backlog block runs to end-of-text (the provisioner emits nothing after it); _strip_code_fence
    # removes a trailing ``` fence if the model wrapped the block.
    agent_md = _strip_code_fence(agent_part.strip())
    backlog_md = _strip_code_fence(backlog_part.strip())
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


# ---- ideate (divergent backlog generation — the anti-shallowness lane) -----
def _parse_ideas(text: str):
    """Parse the ideate lane's `[tier] | leverage | idea` lines into (leverage, tier, idea) tuples,
    sorted by leverage descending (highest-leverage first). The agent emits candidates; PYTHON owns
    the backlog write (same steering boundary as provision)."""
    ideas = []
    for ln in (text or "").splitlines():
        # tolerant of how a model actually formats it: leading bullets / numbers / markdown bold,
        # optional brackets around the tier, the 'chore' tier, |/:/-/— separators, multi-digit (or
        # absent -> 3) leverage. The tier must lead the line so prose ("this feature is nice") is ignored.
        m = re.match(r"^[\s\-*\d.)#>]*\**\[?\s*(feature|refactor|architecture)\s*\]?\**"
                     r"\s*[|:\-–—]*\s*(\d+)?\s*[|:\-–—]*\s*(.+?)\s*$", ln.strip(), re.I)
        if m and len(m.group(3).strip()) > 8:          # a real idea, not a bare tier/header line (chore dropped)
            lev = min(5, max(1, int(m.group(2)))) if m.group(2) else 3
            ideas.append((lev, m.group(1).lower(), m.group(3).strip().rstrip("`").strip()))
    ideas.sort(key=lambda t: -t[0])
    return ideas


def ideate() -> int:
    """One-shot divergent pass: pi proposes ambitious, leverage-ranked, tier-tagged improvements; the
    runner PREPENDS them (highest-leverage first) to the backlog as `- [ ] [tier] ...` items. No git,
    no gate, no loop — the greedy gate-enforced loop executes them later. Prints one JSON line."""
    goal_line = (f"\n\nNORTH-STAR GOAL (rank every idea by how much it advances THIS):\n{GOAL}\n"
                 if GOAL else "\n\n(No north-star goal set — propose the highest-leverage improvements "
                              "toward making this project excellent at what it's for.)\n")
    task = ("Read this repository and propose its next batch of ambitious, high-leverage improvements "
            "per ideate.md. Output ONLY the idea lines." + goal_line)
    try:
        p = run_pi(task, system_md=IDEATE_MD, timeout=600)
    except subprocess.TimeoutExpired:
        print(json.dumps({"ok": False, "error": "ideate timed out"}))
        return 5
    raw = final_text(p.stdout) or ""
    ideas = _parse_ideas(raw)
    if not ideas:
        log(f"ideate: no parseable ideas. Raw agent output (first 800 chars):\n{raw[:800]}")
        print(json.dumps({"ok": False, "error": "ideate emitted no parseable ideas"}))
        return 5
    new_lines = [f"- [ ] [{tier}] {idea}" for _lev, tier, idea in ideas]
    try:
        existing = BACKLOG.read_text(encoding="utf-8") if BACKLOG.exists() else "# backlog\n"
    except OSError:
        existing = "# backlog\n"
    # prepend ambitious items above the existing menu, after a header line if present
    lines = existing.splitlines()
    head = 1 if lines and lines[0].lstrip().startswith("#") else 0
    merged = lines[:head] + ([""] if head else []) + new_lines + lines[head:]
    try:
        BACKLOG.parent.mkdir(parents=True, exist_ok=True)
        BACKLOG.write_text("\n".join(merged).rstrip() + "\n", encoding="utf-8")
    except OSError as e:
        print(json.dumps({"ok": False, "error": str(e)}))
        return 5
    print(json.dumps({"ok": True, "added": len(new_lines),
                      "top": new_lines[0][:90] if new_lines else ""}))
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
    ap.add_argument("--ideate", action="store_true",
                    help="one-shot: prepend ambitious, leverage-ranked, tier-tagged ideas to the backlog")
    ap.add_argument("--solomon", action="store_true",
                    help="supervisor fix-session: diagnose + fix a persistent gate failure (one iteration)")
    a = ap.parse_args(argv)
    configure(a.repo, a.name or Path(a.repo).name, a.provider, a.model)
    global SHIP, GATE_CMD, REASONING, GOAL, BEAUTIFY, SOLOMON, INTERVAL
    SHIP = a.ship
    GATE_CMD = a.gate or ""
    REASONING = a.reasoning or ""
    GOAL = (a.goal or "").strip()
    BEAUTIFY = a.beautify
    SOLOMON = a.solomon
    REASONING = REASONING or "xhigh"     # default to max reasoning when not explicitly set
    INTERVAL = max(1, a.interval)        # the cooldown AND the lock-staleness base (acquire_lock takeover)
    if BEAUTIFY or SOLOMON:
        a.once = True  # beautify + the supervisor fix-session are single-shot

    if a.smoke:
        return smoke()

    _load_env()
    if a.provision:                        # generate the contract files, then exit (no git/loop needed)
        return provision()
    if a.ideate:                           # prepend ambitious ideas to the backlog, then exit
        return ideate()
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
            _refresh_config_from_registry()   # pick up dashboard edits to model/gate/reasoning/goal mid-loop
            one_iteration()
            if _HALTED:
                log("halted after an unrecoverable revert failure — operator action required "
                    "(the repo is left at status=error/reverted for the supervisor to escalate)")
                break
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
        # Preserve the error/reverted heartbeat on a halt so the supervisor still diagnoses
        # revert_failed (and the watchdog leaves it alone); a normal exit reports 'stopped'.
        if not _HALTED:
            heartbeat(status="stopped", phase=None)
        release_lock()
        try:
            STOP.unlink()
        except OSError:
            pass
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
