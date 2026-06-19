#!/usr/bin/env node
/**
 * agent_browser_driver.js — Playwright driver for the agent-controlled browser (Feature 1).
 *
 * One-shot per action: the Python bridge (agent_browser.py) calls this with a JSON payload
 * describing the action (navigate / click / type / scroll / screenshot). The driver owns a
 * persistent Chromium instance + page stored in a temp user-data-dir keyed by repo path, so
 * repeated actions on the same repo reuse the same browser session (state persists across
 * actions within a session). After each action the driver captures a screenshot + the
 * cursor's current position (as a % of the viewport) and prints a single JSON line to stdout.
 *
 * Usage:
 *   node agent_browser_driver.js <repo_path> '<action_json>'
 *
 * action_json: { action: "navigate"|"click"|"type"|"scroll"|"screenshot",
 *                viewport: [w, h], url?, x_pct?, y_pct?, text?, dx?, dy? }
 *
 * Output (JSON line): { ok, url, screenshot_b64, x, y, status, error? }
 *   x, y are cursor position as viewport percentages (0-100) for the panel's visible cursor.
 *
 * The browser is launched HEADLESS so it never steals the operator's cursor/focus — the
 * operator sees the agent's browser ONLY through the in-app panel's screenshot stream.
 */
const { chromium } = require('playwright');
const fs = require('fs');
const os = require('os');
const path = require('path');

// Persistent session store: one Chromium user-data-dir per repo path, so actions on the same
// repo reuse the same browser context (login state, cookies, etc. persist across actions).
function userDataDir(repoPath) {
  const base = path.join(os.tmpdir(), 'solomon-agent-browser');
  try { fs.mkdirSync(base, { recursive: true }); } catch {}
  // sanitize repo path into a safe folder name
  const safe = (repoPath || 'default').replace(/[^A-Za-z0-9._-]/g, '_').slice(0, 64);
  return path.join(base, safe);
}

async function main() {
  const repoPath = process.argv[2] || '.';
  let payload;
  try { payload = JSON.parse(process.argv[3] || '{}'); } catch { payload = {}; }
  const action = payload.action || 'screenshot';
  const vw = parseInt((payload.viewport || [1280, 800])[0]);
  const vh = parseInt((payload.viewport || [1280, 800])[1]);

  let browser, context, page;
  try {
    browser = await chromium.launchPersistentSession(userDataDir(repoPath), {
      headless: true,
      args: ['--no-first-run', '--no-default-browser-check'],
    });
    context = browser.contexts()[0] || await browser.newContext({ viewport: { width: vw, height: vh } });
    await context.setViewportSize({ width: vw, height: vh });
    page = context.pages()[0] || await context.newPage();
  } catch (e) {
    process.stdout.write(JSON.stringify({ ok: false, error: 'launch failed: ' + String(e).slice(0, 200) }) + '\n');
    process.exit(1);
  }

  let ok = true, error = null, url = '', screenshotB64 = '', x = 0, y = 0, status = 'live';
  try {
    if (action === 'navigate' && payload.url) {
      await page.goto(payload.url, { waitUntil: 'domcontentloaded', timeout: 15000 });
      await page.waitForTimeout(400);
      url = page.url();
    } else if (action === 'click') {
      const px = Math.round((parseFloat(payload.x_pct) / 100) * vw);
      const py = Math.round((parseFloat(payload.y_pct) / 100) * vh);
      await page.mouse.click(px, py);
      x = parseFloat(payload.x_pct); y = parseFloat(payload.y_pct);
      await page.waitForTimeout(200);
      url = page.url();
    } else if (action === 'type' && typeof payload.text === 'string') {
      await page.keyboard.type(payload.text, { delay: 8 });
      await page.waitForTimeout(150);
      url = page.url();
    } else if (action === 'scroll') {
      await page.mouse.wheel(parseInt(payload.dx || 0), parseInt(payload.dy || 300));
      await page.waitForTimeout(150);
      url = page.url();
    } else if (action === 'screenshot') {
      url = page.url();
    } else {
      ok = false; error = 'unknown action: ' + action;
    }
    if (ok) {
      const buf = await page.screenshot({ type: 'png', fullPage: false });
      screenshotB64 = buf.toString('base64');
      // for non-click actions, report the last known cursor position (0,0 if none)
      if (action !== 'click') { x = 0; y = 0; }
      status = action === 'navigate' ? 'navigated' : 'ready';
    }
  } catch (e) {
    ok = false; error = String(e).slice(0, 300); status = 'error';
  } finally {
    try { await browser.close(); } catch {}
  }
  process.stdout.write(JSON.stringify({ ok, url, screenshot_b64: screenshotB64,
    x, y, status, error }) + '\n');
}

main().catch(e => {
  process.stdout.write(JSON.stringify({ ok: false, error: String(e).slice(0, 300) }) + '\n');
  process.exit(1);
});