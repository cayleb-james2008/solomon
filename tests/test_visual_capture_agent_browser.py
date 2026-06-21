import json
import sys
import types

from improver import visual_review


def test_capture_uses_persistent_agent_browser(tmp_path, monkeypatch):
    calls = []

    class FakeBrowser:
        def __init__(self, repo_path, runtime_dir, allowed_origins):
            self.frame_file = runtime_dir / "browser_frame.jpg"
            self.frame_file.parent.mkdir(parents=True, exist_ok=True)
            self.frame_file.write_bytes(b"jpeg")

        def __enter__(self): return self
        def __exit__(self, *args): return False

        def navigate(self, url):
            calls.append(url)
            # 'frame' present == this page's screenshot CLI succeeded (real observe() sets it only then);
            # _run_capture now reads frame_file ONLY when 'frame' is set, so the fixture must include it.
            return {"ok": True, "url": url, "frame": {"available": True},
                    "elements": [{"ref": "@e1", "role": "button", "name": "Run"}]}

    monkeypatch.setattr(visual_review, "AgentBrowser", FakeBrowser)
    result = visual_review._run_capture("http://127.0.0.1:4000", ["/", "/settings"], tmp_path)
    assert result["ok"] is True
    assert calls == ["http://127.0.0.1:4000/", "http://127.0.0.1:4000/settings"]
    assert len(result["pages"]) == 2
    assert result["pages"][0]["screenshot_b64"]
    assert "button" in result["pages"][0]["a11y_yaml"]


def test_capture_returns_none_when_no_screenshots(tmp_path, monkeypatch):
    # every page failing to capture (e.g. agent-browser binary missing -> every navigate fails) must
    # yield None, not ok:True with zero screenshots — else the mandatory visual gate passes blind.
    class FailBrowser:
        def __init__(self, *a, **k):
            self.frame_file = tmp_path / "f.jpg"

        def __enter__(self): return self
        def __exit__(self, *a): return False

        def navigate(self, url):
            return {"ok": False, "error": "agent-browser 0.27.0 is not installed"}

    monkeypatch.setattr(visual_review, "AgentBrowser", FailBrowser)
    assert visual_review._run_capture("http://127.0.0.1:4000", ["/", "/x"], tmp_path) is None


def test_capture_skips_stale_frame_when_screenshot_failed(tmp_path, monkeypatch):
    # page A succeeds (frame present); page B navigates ok but its screenshot CLI failed (no 'frame').
    # B must NOT inherit A's reused frame image — its screenshot_b64 stays empty.
    class MixedBrowser:
        def __init__(self, *a, **k):
            self.frame_file = tmp_path / "fr.jpg"
            self.frame_file.write_bytes(b"PAGE_A_IMAGE")

        def __enter__(self): return self
        def __exit__(self, *a): return False

        def navigate(self, url):
            if url.endswith("/a"):
                return {"ok": True, "frame": {"available": True}, "elements": []}
            return {"ok": True, "elements": []}            # page B: ok but NO frame (screenshot failed)

    monkeypatch.setattr(visual_review, "AgentBrowser", MixedBrowser)
    res = visual_review._run_capture("http://127.0.0.1:4000", ["/a", "/b"], tmp_path)
    assert res["ok"] is True
    a = next(p for p in res["pages"] if p["path"] == "/a")
    b = next(p for p in res["pages"] if p["path"] == "/b")
    assert a["screenshot_b64"]                              # page A's fresh frame embedded
    assert b["screenshot_b64"] == ""                       # page B did NOT inherit A's image


def test_visual_review_uses_unified_provider_shim():
    """§4.2: the vision reviewer points at the single unified provider.ts shim."""
    assert visual_review.VISION_EXT.name == "provider.ts"


def test_vision_agent_returns_none_when_pi_missing(monkeypatch):
    """The vision agent returns None (not '') when pi is unavailable — a failed-to-run review must be
    distinguishable from a clean pass so the caller can surface ok:false."""
    import shutil
    monkeypatch.setattr(shutil, "which", lambda name: None)
    assert visual_review._run_vision_agent("task", "model-x", {}, {}) is None


