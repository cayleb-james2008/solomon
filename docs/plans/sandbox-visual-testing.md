# Plan — Sandboxed visual/UX app-testing for the Solomon RSI agent

> Goal: let the RSI agent **spin up the app in an isolated sandbox and test it as a user would** —
> opening UI/UX improvement lanes and putting the agent in the user's seat — while you keep using
> your own live instance untouched. Reuse Solomon's existing seams (the runner-owned gate, `project_*`
> config, the backlog/contract, `SOVER_HOME`-style isolation). No new agent runtime, no new service.

## The core problem & answer

The pi coder (`kimi-k2.7-code`) is **text-only — no vision**. So "test visually" becomes: a headless
sandbox renders the app and emits **text signals the agent reasons over** — the accessibility tree
(YAML), trimmed DOM, and console / network (4xx-5xx, uncaught-exception) errors — plus a **screenshot**.
Text is the deterministic pass/fail channel; a **vision-model screenshot critique** and **pixel
baselines** are *advisory* layers on top. This catches what actually breaks these apps: a route 500s,
the cockpit poll returns non-200, a JS exception kills `live.js` so metrics never populate, a control
goes missing.

## Architecture — four thin layers, each reusing a pattern

1. **Sandbox runner** — `improver/sandbox.py`, an ~80-line context manager. On enter: pick a free
   `127.0.0.1:0` port; `mkdtemp()` a throwaway state dir; build a **clean child env** (mirror
   `_clean_env()` — strip `PYTHONPATH`/`PYTHONHOME`) plus sandbox-only vars (sover:
   `SOVER_API_PORT=<free>`, `SOVER_HOME=<tmp>`, `SOVER_PROFILE=sandbox`, **auto_post forced off, no
   real secrets**); `Popen` the launch command in the **branch checkout** (the loop already has the
   branch checked out in-tree — no worktree plumbing); poll the health URL to 200. On exit (`finally`):
   terminate the subprocess, `browser.close()`, `shutil.rmtree(tmp)`, PID orphan-guard. **This is the
   only new isolation primitive** — and it's what lets your live `:8770` instance run undisturbed.
2. **Capture layer** — `improver/visual_verify.py` driven by **Playwright-Python** (headless Chromium,
   installed into the repo venv). Per configured page at a fixed 1280×800 viewport: navigate; wire
   `page.on("console")` (error/warning), `page.on("requestfailed")`, `page.on("response")` (4xx/5xx);
   capture `aria_snapshot()` (YAML a11y tree), trimmed `page.content()`, the error log, and a
   `page.screenshot()`.
3. **See-layer** — text artifacts are the **always-on, deterministic** input to the agent + the gate;
   the screenshot feeds an **optional vision-model critique** and **pixel-baseline diff** (advisory).
4. **Two modes**:
   - **Visual GATE dimension** — runs *inside* `one_iteration()` right after the pytest gate passes;
     a hard text-failure (console error / 5xx during capture) reverts the branch via the same
     `_drop_branch(...)` mechanism. Default **OFF** per repo.
   - **UI/UX DISCOVERY lane** — `--discover` (cloned from `--beautify`): boots the sandbox, the agent
     tours the app as a user and **appends `- [ ]` findings to `backlog.md`** for later gated
     iterations to ship. Operator-triggered (a dashboard button).

## Per-repo `sandbox` config (threaded the same 5-edit path as `gate`)

```jsonc
"sandbox": {
  "enabled": false,                 // master switch; off => visual gate + discover no-op
  "launch": "",                     // shell cmd in the branch cwd; empty => derived (sover: .venv python scripts/run_api.py)
  "port_env": "SOVER_API_PORT",     // env var the ephemeral port is injected as
  "state_env": "SOVER_HOME",        // env var the temp state dir is injected as
  "extra_env": {"SOVER_PROFILE": "sandbox"},  // sandbox-only env (disposable profile / auto_post off)
  "health": "/",                    // path polled for 200
  "pages": ["/", "/dashboard/cockpit/"]        // the "app as a user sees it" — drives gate + baselines
}
```
Edits mirror the `gate` key exactly: `control.project_sandbox()` accessor + `set_repo_config(sandbox=)`
writer → `repos.json` → `app.get_state()` + `Api.set_repo_config()` → `web/app.js` Config tab → passed
into `control.start()`'s spawn args + a `run_improver.py` argparse arg/global.

## Phased delivery (each independently verifiable)

