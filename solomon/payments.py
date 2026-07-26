"""Stripe payment rail — payment-link auto-creation + settled-payment polling.

Money-IN only. This module NEVER creates refunds, payouts, transfers, or any
money-OUT operation — the money guard is sacred. It does exactly two things:

1. ensure_payment_link(): idempotently create a Stripe product + one-time
   price + payment link for a tool, recorded in tools.json ("stripe_link").
   Creating these resources is free and moves no money.
2. poll_revenue(): fetch newly completed Checkout Sessions since the last
   cursor, pass the money guard ("collect"), and append to the revenue ledger.
   Polling the Stripe API replaces any webhook infrastructure for v1 — the
   existing 30-min cron is the polling cadence.

CLI:
    uv run python -m solomon.payments sync-links   # create missing links
    uv run python -m solomon.payments poll          # settle new payments
    uv run python -m solomon.payments status        # rail overview
"""
from __future__ import annotations

import json
import os
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

STRIPE_API = "https://api.stripe.com/v1"
DEFAULT_PRICE_CENTS = 500  # $5 one-time — matches the landing page copy
STATE_FILE = "stripe_state.json"


class StripeError(RuntimeError):
    """Raised on missing credentials or Stripe API errors."""


def _key() -> str:
    key = os.environ.get("STRIPE_SECRET_KEY", "").strip()
    if not key:
        raise StripeError("STRIPE_SECRET_KEY not set (check .env)")
    return key


def _flatten(data: dict, prefix: str = "") -> list:
    """Stripe form-encoding: nested dicts/lists become bracketed keys."""
    out = []
    for k, v in (data or {}).items():
        key = f"{prefix}[{k}]" if prefix else k
        if isinstance(v, dict):
            out.extend(_flatten(v, key))
        elif isinstance(v, (list, tuple)):
            for i, item in enumerate(v):
                if isinstance(item, dict):
                    out.extend(_flatten(item, f"{key}[{i}]"))
                else:
                    out.append((f"{key}[{i}]", str(item)))
        elif v is not None:
            out.append((key, str(v)))
    return out


