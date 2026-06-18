# Ultra Code Plan — Visually Controlled Sandbox E2E Reviewer for Solomon

> **Goal:** After each RSI iteration that passes its gate, spin up the repo's app in an
> ephemeral sandbox, drive a **vision-capable** agent through it as a real user, capture
> screenshots + a11y trees + console/network errors, produce a one-time feedback report
> that is fed back to the RSI loop, then wrap up the iteration. The operator sees the
> screenshots + agent walkthrough **in the Solomon workspace drawer**, next to the RSI
> run, with a clean Claude/Codex desktop-app aesthetic.

## Scope

**In:**
- `improver/sandbox.py` — ephemeral port + temp-state sandbox booter (new)
- `improver/capture.js` — Playwright Node script: navigate pages, capture screenshots + a11y + console/network errors (new)
- `improver/vision-cloud.ts` — pi provider extension that registers a vision-capable model (new)
- `improver/visual_review.md` — the visual reviewer agent contract (new)
- `improver/visual_review.py` — orchestrator: sandbox → capture → pi vision agent → feedback report (new)
- `control.py` — `project_sandbox()`, `set_repo_config(sandbox=)`, `read_visual_review()` (modify)
- `run_improver.py` — call `visual_review.run()` after gate-green, feed feedback once (modify)
- `app.py` — API methods: `get_visual_review()`, sandbox config in `get_state()` (modify)
- `web/app.js` — Visual Review tab in workspace drawer with screenshot gallery (modify)
- `web/styles.css` — visual review gallery + live indicator styling (modify)

**Out of scope:**
- Pixel-baseline diffing (advisory layer, can add later)
- Full Playwright-Python integration (using Node Playwright via npx is leaner — no pip install needed)
- Modifying the target repos (sover, maki, asmodeus) — they are RSI-managed

## Interdependency Map

| Module | Owns | Depends on | Coupling |
|--------|------|------------|----------|
| `sandbox.py` | Port allocation, temp state dir, subprocess lifecycle | `control._clean_subenv()`, repo path | Swappable — standalone module |
| `capture.js` | Playwright navigation, screenshot/a11y/console capture | Node + Playwright (npx), sandbox URL | Swappable — called by visual_review.py |
| `vision-cloud.ts` | pi provider registration for vision model | pi extension API, OLLAMA_API_KEY | Swappable — loaded via `-e` |
| `visual_review.md` | Agent contract for the visual reviewer | pi system prompt mechanism | Tightly coupled to visual_review.py |
| `visual_review.py` | Orchestrates sandbox + capture + pi vision agent + feedback | sandbox.py, capture.js, vision-cloud.ts, run_improver.py globals | Tightly coupled to run_improver.py |
| `control.py` | Config accessors, state management | repos.json | Modified — new accessors |
| `run_improver.py` | RSI loop, gate, ship | visual_review.py (new import) | Modified — calls visual_review after gate |
| `app.py` | pywebview API bridge | control.py | Modified — new API methods |
| `web/app.js` | UI rendering | app.py API | Modified — new tab + gallery |
| `web/styles.css` | Styling | web/app.js classes | Modified — new CSS |

**External contracts:**
- `repos.json` schema gains optional `sandbox` object per repo
- Heartbeat schema gains optional `visual_review` field
- Runtime dir gains `visual_review/` subdirectory with screenshots + report.json

## Risk Register

| Risk | Blast radius | Mitigation |
|------|-------------|------------|
| Sandbox process leaks (orphaned on crash) | Port exhaustion, stale processes | PID tracking + orphan-guard kill in `finally`; always use ephemeral port |
| Playwright not installed | Visual review silently fails | Use `npx playwright` (auto-installs); degrade gracefully — skip review if capture fails |
| Vision model not vision-capable | Agent gets text-only, misses visual issues | Config per-repo: `sandbox.vision_model` — must be set; skip if unset |
| Sandbox corrupts repo working tree | RSI loop sees dirty tree | Sandbox runs in temp state dir; never writes to repo path; only reads from checked-out branch |
| Visual review adds latency to each iteration | Loop slows down | Timeout on each phase (boot 30s, capture 60s, agent 120s); skip on timeout |
| Secrets leak into sandbox | Real credentials exposed to ephemeral env | Strip all secret env vars; force disposable profile; no real tokens |
| Feedback loop creates circular RSI | Agent improves for the reviewer, not the user | Feedback is ONE-TIME per iteration, appended to the next backlog item — not a permanent loop |

