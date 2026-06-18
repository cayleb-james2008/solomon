# Solomon architecture review � findings

_Adversarial multi-agent review (7 dimensions + verify), 53 unique findings. Generated 2026-06-18._


## CRITICAL

### RUNG-0 dirty_tree recovery (reset_to_base) silently destroys operator WIP � only un-pushed COMMITS are guarded, not uncommitted tracked changes
- **file:** `control.py` reset_to_base(), lines 1240-1257; invoked from solomon.recover() dirty_tree branch line 227
- **category:** invariant-violation
- **problem:** The dirty_tree diagnosis fires precisely when the working tree has uncommitted TRACKED changes (tree_dirty() uses --untracked-files=no). RUNG-0 recover() then calls control.reset_to_base(), whose only safety guard checks origin/base..base for un-PUSHED COMMITS. It does NOT check for uncommitted working-tree changes. It runs `git checkout --force <base>` (discards uncommitted tracked edits on the current branch) then `git reset --hard origin/base` � destroying exactly the operator WIP that tree_dirty detected, with no escalation. The is_running() guard in recover() only blocks the reset while a loop is LIVE; once the operator stops the loop (the documented remedy: 'commit or stash your changes; the loop resumes once it's clean'), the very next unattended --supervise sweep diagnoses dirty_tree as auto_safe and force-resets, deleting the work. This violates the never-hand-patched/reversible-first invariant ('RUNG-0 never discards un-pushed commits') because uncommitted changes are even less recoverable than un-pushed commits, yet are unguarded.
- **fix:** In reset_to_base(), before the force-checkout, run `git status --porcelain --untracked-files=no`; if non-empty, return {ok:False,error:'uncommitted tracked changes � escalate (won't auto-discard)'} so recover() escalates to RUNG-2 instead of destroying WIP. The dirty_tree diagnosis should never be auto_safe when the dirt is operator-authored.

### Revert-failure wedge: loop spins forever at status=error with the rsi branch checked out and no auto-recovery
- **file:** `improver/run_improver.py` _abort_branch / _drop_branch (lines 337-366) + main loop (1248-1262); diagnose/recover in improver/solomon.py (75-77, 196-257)
- **category:** wedge
- **problem:** When _abort_branch's `git checkout --force BASE_BRANCH` fails (e.g. a leaked file handle on a tracked file � routine on Windows after a test subprocess � or a name conflict), _abort_branch returns False. `git reset --hard` (line 341) already ran, so the tree has NO dirty TRACKED files, but HEAD is still on the rsi branch with un-reverted committed code. _drop_branch writes status=error/phase=reverted and returns. The main `while True` loop (line 1253) calls one_iteration() again every interval with no check on _hb['status']. Next iteration tree_dirty() (tracked-only) returns False, preflight retries `checkout --force` which fails the same way, and the loop spins producing status=error with zero progress. The supervisor maps status=error+phase=reverted to category `revert_failed` which is auto_safe=False and != gate_red_streak, so recover() escalates and NEVER runs the reset it recommends � the only exit is manual operator action. The branch also leaks (reset_to_base explicitly never deletes feature branches).
- **fix:** After a failed _abort_branch, set the STOP sentinel (or break the loop) so the runner halts instead of spinning; and make solomon.recover() auto-run reset_to_base for `revert_failed` when the loop is not live (it already guards is_running), plus delete the leaked branch via cleanup_worktrees.

### Dirty-tree + live-loop deadlock: preflight wedges on status=error but the loop never exits, so the supervisor refuses to reset
- **file:** `improver/run_improver.py` one_iteration tree_dirty guard (714-719); solomon.recover dirty_tree branch (221-230)
- **category:** wedge
- **problem:** If a TRACKED file becomes dirty (operator WIP, or � more insidiously � a tool that rewrites a tracked file the agent left modified that reset --hard couldn't reach because checkout failed earlier), one_iteration() returns at line 718 with status=error/phase=preflight every iteration. The loop process stays alive and keeps looping, so control.is_running() is True. The supervisor classifies this as `dirty_tree` (auto_safe), but recover() refuses to reset_to_base while the loop is live (line 224-226) and escalates. Net result: the live loop can't clean the tree (it only skips), and the supervisor won't clean it because the loop is live � a hard deadlock that only ends when the operator manually stops the loop AND resets. The loop burns an iteration's CPU/log churn every interval with zero progress indefinitely.
- **fix:** On a persistent dirty tree (e.g. N consecutive dirty-tree skips), have the runner itself self-stop (write STOP) so is_running() goes False and the supervisor's dirty_tree auto-recovery can run; or stash-and-continue tracked changes with an explicit operator-recoverable note.


## HIGH

### Auto-merge CI-red ship has NO diagnose category and NO recovery path � permanent silent wedge
- **file:** `improver/solomon.py` diagnose() lines 70-88; vs run_improver.py one_iteration() lines 877-886 and _auto_merge() line 690
- **category:** wedge
- **problem:** When ship=auto-merge opens a PR whose CI is red, _auto_merge returns state 'open (CI red � not merged)' and the iteration STILL records history status='shipped' (line 886) and heartbeat status='sleeping' (line 885). diagnose() classifies by heartbeat status/phase and history status; 'shipped'/'sleeping' map to category 'ok'. gate_red_streak only matches history status in ('reverted','error'), never 'shipped'. So every subsequent iteration re-bases off main (which lacks the un-merged PR), the agent re-implements the same backlog item, opens a NEW red PR, and the supervisor reports 'ok' forever. There is literally no diagnose category that inspects the open-PR CI rollup (pr['checks']=='failure'), so this unhealthy state has zero recovery path. This is broader than the known PR-7 spin: it's an entire class of unhealthy state invisible to the watchdog.
- **fix:** Add a diagnose category (e.g. ci_red_streak) that reads list_prs/heartbeat last_pr and flags when the most recent N shipped PRs carry checks=='failure' (or a stuck auto-merge), routing to RUNG-1 fix-session / escalation. Alternatively record history status='ci_red' when _auto_merge leaves a PR open on red, and include it in the gate_red_streak status set.

### 'stuck' (hang) detection only covers phase=='implement' � hangs in test/commit/ship/pr/merge phases are never diagnosed
- **file:** `improver/solomon.py` diagnose() line 84
- **category:** wedge
- **problem:** The stuck category requires `running and phase == "implement" and _stale(hb,repo)`. But the loop sets phase to 'test', 'commit', 'ship', 'pr', and 'merge' during an iteration (run_improver lines 828/846/922/938/948), and run_gate (subprocess.run with NO timeout) or _await_pr_checks (up to 6�8s, plus gh network calls) can hang there. A gate command that hangs (an interactive prompt, a deadlocked test) blocks with no improver timeout � run_gate's subprocess.run has no timeout argument, unlike run_pi. Since phase is 'test', diagnose never returns 'stuck', the lock stays held, is_running() stays True, and no recovery ever fires: a silent wedge. _stale's own threshold (max(3*interval, 3600) = at least 1h) further means even an implement-phase hang takes an hour to flag.
- **fix:** Broaden the stuck check to any non-sleep/non-idle phase while running and stale (drop the `phase=='implement'` restriction, or whitelist sleep/idle), and add a timeout to run_gate's subprocess.run so a hung gate self-aborts the iteration instead of pinning the lock.

### ship=push never ticks the backlog -> loop re-pushes the same item forever (duplicate branches)
- **file:** `improver/run_improver.py` _ship (lines 933-936), _ship_succeeded (889-895), one_iteration tick (883-884)
- **category:** wedge
- **problem:** In ship=push mode _ship returns state='pushed (no PR)' with number=None. _ship_succeeded only returns True for a PR number or a state containing 'local', so 'pushed (no PR)' yields False and _mark_backlog_done is never called. Every subsequent iteration re-bases off main (which lacks the pushed branch), re-selects the SAME top backlog item, and pushes a new timestamped rsi/iter-* branch for an identical change. The backlog never advances and origin accumulates duplicate near-identical branches indefinitely. SOLOMON_RSI.md step 8 only enumerates tick-on-open(pr)/merge(auto-merge)/keep(local); push falls through every gate.
- **fix:** Treat a verified push as a successful ship for backlog-advance: in _ship_succeeded also return True when state.startswith('pushed') and pr.get('verified') is True (and not a fail state).

### auto-merge red-CI PR still ticks the backlog item and records 'shipped' -> item lost, red PR leaks, no escalation
- **file:** `improver/run_improver.py` _auto_merge (688-690), _ship_succeeded (892-893), one_iteration (883-886), solomon.py diagnose (86-88)
- **category:** invariant-violation
- **problem:** When _auto_merge sees checks=='failure' it returns state='open (CI red - not merged)' but KEEPS pr['number']. Back in one_iteration, _ship_succeeded(pr) returns True (number present) and not item_deviated, so _mark_backlog_done(goal) ticks the item, and _record_history('shipped', ...) records success. The backlog item is consumed while the change never reached the base; the red PR is left open forever. solomon.py diagnose() gate_red_streak only fires on history statuses 'reverted'/'error' (line 86), so a stream of 'shipped' red-CI auto-merges is invisible to the supervisor and never escalates. This is the deeper mechanism behind the observed 3h spin on PR 7: the item was already ticked, so the loop moved on while the red PR rotted.
- **fix:** In _auto_merge, drop or null the number on the red-CI/awaiting-CI leave-open states, and have one_iteration record status='blocked' (not 'shipped') + skip _mark_backlog_done when pr['state'] indicates an un-merged auto-merge PR; add a diagnose category for open-but-red/queued auto-merge PRs so the supervisor escalates.

### STOP during the auto-merge CI poll makes _await_pr_checks return None, which _auto_merge merges immediately (CI bypass)
- **file:** `improver/run_improver.py` _await_pr_checks (640-648), _auto_merge (684-701)
- **category:** race
- **problem:** _await_pr_checks breaks out of its poll loop when STOP.exists() and returns `last`, which is still None if the freshly-created PR's rollup hasn't registered yet. _auto_merge treats checks==None as 'no CI configured -> merge now' and squash-merges immediately. So pressing Stop during the post-PR-create CI window causes an un-CI'd PR to be force-merged into the base � the exact empty-rollup bypass _await_pr_checks exists to prevent, and a violation of pr-only-shipping-with-auto-revert (CI). The iteration's only STOP guard (line 866) runs BEFORE _ship, so a Stop arriving during ship/poll is never re-checked before the merge.
- **fix:** Distinguish 'aborted by STOP' from 'genuinely no CI': have _await_pr_checks return a sentinel (e.g. 'pending') when it broke on STOP without a concrete state, or have _auto_merge refuse to merge (leave open) whenever the iteration was stopped mid-poll.

### Windows PID reuse makes lock liveness checks unreliable (wedge + false-steal)
- **file:** `improver/run_improver.py` acquire_lock (run_improver.py:967-988); _pid_alive/is_running/clear_lock (control.py:548-574, 1204-1222)
- **category:** race
- **problem:** acquire_lock(), is_running() (control.py) and clear_lock() (control.py) all decide whether a lock is held by reading a recorded PID and asking tasklist whether ANY process with that PID is alive. Windows aggressively recycles PIDs. Two distinct failures follow once the original runner has died: (a) WEDGE � the dead runner's PID gets reassigned to some unrelated process; _pid_alive returns True, so acquire_lock returns False ("already running"), control.clear_lock refuses ("lock held by live pid"), and the supervisor's stale_lock recovery can never fire � the loop is permanently un-startable with no live runner. (b) FALSE-LIVE � is_running() reports a foreign process as the running improver, so app.get_state()/the dashboard shows the repo as running and start() short-circuits, when in fact no loop exists. The lock records only the PID, with no start-time/identity token to distinguish 'my runner' from 'whatever owns this PID now'.
- **fix:** Record an identity token alongside the PID (e.g. PID + process creation time, or a random run-id written into the heartbeat and cross-checked), and treat the lock as live only when BOTH the PID is alive AND the token matches the live runner � so a recycled PID is correctly seen as stale.

### release_lock unconditionally deletes the lock on any read error, even one another process owns
- **file:** `improver/run_improver.py` release_lock (run_improver.py:991-999)
- **category:** race
- **problem:** release_lock is documented to 'only unlink if the lock is still ours', but its except clause deletes the lock unconditionally on ANY OSError/ValueError from the read. Concrete scenario with two runners on one repo (the known acquire_lock TOCTOU already lets two coexist): runner A is mid-release and reads the lock; if the read transiently fails (file locked/replaced by runner B's os.replace in acquire_lock, or a partial/empty read during B's two-step write) the int() raises ValueError, and the except path calls LOCK.unlink() � deleting the lock that runner B legitimately holds. A third acquirer then wins the now-absent lock via open(LOCK,'x'), producing a third concurrent runner. The fallback should never delete a lock whose ownership it could not confirm.
- **fix:** Drop the unconditional fallback unlink; on a read error, leave the lock alone (a genuinely stale lock is reclaimed later by acquire_lock's stale-PID path). Only unlink when the read confirms the PID equals os.getpid().

### start() deletes the STOP sentinel before checking is_running � silently revokes a pending Stop against a live loop
- **file:** `control.py` start (control.py:580-602)
- **category:** invariant-violation
- **problem:** SOLOMON_RSI.md's halt-switch invariant says a Stop must be honored and 'Start must not silently revoke a live stop'. The code's intent (comment) is that Start always means 'do not stop', but the ORDERING produces a silent-revoke bug for a live loop: start() removes runtime/<name>/stop FIRST, THEN checks is_running(). When a loop IS running and an operator had just clicked Stop (sentinel present, loop about to read it at the next inter-iteration/cooldown poll), an immediate Start removes the sentinel and returns {already:True} without spawning. The live loop never sees the stop, so the Stop the operator issued is silently discarded mid-flight while the loop keeps running and may open a PR � exactly the 'Start silently revokes a live stop' case the spec forbids. The deletion is unconditional and not coupled to actually (re)launching a fresh run.
- **fix:** Only clear the stop sentinel on the path that actually spawns a fresh runner (i.e. after the is_running short-circuit, guarding the new Popen). If a loop is already running, do not delete its pending stop.

### Supervisor 'stuck' restart races the stopped loop's lock release and re-arms the acquire_lock TOCTOU
- **file:** `improver/solomon.py` recover stuck branch (solomon.py:231-243)
- **category:** race
- **problem:** The 'stuck' recovery calls control.stop() (writes the stop sentinel), waits up to 10s for is_running() to go false, then control.start() which deletes the sentinel and spawns a new runner. Two problems compound: (1) is_running() reads the lock PID, but the old runner only checks STOP between iterations/during the 1s cooldown loop � it can have exited its loop yet still be inside its finally block (writing heartbeat, release_lock, unlink stop) when the 10s window elapses; if the old PID is already gone from tasklist but release_lock hasn't run, the lock file still holds the dead PID. (2) control.start() then deletes the stop sentinel and spawns the new child while the OLD runner's finally may STILL run STOP.unlink() (harmless) and release_lock() � and the new child's acquire_lock now hits the empty-file/stale-PID TOCTOU against a lock in flux, the precise two-runners condition observed live. The restart path has no barrier ensuring the old runner has fully released before the new one acquires.
- **fix:** After the grace window, require the lock file to be ABSENT (old runner's release_lock completed) � not merely is_running()==False � before restarting; if the lock is still present, clear it via clear_lock (which now must use the identity token) or escalate rather than spawning into a contended lock.

### auto-merge CI-red PR still ticks the backlog item as done (compounding-base + pr-only-shipping violation)
- **file:** `improver/run_improver.py` _ship_succeeded() lines 889-895; one_iteration() line 883; _auto_merge() lines 687-690
- **category:** invariant-violation
- **problem:** In ship=auto-merge mode, when a PR's CI is red, _auto_merge() leaves the PR open and returns it UNCHANGED with its number intact ({**pr, 'state': 'open (CI red � not merged)'}). Back in one_iteration(), the backlog-advance guard is `_ship_succeeded(pr) and not item_deviated`, and _ship_succeeded returns True for any pr with a number � it never inspects merge state or CI. So the backlog item is marked `- [x]` even though the change NEVER merged and CI is RED. Next iteration re-bases off the integration branch (which lacks the unmerged PR), advances to the next item, and the red change is orphaned forever. This violates compounding-base (only green PR-merged changes should advance) and pr-only-shipping-with-auto-revert (a CI-red change is silently treated as shipped). It is deeper than the known auto-merge spin: the item is permanently lost, not merely re-tried.
- **fix:** In _ship_succeeded (or the tick guard), for ship=auto-merge require pr.get('state')=='merged' (or a CI-success state) before returning True; for pr-mode keep ticking on PR-open as designed but never tick when the auto-merge state string contains 'CI red'/'not merged'/'awaiting CI'.

### Supervisor performs git mutations under is_running() TOCTOU without holding the runner's single-flight lock
- **file:** `improver/solomon.py` recover() dirty_tree branch lines 221-230 and stuck/gate_red_streak branches; control.reset_to_base lines 1225-1257
- **category:** race
- **problem:** SOLOMON_RSI invariant supervisor-authorized-recovery requires the supervisor to hold the runner's single-flight lock during recovery 'so it never mutates git under a live iteration.' The code instead only does a non-atomic `if control.is_running(repo): escalate` check, then calls control.reset_to_base() (git checkout --force + reset --hard origin/base) WITHOUT acquiring runtime/<name>/lock. Between the is_running() check and the reset, a runner can start an iteration (it writes its lock only late in main(), after gh/key/branch checks � a multi-second window during which is_running() returns False), then the supervisor and the runner mutate the same working tree concurrently: hard-reset vs. branch-checkout/commit. The lock that would actually serialize them is never taken by the supervisor.
- **fix:** Have the supervisor acquire the same runtime/<name>/lock (run_improver.acquire_lock semantics) before any git-mutating recovery and release it after, so it is mutually exclusive with a runner iteration rather than relying on a racy is_running() snapshot.

### Preflight refuse-on-out-of-band-base-move is invisible to the supervisor; the loop wedges forever and is reported healthy
- **file:** `improver/solomon.py` diagnose() lines 70-89; run_improver.one_iteration() preflight refuse lines 742-750
- **category:** wedge
- **problem:** The never-hand-patched keystone is enforced at the runner: when BASE_BRANCH has un-pushed commits, one_iteration() sets status='error', phase='preflight' with a summary containing 'Refusing to hard-reset' and returns � but the loop keeps running (sleeps the interval and retries the same refuse every iteration), and writes NO history.jsonl record. diagnose() has no branch that matches this state: the phase=='preflight' branch additionally requires 'dirty' in the summary (which this summary lacks), has_lock&&!running is false (the runner is alive holding the lock), and gate_red_streak needs >=3 reverted/error rows in history.jsonl (none are written). Every other branch fails, so diagnose falls through to cat='ok' / healthy=True. Result: a repo with an out-of-band base commit spins doing zero work indefinitely while the dashboard and `--supervise` sweep both report it HEALTHY � the keystone refuses correctly but the operator is never alerted to act.
- **fix:** Add a diagnose branch for status=='error' and phase=='preflight' with 'out-of-band'/'Refusing to hard-reset'/'not on origin' in the summary -> category like 'base_out_of_band', auto_safe=False, escalate with copy-paste push/revert steps; and/or have the runner _record_history('error',...) on the refuse so gate_red_streak can also catch it.

### Anti-gaming skip/xfail check is blind to new (untracked) test files
- **file:** `improver/run_improver.py` one_iteration line 839; _new_skip_markers line 481-486
- **category:** invariant-violation
- **problem:** The skip/xfail anti-gaming rail runs BEFORE the commit (git add/commit are at lines 847-851), so it inspects `git diff BASE_BRANCH` of the still-uncommitted working tree. `git diff <commit>` only shows changes to TRACKED files; it omits untracked files entirely. Pi normally creates NEW test files (untracked), so any skip/xfail/`@pytest.mark.skip` it writes into a brand-new test file is invisible to `_new_skip_markers`. A change that adds a new test file full of `@pytest.mark.skip`-decorated stubs (or that re-implements a moved test as a skipped stub) passes the gate green with the skip rail seeing an empty diff. Spec step 6 requires 'tests not deleted, weakened, skip/xfail-ed' to be enforced.
- **fix:** Run `git add -A` (or stage) before computing the anti-gaming diff, and use `git diff --cached BASE_BRANCH` (or `git diff BASE_BRANCH -- .` after add) so new/untracked files are included; or compute the diff after commit against BASE_BRANCH..HEAD.

### Anti-gaming only tracks `passed`, never the spec-mandated `collected` count
- **file:** `improver/run_improver.py` run_gate line 444-478; _anti_gaming_reason line 489-499
- **category:** invariant-violation
- **problem:** SOLOMON_RSI.md step 2 and step 6 explicitly require the baseline to capture 'the pass + collected counts' and anti-gaming to verify 'pass + collected counts did not drop vs the baseline'. run_gate never parses pytest's 'N collected' / 'N selected' line, and _anti_gaming_reason compares only `passed`. A gamed change can delete 5 real tests and add 5 trivial `assert True` tests: `passed` stays flat (or rises), the diff has no skip markers, and the gate ships it green. The collected-count rail that would catch test-set churn is simply absent.
- **fix:** Parse pytest's collected count (e.g. `(\d+) (?:items?|tests?) collected` / the `collected N items` header) and unittest's `Ran N`, store it as `collected`, and add a `collected` drop check to _anti_gaming_reason as the spec requires.

### _new_skip_markers misses the common skip forms (skip() call, skipIf, skipTest, SkipTest)
- **file:** `improver/run_improver.py` _new_skip_markers line 481-486
- **category:** robustness
- **problem:** The regex `@\s*(pytest\.mark\.(skip|xfail)|unittest\.skip)` only catches DECORATOR-form skips. It does not match: an in-body `pytest.skip("wip")` / `pytest.xfail(...)` call, `@pytest.mark.skipif(...)`, `self.skipTest(...)`, `raise unittest.SkipTest`, or a bare early `return` that neuters a test. A test gutted by inserting `pytest.skip('todo')` as its first statement keeps the file 'collected and passed-or-skipped', leaves the decorator-only regex unmatched, and ships green � the most natural way an agent weakens a test is not detected.
- **fix:** Broaden the regex to also match `pytest\.(skip|xfail)\s*\(`, `pytest\.mark\.skipif`, `\.skipTest\s*\(`, and `raise\s+(unittest\.)?SkipTest`; pairing this with a real collected/passed-delta check is the durable fix.

### release_lock() deletes a lock it cannot parse � re-opens two-runners-on-one-repo
- **file:** `improver/run_improver.py` release_lock(), lines 1030-1038
- **category:** race
- **problem:** acquire_lock()'s TOCTOU was fixed, but release_lock still has a steal path. It reads the lock, and on ANY OSError/ValueError (e.g. the file contains a non-integer: a racer's partial write, an os.replace mid-flight, or corruption) it falls into the except branch and unconditionally unlinks the lock � even though the lock may be held by another LIVE improver. This violates the 'only unlink if it's ours' guarantee precisely in the corruption case that the new acquire_lock takes pains to tolerate, and re-creates the exact two-concurrent-runners failure (PIDs 17964+18152) the TOCTOU fix was meant to close: process A can't parse the lock B just wrote, deletes it, a third acquirer sees no lock and starts alongside B.
- **fix:** In the except branch, do NOT unlink. Only unlink when the parsed pid equals os.getpid(); on parse failure, leave the lock alone (a future stale-takeover will reclaim it if dead).

### Solomon fix-session ignores the global auto_push gate � pushes/PRs/auto-merges when operator disabled pushing
- **file:** `improver/solomon.py` solomon_fix_session() line 182; app.py supervise() line 219-221
- **category:** invariant-violation
- **problem:** The autonomy model (SOLOMON_RSI.md) states auto_push=False must keep all work local (no push/PR). control.start/beautify correctly use effective_ship(repo, auto_push) which forces 'local' when auto_push is off. But solomon_fix_session spawns the runner with `--ship project_ship(repo)` directly � never effective_ship � and app.supervise() gates the fix-session on allow_pi/auto_ai_fix only, never on auto_push. So an unattended `--supervise` sweep with auto_ai_fix=True but auto_push=False will, on a gate_red_streak repo whose ship is 'pr'/'auto-merge', push a branch / open a PR / squash-merge to the integration branch � exactly what auto_push=False is supposed to forbid.
- **fix:** Thread auto_push into recover()/solomon_fix_session and pass control.effective_ship(repo, auto_push) instead of project_ship(repo), so a fix-session honors the same local-only gate as a normal run.

### PID reuse makes a dead loop look 'running' forever � permanent wedge, stale_lock never fires
- **file:** `control.py` _pid_alive() lines 548-561; is_running() 564-574
- **category:** wedge
- **problem:** _pid_alive only checks that SOME process with that PID exists, not that it is a Solomon improver. Windows recycles PIDs aggressively. If a runner dies and its PID is reused by any unrelated process (browser, a pytest python, explorer child), is_running() returns True indefinitely. Consequences: control.start() refuses with already=True and the loop is permanently 'running' but dead; the supervisor's stale_lock category requires `has_lock and not running`, so it never triggers and never clears the lock; the dashboard shows a live loop that is gone. No heartbeat-freshness cross-check guards this.
- **fix:** Cross-check liveness with heartbeat freshness (treat a present lock whose heartbeat.updated_at is older than ~3x interval as dead regardless of PID), or record process start-time/a creation token in the lock and verify it via `tasklist /v`/WMIC CreationDate before declaring alive.

### CI-red PRs are recorded as 'shipped' and never re-checked � supervisor blind, success_rate corrupted
- **file:** `improver/run_improver.py` one_iteration() line 886; _auto_merge() pending path 692-697; control.metrics()
- **category:** correctness
- **problem:** Goes deeper than the known auto-merge red-CI spin. After _ship(), one_iteration unconditionally calls _record_history("shipped", ...) regardless of the PR's eventual CI result. For ship=auto-merge with checks=='pending', native `--auto` is queued and the loop moves on; if CI later goes red, GitHub never merges and the PR sits open-red forever. Nothing re-inspects it: the supervisor's gate_red_streak only scans history.jsonl statuses, which all say 'shipped'. So a perpetually-red PR is invisible to recovery, control.metrics() counts it toward shipped/success_rate, and the operator dashboard shows a healthy success rate while PRs rot. This is the mechanism that let runtime/repo spin on PR 7 for 3h with no escalation.
- **fix:** Record history status from the ship outcome (e.g. 'shipped-ci-red'/'open-failing' when pr.checks=='failure' or state contains 'CI red'), and have the supervisor diagnose an open rsi/* PR that has been CI-red for N polls as a recoverable streak.

### Unattended supervisor auto-resets uncommitted TRACKED changes (dirty_tree is auto_safe), destroying WIP without escalation
- **file:** `improver/solomon.py` diagnose() line 78-79; _AUTO_SAFE line 22; recover() dirty_tree branch 221-230; control.reset_to_base()
- **category:** robustness
- **problem:** dirty_tree is classified auto_safe=True and is in _AUTO_SAFE, so on an unattended `--supervise` sweep recover() runs control.reset_to_base() automatically whenever the loop is not currently running. reset_to_base does `checkout --force` + `reset --hard` + `reset --hard origin/base`, which guards only UN-PUSHED base COMMITS � it does NOT guard uncommitted tracked working-tree changes. tree_dirty() now flags exactly those tracked uncommitted changes. So any uncommitted tracked edit present when the loop is stopped (a dead run's partial work, or an operator edit) is silently hard-reset away by the scheduled supervisor, with no escalation and no recovery. The deeper form of the dirty-tree wedge: recovery doesn't just unstick, it deletes WIP.
- **fix:** Make reset_to_base refuse (escalate) when `git status --porcelain --untracked-files=no` is non-empty (uncommitted tracked changes), or downgrade dirty_tree to auto_safe=False so a tree reset always requires operator confirmation.

### Deviation loop spins forever shipping unrelated PRs and never advancing the backlog
- **file:** `improver/run_improver.py` one_iteration noop/deviation handling (817-821, 883-884); _note_noop (552-563)
- **category:** wedge
- **problem:** When the agent DEVIATES (makes a real change but to something other than the named backlog item) it returns ITEM-STATUS: deviated, setting item_deviated=True. Because head_sha() != base, this is NOT a noop, so _note_noop() (the only deferral trigger) never fires. _mark_backlog_done is gated on `not item_deviated`, so the item is never ticked. The same top backlog item is therefore re-selected every iteration; each iteration ships an unrelated PR (consuming a PR, CI, and operator review) while the backlog never advances. The supervisor's gate_red_streak only catches reverted/error history, not shipped-but-deviated, so nothing escalates. This is an unbounded zero-progress spin on the intended goal.
- **fix:** Track consecutive deviations per goal (mirror _note_noop) and defer/flag the item after a small limit so the loop advances instead of re-shipping unrelated PRs under the same item name.

### Out-of-band-base-move refusal permanently wedges the loop after a successful local/push ship or a dead run leaves a committed rsi branch merged into base
- **file:** `improver/run_improver.py` one_iteration preflight out-of-band guard (734-750)
- **category:** wedge
- **problem:** The keystone refuses to hard-reset BASE_BRANCH whenever it is ahead of origin/BASE_BRANCH by any commit. This correctly blocks an operator hand-patch, but it ALSO triggers on commits the runner itself can create on base without ever pushing them: e.g. a crash between commit and push, or any future code path that fast-forwards base. Once n_ahead>0 and those commits are not on origin, EVERY iteration hits line 742 and returns status=error with zero progress � the loop is wedged until a human pushes or reverts those commits. There is no auto-recovery: status=error/phase=preflight with 'out-of-band' (not 'dirty') in the summary matches NO diagnose category (line 78 requires the word 'dirty'), so the supervisor reports `ok`/escalates nothing and the dashboard shows a healthy repo that is silently dead.
- **fix:** Add a diagnose category for status=error/phase=preflight with 'out-of-band'/'un-pushed' in the summary so the supervisor surfaces and escalates it (with copy-paste push/revert steps) instead of reporting healthy; consider distinguishing runner-created un-pushed commits from operator hand-patches.


## MEDIUM

### gate_red_streak window is fixed at exactly the last 3 history records, with no backoff � does not distinguish a transient streak from a chronic one and cannot escalate when a fix-session also fails
- **file:** `improver/solomon.py` diagnose() line 86; recover() gate_red_streak branch lines 244-256
- **category:** robustness
- **problem:** gate_red_streak triggers iff the LAST 3 history records are all reverted/error. RUNG-1 spawns one solomon_fix_session per supervise sweep with no backoff and no thrash limit: the anti-thrash block at lines 91-96 only applies when `safe` is True, but gate_red_streak sets safe=False, so it is explicitly EXEMPT from anti-thrash. A fix-session itself records history (status reverted/error if it fails its gate), so on the next sweep the streak is still present and another fix-session spawns � unbounded repeated fix-sessions on auto_ai_fix, each consuming provider budget, with no escalation to the operator after repeated failures. The known PR-7 spin is one instance; the general defect is that the only code-fix rung has no failure ceiling.
- **fix:** Count prior rung-1 solomon_fix_session records in read_supervisor_log for this category; after N (e.g. 2) failed fix-sessions, escalate to RUNG-2 instead of spawning another, mirroring the anti-thrash ceiling that auto categories get.

### _stale() returns False (never stuck) when the heartbeat has no updated_at, masking a runner that died before its first heartbeat write
- **file:** `improver/solomon.py` _stale() lines 46-54
- **category:** correctness
- **problem:** _stale returns False when updated_at is missing or unparseable. Combined with the stuck check requiring running==True (which needs a live PID in the lock), a runner that acquired the lock and then hung/died before writing any heartbeat (or wrote a heartbeat without updated_at) is never classified stuck. If the PID is dead, has_lock+not running yields stale_lock (recoverable). But if the PID is somehow alive yet wedged with no heartbeat progress, _stale=False means diagnose returns 'ok' indefinitely. The conservative False also means a corrupted/rolled-back clock or a heartbeat written with a future timestamp silently disables stuck detection.
- **fix:** When running==True but updated_at is missing/unparseable AND started_at is older than the stale threshold, treat as stuck (fail toward flagging, not toward 'ok'); use started_at as a fallback age source.

### gh pr create failure after a successful push leaks an orphan remote branch with no PR, and the item is retried
- **file:** `improver/run_improver.py` _open_pr (666-668), _ship (923-951), control.cleanup_worktrees (1174-1198)
- **category:** robustness
- **problem:** _ship pushes the branch first (git push -u origin), then calls _open_pr. If gh pr create fails (transient gh error, rate limit, branch-protection, network), _open_pr returns state='push-only' with number=None � but the branch is already on origin. _ship_succeeded returns False so the backlog item is not ticked, the next iteration re-pushes a new timestamped branch for the same item, and the orphaned remote branch is never cleaned: cleanup_worktrees only deletes LOCAL branches (git branch -D), never remote refs. Over time origin fills with rsi/iter-* branches that have no PR.
- **fix:** On _open_pr failure after a verified push, either retry pr-create or push --delete the just-pushed branch so it is not orphaned; and add remote rsi/* branch pruning (branches with no open PR) to cleanup_worktrees.

### Persistent agent deviation ships unbounded PRs while the top backlog item never advances and never escalates
- **file:** `improver/run_improver.py` one_iteration (817-821, 883-884), _note_noop (552-563)
- **category:** wedge
- **problem:** When the agent returns ITEM-STATUS: deviated, _mark_backlog_done is skipped so the top item stays unchecked and is re-selected every iteration. _note_noop (the only deferral mechanism) is called ONLY on the no-change branch (tree clean AND head==base), never when a deviation actually ships changes. So an agent that repeatedly deviates to other changes while shipping a PR each time produces a stream of PRs that all 'ship' (status='shipped'), the same top item is retried indefinitely, and nothing defers it or escalates (gate_red_streak only sees reverted/error). The backlog is effectively wedged on item #1.
- **fix:** Track consecutive item_deviated ships per goal (mirroring _noop_counts) and call _defer_backlog_item after a limit so the loop advances; or escalate via a diagnose category when the same top item deviates N times.

### auto-merge 'pending' with native --auto unavailable leaves a never-merging PR but still ticks the item
- **file:** `improver/run_improver.py` _auto_merge (691-697), one_iteration (883-886)
- **category:** invariant-violation
- **problem:** In _auto_merge, when checks=='pending' and `gh pr merge --auto` fails (repo has auto-merge disabled / no branch protection), it returns state='open (awaiting CI)' with the number retained. one_iteration then ticks the backlog (number present) and records 'shipped'. No native auto-merge was actually queued, so the PR will NEVER merge automatically � it sits open with pending/eventually-green CI that nobody acts on, while the item is already consumed. Same leak class as the red-CI case but on the pending path.
- **fix:** Null the number (or set a non-shipped state) when native auto-merge could not be queued so the item is not ticked; surface these stuck PRs to the supervisor for escalation.

### Push 'verified=False' (rc=0 but branch not on origin) does not block PR-open or escalate
- **file:** `improver/run_improver.py` _ship (928-945)
- **category:** correctness
- **problem:** After git push returns rc=0, _branch_on_remote re-checks origin. If the branch is NOT visible (push silently rejected by a hook/proxy that still returned 0), the code only logs a WARNING and proceeds to _open_pr anyway. SOLOMON_RSI.md step 7 requires verifying the branch actually landed before opening the PR; here the verification result is recorded but never gates behavior. The subsequent gh pr create will usually fail (no remote head) and fall to 'push-only', but the failure is reported as a benign warning, not an error/escalation, so an operator sees a healthy-looking 'shipped' state with no PR.
- **fix:** When SHIP in (pr, auto-merge) and verified is False, return a 'push-unverified' failure state (do not open a PR, do not tick the item) and set heartbeat status='error' so the supervisor can surface it.

### Fixed-name temp files for heartbeat and lock collide between two processes sharing a runtime dir
- **file:** `improver/run_improver.py` heartbeat (run_improver.py:274-283); acquire_lock (run_improver.py:982-988)
- **category:** race
- **problem:** heartbeat() writes HEARTBEAT.with_suffix('.tmp') then os.replace, and acquire_lock writes RUNTIME/'lock.tmp' then os.replace. Both temp paths are FIXED (one per runtime dir), not per-process. Whenever two processes target the same runtime/<name> dir � the known double-runner from the acquire_lock TOCTOU, or a loop running while the supervisor/another --start writes � their tmp writes interleave on the same file: process B truncates/writes heartbeat.tmp while A is mid-write, and whichever os.replace runs second wins, so a heartbeat can be promoted from a half-written/foreign tmp, or a lock.tmp can be replaced with the wrong PID. os.replace is atomic for the destination but the shared SOURCE tmp file is the race. Reading code (read_heartbeat) then json.loads a possibly-corrupt-but-now-promoted file (it tolerates corruption by returning None, but a structurally-valid file written by the wrong process is silently trusted).
- **fix:** Use a unique temp suffix per writer (e.g. include os.getpid()/a uuid in the tmp filename) before os.replace, so concurrent writers never share the same source temp file.

### auto-merge proceeds even when STOP arrived during CI polling � Stop does not prevent the merge
- **file:** `improver/run_improver.py` _await_pr_checks (run_improver.py:633-648); _ship auto-merge tail (run_improver.py:947-950)
- **category:** correctness
- **problem:** The spec halt-switch says a mid-iteration Stop keeps the gate-green branch locally but does NOT ship it; one_iteration() honors this with a STOP.exists() check before _ship. But on ship=auto-merge the merge happens INSIDE _ship via _await_pr_checks (which can sleep up to attempts*delay ~48s polling CI) followed immediately by _auto_merge. _await_pr_checks does check STOP.exists() to break its polling loop early � but breaking early just returns the last checks value, and _ship then calls _auto_merge unconditionally, which squash-merges and deletes the branch. So an operator Stop issued after the PR was opened (during the CI-poll window) does not stop the merge: the iteration ships and merges to the integration branch despite the Stop. The pre-_ship STOP gate cannot cover this because the PR/merge all happen after it, within _ship.
- **fix:** Re-check STOP.exists() in _ship immediately before _auto_merge (and ideally before _open_pr) on the auto-merge path; if a stop is pending, leave the PR open/unmerged and report a stopped state instead of merging.

### backlog.md is updated by the loop with no coordination against operator edits via write_contract
- **file:** `improver/run_improver.py` _mark_backlog_done/_defer_backlog_item/ideate (run_improver.py:528-587,1107-1120); write_contract (control.py:864-875)
- **category:** race
- **problem:** The runner mutates improver/<name>/backlog.md by full-file read-modify-write in _mark_backlog_done and _defer_backlog_item, and ideate() does the same (read existing, prepend, write whole file). Concurrently, control.write_contract(repo,'backlog',text) (exposed to the dashboard via app.Api.write_contract) overwrites the same file wholesale when an operator edits the backlog in the UI, and control.ideate() can be operator-triggered while a loop iteration is finishing. All are last-writer-wins full-file overwrites with no lock or atomic replace. Interleavings: operator saves an edited backlog at the same moment the loop ticks an item -> one of the two writes is lost (operator's edits silently reverted, or the tick lost so the item re-ships next iteration). Because these are plain write_text/json on a shared path, a write can also be observed half-truncated by a concurrent reader (_top_backlog_item).
- **fix:** Serialize backlog writes (e.g. take the runtime lock, or write via tmp+os.replace and refuse operator edits while a loop holds the lock), or at minimum write atomically and disable the UI backlog-save while the loop is running for that repo.

### diagnose() reads heartbeat and lock separately, can classify a just-restarted loop as stale_lock and clear a live lock
- **file:** `improver/solomon.py` diagnose (solomon.py:57-98); recover stale_lock (solomon.py:209-213)
- **category:** race
- **problem:** diagnose() computes has_lock = os.path.exists(lock) and running = is_running(repo) (which reads the lock PID) as two separate filesystem reads, then classifies 'stale_lock' when has_lock and not running. During the brief window in which a freshly-spawned runner has created the lock file via open(LOCK,'x') but has not yet had its PID become visible to tasklist (process just started / lock written but PID not yet in tasklist's snapshot), is_running can return False while the lock exists -> diagnose returns stale_lock, and the unattended --supervise sweep's RUNG-0 immediately calls control.clear_lock(repo). clear_lock re-reads the PID and (because the new runner's PID may not yet register, or because of the PID-reuse issue) can remove the LIVE runner's lock, after which a second --start or the racing acquire_lock can launch a duplicate runner. The diagnose read and the recover action are also separated in time (TOCTOU between diagnose() and recover()).
- **fix:** Require corroborating evidence of staleness before clearing (e.g. heartbeat status==stopped AND lock older than a threshold AND PID confirmed dead), and re-validate liveness inside clear_lock atomically rather than acting on a diagnose() snapshot taken earlier.

### Wedge detection only covers phase=='implement'; a runner frozen in any other phase (e.g. hung git push/fetch) is reported healthy
- **file:** `improver/solomon.py` diagnose() line 84 (stuck branch); run_improver.git() lines 301-303 (no timeout)
- **category:** wedge
- **problem:** The only stale-heartbeat detection is `running and phase == 'implement' and _stale(hb)`. But git() in the runner shells out with NO timeout, so a network-stalled `git fetch origin` (preflight) or `git push -u origin` (ship) blocks the runner indefinitely with the heartbeat frozen at phase 'preflight'/'ship'/'pr'/'merge'. In those phases diagnose() matches no branch (status is not 'error', has_lock&&running is true so stale_lock/stop_lingering don't apply, phase!='implement' so 'stuck' doesn't apply) and returns cat='ok'/healthy. A real hang during shipping therefore never triggers stop+restart or escalation. The pi-implement phase is the one place that is ALREADY bounded by run_pi's 1800s timeout, so the guarded phase is the one least likely to wedge, while the unbounded-git phases are unguarded.
- **fix:** Generalize the stale check to `running and _stale(hb) and phase not in (None,'sleep')` (any active phase), and/or add a timeout to the runner's git() calls (esp. fetch/push) so a network stall self-aborts instead of freezing the heartbeat.

### Unattended --supervise sweep hard-resets operator tracked WIP in a managed repo without confirmation
- **file:** `improver/solomon.py` diagnose() dirty_tree line 78-79 (auto_safe=True), _AUTO_SAFE line 22, recover() lines 221-230; control.reset_to_base lines 1247-1254
- **category:** robustness
- **problem:** dirty_tree is classified auto_safe=True and is in _AUTO_SAFE, so the unattended `--supervise` path (app.py --supervise -> recover with no operator gate for RUNG-0) will, once the loop is not running, call reset_to_base() which executes `git checkout --force base; git reset --hard; git reset --hard origin/base`. tree_dirty() counts only TRACKED uncommitted changes, i.e. genuine operator work-in-progress in the managed repo. reset_to_base guards only against un-PUSHED base COMMITS, not uncommitted tracked edits, so those edits are silently destroyed by an automated sweep. This is destructive auto-recovery on operator data, contrary to the spec's 'reversible-first, never discards' framing for RUNG-0.
- **fix:** Make dirty_tree auto_safe=False (escalate with copy-paste steps) so destroying tracked uncommitted changes always requires explicit operator action, or have reset_to_base stash/refuse when `git status --porcelain --untracked-files=no` is non-empty.

### Anti-gaming skip/xfail detection misses common test-weakening forms; pass-count rail silently inactive for custom gates
- **file:** `improver/run_improver.py` _new_skip_markers() lines 481-486; _anti_gaming_reason() lines 489-499; one_iteration() lines 780-783
- **category:** invariant-violation
- **problem:** The gate-enforced-by-runner / 'gate not stubbed' anti-gaming claim is only partially enforced. _new_skip_markers requires a leading '@' DECORATOR matching pytest.mark.(skip|xfail)/unittest.skip, so it misses module-level `pytestmark = pytest.mark.skip`, in-body `pytest.skip()/pytest.xfail()`, `@unittest.skipIf/skipUnless`, and conftest `collect_ignore`/autouse fixtures that disable tests. The only other rail is the pass-count drop, which the code itself warns is INACTIVE for a custom GATE_CMD that prints no parseable 'N passed' (base 0 vs after 0 is a no-op). So with a custom gate, an agent can weaken/disable tests via conftest or non-decorator skips and still 'pass' the gate with both anti-gaming rails inert.
- **fix:** Broaden the diff scan to also flag added pytest.skip(/pytest.xfail(/pytestmark = .../skipIf/skipUnless/collect_ignore and edits to conftest.py/pytest.ini addopts; and treat a non-parseable custom-gate count as a hard requirement (fail provisioning unless the gate emits a count) rather than only logging a warning.

### unittest count math counts skipped tests as passed, poisoning the anti-gaming baseline
- **file:** `improver/run_improver.py` run_gate line 469-475
- **category:** correctness
- **problem:** For unittest gates, `passed = max(Ran - failures - errors, 0)`. unittest reports skips separately (`OK (skipped=3)` / `FAILED (failures=1, skipped=2)`) and they are NOT subtracted, so skipped tests are counted as passed, inflating the baseline pass count. Two consequences: (1) the heartbeat/PR 'N passed' is wrong; (2) more importantly the baseline `passed` is inflated, so a later change that legitimately reduces skips-counted-as-passes can trip a false anti-gaming revert, and a change that converts real passing tests into skips keeps the inflated count flat � masking the very weakening the rail exists to catch.
- **fix:** Also parse `skipped=(\d+)` and subtract it (track skipped separately): `passed = max(ran - failed - errors - skipped, 0)` and surface skipped in the tests dict.

### _detect_stack picks pytest for manifest repos that only have a unittest tests/ dir, wedging the gate
- **file:** `control.py` _detect_stack / _py_test_cmd line 897-907
- **category:** robustness
- **problem:** When a repo has a manifest (pyproject.toml/requirements.txt/setup.py) the `has_manifest` branch runs `_py_test_cmd()` FIRST, which returns `python -m pytest` whenever `pytest_cfg` is false but a `tests/` dir exists (`pytest_cfg or not isdir('tests')` is False, so it falls through to the pytest line only when pytest_cfg is true; when pytest_cfg is false AND tests/ exists it returns unittest discover � but the venv+tests fallback at line 914 is never reached because has_manifest already matched). The real hazard: a manifest-bearing repo whose tests live OUTSIDE a top-level `tests/` dir (e.g. `src/pkg/tests/` or `test/`) gets `python -m pytest` with no config; if the project actually needs `unittest discover -s test` or a specific rootdir, every iteration's auto-set gate errors and the loop reverts forever (ensure_contracts auto-sets this gate at line 1053-1056 without ever verifying it runs green).
- **fix:** Have ensure_contracts run the auto-detected gate once on the base and only adopt it if it exits cleanly / is parseable; otherwise leave the gate unset and surface a 'set a gate' error rather than auto-wedging the loop on a wrong command.

### ensure_contracts auto-sets a gate command that is never validated to run green
- **file:** `control.py` ensure_contracts line 1052-1056
- **category:** wedge
- **problem:** On start, when the operator hasn't set a gate, ensure_contracts writes the stack-detected `test_cmd` into repos.json as the repo's gate with no check that it actually runs (let alone runs green) on the base. If detection is wrong (wrong rootdir, missing dev deps, non-pytest layout, a venv python that can't import the project), every iteration's base-gate measurement is RED, run_improver aborts each iteration with 'Base gate is RED before any change', and the loop spins producing zero PRs indefinitely. The base-red path (run_improver line 769-776) correctly skips, but nothing escalates a persistently-red base that is caused by a mis-detected auto-gate � solomon.diagnose only catches reverted/error HISTORY, and base-red aborts write status=error+phase=preflight which maps to no specific recovery category.
- **fix:** Smoke-run the detected gate once (capture returncode) before persisting it, and only set it if it executes; on persistent base-red add a diagnose category so the supervisor escalates instead of silently spinning.

### set_repo_config writes repos.json non-atomically � a torn read silently drops the ENTIRE registry config
- **file:** `control.py` set_repo_config() lines 242-247
- **category:** robustness
- **problem:** set_repo_config does open(REPOS_JSON,'w') then json.dump with no temp-file+rename. pywebview js_api callbacks run on separate threads, and add_project triggers set_repo_config(goal) -> enrich_contract -> ensure_contracts -> set_repo_config(gate) which can interleave with a UI set_repo_config or a concurrent get_state read. A crash or concurrent read mid-write yields a truncated file; _read_repos_json catches json.JSONDecodeError and returns [], so EVERY per-repo override (provider, model, ship, gate, goal, pr_target_branch) silently vanishes and all repos fall back to defaults � e.g. a unittest repo reverts to the built-in pytest gate and reds out. The heartbeat write uses tmp+os.replace; this registry write does not.
- **fix:** Write to a temp file in the same dir and os.replace() onto REPOS_JSON (mirroring heartbeat()), so a reader/crash never sees a partial file.

### First Start after onboarding spawns the loop with the OLD (wrong) gate even though ensure_contracts just detected the right one
- **file:** `control.py` start() lines 596 + 616; ensure_contracts() 1053-1056
- **category:** correctness
- **problem:** start() calls ensure_contracts(repo) (line 596) which, when no gate is set, runs set_repo_config(name, gate=detected) and persists the auto-detected gate to repos.json. But the local `repo` dict passed to start() is not refreshed, so the subprocess args at line 616 use `project_gate(repo)` from the STALE dict � still '' � and the runner is spawned with the built-in pytest gate. For a unittest-only repo (e.g. a freshly added sover-like project with no manifest), this first run hits a RED base gate every iteration and aborts (preflight 'Base gate is RED') until the operator stops and restarts. The detected gate only takes effect on the NEXT launch.
- **fix:** After ensure_contracts(repo), re-read the repo (e.g. self._repo(name) / load_repos lookup) before building args, or have ensure_contracts return the detected gate and use it directly when project_gate(repo) is empty.

### Bare provider key (OLLAMA_API_KEY value) is not redacted from agent output
- **file:** `improver/run_improver.py` _SECRET_TOKEN_PATTERNS / _redact() lines 182-213
- **category:** security
- **problem:** The pi agent runs with the active provider's key in its env (OLLAMA_API_KEY or OPENROUTER_API_KEY) and can read a committed .env. _redact catches gh*, github_pat_, sk-* (OpenRouter is sk-or-... so it's covered), and Bearer tokens, plus NAME=value where NAME matches *API_KEY etc. But a BARE Ollama key value echoed by the model without a NAME= prefix matches none of the token patterns (Ollama keys are not sk-/gh-shaped), so it would pass through into a commit body, PR body, history.jsonl, or log. The redactor's own docstring claims defense for 'its own key' but the most likely leaked key (Ollama) is not pattern-covered.
- **fix:** Add an exact-value redaction pass: substitute the actual loaded OLLAMA_API_KEY/OPENROUTER_API_KEY string (from os.environ) with [REDACTED] in _redact, so the agent's own key is scrubbed regardless of shape.

### Unchecked non-force `git checkout BASE_BRANCH` after ship can strand the runner on the rsi branch and mis-tick the backlog
- **file:** `improver/run_improver.py` one_iteration post-gate paths (855, 868, 878)
- **category:** robustness
- **problem:** Three post-gate code paths run plain `git("checkout", BASE_BRANCH)` (no --force) and never check the return code. If the checkout fails (e.g. a gate run left an untracked-but-now-tracked conflict, a sparse-checkout edge, or a Windows file lock on a file that differs between branches), HEAD stays on the rsi branch. At line 878 the code then proceeds to _mark_backlog_done and writes status=sleeping as if the base were clean. The next preflight's `checkout --force` usually self-heals, but in the window the heartbeat misreports success while HEAD is on a feature branch, and an interleaved supervisor/diagnose call reads inconsistent state. The stop path (868) and rev-list-failed path (855) have the same unchecked-checkout gap.
- **fix:** Check the checkout return code (use --force) and, on failure, escalate to status=error rather than continuing to tick the backlog and report sleeping.

### `git clean -fd` in preflight silently deletes operator-created untracked files every iteration
- **file:** `improver/run_improver.py` one_iteration preflight (732); tree_dirty (326-330)
- **category:** correctness
- **problem:** tree_dirty() deliberately ignores untracked files, and preflight unconditionally runs `git clean -fd` which deletes ALL non-ignored untracked files. The justification (leftovers from a dropped iteration) is sound for runner-created files, but `git clean -fd` cannot distinguish a dropped-iteration leftover from a NEW untracked file an operator just added (e.g. a new test file, a scratch doc, a not-yet-`git add`ed module). On a continuously running loop this destroys that work on the very next iteration with no warning and no recovery � a data-loss path that contradicts the 'must not clobber operator work' intent stated for tracked files. The never-hand-patched guard protects committed base commits but this path silently bulldozes uncommitted operator files.
- **fix:** Restrict clean to the rsi branch's own added files (e.g. `git clean -fd` only paths under the iteration's diff), or count untracked files in the dirty check and skip+escalate rather than deleting, so a stray operator file can't be destroyed unannounced.


## LOW

### stop_lingering can never reach its recovery and a stale stop sentinel can silently block a fresh loop start without diagnosis
- **file:** `improver/solomon.py` diagnose() line 82; recover() stop_lingering lines 214-220; run_improver main() lines 1242-1243/1268-1271
- **category:** robustness
- **problem:** stop_lingering requires `has_stop and not running`. The runner removes STOP on clean exit (finally, line 1269) and on start (line 1243), so the sentinel only lingers if the process was killed mid-iteration without its finally running. That is exactly when the lock also lingers � and the stale_lock branch (line 80) is checked BEFORE stop_lingering (line 82), so a crashed runner is diagnosed stale_lock, never stop_lingering. recover() for stale_lock clears only the lock, leaving the stop sentinel; the next start() does clear it, but the unattended supervisor will keep re-diagnosing stop_lingering on the following sweep and clearing it as a separate cycle. The category is largely dead code, and a leftover stop file is handled only incidentally rather than as a first-class recovery.
- **fix:** Have stale_lock recovery also remove a co-resident stop sentinel (a crashed run leaves both), or check stop_lingering before stale_lock; otherwise document that stop_lingering only covers the lock-already-released case.

### Preflight increments the iteration counter before early-return failures, burning --max-iterations on no-op wedges
- **file:** `improver/run_improver.py` one_iteration (707-776), main max-iterations check (1256-1258)
- **category:** robustness
- **problem:** _hb['iteration'] += 1 executes at the very top of one_iteration, before every early return (dirty tree, checkout failure, un-pushed-base refusal, base-gate RED). A repo stuck on a transiently red base or a dirty tree increments the counter each loop and, with --max-iterations set, exits having done zero real work � silently giving up under a budget meant for real iterations. It also inflates the 'iterations' metric with no-op preflight bails.
- **fix:** Only increment the iteration counter once the iteration actually reaches the implement phase (after preflight succeeds), or track preflight-bail iterations separately so they don't consume the budget.

### _parse_ideas leverage regex matches only a single digit, silently dropping valid lines
- **file:** `improver/run_improver.py` _parse_ideas line 1080-1085
- **category:** correctness
- **problem:** The regex captures leverage as `(\d)` � exactly one digit. ideate.md specifies leverage 1�5, so single digits are the norm, but any line where the model emits a two-digit value (e.g. '10'), a decimal ('4.5'), or pads it ('05') fails the whole-line match and the idea is silently discarded with no log � a parser dropping data. Combined with the leverage being used only for sort order, a malformed leverage shouldn't nuke an otherwise-valid ambitious idea. ideate() reports 'emitted no parseable ideas' only if ALL lines fail, so partial silent drops are invisible.
- **fix:** Use `(\d+)` (clamp to 1-5) for leverage so multi-digit values parse, and log a count of unparseable idea lines so silent drops are visible.

### _parse_provision strips backticks too aggressively, can corrupt contract bodies
- **file:** `improver/run_improver.py` _parse_provision line 1030-1038
- **category:** correctness
- **problem:** The parser does `.strip().strip('`').strip()` on each block. `str.strip('`')` removes ALL leading/trailing backtick characters, not just a fenced ```` ``` ```` wrapper. A legitimate AGENT.md whose first content line is an inline-code span like `` `pytest -q` `` (very plausible for the gate/command section the contract is supposed to name) has its leading backtick(s) stripped, silently mangling the written contract. Likewise the backlog `.split('===')[0]` truncates at the first literal `===` that could appear inside a generated horizontal-rule or table, dropping the remainder of the backlog.
- **fix:** Only strip a complete leading/trailing ```` ``` ```` fence (regex-match the fence) rather than `strip('`')`, and split the backlog block on the explicit closing delimiter rather than any `===` substring.

### Base-gate-red SOLOMON fix-session has its post-change pass-count compared to an inflated/red baseline
- **file:** `improver/run_improver.py` one_iteration line 762-783, 839
- **category:** correctness
- **problem:** For a SOLOMON fix-session the base gate is RED by definition, so base_tests captures the failing run's counts (e.g. passed=70, failed=5). The fix makes the gate green (passed=75). _anti_gaming_reason compares post passed(75) >= base passed(70): fine. But if the fix legitimately quarantines one genuinely-broken test by deleting it while fixing the rest, post passed could equal base passed and the change is reverted as 'gamed' even though it is a correct recovery � the recovery ladder's one sanctioned code path can be blocked by its own anti-gaming rule because the baseline was measured on a red tree where the pass count is not a stable floor.
- **fix:** For SOLOMON sessions, gate anti-gaming on the green base from before the streak (or relax the pass-count floor to require only that failed/errors reach 0), since a red baseline's pass count is not a valid 'did not drop' reference.

### Running loop never re-reads repos.json � dashboard shows new goal/ship/reasoning while the loop runs the stale ones
- **file:** `improver/run_improver.py` main() lines 1181-1262 (config captured once into globals; loop never re-reads)
- **category:** robustness
- **problem:** All per-run config (goal, ship, gate, reasoning, model, interval, max_iterations, pr_target_branch) is captured once from argv into module globals at launch and never re-read inside the while-loop. control.set_repo_config edits repos.json but sends no signal to the live process. So editing a repo's Goal/Ship/Reasoning in the dashboard has zero effect on the in-flight loop until stop+start, while get_state() reads repos.json fresh and shows the NEW values � the operator sees a divergence (dashboard says goal X, loop is steering toward goal Y) with no indication the change isn't live. A foot-gun for 'I changed the goal but nothing changed.'
- **fix:** Either re-read the repo's config at the top of each iteration (cheap, from repos.json by name), or surface a 'config changed � restart to apply' flag in the heartbeat/dashboard so the divergence is visible.

### reset_to_base / preflight un-pushed guard uses different commit-count semantics, letting a base with a merge of unrelated history slip through
- **file:** `control.py` reset_to_base (1243-1246) vs one_iteration preflight (740-741)
- **category:** correctness
- **problem:** The runner's preflight counts `rev-list --count origin/BASE..BASE` (any ahead commit refuses). reset_to_base instead checks `log --oneline origin/BASE..BASE` non-empty. These usually agree, but reset_to_base runs `git fetch` only AFTER the ahead-check (line 1253), so on first invocation origin/BASE may be a stale ref: a base that was actually fast-forwarded to match a freshly-pushed origin can still show as 'ahead' against the stale local origin ref, causing reset_to_base to refuse with 'un-pushed commits' and escalate spuriously � or, conversely, miss genuinely un-pushed commits if origin/BASE is stale-ahead. The runner's preflight correctly fetches BEFORE counting (line 735); reset_to_base does not, so the supervisor's recovery and the runner's guard can disagree on whether the base is safe to reset.
- **fix:** Run `git fetch origin` before the un-pushed-commit check in reset_to_base, matching the runner's preflight ordering, so the guard compares against current origin truth.


---

## Remediation status (2026-06-18 overnight session)

**Fixed + tested this session (committed to Solomon; 134 tests green):**
- Lock TOCTOU double-runner; release_lock steal; Start revokes live Stop.
- reset_to_base destroying uncommitted operator WIP (watchdog-safety keystone).
- Dirty-tree wedge: skip only on the BASE branch; auto-heal dead-run rsi/* litter.
- Ship-accounting: tick only a LANDED ship; auto-merge CI-red records 'blocked' (not 'shipped'),
  ship=push ticks on a verified push; STOP during the CI poll no longer merges an un-CI'd PR.
- Anti-gaming: stage before the diff (catch skips in NEW untracked tests); collected-count rail;
  broadened skip-form detection.
- Wedge/hang detection: gate timeout; 'stuck' covers any active phase; 'base_out_of_band' diagnosis;
  deviation deferral; revert-failure HALT; watchdog leaves 'error' states for the supervisor.
- Supervisor: ci_red_streak escalation; fix-session honors auto_push (effective_ship).
- Robustness: atomic repos.json write; pytest scoped to tests/; gitignore relocated repo clones.
- New: overnight watchdog (monitor.py + SolomonWatchdog scheduled task) restarts crashed loops.

**Deferred (documented for a follow-up session — higher blast-radius / lower urgency):**
- **PID-reuse identity token** (HIGH): `_pid_alive` only checks PID existence; a reused PID can pin a
  dead loop "running" forever or let `clear_lock` remove a fresh lock. Robust fix needs a run-id token
  (PID + creation-time, or a heartbeat run-id) cross-checked in is_running/clear_lock/acquire_lock —
  invasive; partial fixes risk new wedges, so left whole.
- **Supervisor holds the runner's single-flight lock during git mutation** (HIGH): recover() guards
  with a non-atomic `is_running()` check rather than acquiring the lock; a race with a starting
  iteration is possible. Needs the supervisor to acquire/release runtime/<name>/lock around recovery.
- **`git clean -fd` in preflight deletes operator UNTRACKED files on base** (MEDIUM): intentional
  dropped-iteration cleanup, but it can eat operator scratch files. Scope it to rsi/* branches or
  skip+escalate when untracked non-ignored files look operator-authored.
- **gate_red_streak anti-thrash ceiling** (MEDIUM): a fix-session can respawn each sweep under
  auto_ai_fix+unattended (the unattended watchdog uses allow_pi=False, so it escalates — not affected).
- **push-unverified should block PR-open / surface an error** (MEDIUM); **first-Start stale-dict gate**
  (MEDIUM, config drift); **bare loaded-key redaction + parser nits** (LOW).


---

## Overnight dogfood session (2026-06-18 — operator run + hardening)

Operating the three loops as a user while hardening Solomon. **Shipped to `main` this session
(full suite 151 passed):**
- `887fb2e` runner: adopt the existing PR on a `gh pr create … already exists` (the sover dup-PR
  spin); re-read repos.json each iteration so a dashboard model/gate/goal edit applies without a
  restart; flag narrated-but-unwritten no-ops.
- `6fc53e9` lock/PID: recycled-PID-proof liveness — the lock carries `<pid>\n<run_id>`, and
  `is_running`/`clear_lock`/`acquire_lock` corroborate a live PID with run-id match + heartbeat
  freshness (window ≥ the longest agent session). Fixes the watchdog `clear_lock`/`stale_lock` churn.
- `fdba8ed` supervisor: hold the runner's single-flight lock during `reset_to_base` recovery
  (`acquire_supervisor_lock`/`release_supervisor_lock`), replacing the racy `is_running()` snapshot.

All three loops relaunched on the new runner; **asmodeus moved kimi → glm-5.2** (the stale-config
bug) and now writes real code instead of hallucinating it.

### LIVE ISSUE — maki wedged on a conflicted in-progress merge (OPERATOR ACTION REQUIRED)
`workspace/projects/maki` has `main` **1 commit ahead of origin** (`33d3239 test: guard in-app reader
wiring`, 00:27) **and an in-progress conflicted merge** (unmerged paths: backend/assistant.py,
convert.py, job_runner.py, jobstore.py, settings.py). The loop correctly REFUSES every iteration
(never-hand-patched keystone) → wedged at `status=error/preflight`. This predates the session — a
manual or agent-run `git merge` (PR #19 "Needle-movers" is the conflicting change). NOT auto-fixed
(it may be operator WIP; resolving it would touch a managed repo's tree). **Safe operator options**
(in `workspace/projects/maki`): `git merge --abort` then `git push origin main` if `33d3239` is
wanted; OR `git reset --hard origin/main` to discard both (destructive). The loop self-resumes once
`main == origin` and the tree is clean.

### ROOT CAUSE — agents run git/gh directly despite the explicit contract (recommended fix)
`improver/sover/AGENT.md:35` says *"Do NOT run git or gh directly"*, yet glm-5.2 (xhigh) creates its
OWN branches (`rsi/milestone-scoring`, `rsi/chat-scaffold-vetted-templates`) and opens PRs itself
(#18/#19/#20/#23/#24). This breaks the runner's branch-per-iteration model — dup PRs accumulate, the
item never ticks (push-only), and a self-run `git merge` is the likely cause of maki's conflicted
main. Contract text is insufficient for a capable agent. **Recommended robust fix (deferred — high
blast-radius, needs careful live testing): strip `git`+`gh` from the AGENT subprocess PATH in
`run_pi()`** (prepend a dir of refusing shims) while keeping the explicit `.venv\Scripts\python` gate
runnable; the runner's own git/gh use the real PATH and are unaffected. Wave-1 Item 1 only covers a
PR opened on the runner's *current* branch; the agent's-own-branch case still needs this.

---

## Overnight continuation session (2026-06-18 — operator run + hardening, cont'd)

Continued the overnight campaign. All three loops verified live and producing real, test-green work
on glm-5.2. **Shipped to `main` this session (full suite 165 passed, up from 160):**
- `459a32d` preflight: guard `git clean -fd` from deleting operator UNTRACKED files — the blanket
  clean on the base branch silently destroyed operator scratch files every iteration (a data-loss
  path the never-discard-operator-work keystone covered for TRACKED files but not untracked). The
  guard checks `git status --porcelain --untracked-files=normal` for `??` entries and skip+escalates
  if any are present; only cleans when there is nothing to destroy.
- `f82a23e` control: (1) `reset_to_base` now fetches origin BEFORE the un-pushed-commit check so
  `origin/{base}` is current truth (matching the runner's preflight ordering — a stale local origin
  ref could spuriously refuse or miss un-pushed commits); (2) `start()` re-reads the live repo dict
  from `load_repos()` after `ensure_contracts` auto-sets a detected gate, so the first launch uses
  the right gate instead of the stale `''` (which spawned the runner with the built-in pytest gate
  and reded out every iteration on a unittest-only project until a manual restart).

### Loop progress this session
- **maki**: PR #43 merged (353 tests, atomic search-history write). Runner crashed after iter 2
  (no clean-exit log line; both PIDs gone). Restarted on the new code.
- **sover**: PR #29 merged (215 tests, LLM-driven render/measure/monetize lanes) + PR #30 merged
  (224 tests, **beyond-tree self-expansion** — a north-star milestone: the profile no longer stops
  growing when the fixed 8-node tree is complete; it authors NEW capabilities from vetted templates).
  Now iter 6 on a counterfactual code-mod simulator.
- **asmodeus**: 2 local ships (360 tests each — auto-breaker wired into paper mode + venue
  bracket-fill PnL). Now iter 3 on an `asmodeus status` CLI subcommand.

### Remaining open items (lower priority)
- **gate_red_streak anti-thrash ceiling** (MEDIUM): a fix-session can respawn each sweep under
  auto_ai_fix. The watchdog uses allow_pi=False, so this only bites under manual auto_ai_fix.
- **_parse_provision backtick stripping** (LOW): can corrupt contract bodies with inline-code spans.
- **Preflight increments iteration counter before early-return** (LOW): burns --max-iterations on
  no-op wedges.
- **SOLOMON fix-session anti-gaming baseline** (LOW): red baseline comparison can block a legitimate
  recovery that quarantines a broken test.
