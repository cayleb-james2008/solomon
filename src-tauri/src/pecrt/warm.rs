//! Three-tier warm context + `reconstruct_context` — cheap, bounded memory across wakes.
//!
//! A continuous reasoning thread that reloads its FULL history every wake is both slow and a cache
//! killer (the prompt prefix changes every time, so the LLM provider's prefix cache never hits). This
//! module gives the thread three tiers with strict, distinct roles:
//!
//!   * [`WorkingTier`] — WORKING memory. HARD-BOUNDED (a byte cap AND an entry cap), REWRITTEN each
//!     wake from the other tiers. This is the only tier that grows within a wake, and it can NEVER
//!     grow unbounded: past the cap, the OLDEST entries are dropped (truncation, not append). This is
//!     the "scratchpad" the model reasons over right now.
//!
//!   * [`ObservationLog`] — SHORT-TERM memory. An append-only, DATED log of FACTS — one line per
//!     observed event ("lane kairos finished cycle 412, shipped PR #88, equity +0.42"). It is NEVER
//!     prose-of-prose: a write that looks like a summary-of-summaries (a digest of digests, a
//!     "summary of the summaries", a rollup with no concrete datum) is REJECTED. The log is the
//!     durable record the working tier is rebuilt from, so it must stay factual and append-only —
//!     compaction-by-summarization is exactly the failure mode (drift, hallucinated history) this
//!     forbids.
//!
//!   * [`LongTermAdapter`] — LONG-TERM memory. A READ-ONLY adapter over the ledgers Solomon ALREADY
//!     keeps: `runtime/outcomes.jsonl`, `runtime/<lane>/freshness.json`, and the progress/calibration
//!     ledgers. NO migration, NO new store — it just reads the tail of what exists. The long-term
//!     tier is authoritative-by-reference; the thread does not copy it, it points at it.
//!
//! [`reconstruct_context`] assembles these behind a STABLE prompt prefix ([`STABLE_PREFIX`]) so the
//! LLM provider's prefix cache hits wake after wake. The prefix bytes are FIXED (a constant, pinned
//! by test and cross-checked against the Python mirror); only the content AFTER the prefix varies.
//! The function tracks and emits a running cache-hit rate over the stable prefix so cache behavior is
//! observable in logs (acceptance criterion (c)).
//!
//! ## Acceptance-criterion crosswalk
//!
//!   (a) a lane finishing a cycle appends ONE dated observation line; the next wake rebuilds the
//!       working tier from the observation-log tail + the long-term adapter's TAILS — it does NOT
//!       re-read the full ledgers end-to-end (`LongTermAdapter::tail_*` read only the last N).
//!   (c) `reconstruct_context` returns the stable prefix first and records a cache hit/miss; the
//!       running rate is exposed via [`CacheStats`] for the caller to log.
//!   (d) `WorkingTier::push` enforces the hard bound — a test feeds > cap entries and asserts
//!       truncation.
//!   (e) `ObservationLog::validate_fact` rejects a summary-of-summary write — a test asserts the
//!       rejection.

use serde_json::Value;
use std::path::{Path, PathBuf};

/// Hard cap on the WORKING tier's entry count. Rewritten each wake; never exceeds this many entries.
pub const WORKING_MAX_ENTRIES: usize = 64;

/// Hard cap on the WORKING tier's total serialized byte size. Whichever cap binds first truncates.
/// 16 KiB is a generous working scratchpad while staying a small, cache-friendly, bounded prefix tail.
pub const WORKING_MAX_BYTES: usize = 16 * 1024;

/// The long-term tier is READ-ONLY — a compile-time-visible assertion of the no-migration contract.
/// (Referenced by `mod.rs` re-export so the doctrine is discoverable from the crate surface.)
pub const LONG_TERM_ADAPTER_READONLY: bool = true;

