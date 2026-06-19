"""Tests for per-phase model/provider/reasoning selection (_apply_phase_config).

  PH-1  light phase (beautify) smart default -> cheap worker model + low reasoning
  PH-2  deep phase (implement) with no override keeps the repo's strong model + reasoning
  PH-3  explicit phases.<phase>.{model,reasoning} override wins
  PH-4  explicit phases.<phase>.provider switches provider (+ that provider's default model)
"""
import importlib.util
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

RUNNER = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                      "improver", "run_improver.py")


def _load_runner():
    spec = importlib.util.spec_from_file_location("run_improver", RUNNER)
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


def test_light_phase_smart_default_cheap_model():
    m = _load_runner()
    m.PHASE = "beautify"
    m.PI_PROVIDER, m.PI_MODEL, m.REASONING = "maki-cloud", "glm-5.2", "xhigh"
    m._apply_phase_config({"name": "x", "provider": "ollama-cloud", "model": "glm-5.2", "reasoning": "xhigh"})
    assert m.PI_MODEL == "minimax-m3"   # light phase -> cheap worker model
    assert m.REASONING == "low"


def test_deep_phase_keeps_repo_config():
    m = _load_runner()
    m.PHASE = "implement"
    m.PI_PROVIDER, m.PI_MODEL, m.REASONING = "maki-cloud", "glm-5.2", "high"
    m._apply_phase_config({"name": "x", "provider": "ollama-cloud", "model": "glm-5.2", "reasoning": "high"})
    assert m.PI_MODEL == "glm-5.2"      # deep phase -> unchanged
    assert m.REASONING == "high"


def test_explicit_phase_override_wins():
    m = _load_runner()
    m.PHASE = "implement"
    m.PI_PROVIDER, m.PI_MODEL, m.REASONING = "maki-cloud", "glm-5.2", "xhigh"
    row = {"name": "x", "provider": "ollama-cloud", "model": "glm-5.2",
           "phases": {"implement": {"model": "qwen/qwen3-coder", "reasoning": "medium"}}}
    m._apply_phase_config(row)
    assert m.PI_MODEL == "qwen/qwen3-coder"
    assert m.REASONING == "medium"


def test_explicit_phase_provider_switch():
    m = _load_runner()
    m.PHASE = "review"
    m.PI_PROVIDER, m.PI_MODEL, m.REASONING = "maki-cloud", "glm-5.2", "xhigh"
    row = {"name": "x", "provider": "ollama-cloud", "phases": {"review": {"provider": "openrouter"}}}
    m._apply_phase_config(row)
    assert m.PI_PROVIDER == "openrouter"            # provider switched
    assert m.PI_MODEL == "qwen/qwen3-coder"         # openrouter's default model (no phase.model given)
