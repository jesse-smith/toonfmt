//! Opt-in token-savings stats store: an append-only SQLite (WAL) event log of the
//! bytes the TOON transform saved, written off the proxy hot path.
//!
//! **Why this shape (decided in `plans/2026-06-03-token-savings-stats.md`):**
//! - **SQLite, WAL, append-one-row-per-delivered-result, aggregate-at-read.** One
//!   `toonfmt` process runs per `.mcp.json` server, so the store is inherently
//!   written by N concurrent processes. WAL serializes those writers and never
//!   blocks the reader; `busy_timeout` turns the rare lock collision into a sub-ms
//!   retry instead of `SQLITE_BUSY`. We never update-an-aggregate (no read-modify-
//!   write race) — `toonfmt stats` (S4) does `SUM(...) GROUP BY project_path`.
//! - **Async bounded MPSC + one writer task.** The hot path `record`s a [`Savings`]
//!   via **`try_send`** — never `send().await`, never `fsync` per call. A dedicated
//!   writer task owns the single rusqlite [`Connection`] (which is not `Sync`, so it
//!   is confined to that one task; the channel *is* the synchronization — no mutex).
//!   The bound is **OOM-insurance against a wedged writer** (e.g. an NFS hang), not
//!   burst protection: the producer is model-inference-rate-limited (~1–10/s) and a
//!   local WAL INSERT is microseconds, so steady-state queue depth is ~0. On a
//!   permanent wedge a bounded queue sheds only stats rows (the least-important
//!   data) and never harms the proxy; an unbounded queue would OOM-kill it.
//! - **Delivered-only.** [`StatsHandle::record`] drops the all-zero
//!   [`Savings::default`] sentinel — which is exactly S2's not-delivered / shadowed-
//!   `structuredContent` case — so the store holds only results the model actually
//!   ingested, and each row's signed `saved_bytes` (a TOON block *can* grow) is
//!   preserved for an honest "N results grew" readout.
//!
//! **Gating is the caller's job.** This module has no flag awareness: when stats are
//! off, the proxy simply never constructs a [`Stats`], so no channel, no writer task,
//! and no DB file exist — the default path stays byte-for-byte zero-overhead.
//!
//! **Injectable base dir:** [`Stats::open`] takes the base directory so tests drive a
//! throwaway tempdir; [`Stats::open_if_enabled`] resolves the production `~/.toonfmt/`
//! from `$HOME` (mirroring `credential_store`'s convention). The S4 reader
//! [`read_summary`] follows the same split ([`read_summary_in`] takes the base dir).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow};
use rusqlite::{Connection, OpenFlags, params};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::transform::Savings;

/// Directory name under `$HOME` for the production store. Distinct from
/// `credential_store`'s `.toonfmt-auth` — stats are not secrets and live apart.
const STORE_DIR: &str = ".toonfmt";
/// The stats DB filename under the store directory.
const DB_FILE: &str = "stats.db";

/// Bounded queue capacity. Sized for OOM-insurance, not burst (see module docs):
/// the producer can't realistically fill it, so this only caps memory if the writer
/// wedges. 1024 events ≈ a few tens of KB of `Savings`, trivially cheap to reserve.
const QUEUE_CAP: usize = 1024;

/// `busy_timeout` for the writer connection: a lock collision (another `toonfmt`
/// process mid-commit) retries for up to this long before surfacing `SQLITE_BUSY`.
/// Generous because a stats write is never latency-critical.
const BUSY_TIMEOUT_MS: u32 = 5_000;

/// One-shot schema + pragma init, run on the writer connection at open. WAL +
/// `synchronous=NORMAL` is the durable-enough/fast pairing for a stats log (a
/// process kill keeps OS-buffered writes; only power loss drops the page cache —
/// fine here). `busy_timeout` is set separately via the typed API.
const INIT_SQL: &str = "\
    PRAGMA journal_mode=WAL;\
    PRAGMA synchronous=NORMAL;\
    CREATE TABLE IF NOT EXISTS events (\
        ts             INTEGER NOT NULL,\
        project_path   TEXT    NOT NULL,\
        original_bytes INTEGER NOT NULL,\
        saved_bytes    INTEGER NOT NULL\
    );\
    CREATE INDEX IF NOT EXISTS idx_project_ts ON events(project_path, ts);";