/// The STABLE prompt prefix. Assembled FIRST in every reconstructed context so the LLM provider's
/// prefix cache hits across wakes. These bytes must NOT change wake-to-wake (only content AFTER the
/// prefix varies) and must be byte-identical to the Python mirror's `STABLE_PREFIX`
/// (`pecrt.py`) — pinned by `stable_prefix_is_frozen` and the cross-impl golden check.
pub const STABLE_PREFIX: &str = "\
[PECRT continuous reasoning thread — stable context prefix v1]
You are a persistent, event-driven reasoning thread. You are a SCHEDULER and MEMORY wrapper, not an \
authority: you decide WHEN existing, already-gated operations run, never WHAT the gates permit. You \
cannot edit the repos.json whitelist/tiers, raise any lane's cycle_budget, or bypass the skeptic, \
kill, blast-radius, or freshness gates; every action you schedule re-enters those existing gates \
unchanged. Parking is PREFERRED when you are blocked on external truth — do NOT manufacture busy-work.
[working context follows]
";

/// One WORKING-tier entry — a short reasoning note or a projected fact for the current wake.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkingEntry {
    pub text: String,
}

/// The HARD-BOUNDED working tier. Rewritten each wake; `push` enforces BOTH caps by dropping the
/// OLDEST entries (FIFO truncation) — it can never grow past `WORKING_MAX_ENTRIES` entries or
/// `WORKING_MAX_BYTES` total bytes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkingTier {
    entries: Vec<WorkingEntry>,
}

impl WorkingTier {
    pub fn new() -> Self {
        WorkingTier { entries: Vec::new() }
    }

    /// Current entry count.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Total serialized bytes (sum of entry text lengths + 1 newline each — the render cost).
    pub fn byte_len(&self) -> usize {
        self.entries.iter().map(|e| e.text.len() + 1).sum()
    }

    /// Append an entry, then enforce BOTH hard caps by dropping the oldest entries until BOTH the
    /// entry-count and byte caps hold. A single entry larger than the byte cap is itself truncated to
    /// fit (it is never dropped to empty, so a huge line still leaves a bounded trace). This is the
    /// bound that makes the tier "hard-bounded, rewritten each wake" — growth is impossible.
    pub fn push(&mut self, text: &str) {
        // Clamp a single oversized entry to the byte cap up front (leave room for its newline).
        let clamped = if text.len() + 1 > WORKING_MAX_BYTES {
            let mut cut = WORKING_MAX_BYTES.saturating_sub(1);
            // don't split a UTF-8 codepoint
            while cut > 0 && !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text[..cut].to_string()
        } else {
            text.to_string()
        };
        self.entries.push(WorkingEntry { text: clamped });

        // Drop oldest until the ENTRY cap holds.
        while self.entries.len() > WORKING_MAX_ENTRIES {
            self.entries.remove(0);
        }
        // Drop oldest until the BYTE cap holds (keep at least the newest entry).
        while self.byte_len() > WORKING_MAX_BYTES && self.entries.len() > 1 {
            self.entries.remove(0);
        }
    }

    /// Render the tier to a string (one entry per line) for inclusion after the stable prefix.
    pub fn render(&self) -> String {
        let mut s = String::new();
        for e in &self.entries {
            s.push_str(&e.text);
            s.push('\n');
        }
        s
    }
}

/// Whether a candidate observation-log line is a legal FACT (append) or a rejected
/// summary-of-summary / prose-of-prose (never append).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactVerdict {
    /// A concrete dated fact — legal to append.
    Fact,
    /// A summary-of-summaries / digest-of-digests / contentless rollup — REJECTED.
    RejectedSummary { why: &'static str },
}

impl FactVerdict {
    pub fn is_fact(&self) -> bool {
        matches!(self, FactVerdict::Fact)
    }
}

/// The append-only, DATED SHORT-TERM observation log. One line per observed event; every line
/// carries an ISO-8601 date and states a concrete fact. `validate_fact` is the gate that keeps the
/// log from degrading into prose-of-prose.
#[derive(Debug, Clone)]
pub struct ObservationLog {
    path: PathBuf,
}

