# Maki improvement backlog

Ranked roadmap toward the north star: a **full-stack, AI-powered media app** on `pi` + a
lightweight local model — where the local LLM has **full control** to find, download,
organize, and **display** media, merging publicly-available APIs. Today it's manga →
Kindle; the direction is in-app reading (and eventually watching) of media pulled from
open sources. The improver picks the top unchecked item each run (or a clear bug/cleanup
it finds). Keep items small enough to ship + test in one iteration; split anything bigger.

## Near-term (high leverage, low risk)

- [x] Persist `last_search` results with a small cap + prune old entries (shipped via PR #1).
- [x] Add a `maki_cli.py` `version` command and surface the app version in the UI footer (shipped via PR #2).
- [x] Downloader: validate image bytes by magic number (JPEG/PNG/WebP) before counting a page done.
- [x] Settings: a "reading direction" toggle (RTL/LTR) plumbed through KCC device options.
- [x] jobstore: make `update_job` safe across the app + the detached job_runner (file lock or single-writer).
- [x] Graceful "no model / Ollama down" messaging in the assistant path with a retry hint.
- [x] More unit coverage for `kindle.py` drive-scoring and `convert.py` device presets (mocked).

## Mid-term (toward the full-stack AI media app)

- [x] **Display the manga in-app**: a simple paged reader for a downloaded series (the LLM
      can already `open` a series; let it actually show pages in the window).
- [x] **Give the LLM full control**: keep growing its command surface (have `resume`/`open`;
      add `delete`, `follow`, `recommend`) so it can manage the whole library by chat.
- [x] **Merge another public API**: add a non-manga source behind `sources.py` (e.g. a shows/
      anime metadata API) — the first step toward "watching"/broader media consumption.
- [x] In-app reader: open a downloaded EPUB/series in a simple paged viewer.
- [x] Series detail page: cover, description, chapter count, per-volume download/transfer.
- [x] Tagging / collections and a search box over the local library.
- [x] Additional sources behind the existing `sources.py` interface (keep it pluggable).
- [x] Resume/repair: detect partial/corrupt chapters and re-fetch only the missing pages.
- [x] Background auto-update: check followed series for new chapters and queue them.

## Longer-term (AI-powered)

- [x] Assistant-driven recommendations from the local library ("what should I read next").
- [x] Natural-language library queries routed through the local model + `maki_cli`.
- [x] Smart bundling/volume-splitting tuned to device screen + file-size budget.

## Quality / infra (only when a product item isn't clearly higher-leverage)

- [x] Replace remaining bare `except Exception` with logged, specific handling.
- [x] Add `ruff`/format config and a lint step to CI.
- [x] Expand the e2e smoke to a mocked download→convert path.
- [ ] Library view: sort options (recently added / title / size) and a series count header.  (deferred: agent could not implement after repeated tries)
