"""Solomon v2 entrypoint — `python -m solomon` or `solomon` CLI."""
import asyncio
import sys
import argparse

from .config import Config
from .cli import run_cli


def main():
    parser = argparse.ArgumentParser(
        prog="solomon",
        description="Solomon v2 — Autonomous Profit CEO",
    )
    parser.add_argument(
        "command",
        choices=["run", "dashboard", "test-browser", "test-vlm", "test-llm", "guard-check", "webhook", "loop", "discover"],
        help="run=CEO cycle, dashboard=status, test-*=self-test, guard-check=verify money guard, webhook=Polar listener, loop=continuous CEO, discover=find new income channels",
    )
    parser.add_argument("--once", action="store_true", help="Run one CEO cycle then exit")
    parser.add_argument("--dry", action="store_true", help="Dry run — no browser, no network")
    args = parser.parse_args()
    asyncio.run(run_cli(args))


if __name__ == "__main__":
    main()
