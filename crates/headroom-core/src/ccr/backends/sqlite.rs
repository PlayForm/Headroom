//! SQLite-backed CCR store.
//!
//! The default **production** backend: persistent across worker
//! restarts and shareable across workers via a shared DB file. Schema:
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS ccr_entries (
//!     hash          TEXT PRIMARY KEY,
//!     original      BLOB NOT NULL,
//!     created_at    INTEGER NOT NULL,   -- unix-seconds
//!     ttl_seconds   INTEGER NOT NULL,   -- idle window, restarted on get
//!     last_accessed INTEGER NOT NULL    -- unix-seconds
//! );
//! ```
//!
//! The TTL is an **idle window** (#2604): every successful `get`
//! restarts the row's clock via `last_accessed`, bounded by an absolute
//! max lifetime measured from `created_at`. On every `get` we
//! lazy-purge stale rows (`WHERE last_accessed + ttl_seconds < now OR
//! created_at + max_lifetime < now`) - no background reaper thread,
//! no cron. DBs created by pre-sliding builds are migrated in place
//! (the `last_accessed` column is added, backfilled from `created_at`).
//!
//! `put` also runs the sweep, debounced to once every 60 seconds so a
//! compress-heavy, retrieve-light workload cannot accumulate expired
//! rows forever, and so high write-concurrency does not re-execute the
//! same DELETE (report 06 F10/T12).
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
//! (e.g. one per worker) - multi-worker safety is provided by SQLite's
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

use crate::ccr::{max_lifetime_for, CcrStore};

/// Minimum interval between lazy-purge sweeps, in seconds.
///
/// Prevents a sustained burst of concurrent `put` calls from each
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
	/// Default idle TTL applied on every `put`. Mirrors Python's
	/// `compression_store` idle window.
	default_ttl_seconds: u64,
	/// Absolute max lifetime (seconds since `created_at`) that caps the
	/// sliding idle window. Defaults to 8x the idle TTL.
	max_lifetime_seconds: u64,
	/// Path the connection was opened against - kept for diagnostics
	/// and for the proxy-restart simulation test.
	path: PathBuf,
	/// Tracks the last time we ran a lazy-purge sweep. Debounced to
	/// once per [`PURGE_DEBOUNCE_SECS`] to avoid redundant DELETE
	/// statements under high concurrent write load.
	last_purge: Mutex<Option<Instant>>,
}

impl SqliteCcrStore {
	/// Open or create the DB file at `path` and prepare the schema.
	/// `default_ttl_seconds` is the idle window; the absolute max
	/// lifetime defaults to 8x that (see
	/// [`crate::ccr::DEFAULT_MAX_LIFETIME_MULTIPLIER`]).
	/// Errors surface to the caller (`from_config`); we never silently
	/// fall back to the in-memory backend (`feedback_no_silent_fallbacks.md`).
	pub fn open(path: impl AsRef<Path>, default_ttl_seconds: u64) -> rusqlite::Result<Self> {
		let max_lifetime = max_lifetime_for(Duration::from_secs(default_ttl_seconds)).as_secs();
		Self::open_with_ttls(path, default_ttl_seconds, max_lifetime)
	}

	/// Full-control constructor: idle window and absolute max lifetime
	/// specified independently.
	pub fn open_with_ttls(
		path: impl AsRef<Path>,
		default_ttl_seconds: u64,
		max_lifetime_seconds: u64,
	) -> rusqlite::Result<Self> {
		let path_buf = path.as_ref().to_path_buf();
		let conn = Connection::open(&path_buf)?;

		// WAL gives us readers-don't-block-writers. `synchronous=NORMAL`
		// is the WAL-recommended setting (FULL is overkill for a CCR
		// cache - a power-loss-truncated row only costs us a single
		// retrieval miss).
		conn.pragma_update(None, "journal_mode", "WAL")?;
		conn.pragma_update(None, "synchronous", "NORMAL")?;
		// Default busy timeout is 0 (fail-fast) - with multiple aphrodite
		// processes sharing one ccr.db (e.g. two token proxies both
		// defaulting to the same path), a write colliding with another
		// process's write/checkpoint returned SQLITE_BUSY immediately,
		// `put` logged a warning and returned `false`, and the caller
		// proceeded to destroy the original content anyway (see F3/T2's
		// fix in the aphrodite proxy layer). Block briefly instead of
		// failing immediately under normal cross-process contention.
		conn.busy_timeout(Duration::from_secs(5))?;

		conn.execute(
			"CREATE TABLE IF NOT EXISTS ccr_entries (
                 hash          TEXT PRIMARY KEY,
                 original      BLOB NOT NULL,
                 created_at    INTEGER NOT NULL,
                 ttl_seconds   INTEGER NOT NULL,
                 last_accessed INTEGER NOT NULL
             )",
			[],
		)?;
		Self::migrate_legacy_schema(&conn)?;
		// No secondary index - the schema is one-row-per-PK and the only
		// non-PK lookup (the lazy-purge sweep) is a `WHERE` predicate on
		// a small table; an index on the expiry expressions would cost
		// more than it saves.

