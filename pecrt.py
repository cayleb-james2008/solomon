"""PECRT Layer 0 — decision-identical Python mirror of src-tauri/src/pecrt/.

This module is the Python side of the dual implementation mandated by the continuous-reasoning-thread
architecture doctrine, following the established control.py <-> src-tauri/src/control/ bug-for-bug
port pattern (documented in src-tauri/src/control/contracts.rs L1-7 and control-port-spec.json).

It is DECISION-IDENTICAL to the Rust crate: the SAME bounds, the SAME event schema, the SAME
deny/allow verdicts, the SAME stable-prefix bytes, the SAME freshness OR-rule, the SAME
summary-of-summary rejection. Where a decision is load-bearing, the Rust doc-comment and this
docstring state it identically; the Rust test `stable_prefix_is_frozen` and this module's
`STABLE_PREFIX` pin the shared constant so the two can never drift silently.

Three parts (mirroring the three Rust files):
    * bus  — the shared wake-bus: WakeSource taxonomy (superset of improver::park), FreshnessEvent
             (the standardized freshness-probe JSON), and next_wake() (pure poll decision).
    * warm — three-tier warm context: WorkingTier (hard-bounded), ObservationLog (append-only dated
             FACTS, rejects summary-of-summaries), LongTermAdapter (read-only over existing ledgers),
             and reconstruct_context() behind a STABLE prompt prefix for provider cache hits.
    * safety — the HARD invariant: the thread is a scheduler+memory WRAPPER, never a new authority.
             classify_schedule() DENIES any attempt to touch the whitelist/tiers, cycle_budget, or the
             skeptic/kill/blast-radius/freshness gates; every allowed action re-enters the existing
             gate funnel unchanged.

Run `python pecrt.py` to execute the self-check (mirrors the Rust #[test] suite).
"""

from __future__ import annotations

import json
import os
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Optional

# =========================================================================== #
# bus — shared wake-bus
# =========================================================================== #


class WakeSource(Enum):
    """What ended (or would end) a wait on the bus. A strict SUPERSET of
    improver::park::WakeSource — the first five tags are byte-identical to park's taxonomy."""

    KILL = "kill"
    FRESH_DATA = "fresh_data"
    OPS_CHANGE = "ops_change"
    PROVIDER_RECOVERY = "provider_recovery"
    FLOOR_ELAPSED = "floor_elapsed"
    FILE_APPEND = "file_append"
    SQLITE_ROW = "sqlite_row"

    @property
    def tag(self) -> str:
        return self.value

    @property
    def priority(self) -> int:
        """Lower wins when two sources are simultaneously ready. KILL is always 0; FLOOR last."""
        return {
            WakeSource.KILL: 0,
            WakeSource.FRESH_DATA: 1,
            WakeSource.OPS_CHANGE: 2,
            WakeSource.PROVIDER_RECOVERY: 3,
            WakeSource.FILE_APPEND: 4,
            WakeSource.SQLITE_ROW: 5,
            WakeSource.FLOOR_ELAPSED: 6,
        }[self]


@dataclass
class FreshnessEvent:
    """The standardized freshness EVENT — the exact JSON the freshness probe emits on its last
    stdout line (improver::freshness L18-19): {metric_id, latest_ts, n_samples, observable}."""

    metric_id: str
    latest_ts: float
    n_samples: int
    observable: bool

    @staticmethod
    def from_dict(v: dict) -> Optional["FreshnessEvent"]:
        metric_id = v.get("metric_id")
        latest_ts = v.get("latest_ts")
        n_samples = v.get("n_samples")
        if metric_id is None and latest_ts is None and n_samples is None:
            return None
        return FreshnessEvent(
            metric_id=metric_id if isinstance(metric_id, str) else "",
            latest_ts=float(latest_ts) if isinstance(latest_ts, (int, float)) else 0.0,
            n_samples=int(n_samples) if isinstance(n_samples, int) else 0,
            observable=v.get("observable") is True,
        )

    def advanced(self, before: Optional["FreshnessEvent"]) -> bool:
        """SAME OR-rule as park.has_fresh_data (more samples OR newer ts) + observability guard.
        An unobservable current reading is NEVER an advance; a missing baseline never fires."""
        if not self.observable:
            return False
        if before is None:
            return False
        return self.n_samples > before.n_samples or self.latest_ts > before.latest_ts