/// Owns the writer task and the channel into it. Construct once per serve session
/// (only when stats are enabled); hand [`StatsHandle`] clones to the proxy via
/// [`Stats::handle`]; call [`Stats::shutdown`] at teardown to flush and join.
pub struct Stats {
    handle: StatsHandle,
    writer: JoinHandle<()>,
}

/// A cheap, `Clone`able recorder handed to the proxy hot path. Holds the sending end
/// of the writer channel plus a shared dropped-event counter.
#[derive(Clone)]
pub struct StatsHandle {
    tx: mpsc::Sender<Savings>,
    /// Count of events shed because the queue was full (a wedged writer). Surfaced
    /// for diagnostics; never affects the proxy.
    dropped: Arc<AtomicU64>,
}

impl Stats {
    /// Open (creating if absent) the WAL stats DB under `base_dir` and spawn the
    /// writer task, which stamps every row with `project_path`. Must be called from
    /// within a tokio runtime (it `spawn_blocking`s the writer).
    pub fn open(base_dir: impl Into<PathBuf>, project_path: String) -> Result<Stats> {
        let base_dir = base_dir.into();
        std::fs::create_dir_all(&base_dir)
            .with_context(|| format!("creating stats dir {}", base_dir.display()))?;
        let db_path = base_dir.join(DB_FILE);
        let conn = Connection::open(&db_path)
            .with_context(|| format!("opening stats db {}", db_path.display()))?;
        conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS as u64))
            .context("setting stats db busy_timeout")?;
        conn.execute_batch(INIT_SQL)
            .context("initializing stats db schema/pragmas")?;

        let (tx, rx) = mpsc::channel::<Savings>(QUEUE_CAP);
        // The writer owns `conn` for its whole life — `Connection` is not `Sync`, so
        // confining it to this one blocking task is what makes the mutex-free design
        // sound. `spawn_blocking` keeps the synchronous SQLite calls off the async
        // worker threads.
        let writer = tokio::task::spawn_blocking(move || writer_loop(conn, rx, project_path));

        Ok(Stats {
            handle: StatsHandle {
                tx,
                dropped: Arc::new(AtomicU64::new(0)),
            },
            writer,
        })
    }

    /// The single opt-in seam. When `enabled` is false this returns `None` **without
    /// touching the filesystem** — no directory, no DB file, no channel, no writer
    /// task — which is the load-bearing zero-overhead-off guarantee. When true it
    /// opens the production store under `~/.toonfmt/`; a failure to open (e.g. `$HOME`
    /// unset, unwritable dir) is logged and degrades to `None` rather than failing the
    /// proxy — stats are a perk, the passthrough is the contract.
    pub fn open_if_enabled(enabled: bool, project_path: String) -> Option<Stats> {
        Self::open_gated(enabled, Self::home_store_dir(), project_path)
    }

    /// Resolve the production base dir `~/.toonfmt/` (mirrors `credential_store`'s
    /// `$HOME` convention). `Err` if `$HOME` is unset — surfaced through the gate's
    /// degrade-to-`None` path, never panics.
    fn home_store_dir() -> Result<PathBuf> {
        let home = std::env::var_os("HOME")
            .ok_or_else(|| anyhow!("$HOME is not set; cannot locate the stats store"))?;
        Ok(Path::new(&home).join(STORE_DIR))
    }

    /// Gate + degrade core, with the base dir injected so tests can assert the
    /// off-path creates nothing under a tempdir. `base_dir` is a `Result` so a failed
    /// `$HOME` resolution is only consulted on the enabled path (disabled never looks).
    fn open_gated(enabled: bool, base_dir: Result<PathBuf>, project_path: String) -> Option<Stats> {
        if !enabled {
            return None; // off → nothing happens, byte-for-byte zero overhead
        }
        match base_dir.and_then(|dir| Self::open(dir, project_path)) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "stats store disabled (open failed)");
                None
            }
        }
    }

    /// A recorder clone for the hot path.
    pub fn handle(&self) -> StatsHandle {
        self.handle.clone()
    }

    /// Drop our template sender and await the writer. Once every [`StatsHandle`]
    /// clone the caller handed out is also dropped, the channel closes, the writer
    /// drains the remaining buffered events, INSERTs them, and exits — so all
    /// enqueued (non-dropped) events are flushed before this returns.
    pub async fn shutdown(self) {
        let Stats { handle, writer } = self;
        // Surface any shed events (a wedged writer hitting the queue bound). Read via
        // the shared `Arc` counter before dropping our template handle; normally 0.
        let dropped = handle.dropped();
        if dropped > 0 {
            tracing::warn!(dropped, "stats: dropped events (writer could not keep up)");
        }
        drop(handle); // close our end; writer sees `None` once all clones are gone too
        if let Err(e) = writer.await {
            tracing::warn!(error = %e, "stats writer task did not join cleanly");
        }
    }
}