/// Phrases that mark a write as a summary / summary-of-summaries / prose rollup rather than a
/// first-order fact. Case-insensitive substring match. This is intentionally about SHAPE (a rollup
/// of prior prose) — a first-order line "lane kairos shipped PR #88, equity +0.42" trips none of
/// these.
///
/// HONEST SCOPE: this is a best-effort SHAPE tripwire, NOT a semantic classifier. It reliably
/// catches the canonical summary-of-summary / digest-of-digest shapes and the common summarization
/// openers; it can NOT catch an arbitrary paragraph of prose that happens to carry a digit and avoids
/// every opener (e.g. a narrative "things improved across 6 lanes"). The durable guarantee the
/// observation log gives is APPEND-ONLY + DATED + non-trivially-factual (a datum present); keeping
/// each line genuinely first-order is enforced primarily by the WRITER contract (lanes append one
/// concrete event per finished cycle), with this tripwire as the mechanical backstop against the
/// most common degradation shapes. Do not read it as a proof that no summary can ever land.
const SUMMARY_MARKERS: &[&str] = &[
    // doubled-noun shapes (a digest of digests / summary of summaries)
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
    // common summarization OPENERS / rollup prefixes (a line that STARTS as a summary of prior prose)
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
];

impl ObservationLog {
    /// An observation log at `runtime/<lane>/observations.jsonl` (created lazily on first append).
    pub fn at(path: PathBuf) -> Self {
        ObservationLog { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Validate a candidate log line. A legal fact:
    ///   * is non-empty after trimming, AND
    ///   * contains at least one concrete DATUM signal — an ISO-date/time, a number, a PR/#id, a
    ///     lane/metric token — (a bare adjectival sentence with no datum is not a fact), AND
    ///   * does NOT match any [`SUMMARY_MARKERS`] shape (summary-of-summaries + common summarization
    ///     openers are rejected).
    ///
    /// HONEST SCOPE (see [`SUMMARY_MARKERS`]): the summary check is a best-effort SHAPE tripwire, not
    /// a semantic classifier — it backstops the common degradation shapes, it is not a proof that no
    /// summary can ever pass. The hard, durable guarantees are append-only + dated + datum-present.
    ///
    /// PURE — no IO; `append_fact` calls this and refuses on a non-fact.
    pub fn validate_fact(text: &str) -> FactVerdict {
        let t = text.trim();
        if t.is_empty() {
            return FactVerdict::RejectedSummary { why: "empty line is not a fact" };
        }
        let lower = t.to_ascii_lowercase();
        if let Some(_m) = SUMMARY_MARKERS.iter().find(|m| lower.contains(**m)) {
            return FactVerdict::RejectedSummary {
                why: "summary-of-summaries / digest-of-digests — the observation log stores \
                      first-order dated FACTS only, never prose-of-prose",
            };
        }
        // Require at least one concrete datum: any digit (dates, counts, ids, equity all carry one).
        // A contentless rollup like "everything went well overall" carries no digit and is rejected.
        if !t.chars().any(|c| c.is_ascii_digit()) {
            return FactVerdict::RejectedSummary {
                why: "no concrete datum (date / count / id / metric) — not a first-order fact",
            };
        }
        FactVerdict::Fact
    }

    /// Build a dated fact line: `"<iso_date>\t<fact>"`. Pure — the caller supplies the date so this
    /// is testable without a clock. The date makes the log DATED (a hard requirement); the fact must
    /// pass `validate_fact`.
    pub fn dated_line(iso_date: &str, fact: &str) -> String {
        format!("{iso_date}\t{fact}")
    }

    /// Append ONE dated fact to the log, atomically (create+append). REJECTS (returns the verdict,
    /// writes nothing) when `fact` is not a first-order dated fact. This is the ONLY write path — the
    /// log can never gain a summary-of-summaries line. Returns `Ok(())` on a successful append.
    ///
    /// Note: this validates the FACT payload (not the whole dated line), then persists
    /// `dated_line(iso_date, fact)`. IO errors (dir create / open / write) surface as `Err`.
    pub fn append_fact(&self, iso_date: &str, fact: &str) -> Result<(), FactVerdict> {
        match Self::validate_fact(fact) {
            FactVerdict::Fact => {}
            rejected => return Err(rejected),
        }
        let line = Self::dated_line(iso_date, fact);
        // best-effort dir create; append; a real IO failure is swallowed to a soft error verdict so
        // the reasoning loop never wedges on a disk hiccup (the fact is lost, not fatal).
        if let Some(parent) = self.path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return Err(FactVerdict::RejectedSummary { why: "observation-log dir unwritable" });
            }
        }
        use std::io::Write;
        match std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            Ok(mut f) => {
                let _ = writeln!(f, "{line}");
                Ok(())
            }
            Err(_) => Err(FactVerdict::RejectedSummary { why: "observation-log unwritable" }),
        }
    }

    /// Read the LAST `n` observation lines (the tail) WITHOUT reading the whole file end-to-end.
    /// Backed by [`read_last_lines`], which seeks from the END and reads bounded chunks — so a
    /// million-line log costs a few KiB of IO, not a full-file read. Missing file => empty. This is
    /// what the next wake rebuilds working context from — bounded, not end-to-end (acceptance (a)).
    pub fn tail(&self, n: usize) -> Vec<String> {
        read_last_lines(&self.path, n)
    }
}