## Orchestration Shape

**Sequential** — each step depends on the previous one's output. The visual review is a
linear pipeline: sandbox boot → capture → vision agent → feedback report → UI display.

## Sequenced Plan

### Step 1: `improver/sandbox.py` (new)
- Context manager that boots a repo's app on an ephemeral port with temp state
- Allocates free port, creates temp dir, strips secrets from env, Popen launch command
- Polls health URL to 200, yields sandbox handle
- On exit: terminate process, cleanup temp dir
- **Verify:** `python -c "from sandbox import Sandbox; print('ok')"` imports cleanly

### Step 2: `improver/capture.js` (new)
- Node script using Playwright to navigate configured pages
- Captures: screenshot (PNG), a11y snapshot (YAML), console errors, network 4xx/5xx
- Outputs JSON to stdout with base64 screenshots + text artifacts
- **Verify:** `npx playwright --version` works; script syntax valid

### Step 3: `improver/vision-cloud.ts` (new)
- pi provider extension registering a vision-capable model (e.g. `qwen/qwen2.5-vl-72b`)
- Mirrors maki-cloud.ts structure but sets `input: ["text", "image"]`
- **Verify:** TypeScript syntax valid; provider name unique

### Step 4: `improver/visual_review.md` (new)
- Agent contract: "You are the Visual Reviewer. You are given screenshots + a11y trees
  of an app after an RSI iteration. Test as a real user would. Report findings."
- Embeds RSI concepts from the video: anti-gaming (don't just make it look green),
  crane-climbing (each improvement builds on the last), metric-aware (don't cheat the
  visual check), exploration (try wacky things a user would do)
- Output format: structured findings (severity, category, description, screenshot_ref)

### Step 5: `improver/visual_review.py` (new)
- `run(repo, branch, sandbox_config, vision_model)` — main entry point
- Boots sandbox via sandbox.py, runs capture.js, feeds results to pi vision agent
- Saves screenshots to `runtime/<name>/visual_review/`, writes `report.json`
- Returns feedback text for the RSI loop
- **Verify:** imports cleanly, function signature correct

### Step 6: `control.py` modifications
- `project_sandbox(repo)` — returns sandbox config dict or None
- `set_repo_config(sandbox=...)` — upsert sandbox config in repos.json
- `read_visual_review(repo)` — read latest report.json from runtime
- **Verify:** `python -c "import control; print(control.project_sandbox({}))"` returns None

### Step 7: `run_improver.py` modifications
- Import visual_review at top
- After gate-green + commit, before ship: if sandbox enabled, run visual_review
- Feedback appended to heartbeat + written to runtime
- One-time feedback: the review report is saved, and the next iteration's task includes
  "Previous visual review found: ..." — then it's done
- **Verify:** syntax check via `python -c "import run_improver"` (from improver dir)

### Step 8: `app.py` modifications
- `get_visual_review(name)` — API method to read latest visual review
- Include `sandbox` config in `get_state()` per-repo
- `set_repo_config` accepts `sandbox` parameter
- **Verify:** `python -c "import app; print('ok')"` (from solomon dir)

### Step 9: `web/app.js` modifications
- New workspace tab: "Review" (between "Diff" and "Backlog")
- Shows screenshot gallery (thumbnails → full-size on click)
- Shows agent findings list with severity badges
- Shows live indicator when review is in progress
- Mock data for browser preview
- **Verify:** mock mode renders the tab

### Step 10: `web/styles.css` modifications
- `.vr-gallery` — screenshot thumbnail grid
- `.vr-screenshot` — full-size lightbox
- `.vr-finding` — finding card with severity color
- `.vr-live` — pulsing indicator for in-progress review
- **Verify:** CSS syntax valid

### Step 11: Completion audit
- Verify all files exist and are syntactically valid
- Test sandbox boot with a mock app
- Check that the UI renders the Review tab in mock mode
- Verify no existing functionality is broken