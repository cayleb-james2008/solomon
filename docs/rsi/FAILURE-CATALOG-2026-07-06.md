# MASTER FAILURE-MODE CATALOG — for the new Solomon-hosted RSI engine (kairos-first, Ollama-cloud brains, weeks unattended)

*Input note: 4 autopsies arrived complete (solomon, asmodeus, dotz, sover); the 5th record was truncated mid-object and is excluded. Kairos context below is drawn from its repo (AGENTS.md, strategist harness, sacred floors mirroring asmodeus).*

Ranking = (how many projects exhibited it) × (how completely it killed the loop).

---

## Ranked failure modes

### 1. Value-blind objective: the loop optimizes a signal it cannot observe
**Mechanism:** The fitness function requires data the system never produces, so every meta-decision is made on zero information — yet the optimizer keeps deciding. Asmodeus scored builders on closed trades while `trades` had 0 rows ever → scores monotonically fell → 65 rollback ping-pongs, 0 adoptions. Sover's genome promotion required ≥3 posts on the *active* genome the same day publishing died → 45 variants bred, 38 never evaluable, champion=None forever. Solomon measured "shipped" (PR opened) while the money project lost $3.71/day and its value probes queried nonexistent tables ("yellow unobservable"). Dotz counted empty test suites as green (passed:0, ok:true) and let the agent grade itself.
**Projects:** all 4. **Owner diagnosed:** mostly yes (asmodeus yield, sover deadlock, solomon ship-count); no for cadence drift and parse-fallback flavors.
**Countermeasure:** a **metric-freshness ledger** — `{metric_id, last_new_datum_ts, n_new_samples_in_window}` — consulted before every meta-step. Policy: *no promote/rollback/mutate decision, and no token spend, unless the objective gained ≥N new samples since the last decision.* Tripwire: a tier-1 value metric reading "unobservable" (schema mismatch, empty table, parse fallback to 0/0) is **RED and halts meta-optimization**, never yellow. For kairos the objective is settled paper/live PnL rows in `.state/kairos.db` — the ledger gates on new settled rows, not iterations.

