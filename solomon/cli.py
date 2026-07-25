"""CLI dispatcher — routes solomon subcommands to the right handler."""
import asyncio
import os
import sys

from rich.console import Console
from rich.panel import Panel
from rich.table import Table

from .config import Config
from .ceo import CEO
from .guard import check_action, MONEY_OUT_KINDS, MONEY_IN_KINDS, MoneyGuardError
from .ledger import Ledger
from .llm import LLMClient
from .vlm import VLMFallback
from .browser import StealthBrowser


console = Console()


async def run_cli(args):
    cfg = Config.load()

    if args.command == "run":
        await _cmd_run(cfg, once=args.once, dry=args.dry)
    elif args.command == "loop":
        await _cmd_loop(cfg)
    elif args.command == "dashboard":
        await _cmd_dashboard(cfg)
    elif args.command == "test-browser":
        await _cmd_test_browser(cfg)
    elif args.command == "test-vlm":
        await _cmd_test_vlm(cfg)
    elif args.command == "test-llm":
        await _cmd_test_llm(cfg)
    elif args.command == "guard-check":
        await _cmd_guard_check(cfg)
    elif args.command == "webhook":
        _cmd_webhook(cfg)


async def _cmd_run(cfg: Config, once: bool, dry: bool):
    """Run the CEO loop."""
    ceo = CEO(cfg)

    # Register channels based on config
    if cfg.channel_freelance:
        from .channels.freelance import FreelanceChannel
        ceo.register_channel("freelance", FreelanceChannel())
    if cfg.channel_content:
        from .channels.content import ContentChannel
        ceo.register_channel("content", ContentChannel())
    if cfg.channel_microtask:
        from .channels.microtask import MicrotaskChannel
        ceo.register_channel("microtask", MicrotaskChannel())
    if cfg.channel_ai_wrapper:
        from .channels.ai_wrapper import AIWrapperChannel
        ceo.register_channel("ai_wrapper", AIWrapperChannel())

    if not ceo.channels:
        console.print("[red]No channels enabled. Check .env SOLOMON_CHANNEL_* settings.[/red]")
        return

    await ceo.run(once=once, dry=dry)


async def _cmd_loop(cfg: Config):
    """Run the CEO continuously — infinite cycles with sleep between."""
    console.print(Panel(
        f"[cyan]Solomon v2 — Continuous CEO Loop[/cyan]\n"
        f"  Cycle sleep: {cfg.cycle_sleep}s\n"
        f"  Channels: freelance={cfg.channel_freelance}, content={cfg.channel_content}, "
        f"microtask={cfg.channel_microtask}, ai_wrapper={cfg.channel_ai_wrapper}\n"
        f"  Auto-submit: {cfg.auto_submit}\n"
        f"  Press Ctrl+C to stop.",
        title="Solomon v2",
    ))
    ceo = CEO(cfg)
    if cfg.channel_freelance:
        from .channels.freelance import FreelanceChannel
        ceo.register_channel("freelance", FreelanceChannel())
    if cfg.channel_content:
        from .channels.content import ContentChannel
        ceo.register_channel("content", ContentChannel())
    if cfg.channel_microtask:
        from .channels.microtask import MicrotaskChannel
        ceo.register_channel("microtask", MicrotaskChannel())
    if cfg.channel_ai_wrapper:
        from .channels.ai_wrapper import AIWrapperChannel
        ceo.register_channel("ai_wrapper", AIWrapperChannel())
    if not ceo.channels:
        console.print("[red]No channels enabled.[/red]")
        return
    # Force infinite loop for the loop command
    ceo.cfg.max_cycles = 0
    await ceo.run(once=False, dry=False)


def _cmd_webhook(cfg: Config):
    """Start the Polar webhook listener for automatic revenue logging."""
    from .webhook import run_webhook_server
    port = int(os.getenv("SOLOMON_WEBHOOK_PORT", "8019"))
    run_webhook_server(port=port)


async def _cmd_dashboard(cfg: Config):
    """Show the revenue dashboard."""
    ledger = Ledger(cfg.revenue_ledger_path)
    total = ledger.get_total()
    recent = ledger.get_recent(10)

    table = Table(title="Solomon v2 — Revenue Dashboard")
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
            row.get("note", "")[:50],
        )

    console.print(table)
    console.print(f"\n[bold green]Total Revenue: ${total:.2f}[/bold green]")

    # Channel status
    console.print("\n[dim]Channels:[/dim]")
    console.print(f"  freelance:  {'[green]ON[/green]' if cfg.channel_freelance else '[red]OFF[/red]'}")
    console.print(f"  content:    {'[green]ON[/green]' if cfg.channel_content else '[red]OFF[/red]'}")
    console.print(f"  microtask:  {'[green]ON[/green]' if cfg.channel_microtask else '[red]OFF[/red]'}")
    console.print(f"  ai_wrapper: {'[green]ON[/green]' if cfg.channel_ai_wrapper else '[red]OFF[/red]'}")
    console.print(f"  auto_submit: {'[red]ON[/red]' if cfg.auto_submit else '[green]OFF (safe)[/green]'}")


