"""Landing-page copy and CTA tests."""
from solomon.landing import render_html


def test_landing_describes_optional_support_without_paid_access_claim():
    html = render_html([{
        "name": "text-summarizer",
        "description": "Summarize text.",
        "url": "https://text-summarizer.solomontools.workers.dev",
        "stripe_link": "https://buy.stripe.com/live-link",
    }])
    assert "Free to try" in html
    assert "Support with $5" in html
    assert "Optional $5 support" in html
    assert "Pay once, use forever" not in html
    assert "buy.stripe.com/live-link" in html