/// The READ-ONLY long-term adapter over EXISTING ledgers. It reads TAILS only — never the whole file
/// into context — and NEVER writes. `here` is the operator data dir (`control::paths::here()`), `lane`
/// the repo name; the ledger paths mirror exactly what the existing code already maintains.
#[derive(Debug, Clone)]
pub struct LongTermAdapter {
    here: PathBuf,
    lane: String,
}

impl LongTermAdapter {
    pub fn new(here: PathBuf, lane: &str) -> Self {
        LongTermAdapter { here, lane: lane.to_string() }
    }

    /// `runtime/outcomes.jsonl` (fleet-wide outcomes — shared, not per-lane).
    pub fn outcomes_path(&self) -> PathBuf {
        self.here.join("runtime").join("outcomes.jsonl")
    }

    /// `runtime/<lane>/freshness.json` (the LEDGER the freshness gate maintains).
    pub fn freshness_path(&self) -> PathBuf {
        self.here.join("runtime").join(&self.lane).join("freshness.json")
    }

    /// `runtime/<lane>/progress.json` (the progress ledger).
    pub fn progress_path(&self) -> PathBuf {
        self.here.join("runtime").join(&self.lane).join("progress.json")
    }

    /// Read the last `n` lines of `outcomes.jsonl` — a bounded tail (reverse-seek), never the whole
    /// ledger read end-to-end.
    pub fn tail_outcomes(&self, n: usize) -> Vec<String> {
        read_last_lines(&self.outcomes_path(), n)
    }

    /// Read the current freshness LEDGER value (whole small file — it is one object, ~200 bytes).
    /// `None` when absent/unparseable. Read-only; NO migration.
    pub fn read_freshness(&self) -> Option<Value> {
        let text = std::fs::read_to_string(self.freshness_path()).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Read the current progress LEDGER value (whole small file). `None` when absent/unparseable.
    pub fn read_progress(&self) -> Option<Value> {
        let text = std::fs::read_to_string(self.progress_path()).ok()?;
        serde_json::from_str(&text).ok()
    }
}

/// Read the last `n` non-empty lines of a file WITHOUT reading it end-to-end: seek from the END and
/// read fixed-size chunks backward until `n` newlines are found (or the file start is reached), then
/// return the last `n` non-empty lines. A giant append-only ledger costs O(bytes-of-the-tail) IO, not
/// O(whole file) — the property acceptance criterion (a) requires. Missing/unreadable file => empty.
/// Never returns more than `n`. Shared by the observation-log tail and the long-term outcomes tail.
/// `pub(crate)`: the D8 cross-project wins reader (`ceo::wins`) reuses this exact bounded tail rather
/// than re-implementing a whole-file read of the outcomes ledger.
pub(crate) fn read_last_lines(path: &Path, n: usize) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    if n == 0 {
        return Vec::new();
    }
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let file_len = match f.seek(SeekFrom::End(0)) {
        Ok(l) => l,
        Err(_) => return Vec::new(),
    };
    if file_len == 0 {
        return Vec::new();
    }