impl StatsHandle {
    /// Record one transformed `tools/call` result's delivered savings. Non-blocking
    /// by construction (`try_send`):
    /// - all-zero [`Savings::default`] (S2's not-delivered / shadowed case) → skip;
    /// - queue full (writer wedged) → drop + bump the `dropped` counter;
    /// - channel closed (post-shutdown) → ignore.
    ///
    /// Never `await`s, never blocks the proxy.
    pub fn record(&self, savings: Savings) {
        if savings == Savings::default() {
            return; // nothing delivered to the model → no row
        }
        match self.tx.try_send(savings) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // Writer has shut down; the proxy may still emit a few results during
                // teardown. Silently ignore — the store is closed.
            }
        }
    }

    /// Count of events shed due to a full queue (diagnostics; 0 in normal operation).
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// The writer task body: block on the channel, INSERT each event stamped with the
/// session's `project_path`, exit when the channel closes. A failed INSERT is logged
/// and dropped — a stats write must never escalate into a proxy fault.
fn writer_loop(conn: Connection, mut rx: mpsc::Receiver<Savings>, project_path: String) {
    while let Some(s) = rx.blocking_recv() {
        // `original_bytes: u64` → store as i64 (SQLite has no unsigned integer;
        // rusqlite's `ToSql` is i64-based). A tool result is never near i64::MAX
        // bytes, so the cast is lossless in practice.
        let res = conn.execute(
            "INSERT INTO events (ts, project_path, original_bytes, saved_bytes) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                now_unix(),
                project_path,
                s.original_bytes as i64,
                s.saved_bytes
            ],
        );
        if let Err(e) = res {
            tracing::warn!(error = %e, "stats: INSERT failed; dropping event");
        }
    }
}

/// Current unix time in seconds, saturating to 0 if the clock is before the epoch
/// (cannot happen on a sane host; avoids an unwrap).
fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ============================================================================
// S4 — read side: `toonfmt stats` aggregates the append-only log at read time.
// ============================================================================

/// One project's aggregated savings (the per-`project_path` GROUP BY row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSummary {
    /// The `$CLAUDE_PROJECT_DIR` the writer stamped (may be `""` if it was unset).
    pub project_path: String,
    /// Number of delivered, recorded results (rows) for this project.
    pub results: i64,
    /// Σ of the original `content` block bytes the model would have read as JSON.
    pub original_bytes: i64,
    /// Σ of signed per-block deltas (`original − toon`). Net bytes saved; **can be
    /// negative** if TOON grew more blocks than it shrank for this project.
    pub saved_bytes: i64,
    /// Count of rows whose `saved_bytes < 0` — results the transform *grew*. Reported
    /// independently of `saved_bytes`'s sign (a net-positive project can still hold
    /// grew-rows); per the locked S4 decision, this is the honest "N grew" figure.
    pub grew_results: i64,
}

impl ProjectSummary {
    /// Percent of original bytes saved (`saved / original * 100`), or `0.0` when there
    /// is no baseline (no original bytes ⇒ no meaningful ratio). Signed: a net-grown
    /// project reads negative.
    pub fn saved_pct(&self) -> f64 {
        if self.original_bytes <= 0 {
            0.0
        } else {
            self.saved_bytes as f64 / self.original_bytes as f64 * 100.0
        }
    }
}

/// The whole-store readout: every project's row plus the grand totals. An **empty**
/// `projects` vec is the "no stats yet" state (store absent or no delivered rows);
/// the caller turns that into a friendly message, never a DB error.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Summary {
    pub projects: Vec<ProjectSummary>,
}

impl Summary {
    /// True when nothing has been recorded — drives the graceful empty-state message.
    pub fn is_empty(&self) -> bool {
        self.projects.is_empty()
    }

