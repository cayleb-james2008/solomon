"""CEO loop — observe→think→act→log.

The CEO is the brain. Each cycle:
1. OBSERVE: Launch the browser, check each active channel's current state.
2. THINK: Ask the LLM which channel to prioritize and what action to take.
3. ACT: Execute the chosen channel's action (with money guard).
4. LOG: Record the outcome, check for revenue events.
"""
import asyncio
import json
from datetime import datetime, timezone

from rich.console import Console
from rich.panel import Panel
from rich.table import Table

from .config import Config
from .llm import LLMClient
from .vlm import VLMFallback
from .browser import StealthBrowser
from .guard import guard, MoneyGuardError
from .ledger import Ledger

console = Console()


class CEO:
    """The autonomous CEO — picks channels, drives actions, collects revenue."""

    def __init__(self, cfg: Config):
        self.cfg = cfg
        self.llm = LLMClient(cfg)
        self.vlm = VLMFallback(cfg)
        self.vlm.set_main_llm(self.llm)
        self.browser = StealthBrowser(cfg)
        self.ledger = Ledger(cfg.revenue_ledger_path)
        self.channels = {}
        self._cycle_count = 0

    def register_channel(self, name: str, channel):
        """Register an income channel."""
        self.channels[name] = channel

    async def run(self, once: bool = False, dry: bool = False):
        """Run the CEO loop."""
        max_cycles = 1 if once else self.cfg.max_cycles
        if max_cycles == 0:
            max_cycles = float("inf")

        if dry:
            console.print(Panel("[yellow]DRY RUN — no browser, no network[/yellow]", title="Solomon v2"))
            await self._run_cycle_dry()
            return

        # Launch browser
        console.print("[dim]Launching stealth browser...[/dim]")
        try:
            await self.browser.launch()
        except Exception as e:
            console.print(f"[red]Browser launch failed: {e}[/red]")
            console.print("[yellow]Falling back to dry run.[/yellow]")
            await self._run_cycle_dry()
            return

        try:
            while self._cycle_count < max_cycles:
                await self._run_cycle()
                self._cycle_count += 1
                if self._cycle_count < max_cycles:
                    console.print(f"[dim]Sleeping {self.cfg.cycle_sleep}s before next cycle...[/dim]")
                    await asyncio.sleep(self.cfg.cycle_sleep)
        finally:
            await self.browser.close()
            self._print_dashboard()

    async def _run_cycle(self):
        """One observe→think→act→log cycle."""
        console.print(Panel(f"[cyan]CEO Cycle {self._cycle_count + 1}[/cyan]", title="Solomon v2"))

        # 1. OBSERVE — check each channel
        observations = {}
        for name, channel in self.channels.items():
            try:
                obs = await channel.discover(self.browser, self.llm, self.vlm)
                observations[name] = obs
                console.print(f"  [green]●[/green] {name}: {obs.get('summary', 'no summary')}")
            except Exception as e:
                observations[name] = {"summary": f"error: {e}", "error": str(e)}
                console.print(f"  [red]✗[/red] {name}: {e}")

        # 2. THINK — ask the LLM which channel to prioritize
        decision = await self._think(observations)
        console.print(f"  [blue]→[/blue] Decision: {decision.get('channel', 'none')} — {decision.get('action', 'none')}")

        # 3. ACT — execute the chosen channel's action (money guard checks inside)
        channel_name = decision.get("channel")
        if channel_name and channel_name in self.channels:
            try:
                result = await self.channels[channel_name].act(
                    self.browser, self.llm, self.vlm, decision
                )
                console.print(f"  [green]✓[/green] Result: {result.get('summary', 'done')}")

                # 4. LOG — check for revenue
                if result.get("revenue_usd"):
                    self.ledger.log_revenue(
                        channel=channel_name,
                        amount_usd=result["revenue_usd"],
                        source=result.get("source", "unknown"),
                        note=result.get("note", ""),
                    )
                    console.print(f"  [bold green]💰 Revenue logged: ${result['revenue_usd']:.2f}[/bold green]")
            except MoneyGuardError as e:
                console.print(f"  [bold red]🛑 MONEY GUARD: {e}[/bold red]")
            except Exception as e:
                console.print(f"  [red]✗ Action failed: {e}[/red]")
        else:
            console.print("  [yellow]○ No action this cycle[/yellow]")

    async def _run_cycle_dry(self):
        """Dry run — no browser, just LLM + channel status."""
        console.print("[dim]Checking channel configs...[/dim]")
        for name in self.channels:
            console.print(f"  [green]●[/green] {name}: configured")
        console.print(f"\n[dim]Revenue total: ${self.ledger.get_total():.2f}[/dim]")
        console.print("[green]✓ Dry run complete. All systems configured.[/green]")

    async def _think(self, observations: dict) -> dict:
        """Ask the LLM which channel to prioritize and what action to take."""
        channel_list = "\n".join(
            f"- {name}: {obs.get('summary', 'no data')}"
            for name, obs in observations.items()
        )

        system = """You are Solomon, an autonomous profit-focused CEO. Your job is to pick the income channel with the highest expected value this cycle and decide what action to take.

Rules:
- You NEVER spend money. You only collect.
- You prefer channels with active opportunities over idle ones.
- If a channel has an actionable opportunity, prioritize it.
- If no channel has an immediate opportunity, return {"channel": null, "action": "wait"}.

Respond with JSON: {"channel": "<name or null>", "action": "<description>", "reasoning": "<one sentence>"}"""

        user = f"Current channel observations:\n{channel_list}\n\nWhich channel should we act on this cycle?"

        try:
            return await self.llm.ask_json(system, user)
        except Exception as e:
            console.print(f"  [yellow]LLM think failed ({e}), defaulting to wait[/yellow]")
            return {"channel": None, "action": "wait", "reasoning": "LLM error"}

    def _print_dashboard(self):
        """Print the revenue dashboard."""
        total = self.ledger.get_total()
        recent = self.ledger.get_recent(5)

        table = Table(title="Solomon v2 — Revenue Dashboard", show_header=True)
        table.add_column("Timestamp", style="dim")
        table.add_column("Channel", style="cyan")
        table.add_column("Amount", justify="right", style="green")
        table.add_column("Source", style="yellow")
        table.add_column("Note")

        for row in recent:
            table.add_row(
                row.get("ts", "")[:19],
                row.get("channel", ""),
                f"${row.get('amount_usd', 0):.2f}",
                row.get("source", ""),
                row.get("note", "")[:40],
            )

        console.print()
        console.print(table)
        console.print(f"\n[bold green]Total Revenue: ${total:.2f}[/bold green]")
