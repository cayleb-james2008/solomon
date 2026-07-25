"""Channel base — all income channels implement this interface."""
from abc import ABC, abstractmethod


class Channel(ABC):
    """An income channel with discover() + act() methods."""

    name: str = "base"

    @abstractmethod
    async def discover(self, browser, llm, vlm) -> dict:
        """Observe the channel's current state. Returns a summary dict.

        Returns: {"summary": str, "opportunities": list[dict], ...}
        """
        ...

    @abstractmethod
    async def act(self, browser, llm, vlm, decision: dict) -> dict:
        """Execute an action on this channel. Returns an outcome dict.

        Returns: {"summary": str, "revenue_usd": float | None, "source": str, "note": str}
        """
        ...
