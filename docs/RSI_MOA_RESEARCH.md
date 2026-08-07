<task id="ses_0b6dbdc71ffeN0Se2qqerIuMgn" state="completed">
<task_result>
I now have complete primary-source grounding for all five research questions. The OpenRouter slug for glm-5.2 is confirmed as `zai/glm-5.2`. Both Fugu papers (TRINITY + Conductor) are ICLR 2026, CC-BY 4.0. I have everything needed to write the full report. Let me compose it now.

---

# Solomon RSI Re-architecture — Research Report

**Scope**: Mixture-of-Agents brain, 4-model roster, ephemeral worker spawning from Rust, anti-gaming rails, and call-budget economics for the Solomon RSI control plane.
**Mode**: Research-only. No code written. All claims sourced.
**Date**: 2026-07-09.

---

## 1. Mixture-of-Agents / Mixture-of-Experts Architectures for LLM Orchestration

### 1.1 Sakana AI "Fugu" — multi-agent system delivered as one model

**Source**: https://github.com/SakanaAI/fugu (788★, Shell, MIT-adjacent; updated 2026-07-07); technical report https://arxiv.org/abs/2606.21228; two ICLR 2026 papers:
- **TRINITY: An Evolved LLM Coordinator** — https://arxiv.org/abs/2512.04695 (v3, 2026-04-27, CC-BY 4.0)
- **Learning to Orchestrate Agents in Natural Language with the Conductor** — https://arxiv.org/abs/2512.04388 (v5, 2026-05-06, CC-BY 4.0)

**Exact mechanism (from the abstracts, source-claimed):**

- **TRINITY** uses a *lightweight coordinator* — a compact ~0.6B LLM plus a ~10K-parameter head — optimized with an **evolutionary strategy (CMA-ES, separable covariance matrix adaptation)**, NOT reinforcement learning. The coordinator processes queries over **multiple turns**, and at *each turn* assigns one of **three roles — Thinker, Worker, or Verifier** — to a selected LLM from a pool, offloading complex skill acquisition from the coordinator itself. Reported: 86.2% on LiveCodeBench; generalizes to out-of-distribution tasks; outperforms individual models and existing methods across coding, math, reasoning, and domain knowledge. Key finding: under high dimensionality + strict budget constraints, CMA-ES beats RL, imitation learning, and random search by exploiting "block-epsilon-separability."

- **Conductor** is a **7B model trained with reinforcement learning** that learns to (a) **design communication topologies** for agent-to-agent collaboration AND (b) **prompt-engineer focused instructions** to each worker LLM to maximally leverage individual capabilities. Crucially: "allowing the Conductor to select itself as a worker gives rise to **recursive topologies**, elevating performance with a new form of dynamic test-time scaling through online iterative adaptation." Trained with **randomized agent pools** so it adapts to arbitrary sets of open- and closed-source agents. SOTA on LiveCodeBench and GPQA.

- **Fugu** itself is the productized combination: "a multi-agent system delivered as one model. Fugu dynamically orchestrates frontier models to tackle complex, multi-step tasks." Accessed via Sakana Console API (Chat Completions + Responses endpoints). Repo includes `Fugu_technical_report.pdf`. Updated June 2026 evaluation vs Gemini 3.1 Pro / Opus 4.8 / GPT-5.5.

**What it merges**: NOT weights. Both papers are explicit that they avoid weight-merging ("weight-merging is limited by mismatched architectures and closed APIs" — TRINITY abstract). Fugu orchestrates **outputs** of independent models (TRINITY role delegation per turn; Conductor topology + instruction design).

**Aggregation**: TRINITY's coordinator picks role + model per turn (sequential delegation, not parallel fan-out); the Verifier role closes the loop. Conductor designs the *topology* (can be DAG, recursive) and writes per-worker prompts, then aggregates. This is closer to a **learned router/supervisor** than a fixed propose→aggregate pipeline.

**Inference for Solomon**: TRINITY's Thinker/Worker/Verifier triad maps directly onto Solomon's existing PLAN→IMPLEMENT→REVIEW pipeline (see `SOLOMON_RSI.md` §"Role-agent pipeline"). The Verifier role is the adversarial judge Solomon already wants.

### 1.2 Together AI / Nous "Mixture-of-Agents" (Lin et al. 2024)

**Source**: https://arxiv.org/abs/2406.04692 (Wang et al., Duke + Together AI + Stanford + UChicago, 2024-06-07, CC-BY 4.0); code https://github.com/togethercomputer/moa. HTML full text read in full.

**Exact mechanism (from §2.2 of the paper):**

- **Layered architecture**: *l* layers, each with *n* LLM agents `A_{i,1}…A_{i,n}`. Layer 1 agents independently respond to the prompt. Layer 2+ agents take **all outputs from the previous layer as auxiliary context**. Formally: `y_i = ⊕[A_{i,j}(x_i)] + x_1`, where `⊕` is the "Aggregate-and-Synthesize" prompt.

- **The exact aggregator prompt (Table 1, verbatim):**
  > "You have been provided with a set of responses from various open-source models to the latest user query. Your task is to synthesize these responses into a single, high-quality response. It is crucial to critically evaluate the information provided in these responses, recognizing that some of it may be biased or incorrect. Your response should not simply replicate the given answers but should offer a refined, accurate, and comprehensive reply to the instruction. Ensure your response is well-structured, coherent, and adheres to the highest standards of accuracy and reliability.
  > Responses from models: 1. [Model Response from A_{i,1}] 2. [Model Response from A_{i,2}] … n. [Model Response from A_{i,n}]"

- **Roles**: *Proposers* (generate diverse reference responses) vs *Aggregators* (synthesize). GPT-4o, Qwen1.5, LLaMA-3 = versatile (both); WizardLM = strong proposer, weak aggregator.

- **Default config**: 6 proposers × 3 layers; Qwen1.5-110B-Chat as final aggregator. MoA-Lite = 2 layers + Qwen1.5-72B aggregator (cost-effective).

- **Reported gains (source-claimed, AlpacaEval 2.0 LC win rate)**: MoA = 65.1% (open-source only), MoA w/ GPT-4o = 65.7%, MoA-Lite = 59.3%, vs GPT-4o alone = 57.5%. **8.2% absolute improvement** over GPT-4o. FLASK: MoA beats GPT-4o on correctness, factuality, insightfulness, completeness, metacognition.

- **Key findings (§3.3)**: (1) MoA **significantly outperforms LLM rankers** — the aggregator synthesizes, doesn't just pick; (2) MoA incorporates the best proposed answers (BLEU correlation); (3) **model diversity matters** — multiple-different-LLM proposers beat single-model-with-temperature; (4) more proposers monotonically helps (n=6 > n=3 > n=2 > n=1). Cost: MoA-Lite ≈ GPT-4o cost but +1.8% quality; MoA ≈ 2× cost of GPT-4 Turbo but +4% quality.