### 2. Provider quota monoculture → capability collapse → fleet death
**Mechanism:** All lanes on one weekly-capped account (ollama-cloud glm-5.2); one 429 killed the entire Gen-1 fleet simultaneously (Jul 4 12:57Z). The escape hatch was a free model that structurally cannot implement (nemotron: empty diffs, narrated edits) — process green, production zero. Asmodeus's fallback (kimi-k2.7-code) failed 78/78 calls with 0 tokens: a dead endpoint nobody canaried. Dotz's escalation-ladder fallback for openrouter was *the same failing model by design*. Solomon Gen-2 added a 24h blanket cooldown: one quota error = fleet asleep a day. Sover's brain churned across three providers, shipping 13 empty-summary PRs during the weak-model window.
**Projects:** all 4. **Owner diagnosed:** partially (quota yes; free-model incapability and dead-fallback no).
**Countermeasure:** a **provider budget ledger** (`{provider, model, window_cap, spent, park_until, last_canary_pass}`) owned by the scheduler. Policies: (1) 429 parks *that provider*, never the fleet (port asmodeus's TradeLocker parking to the LLM transport); (2) a fallback must be a *different* endpoint that passed a **capability canary** (one tiny real implement-task with diff verification) within 24h; (3) reserve headroom: never plan more calls than remaining budget across all providers; (4) cooldowns are per-provider and exponential, not 86400s blankets.

### 3. Detection without actuation: every diagnosis dead-ends at "wait for operator"
**Mechanism:** Observability was uniformly excellent and remediation uniformly absent. Solomon Gen-2's planner *recommended* "run Ideate" but had no ideate job kind — it parked the repo as operator-blocked; the amputated Gen-1 ladder had been healing that exact state hours earlier. Sover's CGO wrote accurate "STALLED FUNNEL, 0 publishing for 6+ days" briefs for weeks while forbidden from touching infra; its life-support watchdog lived *outside the writable scope* calling a nonexistent action every 15 min. Dotz's probes counted 314 consecutive reds on a dead heartbeat with no resurrector. Sover's approval-gated capability tree froze the moment the operator stopped clicking.
**Projects:** all 4. **Owner diagnosed:** partially (dotz/sover yes; solomon's missing ideate arm no).
**Countermeasure:** a **closed action registry**: at engine build time, assert that every diagnosis code the planner can emit maps to ≥1 executable, auto-safe remediation job kind (schema-checked; CI fails otherwise). Escalations to the operator carry a **TTL**: on expiry the engine executes its documented degraded-mode fallback (e.g., swap provider, restart lane, skip node) instead of waiting forever. Everything that keeps the loop alive must live *inside* the loop's writable, gated scope.

### 4. Liveness chained to a fragile host; nothing resurrects the dead
**Mechanism:** Solomon's only heartbeat ran in a visible GUI window's tick thread (operator rule: no scheduled tasks) — 8h and 25.5h watchdog gaps; the standstill alarm was dead during the standstill. Asmodeus was "purely the app exe": shell closed Jul 4, everything dead 2+ days by contract. Dotz's lane was killed mid-iteration by an operator rebuild and nothing restarted it (49h red). Sover's pi-runner heartbeat died Jun 27 with the gate still claiming it active.
**Projects:** all 4. **Owner diagnosed:** no (solomon GUI-liveness, sover stale gate) / partly (asmodeus contract was deliberate).
**Countermeasure:** **host-independent liveness**: a minimal out-of-band supervisor (Windows scheduled task or service — the "no scheduled tasks" rule must be renegotiated; it is the proximate cause of two multi-day outages) whose only job is: read heartbeat file → if stale beyond T, restart the engine process and page once (marker-deduped). The engine writes the heartbeat *before* each cycle (kairos's strategist already does this — keep it). A dead-man tripwire, not a feature.

### 5. Retry theater: repeating an action that cannot change state
**Mechanism:** Solomon Gen-2's head-of-queue scheduler ran the same no-op `proof_required` job 303 times (the anti-retry-theater job *was* retry theater) because finishing it mutated nothing that fed the next plan, starving all AI jobs behind it. Dotz's fallback rung reran the identical model. Asmodeus's 5-min meta cadence re-measured the same zero-trade state 288×/day, burning ~841K tokens. Asmodeus's unported refutation-blocklist reader let the builder re-litigate the same 3 dead families 166 times. Sover's watchdog retried an invalid CLI action every 15 min for weeks.
**Projects:** all 4. **Owner diagnosed:** partially (dotz fallback yes; solomon deadlock diagnosed only after the fact; cadence no).
**Countermeasure:** a **progress ledger** keyed by `hash(job_kind, target, diagnosis)`: if the same key completes N times (N=3) without any observable state delta (git diff, DB row, config change), the key is **quarantined** and the scheduler must select a different job — plus round-robin/aging so no single queue head can starve others. Every ledger (refutations, done-work, failures) must have a *reader wired into the prompt*, verified by a startup contract test — write-only ledgers are bugs.

### 6. The control plane exempts itself from its own gates
**Mechanism:** Solomon's keystone was gated-PR-only shipping — yet Solomon itself ran 601 uncommitted lines on an off-base branch, hand-built into the production exe (.bak/.bak2/.bak3), and the ungated vibecoded Gen-2 deadlocked within hours. An unversioned `.env` swap by a "solomon" session tripped asmodeus's breaker on a −40.8% *phantom* drawdown (equity-source discontinuity, not losses) and froze the promotion pipeline. Operator hand-seeded the only surviving asmodeus specs; operator provider churn was un-diffed config surgery everywhere.
**Projects:** solomon, asmodeus, sover (dotz indirectly via mid-iteration kill). **Owner diagnosed:** no.
**Countermeasure:** **controller-clean preflight**: the engine refuses to start from a dirty or off-base tree (its own repo included). All config (providers, cadences, .env) is versioned and changes land as gated commits with provenance tags (`operator:` vs `rsi:`); a **config-provenance tripwire** halts trading-adjacent lanes when an unversioned mutation of watched files is detected (asmodeus's guardian did this right — generalize it). Equity/value baselines carry a source-id; a source switch resets the high-water mark instead of tripping breakers.

### 7. Problem generation mismatched to executor capability
**Mechanism:** CEO/campaign planners emitted grand architecture steps free models couldn't execute (→ noop streaks misdiagnosed as "backlog exhausted" with 30 items open); early self-picked goals were trivial README tweaks; sover's ideation produced empty output, already-done tasks, and 4 duplicate PRs of one feature; dotz's evidence shows *concreteness of items was the strongest predictor of shipping*.
**Projects:** all 4. **Owner diagnosed:** yes (mostly).
**Countermeasure:** a **task-size calibration table** per model (`{model, size_class, ship_rate}`) updated from outcomes; the planner may only emit items in classes where the assigned model's ship-rate ≥ threshold, else must decompose. Every backlog item must name a real symbol + a new behavior + a required test (the dotz supervisor format). Dedupe new ideas against landed commits and the done-ledger before enqueue.

### 8. Telemetry noise and alert fatigue bury the real alarm
**Mechanism:** 310 undeduped housekeeping pages vs 17 standstill pages (solomon); 184 `title='t'` test notifications in the prod ledger; sover's tracked AGENT_LOG.md absorbed 3,413 test-run bootstrap lines, became "dirty tree = work," and manufactured 10 merged junk PRs. The operator was correctly paged about the standstill and didn't act — consistent with desensitization.
**Projects:** solomon, sover. **Owner diagnosed:** junk-PRs yes; page-flood no.
**Countermeasure:** every page class requires a **d

---

> **[TRUNCATION MARKER — added 2026-07-06 by the rsi-v3.1 skeptic-fix pass; not original text.]**
> This design input arrived truncated mid-sentence in countermeasure #8 (the committed file ended
> at "every page class requires a **d"). The implemented #8 behavior (24h/category page-marker
> dedupe, standstill once-until-recovery pages, transitions-only provenance actions, and the
> two-consecutive-probe confirmation for the controller-dirty page) was built from the mechanism
> section above and the surviving fragment; full compliance with the operator's intended #8 text
> is unverifiable until the operator restores it from the source document.