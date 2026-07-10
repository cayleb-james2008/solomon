# Solomon RSI Re-architecture — Implementation Plan (MoA Brain + Ephemeral Workers)

> **Status**: Decision-complete spec. Implementation NOT started. Awaits lead approval before mutating work begins.
> **Binding inputs**: `docs/RSI_MOA_RESEARCH.md` (verdicts section 1.6 / 2.5 / 3.4 / 4.5 / 5.4) + operator decisions.
> **Date**: 2026-07-09. **Author**: ultra-planner.

---

## 0. Goal + Success Criteria

### Goal (one line)
Transform Solomon from a pi-agent-orchestrator into a single autonomous AI-CEO with a Mixture-of-Agents brain + ephemeral specialized workers, then **prove it performs at goal-state for at least 1 hour**.

### Success criteria (the 1-hour proof — section 8)
The plan succeeds iff, within 60 minutes of arming the rebuilt Solomon, ALL hold simultaneously:
1. **MoA brain firing**: `runtime/autopilot_events.jsonl` shows iterations using the MoA path (parallel Layer-1 workers then aggregator), NOT `proof_required` spins or single-model fallback loops.
2. **At least one shipped PR**: a lane's `history.jsonl` shows `status: "shipped"` with a PR URL, timestamped within the proof hour.
3. **No stuck lane**: no lane in `autopilot_state.json` is `proof_required` with `requires_ai: false` and a stuck-sweep count climbing.
4. **Honest ops**: `runtime/ops_status.json` probes are green or honest-red (a red probe names a real missing outcome), never false-green.
5. **CEO rhythm**: `morning_plan`/`evening_summary` fired during the hour (or are due-but-cooled, honestly).
6. **Signal not noise**: `runtime/_notify.jsonl` is not flooded (no repeated dedup-blocked pages; no quota-error storm).

### Non-goals (explicit)
- **NOT** shipping the full 4-worker MoA on every lane immediately — v1 ships a minimal MoA (aggregator + 1-2 workers) to prove the loop; the 4-worker roster is the target state, not the proof-hour bar.
- **NOT** rewriting the gate, anti-gaming, freshness, supervisor recovery, or money-guard machinery — these are PRESERVED (see section 5).
- **NOT** deleting the per-repo backlogs — they are repurposed as worker input (see section 6).
- **NOT** adding Claude/Anthropic as a 5th model, a TRINITY learned coordinator, or streaming/visual phases in v1 (research 1.6 flags these as Phase-2).

---

## 1. Operator Decisions (binding — override SOLOMON_RSI.md where they conflict)

These are the operator's ratified decisions, recorded here so the implementer slices do not re-derive them:

| # | Decision | Effect on SOLOMON_RSI.md |
|---|---|---|
| D1 | **Dissolve the pi-agent lane contract model.** Solomon becomes ONE autonomous AI-CEO acting directly on managed repos. | Retires `agent-implements-under-contract` + `never-hand-patched` (the keystone) as sole-author invariants. |
| D2 | **Solomon spawns ephemeral specialized workers** (system-prompt S + skill K + task T + capability envelope). | Generalizes the single per-repo AGENT.md into N role-specific worker prompts. |
| D3 | **2-layer MoA brain**: glm-5.2 aggregator+verifier; workers = kimi-k2.7-code (plan), deepseek-v4-pro (implement), minimax-m2.7 (ideate/probe). | Replaces the single-model `ollama_chat`/`run_pi` calls in the implement + plan + review phases. |
| D4 | **Daily call budget raised to 500** (`repos.json` autopilot `daily_call_budget` 40 to 500) + weekly cap raised so it does not re-bind. | Loosens the budget that currently puts the fleet in a 24h cooldown. |
| D5 | **Highest-leverage lanes: asmodeus, kairos, sover** (live-money + live-audience). dotz/maki/daedulus/solomon-self are lower priority but in scope. | Focuses the proof-hour effort. |
| D6 | **Full autonomous execution granted.** No per-step operator gating. The 1-hour proof is the stop condition. | Removes the human-in-the-loop PR-merge gate for the proof hour (auto-merge is already the default). |

**Operator-lifted keystones** (the operator explicitly said *"solomon is cool to change everything and anything"*):
- `never-hand-patched` — Solomon may directly edit a managed repo's working tree (via the MoA implementer worker) as part of an iteration. The `rsi/*` branch + gate + PR discipline is PRESERVED (reversibility + auditability), but the "only the pi agent touches the tree" restriction is gone.
- `agent-implements-under-contract` — the per-repo AGENT.md is no longer the sole author; role-specialized MoA workers author, gated by the runner.

**Operator-preserved keystones** (STAY — the MoA brain is downstream of all of these):
- `gate-enforced-by-runner` — the Rust runner parses the gate, never the model. Unchanged.
- `never-edit-the-oracle` — `repos.json` gate/EVAL_CMD/freshness/budget are operator-owned; the MoA brain cannot edit them. Unchanged.
- `compounding-base` — every iteration still branches off the verified-best integration tip. Unchanged.
- `branch-per-iteration` — fresh `rsi/*` branch each iteration. Unchanged.
- `pr-only-shipping-with-auto-revert` — ship via PR, revert on red. Unchanged.
- `supervisor-authorized-recovery` — the RUNG-0/1/2 ladder stays; the MoA brain does not bypass it. Unchanged.
- **NO-MONEY-OUT guard** (`money_guard.rs`) — fail-closed, outermost default-DENY. Unchanged, untouched in this work.

---

## 2. Architecture (citing research verdicts)

### 2.1 The MoA brain — where it lives

**Verdict (research 1.6 + 3.4)**: a 2-layer MoA with role-specialized workers + a separate adversarial verifier, all runner-adjudicated. Replace the single-model LLM call with `async-openai` + `tokio::join!` for parallel Layer-1 fan-out.

**New module**: `src-tauri/src/improver/brain.rs` — the MoA brain. It does NOT replace `run_pi` wholesale; it replaces the *model-call* layer. The existing `run_pi` (the pi-CLI subprocess spawn) is the *implementer worker's* execution path. The MoA brain orchestrates WHICH workers run and HOW their outputs are aggregated.

**Two integration points** (the only places `ollama_chat`/`run_pi` are called for reasoning):

