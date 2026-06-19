"""Guard: no child process may pop a console window on this app's desktop.

Every process spawn in first-party code must be windowless on Windows:

* Python ``subprocess.{Popen,run,call,check_call,check_output}`` must pass
  ``**hidden_subprocess_kwargs(...)`` (CREATE_NO_WINDOW + SW_HIDE). ``os.system``
  is banned outright (always shells through a visible cmd).
* Node/TS ``child_process`` ``spawn/exec/execFile/fork`` (and *Sync variants)
  must pass ``windowsHide: true``.

This test fails the gate on any unguarded spawn, so console popups can never
regress -- including via the autonomous self-improvement loops, which must pass
this gate before merging. Drop it into a repo's ``tests/`` next to the others.

Portable across repos: it discovers the repo root as the parent of this file's
directory and scans first-party source only (vendored / build dirs are skipped).
"""

from __future__ import annotations

import ast
import re
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

# Vendored, generated, or build-output trees we never police, plus the test tree.
SKIP_DIRS = {
    ".venv", "venv", "env", "build", "dist", "node_modules", ".git",
    "__pycache__", ".pytest_cache", ".mypy_cache", "tests", "release",
    "site-packages", ".ruff_cache",
}
# The helper module(s) themselves legitimately call subprocess with raw flags.
ALLOW_FILES = {"winproc.py", "subprocess_util.py"}

_PY_SPAWN = {"Popen", "run", "call", "check_call", "check_output"}
_JS_SPAWN = (
    "spawn", "spawnSync", "exec", "execSync", "execFile", "execFileSync", "fork",
)


def _scan_dir_ok(path: Path) -> bool:
    rel = path.relative_to(REPO)
    return not any(part in SKIP_DIRS for part in rel.parts)


# ── Python ──────────────────────────────────────────────────────────────────
def _py_files():
    for p in REPO.rglob("*.py"):
        if _scan_dir_ok(p) and p.name not in ALLOW_FILES:
            yield p


def _subprocess_bindings(tree: ast.AST) -> tuple[set[str], set[str]]:
    """Per-file names that resolve to the subprocess module / its spawn funcs.

    Catches ``import subprocess as sp`` (``sp.run(...)``) and
    ``from subprocess import Popen, run as r`` (bare ``Popen(...)`` / ``r(...)``),
    not just the plain ``subprocess.run(...)`` form.
    """
    module_aliases = {"subprocess"}
    func_aliases: set[str] = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            for a in node.names:
                if a.name == "subprocess":
                    module_aliases.add(a.asname or "subprocess")
        elif isinstance(node, ast.ImportFrom) and node.module == "subprocess":
            for a in node.names:
                if a.name in _PY_SPAWN:
                    func_aliases.add(a.asname or a.name)
    return module_aliases, func_aliases


def _is_subprocess_spawn(call: ast.Call, modules: set[str], funcs: set[str]) -> bool:
    f = call.func
    if (
        isinstance(f, ast.Attribute)
        and f.attr in _PY_SPAWN
        and isinstance(f.value, ast.Name)
        and f.value.id in modules
    ):
        return True
    return isinstance(f, ast.Name) and f.id in funcs


def _is_os_system(call: ast.Call) -> bool:
    f = call.func
    return (
        isinstance(f, ast.Attribute)
        and f.attr == "system"
        and isinstance(f.value, ast.Name)
        and f.value.id == "os"
    )


def _has_hidden_spread(call: ast.Call) -> bool:
    """True if the call has ``**hidden_subprocess_kwargs(...)``."""
    for kw in call.keywords:
        if kw.arg is not None or not isinstance(kw.value, ast.Call):
            continue
        vf = kw.value.func
        name = vf.attr if isinstance(vf, ast.Attribute) else getattr(vf, "id", "")
        if name == "hidden_subprocess_kwargs":
            return True
    return False


