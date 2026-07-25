"""Polar webhook listener — receives payment events and logs revenue.

When a customer buys an AI-wrapper tool via Polar checkout, Polar sends a
webhook to this endpoint. The handler:
1. Verifies the webhook signature (HMAC-SHA256)
2. Extracts the amount and product info
3. Passes through the money guard (money-IN: "collect")
4. Appends a revenue event to the ledger

Run standalone: uv run python -m solomon.webhook --port 8019
Or behind a reverse proxy / Cloudflare Tunnel for production.
"""
import asyncio
import hashlib
import hmac
import json
import os
from http.server import HTTPServer, BaseHTTPRequestHandler

from .config import Config
from .guard import guard
from .ledger import Ledger


class PolarWebhookHandler(BaseHTTPRequestHandler):
    """Handles incoming Polar webhook events."""

    def do_POST(self):
        content_length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(content_length)

        # Get config + ledger from the server instance
        cfg = self.server.cfg
        ledger = self.server.ledger
        webhook_secret = os.getenv("POLAR_WEBHOOK_SECRET", "")

        # Verify signature if secret is set
        if webhook_secret:
            signature = self.headers.get("X-Polar-Signature", "")
            expected = hmac.new(
                webhook_secret.encode(), body, hashlib.sha256
            ).hexdigest()
            if not hmac.compare_digest(signature, expected):
                self.send_response(401)
                self.end_headers()
                self.wfile.write(b'{"error": "invalid signature"}')
                return

        # Parse the webhook payload
        try:
            payload = json.loads(body)
        except json.JSONDecodeError:
            self.send_response(400)
            self.end_headers()
            self.wfile.write(b'{"error": "invalid JSON"}')
            return

        # Polar webhook event types: checkout.created, checkout.updated,
        # subscription.created, subscription.updated, order.created
        event_type = payload.get("type", "")
        data = payload.get("data", {})

        # Only log revenue on successful payment events
        revenue_events = {
            "checkout.updated": lambda d: d.get("status") == "succeeded",
            "order.created": lambda d: True,
            "subscription.created": lambda d: True,
        }

        if event_type in revenue_events and revenue_events[event_type](data):
            amount = data.get("amount", 0) / 100  # Polar uses cents
            currency = data.get("currency", "usd")
            product_id = data.get("product_id", "")
            customer_email = data.get("customer_email", "")
            order_id = data.get("id", "")

            if currency != "usd":
                # Non-USD — skip for now (deferred: currency conversion)
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b'{"status": "skipped_non_usd"}')
                return

            # Money guard — this is money-IN ("collect"), must pass
            try:
                guard("collect", {"source": "polar", "order_id": order_id})
            except Exception:
                self.send_response(403)
                self.end_headers()
                self.wfile.write(b'{"error": "money guard denied"}')
                return

            # Log the revenue
            ledger.log_revenue(
                channel="ai_wrapper",
                amount_usd=amount,
                source=f"polar:{product_id}",
                note=f"order={order_id} customer={customer_email}",
            )

            print(f"[POLAR] Revenue logged: ${amount:.2f} from order {order_id}")
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b'{"status": "logged"}')
        else:
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b'{"status": "ignored"}')

    def do_GET(self):
        """Health check endpoint."""
        if self.path == "/health":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"status": "ok", "service": "solomon-webhook"}')
        else:
            self.send_response(404)
            self.end_headers()

    def log_message(self, format, *args):
        print(f"[WEBHOOK] {args[0]}")


def run_webhook_server(port: int = 8019):
    """Start the Polar webhook listener."""
    cfg = Config.load()
    ledger = Ledger(cfg.revenue_ledger_path)

    server = HTTPServer(("0.0.0.0", port), PolarWebhookHandler)
    server.cfg = cfg
    server.ledger = ledger

    print(f"[SOLOMON] Webhook listener on :{port}")
    print(f"  Endpoint: POST http://localhost:{port}/")
    print(f"  Health:   GET  http://localhost:{port}/health")
    print(f"  Revenue total: ${ledger.get_total():.2f}")
    print(f"  Waiting for Polar payment events...")

    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n[SOLOMON] Webhook listener stopped.")
        server.server_close()


if __name__ == "__main__":
    import argparse
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=8019)
    args = parser.parse_args()
    run_webhook_server(args.port)