def _req(method: str, path: str, data: dict | None = None) -> dict:
    body = urllib.parse.urlencode(_flatten(data)).encode() if data else None
    req = urllib.request.Request(
        STRIPE_API + path,
        data=body,
        method=method,
        headers={
            "Authorization": f"Bearer {_key()}",
            "User-Agent": "SolomonBot/1.0",
            "Content-Type": "application/x-www-form-urlencoded",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return json.loads(r.read())
    except urllib.error.HTTPError as e:
        detail = e.read().decode(errors="replace")[:300]
        raise StripeError(f"Stripe {method} {path} -> {e.code}: {detail}") from e


def ensure_payment_link(tool: dict) -> str | None:
    """Create (or reuse) a Stripe payment link for a tool registry entry.

    Idempotent: returns the existing tool["stripe_link"] if present; otherwise
    creates product -> price -> payment link and mutates the tool dict.
    Returns the link URL, or None on failure (caller keeps going).
    """
    existing = tool.get("stripe_link")
    if existing:
        return existing
    name = tool.get("name", "ai-tool")
    pretty = name.replace("-", " ").title()
    try:
        product = _req("POST", "/products", {
            "name": f"Solomon Tools — {pretty}",
            "description": tool.get("description", f"AI-powered {pretty}"),
            "metadata": {"solomon_tool": name},
        })
        price = _req("POST", "/prices", {
            "currency": "usd",
            "unit_amount": DEFAULT_PRICE_CENTS,
            "product": product["id"],
        })
        link = _req("POST", "/payment_links", {
            "line_items": [{"price": price["id"], "quantity": 1}],
            "metadata": {"solomon_tool": name},
            # The account has Stripe Managed Payments enabled by default. We
            # do not yet have a configured Stripe tax code for these digital
            # tools, so disable that optional rail rather than creating a link
            # that Stripe rejects. Tax handling can be enabled deliberately
            # later once the operator selects the correct tax classification.
            "managed_payments": {"enabled": False},
        })
        url = link.get("url")
        if url:
            tool["stripe_link"] = url
            tool["stripe_link_id"] = link["id"]
            tool["stripe_product_id"] = product["id"]
        return url
    except (StripeError, KeyError) as e:
        print(f"[PAYMENTS] link creation failed for {name}: {e}")
        return None


def sync_links(wrappers_dir: Path) -> int:
    """Create payment links for every registered tool missing one.
    Returns the number of links created. Self-healing: safe to run any time."""
    from .landing import load_registry, save_registry

    tools = load_registry(wrappers_dir)
    created = 0
    for t in tools:
        if t.get("stripe_link"):
            continue
        if ensure_payment_link(t):
            created += 1
    if created:
        save_registry(wrappers_dir, tools)
    return created


def _load_state(runtime_dir: Path) -> dict:
    p = runtime_dir / STATE_FILE
    if p.exists():
        try:
            return json.loads(p.read_text(encoding="utf-8"))
        except Exception:
            pass
    return {"last_created": 0, "seen": []}


def _save_state(runtime_dir: Path, state: dict) -> None:
    state["seen"] = state["seen"][-500:]  # bound the dedup list
    (runtime_dir / STATE_FILE).write_text(json.dumps(state, indent=2), encoding="utf-8")


def fetch_completed_sessions(since: int = 0, limit: int = 50) -> list:
    """List completed Checkout Sessions newer than the `created` cursor."""
    params = {"status": "complete", "limit": limit}
    if since:
        params["created[gt]"] = since
    qs = urllib.parse.urlencode(params)
    data = _req("GET", f"/checkout/sessions?{qs}")
    return data.get("data", [])


def poll_revenue(runtime_dir: Path, ledger) -> int:
    """Settle newly completed payments into the revenue ledger.

    Every session passes the money guard as money-IN ("collect") before it is
    logged. Returns the number of new payments logged. Idempotent via the
    seen-session list + created cursor in runtime/stripe_state.json.
    """
    from .guard import guard

    state = _load_state(runtime_dir)
    sessions = fetch_completed_sessions(state["last_created"])
    logged = 0
    max_created = state["last_created"]
    for s in sessions:
        sid = s.get("id", "")
        if not sid or sid in state["seen"]:
            continue
        state["seen"].append(sid)
        max_created = max(max_created, s.get("created", 0))
        if s.get("currency", "usd") != "usd":
            continue  # non-USD: mark seen, don't log (deferred: conversion)
        amount = (s.get("amount_total") or 0) / 100
        if amount <= 0:
            continue
        guard("collect", {"source": "stripe", "session": sid})
        email = (s.get("customer_details") or {}).get("email", "")
        ledger.log_revenue(
            channel="ai_wrapper",
            amount_usd=amount,
            source=f"stripe:{s.get('payment_link') or 'checkout'}",
            note=f"session={sid} email={email}",
        )
        logged += 1
    state["last_created"] = max_created
    _save_state(runtime_dir, state)
    return logged


def main(argv=None) -> None:
    import argparse

    from .config import Config
    from .landing import load_registry
    from .ledger import Ledger

    parser = argparse.ArgumentParser(prog="solomon.payments")
    parser.add_argument("command", choices=["sync-links", "poll", "status"])
    args = parser.parse_args(argv)

    cfg = Config.load()
    wrappers = cfg.runtime_dir / "ai_wrappers"
    ledger = Ledger(cfg.revenue_ledger_path)

    if args.command == "sync-links":
        n = sync_links(wrappers)
        print(f"[PAYMENTS] links synced ({n} created)")
    elif args.command == "poll":
        n = poll_revenue(cfg.runtime_dir, ledger)
        print(f"[PAYMENTS] poll complete: {n} new payment(s), total ${ledger.get_total():.2f}")
    else:
        tools = load_registry(wrappers)
        linked = sum(1 for t in tools if t.get("stripe_link"))
        print(f"[PAYMENTS] tools={len(tools)} linked={linked} revenue=${ledger.get_total():.2f}")


if __name__ == "__main__":
    main()
