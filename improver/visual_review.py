"""Visual review orchestrator — boots the app in a sandbox, captures screenshots + a11y,
feeds them to a vision-capable pi agent, and produces a one-time feedback report.

Called by run_improver.py after the gate passes, BEFORE the branch ships. The feedback
is saved to runtime/<name>/visual_review/report.json and surfaced in the heartbeat + the
Solomon dashboard's Visual Review tab.

The flow:
  1. Boot the app in an ephemeral sandbox (sandbox.py)
  2. Drive the persistent agent-browser adapter over the configured pages
  3. Save screenshots to runtime/<name>/visual_review/
  4. Build a pi task with the screenshots (base64) + a11y trees + console/network errors
  5. Run the vision agent (visual_review.md contract) → parse findings
  6. Save report.json; return feedback text for the RSI loop
"""
from __future__ import annotations

import base64
import json
import os
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

from improver.agent_browser import AgentBrowser

# Import sibling modules — visual_review.py lives in improver/, same as run_improver.py
HERE = Path(__file__).resolve().parent
CONTROL = HERE.parent

# visual_review.md contract — the system prompt for the vision agent
VISUAL_REVIEW_MD = HERE / "visual_review.md"
VISION_EXT = HERE / "vision-cloud.ts"
CAPTURE_JS = HERE / "capture.js"  # legacy helper path; the active capture function uses AgentBrowser

_NO_WINDOW = 0x08000000 if sys.platform == "win32" else 0


def _now() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _ts_stamp() -> str:
    return datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S")


def _legacy_playwright_capture(base_url: str, pages: list, timeout: int = 60) -> dict | None:
    """Run capture.js via node, parse its JSON stdout output. Returns the capture result
    or None on failure (never raises — visual review is best-effort)."""
    import shutil
    node = shutil.which("node")
    if not node:
        return None
    pages_json = json.dumps(pages or ["/"])
    try:
        p = subprocess.run(
            [node, str(CAPTURE_JS), base_url, pages_json],
            capture_output=True, text=True, timeout=timeout,
            env=_clean_env_for_capture(), creationflags=_NO_WINDOW,
        )
    except (subprocess.TimeoutExpired, OSError):
        return None
    if p.returncode != 0:
        return None
    # parse the last non-empty line as JSON
    for line in reversed((p.stdout or "").splitlines()):
        line = line.strip()
        if line.startswith("{"):
            try:
                return json.loads(line)
            except json.JSONDecodeError:
                break
    return None


def _clean_env_for_capture() -> dict:
    """Env for the capture subprocess — strip secrets + python path pollution."""
    env = dict(os.environ)
    for k in ("PYTHONPATH", "PYTHONHOME", "GITHUB_TOKEN", "GH_TOKEN"):
        env.pop(k, None)
    return env


def _run_capture(base_url: str, pages: list, runtime_dir: Path,
                 repo_path: str | None = None) -> dict | None:
    """Capture pages through the persistent, policy-bounded agent-browser adapter."""
    captured = []
    try:
        with AgentBrowser(repo_path or str(CONTROL), Path(runtime_dir),
                          allowed_origins=[base_url]) as browser:
            for page in pages or ["/"]:
                path = str(page or "/")
                url = f"{base_url.rstrip('/')}/{path.lstrip('/')}"
                state = browser.navigate(url)
                item = {"path": path, "console_errors": state.get("consoleErrors") or [],
                        "network_errors": state.get("networkErrors") or []}
                if not state.get("ok"):
                    item["nav_error"] = state.get("error") or "navigation failed"
                    captured.append(item)
                    continue
                try:
                    item["screenshot_b64"] = base64.b64encode(
                        browser.frame_file.read_bytes()).decode("ascii")
                except OSError:
                    item["screenshot_b64"] = ""
                item["a11y_yaml"] = "\n".join(
                    f"- {element.get('role', 'element')}: {element.get('name', '')} "
                    f"[{element.get('ref', '')}]"
                    for element in state.get("elements") or [] if isinstance(element, dict)
                )
                captured.append(item)
        return {"ok": True, "pages": captured}
    except Exception:  # noqa: BLE001 - caller converts unavailable capture into a gate result
        return None


def _save_screenshots(capture_result: dict, out_dir: Path) -> list:
    """Save base64 screenshots to PNG files. Returns list of {path, page_path} dicts."""
    screenshots = []
    pages = capture_result.get("pages") or []
    for i, page_data in enumerate(pages):
        b64 = page_data.get("screenshot_b64")
        if not b64:
            continue
        fname = f"screenshot_{i:02d}.png"
        fpath = out_dir / fname
        try:
            fpath.write_bytes(base64.b64decode(b64))
            screenshots.append({
                "path": str(fpath),
                "page_path": page_data.get("path", "/"),
                "filename": fname,
            })
        except Exception:
            pass
    return screenshots


