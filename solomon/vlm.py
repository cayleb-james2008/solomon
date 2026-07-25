"""VLM fallback — converts screenshots to text descriptions for text-only main LLMs.

If the main LLM is vision-capable, the VLM is bypassed and the image is sent
directly to the main model. Otherwise, the screenshot is sent to a small VLM
(MiniCPM-V-2.6 via llama.cpp local, or cloud vision API fallback) and the
text description is returned for the main LLM to use.
"""
import base64
import json
from pathlib import Path
from typing import Optional

from openai import AsyncOpenAI

from .config import Config
from .llm import LLMClient


class VLMFallback:
    """Screenshot → text description, for text-only main LLMs."""

    def __init__(self, cfg: Config):
        self.cfg = cfg
        self.vlm_client = AsyncOpenAI(
            base_url=cfg.vlm_base_url,
            api_key=cfg.vlm_api_key or "sk-no-key-needed",
        )
        self.vlm_model = cfg.vlm_model
        self._main_llm: Optional[LLMClient] = None

    def set_main_llm(self, llm: LLMClient):
        """Inject the main LLM for vision-capability detection."""
        self._main_llm = llm

    async def describe_screenshot(self, screenshot_path: str, question: str = "Describe what you see on this page in detail. What elements are interactive? What is the current state of the page?") -> str:
        """Take a screenshot file, return a text description.

        If the main LLM is vision-capable (detected by model name), it handles
        the image directly. Otherwise, the VLM fallback is used.
        """
        # Read screenshot as base64
        screenshot = Path(screenshot_path)
        if not screenshot.exists():
            return f"[ERROR: screenshot not found at {screenshot_path}]"

        with open(screenshot, "rb") as f:
            image_b64 = base64.b64encode(f.read()).decode("utf-8")

        # Determine if main LLM is vision-capable
        if self._main_llm and self._is_vision_capable(self._main_llm.model):
            return await self._describe_with_main_llm(image_b64, question)

        # Use VLM fallback
        return await self._describe_with_vlm(image_b64, question)

    def _is_vision_capable(self, model: str) -> bool:
        """Heuristic: check if the main LLM model name indicates vision capability."""
        vision_keywords = ["vision", "vlm", "gpt-4o", "gpt-4-turbo", "claude-3", "gemini", "qwen2-vl", "qwen2.5-vl", "minicpm-v", "llama-3.2-vision", "glm-4v", "glm-4.5v", "glm-5"]
        model_lower = model.lower()
        return any(k in model_lower for k in vision_keywords)

    async def _describe_with_vlm(self, image_b64: str, question: str) -> str:
        """Send image to the VLM and return text description."""
        try:
            resp = await self.vlm_client.chat.completions.create(
                model=self.vlm_model,
                messages=[
                    {
                        "role": "user",
                        "content": [
                            {"type": "text", "text": question},
                            {
                                "type": "image_url",
                                "image_url": {
                                    "url": f"data:image/png;base64,{image_b64}",
                                },
                            },
                        ],
                    }
                ],
                max_tokens=500,
                temperature=0.3,
            )
            return resp.choices[0].message.content or "[VLM returned empty response]"
        except Exception as e:
            return f"[VLM ERROR: {e}]"

    async def _describe_with_main_llm(self, image_b64: str, question: str) -> str:
        """Send image directly to the vision-capable main LLM."""
        if not self._main_llm:
            return "[ERROR: main LLM not set]"
        try:
            resp = await self._main_llm.client.chat.completions.create(
                model=self._main_llm.model,
                messages=[
                    {
                        "role": "user",
                        "content": [
                            {"type": "text", "text": question},
                            {
                                "type": "image_url",
                                "image_url": {
                                    "url": f"data:image/png;base64,{image_b64}",
                                },
                            },
                        ],
                    }
                ],
                max_tokens=500,
                temperature=0.3,
            )
            return resp.choices[0].message.content or "[Main LLM returned empty response]"
        except Exception as e:
            # If the main LLM claims vision but fails, fall back to VLM
            return await self._describe_with_vlm(image_b64, question)
