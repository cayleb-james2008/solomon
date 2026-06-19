import json
from pathlib import Path

import app
import control
from improver import app_test_runtime


def test_detect_config_serves_web_directory(tmp_path):
    (tmp_path / "web").mkdir()
    (tmp_path / "web" / "index.html").write_text("ok", encoding="utf-8")
    config = app_test_runtime.detect_config({"name": "demo", "path": str(tmp_path)})
    assert config["launch"][1:3] == ["-m", "http.server"]
    assert config["launch"][-2:] == ["--directory", "web"]
    assert config["health"] == "/index.html"


def test_api_exposes_app_test_contract(monkeypatch, tmp_path):
    repo = {"name": "demo", "path": str(tmp_path)}
    monkeypatch.setattr(control, "load_repos", lambda: [repo])
    monkeypatch.setattr(control, "start_app_test", lambda value: {"ok": True, "name": value["name"]})
    monkeypatch.setattr(control, "stop_app_test", lambda value: {"ok": True})
    monkeypatch.setattr(control, "app_test_state", lambda value, after_seq=0:
                        {"ok": True, "seq": after_seq + 1})
    monkeypatch.setattr(control, "app_test_frame", lambda value, after_seq=0:
                        {"ok": True, "seq": after_seq + 1, "data": "abc"})
    monkeypatch.setattr(control, "read_app_test_report", lambda value: {"ok": True, "findings": []})
    api = app.Api()
    assert api.start_app_test("demo")["ok"]
    assert api.app_test_state("demo", 4)["seq"] == 5
    assert api.app_test_frame("demo", 7)["seq"] == 8
    assert api.read_app_test_report("demo")["findings"] == []
    assert api.stop_app_test("demo")["ok"]


def test_state_after_seq_returns_unchanged(tmp_path, monkeypatch):
    monkeypatch.setattr(control, "HERE", str(tmp_path))
    repo = {"name": "demo", "path": str(tmp_path)}
    runtime = tmp_path / "runtime" / "demo"
    runtime.mkdir(parents=True)
    (runtime / "browser_state.json").write_text(json.dumps({"ok": True, "seq": 9}), encoding="utf-8")
    assert control.app_test_state(repo, after_seq=9) == {"ok": True, "unchanged": True, "seq": 9}