- **Phase 0 — Sandbox runner + boot proof** (no agent, no gate). `improver/sandbox.py` boots the
  branch's sover on a random port with a temp `SOVER_HOME`. *Gate:* health returns 200; your `:8770`
  is untouched; temp dir cleaned up.
- **Phase 1 — Capture layer.** Install Playwright in the repo venv; `visual_verify.py` captures the
  configured pages → aria YAML + console/network log + screenshot. *Gate:* artifacts produced for
  sover's pages with zero false console errors on a clean build.
- **Phase 2 — Visual GATE wired in.** `VISUAL_GATE` global + `run_visual_gate()` called in
  `one_iteration()` after the pytest gate, `if VISUAL_GATE and not BEAUTIFY and not SOLOMON`. *Gate:* a
  `--once` run with a deliberate UI break (console error) reverts; a clean change ships, with a
  `visual:{}` field in the heartbeat/PR body. **← MVP ends here (text-only).**
- **Phase 3 — UI/UX DISCOVERY lane.** `--discover` flag + `improver/discover.md` audit contract; the
  agent tours the sandbox and appends `- [ ]` UI/UX items to the backlog. *Gate:* a Discover run
  files ≥1 actionable item.
- **Phase 4 — Advisory signals.** Claude-API vision critique per screenshot + committed pixel
  baselines with a diff threshold. *Gate:* a deliberate layout change exceeds the pixel diff and is
  surfaced (advisory).

## MVP (smallest valuable slice)
Phase 0 + 1 + the **text-only** half of Phase 2: `sandbox.py` boots sover on an ephemeral port with a
temp state dir; `visual_verify.py` drives headless Chromium over the home + cockpit pages;
`run_visual_gate()` fails the iteration on **any console error / 5xx** during capture; default OFF,
surfaced in the heartbeat. No vision, no baselines, no discover lane yet.

## Risks (one is a blocker)
- **🚩 Dev-mode isolation gap (blocker for Phase 0):** sover honors `SOVER_HOME` only in the *frozen*
  build; in dev `STATE_ROOT = repo root`, so a temp `SOVER_HOME` is ignored and the sandbox would
  write into the branch working tree — corrupting it. **Fix:** make `sover_profile.py` honor
  `SOVER_HOME` in dev too (a ~1-line change, shipped *by the pi agent* as a sover backlog item), or
  run the frozen build (slower). Verify in Phase 0 before relying on temp-state isolation.
- **Secret leak:** `run_api.py` calls `load_dotenv(WORKSPACE/.env)` → the sandbox inherits real
  secrets unless the sandbox env empties/overrides the sensitive keys. Force `auto_post` off + a
  no-real-credentials disposable profile (no handles/tokens).
- **Flaky false reverts:** filter console to **error-level** for the hard gate (warnings advisory),
  generous health-poll timeout, keep the visual gate default-OFF until tuned.
- **Port/process orphans:** PID tracking + orphan-guard kill in `finally`; never reuse a fixed port.
- **Playwright footprint:** bundled Chromium per managed venv + boot latency. Only install where
  `sandbox.enabled`; run vision/pixel checks on a cadence, not every micro-iteration.
- **Generality:** the knobs are sover-specific; other repos supply their own launch/health/state-env,
  falling back to `_detect_stack` entrypoints and treating unknown repos as not-sandboxable.

## Decisions for you (the open questions)
1. **Gate severity:** text errors hard-fail; pixel/vision **advisory** until baselines are trusted? *(recommended)*
2. **Dev-mode `SOVER_HOME`:** patch sover to honor it in dev (clean, 1-line, agent-shipped) vs. always build the frozen exe (slower)?
3. **Vision provider/cost:** Claude API for screenshot critique, or skip vision for MVP (text + pixel-diff only)? Per-iteration vision budget?
4. **Pages that define "the app":** just the shell + cockpit dashboards, or also onboarding/chat surfaces?
5. **Baseline storage:** in Solomon (`improver/visual_baselines/<repo>/`) or inside each repo (versions with its UI)?
6. **Discovery trigger:** operator button only first, add scheduling later? *(recommended)*

## Note — this respects the never-hand-patched invariant
The **sandbox + visual gate + discovery lane live in Solomon** (the harness — built directly). The
**sover-side prerequisite** (honoring `SOVER_HOME` in dev) and every UI/UX fix the discovery lane
finds are shipped **by sover's pi agent under the gate**, never hand-patched.