    const CHUNK: u64 = 8 * 1024;
    let mut pos = file_len;
    let mut buf: Vec<u8> = Vec::new();
    // We want n lines; count newlines in the accumulated (end-of-file) bytes. Because a trailing
    // newline terminates the LAST line, we read until we have counted n+1 newlines (or hit start),
    // guaranteeing at least n full lines are present in `buf`.
    let mut newlines = 0usize;
    while pos > 0 && newlines <= n {
        let read_size = CHUNK.min(pos);
        pos -= read_size;
        if f.seek(SeekFrom::Start(pos)).is_err() {
            break;
        }
        let mut chunk = vec![0u8; read_size as usize];
        if f.read_exact(&mut chunk).is_err() {
            break;
        }
        newlines += chunk.iter().filter(|&&b| b == b'\n').count();
        // prepend this earlier chunk in front of what we already have.
        chunk.extend_from_slice(&buf);
        buf = chunk;
    }

    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].iter().map(|s| s.to_string()).collect()
}

/// Running cache-hit statistics over the STABLE prefix. `reconstruct_context` records one hit or miss
/// per call; `rate()` is the observable cache-hit rate (acceptance criterion (c)). A "hit" means the
/// stable prefix bytes were unchanged since the last reconstruction (so the provider's prefix cache
/// can serve it); a "miss" is the first reconstruction or any prefix change (never expected in
/// steady state, since the prefix is a constant — a miss there is a real regression signal).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    /// Hash of the prefix at the last reconstruction, to detect an unexpected prefix change.
    last_prefix_hash: u64,
}

impl CacheStats {
    pub fn new() -> Self {
        CacheStats::default()
    }

    /// Record a reconstruction and return whether the stable prefix HIT the cache (unchanged prefix).
    /// The very first call is a miss (nothing cached yet); every subsequent call with the same prefix
    /// bytes is a hit.
    pub fn record(&mut self, prefix: &str) -> bool {
        let h = fnv1a(prefix);
        let hit = self.hits + self.misses > 0 && h == self.last_prefix_hash;
        if hit {
            self.hits += 1;
        } else {
            self.misses += 1;
        }
        self.last_prefix_hash = h;
        hit
    }

    /// Observable cache-hit rate in [0.0, 1.0]. 0.0 before any reconstruction.
    pub fn rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        }
    }

    /// A one-line, log-friendly summary of the cache behavior (emitted by the caller each wake).
    pub fn log_line(&self) -> String {
        format!(
            "pecrt cache: prefix_hit_rate={:.3} ({} hits / {} total)",
            self.rate(),
            self.hits,
            self.hits + self.misses
        )
    }
}

/// A tiny FNV-1a hash — enough to detect a prefix change; not cryptographic.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// The assembled context for one wake: the stable prefix, then the (bounded) working tier render.
/// `prefix_cache_hit` reports whether this reconstruction's stable prefix hit the provider cache.
#[derive(Debug, Clone, PartialEq)]
pub struct ReconstructedContext {
    pub prefix: String,
    pub working: String,
    pub prefix_cache_hit: bool,
}

impl ReconstructedContext {
    /// The full prompt = stable prefix + working render. The prefix is ALWAYS first so the provider
    /// prefix-cache can serve it.
    pub fn full_prompt(&self) -> String {
        format!("{}{}", self.prefix, self.working)
    }
}

/// The convenience bundle a thread holds across wakes: the tiers + the cache stats.
#[derive(Debug, Clone)]
pub struct WarmContext {
    pub observations: ObservationLog,
    pub long_term: LongTermAdapter,
    pub cache: CacheStats,
}