    /// Grand totals across all projects: `(results, original_bytes, saved_bytes,
    /// grew_results)`. Summed in Rust over the already-aggregated rows (a handful of
    /// projects — no second query needed).
    pub fn totals(&self) -> (i64, i64, i64, i64) {
        self.projects.iter().fold((0, 0, 0, 0), |(r, o, s, g), p| {
            (
                r + p.results,
                o + p.original_bytes,
                s + p.saved_bytes,
                g + p.grew_results,
            )
        })
    }

    /// Total percent saved across all projects (Σsaved / Σoriginal). `0.0` with no
    /// baseline. Robust per Q1: the bytes ratio approximates the token ratio.
    pub fn total_saved_pct(&self) -> f64 {
        let (_, original, saved, _) = self.totals();
        if original <= 0 {
            0.0
        } else {
            saved as f64 / original as f64 * 100.0
        }
    }
}

/// Read the production store (`~/.toonfmt/stats.db`) and aggregate it. Resolves the
/// base dir exactly as the writer does; a `$HOME`-unset failure surfaces as an empty
/// summary (there can be no store without a home dir), not an error.
pub fn read_summary() -> Result<Summary> {
    match Stats::home_store_dir() {
        Ok(dir) => read_summary_in(&dir),
        // No `$HOME` ⇒ no store could ever have been written ⇒ empty state.
        Err(_) => Ok(Summary::default()),
    }
}

/// Aggregate the store under `base_dir` (injected for tests). **Side-effect-free and
/// read-only** — the two locked S4 invariants:
///
/// 1. **No-create open.** rusqlite's default [`Connection::open`] sets
///    `SQLITE_OPEN_CREATE`, so a reader run before any `--stats` serve would *create*
///    an empty DB as a side effect. We open `OPEN_READ_ONLY` (no create bit). As
///    belt-and-suspenders we also pre-check `exists()`: a missing file is the
///    empty-state signal (returned as `Summary::default()`), so we never even reach
///    the open for the common not-yet-opted-in case — and a genuinely *unreadable
///    existing* file still surfaces its error rather than masquerading as "no stats".
/// 2. **Aggregate-at-read.** One `GROUP BY project_path` over the append-only log;
///    the grew-count is `SUM(saved_bytes < 0)` per project, sign-independent.
pub fn read_summary_in(base_dir: &Path) -> Result<Summary> {
    let db_path = base_dir.join(DB_FILE);
    if !db_path.exists() {
        return Ok(Summary::default()); // never served with --stats → no store
    }
    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening stats db read-only {}", db_path.display()))?;
    query_summary(&conn)
}

