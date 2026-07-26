"""CDP co-pilot driver for the visible PatchRight Chromium (port 9223).

Usage: uv run python scripts/copilot.py <cmd> [args]
  snap                 — list URL, title, visible inputs/buttons/links
  shot <path>          — screenshot
  goto <url>           — navigate
  click <text>         — click first visible element matching text
  clicklabel <label>   — click element by aria-label
  fill <label> <value> — fill input by label/name/placeholder
  press <key>          — keyboard press (e.g. Enter, Tab)
  check <name>         — check a checkbox by nearby text
"""
import sys
from patchright.sync_api import sync_playwright

CDP = "http://localhost:9223"


def connect():
    p = sync_playwright().start()
    b = p.chromium.connect_over_cdp(CDP)
    ctx = b.contexts[0]
    page = ctx.pages[0] if ctx.pages else ctx.new_page()
    return p, b, page


def snap(page):
    print("URL:", page.url)
    print("TITLE:", page.title())
    els = page.locator("input, button, select, [role='button'], a").all()
    shown = 0
    for el in els:
        if shown >= 50:
            break
        try:
            if not el.is_visible():
                continue
            tag = el.evaluate("e => e.tagName.toLowerCase()")
            kind = el.get_attribute("type") or ""
            label = (el.get_attribute("aria-label") or el.get_attribute("name")
                     or el.get_attribute("placeholder") or "")
            text = (el.inner_text() or "").strip().replace("\n", " ")[:45]
            if label or text:
                print(f"  [{shown}] {tag} type={kind} label={label!r} text={text!r}")
                shown += 1
        except Exception:
            pass


def main():
    cmd = sys.argv[1]
    p, b, page = connect()
    try:
        if cmd == "snap":
            snap(page)
        elif cmd == "shot":
            page.screenshot(path=sys.argv[2])
            print("saved", sys.argv[2])
        elif cmd == "goto":
            page.goto(sys.argv[2], wait_until="domcontentloaded")
            page.wait_for_timeout(2000)
            snap(page)
        elif cmd == "click":
            page.get_by_text(sys.argv[2], exact=False).first.click()
            page.wait_for_timeout(1800)
            snap(page)
        elif cmd == "clickhref":
            page.locator(f"a[href*='{sys.argv[2]}']").first.click()
            page.wait_for_timeout(2500)
            snap(page)
        elif cmd == "clicklabel":
            page.get_by_label(sys.argv[2]).first.click()
            page.wait_for_timeout(1500)
            snap(page)
        elif cmd == "fill":
            label, value = sys.argv[2], sys.argv[3]
            try:
                page.get_by_label(label).first.fill(value)
            except Exception:
                page.locator(f"input[name='{label}'], input[placeholder*='{label}']").first.fill(value)
            print(f"filled {label!r}")
        elif cmd == "press":
            page.keyboard.press(sys.argv[2])
            page.wait_for_timeout(1200)
            snap(page)
        elif cmd == "check":
            page.get_by_text(sys.argv[2], exact=False).first.click()
            page.wait_for_timeout(800)
            print("checked", sys.argv[2])
    finally:
        b.close()  # detaches only; browser stays open
        p.stop()


if __name__ == "__main__":
    main()
