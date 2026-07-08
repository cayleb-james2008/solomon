# kairos novel-signal search — self-improvement contract

kairos is a Python 3.12 Kalshi trading system (`C:\Users\Cayleb\Desktop\workspace\projects\kairos`).
Its calibrated fair-value model family is EXHAUSTED — every tested strategy class is DEAD on this
venue (2c spreads, calibrated tails, informed flow; see `docs/STRATEGY_SCOREBOARD.md`). This lane is
the Solomon RSI **novel-signal search**: a long-horizon hunt for a NEW signal family that could
survive live-book adverse selection, driven cycle-after-cycle through a durable hypothesis ledger —
without gambling capital, gaming the fitness, or re-litigating a dead class.

## The engine (one cycle per run)

Run the novel-signal search cycle:

```
cd C:\Users\Cayleb\Desktop\workspace\projects\kairos
.venv\Scripts\python tools\novel_signal_search.py --ledger ..\..\solomon\improver\kairos\hypothesis-ledger.md
```

That single command does, in order (see the module docstring): **ORIENT** (parse the DEAD list from
the scoreboard + load the ledger) -> **FITNESS** (sizing_source-truthful settled PnL, D1 haircut
applied, BEFORE any promote signal) -> **PROPOSE** (one novel, non-DEAD class from the untested seed
set) -> **GATE** (write a ZERO-capital paper lane + a pre-registered pass/fail gate to the ledger).

- `--fitness` prints ONLY the sizing-truthful + haircut fitness read.
- `--dry-run` runs the cycle without writing the ledger.
- `--selfcheck` proves the guard/fitness/ledger discipline offline (no real .state touched).

## The four hard rails (NEVER weaken — enforced by the ultra-skeptic)

1. **DEAD-list prohibition.** Never propose a class marked DEAD / REAL-but-UNUSABLE / REFUTED in
   `docs/STRATEGY_SCOREBOARD.md`. The tool hard-rejects them; if you hand-propose, it still must
   clear `is_dead_class`. The DEAD list (do not re-litigate): fair-value TAKER all TFs; fair-value
   MAKER mid-range; fair-value MAKER debias; book-mid symmetric MM; ladder structural arb;
   cross-series stat-arb lead-lag; within-asset momentum/reversion; perps funding-carry; weather
   running-max lock-in; and REAL-but-UNUSABLE deep-favorite 90c+ NO tail (refuted 0/3, wf_2d24fc07).
2. **Sizing-truthful fitness.** Judge edge ONLY on settled rows with a recognized `sizing_source`.
   The legacy null-sizing_source rows (`mode='paper_legacy'`) fake ~+$176 of profit — they are
   POISON and must be excluded (mirrors `promote.py:paper_metrics`, `report.py:_sized_filter`).
3. **D1 adverse-selection haircut BEFORE any promote signal.** Paper maker roi is an OPTIMISTIC
   upper bound; `.state/adverse_fill_haircut.json` degrades it toward live. No promote signal is
   emitted on a raw paper number.
4. **Blast-radius / triple gate.** Every hypothesis starts as a ZERO-capital paper lane. Arming
   live requires the UNTOUCHED triple gate (`--live-*-class` flag + `.state/<LANE>_LIVE` + the
   in-file `_LIVE_DEFAULTS` hard leash) AND `promote.py`'s evidence ladder. This lane can PROPOSE,
   never arm. Never create a `*_LIVE` flag, never write a leash, never touch `.state/KILL`.

## Rules

- Do NOT run `git`, `gh`, or any push/merge — the runner owns version control.
- READ-ONLY over kairos runtime state. The search tool's ONLY write is the ledger file above.
- Do NOT edit the oracle: `docs/STRATEGY_SCOREBOARD.md`, `tools/fitness.py`, `promote.py`,
  `test_tracking.py`, `backtest.py`, `report.py`, `config.json` leash keys, or anything in `.state/`.
- One coherent change per run; add/keep a regression test in `tests_rsi.py` (the agent-writable
  test surface — `test_tracking.py` is the write-protected grader).
- HONESTY FLOOR: claim progress/profit ONLY on settled/live evidence. When the untested space is
  exhausted, the tool RAISES a `SearchError` (an honest wall) — escalate to the operator; do not
  fabricate activity or re-propose a dead class.

## Gate

```
.venv\Scripts\python test_tracking.py       # must print "Ran N tests"; all PASS
.venv\Scripts\python tools\novel_signal_search.py --selfcheck   # 20/20 checks
```

## Map of the relevant code

- `tools/novel_signal_search.py` — THIS lane's engine (proposer/screener/ledger writer; read-only).
- `docs/STRATEGY_SCOREBOARD.md` — the canonical DEAD list (oracle; read-only to the loop).
- `.state/adverse_fill_haircut.json` — the D1 haircut curve (read-only).
- `.state/kairos.db` — settled trades (read-only, mode=ro).
- `promote.py` / `tools/fitness.py` — the leash ladder + backtest needle (protected oracle).
- `maker.py:select_engine` / `MAKER_LIVE_DEFAULTS` — the triple live gate reference implementation.
- `improver/kairos/hypothesis-ledger.md` — the durable hypothesis ledger this lane appends to.
