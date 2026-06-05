# Token-savings stats (opt-in)

**Date:** 2026-06-03  <!-- last worked on (or created); rename on meaningful revisit -->
**Prior:** plans/2026-06-02-self-update.md (Phase B — self-update)
**Status:** in progress — Q1–Q3 + project_path provenance all DECIDED; S1+S2 landed (2026-06-05); S3 next.

## Goal
Let a user *see* what toonfmt buys them: an **opt-in** readout of how many bytes/tokens the TOON
transform saved. Models the same instinct as RTK's `gain` analytics (cumulative savings the user
values), adapted to this proxy. Opt-in because the default path must stay a silent, zero-overhead
passthrough.

## The thinking (settle these BEFORE writing tasks)
This is filed as a design note, not a ready plan, because three correctness questions decide the
whole shape:

1. **Report bytes + %, NOT a token figure. DECIDED 2026-06-04.** Anthropic's tokenizer is not
   public, so a precise per-call token count is impossible locally. **Decision:** report **exact
   bytes saved** and **% saved (`Σdelta / Σoriginal`, measured in bytes)** — and *no* token number
   at all. Rejected the bytes/4 `≈tokens` figure outright: an exact-bytes readout already conveys
   the win, and a fabricated token integer (even `≈`-labelled) *looks* tokenizer-derived when it
   isn't — adds a false-precision number rather than information. % is also a more robust estimator
   than absolute tokens (`bytes_saved/baseline` ≈ `tokens_saved/baseline_tokens` because the
   TOON-sparse vs. JSON-dense punctuation bias largely cancels in the ratio). A real BPE tokenizer
   is the wrong trade — it'd approximate a *different* model's tokenizer and break the single-binary
   ethos for marginal gain. Wording: "saved 12.6 KB (41%) of the JSON the model would have read."
