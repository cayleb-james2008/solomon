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
            return {"ok": True, "url": url, "elements": [{"ref": "@e1", "role": "button",
                                                              "name": "Run"}]}

    monkeypatch.setattr(visual_review, "AgentBrowser", FakeBrowser)
    result = visual_review._run_capture("http://127.0.0.1:4000", ["/", "/settings"], tmp_path)
    assert result["ok"] is True
    assert calls == ["http://127.0.0.1:4000/", "http://127.0.0.1:4000/settings"]
    assert len(result["pages"]) == 2
    assert result["pages"][0]["screenshot_b64"]
    assert "button" in result["pages"][0]["a11y_yaml"]


def test_visual_review_uses_unified_provider_shim():
    """§4.2: the vision reviewer points at the single unified provider.ts shim."""
    assert visual_review.VISION_EXT.name == "provider.ts"


def test_vision_agent_returns_none_when_pi_missing(monkeypatch):
    """The vision agent returns None (not '') when pi is unavailable — a failed-to-run review must be
    distinguishable from a clean pass so the caller can surface ok:false."""
    import shutil
    monkeypatch.setattr(shutil, "which", lambda name: None)
    assert visual_review._run_vision_agent("task", "model-x", {}, {}) is None


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

