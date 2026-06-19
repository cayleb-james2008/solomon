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