/// The read query, factored out so tests can drive it against an in-memory or
/// tempdir connection. Rows are ordered by bytes saved descending (the biggest win
/// first — what the user came to see).
fn query_summary(conn: &Connection) -> Result<Summary> {
    let mut stmt = conn
        .prepare(
            "SELECT project_path, \
                    COUNT(*), \
                    COALESCE(SUM(original_bytes), 0), \
                    COALESCE(SUM(saved_bytes), 0), \
                    COALESCE(SUM(CASE WHEN saved_bytes < 0 THEN 1 ELSE 0 END), 0) \
             FROM events \
             GROUP BY project_path \
             ORDER BY SUM(saved_bytes) DESC",
        )
        .context("preparing stats summary query")?;
    let rows = stmt
        .query_map([], |r| {
            Ok(ProjectSummary {
                project_path: r.get(0)?,
                results: r.get(1)?,
                original_bytes: r.get(2)?,
                saved_bytes: r.get(3)?,
                grew_results: r.get(4)?,
            })
        })
        .context("querying stats summary")?;
    let projects = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("collecting stats summary rows")?;
    Ok(Summary { projects })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Throwaway base dir under the OS temp dir, removed on drop. Mirrors
    /// `credential_store`'s test helper (avoids a `tempfile` dep not in the offline
    /// cargo cache); the unique stem keeps concurrent tests from colliding.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(label: &str) -> Self {
            let p = std::env::temp_dir().join(format!("toonfmt-stats-test-{label}"));
            let _ = std::fs::remove_dir_all(&p);
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn saved(original: u64, delta: i64) -> Savings {
        Savings {
            original_bytes: original,
            saved_bytes: delta,
        }
    }

    /// `(count, Σsaved_bytes, Σoriginal_bytes)` from a fresh read connection on the
    /// same db file the writer used.
    fn read_totals(base_dir: &Path) -> (i64, i64, i64) {
        let conn = Connection::open(base_dir.join(DB_FILE)).unwrap();
        conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(saved_bytes), 0), COALESCE(SUM(original_bytes), 0) \
             FROM events",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    }

    /// Test-only constructor for an undrained channel + its receiver, so drop-on-full
    /// is deterministic (no live writer racing to empty it).
    fn channel_for_test(cap: usize) -> (StatsHandle, mpsc::Receiver<Savings>) {
        let (tx, rx) = mpsc::channel(cap);
        (
            StatsHandle {
                tx,
                dropped: Arc::new(AtomicU64::new(0)),
            },
            rx,
        )
    }

    /// The writer drains every enqueued event before exit: N records → exactly N rows,
    /// and the summed columns match. Shutdown flushes the buffer (no loss under cap).
    #[tokio::test]
    async fn writer_drains_all_events_and_sums_match() {
        let tmp = TempDir::new("drain");
        let stats = Stats::open(tmp.path(), "proj-a".to_string()).unwrap();
        let h = stats.handle();

        let n = 100i64; // well under QUEUE_CAP → nothing dropped
        let mut want_saved = 0i64;
        let mut want_original = 0i64;
        for i in 0..n {
            let s = saved(1000 + i as u64, 400 - i); // varied, some could be small
            want_saved += s.saved_bytes;
            want_original += s.original_bytes as i64;
            h.record(s);
        }
        let dropped = h.dropped(); // read before the drop below
        drop(h); // drop the clone so the channel can close on shutdown
        stats.shutdown().await; // drains + joins

        let (count, sum_saved, sum_original) = read_totals(tmp.path());
        assert_eq!(count, n, "every enqueued event must be persisted");
        assert_eq!(sum_saved, want_saved);
        assert_eq!(sum_original, want_original);
        // Under cap, nothing is shed (no-drop guarantee; full-queue drop is covered
        // explicitly by `record_drops_on_full_without_blocking`).
        assert_eq!(dropped, 0);
    }

    /// A negative `saved_bytes` (TOON grew the block) round-trips through SQLite as a
    /// signed value — the store must not coerce it to unsigned/zero.
    #[tokio::test]
    async fn negative_savings_round_trips_signed() {
        let tmp = TempDir::new("negative");
        let stats = Stats::open(tmp.path(), "proj-neg".to_string()).unwrap();
        let h = stats.handle();
        h.record(saved(10, -5)); // delivered, but TOON was larger
        drop(h);
        stats.shutdown().await;

        let (count, sum_saved, sum_original) = read_totals(tmp.path());
        assert_eq!(count, 1);
        assert_eq!(sum_saved, -5, "negative delta must survive the round-trip");
        assert_eq!(sum_original, 10);
    }

    /// The all-zero `Savings::default()` sentinel (S2's not-delivered / shadowed-
    /// `structuredContent` case) is dropped before it ever enqueues — no row.
    #[tokio::test]
    async fn zero_savings_default_is_not_recorded() {
        let tmp = TempDir::new("zero");
        let stats = Stats::open(tmp.path(), "proj-zero".to_string()).unwrap();
        let h = stats.handle();
        h.record(Savings::default()); // shadowed result: contributes nothing
        h.record(saved(0, 0)); // also the zero sentinel
        h.record(saved(50, 20)); // one real delivery
        drop(h);
        stats.shutdown().await;

        let (count, sum_saved, _) = read_totals(tmp.path());
        assert_eq!(count, 1, "only the one delivered event is stored");
        assert_eq!(sum_saved, 20);
    }

    /// `record` is non-blocking and drops-on-full: with an undrained, tiny-capacity
    /// channel, the first `cap` events buffer and the next `extra` are dropped (and
    /// counted), all without the synchronous `record` ever blocking.
    #[test]
    fn record_drops_on_full_without_blocking() {
        let cap = 4;
        let extra = 5;
        let (h, _rx) = channel_for_test(cap); // hold rx, never drain
        for _ in 0..(cap + extra) {
            h.record(saved(100, 10)); // returns immediately every time
        }
        assert_eq!(
            h.dropped(),
            extra as u64,
            "exactly the over-capacity events are dropped + counted"
        );
        // `_rx` still holds the first `cap` buffered events (channel not closed).
    }

    /// Two `Stats` instances (distinct connections) writing the *same* db file
    /// concurrently both land their rows — WAL + `busy_timeout` make multi-writer
    /// safe. Stands in for the real N-process case (one `toonfmt` per `.mcp.json`).
    #[tokio::test]
    async fn two_writers_share_one_db_via_wal() {
        let tmp = TempDir::new("concurrent");
        let a = Stats::open(tmp.path(), "proj-a".to_string()).unwrap();
        let b = Stats::open(tmp.path(), "proj-b".to_string()).unwrap();
        let (ha, hb) = (a.handle(), b.handle());

        for _ in 0..50 {
            ha.record(saved(200, 30));
            hb.record(saved(300, 40));
        }
        drop(ha);
        drop(hb);
        a.shutdown().await;
        b.shutdown().await;

        let (count, sum_saved, sum_original) = read_totals(tmp.path());
        assert_eq!(count, 100, "all rows from both writers persist");
        assert_eq!(sum_saved, 50 * 30 + 50 * 40);
        assert_eq!(sum_original, 50 * 200 + 50 * 300);

        // Per-project grouping (what S4 will read) sees both projects distinctly.
        let conn = Connection::open(tmp.path().join(DB_FILE)).unwrap();
        let projects: i64 = conn
            .query_row("SELECT COUNT(DISTINCT project_path) FROM events", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(projects, 2);
    }

    /// **Zero-overhead-off (load-bearing):** with the gate disabled, the factory
    /// returns `None` and creates **nothing** under the base dir — no directory, no
    /// DB file. This is the whole opt-in contract: the default path must not so much
    /// as touch the filesystem.
    #[test]
    fn disabled_gate_creates_nothing() {
        let tmp = TempDir::new("disabled");
        let dir = tmp.path().join("toonfmt-store"); // never created when off
        let stats = Stats::open_gated(false, Ok(dir.clone()), "p".to_string());
        assert!(stats.is_none(), "disabled → None");
        assert!(!dir.exists(), "disabled gate must not create the store dir");
        assert!(
            !dir.join(DB_FILE).exists(),
            "disabled gate must not create the db"
        );
    }

    /// Enabled but the base dir can't resolve / can't be created → the gate logs and
    /// degrades to `None`, never propagating an error that would fault the proxy.
    #[test]
    fn enabled_open_failure_degrades_to_none() {
        // A base dir *under a regular file* → create_dir_all cannot succeed.
        let tmp = TempDir::new("openfail");
        std::fs::create_dir_all(tmp.path()).unwrap();
        let blocker = tmp.path().join("iam-a-file");
        std::fs::write(&blocker, b"x").unwrap();
        let unusable = blocker.join("nested");

        let stats = Stats::open_gated(true, Ok(unusable), "p".to_string());
        assert!(
            stats.is_none(),
            "an open failure must degrade to None, not panic/propagate"
        );

        // Also: an unresolved base dir (the `$HOME`-unset analogue) degrades too.
        let stats = Stats::open_gated(true, Err(anyhow!("no home")), "p".to_string());
        assert!(stats.is_none());
    }

    /// WAL mode actually took effect on the writer connection (the pragma in
    /// `INIT_SQL` is load-bearing for multi-process safety).
    #[tokio::test]
    async fn journal_mode_is_wal() {
        let tmp = TempDir::new("walmode");
        let stats = Stats::open(tmp.path(), "p".to_string()).unwrap();
        stats.shutdown().await;
        let conn = Connection::open(tmp.path().join(DB_FILE)).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    // --- S4 read side: read_summary_in ---

    /// Seed a multi-project store by writing through real `Stats` instances (the same
    /// path production uses), so the reader is tested against the committed schema.
    async fn seed(base: &Path, events: &[(&str, u64, i64)]) {
        // Group by project so each project's rows go through one writer (mirrors the
        // one-process-per-project reality), then flush via shutdown.
        let mut by_project: std::collections::BTreeMap<&str, Vec<(u64, i64)>> = Default::default();
        for &(proj, orig, delta) in events {
            by_project.entry(proj).or_default().push((orig, delta));
        }
        for (proj, rows) in by_project {
            let stats = Stats::open(base, proj.to_string()).unwrap();
            let h = stats.handle();
            for (orig, delta) in rows {
                h.record(saved(orig, delta));
            }
            drop(h);
            stats.shutdown().await;
        }
    }

    /// Multi-project aggregate: per-project results/bytes/% and the grand totals all
    /// match the seeded data, ordered by bytes saved descending (biggest win first).
    #[tokio::test]
    async fn read_summary_aggregates_per_project() {
        let tmp = TempDir::new("read-multi");
        seed(
            tmp.path(),
            &[
                ("proj-big", 1000, 600),  // 600 saved
                ("proj-big", 1000, 400),  // → proj-big: 2 results, 2000 orig, 1000 saved
                ("proj-small", 500, 100), // → proj-small: 1 result, 500 orig, 100 saved
            ],
        )
        .await;

        let summary = read_summary_in(tmp.path()).unwrap();
        assert!(!summary.is_empty());
        assert_eq!(summary.projects.len(), 2);

        // Ordered by Σsaved desc → proj-big first.
        let big = &summary.projects[0];
        assert_eq!(big.project_path, "proj-big");
        assert_eq!(big.results, 2);
        assert_eq!(big.original_bytes, 2000);
        assert_eq!(big.saved_bytes, 1000);
        assert_eq!(big.grew_results, 0);
        assert!((big.saved_pct() - 50.0).abs() < 1e-9);

        let small = &summary.projects[1];
        assert_eq!(small.project_path, "proj-small");
        assert_eq!(small.results, 1);
        assert_eq!(small.saved_bytes, 100);

        // Grand totals.
        let (results, original, saved, grew) = summary.totals();
        assert_eq!((results, original, saved, grew), (3, 2500, 1100, 0));
        assert!((summary.total_saved_pct() - 1100.0 / 2500.0 * 100.0).abs() < 1e-9);
    }

    /// "N results grew" counts negative-delta rows **per project, independent of the
    /// project's net sign** (locked S4 decision): a project that nets positive can
    /// still report grew-rows.
    #[tokio::test]
    async fn read_summary_counts_grew_rows_sign_independent() {
        let tmp = TempDir::new("read-grew");
        seed(
            tmp.path(),
            &[
                ("proj", 1000, 800), // big win
                ("proj", 20, -15),   // grew (TOON larger) — but project still nets +
                ("proj", 30, -10),   // grew again
            ],
        )
        .await;

        let summary = read_summary_in(tmp.path()).unwrap();
        let p = &summary.projects[0];
        assert_eq!(p.results, 3);
        assert_eq!(p.saved_bytes, 800 - 15 - 10, "net is still positive");
        assert!(p.saved_bytes > 0);
        assert_eq!(
            p.grew_results, 2,
            "both grew-rows counted despite net-positive"
        );
    }

    /// File-absent → graceful empty state, NOT an error, and **no DB is created** by
    /// the read (the side-effect-free invariant: a reader run before any `--stats`
    /// serve must not litter an empty store).
    #[test]
    fn read_summary_absent_store_is_empty_and_creates_nothing() {
        let tmp = TempDir::new("read-absent");
        let base = tmp.path().join("never-served");
        // Dir doesn't even exist yet.
        let summary = read_summary_in(&base).unwrap();
        assert!(
            summary.is_empty(),
            "absent store → empty summary, not an error"
        );
        assert!((summary.total_saved_pct() - 0.0).abs() < 1e-9);
        assert!(!base.join(DB_FILE).exists(), "read must not create the db");
        assert!(!base.exists(), "read must not create the store dir");
    }

    /// A net-negative project (TOON grew more than it shrank) reports a negative
    /// saved-bytes and a negative %, never coerced to zero — the honest readout.
    #[tokio::test]
    async fn read_summary_net_negative_project_reads_signed() {
        let tmp = TempDir::new("read-neg");
        seed(tmp.path(), &[("shrinky", 100, 10), ("shrinky", 40, -50)]).await;
        let summary = read_summary_in(tmp.path()).unwrap();
        let p = &summary.projects[0];
        assert_eq!(p.saved_bytes, 10 - 50);
        assert_eq!(p.grew_results, 1);
        assert!(
            p.saved_pct() < 0.0,
            "net-negative project shows a negative %"
        );
    }
}
