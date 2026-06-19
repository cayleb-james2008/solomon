# Solomon — 10 Non-Shallow Improvements (draft)

Drafted as a design input, not a shallow backlog. Each item names the structural
problem, the proposed change, and why it moves the needle (vs. a cosmetic win).
Ordered by leverage, highest first.

---

## 1. Close the loop on the loop: self-applied RSI with a Solomon-specific needle

**Problem.** Solomon orchestrates RSI for managed repos but is not itself a managed
repo under its own loop — the harness that enforces the invariants never compounds.
Every hardening item in `SOLOMON_RSI.md` ("make invariants mechanically enforced")
was landed by hand, not by the loop. That is the exact anti-pattern the spec warns
about: a greedy ratchet that only climbs shallow local optima because no one runs
the divergent lane on the orchestrator itself.

**Change.** Register `solomon` as a managed repo under its own control surface
(`repos.json` row pointing at this folder), with a north-star goal of *"mechanically
enforce every SOLOMON_RSI invariant and raise the gate's anti-gaming coverage; a
truthful null beats a gamed green."* Add a Solomon-specific `EVAL_CMD` that scores
the invariant-coverage ratio (count of invariants with a test in
`tests/test_invariants.py` / total invariants) so the richer needle rewards
*enforcement*, not *test count*. The supervisor treats Solomon's own repo with the
same RUNG-0…2 ladder — no special-casing.

**Why it moves the needle.** Turns the orchestrator from a hand-maintained tool into
a compounding system. The same gate + anti-gaming + PR-ship discipline that improves
managed repos now improves the thing that enforces it. Recursive self-improvement
that is actually recursive, not performed on others.

---

## 2. Gate the gate: a meta-gate that validates the runner's anti-gaming checks don't regress