def _python_violations() -> list[str]:
    out: list[str] = []
    for path in _py_files():
        try:
            tree = ast.parse(path.read_text(encoding="utf-8"))
        except (SyntaxError, UnicodeDecodeError):
            continue
        modules, funcs = _subprocess_bindings(tree)
        for node in ast.walk(tree):
            if not isinstance(node, ast.Call):
                continue
            if _is_os_system(node):
                out.append(f"{path}:{node.lineno}: os.system() is banned (pops a cmd window)")
            elif _is_subprocess_spawn(node, modules, funcs) and not _has_hidden_spread(node):
                fn = node.func.attr if isinstance(node.func, ast.Attribute) else node.func.id
                out.append(
                    f"{path}:{node.lineno}: subprocess {fn}() must pass "
                    "**hidden_subprocess_kwargs(...)"
                )
    return out


# ── Node / TypeScript ─────────────────────────────────────────────────────────
def _js_files():
    for ext in ("*.ts", "*.tsx", "*.js", "*.jsx", "*.mjs", "*.cjs"):
        for p in REPO.rglob(ext):
            if _scan_dir_ok(p):
                yield p


def _bound_spawn_names(src: str) -> tuple[set[str], set[str]]:
    """Return (bare destructured spawn names, namespace aliases) bound to
    child_process in this file. Avoids false positives like ``regex.exec``."""
    bare: set[str] = set()
    ns: set[str] = set()
    cp = r"['\"](?:node:)?child_process['\"]"
    # import { spawn, execFile as ef } from "child_process"
    for m in re.finditer(r"import\s*\{([^}]*)\}\s*from\s*" + cp, src):
        for piece in m.group(1).split(","):
            piece = piece.strip()
            if not piece:
                continue
            local = piece.split(" as ")[-1].strip()
            if local:
                bare.add(local)
    # const { spawn } = require("child_process")
    for m in re.finditer(r"(?:const|let|var)\s*\{([^}]*)\}\s*=\s*require\(\s*" + cp + r"\s*\)", src):
        for piece in m.group(1).split(","):
            piece = piece.strip()
            local = piece.split(":")[-1].strip()
            if local:
                bare.add(local)
    # import * as cp from "child_process" / import cp from "child_process"
    for m in re.finditer(r"import\s+(?:\*\s+as\s+)?(\w+)\s+from\s*" + cp, src):
        ns.add(m.group(1))
    # const cp = require("child_process")
    for m in re.finditer(r"(?:const|let|var)\s+(\w+)\s*=\s*require\(\s*" + cp + r"\s*\)", src):
        ns.add(m.group(1))
    return bare, ns


def _call_arg_span(src: str, open_paren: int) -> str:
    """Substring of the balanced (...) starting at ``open_paren``."""
    depth = 0
    for i in range(open_paren, len(src)):
        c = src[i]
        if c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
            if depth == 0:
                return src[open_paren : i + 1]
    return src[open_paren:]


def _js_violations() -> list[str]:
    out: list[str] = []
    for path in _js_files():
        try:
            src = path.read_text(encoding="utf-8")
        except (UnicodeDecodeError, OSError):
            continue
        if "child_process" not in src:
            continue
        bare, ns = _bound_spawn_names(src)
        patterns: list[re.Pattern] = []
        for fn in _JS_SPAWN:
            if fn in bare:
                # bare call not preceded by a dot (skip obj.spawn / regex.exec)
                patterns.append(re.compile(r"(?<![.\w])" + fn + r"\s*\("))
            for alias in ns:
                patterns.append(re.compile(r"\b" + re.escape(alias) + r"\." + fn + r"\s*\("))
        for pat in patterns:
            for m in pat.finditer(src):
                open_paren = src.index("(", m.start())
                span = _call_arg_span(src, open_paren)
                if "windowsHide" not in span:
                    line = src.count("\n", 0, m.start()) + 1
                    out.append(
                        f"{path}:{line}: child_process {m.group().rstrip('(').strip()}() "
                        "must pass windowsHide: true"
                    )
    return out


# ── the test ──────────────────────────────────────────────────────────────────
def test_no_unguarded_process_spawns():
    violations = _python_violations() + _js_violations()
    assert not violations, (
        "Unguarded process spawns would pop console windows on the desktop:\n  "
        + "\n  ".join(violations)
        + "\n\nRoute Python spawns through **hidden_subprocess_kwargs(...) and "
        "Node spawns through windowsHide: true."
    )