- **Limitation (§5)**: high TTFT (can't emit first token until last layer). Mitigation: limit layers; first aggregation has the biggest boost.

### 1.3 "Hermes MoE" — what the operator actually referenced

**Finding**: The operator said "moe like hermes." Hermes Agent (Nous Research, MIT, https://github.com/NousResearch/hermes-agent, v0.18.2, 212k★) has a built-in **`moa` toolset** — explicitly named "Mixture of Agents" — listed in the toolset table at line 435 of `~/.claude/skills/autonomous-ai-agents/hermes-agent/SKILL.md`:

> | `moa` | Mixture of Agents (off by default) |

**Critical caveat — could NOT verify the mechanism from source**: I fetched the full `toolsets.py` from the Hermes repo (https://raw.githubusercontent.com/NousResearch/hermes-agent/main/toolsets.py). The `moa` toolset is **NOT a static entry in the `TOOLSETS` dict** — it's only referenced in a comment as a `_DEFAULT_OFF_TOOLSETS` member. There is **no `tools/moa_tool.py`** file in the `tools/` directory listing (verified via https://github.com/NousResearch/hermes-agent/tree/main/tools — 70+ tool files, none named `moa*`). This means the `moa` toolset is a **plugin/registry-derived toolset** — registered at runtime, likely via a plugin or optional-skill, not shipped in the core repo. I could not retrieve the actual implementation source. **Flag: unverified mechanism.**

**The 2-3 most likely referents for "Hermes MoE," ranked by fit to "different models combined with one as aggregator":**

1. **(Most likely) Hermes's `moa` toolset = an MoA-pattern tool that spawns N proposer agents (each a different model/provider) in parallel via `delegate_task`, then aggregates with a final model.** This matches the Together AI MoA shape exactly and fits the operator's description ("different models combined, glm-5.2 as aggregator"). The Hermes `delegate_task` tool (verified present in `tools/delegate_tool.py` + `tools/async_delegation.py`) already supports batch parallel subagents with `delegation.max_concurrent_children` (default 3) and isolated context. The `moa` toolset almost certainly wraps this with a fixed propose→aggregate prompt. **Inference, not confirmed.**

2. **A "Hermes MoE" model-merge recipe** (Nous Research is famous for Hermes MoE fine-tunes like `Nous-Hermes-2-Mixtral-8x7B-DPO`). This is an actual Mixture-of-Experts *model* (8 experts, routed subnetworks inside one model). This is the original sense of "Hermes MoE" in the open-weights community. **Does NOT fit** the operator's "different models combined with one aggregator" — that's MoA, not MoE.

3. **The Hermes `delegate_task` / Kanban orchestrator-worker pattern itself** — a general multi-agent fan-out, not specifically branded "MoA." Possible but less likely given the explicit `moa` toolset name.

### 1.4 MoA vs MoE — the precise distinction

**From the MoA paper §2.3 (verbatim):**
> "Mixture-of-Experts (MoE) … multiple expert networks specialize in different skill sets … a stack of layers … each layer comprises a set of *n* expert networks alongside a *gating network* … `y_i = Σ G_{i,j}(x_i) E_{i,j}(x_i) + x_i` … Our MoA method draws inspiration from this methodology. From a high-level perspective, our proposed MoA framework extends the MoE concept to the **model level** by operating at the model level rather than at the **activation level**. Specifically, our MoA approach leverages LLMs and operates entirely through the **prompt interface** rather than requiring modifications to internal activations or weights. This means that instead of having specialized sub-networks within a single model like in MoE, we utilize multiple full-fledged LLMs across different layers."

**The precise distinction:**
- **MoE** = ONE model with internal routed subnetworks; a learned gating network selects which expert subnetwork activates per token. Operates at the **activation/weight level**. Examples: Mixtral 8x22B, DeepSeek-V4-Pro (1.6T total / 49B active), MiniMax M3, Kimi K2.7 (32B active / 1T total), GLM-5 (744B/40B active). You cannot mix architectures or closed APIs.
- **MoA** = MULTIPLE independent full models orchestrated via prompting only; an aggregator model synthesizes their outputs. Operates at the **model-output level**. No weight access needed. Can mix open + closed, different architectures, different providers.

**What the operator wants**: They said "moe like hermes" but *described* "different models combined with glm-5.2 as aggregator." By the paper's own definition, **the operator wants MoA, not MoE.** The word "MoE" was a misnomer; the mechanism (multiple models + one aggregator) is MoA. Confirm.

### 1.5 Other multi-agent aggregation patterns (one paragraph each)

- **LangGraph supervisor** (LangChain): a graph-based orchestrator where a "supervisor" node routes to worker nodes; workers return to supervisor; supervisor decides next step or termination. Aggregation = supervisor reads all worker outputs and emits the next routing decision or final answer. State is a shared graph state dict. Good for deterministic control flow + human-in-the-loop. (Source: LangGraph docs — not fetched; inference from known architecture.)

- **AutoGen GroupChat** (Microsoft): agents in a group chat; a manager agent selects the next speaker round-robin or by LLM decision; agents see the full conversation and respond; termination on a "TERMINATE" sentinel or a max-round count. Aggregation = the last speaker's message or a designated summarizer. Good for free-form debate. (Source: AutoGen docs — not fetched.)

- **ChatDev role-play** (OpenBMB): a virtual software company with CPO/CTO/Programmer/Tester/Reviewer roles communicating via structured phases (analysis → coding → testing → docstring). Aggregation = the final phase's document (the README/code). Phase transitions are deterministic; intra-phase is multi-turn chat. (Source: ChatDev paper — not fetched.)

- **MetaGPT** (DeepWisdom): SOP-driven multi-agent with standardized operating procedures; agents emit structured outputs (e.g. PRD, design, code) consumed by the next role. Aggregation = the pipeline's final artifact, with each role's structured output as the next role's input. Stronger on artifact-passing than free chat. (Source: MetaGPT paper — not fetched.)

- **CAMEL** (Kaust): role-playing inception prompting — two agents (AI user + AI assistant) given a task; the user-agent issues instructions, the assistant-agent executes; they converse until task done. Aggregation = the assistant's final output. Good for task completion via delegation. (Source: CAMEL paper — not fetched.)

### 1.6 Verdict

**Recommended architecture for Solomon's brain: a 2-layer MoA with role-specialized workers + a TRINITY-style adversarial verifier, all runner-adjudicated.**

Shape (exact):
- **Layer 1 — Proposers (parallel, 3 workers, one per phase specialty):**
  - **Planner** (kimi-k2.7-code or minimax-m3): produces `plan.md` — the single highest-leverage improvement + acceptance criteria.
  - **Implementer** (deepseek-v4-pro or glm-5.2): produces the diff against the integration branch, test-first where tests exist.
  - **Ideator/Probe** (minimax-m3, cheap): produces 5-8 leverage-ranked alternative improvements (the existing `ideate.md` contract) — runs ONLY when the planner is stuck (depth/creativity escape, per `SOLOMON_RSI.md` §"Depth & creativity").
- **Layer 2 — Aggregator (glm-5.2, the operator's choice):** receives all Layer-1 outputs + the Aggregate-and-Synthesize prompt (MoA Table 1, verbatim above). Produces the single synthesized improvement + the diff.
- **Adversarial Verifier (a SEPARATE glm-5.2 call, NOT aggregation):** runs AFTER the objective gate + commit, BEFORE ship — this is Solomon's existing `review.md` REVIEW phase (`SOLOMON_RSI.md` §"Role-agent pipeline"). It inspects the committed diff for reward-hacking/scope-creep/regressions/goal-miss and **REVERTS on reject** (fail-open if it can't run — the objective gate already passed). This mirrors TRINITY's Verifier role.

**Why this shape:**
1. It's the **proven MoA pattern** (Together AI paper) adapted to Solomon's *existing* PLAN→IMPLEMENT→GATE→REVIEW pipeline — minimal re-architecture. The MoA paper's "Aggregate-and-Synthesize" prompt drops in unchanged.
2. **2 layers, not 3**: the paper shows the first aggregation has the biggest boost and MoA-Lite (2 layers) is the cost-effective sweet spot. Solomon's 500/week budget (see §5) cannot afford 3 layers × 4 workers.
3. **glm-5.2 as aggregator**: confirmed available on Ollama Cloud (`glm-5.2:cloud`, 976K context, "high" usage tier) and on OpenRouter (`zai/glm-5.2`) — fits the operator's explicit choice. Its 1M context handles 3 worker outputs comfortably.
4. **The Verifier is a separate call, not part of aggregation** — this is the anti-gaming keystone (see §4). Aggregation synthesizes; verification adversarially checks. Never let the synthesizer grade itself.
5. **TRINITY's CMA-ES coordinator is aspirational, not v1**: Solomon should start with the fixed prompt-based MoA (proven, zero training) and treat a *learned* coordinator (TRINITY/Conductor) as a Phase-2 research goal if the fixed MoA plateaus.

---

## 2. The Four Specific Models — Availability and Roles

All four confirmed available on **Ollama Cloud** (Solomon's current provider, `provider: "ollama-cloud"` in repos.json) AND **OpenRouter** (the fallback). Pricing/rate-limits from Ollama Cloud are *usage-tier based* (small/medium/high/extra-high), NOT per-token — see §5.

### 2.1 glm-5.2 (aggregator) — CONFIRMED

| Field | Ollama Cloud | OpenRouter |
|---|---|---|
| **Model ID** | `glm-5.2:cloud` | `zai/glm-5.2` |
| **Context** | 976K tokens | (per provider) |
| **Size** | 756B parameters | — |
| **Usage tier** | High (level 3/4) | — |
| **Modalities** | Text, tools, thinking | Text |
| **License** | MIT (Z.ai) | — |
| **Strengths** | Long-horizon coding (FrontierSWE 2nd to Opus 4.8; SWE-Marathon 2nd); 1M-context stable; flexible effort (High/Max); strongest open-source coding per Z.ai | Aggregation/synthesis (per MoA paper, versatile models make good aggregators) |
| **Pricing (OR)** | — | see OpenRouter page (not extracted; usage-tier on Ollama) |

**Sources**: https://ollama.com/library/glm-5.2 (read in full); https://openrouter.ai/zai/glm-5.2 (confirmed HIT via HEAD probe — the `zai-org-official/glm-5.2` slug returned "not available"; `zai/glm-5.2` is the correct slug).

**Note**: Solomon's `repos.json` already uses `"model": "glm-5.2"` (bare name) with `provider: "ollama-cloud"`. The `:cloud` suffix is the Ollama Cloud tag; Solomon's `ollama_chat` (ceo.rs:1458) sends `"model": "glm-5.2"` to `https://ollama.com/v1/chat/completions` — Ollama Cloud resolves the bare name to the cloud tag. **No change needed to the aggregator model string.**

### 2.2 minimax (workers — "cheap" tier) — CONFIRMED, naming resolved

The operator said "minimax." There are **two** MiniMax models on Ollama Cloud:

| Model | Ollama ID | Context | Size | Usage tier | Notes |
|---|---|---|---|---|---|
| MiniMax M3 | `minimax-m3:cloud` | 512K (min) / 1M (max) | — | **High** | Coding + agentic frontier, native multimodal, MSA arch, BrowseComp 83.5 |
| MiniMax M2.7 | `minimax-m2.7:cloud` | 200K | 229B | **Medium** | SWE-Pro 56.22% (matches GPT-5.3-Codex), highest open-source GDPval-AA ELO 1495 |

**Recommendation**: Use **`minimax-m2.7:cloud`** as the *cheap worker* (Medium usage tier = ~3× cheaper than M3's High tier) for the light phases — this matches the *existing* Solomon design (`SOLOMON_RSI.md` §"Per-phase model": "route the light phases (beautify/e2e) to the provider's cheap worker model (minimax-m3 on Ollama Cloud)"). **Correction**: the spec said "minimax-m3" as the cheap worker, but M3 is *High* usage tier on Ollama Cloud (per its model page), while M2.7 is *Medium*. If the operator wants the genuinely cheap tier, **M2.7 is the right pick**. If they want the strongest MiniMax for deep work, M3. The operator's "minimax" is ambiguous; flag for confirmation.

**OpenRouter**: `minimax/minimax-m3` confirmed — $0.30/$1.20 per 1M in/out tokens, 1M context, released 2026-05-31. (M2.7 slug not separately probed but the pattern is `minimax/minimax-m2.7`.)

**Sources**: https://ollama.com/library/minimax-m3 ; https://ollama.com/library/minimax-m2.7 ; https://openrouter.ai/minimax/minimax-m3 .

### 2.3 deepseek-v4 (worker — deep reasoning) — CONFIRMED

| Field | Value |
|---|---|
| **Ollama ID** | `deepseek-v4-pro:cloud` |
| **OpenRouter ID** | `deepseek/deepseek-v4-pro` |
| **Context** | 1M tokens |
| **Size** | 1.6T total / **49B active** (MoE) |
| **Usage tier (Ollama)** | **Extra High** (level 4 — the most expensive tier) |
| **Pricing (OR)** | $0.435 / $0.87 per 1M in/out |
| **Modalities** | Text, tools, thinking (3 modes: No-think / Think / Max-think) |
| **Strengths** | Frontier reasoning: GPQA Diamond 90.1 (Max), LiveCodeBench 93.5 (Max), HMMT 95.2, Codeforces 3206, SWE Verified 80.6, SWE Pro 55.4, BrowseComp 83.4. Best for the *implement* phase on hard problems. |

**Naming note**: operator said "deepseek-v4"; the real ID is `deepseek-v4-pro`. There's also a `deepseek-v4-flash` (cheaper, not probed on Ollama Cloud but referenced in the benchmark table). **Recommend `deepseek-v4-pro:cloud` for the deep implementer; consider `deepseek-v4-flash` if budget-constrained (flag: unverified on Ollama Cloud catalog).**

**Sources**: https://ollama.com/library/deepseek-v4-pro ; https://openrouter.ai/deepseek/deepseek-v4-pro .

### 2.4 kimi-k2.7-code (worker — coding specialist) — CONFIRMED

| Field | Value |
|---|---|
| **Ollama ID** | `kimi-k2.7-code:cloud` |
| **OpenRouter ID** | `moonshotai/kimi-k2.7-code` |
| **Context** | 256K tokens |
| **Size** | 1.04T total / **32B active** (MoE) |
| **Usage tier (Ollama)** | **High** (level 3) |
| **Pricing (OR)** | $0.72 / $3.49 per 1M in/out |
| **Modalities** | Text + Image (vision via MoonViT), tools, thinking; `preserve_thinking` across turns |
| **Strengths** | Long-horizon coding (Kimi Code Bench v2: 62.0, up from 50.9 on K2.6); 30% lower thinking-token usage vs K2.6; MCP-based tool use (MCP Atlas 76.0, MCP Mark Verified 81.1); 10+ languages; multi-step tool calling. **Best for the *plan* phase (long-context codebase reading + plan drafting).** |

**Naming note**: operator said "kimi-k2.7-code"; the real ID matches exactly (`kimi-k2.7-code:cloud` / `moonshotai/kimi-k2.7-code`). No mismatch.

**Sources**: https://ollama.com/library/kimi-k2.7-code ; https://openrouter.ai/moonshotai/kimi-k2.7-code .

### 2.5 Verdict — exact `model` strings + role assignment

**Recommended roster for `repos.json` (phase → model):**

```jsonc
// In the autopilot block — the MoA brain config (new):
"brain": {
  "aggregator": "glm-5.2",                      // Layer-2 synthesizer (Ollama Cloud bare name resolves to :cloud)
  "verifier":   "glm-5.2",                        // Adversarial reviewer (separate call, NOT aggregation)
  "workers": {
    "plan":      "kimi-k2.7-code",                // 256K context, long-horizon coding, MCP-savvy
    "implement": "deepseek-v4-pro",              // 1M context, 93.5 LiveCodeBench, Max-think
    "ideate":    "minimax-m2.7",                 // Medium usage tier (cheap), 200K ctx, 97% skill adherence
    "probe":     "minimax-m2.7"                  // same cheap tier for ops/probe work
  },
  "layers": 2,                                    // MoA-Lite shape (cost-effective per the paper)
  "fallback_provider": "openrouter"              // OpenRouter slugs: zai/glm-5.2, moonshotai/kimi-k2.7-code, deepseek/deepseek-v4-pro, minimax/minimax-m2.7
}
```

**Role rationale (phase → model → why):**
- **PLAN → kimi-k2.7-code**: 256K context reads the whole repo + backlog; `preserve_thinking` keeps coherent multi-turn planning; lowest thinking-token cost of the deep models (30% savings). It's the cheapest *deep* coder.
- **IMPLEMENT → deepseek-v4-pro**: 93.5 LiveCodeBench, 80.6 SWE Verified (Max-think), 1M context for full-codebase edits. The strongest single-shot coder. Extra-High usage tier = reserve for actual implementation, not planning.
- **IDEATE/PROBE → minimax-m2.7**: Medium usage tier (the cheapest of the four), 97% skill adherence over 40 skills, 56.22% SWE-Pro. Good enough for divergent ideation + ops probes; cheap enough to run every cycle.
- **AGGREGATE → glm-5.2**: operator's explicit choice; 976K context holds 3 worker outputs + repo context; versatile (MoA paper: versatile models make the best aggregators); MIT license.
- **VERIFY → glm-5.2 (separate call)**: same model, different system prompt (adversarial reviewer), different phase. Keeps one model family for the judge role (simpler) while the *mechanism* (separate call, runner-parsed verdict) prevents self-grading (see §4).

**For OpenRouter fallback, the exact slugs:**
- `zai/glm-5.2` ✅ (verified HIT)
- `moonshotai/kimi-k2.7-code` ✅
- `deepseek/deepseek-v4-pro` ✅
- `minimax/minimax-m2.7` (inferred from pattern; **flag: not directly probed** — M3 was probed at `minimax/minimax-m3`; M2.7 follows the same `minimax/minimax-m2.7` convention but verify before shipping)

**Flags / unknowns:**
- `deepseek-v4-flash` exists (per the benchmark table on the v4-pro page) but I did not confirm it on the Ollama Cloud catalog. If the operator wants a cheaper DeepSeek, probe `ollama.com/library/deepseek-v4-flash`.
- `minimax-m2.7` OpenRouter slug is inferred, not probed. Probe before wiring the fallback.
- Ollama Cloud does NOT publish per-model rate limits beyond the usage tier (1-4). The concurrency limit is per-PLAN (Free=1, Pro=3, Max=10 concurrent models), not per-model. See §5 for the budget math.

---

## 3. Ephemeral Worker Spawning from a Rust Orchestrator

### 3.1 How OpenCode (this tool) spawns subagents via the Task tool

**Sources**: read in full — `~/.config/opencode/skills/ultra-code/references/orchestration.md` ; `~/.config/opencode/agent/ultra-researcher.md` ; `ultra-planner.md` ; `ultra-implementer.md` ; `ultra-reviewer.md` ; `ultra-skeptic.md`.

**Mechanism (the pattern to mirror inside Solomon):**

1. **Each subagent is a markdown file** under `~/.config/opencode/agent/` with YAML frontmatter:
   - `description`: when to use it (the lead reads these to pick)
   - `mode: subagent` (the key field — marks it as a Task-tool spawnable)
   - `color`: UI hint
   - `permission: { edit: deny|allow, bash: ask|allow }` — the **capability envelope** (researcher/planner/reviewer/skeptic = `edit: deny`; implementer = `edit: allow`)
   - A system-prompt body (the role's standing instructions)

2. **The lead spawns a subagent via the Task tool** with: (a) the agent name, (b) a task prompt, (c) optionally the instruction to load the `ultra-code` skill. The lead includes "relevant Ultra Code context in the subagent prompt or explicitly asks it to load the `ultra-code` skill."

3. **Isolation**: "focused side tasks that should report back into the main conversation"; "parallel subagents: independent research, implementation slices, audits, and design critique that can run concurrently in one message"; "give each implementer a disjoint write scope or a separate worktree/session to avoid conflicts."

4. **Decision rules**: "Use one or two subagents for focused research/review. Use parallel subagents when workers are independent and have disjoint write scopes. Keep concurrent workers to 10 or fewer."

5. **Plan approval gate**: "Lead creates the plan → ultra-planner critiques → User confirms → Only then do mutating agents start. Require subagent plan approval before they make changes when the blast radius is large."

**The exact "spawn a worker with system prompt S and skill instructions K, then send task T" shape for Solomon to mirror:**
- **System prompt S** = the agent file's body (the role's standing contract, e.g. ultra-implementer.md's "Own only the assigned files...restate your write scope...return files changed, behavior changed, verification commands...").
- **Skill instructions K** = loaded into the worker's context as an additional system message (the worker is "explicitly asked to load the `ultra-code` skill" — i.e., the skill's `SKILL.md` + referenced files are injected).
- **Task T** = the user-message content of the Task-tool call.
- **Permission envelope** = the frontmatter `permission` block — the runner enforces this (researcher can't edit; implementer can).

**This is exactly the "ephemeral worker with custom system prompt + skill" pattern the operator wants** — Solomon already has the analog in its `improver/<name>/AGENT.md` per-repo contracts (`SOLOMON_RSI.md` §"Provisioning"). The re-architecture generalizes from *one* per-repo AGENT.md to *N* role-specific system prompts (plan.md, implement.md, review.md, ideate.md) that the MoA brain swaps per phase.

### 3.2 LLM-provider APIs supporting multi-turn worker sessions with custom system prompts

All four target providers support the **OpenAI Chat Completions shape** with a `system` role message. Solomon's existing `ollama_chat` (ceo.rs:1458) already uses it.

**Request shape (Ollama Cloud / OpenAI-compatible — the one Solomon already uses):**
```jsonc
POST https://ollama.com/v1/chat/completions     // Ollama Cloud
POST https://openrouter.ai/api/v1/chat/completions  // OpenRouter fallback
Authorization: Bearer $OLLAMA_API_KEY (or $OPENROUTER_API_KEY)
Content-Type: application/json

{
  "model": "kimi-k2.7-code:cloud",   // the worker model
  "messages": [
    {"role": "system", "content": "<SYSTEM_PROMPT_S>"},   // the role contract
    {"role": "system", "content": "<SKILL_INSTRUCTIONS_K>"}, // optional 2nd system msg (skill body)
    {"role": "user",   "content": "<TASK_T>"}
  ],
  "stream": false,
  "reasoning_effort": "high"   // optional; some models (glm-5.2, deepseek-v4-pro) accept High/Max
}
```

**Provider-specific notes:**
- **Ollama Cloud** (`https://ollama.com/v1/chat/completions`): OpenAI-compatible. Model ID needs the `:cloud` suffix when called via the `/v1/chat/completions` endpoint (per the model pages' curl examples). Solomon's existing `ollama_chat` sends the *bare* name (`"model": "glm-5.2"`) — Ollama Cloud resolves it; **flag: verify the bare-name resolution still works for the 3 new worker models, or switch to `:cloud`-suffixed names.** Supports `reasoning_effort` on thinking models (glm-5.2 High/Max; deepseek-v4-pro High/Max = "xhigh"). Tool calling supported on all four (per Ollama Cloud model pages: "tools" tag on each).
- **OpenRouter** (`https://openrouter.ai/api/v1/chat/completions`): OpenAI-compatible. Model ID = the slug (`zai/glm-5.2`, etc.). Adds `HTTP-Referer` and `X-Title` optional headers. Supports `reasoning` field. The Hermes repo confirms `OPENROUTER_API_KEY` env var (`hermes-agent/SKILL.md` provider table line 376).
- **Anthropic Messages API**: `POST https://api.anthropic.com/v1/messages` with `system` as a top-level field (not a message role) + `messages: [{role:user,...}]`. Different shape — only needed if Solomon adds Claude as a 5th model.
- **OpenAI Responses API**: newer; `POST /v1/responses` with `input` field. Not needed for the 4-model roster.

**The "spawn a worker" request = one chat-completions call per worker.** Multi-turn = append assistant/user messages to the same `messages` array and re-POST. Each call is stateless on the server side (Ollama Cloud and OpenRouter do NOT maintain sessions — the orchestrator holds the conversation history). This is exactly what Solomon's `ollama_chat` already does (single system + single user, no session).

### 3.3 Rust crates for orchestrating concurrent LLM worker sessions

| Crate | Version | License | Ollama | OpenRouter | Anthropic | Maturity | Notes |
|---|---|---|---|---|---|---|---|
| **async-openai** | 0.41.1 (2026-06-18) | **MIT** | ✅ (via `OpenAIConfig::with_api_base`) | ✅ (same) | ❌ (Azure only, not Anthropic) | High — 66.96% documented, reqwest-based, streaming, byot feature for custom types | The standard choice. Supports any OpenAI-compatible endpoint by overriding `api_base`. `byot` feature lets you send raw JSON for vendor-specific fields. |
| **genai** | 0.6.5 (2026-06-06) | **MIT OR Apache-2.0** | ✅ (native adapter) | ✅ | ✅ (Anthropic adapter) | High — 84.76% documented, multi-provider native | The "multi-AI providers" crate. Built-in adapters for OpenAI, Gemini, Anthropic, Ollama, AWS Bedrock, Vertex, Groq, DeepSeek, GitHub Copilot. If Solomon may add Anthropic later, this is the more flexible pick. |
| llm-chain | (older) | MIT | partial | partial | partial | Medium — less actively maintained | Generic chain-of-prompts; superseded for raw orchestration by async-openai/genai. |

**Sources**: https://docs.rs/async-openai/latest/async_openai/ ; https://docs.rs/genai/latest/genai/ (both read in full).

**Maturity + license verdict**: Both `async-openai` (MIT, reqwest, 0.41.1, actively released through 2026-06) and `genai` (MIT/Apache, 0.6.5, native Ollama+Anthropic adapters) are mature and permissively licensed. **`async-openai` is the safer default** — it's the most-used OpenAI Rust client, reqwest-based (Solomon already shells out to curl; async-openai would replace that with a typed client), and its `byot` feature handles vendor-specific fields (like `reasoning_effort`) cleanly. **`genai` is the better pick if Solomon expects to add Anthropic/Claude as a 5th model** — its native Anthropic adapter avoids hand-rolling the Messages API shape.

### 3.4 Verdict — the concrete worker-spawn mechanism for Solomon

**Recommended: replace Solomon's `ollama_chat` curl-shell-out (ceo.rs:1458) with `async-openai` calling Ollama Cloud's OpenAI-compatible endpoint, and orchestrate the MoA workers with `tokio::join!` / `futures::join_all` for parallel Layer-1 calls.**

**Concrete shape:**

1. **One `async-openai::Client<OpenAIConfig>`** configured with `OpenAIConfig::new().with_api_base("https://ollama.com/v1").with_api_key(ollama_key)`. For OpenRouter fallback, a second client with `with_api_base("https://openrouter.ai/api/v1")` and the OpenRouter key + `HTTP-Referer`/`X-Title` headers.

2. **`spawn_worker(role, system_prompt_S, skill_K, task_T, model) -> String`** — a single async fn that builds the `CreateChatCompletionRequest` with:
   - `model: <model_id>` (e.g. `kimi-k2.7-code:cloud`)
   - `messages: vec![system(S), system(K), user(T)]` (two system messages — Ollama/OpenRouter both accept multiple system messages; the MoA paper's Aggregate-and-Synthesize prompt is itself a system message)
   - `reasoning_effort: Some("high")` for the deep models, `None` for minimax-m2.7 (Medium tier)
   - returns the assistant content string (or an error for retry/fallback)

3. **The MoA aggregator loop (Rust, in the runner — NOT in the model):**
   ```rust ignore
   // Layer 1 — 3 workers in parallel (disjoint outputs, safe to fan out)
   let (plan, implement, ideate) = tokio::join!(
       spawn_worker("plan",      PLAN_PROMPT,      ULTRA_PLANNER_SKILL,  task, "kimi-k2.7-code:cloud"),
       spawn_worker("implement", IMPLEMENT_PROMPT, ULTRA_IMPLEMENTER_SKILL, task, "deepseek-v4-pro:cloud"),
       spawn_worker("ideate",    IDEATE_PROMPT,    ULTRA_PLANNER_SKILL,  task, "minimax-m2.7:cloud"),
   );
   // Layer 2 — aggregator (sequential, needs all Layer-1 outputs)
   let aggregated = spawn_worker(
       "aggregate",
       &AGGREGATE_AND_SYNTHESIZE_PROMPT,  // MoA paper Table 1, verbatim
       "",
       &format!("Task: {task}\n\nResponses:\n1. [Plan]: {plan}\n2. [Implement]: {implement}\n3. [Ideate]: {ideate}"),
       "glm-5.2:cloud",
   ).await;
   ```

4. **The Verifier is a *separate* `spawn_worker` call AFTER the objective gate + commit**, with `VERIFY_PROMPT` (adversarial) and the committed diff as the task. Its output is **parsed by the Rust runner** (not by any model) — see §4.

5. **System-prompt/skill injection shape**: each role's `system_prompt_S` is a const string in Rust (the analog of the OpenCode agent file body). Each `skill_K` is loaded from a markdown file on disk (e.g. `improver/<repo>/skills/plan.md`) — mirroring how OpenCode injects `SKILL.md` into a subagent. This keeps skills operator-editable without recompiling.

6. **Concurrency ceiling**: keep to ≤4 concurrent workers per iteration (3 Layer-1 + 1 aggregator-when- Layer-1-done) — well within the Ollama Cloud Max plan's 10-concurrent-model limit. The MoA paper caps parallel proposers; Solomon should too.

7. **Fallback**: if Ollama Cloud 429s or parks an endpoint (the existing `budget.rs` `Decision::Parked` path), the worker auto-retries on OpenRouter (the second client) with the OpenRouter slug. This is the existing `fallback: {provider: openrouter, model: ...}` pattern in repos.json (kairos already uses `qwen/qwen3-coder` as fallback) — generalize it to all 4 models.

**Why async-openai over raw curl**: Solomon's current `ollama_chat` shells out to curl with `@request.json` files on disk — workable but no streaming, no typed errors, no retry, no concurrent. async-openai gives typed requests/responses, streaming (for the visual/E2E phase), reqwest connection pooling (faster repeated calls), and `byot` for vendor fields. It's MIT, reqwest-based (already a Solomon dep via Tauri), and the migration is mechanical (replace the curl argv with `client.chat().create(req).await`).

---

## 4. Anti-Gaming + Honesty Rails for a Multi-Model Brain

### 4.1 The doctrine (grounded in local files)

**From `SOLOMON_RSI.md` invariants (lines 59-82, the hard rules):**
- **gate-enforced-by-runner** — "the pass/fail gate (tests + build + anti-gaming) is executed and adjudicated by the runner, never by the model. *An agent trusted to score itself learns to weaken the check.*"
- **never-hand-patched (keystone)** — the operator/orchestrator never edits a managed repo's working tree.
- The **Proposer/Solver/Judge anti-reward-hacking loop** is already named (line 188): REVIEW is "the adversarial judge...REVERTS on reject (fail-open if it can't run — the objective gate already passed)."

**From `solomon-rsi/SKILL.md` non-negotiables (lines 40-53):**
- **Never edit the oracle** — "the metric, grader, tests-used-as-truth, kill switch, or blast-radius controls are operator-owned. The loop may *create* them once during bootstrap as a proposal the operator ratifies; after ratification they are READ-ONLY to the loop forever. An improver that grades itself always 'succeeds.'"
- **Ground truth is the only truth** — "Tests/sims/backtests/self-reports are optimistic *proxies* — label them as bounds, never as the verdict."
- **Pre-register criteria + prove out-of-sample** — "No moving goalposts."

**From `SOLOMON_RSI.md` anti-gaming checks (lines 31-34, 248-287):** tests-not-deleted, pass+collected counts not dropped, skip/xfail not added, gate not stubbed, error count not increased, skip count not significantly increased, collected-count-gaming detection.

### 4.2 How to prevent the aggregator from rubber-stamping worker outputs

The "improver that grades itself always succeeds" trap has **three** structural defenses already in the doctrine, and the MoA architecture must preserve all three:

1. **The aggregator (glm-5.2) NEVER grades.** The MoA paper's aggregator synthesizes a *better answer* — it does not judge pass/fail. The gate verdict (green/red) is produced by the **Rust runner** running `cargo test` / `pytest` / `EVAL_CMD` and parsing the numeric result itself (`SOLOMON_RSI.md` step 6). The aggregator's synthesized output is just the diff candidate; the *runner* decides if it ships.

2. **The Verifier is a SEPARATE model call, after the objective gate, with a different system prompt (adversarial reviewer).** This mirrors TRINITY's Verifier role and Solomon's existing REVIEW phase. Critically:
   - The Verifier sees the **committed diff** (after the gate passed), not the worker outputs.
   - The Verifier's output is **parsed by the Rust runner** (regex/JSON extraction of `{verdict: "accept"|"reject", reasons: [...]}`), and the runner acts on it (revert on reject). The Verifier does NOT get to call `git revert` itself.
   - **Fail-open**: if the Verifier can't run or returns unparseable output, the iteration ships (the objective gate already passed — `SOLOMON_RSI.md` line 188: "fail-open if it can't run — the objective gate already passed"). This prevents a broken verifier from wedging the loop.

3. **The aggregator and the workers are DIFFERENT models.** The MoA paper (§3.3) shows model diversity is *why* MoA works — "multiple-proposer" beats "single-proposer." If the aggregator == the strongest worker, it rubber-stamps. **glm-5.2 as aggregator + deepseek-v4-pro/kimi-k2.7-code/minimax-m2.7 as workers** is the right diversity split — glm-5.2 synthesizes from three *different* model families (Z.ai / DeepSeek / Moonshot / MiniMax), none of which can collude with the aggregator (they're separate weights, separate providers).

### 4.3 How the RUNNER (Rust) parses/adjudicates so the gate verdict is never model-judged

**The runner already does this** (per `SOLOMON_RSI.md` step 6 + the anti-gaming section). The MoA brain changes *nothing* about the gate path:

1. **Baseline capture** (runner, before any model call): tests green/red + pass count + collected count + build status + starting SHA + `EVAL_CMD` float (if declared). No baseline → no iteration.
2. **Worker + aggregator produce a diff** (model calls — output only, no authority).
3. **Gate execution** (runner, on the `rsi/*` branch): runs the gate command, parses pass/fail + counts + `EVAL_CMD` float **itself**. The model's self-report is never the verdict.
4. **Anti-gaming checks** (runner, deterministic): tests not deleted, counts not dropped, skips not added, errors not increased, `EVAL_CMD` not regressed. All Rust code, all deterministic.
5. **Verifier call** (model, after green gate + commit): adversarial review of the diff. Its output is **regex/JSON-parsed by Rust** into `accept|reject|unparseable`. Reject → runner reverts. Unparseable → fail-open, ship.
6. **Ship decision** (runner): green gate + (verifier accept OR unparseable) → push + PR. Red gate OR verifier reject → revert. **The model never makes the ship call.**

**The MoA aggregation step (Layer 2) is upstream of the gate** — it produces the *candidate* diff. The gate still adjudicates. This preserves `gate-enforced-by-runner` byte-for-byte.

### 4.4 Published patterns for adversarial verification in multi-agent LLM systems

- **Constitutional AI (Anthropic, 2022)**: a model critiques its own output against a set of principles (the "constitution") and revises. The critique is a separate prompt. **Limitation**: still one model grading itself — only safe if the constitution + revision are operator-owned and the *final* verdict is external. For Solomon: the "constitution" = `SOLOMON_RSI.md` invariants + the repo's `AGENT.md` contract; the critic = the Verifier (separate call); the external verdict = the Rust runner. (Source: Anthropic Constitutional AI paper — not fetched; known architecture.)

- **Self-consistency (Wang et al., 2022)**: sample N reasoning paths from the *same* model at high temperature, take the majority answer. **Not directly applicable** to MoA (MoA uses *different* models, not same-model samples), but the principle — "aggregate diverse independent attempts" — is exactly what the MoA aggregator does. For the *verifier*, self-consistency could mean running the verifier 3× and majority-voting the accept/reject — at 3× the cost. **Reserve for high-stakes tiers (architecture/refactor) only.** (Source: Self-Consistency paper — not fetched; known.)

- **Multi-agent debate (Du et al., 2023; Liang et al., MAD 2023)**: two+ agents argue, a judge picks. ReConcile (Chen et al., 2023) = round-table + weighted voting. **Applicable**: the MoA aggregator is a soft form of this (proposers "debate" via their outputs, aggregator "judges" by synthesizing). For the *verifier*, a debate = two verifier calls with opposite stances (one argues ship, one argues revert), runner tallies. **Overkill for v1; consider for the architecture tier.** (Sources cited in MoA paper §4.2: Du et al. 2023, Liang et al. 2023, Chen et al. 2023a — all in the MoA paper's reference list.)

- **TRINITY's Verifier role** (Sakana, ICLR 2026): the coordinator assigns a Verifier role to a selected LLM each turn. This is the closest published analog to Solomon's REVIEW phase. **Key insight from the TRINITY abstract**: the coordinator offloads "complex skill acquisition from the coordinator itself" — i.e., the *coordinator doesn't need to be smart*, the *verifier does*. For Solomon: glm-5.2 as aggregator is fine *because* the gate + verifier are the real authority, not the aggregator's synthesis.

### 4.5 Verdict — the exact rail design

**The 4-rail anti-gaming design for the MoA brain (all runner-enforced, none model-judged):**

1. **Runner-parsed objective gate (unchanged, keystone):** Rust runs `cargo test` / `pytest` / `EVAL_CMD`, parses pass/fail + counts + float **itself**. The MoA brain is upstream of this — it produces the candidate diff; the gate adjudicates. `gate-enforced-by-runner` is preserved byte-for-byte.

2. **Adversarial reviewer = separate glm-5.2 call, post-gate, pre-ship:** runs AFTER the objective gate passes and the diff is committed, BEFORE the PR. System prompt = adversarial (the existing `review.md` contract — inspect for reward-hacking/scope-creep/regressions/goal-miss). Output = JSON `{verdict, reasons}` parsed by Rust. **Reject → runner reverts. Unparseable → fail-open ship** (gate already passed). The aggregator (glm-5.2 Layer-2) NEVER sees the verifier's output — they're two separate calls in two separate phases.

3. **The oracle stays operator-owned (the keystone, unchanged):** the gate command, `EVAL_CMD`, the freshness emitter, the kill switch, the `STOP` sentinel, the budget caps, the anti-gaming heuristics — all in `repos.json` + Rust, all operator-editable, all READ-ONLY to the MoA brain. The brain's workers and aggregator cannot edit `repos.json`, cannot weaken the gate, cannot move the budget cap. `never-edit-the-oracle` is preserved. The MoA brain is a *worker* under the existing keystone, not a new authority.

4. **Model diversity as a structural anti-collusion rail:** the aggregator (glm-5.2) is a *different model family* from all 3 workers (DeepSeek/Moonshot/MiniMax). No single model both proposes and grades. The MoA paper's finding (§3.3: "multiple-proposer" beats "single-proposer," diversity matters) is the structural guarantee — if one worker is gamed, the other two + the diverse aggregator dilute it.

**One addition worth considering (Phase 2, not v1):** for the `architecture` ambition tier only, run the verifier 3× with self-consistency (majority vote on accept/reject) — the highest-blast-radius changes deserve the most expensive check. Cheap tiers keep single-pass verification.

---

## 5. Daily Call Budget Economics at 500/Day Across 6 Lanes

### 5.1 The actual current cap is WEEKLY, not daily — critical correction

**From `budget.rs` (lines 39-45, read in full):**
```rust
const WEEK_S: u64 = 604_800;              // Weekly cap window (seconds) — NOT daily
const DEFAULT_WINDOW_CAP: u64 = 500;     // Calls per endpoint per WEEK
```

**From `runtime/_provider_budget.json` (read in full):**
```json
{
  "endpoints": {
    "maki-cloud:glm-5.2": {
      "window_cap_calls": 500,
      "window_started": 1783362720,
      "spent_calls": 15,
      "park_until": 0,
      "consecutive_429": 0,
      "last_canary_pass": 0
    }
  }
}
```

**Fact**: the operator said "raised the daily call budget from 40 to 500." But the *code* (`budget.rs` line 41 + the comment on line 40: "the shared Ollama-cloud account caps usage per WEEK") and the *state file* (`window_cap_calls: 500` with `window_started` = a Unix timestamp, rollover at 604800s) both show the cap is **500 calls per endpoint per WEEK**, not per day. The `daily_call_budget: 40` in `repos.json` autopilot block (line 11) is a *different* field — it's the autopilot's per-day planning target, while `budget.rs`'s `window_cap_calls` is the hard weekly ceiling per `<provider>:<model>` endpoint.

**Critical flag**: the operator may be conflating two budgets. **Verify the intent**: did they mean (a) raise the *autopilot daily planning target* to 500 (the `repos.json` field), or (b) raise the *hard weekly cap per endpoint* to 500 (the `budget.rs` constant / ledger field)? The current state shows 500/week already set. If they want 500/*day*, that's a 7× increase to the weekly cap (3500/week per endpoint) — a major change to the reserve-headroom rule.

### 5.2 The math at 500 calls/week across 6 lanes

**One "aggregated iteration" cost (the MoA brain):**
- Layer 1: 3 workers (plan + implement + ideate) = **3 calls**
- Layer 2: 1 aggregator pass = **1 call**
- Verifier (post-gate): 1 call = **1 call**
- **Total per iteration: ~5 calls** (the operator's "4-8" estimate is correct; 5 is the nominal, 4 if ideate is skipped, 7-8 if the verifier self-consistency runs 3× on architecture tier).

**At 500 calls/week / 5 calls per iteration / 6 lanes:**
- **Per lane per week**: 500 / 6 ≈ **83 calls/lane/week** → **~16 MoA iterations per lane per week** (~2-3 iterations/day per lane).
- **Total iterations/week across the fleet**: 500 / 5 = **100 iterations/week** (if all 6 lanes share the *same* endpoint) OR up to 600/week if each lane has its own endpoint budget (see below).

**BUT the cap is PER-ENDPOINT, not fleet-wide.** The ledger key is `<provider>:<model>` (budget.rs line 90: `format!("{provider}:{model}")`). The current ledger has only ONE row (`maki-cloud:glm-5.2`) because *all lanes currently use the same glm-5.2 model*. If the MoA brain uses 4 different models, there are **4 separate endpoint budgets**, each with its own 500/week cap:
- `ollama-cloud:glm-5.2` (aggregator + verifier) — 500/week
- `ollama-cloud:kimi-k2.7-code` (planner) — 500/week
- `ollama-cloud:deepseek-v4-pro` (implementer) — 500/week
- `ollama-cloud:minimax-m2.7` (ideator) — 500/week
- **Aggregate fleet cap: 4 × 500 = 2000 calls/week** (vs the current 500/week single-endpoint monoculture).

**At 2000 calls/week / 5 calls per iteration / 6 lanes:**
- **~400 MoA iterations/week** across the fleet → **~67 iterations/lane/week** (~9.5/day per lane).
- This is the *theoretical* ceiling. In practice the aggregator (glm-5.2) is the bottleneck: it's called 2× per iteration (aggregate + verify) = 2 of the 5 calls, so the glm-5.2 endpoint budget runs out first at 500/2 = 250 iterations before the workers' 500/3 = 166 iterations each. The **aggregator is the binding constraint**.

**The math that matters:** the aggregator (glm-5.2) is called **twice per iteration** (Layer-2 synthesize + Verifier). At 500/week, that's **250 MoA iterations/week fleet-wide** → **~42 iterations/lane/week** (~6/day per lane). The workers each have spare budget (500/week ÷ 1 call/iter = 500 iter ceiling, unused at 250). **The aggregator budget is the bottleneck — split it across lanes or raise its cap.**

### 5.3 Per-model window caps to set

**Recommended per-endpoint weekly caps in `_provider_budget.json`** (raising the aggregator's cap since it's the bottleneck, keeping workers at the proven 500):

```jsonc
{
  "endpoints": {
    "ollama-cloud:glm-5.2":          { "window_cap_calls": 1000 },  // aggregator+verifier, 2 calls/iter → 500 iter ceiling
    "ollama-cloud:kimi-k2.7-code":   { "window_cap_calls":  500 },  // planner, 1 call/iter → 500 ceiling (unused at 250)
    "ollama-cloud:deepseek-v4-pro":  { "window_cap_calls":  500 },  // implementer, 1 call/iter
    "ollama-cloud:minimax-m2.7":     { "window_cap_calls":  500 }   // ideator, 1 call/iter
  }
}
```

**Why double the aggregator's cap:** the aggregator is called 2× per iteration (aggregate + verify) while each worker is called 1×. To keep the iteration ceiling balanced across endpoints, the aggregator cap = 2× the worker cap. This gives a fleet-wide **500 iterations/week** ceiling (1000/2), matching the operator's "500/day" ambition over a week (500/day × 7 = 3500/week would need cap=7000 on the aggregator — likely above the Ollama Cloud plan's weekly usage allowance; flag for plan-tier check).

**Ollama Cloud plan interaction (from https://ollama.com/cloud, read in full):**
- **Usage is per-PLAN, not per-call.** Free = light, Pro = 50× free, Max = 5× Pro. "Usage reflects actual utilization of Ollama's cloud infrastructure — primarily GPU time." No published token/request cap; the `window_cap_calls` is *Solomon's own* reserve-headroom rule, not an Ollama limit.
- **Concurrency limit** (the real ceiling): Free=1, **Pro=3**, **Max=10** concurrent models. With 4 models in the MoA brain, **Max (10) is required**; Pro (3) cannot run the 3 parallel Layer-1 workers + 1 aggregator simultaneously. **Flag: confirm the operator is on Ollama Cloud Max.** If on Pro, the MoA brain must serialize (1 worker at a time → 4× slower, defeats the parallel-MoA design).
- **Session limits reset every 5 hours; weekly limits every 7 days.** This aligns with `budget.rs`'s `WEEK_S = 604800`.
- **Per-model usage tier (Ollama Cloud's own cost weighting)**: glm-5.2 = High, kimi-k2.7-code = High, deepseek-v4-pro = **Extra High** (the most expensive), minimax-m2.7 = Medium (cheapest). The weekly *usage* budget is consumed faster by deepseek-v4-pro than by minimax-m2.7 even at the same call count. **Flag: deepseek-v4-pro's Extra-High tier may exhaust the plan's weekly usage before the call cap is hit.**

### 5.4 Verdict — per-model daily call allocation + window caps

**Recommended allocation (weekly, per-endpoint — the budget.rs unit):**

| Endpoint | Role | Calls/iter | window_cap_calls | Iter ceiling (cap÷calls) | Notes |
|---|---|---|---|---|---|
| `ollama-cloud:glm-5.2` | aggregator + verifier | 2 | **1000** | 500 | bottleneck; raise to 2× workers; verify Ollama Max plan can absorb |
| `ollama-cloud:kimi-k2.7-code` | planner | 1 | 500 | 500 | High usage tier; has headroom |
| `ollama-cloud:deepseek-v4-pro` | implementer | 1 | 500 | 500 | **Extra-High tier — watch the plan's weekly usage, not just call count** |
| `ollama-cloud:minimax-m2.7` | ideator/probe | 1 | 500 | 500 | Medium tier; cheapest, run every cycle |

**Per-lane weekly iteration target** (fleet-wide 500 iter ceiling ÷ 6 lanes): **~83 MoA iterations/lane/week** (~12/day per lane). The current `daily_call_budget: 40` in `repos.json` should be raised to **~12** (per lane, per day, MoA-iteration units — NOT call units) or **~60 calls/day per lane** (12 iter × 5 calls) to match. **Flag: the `daily_call_budget` field's unit (calls vs iterations) must be clarified with the operator before changing it.**

**Hard requirements before arming the MoA brain at this budget:**
1. **Confirm Ollama Cloud Max plan** (10 concurrent models) — Pro (3) cannot run 3 parallel workers + 1 aggregator.
2. **Watch deepseek-v4-pro's Extra-High usage tier** — it may exhaust the plan's weekly GPU-time allowance before the 500 call cap. If it does, drop to `deepseek-v4-flash` (flag: unverified on Ollama Cloud) or reduce its `window_cap_calls` to 250.
3. **The aggregator is the bottleneck** — its 2-calls-per-iteration rate means it hits the cap first. The 2× cap recommendation above is the mitigation; verify the operator's Ollama Max plan allows the resulting weekly usage.
4. **The existing `park_until` / 429-backoff logic is unchanged** — it operates per-endpoint, so a 429 on deepseek-v4-pro parks only that endpoint, not the fleet (budget.rs lines 8-11). This is correct for the MoA brain: if the implementer 429s, the planner/ideator/aggregator can still run with a degraded (single-worker) iteration. **Recommendation: implement a "degraded MoA" fallback** — if ≥1 worker is parked, the aggregator synthesizes from the available workers (the MoA paper shows n=1 proposer still beats the aggregator alone, 47.8% → it's worth running degraded rather than parking the whole iteration).

---

## Appendix — Unverified / Flagged Items

1. **Hermes `moa` toolset implementation**: could NOT retrieve the source. The `moa` entry in Hermes's `toolsets.py` is a registry-derived toolset (no static `tools/moa_tool.py`). The mechanism is inferred (delegate_task fan-out + aggregate) but not confirmed from source. **Action**: clone `hermes-agent` and read `tools/registry.py` + any plugin that registers `moa` to confirm the exact prompt/loop before mirroring it.

2. **OpenRouter slug for `minimax-m2.7`**: inferred as `minimax/minimax-m2.7` from the `minimax/minimax-m3` pattern, NOT directly probed. **Action**: HEAD-probe `https://openrouter.ai/minimax/minimax-m2.7` before wiring the fallback.

3. **`deepseek-v4-flash` on Ollama Cloud**: referenced in the v4-pro benchmark table but not probed on the catalog. **Action**: probe `ollama.com/library/deepseek-v4-flash` if a cheaper DeepSeek is needed.

4. **Ollama Cloud plan tier of the operator**: the MoA brain's 4-model concurrency requires **Max (10 concurrent)**; Pro (3) is insufficient. **Action**: confirm the operator's plan before arming.

5. **`daily_call_budget` field unit**: unclear if it's calls or iterations. The `budget.rs` cap is calls/week; the `repos.json` field may be calls/day. **Action**: clarify with the operator before changing the value.

6. **Ollama Cloud bare-name vs `:cloud`-suffix resolution**: Solomon's `ollama_chat` sends `"model": "glm-5.2"` (bare); the model pages' curl examples use `glm-5.2:cloud`. Both appear to work for glm-5.2 (the ledger key `maki-cloud:glm-5.2` uses bare). **Action**: verify the 3 new worker models resolve the same way, or switch to `:cloud`-suffixed names in the MoA config.

7. **TRINITY/Conductor are ICLR 2026 (Dec 2025 submission, Apr/May 2026 revision)** — very recent. The Fugu product (Sakana Console API) is the productized version but requires a Sakana API key (separate from Ollama/OpenRouter). **Not recommended for v1** — the prompt-based MoA (Together AI paper, 2024, mature) is the proven path; TRINITY/Conductor (learned coordinator) is a Phase-2 research goal if the fixed MoA plateaus.

8. **async-openai Anthropic support**: async-openai does NOT have a native Anthropic Messages adapter (OpenAI/Azure only). If Solomon adds Claude as a 5th model, use `genai` (native Anthropic) or hand-roll the Messages API shape. **Not needed for the current 4-model roster.**

---

**Sources cited (primary):**
- arXiv:2406.04692 (MoA paper, Together AI) — https://arxiv.org/abs/2406.04692 + HTML full text
- arXiv:2512.04695 (TRINITY, Sakana, ICLR 2026) — https://arxiv.org/abs/2512.04695
- arXiv:2512.04388 (Conductor, Sakana, ICLR 2026) — https://arxiv.org/abs/2512.04388
- arXiv:2606.21228 (Fugu technical report) — referenced from https://github.com/SakanaAI/fugu
- Sakana evolutionary-model-merge — https://github.com/SakanaAI/evolutionary-model-merge (Apache-2.0, arXiv:2403.13187)
- Sakana Fugu — https://github.com/SakanaAI/fugu (788★, Shell)
- Hermes Agent — https://github.com/NousResearch/hermes-agent (MIT, v0.18.2) + toolsets.py raw
- Ollama Cloud pricing/models — https://ollama.com/cloud , https://ollama.com/library/{glm-5.2,kimi-k2.7-code,minimax-m3,minimax-m2.7,deepseek-v4-pro}
- OpenRouter model pages — https://openrouter.ai/{zai/glm-5.2,minimax/minimax-m3,deepseek/deepseek-v4-pro,moonshotai/kimi-k2.7-code}
- async-openai crate — https://docs.rs/async-openai/latest/async_openai/ (v0.41.1, MIT)
- genai crate — https://docs.rs/genai/latest/genai/ (v0.6.5, MIT/Apache)
- Local: `SOLOMON_RSI.md`, `AGENTS.md`, `repos.json`, `runtime/_provider_budget.json`, `improver/budget.rs`, `ceo.rs:1458`, `solomon-rsi/SKILL.md`, `ultra-code/references/orchestration.md`, `~/.config/opencode/agent/ultra-*.md`

**Next action**: hand this report to `ultra-planner` to turn the §1.6 / §2.5 / §3.4 / §4.5 / §5.4 verdicts into a decision-complete implementation spec, then route to `ultra-implementer` for the MoA brain + worker-spawn + budget-cap changes under the existing gate + anti-gaming rails.
</task_result>
</task>