2. **Count only savings DELIVERED to the model, per-block, signed. DECIDED 2026-06-04.** The
   transform's value depends on the `structuredContent` shadowing rule (ARCHITECTURE.md:34-45):
   - **strip path** (content TOON'd, redundant `structuredContent` removed) → the model now reads
     TOON instead of the JSON it would have read → **real delivered saving**. ✓
   - **content-only** (no `structuredContent`) → TOON content *is* what's read. ✓
   - **`structuredContent` kept** (not structurally equal) → the model reads `structuredContent`
     regardless; TOONing the content block saved *nothing the model sees* → contributes **zero**.
   Three pinned subtleties, or the number lies:
   - **Measure per-block content bytes, NOT the on-wire line delta.** `transform.rs:82` re-serializes
     the *whole envelope* compact (`serde_json::to_string`), incidentally minifying JSON-RPC
     plumbing whitespace the model never ingests (the host strips the wrapper). Counting line-length
     delta would conflate that with the TOON win. Measure only the transformed block's text region.
   - **Deltas are SIGNED — TOON can grow a block.** For small/non-tabular payloads (no
     array-of-uniform-objects to collapse) TOON can be *longer* than compact JSON. The aggregate
     sums signed per-block deltas; only counting wins would overstate %. (An honest readout may even
     surface "N results grew" — keeps the metric trustworthy.)
   - **Baseline = original `content` block `text.len()`, captured before the `insert` at
     `transform.rs:53`** (option (i)). Exact wire bytes, no re-serialization guesswork. Chosen over
     (ii) `serialized(structuredContent)` — the latter is what the model *literally* would have read
     on the strip path, but re-serializing a `Value` won't byte-match the server's formatting, so
     it's numerically fuzzier. The equality gate already proved content ≡ structuredContent, so the
     content-text bytes are a faithful, *exact* stand-in. Every counted case uses exact received
     bytes; nothing is invented.
   - **Confirmed: no minify-without-TOON path exists for content blocks.** A block's `text` is only
     ever overwritten at `transform.rs:53`, only with TOON, only after a successful parse — so a
     block is either TOON'd or byte-preserved, never reformatted-but-not-TOON'd. The `minify` token
     in the codebase is the Phase-4 `--structured-content {keep|strip|minify}` flag (unimplemented),
     which acts on the `structuredContent` *field* the strip-default model never reads — irrelevant
     to the delivered metric.
   **Pinned metric:** for each transformed block on a *delivered path* (strip, or content-only),
   `delta = original_text_bytes − toon_bytes` (signed). Aggregate Σdelta and Σoriginal_bytes; report
   `Σdelta` bytes and `Σdelta / Σoriginal` %. Keep path and non-transformed blocks contribute zero.

   *Impl note (for S2, not now):* `transform.rs:53` discards the original length, so capture
   `text.len()` before the `insert`; the counter must be shared across the two `tokio::spawn` pump
   tasks (the `on_line` closure is `FnMut` → `Arc<atomic>` threads cleanly). Only *surfacing* is
   gated by `--stats`/`TOONFMT_STATS=1`; the default path stays zero-overhead.
3. **Where the numbers live — SQLite (WAL), append-only event rows. DECIDED 2026-06-04.** Goal is a
   **cumulative, per-project** readout (RTK-`gain` shape), so a queryable cross-session store, not a
   per-session stderr line. stdout stays inviolable (protocol channel); the store is on disk.
   - **Store: SQLite, WAL mode, `busy_timeout` set, `synchronous=NORMAL`.** One table, append one
     **event row per transformed `tools/call` result** (`saved_bytes`, `original_bytes`,
     `project_path`, `ts`); **never update-an-aggregate** (no read-modify-write race). `toonfmt
     stats` does `SELECT SUM(...) ... GROUP BY project_path` — aggregate-at-read. WAL serializes the
     N concurrent writers (one toonfmt process per `.mcp.json` server) and never blocks the reader;
     `busy_timeout` turns the rare lock collision into a sub-ms retry instead of `SQLITE_BUSY`.
   - **VALIDATED against RTK, not analogized (2026-06-04).** RTK faces our exact problem (per-command
     invocation, concurrent Claude Code sessions, multi-process writes) and its store is
     `~/Library/Application Support/rtk/history.db` — **SQLite in WAL mode** (confirmed: `-wal`/`-shm`
     sidecars present, header WAL), schema `commands(… saved_tokens, savings_pct, project_path …)`
     with a `(project_path, timestamp)` index, **append-only** (3,757 rows accumulated), and
     **no compaction / rotation / archival** at all (2 MB file, just keeps appending). Our design is
     the same shape. (`busy_timeout`/`synchronous` are per-connection, not readable from the file —
     RTK's exact values weren't inspected; we set our own sane WAL pairing regardless.)
   - **Turso (pure-Rust SQLite rewrite) examined and rejected on our constraints, not maturity
     (2026-06-05).** It's beta v0.6.0 with a *real, tested* multi-process WAL feature (purpose-built
     for our N-process case) — so an earlier "alpha" dismissal was wrong. But (1) that feature is
     **64-bit-Unix-only** and we ship `x86_64-pc-windows-msvc` → zero multi-process safety on Windows
     → would force a per-platform storage split (more surface than the C dep saves); and (2) Turso
     warns its multi-process on-disk format is "not recommended for long-term storage across
     versions" — a direct collision with a cumulative-forever store. Reopenable only if Windows is
     dropped. rusqlite-bundled is uniform across all four targets and format-stable.
   - **No compaction needed — explicitly ruled out.** The sharded-NDJSON alternative was rejected
     precisely because its only irreducible complexity (per-process shard files → flock-for-liveness
     + absorbed-set + create-race mtime-grace + atomic-archive compaction) exists *solely* to clean
     up files sharding itself creates. SQLite creates zero extra files, so that whole problem
     category evaporates — RTK proves a single growing file is a non-issue at this scale.
   - **Cost accepted: a 2nd C dependency** (`rusqlite` with the bundled feature → static-links the
     SQLite amalgamation, needs a C compiler at build, ~1 MB). The project already takes C
     transitively via aws-lc-rs; this is a second independent C entry point. Accepted deliberately:
     one contained build-time cost vs. owning bespoke compaction code forever. Single-binary ethos
     is preserved (bundled = no runtime dep; `.mcp.json` story unchanged).
   - **Write model (rides on top, unchanged from the file-based sketch):** an **async bounded MPSC
     queue** — pump tasks `try_send` a delta event per tool-call completion (drop-on-full + bump a
     `dropped` counter, **never** `send().await` on the hot path), a dedicated writer task drains it
     and INSERTs. No per-call `fsync` (a process kill keeps OS-buffered writes; only power-loss loses
     page cache — fine for a stats readout). So an abrupt window-quit costs at most the few in-queue
     events, never a session — which is why this beats flush-on-exit and **retires the deferred
     "measure how Claude Code kills the subprocess" question entirely** (durability no longer depends
     on the teardown signal).
   - **Caveat (document, don't engineer around):** if `~/.toonfmt` (or the chosen data dir) lives on
     an NFS mount, WAL's shared-memory locking softens. Home-dir-on-NFS is rare (corp/academic);
     note it, don't build for it.

## Where the data already is (no new plumbing to measure)
`transform::tools_call_result` (`src/transform.rs:46-53`) already holds, in one place, the original
block `text` and the new `toon` string, **and** knows whether it stripped `structuredContent`
(`should_strip`, line 58-72). So the raw deltas are computable at the existing transform site with
no architectural change — the work is *accounting and surfacing*, not *capturing*.

## Tasks
- [x] **S1 — Decide the open questions. DONE 2026-06-04.** Q1 (bytes+%, no token figure), Q2
  (per-block signed delivered-only delta, baseline (i)), and Q3 (SQLite/WAL append-only event rows,
  async bounded-queue writer, no compaction, 2nd C dep accepted) all decided — see above. The
  "measure Claude Code teardown signal" probe is **retired** (the async-queue write model makes
  durability independent of how the process dies).
- [x] **S2 — Measurement at the transform site. DONE 2026-06-05.** `tools_call_result` now returns
  `Option<(String, Savings)>`; `Savings { original_bytes: u64, saved_bytes: i64 }` (signed,
  delivered-only). Original bytes captured as `text.len()` **before** the `insert`; delivered-ness
  resolved post-strip as `!result_obj.contains_key("structuredContent")` (a kept, unequal SC shadows
  the TOON → `Savings::default()`). Proxy site discards via `.map(|(r,_)| r)` — pure accounting, no
  IO/DB/flag. 5 new tests (s1–s5): content-only exact, **signed-negative** (empirically-grounded
  non-uniform array, TOON > JSON), strip-path delivered, kept-SC zero, multi-block signed sum. Full
  suite 113 passed; clippy `-D warnings` clean (complexity gate not tripped). cairn-verify: PASS.
- [x] **S3 — Stats store + async writer (SQLite/WAL). DONE 2026-06-05.** `rusqlite` 0.40 (bundled)
  added (one-time amalgamation compile absorbed). New `src/stats.rs`: `Stats::open(base_dir,
  project_path)` opens/creates the WAL DB (`PRAGMA journal_mode=WAL; synchronous=NORMAL`,
  `busy_timeout=5s`), append-only `events(ts, project_path, original_bytes, saved_bytes)` + a
  `(project_path, ts)` index. Bounded `tokio::mpsc` (cap 1024) + one `spawn_blocking` writer task
  owning the single `Connection` (not `Sync` → confined; channel is the synchronization, no mutex).
  `StatsHandle::record` `try_send`s — drop-on-full + `dropped` counter (logged at shutdown), and
  **self-gates the not-delivered zero case** (`Savings::default()` → no row), so the store holds
  delivered events only with each row's signed delta intact. `Stats::shutdown` drops the template
  sender and joins → all buffered rows flush. **`project_path` provenance: `$CLAUDE_PROJECT_DIR`,
  read once at serve startup** (process-stable — one toonfmt process serves one project; verified in
  env per memory `claude-code-mcp-subprocess-env`), fallback `std::env::current_dir()` then `""`. NOT
  RTK's per-invocation cwd model (RTK is per-command; toonfmt is a long-lived subprocess), and
  decoupled from `--profile`/OAuth. Wiring: `transform_downstream` takes `Option<&StatsHandle>` and
  records once (transport-blind — both stdio pump + HTTP driver); `--stats` serve flag (OR'd with
  `TOONFMT_STATS=1`); the single opt-in seam is `Stats::open_if_enabled(enabled, project_path)` —
  **off ⇒ returns `None`, touches nothing** (no dir, no DB, no channel, no task), and an open failure
  degrades to `None` (logged) rather than faulting the proxy. 11 new tests (8 stats: drain-N→N +
  summed, signed-negative round-trip, zero-default-skip, try_send drop-on-full never blocks,
  two-writer WAL concurrency + per-project grouping, journal_mode=wal, **disabled-gate-creates-
  nothing**, enabled-open-failure-degrades; 3 cli: `--stats` default-off / both-forms /
  position-independent). Full suite 124 passed; clippy `-D warnings` clean; e2e-smoke confirmed a
  real `--stats` serve writes the row and the off path creates no store dir. cairn-verify: PASS. (No
  cairn-accept — the slice is not user-visible until S4's `toonfmt stats` readout.)
- [ ] **S4 — `toonfmt stats` readout.** New subcommand (parser in `cli.rs`, alongside `login`/
  `update`): print exact bytes + % saved per project and a total, to stdout (this is a user command,
  not the proxy path — stdout is fine here). Honest formatting: bytes + %, no token figure.
  **Four S3-derived constraints (locked 2026-06-05 — none touch the core `SUM … GROUP BY
  project_path` aggregate, all are about how the reader touches the store):**
  1. **Read-only, no-create open.** rusqlite's default `Connection::open` sets `SQLITE_OPEN_CREATE`,
     so a naive reader run before the user ever served with `--stats` would *create* an empty
     `~/.toonfmt/stats.db` as a side effect. `toonfmt stats` must be side-effect-free → open with
     `OPEN_READ_ONLY`, treating the not-found error as the empty-state signal (#2).
  2. **Graceful "no stats yet", not a SQLite error.** Because `open_if_enabled` only creates the dir
     when stats are *on*, a user who never opted in has no DB. Print e.g. `no stats recorded yet —
     serve with --stats or TOONFMT_STATS=1` and exit 0; never surface "unable to open database".
  3. **Reader lives in `stats.rs`, not re-derived in `cli.rs` (DRY).** S3 made `STORE_DIR`,
     `DB_FILE`, and `home_store_dir()` *private* to `stats.rs`. Add a `stats::read_summary()` that
     owns the read-only open + the `GROUP BY` query and returns a plain struct (per-project rows +
     totals); `cli.rs` only formats it. The S3 test helper `read_totals()` already prototyped the
     exact query against the committed schema — promote it, don't re-spell the path.
  4. **"N results grew" = COUNT of negative-delta rows, NOT gated on the project's net sign.**
     **DECIDED 2026-06-05.** S3 stores *row-level* sign — delivered-but-grew blocks are kept with a
     negative `saved_bytes` (only the all-zero not-delivered sentinel is dropped). So a project can
     net *positive* yet still contain blocks that grew. Report `COUNT(*) WHERE saved_bytes < 0` per
     project, regardless of the total's sign (supersedes the earlier "if Σ over a project is
     negative" trigger — that would hide grew-rows inside a net-positive project).
  Multi-project fixture test (drive `read_summary` over a seeded multi-project DB: per-project bytes
  + %, the grew-count, and the file-absent → empty-state path).
- [ ] **S5 — Docs + cairn-accept.** ARCHITECTURE.md (the stats store + the C-dep decision + NFS
  caveat), README (`--stats`/`TOONFMT_STATS` + `toonfmt stats`), memory note for the RTK-validated
  SQLite/WAL decision. Run cairn-accept.

## Acceptance criteria
- Default path (no opt-in) is byte-for-byte and behaviorally identical to today — zero overhead,
  nothing on stdout, **no DB opened, no channel created, no rows written**.
- Reported byte savings are exact and % is bytes-based; **no token figure is presented** (an
  exact-bytes readout conveys the win without fabricating a tokenizer-derived number).
- The metric counts only savings the model actually ingests (strip path + content-only), not
  transforms whose TOON a kept `structuredContent` shadows — verified against the keep/strip tests.
- Per-block deltas are **signed** (a TOON block that grew counts negative); the aggregate is not
  win-only. Measured per-block on content-text bytes, never the whole-envelope line delta.
- Store is SQLite/WAL, append-only event rows (no update-an-aggregate); concurrent writers from
  multiple toonfmt processes are safe via WAL + `busy_timeout`. `toonfmt stats` aggregates at read
  with a per-project breakdown.
- `toonfmt stats` is **side-effect-free**: it opens the store read-only and never creates it, so
  running it before any `--stats` serve prints a graceful "no stats yet" (exit 0), not a DB error.
  "N results grew" counts negative-delta rows per project, independent of the project's net sign.
- The stats writer never blocks or slows the proxy hot path (bounded queue, `try_send`, drop-on-full)
  and never `fsync`s per call; an abrupt process kill loses at most the in-queue events, never the
  store.
- `cargo test` + `cargo clippy -- -D warnings` green.