| # | Call site today | What it does | Becomes (MoA) |
|---|---|---|---|
| A | `ceo.rs:648` `ollama_chat(CEO_MODEL=minimax-m3)` in `morning_plan` | The CEO's daily per-lane goal planner. Single model call to JSON lane goals. | **MoA aggregator-only** (no workers) for v1: the CEO planner synthesizes per-lane goals. Workers are overkill for goal-setting; the aggregator alone is the proven MoA-Lite n=1 path. The Aggregate-and-Synthesize prompt still applies (1 input). |
| B | `improver/iteration.rs:605` `dispatch_engineering_on_ctx(..., TaskKind::Code, ...)` | The implement pass — the ONE coding session per iteration. Today: single `pi::run_pi`. | **Full MoA** (Layer-1 workers then Layer-2 aggregator). The implementer worker (deepseek-v4-pro) produces the diff; the planner (kimi-k2.7-code) + ideator (minimax-m2.7) run in parallel as Layer-1 proposers; the aggregator (glm-5.2) synthesizes the single best approach and the implementer executes it. |
| C | `improver/phases.rs:151` `run_plan_phase` | The pre-implement advisory planner. Single `phase_run_pi` call. | **MoA Layer-1 plan worker** (kimi-k2.7-code) — the planner is already a worker in the MoA shape; `run_plan_phase` becomes a thin wrapper that calls the plan worker directly (no aggregation needed for a read-only plan). |
| D | `improver/phases.rs:53` `run_review_phase` | The adversarial JUDGE (post-gate, pre-ship). Single `phase_run_pi` call, regex-parsed verdict. | **MoA Verifier** (research 4.5 rail #2): a SEPARATE glm-5.2 call with the adversarial system prompt, runner-parsed. This is the SAME mechanism, just re-named as the MoA verifier role. Fail-open unchanged. |
| E | `ceo/focus.rs:280` `ollama_chat(CEO_MODEL, ...)` | The CEO focus/refine call. | **MoA aggregator-only** (same as A). |

**The MoA brain shape (research 3.4, adapted)**:

```
Layer 1 (parallel, tokio::join!):
  spawn_worker(role="plan",   PLAN_PROMPT,   plan_skill,   task, "kimi-k2.7-code")   -> plan_text
  spawn_worker(role="ideate", IDEATE_PROMPT, ideate_skill, task, "minimax-m2.7")     -> ideate_text
  [implementer is NOT a Layer-1 proposer — it EXECUTES the synthesized plan, see below]
Layer 2 (sequential, needs Layer-1):
  spawn_worker(role="aggregate", AGGREGATE_SYNTHESIZE_PROMPT, "", task_with_layer1_outputs, "glm-5.2") -> synthesized_plan
Execute (the implementer worker runs the synthesized plan):
  spawn_worker(role="implement", IMPLEMENT_PROMPT, implement_skill, synthesized_plan, "deepseek-v4-pro") -> diff
  [the runner then runs the gate, parses, anti-gaming-checks — UNCHANGED]
Verify (post-gate, pre-ship — separate call):
  spawn_worker(role="verify", VERIFY_PROMPT, review_skill, committed_diff, "glm-5.2") -> verdict_json
  [runner parses verdict; reject -> revert; unparseable -> fail-open ship]
```

**Why this shape** (not the research's exact "3 Layer-1 workers including implementer"):
- The research's 1.6 had the implementer as a Layer-1 proposer producing a diff. But the gate requires the diff to be applied to a real `rsi/*` branch and the runner to execute the gate. A "proposed diff" that is not applied cannot be gated.
- **The clean mapping**: Layer-1 = plan + ideate (both produce TEXT — a plan and a list of ideas). Layer-2 = aggregator synthesizes the single best plan. The implementer worker then EXECUTES that plan as a real coding session (the existing `run_pi` shape, just with deepseek-v4-pro as the model + the synthesized plan as the task). The gate then runs on the real diff. The verifier is the existing review phase.
- This keeps the gate 100% runner-enforced and the diff real (applied, not proposed). The MoA brain's value is upstream of the gate: better planning + synthesis -> better diffs -> higher ship rate.

### 2.2 The `spawn_worker` async fn

**Signature** (in `src-tauri/src/improver/brain.rs`):

```rust
/// Spawn ONE ephemeral MoA worker. A worker = system-prompt S + skill K + task T + model M.
/// Calls Ollama Cloud's OpenAI-compatible endpoint via `async-openai`, with OpenRouter fallback.
/// Returns the assistant content string, or an error (caller retries on the fallback endpoint).
async fn spawn_worker(
    client: &async_openai::Client<async_openai::config::OpenAIConfig>,
    fallback_client: Option<&async_openai::Client<async_openai::config::OpenAIConfig>>,
    role: &str,                       // "plan" | "ideate" | "aggregate" | "implement" | "verify"
    system_prompt_s: &str,             // the role contract (const string in Rust)
    skill_k: &str,                     // loaded from disk: improver/<repo>/skills/<role>.md (empty if none)
    task_t: &str,                      // the user message
    model: &str,                       // "kimi-k2.7-code" (bare; Ollama Cloud resolves to :cloud)
    fallback_model: Option<&str>,      // OpenRouter slug: "moonshotai/kimi-k2.7-code"
    reasoning_effort: Option<&str>,    // "high" for deep models, None for minimax-m2.7
) -> Result<String, BrainError>;
```

**Implementation notes**:
- Uses `async-openai` (research 3.3: MIT, reqwest-based, mature, `byot` for `reasoning_effort`). Add `async-openai = { version = "0.41", features = ["byot"] }` to `src-tauri/Cargo.toml`.
- The request shape (research 3.2): `messages: vec![system(S), system(K), user(T)]` — two system messages (role contract + skill), one user (task). Ollama Cloud + OpenRouter both accept multiple system messages.
- Model ID: send the BARE name (`"kimi-k2.7-code"`) to Ollama Cloud (research 2.1 note: Solomon's existing `ollama_chat` already does this for glm-5.2 and it resolves). For OpenRouter fallback, send the slug (`"moonshotai/kimi-k2.7-code"`).
- `reasoning_effort`: `"high"` for kimi-k2.7-code + deepseek-v4-pro + glm-5.2 (deep); `None` for minimax-m2.7 (cheap, Medium tier).
- Error handling: on a 429/transport error, retry once on the fallback client (OpenRouter) with the fallback slug. On a second failure, return `BrainError::WorkerFailed(role, reason)` — the caller (the iteration loop) degrades gracefully (see section 7).

**The parallel fan-out** (research 3.4):

```rust
let (plan, ideate) = tokio::join!(
    spawn_worker(&client, fallback, "plan",   PLAN_PROMPT,   plan_skill,   task, "kimi-k2.7-code",  Some("moonshotai/kimi-k2.7-code"),  Some("high")),
    spawn_worker(&client, fallback, "ideate", IDEATE_PROMPT, ideate_skill, task, "minimax-m2.7",   Some("minimax/minimax-m2.7"),       None),
);
```

`tokio::join!` runs both to completion (waits for both). If one fails, the aggregator degrades (research 5.4: n=1 proposer still beats the aggregator alone — worth running degraded rather than parking the iteration).

### 2.3 Ephemeral worker spawning — system prompts + skills + capability envelope

**Worker = system-prompt S + skill K + task T + capability envelope** (research 3.1, mirroring the OpenCode Task-tool subagent pattern).

**Where system prompts live**: `const` strings in `src-tauri/src/improver/brain.rs`. Each role's S is the role's standing contract (the analog of `ultra-implementer.md`'s body). They are short, role-specific, and compile-time-fixed. Examples (the implementer will write the exact text):

- `PLAN_PROMPT` — "You are the Solomon planner. Read the repo read-only. Draft the single highest-leverage improvement + acceptance criteria. Output markdown. Do NOT write files."
- `IMPLEMENT_PROMPT` — "You are the Solomon implementer. You are given a synthesized plan. Implement it on the current `rsi/*` branch, test-first where tests exist. Your diff must pass the gate. Do NOT edit the gate, EVAL_CMD, or any operator-owned oracle."
- `IDEATE_PROMPT` — "You are the Solomon ideator. Propose 5-8 leverage-ranked alternative improvements toward the north-star goal. Trivial chores forbidden."
- `AGGREGATE_SYNTHESIZE_PROMPT` — **verbatim from the MoA paper Table 1** (research 1.2): "You have been provided with a set of responses from various open-source models to the latest user query. Your task is to synthesize these responses into a single, high-quality response. It is crucial to critically evaluate the information provided in these responses, recognizing that some of it may be biased or incorrect. Your response should not simply replicate the given answers but should offer a refined, accurate, and comprehensive reply to the instruction. Ensure your response is well-structured, coherent, and adheres to the highest standards of accuracy and reliability. Responses from models: 1. [Model Response from A_{i,1}] 2. [Model Response from A_{i,2}] ... n. [Model Response from A_{i,n}]"
- `VERIFY_PROMPT` — the existing `review.md` adversarial contract (the REVIEW phase's system prompt), output as JSON `{verdict, reasons}` (runner-parsed).

**Where skills live**: markdown files on disk at `improver/<repo>/skills/<role>.md` (created on first sight, operator-editable without recompiling — research 3.4 note 5). The brain loads them at runtime; a missing skill file returns empty string (byte-identical to "no skill injected"). This mirrors how OpenCode injects `SKILL.md` into a subagent.

**Capability envelope enforcement** (research 3.1, mirroring OpenCode frontmatter `permission: {edit: deny|allow}`):
- The envelope is NOT enforced by the worker (a model cannot be trusted to self-police). It is enforced by the RUNNER, two ways:
  1. **Tool whitelist** — the implementer worker (deepseek-v4-pro) runs via the EXISTING `run_pi` to `build_pi_argv` path, which already restricts the agent to read/write/edit/list/search/run_gate (the `ENGINEERING_ALLOWED_TOOLS` closed set in `ceo/orchestrator.rs:156`). A planner/ideator/verifier worker is a pure chat-completions call (no file access at all — it only sees the prompt + repo context injected as text).
  2. **Scope globs** — the existing `tiers.money_globs` / `tiers.protected` blast-radius config in `repos.json` (already enforced by the ship gate). The implementer worker's edits are gated by the SAME ship gate. A `protected` path (e.g. kairos's `promote.py`) is write-protected regardless of which worker touched it.

**How this replaces the per-repo pi-agent child-process spawn**: today, each lane spawns a `pi` CLI child process (via `run_pi` to `build_pi_argv`). The MoA brain keeps this for the IMPLEMENTER worker (it is the only worker that touches files — and `run_pi`'s pipe-drain + tree-kill + budget-metering is load-bearing). The plan/ideate/aggregate/verify workers are pure chat-completions calls via `async-openai` — no child process, no file access, no pipe deadlock risk. The planner/ideator read the repo context injected as text (the iteration loop already builds the task string with repo context).

### 2.4 Budget changes (research 5)

**Three edits, in order** (research 5.1 critical correction: the current cap is WEEKLY, not daily):

1. **`repos.json` autopilot block** — `daily_call_budget: 40` to `500`. This is the autopilot's per-day planning target. (Unit: calls/day per research 5.4 flag — but the operator's intent is "500/day" headroom, so 500 it is.)

2. **`src-tauri/src/improver/budget.rs`** — `const DEFAULT_WINDOW_CAP: u64 = 500;` (weekly, per-endpoint) to `3500`. Rationale: 500/day x 7 = 3500/week per endpoint. This is the operator's explicit intent ("500/day"). The existing `park_until`/`consecutive_429` exponential backoff machinery handles real Ollama Cloud 429s — this cap is Solomon's own reserve-headroom rule, not an Ollama limit. 3500/week per endpoint x 4 endpoints = 14,000 calls/week fleet ceiling. At 5 calls/iteration = 2,800 iterations/week across 6 lanes = 466/lane/week = 66/lane/day. Well beyond the 1-hour proof bar.

   - **Risk note**: research 5.3 flags deepseek-v4-pro is Extra-High usage tier and may exhaust the Ollama Max plan's weekly GPU-time before the call cap. Mitigation: the existing per-endpoint `park_until` parks deepseek-v4-pro independently if it 429s; the MoA brain's degraded-mode fallback (research 5.4) runs the iteration without the implementer worker if it is parked. No new machinery — the existing rails handle it.

3. **`runtime/_provider_budget.json`** — replace the single `maki-cloud:glm-5.2` row with 4 per-endpoint rows (research 5.3 recommends the aggregator at 2x workers, but the operator's 500/day intent applies uniformly):

```json
{
  "endpoints": {
    "ollama-cloud:glm-5.2":         { "window_cap_calls": 3500, "window_started": 0, "spent_calls": 0, "park_until": 0, "consecutive_429": 0, "last_canary_pass": 0 },
    "ollama-cloud:kimi-k2.7-code":  { "window_cap_calls": 3500, "window_started": 0, "spent_calls": 0, "park_until": 0, "consecutive_429": 0, "last_canary_pass": 0 },
    "ollama-cloud:deepseek-v4-pro": { "window_cap_calls": 3500, "window_started": 0, "spent_calls": 0, "park_until": 0, "consecutive_429": 0, "last_canary_pass": 0 },
    "ollama-cloud:minimax-m2.7":    { "window_cap_calls": 3500, "window_started": 0, "spent_calls": 0, "park_until": 0, "consecutive_429": 0, "last_canary_pass": 0 }
  }
}
```

   - `<now>` = current Unix timestamp; the implementer computes it. Preserve any real `last_canary_pass` stamps from the existing row.
   - Also update the existing `maki-cloud:glm-5.2` key to `ollama-cloud:glm-5.2` (the ledger key is `<provider>:<model>`; the autopilot's provider is `ollama-cloud`, not `maki-cloud` — the `maki-cloud` key appears to be a stale/legacy entry. The implementer will verify the exact key format `budget.rs::endpoint_key` produces and match it.)

4. **Clear the autopilot cooldown**: `runtime/autopilot_state.json` currently has `cooldown: { since, until: +24h, reason: "daily call budget reached" }`. After the budget edits + rebuild, the fleet must be re-armed. The implementer edits `autopilot_state.json` to clear `cooldown` to null and reset `daily.calls` to 0. This is a state-file edit, not a code change.

**Rebuild**: `cargo build --release` in `src-tauri/`. Copy `target/release/solomon.exe` to repo-root `Solomon.exe` (per AGENTS.md distribution model).

### 2.5 The `brain` config block in repos.json (new)

Add to the `repos.json` autopilot block:

```jsonc
"brain": {
  "enabled": true,                    // feature flag — false falls back to the single-model dispatch_engineering_on_ctx path
  "aggregator": "glm-5.2",
  "verifier":   "glm-5.2",
  "workers": {
    "plan":      "kimi-k2.7-code",
    "implement": "deepseek-v4-pro",
    "ideate":    "minimax-m2.7",
    "probe":     "minimax-m2.7"
  },
  "layers": 2,
  "fallback_provider": "openrouter"
}
```

The `improver/ctx.rs::Ctx` gets a new field `moa_enabled: bool` (default `false`), refreshed from `repos.json` autopilot `brain.enabled` each iteration (mirroring how `plan_enabled`/`review_enabled` are refreshed from `pipeline.*`). When `false`, the iteration loop calls `dispatch_engineering_on_ctx` unchanged (the feature-flag fallback). When `true`, it calls `brain::run_moa_iteration`.

The `brain.rs` module reads the worker model IDs + the fallback provider from this block. The OpenRouter slugs (research 2.5) are derived: `zai/glm-5.2`, `moonshotai/kimi-k2.7-code`, `deepseek/deepseek-v4-pro`, `minimax/minimax-m2.7`.

---

## 3. Task Slices (ordered — proof-hour-critical first)

The lead dispatches these to `ultra-implementer` subagents with disjoint write scopes. Slices 1-2 are the proof-hour unlock; slices 3-4 are the full MoA; slice 5 is the cleanup + docs.

### Slice 1 — Budget revive + cooldown clear (PROOF-HOUR-CRITICAL)

**Goal**: Unbind the fleet from the 24h cooldown so the autopilot can run today.

**Write scope**: `repos.json` (autopilot block), `src-tauri/src/improver/budget.rs` (one constant), `runtime/_provider_budget.json`, `runtime/autopilot_state.json` (the `cooldown` + `daily` fields ONLY — slice 2 owns `stuck`/`queue`).

**Changes**:
1. `repos.json`: `autopilot.daily_call_budget` 40 to 500.
2. `budget.rs`: `const DEFAULT_WINDOW_CAP: u64 = 500` to `3500`.
3. `_provider_budget.json`: replace with the 4-endpoint ledger (section 2.4 step 3), preserving any real `last_canary_pass` stamps.
4. `autopilot_state.json`: set `cooldown` to `null`, `daily.calls` to 0. (Do NOT touch `stuck`/`queue` — slice 2 owns those.)

**Tests**:
- The existing `budget.rs` test `preflight_seeds_endpoint_on_first_sight_with_default_cap` asserts `window_cap_calls == 500`. Update it to `3500`. (The test reads the const; updating the const + the assertion in the same commit.)
- The existing `cap_exhausted_is_parked_until_window_end` test seeds `window_cap_calls: 500` explicitly — it still passes (it seeds its own value, does not read the const).
- Run `cargo test -p solomon --lib improver::budget` in `src-tauri/`.

**Verification**: `cargo build --release`; `solomon state` shows `daily.calls: 0`, no cooldown, 4 budget endpoints.

**Rollback**: revert the 4 files. The `daily_call_budget: 40` + `DEFAULT_WINDOW_CAP: 500` path is the current behavior.

**1-hour proof impact**: WITHOUT this slice, the fleet is in a 24h cooldown and the proof hour cannot start. This slice alone unbinds the fleet on the EXISTING single-model brain — the proof hour can begin with glm-5.2-only iterations while slices 2-4 land.

### Slice 2 — Stuck-lane unblocking (PROOF-HOUR-CRITICAL)

**Goal**: Clear the three stuck lanes so the autopilot has lanes to run (not just `proof_required` spins).

**Write scope**: the three managed repos (`dotz`, `daedulus`, `sover`) — git operations + state files only, NO source edits to those repos; AND `runtime/autopilot_state.json` (the `stuck` block + the `queue` entries for the 3 lanes ONLY).

**Per-lane actions** (see section 6 for full rationale):

**dotz** (base-gate-RED, 141 stuck sweeps, dirty tree with real work):
- The dirty tree has substantial real work: AGENTS.md, README.md, 3 docs, 11 `dotz-core/src/*` files, web/app.js (verified via `git status --porcelain` — 16 modified files).
- **PRESERVE the work**: `git stash push -u -m "solomon-MoA: preserve dotz dirty work pre-base-reset"` (stash, do not destroy).
- **Create a recovery branch** for the stashed work: `git checkout -b solomon/preserved-dotz-work-<stamp>` off main, `git stash pop`, `git add -A`, `git commit -m "WIP: preserve dotz in-progress work before MoA rebase"`, `git push -u origin solomon/preserved-dotz-work-<stamp>` (safe on origin). A future dotz iteration (under the MoA brain) can land this work via a real PR.
- **Reset main clean**: `git checkout main`, `git reset --hard origin/main`, `git clean -fd` (the stash is preserved on the recovery branch; the base must be pristine).
- **Verify the base gate**: `cargo test -p dotz-core` in the dotz repo. If it collects tests and passes (green), the lane is unblocked. If it STILL collects 0 tests, that is a real build break in main — escalate to the operator (do not fabricate a green).
- **Clear the stuck state**: edit `runtime/autopilot_state.json` — remove `dotz` from `stuck`, replace its `proof_required` queue entry with a fresh `implement` entry.

**daedulus** (130h stale, stale_lock, no_objective):
- It is a `no_objective` lane (no freshness emitter — repos.json explicitly marks it). The "130h stale" is a heartbeat-stale signal. The `stale_lock` is a leftover lockfile.
- **Clear the lock**: remove `runtime/daedulus/*.lock` files.
- **Reset the lane**: `solomon stop daedulus` (if running), `solomon start daedulus` (RUNG-0).
- **Accept idle**: daedulus is `no_objective` + low-leverage (not in the top-3). For the proof hour, it is acceptable for it to idle. The autopilot will skip it if there is no fresh work. No forced iteration needed.

**sover** (gate_red_streak, last 3 iterations reverted):
- The gate-red-streak means the implementer produced diffs that failed the gate. Under the new MoA brain (slice 3), the synthesized plan + deepseek-v4-pro implementer should produce higher-quality diffs. But the streak itself does not block — the supervisor's anti-thrash ladder handles it.
- **Safe unblock**: `solomon stop sover`, `solomon start sover` (RUNG-0 — clears the consecutive-revert counter). Do NOT hand-patch the gate (it is operator-owned).
- **Verify the gate runs green on base**: `.venv\Scripts\python -m unittest discover -s tests -t tests` in the sover repo. If the BASE gate is red, that is a real blocker (the loop cannot measure a gain from a red base) — escalate to the operator, do not fabricate. If green, the lane is unblocked.

**Tests**: no code tests (this slice is git + state-file ops). Verification is `solomon state` showing no `proof_required` lanes with climbing stuck-sweep counts.

**Rollback**: the dotz stash + recovery branch are reversible (`git stash drop`, `git branch -D`). The state-file edits are reversible (re-add to `stuck`, restore `proof_required`). The git resets are reversible if the operator objects (the stashed work is preserved on the named branch on origin).

**1-hour proof impact**: unblocks dotz + sover for the autopilot to pick up. daedulus stays idle (acceptable).

**Coordination with slice 1**: both edit `autopilot_state.json`. Slice 1 clears `cooldown`/`daily` fields; slice 2 clears `stuck`/`queue` fields. Disjoint. Safe to run in parallel.

### Slice 3 — Minimal MoA brain (aggregator + implementer worker)

**Goal**: Stand up the MoA brain's core path so iterations use the multi-model shape.

**Write scope**: `src-tauri/Cargo.toml` (add `async-openai`), NEW file `src-tauri/src/improver/brain.rs`, `src-tauri/src/improver/mod.rs` (add `mod brain`), `src-tauri/src/improver/iteration.rs` (rewire the implement dispatch), `src-tauri/src/improver/ctx.rs` (add `moa_enabled` field), `repos.json` (add the `brain` autopilot block — section 2.5).

**Changes**:
1. `Cargo.toml`: add `async-openai = { version = "0.41", features = ["byot"] }` and ensure `tokio` has `rt-multi-thread` + `process` features (verify; if Tauri already pulls tokio, just add the features).
2. `improver/brain.rs` (NEW): implement `spawn_worker` (section 2.2), the const system prompts (section 2.3), the `BrainConfig` struct (loaded from `repos.json` autopilot `brain` block — section 2.5), and `run_moa_iteration(ctx, task) -> RunOut` — the orchestrator that runs Layer-1 (plan+ideate) then Layer-2 (aggregate) then execute (implementer via `run_pi`) and returns the RunOut for the existing gate path.
3. `improver/mod.rs`: add `pub mod brain;`.
4. `improver/ctx.rs`: add `pub moa_enabled: bool` field (default `false`), refreshed from `repos.json` autopilot `brain.enabled` in the existing config-refresh path (mirror `plan_enabled`/`review_enabled`).
5. `improver/iteration.rs:605`: wrap the `dispatch_engineering_on_ctx(..., TaskKind::Code, ...)` call. When `ctx.moa_enabled == true`, call `brain::run_moa_iteration(ctx, &task)` instead. When `false`, the existing path runs unchanged (the feature flag — see section 7 rollback).
6. `repos.json`: add the `brain` block to the autopilot (section 2.5) with `enabled: true`.

**Minimal v1 shape** (to hit the proof hour): the MoA brain runs Layer-1 with ONLY the planner (kimi-k2.7-code) — skip the ideator for v1 (it is an escape-hatch phase, not needed for the proof hour). Layer-2 aggregator (glm-5.2) synthesizes. The implementer (deepseek-v4-pro) executes via the existing `run_pi` (with the synthesized plan as the task). The verifier (glm-5.2, separate call) is wired in slice 4 (for v1, the existing `run_review_phase` single-model path stays — it is already a separate adversarial call).

This is the MoA-Lite n=1-proposer shape (research 1.2: 59.3% win rate, cost-effective). It proves the multi-model path end-to-end. The full 3-worker Layer-1 + verifier rewiring is slice 4.

**Tests**:
- `brain.rs` unit tests: `spawn_worker` with a mock client (inject a trait `LlmClient` so tests do not hit the network); test that Layer-1 outputs are passed to Layer-2 correctly; test degraded mode (one Layer-1 worker fails -> aggregator runs with the survivor); test the feature-flag fallback (`moa_enabled == false` -> returns the existing single-model RunOut shape).
- A contract test: `run_moa_iteration` with `moa_enabled == false` produces a byte-identical RunOut to the pre-MoA `dispatch_engineering_on_ctx` path (proving the flag is a clean fallback).

**Verification**: `cargo test -p solomon --lib improver::brain`; `cargo build --release`; a manual `solomon run-improver --repo <path> --name maki --once` with `moa_enabled=true` shows the MoA path in the heartbeat (`phase: "moa_aggregate"` etc.).

**Rollback**: set `brain.enabled = false` in the autopilot brain config (or remove the `brain` block from `repos.json`). The iteration loop falls back to `dispatch_engineering_on_ctx` (the pre-MoA single-model path). No rebuild needed (it is a runtime config read). The `brain.rs` module stays compiled but unused.

**1-hour proof impact**: this slice IS the MoA brain. Without it, slice 1+2 run the fleet on the single-model path (still a valid proof that the fleet loops, but not the MoA goal-state). The lead should land slice 1+2 first, START the proof-hour clock on the single-model path (fleet looping, lanes unblocked), then land slice 3 and re-arm to upgrade to the MoA path mid-proof. The skeptic grades the FINAL state (MoA brain firing).

### Slice 4 — Full MoA (3-worker Layer-1 + verifier rewiring)

**Goal**: Complete the 4-worker roster + the adversarial verifier as the MoA verifier role.

**Write scope**: `src-tauri/src/improver/brain.rs` (extend), `src-tauri/src/improver/phases.rs` (rewire review to call `spawn_worker`), `improver/<repo>/skills/*.md` (new files — the skill inputs).

**Changes**:
1. `brain.rs`: add the ideator worker to the Layer-1 `tokio::join!` (minimax-m2.7). Add the `AGGREGATE_SYNTHESIZE_PROMPT` verbatim (research 1.2 Table 1). Add the degraded-mode path (if ideator fails, aggregate from plan-only; if planner fails, aggregate from ideate-only; if both fail, skip aggregation and run the implementer with the raw task — the MoA paper's n=0 fallback is just the implementer alone, which is the pre-MoA baseline).
2. `phases.rs::run_review_phase`: rewire to call `brain::spawn_worker(role="verify", VERIFY_PROMPT, review_skill, committed_diff, "glm-5.2")` instead of `pi::phase_run_pi`. The runner-parsed verdict (`{verdict, reasons}` JSON extraction) stays identical. Fail-open unchanged.
3. Create `improver/<repo>/skills/{plan,implement,ideate,verify}.md` for each lane — the skill instructions K. Start with minimal content (the role's standing instructions; can be enriched later). A missing file returns empty string (graceful).

**Tests**: extend `brain.rs` tests for the 3-worker Layer-1; test the verifier rewiring produces the same verdict-parsing contract as the existing `run_review_phase`.

**Rollback**: the verifier rewiring is behind the same `moa_enabled` flag. If the MoA verifier produces worse verdicts than the single-model reviewer, flip the flag.

**1-hour proof impact**: this is the "full goal-state" upgrade. The proof hour can pass with slice 3's minimal MoA; slice 4 makes it the complete research-spec shape.

### Slice 5 — Pi-agent dissolution + docs + final proof

**Goal**: Formally retire the pi-agent-as-sole-author invariant; document the new architecture; run the 1-hour proof.

**Write scope**: `SOLOMON_RSI.md` (update invariants), `AGENTS.md` (update the keystone description), `improver/<repo>/AGENT.md` (repurpose — see section 6).

**Changes**:
1. `SOLOMON_RSI.md`: update the invariants section to reflect D1-D6 (section 1). Mark `never-hand-patched` and `agent-implements-under-contract` as DISSOLVED (with the operator's ratification noted). Add a new "MoA brain" section documenting the architecture. PRESERVE all other invariants verbatim.
2. `AGENTS.md`: update the keystone paragraph to reflect that Solomon now acts directly via the MoA brain + ephemeral workers, but the gate/PR/oracle invariants hold.
3. `improver/<repo>/AGENT.md`: repurpose from "the pi agent's sole contract" to "the lane's north-star + the worker system-prompt source." Keep the backlog references. (See section 6.)
4. Run the 1-hour proof procedure (section 8). The skeptic grades against the success criteria.

**Rollback**: docs are reversible (git revert). The pi-agent dissolution is a doctrine change, not a code change — the code already supports the MoA path (slices 3-4); the docs just describe it.

---

## 4. Data Flow (the MoA iteration, end-to-end)

```
autopilot tick (fleet.rs)
  -> picks a lane (asmodeus/kairos/sover high priority)
  -> solomon run-improver --repo <path> --name <lane> --once
  -> improver::run::main -> improver::iteration::one_iteration(ctx)
    1. freshness::short_circuit (UNCHANGED — parks if no fresh objective data)
    2. gitops preflight (clean base, branch rsi/iter-<stamp>) (UNCHANGED)
    3. gates::base_gate (runner runs the gate on the base) (UNCHANGED — gate-enforced-by-runner)
    4. backlog::select_item (pick one improvement) (UNCHANGED)
    5. **MoA brain** (NEW, slice 3-4):
       a. brain::run_moa_iteration(ctx, task):
          - Layer-1: tokio::join!(plan worker [kimi-k2.7-code], ideate worker [minimax-m2.7])
          - Layer-2: aggregate worker [glm-5.2] -> synthesized_plan
          - Execute: run_pi(ctx, synthesized_plan, ...) [deepseek-v4-pro, the implementer worker]
            (the EXISTING run_pi path — budget preflight, record_call, pipe-drain, tree-kill)
          - returns RunOut (the implementer's output)
       b. (if moa_enabled == false: dispatch_engineering_on_ctx — the pre-MoA path)
    6. pi::final_text -> summary (UNCHANGED)
    7. gates::run_gate on the rsi/* branch (UNCHANGED — gate-enforced-by-runner)
    8. anti-gaming checks (UNCHANGED — tests not deleted, counts not dropped)
    9. ship::ship_or_revert (UNCHANGED — PR + auto-revert)
    10. **MoA verifier** (NEW, slice 4): if ctx.review_enabled:
        brain::spawn_worker(role="verify", committed_diff, "glm-5.2") -> JSON verdict
        (runner-parsed; reject -> revert; unparseable -> fail-open ship)
    11. visual E2E (UNCHANGED)
    12. history.jsonl append (UNCHANGED)
  -> loop
```

The MoA brain is steps 5 + 10. Everything else is the existing, gate-enforced, anti-gaming-checked, PR-shipped RSI loop. The brain is UPSTREAM of the gate (produces the candidate diff) and the verifier is a SEPARATE post-gate call (adversarial check). Neither the aggregator nor the verifier grades the gate — the runner does.

---

## 5. Gate + Anti-Gaming Preservation (research 4.5)

**What is PRESERVED byte-for-byte** (the MoA brain changes NOTHING about these):

| Invariant | Mechanism | Preserved? |
|---|---|---|
| `gate-enforced-by-runner` | `gates.rs` runs the gate, parses pass/fail + counts + EVAL_CMD float itself. The MoA brain produces the candidate diff; the gate adjudicates. | UNCHANGED |
| `never-edit-the-oracle` | `repos.json` gate/EVAL_CMD/freshness/budget are operator-owned. The MoA workers cannot edit `repos.json` (the implementer worker runs via `run_pi` whose shims refuse `gh` + the branch/push git verbs; the planner/ideator/verifier are pure chat calls with no file access). | UNCHANGED |
| `compounding-base` | Every iteration branches off the verified-best integration tip. | UNCHANGED |
| `branch-per-iteration` | Fresh `rsi/iter-<stamp>` branch each iteration. | UNCHANGED |
| `pr-only-shipping-with-auto-revert` | Ship via PR, revert on red (local or CI). | UNCHANGED |
| `supervisor-authorized-recovery` | RUNG-0/1/2 ladder; the MoA brain does not bypass it. | UNCHANGED |
| **NO-MONEY-OUT guard** | `money_guard.rs` — fail-closed, outermost default-DENY. The MoA brain's workers are dispatched through `dispatch_engineering_on_ctx` which runs the money-guard FIRST. | UNCHANGED, UNTOUCHED |
| Anti-gaming checks (tests not deleted, counts not dropped, skips not added, EVAL_CMD not regressed) | `iteration.rs` anti-gaming block, all Rust, all deterministic. | UNCHANGED |
| The Verifier is a SEPARATE call, runner-parsed | research 4.5 rail #2: the verifier sees the committed diff, NOT the worker outputs; its output is JSON-parsed by Rust; reject -> revert; unparseable -> fail-open ship. | ENFORCED (slice 4) |
| Model diversity as anti-collusion | research 4.5 rail #4: aggregator (glm-5.2) is a different model family from all workers. | BY DESIGN |

**What is DISSOLVED** (operator-ratified):
- `never-hand-patched` — Solomon may directly edit a managed repo's working tree via the MoA implementer worker. The `rsi/*` branch + gate + PR discipline is preserved.
- `agent-implements-under-contract` — the per-repo AGENT.md is no longer the sole author; role-specialized MoA workers author, gated by the runner.

**The anti-gaming keystone** (research 4.2): "the aggregator (glm-5.2) NEVER grades. The MoA paper's aggregator synthesizes a *better answer* — it does not judge pass/fail. The gate verdict (green/red) is produced by the Rust runner running the gate and parsing the numeric result itself." This is preserved by design: the MoA brain is UPSTREAM of the gate (produces the diff candidate); the runner decides if it ships.

---

## 6. Stuck-Lane Unblocking (detailed — slice 2)

### dotz (base-gate-RED, 141 stuck sweeps, dirty tree)

**Current state** (from `autopilot_state.json` + `git status`):
- Heartbeat: `status: "error"`, `phase: "preflight"`, `reason: "base_gate_red_persistent"`.
- The base gate returns `{passed: 0, failed: 0, errors: 0, skipped: 0, collected: 0, green: false}` — this means the gate command (`cargo test -p dotz-core`) collected ZERO tests. That is a compile/build failure, not a test failure — the dirty tree likely does not build.
- Dirty tree: 16 modified files (AGENTS.md, README.md, 3 docs, 11 `dotz-core/src/*`, web/app.js). This is real, substantial work — NOT agent cruft.
- Stuck sweeps: 141 (the watchdog has tried 141 times).

**Safe unblock** (operator-loosened rules: Solomon may directly act):
1. **PRESERVE the work**: in the dotz repo, `git stash push -u -m "solomon-MoA: preserve dotz dirty work pre-base-reset"`.
2. **Create a recovery branch**: `git checkout -b solomon/preserved-dotz-work-<stamp>`, `git stash pop`, `git add -A`, `git commit -m "WIP: preserve dotz in-progress work before MoA rebase"`. Push the branch (`git push -u origin solomon/preserved-dotz-work-<stamp>`) so it is safe on origin. A future dotz iteration (under the MoA brain) can land this work via a real PR.
3. **Reset main clean**: `git checkout main`, `git reset --hard origin/main`, `git clean -fd` (the stash is preserved on the recovery branch; the base must be pristine).
4. **Verify the base gate**: `cargo test -p dotz-core` in the dotz repo. If it collects tests and passes (green), the lane is unblocked. If it STILL collects 0 tests, that is a real build break in main — escalate to the operator (do not fabricate a green).
5. **Clear the stuck state**: edit `runtime/autopilot_state.json` — remove `dotz` from `stuck`, replace its `proof_required` queue entry with a fresh `implement` entry.

**Why this is safe under the new rules**: the operator lifted `never-hand-patched`. Solomon directly acting to preserve work + reset the base is now permitted. The work is PRESERVED (not destroyed) on a named branch. The base reset is RUNG-0 reversible (`git reset --hard origin/main` never discards pushed commits; the recovery branch is on origin).

### daedulus (130h stale, stale_lock, no_objective)

**Current state**: `no_objective` lane (no freshness emitter — repos.json explicitly marks it). The "130h stale" is a heartbeat-stale signal. The `stale_lock` is a leftover lockfile.

**Safe unblock**:
1. **Clear the lock**: remove `runtime/daedulus/*.lock` files.
2. **Reset the lane**: `solomon stop daedulus` (if running), `solomon start daedulus` (RUNG-0).
3. **Accept idle**: daedulus is `no_objective` + low-leverage (not in the top-3). For the proof hour, it is acceptable for it to idle. The autopilot will skip it if there is no fresh work. No forced iteration needed.

### sover (gate_red_streak, last 3 iterations reverted)

**Current state**: the last 3 iterations produced diffs that failed the gate and were reverted. The `gate_red_streak` diagnosis is a persistent gate-red, not a base-gate-red (the base is presumably green; the iterations' diffs are red).

**Safe unblock**:
1. **Verify the base gate**: `.venv\Scripts\python -m unittest discover -s tests -t tests` in the sover repo. If green, the base is fine; the streak is an implementer-quality problem.
2. **Reset the anti-thrash counter**: `solomon stop sover`, `solomon start sover` (RUNG-0 — clears the consecutive-revert counter).
3. **Let the MoA brain handle it**: under slice 3-4, the MoA brain's synthesized plan + deepseek-v4-pro implementer should produce higher-quality diffs that pass the gate. The streak should self-resolve. If it persists after the MoA brain is armed, that is a real signal (the goal may be too hard for one iteration — escalate, do not force).

---

## 7. Risks + Rollback

### R1: The MoA brain produces WORSE output than the single-model path
**Risk**: the aggregator rubber-stamps a bad plan, or the deepseek-v4-pro implementer hallucinates file edits (the observed dotz failure: "Pi narrated a change but wrote nothing to a clean tree").
**Mitigation**: the `moa_enabled` feature flag (slice 3). If the MoA brain's ship rate is worse than the single-model baseline over the proof hour, flip the flag to `false` and the iteration loop reverts to `dispatch_engineering_on_ctx` (the pre-MoA single-model path). The `brain.rs` module stays compiled but unused.
**Rollback**: `repos.json` autopilot `brain.enabled: false` (or remove the `brain` block). No rebuild needed (it is a runtime config read). The fleet continues on the single-model path.

### R2: A worker model 429s mid-iteration
**Risk**: deepseek-v4-pro (Extra-High tier) 429s, parking the endpoint.
**Mitigation**: the existing `budget.rs` `park_until`/`consecutive_429` machinery parks ONLY that endpoint (research 5.4: parking is per-endpoint). The MoA brain's degraded mode (slice 3 test) runs the iteration without the parked worker — if the implementer is parked, the iteration degrades to "aggregator-only synthesis" (produces a plan, but no execution — records an honest no-op, does not fabricate). If the aggregator is parked, the iteration runs the implementer with the raw task (the pre-MoA baseline). The OpenRouter fallback (slice 3's `fallback_client`) retries once on the OpenRouter slug before parking.
**Rollback**: the existing rails handle it — no new rollback needed.

### R3: The aggregator rubber-stamps (anti-gaming)
**Risk**: the aggregator (glm-5.2) simply echoes one worker's output without synthesizing.
**Mitigation**: research 4.5 rail #4 (model diversity) — the aggregator is a different model family from all workers, so it cannot collude. The Aggregate-and-Synthesize prompt (MoA paper Table 1, verbatim) explicitly instructs critical evaluation. AND the gate is runner-enforced — even a rubber-stamped plan still has to pass the gate. The verifier (separate call) adversarially checks the committed diff. If the aggregator rubber-stamps a BAD plan, the gate catches the bad diff; the verifier catches the bad diff; the iteration reverts. No fabricated success.
**Rollback**: no rollback needed — the gate + verifier are the safety net.

### R4: deepseek-v4-pro exhausts the Ollama Max plan's weekly GPU-time
**Risk**: research 5.3 flag — deepseek-v4-pro is Extra-High tier; it may exhaust the plan's usage allowance before the 3500 call cap.
**Mitigation**: the existing `park_until` parks deepseek-v4-pro independently when it 429s. The MoA brain's degraded mode runs without it. If deepseek-v4-pro is consistently parked, consider dropping to `deepseek-v4-flash` (research appendix flag: unverified on Ollama Cloud — probe before wiring) OR reducing its `window_cap_calls` to 250 in `_provider_budget.json`.
**Rollback**: edit `_provider_budget.json` to lower deepseek-v4-pro's cap, or swap the implementer model in `repos.json` brain config to `glm-5.2` (the aggregator model, which is High tier, cheaper).

### R5: The NO-MONEY-OUT guard is accidentally weakened
**Risk**: the MoA brain's workers could reach a money tool.
**Mitigation**: the workers are dispatched through `dispatch_engineering_on_ctx` (the implementer) which runs `money_guard::guard` FIRST (fail-closed). The planner/ideator/verifier are pure chat calls with NO tool access. `money_guard.rs` is UNTOUCHED in this plan (slice write scopes explicitly exclude it). The closed `MONEY_CAPABLE_KINDS` set is empty today; no money tool exists.
**Rollback**: N/A — the guard is preserved by design.

### R6: The 1-hour proof fails (no shipped PR in the hour)
**Risk**: the MoA brain is slower per iteration (3-4 model calls vs 1), so fewer iterations complete in the hour.
**Mitigation**: the proof-hour bar is "at least one shipped PR in the hour," not "N iterations." A single high-quality shipped PR on asmodeus/kairos/sover passes the bar. If the MoA brain is too slow, flip the `moa_enabled` flag and run the single-model path for the proof hour (it is faster per iteration). The skeptic grades the FINAL state — if the MoA brain is armed but the proof runs on the single-model path, that is an honest report (the MoA brain is the goal-state; the proof demonstrates the fleet loops, the single-model path is the fallback).
**Rollback**: flip `moa_enabled` to false; re-arm the fleet; re-run the proof.

### Kill-switch (UNCHANGED)
The `STOP` sentinel (SOLOMON_RSI.md safety rails) stops the loop immediately. The operator can drop `runtime/<lane>/stop` at any time. The MoA brain checks it via the existing `run_pi` path (the implementer worker) + the iteration loop's existing stop checks. No new kill-switch needed.

---

## 8. The 1-Hour Proof Procedure

The skeptic grades against these. Run AFTER slices 1-4 land + the rebuild.

### Pre-flight (before starting the clock)
1. `cargo build --release` in `src-tauri/` -> copy `target/release/solomon.exe` to `Solomon.exe`.
2. `solomon state` -> confirm: `daily.calls: 0`, no `cooldown`, 4 budget endpoints in `_provider_budget.json`, no lane in `stuck` with climbing sweeps.
3. `solomon stop <lane>` for all lanes, then `solomon start asmodeus`, `solomon start kairos`, `solomon start sover` (the high-leverage trio). Leave dotz stopped (the base reset is done but the MoA brain should focus on the top-3 first).
4. Confirm the autopilot is armed: the watchdog's next sweep should pick up the started lanes.

### The proof hour (start the clock)
Run for 60 minutes. Monitor:

**A. MoA brain firing** — `Get-Content runtime/autopilot_events.jsonl -Wait` (tail). Look for events with `phase: "moa_aggregate"` or `brain: "moa"` (the brain.rs heartbeat stamps). FAIL if all events are `proof_required` or single-model fallback.

**B. At least one shipped PR** — `Get-Content runtime/<lane>/history.jsonl -Wait` for each lane. Look for a record with `"status": "shipped"` and a `"pr"` URL, timestamped within the hour. PASS on the first one.

**C. No stuck lane** — `solomon state` every 10 min. Check `queue[]` for `state: "proof_required"` with `requires_ai: false`. FAIL if any lane's `stuck.sweeps` count climbs above the `STUCK_SWEEP_THRESHOLD` (3).

**D. Honest ops** — `Get-Content runtime/ops_status.json`. Each probe should be `green` or `red` with a real `reason` (e.g. "no new settled fills" — a real missing outcome). FAIL if a probe is `green` with no underlying outcome (false-green).

**E. CEO rhythm** — check `runtime/` for the morning plan / evening summary files. The morning plan should have fired (or be due-but-cooled, honestly). FAIL if the CEO is silent for the whole hour with no logged reason.

**F. Signal not noise** — `Get-Content runtime/_notify.jsonl -Wait`. Count notifications. FAIL if more than 20 notifications in the hour (flooded) OR if the same dedup-key pages repeatedly (dedup broken).

### Stop condition
At 60 minutes, stop the clock. Run `solomon state` + collect the history.jsonl records + the autopilot_events.jsonl tail. The skeptic grades: did A-F all hold? If yes, the proof PASSES. If any failed, the plan returns to the implementer for a fix (the specific failure names the slice that needs work).

### Honest-failure reporting
If the proof fails, the report MUST name the specific failure (e.g. "B failed: no shipped PR; the MoA brain ran 4 iterations on asmodeus but all reverted at the gate — the deepseek-v4-pro diffs failed `cargo test --workspace`"). A truthful failure is not a plan failure — it is evidence for the next iteration. The operator's "a truthful null beats a gamed success" doctrine holds.

---

## 9. Open Questions (resolved by the operator, recorded here)

These were flagged in the research report; the operator's decisions (section 1) resolve them:

| Research flag | Operator decision |
|---|---|
| 5.1: is the cap daily or weekly? | D4: raise both — `daily_call_budget` to 500 (daily planning target) AND `DEFAULT_WINDOW_CAP` to 3500 (weekly hard ceiling). |
| 5.3: Ollama Cloud Max plan required (10 concurrent)? | Assumed YES (the operator runs the fleet; the existing 500/week cap implies a paid plan). The implementer verifies by checking if the 3 parallel Layer-1 workers 429 immediately on arm. If they do, drop to sequential Layer-1 (research 5.3 fallback). |
| 5.4: `daily_call_budget` unit (calls vs iterations)? | D4: calls. 500 calls/day. |
| 2.2: minimax-m2.7 vs m3? | D3: minimax-m2.7 (Medium tier, cheaper — the "cheap worker" per research 2.2 recommendation). |
| 2.4: deepseek-v4-pro vs flash? | D3: deepseek-v4-pro (the strong implementer). If it 429s, the degraded mode handles it. |
| 3.2: bare-name vs `:cloud`-suffix? | Bare name (matches Solomon's existing `ollama_chat` for glm-5.2). The implementer verifies the 3 new worker models resolve; if not, switch to `:cloud`-suffixed. |
| 1.6: TRINITY learned coordinator? | Phase-2, NOT v1. The fixed prompt-based MoA (Together AI paper) is v1. |

---

## 10. Subagent Dispatch Plan (for the lead)

The lead dispatches slices to `ultra-implementer` subagents. Slices 1-2 are proof-hour-critical and should land FIRST (in parallel — disjoint write scopes). Slice 3 depends on 1 (the budget revive is needed for the MoA brain to run). Slice 4 depends on 3. Slice 5 depends on all.

| Slice | Write scope | Depends on | Parallel? |
|---|---|---|---|
| 1 (budget) | `repos.json`, `budget.rs`, `_provider_budget.json`, `autopilot_state.json` (cooldown/daily fields) | — | Yes (with slice 2) |
| 2 (stuck lanes) | `dotz`, `daedulus`, `sover` repos + `autopilot_state.json` (stuck/queue fields) | — | Yes (with slice 1) — BUT both edit `autopilot_state.json`; coordinate so slice 1 clears cooldown/daily and slice 2 clears stuck/proof_required (disjoint fields). |
| 3 (minimal MoA) | `Cargo.toml`, `brain.rs` (new), `mod.rs`, `iteration.rs`, `ctx.rs`, `repos.json` (brain block) | 1 (budget revive) | After 1+2 |
| 4 (full MoA) | `brain.rs`, `phases.rs`, `improver/<repo>/skills/*.md` | 3 | After 3 |
| 5 (docs + proof) | `SOLOMON_RSI.md`, `AGENTS.md`, `improver/<repo>/AGENT.md` | 1-4 | After 1-4 |

**Recommended sequence**: dispatch slices 1+2 in parallel (one message, two implementers). When both land + the rebuild passes, dispatch slice 3. When slice 3 lands + the minimal MoA runs, START THE PROOF-HOUR CLOCK on the minimal MoA (fleet looping, MoA aggregator + implementer worker). While the proof hour runs, dispatch slice 4 (the full MoA) — it can land mid-proof and upgrade the brain. Slice 5 (docs) lands after the proof passes.

**Skeptic grading**: after the proof hour, the lead invokes `ultra-skeptic` with this plan, the changed files, the autopilot_events.jsonl tail, the shipped-PR evidence, and the ops_status.json. The skeptic grades against section 8 A-F.

---

## 11. The pi-agent dissolution (section 6 of the prompt — what happens to AGENT.md + backlog.md)

**Operator's words**: the pi-agents "shoulda been erased with the solomon AI CEO idea we moved to live" — but the backlogs contain real improvement ideas.

**Recommendation (binding for slice 5)**:
1. **KEEP `improver/<repo>/backlog.md`** as input — the MoA brain's planner + ideator workers read the backlog to pick the next improvement. The backlog stays operator-curated (the `agent-implements-under-contract` invariant's "menu curation stays human-owned" sub-clause is PRESERVED — the workers never edit their own backlog; the runner's `backlog::select_item` still picks + ticks items). This honors the operator's "backlogs contain real ideas" intent.
2. **REPURPOSE `improver/<repo>/AGENT.md`** from "the pi agent's sole contract" to "the lane's north-star + the worker system-prompt source." The file becomes the lane's standing instructions (the north-star goal, the gate command, the protected paths) that the MoA workers read as context. It is no longer the SOLE author contract (D1 dissolves that) but it remains the lane's charter.
3. **DO NOT DELETE either** — they are repurposed, not erased. The `ensure_contracts(repo)` provisioning path still writes them on first sight; the MoA brain reads them.
4. The per-repo `LESSONS.md` (the reflect phase's output) stays — the MoA brain's reflect phase (if enabled) still distills lessons.

**Net effect**: the pi-agent-as-sole-author invariant is retired (D1), but the per-repo contract + backlog artifacts survive as worker input. This is the operator's intent: "keep backlogs as input, retire the pi-agent-as-sole-author invariant."

---

**End of plan. Awaits lead approval before mutating work begins.**