@dataclass(frozen=True)
class WatchSource:
    source: WakeSource
    fired: bool


@dataclass(frozen=True)
class WakeReason:
    source: WakeSource
    why: str


def _fired_why(source: WakeSource) -> str:
    return {
        WakeSource.KILL: "operator KILL/Stop — end wait now",
        WakeSource.FRESH_DATA: "watched objective gained new data (freshness advanced)",
        WakeSource.OPS_CHANGE: "ops-plane verdict changed color",
        WakeSource.PROVIDER_RECOVERY: "a parked provider recovered",
        WakeSource.FILE_APPEND: "a watched append-only file grew (new line)",
        WakeSource.SQLITE_ROW: "a watched sqlite table gained a row",
        WakeSource.FLOOR_ELAPSED: "max-park floor elapsed with no earlier event",
    }[source]


def next_wake(sources: list[WatchSource], elapsed_s: int, max_park: int) -> Optional[WakeReason]:
    """Pure wake decision (mirrors bus::next_wake). Any fired source ends the wait, lowest priority
    wins (KILL always outranks). Else if elapsed >= max(max_park, 0), FLOOR_ELAPSED. Else None."""
    ceiling = max(max_park, 0)
    fired = [w for w in sources if w.fired]
    if fired:
        w = min(fired, key=lambda x: x.source.priority)
        return WakeReason(source=w.source, why=_fired_why(w.source))
    if elapsed_s >= ceiling:
        return WakeReason(source=WakeSource.FLOOR_ELAPSED, why="max-park floor elapsed with no earlier event")
    return None


# =========================================================================== #
# warm — three-tier warm context
# =========================================================================== #

WORKING_MAX_ENTRIES: int = 64
WORKING_MAX_BYTES: int = 16 * 1024
LONG_TERM_ADAPTER_READONLY: bool = True

# The STABLE prompt prefix — MUST be byte-identical to src-tauri/src/pecrt/warm.rs STABLE_PREFIX.
STABLE_PREFIX: str = (
    "[PECRT continuous reasoning thread — stable context prefix v1]\n"
    "You are a persistent, event-driven reasoning thread. You are a SCHEDULER and MEMORY wrapper, not an "
    "authority: you decide WHEN existing, already-gated operations run, never WHAT the gates permit. You "
    "cannot edit the repos.json whitelist/tiers, raise any lane's cycle_budget, or bypass the skeptic, "
    "kill, blast-radius, or freshness gates; every action you schedule re-enters those existing gates "
    "unchanged. Parking is PREFERRED when you are blocked on external truth — do NOT manufacture busy-work.\n"
    "[working context follows]\n"
)


class WorkingTier:
    """HARD-BOUNDED working tier (mirrors warm::WorkingTier). push() enforces BOTH caps by dropping
    the OLDEST entries; a single oversized entry is clamped to the byte cap, never dropped to empty."""

    def __init__(self) -> None:
        self._entries: list[str] = []

    def __len__(self) -> int:
        return len(self._entries)

    def byte_len(self) -> int:
        # UTF-8 byte length (+1 newline each) — decision-identical to Rust String::len() (bytes).
        return sum(len(e.encode("utf-8")) + 1 for e in self._entries)

    def push(self, text: str) -> None:
        b = text.encode("utf-8")
        if len(b) + 1 > WORKING_MAX_BYTES:
            cut = WORKING_MAX_BYTES - 1
            while cut > 0 and (b[cut] & 0xC0) == 0x80:  # don't split a UTF-8 codepoint
                cut -= 1
            text = b[:cut].decode("utf-8", errors="ignore")
        self._entries.append(text)
        while len(self._entries) > WORKING_MAX_ENTRIES:
            self._entries.pop(0)
        while self.byte_len() > WORKING_MAX_BYTES and len(self._entries) > 1:
            self._entries.pop(0)

    def render(self) -> str:
        return "".join(e + "\n" for e in self._entries)


