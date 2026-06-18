#!/usr/bin/env node
/**
 * capture.js — Playwright capture for Solomon's visual E2E review.
 *
 * Given a base URL + list of page paths, navigates each page at a fixed viewport,
 * captures: screenshot (PNG, base64), accessibility snapshot (YAML text), console
 * errors, and network 4xx/5xx responses. Outputs a single JSON line to stdout.
 *
 * Usage:
 *   node capture.js <base_url> '<page_paths_json>' [viewport_width] [viewport_height]
 *
 * Output (JSON line):
 *   { pages: [{ path, screenshot_b64, a11y_yaml, console_errors: [...], network_errors: [...] }] }
 *
 * Run with: npx playwright install chromium  (first time only)
 */
const { chromium } = require('playwright');

async function main() {
  const baseUrl = process.argv[2];
  const pagesRaw = process.argv[3] || '["/"]';
  const vw = parseInt(process.argv[4] || '1280');
  const vh = parseInt(process.argv[5] || '800');
  if (!baseUrl) { console.error('capture.js: missing base_url'); process.exit(1); }
  let pages;
  try { pages = JSON.parse(pagesRaw); } catch { pages = ['/']; }

  const browser = await chromium.launch({ headless: true });
  const context = await browser.newContext({ viewport: { width: vw, height: vh } });
  const results = [];

  for (const pagePath of pages) {
    const page = await context.newPage();
    const consoleErrors = [];
    const networkErrors = [];

    page.on('console', msg => {
      if (msg.type() === 'error') consoleErrors.push(msg.text().slice(0, 500));
    });
    page.on('response', resp => {
      const status = resp.status();
      if (status >= 400) networkErrors.push(`${status} ${resp.url().slice(0, 200)}`);
    });
    page.on('requestfailed', req => {
      networkErrors.push(`FAIL ${req.url().slice(0, 200)} — ${req.failure()?.errorText || ''}`);
    });

    const url = baseUrl + pagePath;
    let a11y = '';
    let screenshotB64 = '';
    let navError = null;
    try {
      await page.goto(url, { waitUntil: 'networkidle', timeout: 15000 });
      // Give late-loading content a moment
      await page.waitForTimeout(800);
      // Accessibility snapshot — YAML-ish text tree
      try {
        a11y = await page.accessibility.snapshot();
        a11y = a11y ? JSON.stringify(a11y, null, 1) : '';
      } catch { a11y = ''; }
      // Screenshot
      const buf = await page.screenshot({ type: 'png', fullPage: false });
      screenshotB64 = buf.toString('base64');
    } catch (e) {
      navError = String(e).slice(0, 500);
    }

    results.push({
      path: pagePath,
      url,
      screenshot_b64: screenshotB64,
      a11y_yaml: a11y,
      console_errors: consoleErrors,
      network_errors: networkErrors,
      nav_error: navError,
    });
    await page.close();
  }

  await browser.close();
  process.stdout.write(JSON.stringify({ pages: results }) + '\n');
}

main().catch(e => { console.error('capture.js error:', String(e).slice(0, 300)); process.exit(1); });