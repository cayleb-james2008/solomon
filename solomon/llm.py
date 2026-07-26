"""LLM client — OpenAI-compatible, works with cloud GLM / OpenRouter / local Ornith."""
import base64
import json
from dataclasses import dataclass
from openai import AsyncOpenAI

from .config import Config


@dataclass
class LLMResponse:
    text: str
    usage: dict
    model: str


class LLMClient:
    """Thin wrapper over OpenAI-compatible chat completions."""

    def __init__(self, cfg: Config):
        self.cfg = cfg
        self.client = AsyncOpenAI(
            base_url=cfg.llm_base_url,
            api_key=cfg.llm_api_key or "sk-no-key",
        )
        self.model = cfg.llm_model

    async def chat(self, messages: list[dict], temperature: float = 0.7, max_tokens: int = 2000) -> LLMResponse:
        """Send a chat completion request. messages = [{"role": ..., "content": ...}, ...]"""
        resp = await self.client.chat.completions.create(
            model=self.model,
            messages=messages,
            temperature=temperature,
            max_tokens=max_tokens,
        )
        msg = resp.choices[0].message
        # Reasoning models (e.g. Ornith) may put output in reasoning_content when
        # content is empty (thinking ate max_tokens). Fall back so callers get text.
        text = msg.content or getattr(msg, "reasoning_content", "") or ""
        return LLMResponse(
            text=text,
            usage=resp.usage.model_dump() if resp.usage else {},
            model=resp.model or self.model,
        )

    async def ask(self, system: str, user: str, temperature: float = 0.7, max_tokens: int = 2000) -> str:
        """Convenience: system+user → text response."""
        resp = await self.chat(
            messages=[
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            temperature=temperature,
            max_tokens=max_tokens,
        )
        return resp.text

    async def ask_json(self, system: str, user: str) -> dict:
        """Ask the LLM for a JSON response. Returns parsed dict."""
        messages = [
            {"role": "system", "content": system + "\nRespond with valid JSON only, no markdown fences."},
            {"role": "user", "content": user},
        ]
        resp = await self.chat(messages, temperature=0.3, max_tokens=4000)
        text = resp.text.strip()
        # Strip markdown fences if present
        if text.startswith("```"):
            lines = text.split("\n")
            lines = [l for l in lines if not l.strip().startswith("```")]
            text = "\n".join(lines)
        try:
            return json.loads(text)
        except json.JSONDecodeError:
            # Try to extract JSON from the text
            start = text.find("{")
            end = text.rfind("}")
            if start != -1 and end != -1:
                return json.loads(text[start : end + 1])
            raise