# Summary markers — case-insensitive substring; identical set to warm.rs SUMMARY_MARKERS.
# HONEST SCOPE: a best-effort SHAPE tripwire, NOT a semantic classifier. It catches the canonical
# summary-of-summary shapes and common summarization openers; it cannot catch arbitrary digit-bearing
# prose that avoids every opener. The durable guarantees are append-only + dated + datum-present.
_SUMMARY_MARKERS = (
    # doubled-noun shapes
    "summary of the summar",
    "summary of summar",
    "summaries of summar",
    "digest of digest",
    "summary of the above",
    "recap of the recap",
    "rollup of rollup",
    "overview of overview",
    "in summary, the summaries",
    "to summarize the summaries",
    # common summarization openers / rollup prefixes
    "in summary",
    "to summarize",
    "summarizing the",
    "digest:",
    "recap:",
    "rollup:",
    "overall rollup",
    "high-level recap",
    "high-level summary",
    "tl;dr",
)


class FactVerdict:
    """Fact / RejectedSummary verdict (mirrors warm::FactVerdict)."""

    def __init__(self, is_fact: bool, why: str = "") -> None:
        self.is_fact = is_fact
        self.why = why


class ObservationLog:
    """Append-only DATED SHORT-TERM log. validate_fact keeps it from degrading into prose-of-prose."""

    def __init__(self, path: Path) -> None:
        self.path = Path(path)

    @staticmethod
    def validate_fact(text: str) -> FactVerdict:
        t = text.strip()
        if not t:
            return FactVerdict(False, "empty line is not a fact")
        lower = t.lower()
        if any(m in lower for m in _SUMMARY_MARKERS):
            return FactVerdict(
                False,
                "summary-of-summaries / digest-of-digests — the observation log stores first-order "
                "dated FACTS only, never prose-of-prose",
            )
        if not any(c.isdigit() for c in t):
            return FactVerdict(False, "no concrete datum (date / count / id / metric) — not a first-order fact")
        return FactVerdict(True)

    @staticmethod
    def dated_line(iso_date: str, fact: str) -> str:
        return f"{iso_date}\t{fact}"

    def append_fact(self, iso_date: str, fact: str) -> FactVerdict:
        """Append ONE dated fact; REJECT (write nothing) a non-fact. Returns the verdict."""
        v = ObservationLog.validate_fact(fact)
        if not v.is_fact:
            return v
        self.path.parent.mkdir(parents=True, exist_ok=True)
        with self.path.open("a", encoding="utf-8") as f:
            f.write(ObservationLog.dated_line(iso_date, fact) + "\n")
        return FactVerdict(True)

    def tail(self, n: int) -> list[str]:
        return _read_last_lines(self.path, n)


class LongTermAdapter:
    """READ-ONLY adapter over EXISTING ledgers (mirrors warm::LongTermAdapter). Tails only; NO writes,
    NO migration."""

    def __init__(self, here: Path, lane: str) -> None:
        self.here = Path(here)
        self.lane = lane

    def outcomes_path(self) -> Path:
        return self.here / "runtime" / "outcomes.jsonl"

    def freshness_path(self) -> Path:
        return self.here / "runtime" / self.lane / "freshness.json"

    def progress_path(self) -> Path:
        return self.here / "runtime" / self.lane / "progress.json"

    def tail_outcomes(self, n: int) -> list[str]:
        return _read_last_lines(self.outcomes_path(), n)

    def read_freshness(self) -> Optional[dict]:
        return _read_json(self.freshness_path())

    def read_progress(self) -> Optional[dict]:
        return _read_json(self.progress_path())