**Problem.** Anti-gaming (test-count-not-dropped, no new `skip`/`xfail`, errors did
not rise) lives in `run_improver.run_gate()` and is itself untested against
adversarial inputs — an agent could weaken *the checks themselves* on an
`rsi/*` branch and a green meta-gate would still pass because the meta-gate runs
the weakened checks. The spec calls this out ("an agent trusted to score itself
learns to weaken the check") but the guard is prose-only for the checker code.

**Change.** Add `tests/test_anti_gaming_tamper.py` with a fixture suite of
adversarial gate outputs (deleted tests, mass `skip`, dropping `collected`,
stubbed gate commands) and assert the checker rejects every one. Then add a
**meta-gate step** in the runner that, before running the repo's real gate,
runs the anti-gaming checker against a *known-bad* synthetic output and requires
the checker to reject it — a canary proving the checker wasn't tampered with on
this branch. If the canary passes (checker still rejects known-bad), the real gate
is trusted; if the canary fails, the branch reverts regardless of the real gate.

**Why.** Makes "the agent weakened the checker" a detectable, revertible failure
mode instead of a silent trust hole. Closes the single deepest gap in the
`gate-enforced-by-runner` invariant.

---

## 3. Replace the heartbeat file protocol with a typed, versioned, signed heartbeat

**Problem.** The runner ↔ dashboard contract is a JSON file
(`.rsi/heartbeat.json`) with an unversioned ad-hoc schema documented only in a
docstring. Adding a field requires editing four places (writer, reader, UI,
tests) and a stale reader silently shows stale state. The `run_id` token was
added late and bolted on — exactly the schema-drift the spec's "compounding base"
invariant is meant to prevent, but for the protocol itself.

**Change.** Introduce `Heartbeat` as a `dataclass`-with-`schema_version` in a new
`improver/protocol.py`, migrated once on read. Sign the heartbeat with an
ephemeral per-run HMAC key written only to `runtime/<name>/run_key` (mode 0600)
so a recycled PID + stale file can't impersonate a live runner — the existing
`run_id` becomes a derived field, not the whole defense. The dashboard refuses
to render a heartbeat whose `schema_version` is ahead of what it understands
(surfaces "restart Solomon to read this state" instead of silently dropping
fields), and refuses one whose HMAC fails (treats it as stale).

**Why.** Eliminates an entire class of "the UI showed a runner as stopped but it
was actually iterating" bugs at the protocol level, and makes future
heartbeat additions (screenshots-in-heartbeat, sub-iteration phase events,
agent-cursor positions for Feature 1) safe to add without four-way edits.

---

## 4. Branch-per-iteration is per-iteration, not per-repo — parallelize the ideate lane

**Problem.** The `branch-per-iteration` invariant serializes the loop: one
iteration at a time per repo, because the runner checks out the base, cuts a
branch, runs the gate, ships, loops. The ideate lane (`run_improver --ideate`)
is a *separate* one-shot that blocks the main loop when run. For ambitious
repos this caps throughput at ~1 iteration per gate-time, and ideation is
dead time for shipping.

**Change.** Split the loop into two cooperating processes per repo: an
**exploit loop** (the current runner, unchanged) and an **ideate worker** (a
detached process that only writes backlog items, never touches git). The
ideate worker runs on its own cadence and prepends to `backlog.md` under a
file lock; the exploit loop reads `backlog.md` top-first each iteration as
today. Because the ideate worker never mutates git, `branch-per-iteration` is
preserved — the invariant is about *shipping*, not *thinking*. Throughput rises
to 1 iteration per gate-time with ideation fully overlapped.

**Why.** Removes the false serialization between divergent and convergent work
without touching any invariant. The depth-vs-throughput tradeoff the spec
identifies ("a greedy ratchet climbs shallow local optima") gets solved by
parallelism, not by weakening the ratchet.

---

## 5. Make `EVAL_CMD` a graph, not a scalar — multi-needle eval with Pareto frontier

**Problem.** `EVAL_CMD` parses a single float from stdout. Real products have
*several* needles (latency p99, MAU, revenue, bug-open-rate) that trade off —
a green test gate that improves one while regressing another is currently
indistinguishable. The spec says "evals are everything" but the eval is a point,
not a frontier.

**Change.** Allow `EVAL_CMD` to emit JSON: `{"p99_ms":210,"mau":1234,"rev":42.10}`.
The runner records the full vector in `history.jsonl`, computes a Pareto
dominance check against the baseline (new vector dominates old iff it is
≥ on every key and > on at least one), and reverts on *regression* (dominated)
but ships on *improvement* (dominates) or *tradeoff* (non-dominated — neither
dominates). Tradeoffs are surfaced to the operator as a reviewable PR with the
vector diff, not auto-reverted (preserves human judgment on genuine tradeoffs).
The scalar form is backward-compatible (a float is parsed as a single-key
vector).

**Why.** Stops the agent from gaming a single needle by sacrificing others
("latency went down because error rate went up" is currently a green gate).
Turns the eval from a gate into a *decision surface* the operator can reason
about, which is what the spec's autonomy dial ("oversight stays") actually
requires.

---

## 6. Supervisor with a model: a tiny per-repo learned failure classifier

**Problem.** `solomon.diagnose()` is a hand-written rule cascade
(`category = dirty_tree | revert_failed | ci_red_streak | ...`). It works but
never improves — the same failure mode misnamed today is misnamed forever,
and the operator has no way to correct it except editing the rule code. The
spec's "supervisor-authorized recovery" ladder is RUNG-first, but the
*diagnosis* step that picks the rung is static.

**Change.** Add `improver/diagnose.py` with a tiny classifier: a per-repo
JSONL of `{evidence, category, rung, outcome}` rows in
`runtime/<name>/diagnoses.jsonl`. On each diagnose call, the rule cascade
runs first (unchanged); if it escalates or the operator corrects the
category in the UI, the correction is appended to the JSONL. Every N=50
corrections, a background job fits a zero-shot prompt (or a scikit-learn
logistic regression over hashed evidence tokens, whichever the repo's
stack allows) and the rule cascade gains a `learned_override` that runs
*after* the rules and *before* escalation, with a confidence threshold.
A wrong learned override is itself a correction row, so the classifier
self-corrects.

**Why.** Turns the supervisor from a static ruleset into a system that learns
the *operator's* labeling behavior for failure modes the hand-written rules
don't cover — without giving up the RUNG ladder (learned overrides still
go through the same safe recovery, they just pick the rung better). This is
the "autonomy grows, oversight stays" principle applied to the supervisor
itself.

---

## 7. Worktree-aware everything: the runner should never operate on a bare checkout

**Problem.** The runner does `git checkout BASE_BRANCH` between iterations
and `cleanup_worktrees` prunes — but the repo itself is a single working tree
the operator may also have open in an editor. A long gate run + an operator
editing the same tree is a silent footgun: the operator's uncommitted edit
can be reset by the runner's preflight `reset_to_base`, or the operator can
accidentally commit onto the iteration branch. `branch-per-iteration` is
honored at the branch level but not at the working-tree level.

