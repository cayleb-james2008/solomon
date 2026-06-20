"""Tests for the PUBLIC-repo leak-prevention layer in run_improver.py:

1. _leak_in_diff: a committed diff's ADDED lines are scanned for operator deny-terms + secret tokens
   (removed lines and the +++ header are ignored); a hit reverts the branch before any push.
2. _redact: operator deny_terms (brand/account identity) are scrubbed from agent free-text alongside
   the existing token-shape patterns.
3. _git_add_all: for a PUBLIC repo, configured private_paths are excluded from staging even when NOT
   gitignored — so an agent that un-ignored a private path still cannot push it.
"""
import importlib.util
import os
import shutil
import subprocess
import sys

import pytest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)
sys.path.insert(0, os.path.join(ROOT, "improver"))
RUNNER = os.path.join(ROOT, "improver", "run_improver.py")


def _load_runner():
    spec = importlib.util.spec_from_file_location("run_improver", RUNNER)
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


def _git(path, *a):
    return subprocess.run(["git", "-C", str(path), *a], capture_output=True, text=True)


def _mk_repo(tmp_path):
    work = tmp_path / "work"
    work.mkdir()
    _git(work, "init", "-b", "main")
    _git(work, "config", "user.email", "t@t")
    _git(work, "config", "user.name", "t")
    (work / "f.txt").write_text("base")
    _git(work, "add", "-A")
    _git(work, "commit", "-m", "init")
    return work


# ---- 1. _leak_in_diff -------------------------------------------------------
def test_leak_in_diff_catches_deny_term(monkeypatch):
    m = _load_runner()
    monkeypatch.setattr(m, "_repo_deny_terms", lambda name: ["gains.god.growth"])
    diff = "diff --git a/x b/x\n+++ b/x\n+post to Gains.God.Growth today\n hello\n"
    assert "deny-term" in m._leak_in_diff(diff)


def test_leak_in_diff_catches_secret_token(monkeypatch):
    m = _load_runner()
    monkeypatch.setattr(m, "_repo_deny_terms", lambda name: [])
    diff = "+++ b/cfg.py\n+TOKEN = 'ghp_" + "a" * 36 + "'\n"
    assert "secret" in m._leak_in_diff(diff)


def test_leak_in_diff_ignores_removed_lines_and_header(monkeypatch):
    m = _load_runner()
    monkeypatch.setattr(m, "_repo_deny_terms", lambda name: ["gains.god.growth"])
    # deny-term only on a REMOVED line and in the +++ header path — must NOT trip (we scan added content)
    diff = "+++ b/gains.god.growth.txt\n-old gains.god.growth reference being deleted\n unchanged line\n"
    assert m._leak_in_diff(diff) == ""


def test_leak_in_diff_clean(monkeypatch):
    m = _load_runner()
    monkeypatch.setattr(m, "_repo_deny_terms", lambda name: ["gains.god.growth"])
    assert m._leak_in_diff("+++ b/x\n+a normal harmless code change\n") == ""
    assert m._leak_in_diff("") == ""


# ---- 2. _redact deny_terms --------------------------------------------------
def test_redact_scrubs_deny_terms(monkeypatch):
    m = _load_runner()
    monkeypatch.setattr(m, "_repo_deny_terms", lambda name: ["gains.god.growth"])
    out = m._redact("scheduled a reel for GAINS.GOD.GROWTH at 6am")
    assert "gains.god.growth" not in out.lower()
    assert "[REDACTED]" in out


def test_redact_noop_without_deny_terms(monkeypatch):
    m = _load_runner()
    monkeypatch.setattr(m, "_repo_deny_terms", lambda name: [])
    assert m._redact("a perfectly ordinary summary") == "a perfectly ordinary summary"


# ---- 3. _git_add_all private-path exclusion ---------------------------------
@pytest.mark.skipif(not shutil.which("git"), reason="git not available")
def test_git_add_all_excludes_private_paths_on_public(tmp_path, monkeypatch):
    m = _load_runner()
    work = _mk_repo(tmp_path)
    (work / "normal.py").write_text("ok = 1")
    (work / "profiles").mkdir()
    (work / "profiles" / "ggg").mkdir()
    (work / "profiles" / "ggg" / "SOUL.md").write_text("PRIVATE BRAND IDENTITY")
    monkeypatch.setattr(m, "REPO", work)
    monkeypatch.setattr(m, "NAME", "sover")
    monkeypatch.setattr(m, "_repo_is_public", lambda name: True)
    monkeypatch.setattr(m, "_repo_private_paths", lambda name: ["profiles/ggg"])
    m._git_add_all()
    staged = _git(work, "diff", "--cached", "--name-only").stdout
    assert "normal.py" in staged
    assert "profiles/ggg/SOUL.md" not in staged   # private path excluded even though NOT gitignored


@pytest.mark.skipif(not shutil.which("git"), reason="git not available")
def test_git_add_all_stages_everything_on_private(tmp_path, monkeypatch):
    m = _load_runner()
    work = _mk_repo(tmp_path)
    (work / "normal.py").write_text("ok = 1")
    (work / "anything.txt").write_text("x")
    monkeypatch.setattr(m, "REPO", work)
    monkeypatch.setattr(m, "NAME", "asmodeus")
    monkeypatch.setattr(m, "_repo_is_public", lambda name: False)   # private repo -> plain git add -A
    monkeypatch.setattr(m, "_repo_private_paths", lambda name: ["profiles/ggg"])
    m._git_add_all()
    staged = _git(work, "diff", "--cached", "--name-only").stdout
    assert "normal.py" in staged and "anything.txt" in staged