def _read_last_lines(path: Path, n: int) -> list[str]:
    """Read the last `n` non-empty lines WITHOUT reading the file end-to-end: seek from the END and
    read fixed 8KiB chunks backward until n+1 newlines are found (or start reached). Mirrors
    warm::read_last_lines. Missing/unreadable/n==0 => []."""
    if n <= 0:
        return []
    try:
        with path.open("rb") as f:
            f.seek(0, os.SEEK_END)
            file_len = f.tell()
            if file_len == 0:
                return []
            chunk = 8 * 1024
            pos = file_len
            buf = b""
            newlines = 0
            while pos > 0 and newlines <= n:
                read_size = min(chunk, pos)
                pos -= read_size
                f.seek(pos)
                data = f.read(read_size)
                newlines += data.count(b"\n")
                buf = data + buf
    except OSError:
        return []
    lines = [ln for ln in buf.decode("utf-8", errors="ignore").splitlines() if ln.strip()]
    return lines[max(0, len(lines) - n):]


def _read_json(path: Path) -> Optional[dict]:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


def _fnv1a(s: str) -> int:
    h = 0xCBF29CE484222325
    for b in s.encode("utf-8"):
        h ^= b
        h = (h * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h


@dataclass
class CacheStats:
    """Running cache-hit statistics over the STABLE prefix (mirrors warm::CacheStats)."""

    hits: int = 0
    misses: int = 0
    _last_prefix_hash: int = field(default=0, repr=False)

    def record(self, prefix: str) -> bool:
        h = _fnv1a(prefix)
        hit = (self.hits + self.misses) > 0 and h == self._last_prefix_hash
        if hit:
            self.hits += 1
        else:
            self.misses += 1
        self._last_prefix_hash = h
        return hit

    def rate(self) -> float:
        total = self.hits + self.misses
        return 0.0 if total == 0 else self.hits / total

    def log_line(self) -> str:
        return f"pecrt cache: prefix_hit_rate={self.rate():.3f} ({self.hits} hits / {self.hits + self.misses} total)"


@dataclass
class ReconstructedContext:
    prefix: str
    working: str
    prefix_cache_hit: bool

    def full_prompt(self) -> str:
        return self.prefix + self.working


class WarmContext:
    """The tiers + cache stats a thread holds across wakes (mirrors warm::WarmContext)."""

    def __init__(self, observations: ObservationLog, long_term: LongTermAdapter) -> None:
        self.observations = observations
        self.long_term = long_term
        self.cache = CacheStats()

    def reconstruct_context(self, obs_tail_n: int, outcomes_tail_n: int) -> ReconstructedContext:
        """Rebuild WORKING context from the SHORT-TERM observation tail + LONG-TERM adapter tails —
        WITHOUT re-reading full ledgers end-to-end. Bounded working tier; records cache hit/miss."""
        working = WorkingTier()
        for line in self.observations.tail(obs_tail_n):
            working.push(f"obs: {line}")
        for line in self.long_term.tail_outcomes(outcomes_tail_n):
            working.push(f"outcome: {line}")
        f = self.long_term.read_freshness()
        if f is not None:
            working.push(f"freshness: {json.dumps(f)}")
        p = self.long_term.read_progress()
        if p is not None:
            working.push(f"progress: {json.dumps(p)}")
        hit = self.cache.record(STABLE_PREFIX)
        return ReconstructedContext(prefix=STABLE_PREFIX, working=working.render(), prefix_cache_hit=hit)


# =========================================================================== #
# safety — the HARD invariant (scheduler+memory WRAPPER, never a new authority)
# =========================================================================== #

SKEPTIC_BYPASS_MARKER: str = "skeptic_bypass"

# Identical set + order to safety.rs FORBIDDEN_TARGETS.
FORBIDDEN_TARGETS = (
    "repos.json",
    "whitelist",
    "tier",
    "cycle_budget",
    SKEPTIC_BYPASS_MARKER,
    "skeptic",
    "blast_radius",
    "blast-radius",
    "money_guard",
    "no_money_out",
    "kill_gate",
    "kill_sentinel",
    "freshness_gate",
)


@dataclass(frozen=True)
class ScheduleRequest:
    verb: str
    target: str
    reenters_gates: bool


class ScheduleVerdict:
    def __init__(self, allowed: bool, reason: str) -> None:
        self.allowed = allowed
        self.reason = reason


def targets_governance(target: str) -> bool:
    t = target.lower()
    return any(m in t for m in FORBIDDEN_TARGETS)


def classify_schedule(req: ScheduleRequest) -> ScheduleVerdict:
    """PURE, fail-closed classifier (mirrors safety::classify_schedule).
    (1) governance target -> DENY (no allow arm). (2) not re-entering gates -> DENY. (3) else ALLOW."""
    if targets_governance(req.target):
        return ScheduleVerdict(
            False,
            f"schedule DENIED: target '{req.target}' is a self-governance surface. The persistent "
            "thread is a scheduler+memory WRAPPER, never an authority — it cannot edit the repos.json "
            "whitelist/tiers, raise cycle_budget, or bypass the skeptic/kill/blast-radius/freshness "
            f"gates. This route is fail-closed: there is NO allow arm for a governance target (verb='{req.verb}')",
        )
    if not req.reenters_gates:
        return ScheduleVerdict(
            False,
            f"schedule DENIED: action '{req.verb}' on '{req.target}' declares it would NOT re-enter the "
            "existing gate funnel (money_guard / freshness short-circuit / blast-radius / skeptic). The "
            "thread may only change WHEN a gated action runs, never let one run unchecked",
        )
    return ScheduleVerdict(
        True,
        f"schedule ALLOWED: '{req.verb}' on '{req.target}' may be scheduled — it re-enters the EXISTING "
        "gate funnel (money_guard -> freshness short-circuit -> blast-radius -> skeptic) unchanged. The "
        "thread only decides timing; the gates decide the outcome",
    )


def guard_schedule(req: ScheduleRequest) -> Optional[dict]:
    """Returns None (proceed into the unchanged gate funnel) or a fail-closed refusal dict."""
    v = classify_schedule(req)
    if v.allowed:
        return None
    return {"ok": False, "verb": req.verb, "target": req.target, "pecrt_safety": True, "error": v.reason}


# =========================================================================== #
# self-check (mirrors the Rust #[test] suite — run `python pecrt.py`)
# =========================================================================== #


def _selfcheck() -> int:
    import tempfile

    failures = []

    def check(cond, msg):
        if not cond:
            failures.append(msg)

    # bus: next_wake priority + floor
    check(next_wake([WatchSource(WakeSource.FRESH_DATA, False)], 10, 300) is None, "no fire before floor")
    check(
        next_wake([WatchSource(WakeSource.FRESH_DATA, False)], 300, 300).source is WakeSource.FLOOR_ELAPSED,
        "floor elapses",
    )
    both = [WatchSource(WakeSource.FRESH_DATA, True), WatchSource(WakeSource.KILL, True)]
    check(next_wake(both, 5, 300).source is WakeSource.KILL, "KILL outranks FreshData")

    # freshness OR-rule + observability
    before = FreshnessEvent("x", 100.0, 10, True)
    check(FreshnessEvent("x", 100.0, 11, True).advanced(before), "more samples advances")
    check(not FreshnessEvent("x", 100.0, 99, False).advanced(before), "unobservable never advances")
    check(not FreshnessEvent("x", 100.0, 11, True).advanced(None), "no baseline never advances")

    # warm: working tier hard bound
    w = WorkingTier()
    for i in range(WORKING_MAX_ENTRIES * 4):
        w.push(f"entry {i}")
    check(len(w) == WORKING_MAX_ENTRIES, "working entry cap")
    check(w.byte_len() <= WORKING_MAX_BYTES, "working byte cap")

    # observation log: reject summary-of-summary, accept dated fact
    check(not ObservationLog.validate_fact("a summary of the summaries so far").is_fact, "reject summary-of-summary")
    check(not ObservationLog.validate_fact("everything went well overall").is_fact, "reject no-datum")
    check(ObservationLog.validate_fact("kairos cycle 412 shipped PR #88 equity +0.42").is_fact, "accept fact")
    # Finding 1: digit-bearing prose summaries rejected by the tightened opener tripwire.
    for bad in ("Summarizing the past week: 3 lanes did well", "In summary, 6 lanes improved",
                "Digest: 5 cycles ran", "Overall rollup of the fleet: 90% healthy", "TL;DR: 4 ships today"):
        check(not ObservationLog.validate_fact(bad).is_fact, f"reject digit-bearing summary: {bad!r}")

    with tempfile.TemporaryDirectory() as d:
        here = Path(d)
        lane = "kairos"
        (here / "runtime" / lane).mkdir(parents=True)
        log = ObservationLog(here / "runtime" / lane / "observations.jsonl")
        for i in range(1000):
            log.append_fact("2026-07-08", f"cycle {i} shipped PR #{i}")
        check(log.append_fact("2026-07-08", "a summary of the summaries").is_fact is False, "append rejects summary")
        (here / "runtime" / lane / "freshness.json").write_text(
            json.dumps({"metric_id": "settled_usd_15m", "last_n_samples": 2863})
        )
        warm = WarmContext(log, LongTermAdapter(here, lane))
        ctx = warm.reconstruct_context(8, 8)
        check("cycle 999" in ctx.working, "tail includes newest")
        check("cycle 0 " not in ctx.working, "tail excludes oldest (not end-to-end)")
        check("settled_usd_15m" in ctx.working, "freshness ledger head present")
        check(len(ctx.working.encode("utf-8")) <= WORKING_MAX_BYTES, "working render bounded")
        check(ctx.full_prompt().startswith(STABLE_PREFIX), "stable prefix first")
        check(ctx.prefix_cache_hit is False, "first wake is a cache miss")
        for _ in range(5):
            r = warm.reconstruct_context(8, 8)
            check(r.prefix_cache_hit, "stable prefix hits after first wake")
        check(warm.cache.rate() > 0.8, "cache-hit rate observable and high")

    # warm: reverse-seek tail returns the correct suffix across chunk boundaries
    with tempfile.TemporaryDirectory() as d:
        big = Path(d) / "big.jsonl"
        big.write_text("".join(f"line {i} with some padding to exceed a single chunk\n" for i in range(5000)))
        t = _read_last_lines(big, 3)
        check(len(t) == 3 and t[0].startswith("line 4997") and t[2].startswith("line 4999"), "reverse-seek tail suffix")
        check(len(_read_last_lines(big, 100000)) == 5000, "tail n>lines returns all")
        no_tr = Path(d) / "no_trailing.txt"
        no_tr.write_text("a1\nb2\nc3")
        check(_read_last_lines(no_tr, 2) == ["b2", "c3"], "no trailing newline")
        check(_read_last_lines(no_tr, 0) == [] and _read_last_lines(Path(d) / "nope", 5) == [], "n==0/missing => []")

    # safety: governance mutation always denied
    for target in ("repos.json", "whitelist", "kairos.cycle_budget", "skeptic", "skeptic_bypass", "blast_radius"):
        for verb in ("edit", "raise", "widen", "bypass", "disable"):
            v = classify_schedule(ScheduleRequest(verb, target, True))
            check(not v.allowed, f"SAFETY BREACH: allowed {verb} on {target}")
            check(guard_schedule(ScheduleRequest(verb, target, True)) is not None, f"guard refuses {verb} {target}")
    # ordinary gated action schedulable
    check(classify_schedule(ScheduleRequest("run_iteration", "kairos", True)).allowed, "ordinary action schedulable")
    check(not classify_schedule(ScheduleRequest("run_iteration", "kairos", False)).allowed, "gate-bypass denied")

    # stable prefix frozen invariants
    check(STABLE_PREFIX.startswith("[PECRT continuous reasoning thread — stable context prefix v1]"), "prefix id")
    check("cannot edit the repos.json whitelist/tiers" in STABLE_PREFIX, "prefix invariant clause")
    check("Parking is PREFERRED" in STABLE_PREFIX, "prefix anti-compulsion clause")
    check(STABLE_PREFIX.endswith("[working context follows]\n"), "prefix tail")

    if failures:
        print(f"pecrt.py self-check: {len(failures)} FAILURE(S)")
        for f in failures:
            print("  FAIL:", f)
        return 1
    print("pecrt.py self-check: OK (all decision-mirror checks passed)")
    return 0


if __name__ == "__main__":
    raise SystemExit(_selfcheck())