def _build_vision_task(capture_result: dict, iteration_summary: str, pages_config: list) -> str:
    """Build the pi task text for the vision agent — includes screenshots (base64),
    a11y trees, console/network errors, and the iteration summary."""
    parts = []
    parts.append(f"RSI ITERATION SUMMARY (what the coder changed):\n{iteration_summary}\n")
    parts.append(f"\nPAGES CAPTURED: {len(capture_result.get('pages', []))}\n")

    for page_data in capture_result.get("pages") or []:
        path = page_data.get("path", "/")
        parts.append(f"\n--- PAGE: {path} ---")
        # console errors
        console = page_data.get("console_errors") or []
        if console:
            parts.append("CONSOLE ERRORS:")
            for e in console[:10]:
                parts.append(f"  ! {e}")
        else:
            parts.append("CONSOLE ERRORS: none")
        # network errors
        network = page_data.get("network_errors") or []
        if network:
            parts.append("NETWORK ERRORS:")
            for e in network[:10]:
                parts.append(f"  ! {e}")
        else:
            parts.append("NETWORK ERRORS: none")
        # nav error
        nav_err = page_data.get("nav_error")
        if nav_err:
            parts.append(f"NAVIGATION ERROR: {nav_err}")
        # a11y tree (trimmed to keep prompt manageable)
        a11y = (page_data.get("a11y_yaml") or "").strip()
        if a11y:
            # trim to ~2000 chars per page
            if len(a11y) > 2000:
                a11y = a11y[:2000] + "\n... (truncated)"
            parts.append(f"ACCESSIBILITY TREE:\n{a11y}")
        # screenshot reference — the vision model sees the image via pi's image input
        b64 = page_data.get("screenshot_b64")
        if b64:
            parts.append(f"SCREENSHOT: [base64 PNG, {len(b64)} chars — attached as image input]")
        else:
            parts.append("SCREENSHOT: (capture failed for this page)")

    parts.append("\n\nReview this app per the visual_review.md contract. Output your findings.")
    return "\n".join(parts)


def _parse_findings(text: str) -> list:
    """Parse the agent's ===FINDINGS=== block into a list of finding dicts."""
    findings = []
    in_block = False
    for line in (text or "").splitlines():
        s = line.strip()
        if s == "===FINDINGS===":
            in_block = True
            continue
        if s == "===END===":
            break
        if in_block and "|" in s:
            parts = [p.strip() for p in s.split("|", 2)]
            if len(parts) >= 3:
                findings.append({
                    "severity": parts[0].lower(),
                    "category": parts[1].lower(),
                    "description": parts[2],
                })
    return findings


def _parse_summary(text: str) -> str:
    """Extract the SUMMARY: line from the agent output."""
    for line in (text or "").splitlines():
        s = line.strip()
        if s.upper().startswith("SUMMARY:"):
            return s[len("SUMMARY:"):].strip()
    return ""


