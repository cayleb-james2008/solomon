"""CEO routing tests for deterministic profit safeguards."""
from solomon.ceo import CEO


def test_ready_spotlight_overrides_speculative_llm_choice():
    decision = {"channel": "ai_wrapper", "action": "build", "reasoning": "new idea"}
    observations = {
        "content": {
            "opportunities": [{"topics": [], "spotlight": {"tool": {"name": "mood-analyzer"}}}]
        },
        "ai_wrapper": {"summary": "new tool idea"},
    }
    routed = CEO._enforce_profit_priority(decision, observations)
    assert routed["channel"] == "content"
    assert routed["action"] == "publish_tool_spotlight"


def test_no_spotlight_preserves_llm_choice():
    decision = {"channel": "ai_wrapper", "action": "build"}
    observations = {"content": {"opportunities": [{"topics": [{"title": "topic"}]}]}}
    assert CEO._enforce_profit_priority(decision, observations) == decision