async def _cmd_test_browser(cfg: Config):
    """Test the stealth browser by navigating to a test page."""
    console.print(Panel("[cyan]Testing stealth browser...[/cyan]", title="Solomon v2"))
    browser = StealthBrowser(cfg)

    try:
        await browser.launch()
        console.print("[green]✓ Browser launched[/green]")

        await browser.goto("https://www.google.com")
        console.print(f"[green]✓ Navigated to google.com[/green]")
        console.print(f"  URL: {await browser.get_url()}")
        console.print(f"  Title: {await browser.get_title()}")

        screenshot = await browser.screenshot()
        console.print(f"[green]✓ Screenshot saved to {screenshot}[/green]")

        # Check navigator.webdriver (anti-bot indicator)
        webdriver = await browser.page.evaluate("navigator.webdriver")
        console.print(f"  navigator.webdriver: [yellow]{webdriver}[/yellow] {'(GOOD — stealth is working)' if not webdriver else '(BAD — detected as bot)'}")

        console.print("\n[bold green]✓ Browser test PASSED[/bold green]")
    except Exception as e:
        console.print(f"[red]✗ Browser test FAILED: {e}[/red]")
        raise
    finally:
        await browser.close()


async def _cmd_test_vlm(cfg: Config):
    """Test the VLM fallback by describing a test screenshot."""
    console.print(Panel("[cyan]Testing VLM fallback...[/cyan]", title="Solomon v2"))
    console.print(f"  VLM base URL: {cfg.vlm_base_url}")
    console.print(f"  VLM model: {cfg.vlm_model}")

    llm = LLMClient(cfg)
    vlm = VLMFallback(cfg)
    vlm.set_main_llm(llm)
    console.print(f"  Main LLM model: {cfg.llm_model}")
    console.print(f"  Main LLM vision-capable: {vlm._is_vision_capable(cfg.llm_model)}")

    # Try to describe an existing screenshot, or create one
    screenshot = cfg.runtime_dir / "screenshot.png"
    if not screenshot.exists():
        console.print("[yellow]No screenshot found, launching browser to create one...[/yellow]")
        browser = StealthBrowser(cfg)
        try:
            await browser.launch()
            await browser.goto("https://www.google.com")
            screenshot = await browser.screenshot(str(screenshot))
        finally:
            await browser.close()

    console.print(f"[dim]Sending screenshot to VLM...[/dim]")
    description = await vlm.describe_screenshot(str(screenshot))
    console.print(Panel(description[:500], title="VLM Description"))
    console.print("[bold green]✓ VLM test complete[/bold green]")


async def _cmd_test_llm(cfg: Config):
    """Test the main LLM connection."""
    console.print(Panel("[cyan]Testing LLM connection...[/cyan]", title="Solomon v2"))
    console.print(f"  Base URL: {cfg.llm_base_url}")
    console.print(f"  Model: {cfg.llm_model}")

    llm = LLMClient(cfg)
    try:
        response = await llm.ask("You are a test assistant.", "Say 'Hello, Solomon is alive!' and nothing else.")
        console.print(f"[green]✓ LLM response: {response}[/green]")
    except Exception as e:
        console.print(f"[red]✗ LLM test FAILED: {e}[/red]")
        raise


async def _cmd_guard_check(cfg: Config):
    """Verify the money guard is working correctly."""
    console.print(Panel("[cyan]Money Guard Check[/cyan]", title="Solomon v2"))

    # Test all money-OUT kinds are DENIED
    all_pass = True
    for kind in sorted(MONEY_OUT_KINDS):
        result = check_action(kind)
        if result:
            console.print(f"  [red]FAIL[/red] {kind} was PERMITTED (should be DENIED)")
            all_pass = False

    # Test all money-IN kinds are PERMITTED
    for kind in sorted(MONEY_IN_KINDS):
        result = check_action(kind)
        if not result:
            console.print(f"  [red]FAIL[/red] {kind} was DENIED (should be PERMITTED)")
            all_pass = False

    # Test unknown action is DENIED (fail-closed)
    unknown = check_action("some_unknown_action")
    if unknown:
        console.print("  [red]FAIL[/red] unknown action was PERMITTED (should be DENIED — fail-closed)")
        all_pass = False

    if all_pass:
        console.print(f"  [green]✓ All {len(MONEY_OUT_KINDS)} money-OUT kinds DENIED[/green]")
        console.print(f"  [green]✓ All {len(MONEY_IN_KINDS)} money-IN kinds PERMITTED[/green]")
        console.print("  [green]✓ Unknown actions DENIED (fail-closed)[/green]")
        console.print("\n[bold green]✓ Money guard is SACRED and INTACT[/bold green]")
    else:
        console.print("\n[bold red]✗ MONEY GUARD IS BROKEN[/bold red]")
        sys.exit(1)
