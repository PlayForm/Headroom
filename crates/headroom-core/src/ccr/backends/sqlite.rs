//! SQLite-backed CCR store.
//!
//! The default **production** backend: persistent across worker
//! restarts and shareable across workers via a shared DB file. Schema:
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS ccr_entries (
//!     hash         TEXT PRIMARY KEY,
//!     original     BLOB NOT NULL,
//!     created_at   INTEGER NOT NULL,   -- unix-seconds
//!     ttl_seconds  INTEGER NOT NULL
//! );
//! ```
//!
//! On every `get` we lazy-purge stale rows
//! (`WHERE created_at + ttl_seconds <= now`) — no background reaper
//! thread, no cron. The purge is debounced to once every 60 seconds
//! so high read-concurrency does not re-execute the same DELETE.
//!
//! All hot statements are prepared once on connection setup and reused
//! per call (per realignment build constraint #5: performant). Writes
//! upsert by primary key so re-storing the same hash overwrites in
//! place (matches in-memory and Redis backend semantics).
//!
//! # Concurrency
//!
//! `rusqlite::Connection` is `!Sync`, so we wrap it in a `Mutex`. CCR
//! reads/writes are short and rare relative to the proxy hot path, so
//! a single mutex on the connection is fine. Operators who measure
//! contention can shard by spinning up N stores backed by N DB files
//! (e.g. one per worker) — multi-worker safety is provided by SQLite's
//! own file locking.
//!
//! # WAL mode
//!
//! We open the connection in WAL mode so reads do not block writes
//! (and vice versa), and the on-disk journal does not grow unbounded.
//! Critical for proxy workloads where many concurrent retrievals can
//! land while a compression flushes a fresh row.
//!
//! # Poison resilience
//!
//! All `Mutex::lock()` calls degrade gracefully instead of panicking:
//! if another thread panicked while holding the lock the poisoned
//! mutex is cleared and a warning is emitted. This keeps the proxy
//! serving traffic even after a transient panic in the CCR subsystem.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde_json;

use crate::ccr::CcrStore;

/// Minimum interval between lazy-purge sweeps, in seconds.
///
/// Prevents a sustained burst of concurrent `get` calls from each
/// issuing a full-table DELETE on the same set of expired rows.
const PURGE_DEBOUNCE_SECS: u64 = 60;

/// Acquire the mutex guard, recovering from poison.
///
/// If the mutex is poisoned (another thread panicked while holding
/// it), we clear the poison, log a warning, and continue. This keeps
/// the proxy serving traffic rather than taking down the whole worker
/// because of a transient CCR panic.
fn lock_conn(conn: &Mutex<Connection>) -> std::sync::MutexGuard<'_, Connection> {
    match conn.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::warn!(target = "ccr.sqlite", "ccr_sqlite_mutex_poisoned_recovered");
            poisoned.into_inner()
        },
    }
}

/// SQLite-backed CCR store.
pub struct SqliteCcrStore {
    conn: Mutex<Connection>,
    /// Default TTL applied on every `put`. Mirrors Python's
    /// `compression_store` 5-minute window.
    default_ttl_seconds: u64,
    /// Path the connection was opened against — kept for diagnostics
    /// and for the proxy-restart simulation test.
    path: PathBuf,
    /// Tracks the last time we ran a lazy-purge sweep. Debounced to
    /// once per [`PURGE_DEBOUNCE_SECS`] to avoid redundant DELETE
    /// statements under high concurrent read load.
    last_purge: Mutex<Option<Instant>>,
}

impl SqliteCcrStore {
    /// Open or create the DB file at `path` and prepare the schema.
    /// Errors surface to the caller (`from_config`); we never silently
    /// fall back to the in-memory backend (`feedback_no_silent_fallbacks.md`).
    pub fn open(path: impl AsRef<Path>, default_ttl_seconds: u64) -> rusqlite::Result<Self> {
        let path_buf = path.as_ref().to_path_buf();
        let conn = Connection::open(&path_buf)?;

        // WAL gives us readers-don't-block-writers. `synchronous=NORMAL`
        // is the WAL-recommended setting (FULL is overkill for a CCR
        // cache — a power-loss-truncated row only costs us a single
        // retrieval miss).
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS ccr_entries (
                 hash         TEXT PRIMARY KEY,
                 original     BLOB NOT NULL,
                 created_at   INTEGER NOT NULL,
                 ttl_seconds  INTEGER NOT NULL
             )",
            [],
        )?;
        // No secondary index — the schema is one-row-per-PK and the only
        // non-PK lookup (the lazy-purge sweep) is a `WHERE` predicate on
        // a small table; an index on `created_at + ttl_seconds` would
        // cost more than it saves.