		// Schema migration hook (report 06 F10/T12): `CREATE TABLE IF NOT
		// EXISTS` alone silently keeps whatever schema an older binary
		// already created on disk - a column addition needs this to detect
		// "old file, new code" instead of just running the `CREATE` (a
		// no-op against the existing table) and then failing on every
		// query that references the new column. `user_version` starts at 0
		// on a fresh SQLite file. Version 2 adds `last_accessed` (the
		// sliding idle window, #2604); `migrate_legacy_schema` above is
		// that version's migration branch. Bump this and add a branch when
		// the schema next changes.
		const SCHEMA_VERSION: i64 = 2;
		let on_disk_version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
		if on_disk_version < SCHEMA_VERSION {
			conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
		}

		Ok(Self {
			conn: Mutex::new(conn),
			default_ttl_seconds,
			max_lifetime_seconds,
			path: path_buf,
			last_purge: Mutex::new(None),
		})
	}

	/// DBs created before the sliding-TTL change lack `last_accessed`.
	/// Add it in place and backfill from `created_at` so legacy rows
	/// keep their original expiry baseline rather than being purged or
	/// artificially refreshed.
	fn migrate_legacy_schema(conn: &Connection) -> rusqlite::Result<()> {
		let has_last_accessed = conn
			.prepare("SELECT 1 FROM pragma_table_info('ccr_entries') WHERE name = 'last_accessed'")?
			.exists([])?;
		if !has_last_accessed {
			conn.execute(
				"ALTER TABLE ccr_entries ADD COLUMN last_accessed INTEGER NOT NULL DEFAULT 0",
				[],
			)?;
			conn.execute(
				"UPDATE ccr_entries SET last_accessed = created_at WHERE last_accessed = 0",
				[],
			)?;
		}
		Ok(())
	}

	/// Path the connection was opened against. Test helper.
	pub fn path(&self) -> &Path {
		&self.path
	}

	/// Default idle TTL (seconds) applied on every `put`.
	pub fn default_ttl_seconds(&self) -> u64 {
		self.default_ttl_seconds
	}

	/// Absolute max lifetime (seconds) capping the sliding idle window.
	pub fn max_lifetime_seconds(&self) -> u64 {
		self.max_lifetime_seconds
	}

	/// Run a purge sweep immediately, bypassing the `PURGE_DEBOUNCE_SECS`
	/// window. Not on any hot path - for tests that need to observe
	/// physical deletion without waiting out the debounce, and as a manual
	/// "vacuum now" hook a future ops command could wire up.
	pub fn force_purge_now(&self) {
		let now = Self::now_unix_seconds();
		let conn = lock_conn(&self.conn);
		if let Err(err) = self.purge_expired(&conn, now) {
			tracing::warn!(target = "ccr.sqlite", error = %err, "ccr_sqlite_purge_failed");
		}
		if let Ok(mut last) = self.last_purge.lock() {
			*last = Some(Instant::now());
		}
	}

	/// Drop all expired rows: idle past their window, or past the
	/// absolute max lifetime. Lazy - invoked from `get`. Returns the
	/// number of rows purged.
	fn purge_expired(&self, conn: &Connection, now: u64) -> rusqlite::Result<usize> {
		// Timestamps have whole-second resolution. Use a strict boundary so
		// truncation can extend a cache entry by less than one second but can
		// never expire it before the configured idle or lifetime window.
		let purged = conn.execute(
			"DELETE FROM ccr_entries
             WHERE last_accessed + ttl_seconds < ?1
                OR created_at + ?2 < ?1",
			params![now as i64, self.max_lifetime_seconds as i64],
		)?;
		Ok(purged)
	}

	/// Check whether a purge is due (debounced to `PURGE_DEBOUNCE_SECS`)
	/// and run it if so. Updates `last_purge` in-place.
	fn maybe_purge(&self, conn: &Connection, now: u64) {
		let mut last = match self.last_purge.lock() {
			Ok(g) => g,
			Err(poisoned) => {
				tracing::warn!(target = "ccr.sqlite", "ccr_sqlite_last_purge_mutex_poisoned_recovered");
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

		if let Err(err) = self.purge_expired(conn, now) {
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

	fn get_at(&self, hash: &str, now: u64) -> Option<String> {
		let conn = lock_conn(&self.conn);

		// Lazy purge sweep, then the real lookup. Both happen under
		// the same mutex so the row we read is guaranteed not to have
		// been just-deleted by another caller.
		if let Err(err) = self.purge_expired(&conn, now) {
			tracing::warn!(
				target = "ccr.sqlite",
				error = %err,
				"ccr_sqlite_purge_failed"
			);
		}

		let row: Option<Vec<u8>> = conn
			.query_row(
				"SELECT original FROM ccr_entries
                 WHERE hash = ?1
                   AND last_accessed + ttl_seconds >= ?2
                   AND created_at + ?3 >= ?2",
				params![hash, now as i64, self.max_lifetime_seconds as i64],
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

		let row = row?;
		// Sliding idle window (#2604): a successful hit restarts the
		// row's idle clock. Still under the same mutex as the lookup.
		if let Err(err) = conn.execute(
			"UPDATE ccr_entries SET last_accessed = ?2 WHERE hash = ?1",
			params![hash, now as i64],
		) {
			tracing::warn!(
				target = "ccr.sqlite",
				hash = %hash,
				error = %err,
				"ccr_sqlite_touch_failed"
			);
		}

		String::from_utf8(row).ok()
	}
}

impl CcrStore for SqliteCcrStore {
	fn put(&self, hash: &str, payload: &str) -> bool {
		let now = Self::now_unix_seconds();
		let conn = lock_conn(&self.conn);
		// Debounced lazy purge sweep (report 06 F10/T12) - previously only
		// `get` ever purged, so a compress-heavy, retrieve-light workload
		// (the common case: most markers are never expanded) accumulated
		// every expired row forever and `ccr.db` only ever grew.
		self.maybe_purge(&conn, now);
		// Upsert by PK. ON CONFLICT REPLACE matches the in-memory
		// backend's idempotent re-store semantics.
		let res = conn.execute(
			"INSERT INTO ccr_entries (hash, original, created_at, ttl_seconds, last_accessed)
             VALUES (?1, ?2, ?3, ?4, ?3)
             ON CONFLICT(hash) DO UPDATE SET
                 original      = excluded.original,
                 created_at    = excluded.created_at,
                 ttl_seconds   = excluded.ttl_seconds,
                 last_accessed = excluded.last_accessed",
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
		// compressed block - a missed put degrades gracefully to "model
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
		self.get_at(hash, Self::now_unix_seconds())
	}

	fn len(&self) -> usize {
		let conn = lock_conn(&self.conn);
		conn.query_row("SELECT COUNT(*) FROM ccr_entries", [], |r| r.get::<_, i64>(0))
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
			.query_row("SELECT COALESCE(SUM(LENGTH(original)), 0) FROM ccr_entries", [], |r| r.get(0))
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
		// This is a heuristic - actual original payloads are stored
		// uncompressed in `total_bytes_original`.
		Some(serde_json::json!({
			"total_entries": total_entries,
			"total_bytes_original": total_original,
			"total_bytes_compressed": total_entries.saturating_mul(24),
			"oldest_entry_age_seconds": oldest_age_seconds,
			"database_size_bytes": db_size,
		}))
	}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with_row(
        idle_ttl: u64,
        max_lifetime: u64,
        created_at: u64,
        last_accessed: u64,
    ) -> (tempfile::TempDir, SqliteCcrStore, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store =
            SqliteCcrStore::open_with_ttls(dir.path().join("ccr.sqlite"), idle_ttl, max_lifetime)
                .expect("open sqlite store");
        let hash = "boundary-entry".to_string();
        {
            let conn = store.conn.lock().expect("ccr sqlite mutex poisoned");
            conn.execute(
                "INSERT INTO ccr_entries
                    (hash, original, created_at, ttl_seconds, last_accessed)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    &hash,
                    b"payload".as_slice(),
                    created_at as i64,
                    idle_ttl as i64,
                    last_accessed as i64,
                ],
            )
            .expect("insert boundary row");
        }
        (dir, store, hash)
    }

    #[test]
    fn exact_idle_ttl_boundary_is_still_valid() {
        let (_dir, store, hash) = store_with_row(5, 20, 100, 100);

        assert_eq!(store.get_at(&hash, 105).as_deref(), Some("payload"));
        assert_eq!(store.get_at(&hash, 111), None);
        assert_eq!(store.len(), 0, "expired row must be purged");
    }

    #[test]
    fn exact_max_lifetime_boundary_is_still_valid() {
        let (_dir, store, hash) = store_with_row(5, 10, 100, 108);

        assert_eq!(store.get_at(&hash, 110).as_deref(), Some("payload"));
        assert_eq!(store.get_at(&hash, 111), None);
        assert_eq!(store.len(), 0, "expired row must be purged");
    }
}