**Change.** Make the runner **worktree-native**: each iteration runs in a
fresh `git worktree add` of the base branch under
`runtime/<name>/worktrees/<iteration>/`, leaving the operator's primary
checkout untouched. `cleanup_worktrees` becomes the iteration pruner (it
already exists, just unused for the live loop). The operator's checkout
becomes read-mostly — they edit `AGENT.md`/`backlog.md` (which live in
Solomon's `improver/<name>/`, not the repo) and review PRs. Feature 2's
visualization renders these worktrees explicitly so the operator sees what
the runner is standing in.

**Why.** Removes the operator/runner working-tree contention that the
`never-hand-patched` keystone implicitly assumes away but doesn't enforce.
A worktree-per-iteration makes "the operator never edits the repo's working
tree" *mechanically true* instead of conventionally true, and makes the
"clean base" precondition of every iteration trivially satisfiable (the
worktree is brand new).

---

## 8. Stop shipping on a green gate alone: add a "did the agent's own test run?" precondition

**Problem.** The gate runs the repo's test command and checks pass + collected
counts. But nothing requires the agent to have *added or run a test for the
change it made* — an agent can ship a behavior change with zero new tests as
long as the existing suite is green and the count didn't drop. The spec's
anti-gaming catches *deletion* and *weakening*, not *omission*.

**Change.** Add an anti-omission check: if the diff touches a non-test file
and adds/changes behavior (heuristic: diff contains a new `def `/`function `/`export `
outside `*_test.*`/`test_*`), the runner requires either (a) a test file in
the diff, or (b) the changed file already has a test that the runner's
correlated-test discovery (`_find_correlated_tests`) picks up. If neither,
the branch is marked `needs_test` and reverted with a feedback line
telling the agent to add a covering test for the named symbol. The agent
gets the feedback on the next iteration. A repo can opt out with
`allow_untested_changes: true` (docs/config repos, etc.).

**Why.** Closes the "green by omission" loophole. The spec's gate is a
regression gate, not a coverage gate — this adds a minimal coverage gate
that still respects "the runner adjudicates, never the model" (the
heuristic is deterministic, not model-judged).

---

## 9. Make the halt switch reversible and auditable — not just a sentinel file

**Problem.** `STOP` is a sentinel file; `Start` must not silently revoke a live
stop. But there's no audit trail of who stopped, when, or why, and a stale
`STOP` file left by a crashed loop silently blocks the next start until
manual cleanup. The safety rail is a file, not a state machine.

**Change.** Replace the sentinel with `runtime/<name>/control.jsonl` — an
append-only log of `{ts, action: stop|start|supervise, actor: operator|supervisor|watchdog, reason}`.
A `STOP` is a *flag set on the latest record*, not a file's existence; `Start`
appends a `start` record only if the latest record is not a live `stop`
from the operator (supervisor/watchdog stops auto-clear on recovery). The
watchdog, supervisor, and operator all write here, so the audit trail is
the source of truth and `is_running`/`should_restart` read the same log.
A stale-stop detector clears a `stop` older than 24h with a
`auto_cleared_stale_stop` record (visible to the operator).

**Why.** Turns the halt switch from a boolean file into an audited state
machine. The spec's "halt switch" safety rail becomes debuggable
("why did my loop stop at 3am?") and self-healing (stale stops clear
themselves with a record, not silently).

---

## 10. A real eval harness for Solomon itself — not just for the repos it manages

**Problem.** Solomon ships with 259 unit tests but no end-to-end eval of the
*loop's behavior over time* — does a repo under Solomon actually improve on
its needle faster than a baseline (random-walk commits, or no loop)? The
spec leans on "the eval is the ceiling on depth," but Solomon's own
ceiling is unmeasured. A refactor that makes the loop 10% slower at
compounding would pass the current suite.

**Change.** Add `tests/eval_loop.py` (not run in the default gate — it's
slow): a synthetic toy repo with a known fitness function (e.g. "make the
string sorted"), a pi mock that proposes a single swap per iteration, and
a runner that actually ships. Measure *time-to-sorted* and
*iterations-to-sorted* over N=20 runs against (a) the real loop and (b) a
random-swap baseline. The eval publishes a `loop_fitness` number to
`runtime/solomon/loop_eval.json` that the dashboard surfaces. A regression
that slows compounding shows up as a number, not a vibes report.

**Why.** The spec says "evals are everything" and then doesn't eval the
loop. This is the single highest-leverage missing eval in the whole system:
it turns "is Solomon actually doing RSI?" from an article of faith into a
measured quantity, and it catches the class of regressions (slower
compounding, worse selection) that no unit test can see.

---