impl WarmContext {
    pub fn new(observations: ObservationLog, long_term: LongTermAdapter) -> Self {
        WarmContext {
            observations,
            long_term,
            cache: CacheStats::new(),
        }
    }

    /// Rebuild WORKING context for a wake from the SHORT-TERM observation tail + the LONG-TERM
    /// adapter tails — WITHOUT re-reading the full ledgers end-to-end (acceptance criterion (a)).
    /// The working tier is HARD-BOUNDED, so this can never grow unbounded no matter how large the
    /// ledgers are. Records a cache hit/miss over the stable prefix (acceptance criterion (c)).
    ///
    /// `obs_tail_n` / `outcomes_tail_n` bound how much recent history seeds the working tier — both
    /// are TAILS, so a million-line ledger costs the same as a hundred-line one.
    pub fn reconstruct_context(&mut self, obs_tail_n: usize, outcomes_tail_n: usize) -> ReconstructedContext {
        let mut working = WorkingTier::new();

        // SHORT-TERM: the recent dated facts (tail, not the whole log).
        for line in self.observations.tail(obs_tail_n) {
            working.push(&format!("obs: {line}"));
        }
        // LONG-TERM (read-only adapters, tails only): recent outcomes + the current freshness/progress
        // ledger heads (single small objects). No migration; no end-to-end ledger read.
        for line in self.long_term.tail_outcomes(outcomes_tail_n) {
            working.push(&format!("outcome: {line}"));
        }
        if let Some(f) = self.long_term.read_freshness() {
            working.push(&format!("freshness: {f}"));
        }
        if let Some(p) = self.long_term.read_progress() {
            working.push(&format!("progress: {p}"));
        }

        // Record cache behavior over the FIXED stable prefix (it never changes wake-to-wake).
        let hit = self.cache.record(STABLE_PREFIX);

        ReconstructedContext {
            prefix: STABLE_PREFIX.to_string(),
            working: working.render(),
            prefix_cache_hit: hit,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pecrt_warm_{}_{}", tag, std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    // ---- (d) WORKING tier enforces its hard bound (entry cap AND byte cap) ----

    #[test]
    fn working_tier_truncates_past_entry_cap_not_unbounded() {
        let mut w = WorkingTier::new();
        // feed WAY more than the cap.
        for i in 0..(WORKING_MAX_ENTRIES * 4) {
            w.push(&format!("entry {i}"));
        }
        assert_eq!(w.len(), WORKING_MAX_ENTRIES, "entry count must be hard-capped, not unbounded");
        // FIFO: the OLDEST were dropped, the NEWEST survive.
        let rendered = w.render();
        assert!(rendered.contains(&format!("entry {}", WORKING_MAX_ENTRIES * 4 - 1)), "newest kept");
        assert!(!rendered.contains("entry 0\n"), "oldest dropped");
    }

    #[test]
    fn working_tier_enforces_byte_cap() {
        let mut w = WorkingTier::new();
        // each entry ~1KiB; far more than the byte cap in total.
        let big = "x".repeat(1024);
        for _ in 0..64 {
            w.push(&big);
        }
        assert!(w.byte_len() <= WORKING_MAX_BYTES, "byte size must be hard-capped: {}", w.byte_len());
    }

    #[test]
    fn a_single_oversized_entry_is_clamped_not_dropped_to_empty() {
        let mut w = WorkingTier::new();
        w.push(&"y".repeat(WORKING_MAX_BYTES * 2));
        assert_eq!(w.len(), 1, "one entry survives");
        assert!(w.byte_len() <= WORKING_MAX_BYTES, "clamped to the byte cap");
    }

    // ---- (e) observation log rejects a summary-of-summary; accepts a dated fact ----

    #[test]
    fn observation_log_rejects_summary_of_summary() {
        for bad in [
            "Here is a summary of the summaries from the last 10 cycles",
            "A digest of digests across all lanes",
            "To summarize the summaries: things are trending up",
            "everything went well overall", // no datum
            "   ",                          // empty
        ] {
            let v = ObservationLog::validate_fact(bad);
            assert!(!v.is_fact(), "should reject non-fact: {bad:?}");
        }
    }

    /// Skeptic Finding 1: digit-bearing prose summaries must ALSO be rejected by the tightened
    /// opener tripwire (they defeated the original doubled-noun-only marker set).
    #[test]
    fn observation_log_rejects_digit_bearing_prose_summaries() {
        for bad in [
            "Summarizing the past week: 3 lanes did well, morale is high",
            "In summary, 6 lanes improved and the trend is positive",
            "To summarize: 5 cycles ran and everything is basically fine",
            "Digest: everything is basically fine, 5 cycles ran",
            "Overall rollup of the fleet: 90% healthy",
            "A high-level recap of recent progress, 12 items",
            "TL;DR: 4 ships today",
        ] {
            let v = ObservationLog::validate_fact(bad);
            assert!(!v.is_fact(), "tightened tripwire should reject digit-bearing summary: {bad:?}");
        }
        // and a genuine first-order fact with the same digits still PASSES.
        assert!(ObservationLog::validate_fact("kairos cycle 412 shipped PR #88, equity +0.42").is_fact());
    }

    #[test]
    fn observation_log_accepts_a_dated_first_order_fact() {
        for good in [
            "lane kairos finished cycle 412, shipped PR #88, equity +0.42",
            "asmodeus freshness advanced: n_samples 2863",
            "sover posted 3 items at 2026-07-07T22:58:53Z",
        ] {
            assert!(ObservationLog::validate_fact(good).is_fact(), "should accept fact: {good:?}");
        }
    }

    #[test]
    fn append_fact_persists_facts_and_refuses_summaries() {
        let dir = tmp_dir("append");
        let log = ObservationLog::at(dir.join("observations.jsonl"));
        // a summary is refused, writes NOTHING.
        let err = log.append_fact("2026-07-08", "a summary of the summaries so far").unwrap_err();
        assert!(matches!(err, FactVerdict::RejectedSummary { .. }));
        assert!(log.tail(10).is_empty(), "refused write must not persist");
        // a dated fact appends.
        log.append_fact("2026-07-08", "kairos cycle 412 shipped PR #88 equity +0.42").unwrap();
        let tail = log.tail(10);
        assert_eq!(tail.len(), 1);
        assert!(tail[0].starts_with("2026-07-08\t"), "line must be dated");
        assert!(tail[0].contains("PR #88"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- (a) next wake rebuilds working context from TAILS, not end-to-end ----

    #[test]
    fn reconstruct_rebuilds_working_from_tails_not_full_ledgers() {
        let here = tmp_dir("recon");
        let lane = "kairos";
        let runtime = here.join("runtime").join(lane);
        std::fs::create_dir_all(&runtime).unwrap();
        // A LARGE observation log — reconstruct must only take the TAIL.
        let log = ObservationLog::at(runtime.join("observations.jsonl"));
        for i in 0..1000 {
            log.append_fact("2026-07-08", &format!("cycle {i} shipped PR #{i}")).unwrap();
        }
        // a freshness ledger head.
        std::fs::write(
            runtime.join("freshness.json"),
            json!({"metric_id": "settled_usd_15m", "last_n_samples": 2863}).to_string(),
        )
        .unwrap();

        let adapter = LongTermAdapter::new(here.clone(), lane);
        let mut warm = WarmContext::new(log, adapter);
        let ctx = warm.reconstruct_context(8, 8); // tail only 8 obs

        // working tier is bounded and seeded from the TAIL (recent cycles), not cycle 0.
        assert!(ctx.working.contains("cycle 999"), "newest obs present");
        assert!(!ctx.working.contains("cycle 0 "), "oldest obs NOT loaded (tail only)");
        assert!(ctx.working.contains("settled_usd_15m"), "freshness ledger head present");
        // the working render is bounded regardless of the 1000-line log.
        assert!(ctx.working.len() <= WORKING_MAX_BYTES);
        // full prompt starts with the stable prefix.
        assert!(ctx.full_prompt().starts_with(STABLE_PREFIX));
        let _ = std::fs::remove_dir_all(&here);
    }

    // ---- (c) cache-hit rate over the stable prefix is observable ----

    #[test]
    fn stable_prefix_hits_cache_after_first_wake_and_rate_is_observable() {
        let here = tmp_dir("cache");
        let lane = "kairos";
        std::fs::create_dir_all(here.join("runtime").join(lane)).unwrap();
        let log = ObservationLog::at(here.join("runtime").join(lane).join("observations.jsonl"));
        let adapter = LongTermAdapter::new(here.clone(), lane);
        let mut warm = WarmContext::new(log, adapter);

        let first = warm.reconstruct_context(4, 4);
        assert!(!first.prefix_cache_hit, "first wake is a cache miss (nothing cached)");
        // subsequent wakes hit — the prefix is a FIXED constant.
        for _ in 0..5 {
            let r = warm.reconstruct_context(4, 4);
            assert!(r.prefix_cache_hit, "stable prefix must hit after the first wake");
        }
        assert!(warm.cache.rate() > 0.8, "cache-hit rate must be observable and high: {}", warm.cache.rate());
        assert!(warm.cache.log_line().contains("prefix_hit_rate="), "rate is emittable to logs");
        let _ = std::fs::remove_dir_all(&here);
    }

    // ---- stable prefix is FROZEN (cache correctness + cross-impl contract) ----

    #[test]
    fn stable_prefix_is_frozen() {
        // The prefix must not drift wake-to-wake; pin its identity + key invariant clauses.
        assert!(STABLE_PREFIX.starts_with("[PECRT continuous reasoning thread — stable context prefix v1]"));
        assert!(STABLE_PREFIX.contains("SCHEDULER and MEMORY wrapper, not an"));
        assert!(STABLE_PREFIX.contains("cannot edit the repos.json whitelist/tiers"));
        assert!(STABLE_PREFIX.contains("Parking is PREFERRED"));
        assert!(STABLE_PREFIX.ends_with("[working context follows]\n"));
    }

    #[test]
    fn read_last_lines_returns_correct_suffix_across_chunk_boundaries() {
        let dir = tmp_dir("tail");
        let p = dir.join("big.jsonl");
        // Write far more than one 8KiB chunk so the reverse-seek must span multiple chunks.
        let mut body = String::new();
        for i in 0..5000 {
            body.push_str(&format!("line {i} with some padding to exceed a single chunk boundary\n"));
        }
        std::fs::write(&p, &body).unwrap();
        // last 3 lines, in order.
        let tail = read_last_lines(&p, 3);
        assert_eq!(tail.len(), 3);
        assert!(tail[0].starts_with("line 4997"));
        assert!(tail[2].starts_with("line 4999"));
        // n larger than the file line count returns all lines, still ordered.
        let all = read_last_lines(&p, 100_000);
        assert_eq!(all.len(), 5000);
        assert!(all[0].starts_with("line 0"));

        // file WITHOUT a trailing newline: last line still returned.
        let p2 = dir.join("no_trailing.txt");
        std::fs::write(&p2, "a1\nb2\nc3").unwrap();
        assert_eq!(read_last_lines(&p2, 2), vec!["b2".to_string(), "c3".to_string()]);
        // n == 0 and missing file => empty.
        assert!(read_last_lines(&p2, 0).is_empty());
        assert!(read_last_lines(&dir.join("nope"), 5).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn long_term_adapter_is_read_only_by_contract() {
        const { assert!(LONG_TERM_ADAPTER_READONLY) };
        // the adapter exposes only readers — a compile-time guarantee (no write method exists).
        let a = LongTermAdapter::new(tmp_dir("ro"), "kairos");
        assert!(a.outcomes_path().ends_with("outcomes.jsonl"));
        assert!(a.freshness_path().to_string_lossy().contains("kairos"));
    }
}
