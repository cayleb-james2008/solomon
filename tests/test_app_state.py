"""app.py settings persistence: atomic write + durable ok-propagation for the safety dials.

The auto_push / auto_ai_fix dials are operator opt-in safety gates; a silent revert (false ok:True or
a half-written .solomon.json) re-enables unattended push/PR the operator turned off.
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import app  # noqa: E402


def test_set_auto_push_reports_false_when_write_fails(tmp_path, monkeypatch):
    # _save_state swallows the OSError but must REPORT it: the setter returns ok:False (not a false True)
    # so the UI rollback fires instead of the dial silently reverting to its permissive default next launch.
    bad_parent = tmp_path / "afile"
    bad_parent.write_text("x", encoding="utf-8")             # a regular file -> open(child, "w") raises
    monkeypatch.setattr(app, "_STATE_FILE", str(bad_parent / "state.json"))
    monkeypatch.setattr(app, "_LEGACY_STATE_FILE", str(tmp_path / "legacy.json"))   # don't read real state
    res = app.Api().set_auto_push(False)
    assert res["ok"] is False and res["auto_push"] is False


def test_save_state_is_atomic_no_truncation_on_failure(tmp_path, monkeypatch):
    # a present good state file must survive a FAILED write intact (atomic tmp + os.replace, never a
    # truncate-in-place that would leave a half-written file that next launch reads as {} -> defaults).
    sf = tmp_path / "state.json"
    sf.write_text('{"auto_push": false}', encoding="utf-8")
    monkeypatch.setattr(app, "_STATE_FILE", str(sf))
    monkeypatch.setattr(app.os, "replace",
                        lambda *a, **k: (_ for _ in ()).throw(OSError("replace failed")))
    assert app._save_state({"auto_push": True}) is False
    assert sf.read_text(encoding="utf-8") == '{"auto_push": false}'   # original intact, not truncated


def test_set_auto_push_persists_and_reads_back(tmp_path, monkeypatch):
    # the happy path: ok:True and the value survives to the next launch (a fresh Api()).
    sf = tmp_path / "state.json"
    monkeypatch.setattr(app, "_STATE_FILE", str(sf))
    monkeypatch.setattr(app, "_LEGACY_STATE_FILE", str(tmp_path / "legacy.json"))   # don't read real state
    res = app.Api().set_auto_push(False)
    assert res["ok"] is True and res["auto_push"] is False
    assert app.Api().get_auto_push() is False