def run(repo_path: str, runtime_dir: Path, sandbox_config: dict,
        vision_model: str, iteration_summary: str, provider_key: str = "OLLAMA_API_KEY",
        log_fn=None) -> dict:
    """Main entry: boot sandbox → capture → vision agent → save report.

    Returns {ok, report_path, findings, summary, feedback} or {ok:false, error}.
    Never raises — visual review is best-effort; the RSI loop must not break if it fails.
    """
    log = log_fn or (lambda msg: None)
    out_dir = runtime_dir / "visual_review"
    out_dir.mkdir(parents=True, exist_ok=True)

    pages = sandbox_config.get("pages") or ["/"]
    launch = sandbox_config.get("launch") or ""
    if not launch:
        return {"ok": False, "error": "sandbox config has no 'launch' command"}

    # 1. Boot sandbox
    from sandbox import Sandbox
    log("visual review: booting sandbox...")
    try:
        with Sandbox(sandbox_config, repo_path) as sb:
            log(f"visual review: sandbox healthy at {sb.base_url}")

            # 2. Capture
            log("visual review: capturing pages...")
            capture_result = _run_capture(sb.base_url, pages,
                                          runtime_dir / "app_test_capture", repo_path)
            if not capture_result:
                return {"ok": False, "error": "capture failed (agent-browser unavailable)"}

            # 3. Save screenshots
            screenshots = _save_screenshots(capture_result, out_dir)
            log(f"visual review: saved {len(screenshots)} screenshots")

            # 4. Build + run vision agent
            if not vision_model:
                # No vision model configured — save capture artifacts, skip agent
                report = {
                    "ts": _now(), "ok": True, "findings": [],
                    "summary": "Visual review ran but no vision model configured — screenshots saved.",
                    "screenshots": screenshots, "capture": capture_result,
                    "feedback": "",
                }
                _save_report(report, out_dir)
                return report

            if not os.environ.get(provider_key):
                log("visual review: no API key — skipping vision agent")
                report = {
                    "ts": _now(), "ok": True, "findings": [],
                    "summary": "No API key for vision model — screenshots saved without agent review.",
                    "screenshots": screenshots, "capture": capture_result,
                    "feedback": "",
                }
                _save_report(report, out_dir)
                return report

            task = _build_vision_task(capture_result, iteration_summary, pages)
            log("visual review: running vision agent...")
            agent_text = _run_vision_agent(task, vision_model, sandbox_config, capture_result)
            findings = _parse_findings(agent_text)
            summary = _parse_summary(agent_text) or agent_text[:200]

            # 5. Build feedback for the RSI loop
            feedback = _build_feedback(findings, summary)

            report = {
                "ts": _now(), "ok": True, "findings": findings,
                "summary": summary, "screenshots": screenshots,
                "capture": {"pages": len(capture_result.get("pages", [])),
                            "console_errors": sum(len(p.get("console_errors") or []) for p in capture_result.get("pages", [])),
                            "network_errors": sum(len(p.get("network_errors") or []) for p in capture_result.get("pages", []))},
                "feedback": feedback,
                "agent_raw": agent_text[:2000] if agent_text else "",
            }
            _save_report(report, out_dir)
            log(f"visual review: complete — {len(findings)} findings, feedback: {feedback[:100]}")
            return report

    except Exception as e:
        log(f"visual review: failed — {str(e)[:200]}")
        return {"ok": False, "error": str(e)[:300]}


def _run_vision_agent(task: str, vision_model: str, sandbox_config: dict,
                      capture_result: dict) -> str:
    """Run the pi vision agent. Returns the agent's final text output."""
    import shutil
    pi = shutil.which("pi")
    if not pi:
        return ""

    args = [pi, "--print", "--mode", "json",
            "--provider", "vision-cloud", "--model", vision_model,
            "-e", str(VISION_EXT),
            "--append-system-prompt", str(VISUAL_REVIEW_MD),
            "--no-tools",  # visual review is read-only analysis — no file/bash tools
            task]

    env = dict(os.environ)
    env["RSI_VISION_MODEL"] = vision_model
    env.pop("PYTHONPATH", None)
    env.pop("PYTHONHOME", None)

    try:
        p = subprocess.run(args, capture_output=True, text=True, encoding="utf-8",
                           errors="replace", env=env, timeout=120, creationflags=_NO_WINDOW)
    except (subprocess.TimeoutExpired, OSError):
        return ""

    # Extract the last assistant text from pi --mode json output
    return _final_text(p.stdout or "")


def _final_text(stdout: str) -> str:
    """Extract the last assistant text from a pi --mode json event stream."""
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


def _build_feedback(findings: list, summary: str) -> str:
    """Build the one-time feedback text that gets injected into the next RSI iteration."""
    if not findings:
        return ""
    critical = [f for f in findings if f.get("severity") == "critical"]
    warnings = [f for f in findings if f.get("severity") == "warning"]
    if not critical and not warnings:
        return ""  # only info/pass — no actionable feedback

    parts = ["VISUAL REVIEW FEEDBACK (from the previous iteration's E2E sandbox review):"]
    if critical:
        parts.append("CRITICAL issues found:")
        for f in critical:
            parts.append(f"  - [{f['category']}] {f['description']}")
    if warnings:
        parts.append("Warnings:")
        for f in warnings:
            parts.append(f"  - [{f['category']}] {f['description']}")
    parts.append(f"\nReview summary: {summary}")
    parts.append("Address the most critical visual/functional issue above in this iteration if it "
                 "falls within the current backlog item's scope. Otherwise, note it for a future item.")
    return "\n".join(parts)


def _save_report(report: dict, out_dir: Path) -> None:
    """Save report.json (without the heavy capture data or base64)."""
    slim = {k: v for k, v in report.items() if k not in ("capture", "agent_raw")}
    try:
        (out_dir / "report.json").write_text(json.dumps(slim, indent=2), encoding="utf-8")
    except OSError:
        pass