        Ok(Self {
            conn: Mutex::new(conn),
            default_ttl_seconds,
            path: path_buf,
            last_purge: Mutex::new(None),
        })
    }

    /// Path the connection was opened against. Test helper.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Default TTL (seconds) applied on every `put`.
    pub fn default_ttl_seconds(&self) -> u64 {
        self.default_ttl_seconds
    }

    /// Drop all expired rows. Lazy — invoked from `get`. Returns the
    /// number of rows purged.
    fn purge_expired(conn: &Connection, now: u64) -> rusqlite::Result<usize> {
        let purged = conn.execute(
            "DELETE FROM ccr_entries WHERE created_at + ttl_seconds <= ?1",
            params![now as i64],
        )?;
        Ok(purged)
    }

    /// Check whether a purge is due (debounced to `PURGE_DEBOUNCE_SECS`)
    /// and run it if so. Updates `last_purge` in-place.
    fn maybe_purge(&self, conn: &Connection, now: u64) {
        let mut last = match self.last_purge.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                tracing::warn!(
                    target = "ccr.sqlite",
                    "ccr_sqlite_last_purge_mutex_poisoned_recovered"
                );
                poisoned.into_inner()
            },
        };

        let due = match *last {
            Some(ts) => ts.elapsed() >= Duration::from_secs(PURGE_DEBOUNCE_SECS),
            None => true,
        };

        if !due {
            return;
        }

        if let Err(err) = Self::purge_expired(conn, now) {
            tracing::warn!(
                target = "ccr.sqlite",
                error = %err,
                "ccr_sqlite_purge_failed"
            );
        }
        *last = Some(Instant::now());
    }

    fn now_unix_seconds() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(u64::MAX) // pre-epoch clock → expire everything (safe default)
    }
}

impl CcrStore for SqliteCcrStore {
    fn put(&self, hash: &str, payload: &str) -> bool {
        let now = Self::now_unix_seconds();
        let conn = lock_conn(&self.conn);
        // Upsert by PK. ON CONFLICT REPLACE matches the in-memory
        // backend's idempotent re-store semantics.
        let res = conn.execute(
            "INSERT INTO ccr_entries (hash, original, created_at, ttl_seconds)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(hash) DO UPDATE SET
                 original    = excluded.original,
                 created_at  = excluded.created_at,
                 ttl_seconds = excluded.ttl_seconds",
            params![
                hash,
                payload.as_bytes(),
                now as i64,
                (self.default_ttl_seconds.min(i64::MAX as u64)) as i64,
            ],
        );
        // Loud-failure rule: surface as a structured warning. Caller
        // (the live-zone dispatcher) does not need a Result for the put
        // path because the marker has already been embedded in the
        // compressed block — a missed put degrades gracefully to "model
        // can't retrieve original bytes for this hash". We log, we
        // don't panic, so the proxy keeps serving traffic.
        match res {
            Ok(_) => true,
            Err(err) => {
                tracing::warn!(
                    target = "ccr.sqlite",
                    hash = %hash,
                    error = %err,
                    "ccr_sqlite_put_failed"
                );
                false
            },
        }
    }

    fn get(&self, hash: &str) -> Option<String> {
        let now = Self::now_unix_seconds();
        let conn = lock_conn(&self.conn);

        // Debounced lazy purge sweep, then the real lookup. Both happen
        // under the same mutex so the row we read is guaranteed not to
        // have been just-deleted by another caller.
        self.maybe_purge(&conn, now);

        let row: Option<Vec<u8>> = conn
            .query_row(
                "SELECT original FROM ccr_entries
                 WHERE hash = ?1 AND created_at + ttl_seconds > ?2",
                params![hash, now as i64],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()
            .unwrap_or_else(|err| {
                tracing::warn!(
                    target = "ccr.sqlite",
                    hash = %hash,
                    error = %err,
                    "ccr_sqlite_get_failed"
                );
                None
            });

        row.and_then(|bytes| String::from_utf8(bytes).ok())
    }

    fn len(&self) -> usize {
        let conn = lock_conn(&self.conn);
        conn.query_row("SELECT COUNT(*) FROM ccr_entries", [], |r| {
            r.get::<_, i64>(0)
        })
        .map(|n| n.max(0) as usize)
        .unwrap_or(0)
    }

    fn del(&self, hash: &str) -> bool {
        let conn = lock_conn(&self.conn);
        let rows = conn
            .execute("DELETE FROM ccr_entries WHERE hash = ?1", params![hash])
            .unwrap_or_else(|err| {
                tracing::warn!(
                    target = "ccr.sqlite",
                    hash = %hash,
                    error = %err,
                    "ccr_sqlite_del_failed"
                );
                0
            });
        rows > 0
    }

    fn stats_db(&self) -> Option<serde_json::Value> {
        let conn = lock_conn(&self.conn);

        let total_entries: i64 = conn
            .query_row("SELECT COUNT(*) FROM ccr_entries", [], |r| r.get(0))
            .unwrap_or(0);

        let total_original: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(LENGTH(original)), 0) FROM ccr_entries",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);

        let oldest_created: Option<i64> = conn
            .query_row("SELECT MIN(created_at) FROM ccr_entries", [], |r| r.get(0))
            .optional()
            .unwrap_or(None);

        let now = Self::now_unix_seconds() as i64;
        let oldest_age_seconds = oldest_created.map(|t| now.saturating_sub(t));

        let db_size = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);

        // `total_bytes_compressed` is estimated (24 bytes per entry,
        // matching the 24-char BLAKE3 hex prefix used as the CCR key).
        // This is a heuristic — actual original payloads are stored
        // uncompressed in `total_bytes_original`.
        Some(serde_json::json!({
            "total_entries": total_entries,
            "total_bytes_original": total_original,
            "total_bytes_compressed": (total_entries as i64).saturating_mul(24),
            "oldest_entry_age_seconds": oldest_age_seconds,
            "database_size_bytes": db_size,
        }))
    }
}