def _agent_end_event(text):
    """A one-line pi --mode json stream whose final assistant text is `text`."""
    return json.dumps({"type": "agent_end", "messages": [
        {"role": "assistant", "content": [{"type": "text", "text": text}]}]})


def test_vision_agent_retry_loop_returns_none_when_both_empty(monkeypatch):
    """The retry loop runs TWICE (120s then 180s); both empty -> None (review failed to run)."""
    import shutil
    monkeypatch.setattr(shutil, "which", lambda name: "pi")
    timeouts = []

    def fake_run(args, **kw):
        timeouts.append(kw.get("timeout"))

        class R:
            stdout = ""
            stderr = ""
            returncode = 0
        return R()

    monkeypatch.setattr(visual_review.subprocess, "run", fake_run)
    assert visual_review._run_vision_agent("task", "vm", {}, {}) is None
    assert timeouts == [120, 180]                       # retried once with the longer timeout


def test_vision_agent_retry_succeeds_after_first_timeout(monkeypatch):
    """A first-attempt timeout is retried with the longer 180s timeout; a non-empty 2nd attempt wins."""
    import shutil
    import subprocess
    monkeypatch.setattr(shutil, "which", lambda name: "pi")
    ev = _agent_end_event("SUMMARY: looks good")
    timeouts = []
    state = {"n": 0}

    def fake_run(args, **kw):
        timeouts.append(kw.get("timeout"))
        state["n"] += 1
        if state["n"] == 1:
            raise subprocess.TimeoutExpired("pi", 120)

        class R:
            stdout = ev
            stderr = ""
            returncode = 0
        return R()

    monkeypatch.setattr(visual_review.subprocess, "run", fake_run)
    out = visual_review._run_vision_agent("task", "vm", {}, {})
    assert out and "looks good" in out
    assert timeouts == [120, 180]


def test_vision_agent_first_attempt_text_no_retry(monkeypatch):
    """A non-empty first attempt returns immediately (no retry)."""
    import shutil
    monkeypatch.setattr(shutil, "which", lambda name: "pi")
    ev = _agent_end_event("===FINDINGS===\ncritical|layout|x\n===END===")
    timeouts = []

    def fake_run(args, **kw):
        timeouts.append(kw.get("timeout"))

        class R:
            stdout = ev
            stderr = ""
            returncode = 0
        return R()

    monkeypatch.setattr(visual_review.subprocess, "run", fake_run)
    out = visual_review._run_vision_agent("task", "vm", {}, {})
    assert out and "FINDINGS" in out
    assert timeouts == [120]                            # no retry needed


def test_run_surfaces_failed_review_when_agent_empty(tmp_path, monkeypatch):
    """When the vision agent produces no output, run() returns ok:false with a clear error (NOT a
    clean pass with empty findings) and writes that to report.json — a review-failed-to-run is no
    longer indistinguishable from a review-passed-clean."""
    class _FakeSandbox:
        def __init__(self, config, repo_path):
            self.base_url = "http://127.0.0.1:9"

        def __enter__(self):
            return self

        def __exit__(self, *a):
            return False

    monkeypatch.setitem(sys.modules, "sandbox", types.SimpleNamespace(Sandbox=_FakeSandbox))
    monkeypatch.setattr(visual_review, "_run_capture",
                        lambda *a, **k: {"ok": True, "pages": [
                            {"path": "/", "screenshot_b64": "", "console_errors": [],
                             "network_errors": []}]})
    monkeypatch.setattr(visual_review, "_run_vision_agent", lambda *a, **k: None)  # the failure under test
    monkeypatch.setenv("OLLAMA_API_KEY", "k")
    out = visual_review.run(
        repo_path=str(tmp_path), runtime_dir=tmp_path,
        sandbox_config={"launch": "echo hi", "pages": ["/"]},
        vision_model="vision-x", iteration_summary="changed a button",
        provider_key="OLLAMA_API_KEY", log_fn=lambda m: None,
    )
    assert out["ok"] is False
    assert "no output" in out["error"]
    assert out["findings"] == []
    report = json.loads((tmp_path / "visual_review" / "report.json").read_text(encoding="utf-8"))
    assert report["ok"] is False and "no output" in report["error"]

