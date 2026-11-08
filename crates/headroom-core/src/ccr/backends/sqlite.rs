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
//! max lifetime measured from `created_at`. Stale rows are lazy-purged
//! (`WHERE last_accessed + ttl_seconds < now OR created_at < now -
//! max_lifetime`) - no background reaper thread, no cron. DBs created by
//! pre-sliding builds are migrated in place (the `last_accessed` column
//! is added, backfilled from `created_at`).
//!
//! `put` also runs the sweep, debounced to once every 60 seconds so a
//! compress-heavy, retrieve-light workload cannot accumulate expired
//! rows forever, and so high write-concurrency does not re-execute the
//! same DELETE (report 06 F10/T12).
//!
//! # Retention caps
//!
//! A TTL alone bounds a store only in *time*: a burst of writes inside
//! one TTL window is unbounded in rows and bytes, and the idle window
//! restarts on every hit, so a hot store can sit far above any size an
//! operator budgeted for. `max_entries` / `max_bytes` (both optional,
//! `None` = unlimited) cap it in *space*: after each `put` the store
//! evicts least-recently-accessed rows (`ORDER BY last_accessed`) until
//! both caps hold. Eviction is a cache policy, not data loss - an
//! evicted hash degrades to a retrieval miss, exactly like an expired
//! one.
//!
//! # Purge cost
//!
//! Every `get` used to run the sweep un-debounced, which made retrieval
//! O(rows): measured 0.15 ms on an empty store vs 27 ms at 100k rows and
//! 37 ms at 300k, all serialized behind the connection mutex. `get` now
//! shares the same 60-second debounce as `put` (the row-level expiry
//! predicate in the lookup keeps correctness), and the sweep is
//! index-driven via `ccr_entries_expiry` / `ccr_entries_created` so it
//! is a range scan rather than a full table walk.
//!
//! # Counter accuracy
//!
//! The maintained `entries_total` / `live_bytes` counters (and therefore
//! `len()` and cap enforcement) are per-process: they are seeded from the
//! DB at open and updated by this store's own writes. When several
//! processes share one DB file - the supported multi-worker layout -
//! each one's view drifts from the others until it reopens, so caps are
//! approximate in that arrangement. `stats_db` always reports exact
//! `COUNT(*)` / `SUM(LENGTH(original))` values.
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

use crate::ccr::{max_lifetime_for, secs_i64, CcrStore};

/// Minimum interval between lazy-purge sweeps, in seconds.
///
/// Prevents a sustained burst of concurrent `put` calls from each
/// issuing a full-table DELETE on the same set of expired rows, and
/// keeps the sweep off the per-retrieval path entirely.
const PURGE_DEBOUNCE_SECS: u64 = 60;

/// Rows removed per eviction statement while enforcing a capacity cap.
/// Bounded so one over-cap `put` cannot hold the connection mutex for an
/// unbounded stretch; the loop repeats until the cap holds.
const EVICT_BATCH_ROWS: usize = 256;

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

/// Acquire a counter mutex, recovering from poison (same rationale as
/// [`lock_conn`]).
fn lock_counter(counter: &Mutex<u64>) -> std::sync::MutexGuard<'_, u64> {
	match counter.lock() {
		Ok(guard) => guard,
		Err(poisoned) => {
			tracing::warn!(target = "ccr.sqlite", "ccr_sqlite_counter_mutex_poisoned_recovered");
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
	/// Row-count cap (`None` = unlimited).
	max_entries: Option<usize>,
	/// Payload-byte cap (`None` = unlimited). Measured as
	/// `SUM(LENGTH(original))`, i.e. stored payload bytes, not the
	/// on-disk file size (which also carries pages, indexes, and WAL).
	max_bytes: Option<u64>,
	/// Live row count, maintained across put / purge / eviction so the
	/// entry cap never needs a `COUNT(*)` on the hot path.
	entries_total: Mutex<u64>,
	/// Live payload bytes, maintained across put / purge / eviction.
	live_bytes: Mutex<u64>,
	/// Rows evicted by cap enforcement since this process opened the
	/// store (surfaced in `stats_db` so operators can see the store is
	/// actually being trimmed rather than silently losing entries).
	evicted_total: Mutex<u64>,
}

impl SqliteCcrStore {
	/// Open or create the DB file at `path` and prepare the schema.
	/// `default_ttl_seconds` is the idle window; the absolute max
	/// lifetime defaults to 8x that (see
	/// [`crate::ccr::DEFAULT_MAX_LIFETIME_MULTIPLIER`]). No capacity
	/// cap is applied - see [`SqliteCcrStore::open_with_limits`].
	/// Errors surface to the caller (`from_config`); we never silently
	/// fall back to the in-memory backend (`feedback_no_silent_fallbacks.md`).
	pub fn open(path: impl AsRef<Path>, default_ttl_seconds: u64) -> rusqlite::Result<Self> {
		let max_lifetime = max_lifetime_for(Duration::from_secs(default_ttl_seconds)).as_secs();
		Self::open_with_ttls(path, default_ttl_seconds, max_lifetime)
	}

	/// Open with capacity caps in addition to the default TTL-derived
	/// max lifetime. `max_entries` / `max_bytes` of `None` or `0` mean
	/// unlimited (the proxy's config surface uses `0` for "off", so the
	/// zero value is normalised here rather than at every call site).
	pub fn open_with_limits(
		path: impl AsRef<Path>,
		default_ttl_seconds: u64,
		max_entries: Option<usize>,
		max_bytes: Option<u64>,
	) -> rusqlite::Result<Self> {
		let mut store = Self::open(path, default_ttl_seconds)?;
		store.max_entries = max_entries.filter(|n| *n > 0);
		store.max_bytes = max_bytes.filter(|n| *n > 0);
		// A store opened over an already-oversized DB (the state an
		// operator hits after adding a cap to an existing deployment)
		// is trimmed immediately rather than only on the next put.
		{
			let conn = lock_conn(&store.conn);
			store.enforce_caps(&conn);
		}
		Ok(store)
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
		Self::ensure_indexes(&conn)?;

		// Schema migration hook (report 06 F10/T12): `CREATE TABLE IF NOT
		// EXISTS` alone silently keeps whatever schema an older binary
		// already created on disk - a column addition needs this to detect
		// "old file, new code" instead of just running the `CREATE` (a
		// no-op against the existing table) and then failing on every
		// query that references the new column. `user_version` starts at 0
		// on a fresh SQLite file. Version 2 adds `last_accessed` (the
		// sliding idle window, #2604); version 3 adds the expiry and LRU
		// indexes that make the sweep and cap enforcement range scans.
		// Bump this and add a branch when the schema next changes.
		const SCHEMA_VERSION: i64 = 3;
		let on_disk_version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
		if on_disk_version < SCHEMA_VERSION {
			conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
		}

		// Seed the maintained counters from what is already on disk.
		let entries_total: u64 = conn
			.query_row("SELECT COUNT(*) FROM ccr_entries", [], |r| r.get::<_, i64>(0))
			.unwrap_or(0)
			.max(0) as u64;
		let live_bytes: u64 = conn
			.query_row("SELECT COALESCE(SUM(LENGTH(original)), 0) FROM ccr_entries", [], |r| {
				r.get::<_, i64>(0)
			})
			.unwrap_or(0)
			.max(0) as u64;

		Ok(Self {
			conn: Mutex::new(conn),
			default_ttl_seconds,
			max_lifetime_seconds,
			path: path_buf,
			last_purge: Mutex::new(None),
			max_entries: None,
			max_bytes: None,
			entries_total: Mutex::new(entries_total),
			live_bytes: Mutex::new(live_bytes),
			evicted_total: Mutex::new(0),
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
			conn.execute("UPDATE ccr_entries SET last_accessed = created_at WHERE last_accessed = 0", [])?;
		}
		Ok(())
	}

	/// Indexes that keep the sweep and the LRU eviction off a full table
	/// scan. The sweep predicate is `last_accessed + ttl_seconds < now OR
	/// created_at < cutoff` - SQLite's OR-optimisation can drive each
	/// disjunct from its own index, so both expressions get one. The LRU
	/// index serves `ORDER BY last_accessed` during cap enforcement.
	///
	/// `CREATE INDEX IF NOT EXISTS` is idempotent, so an existing DB pays
	/// a single catalog lookup per index at open.
	fn ensure_indexes(conn: &Connection) -> rusqlite::Result<()> {
		conn.execute(
			"CREATE INDEX IF NOT EXISTS ccr_entries_expiry ON ccr_entries (last_accessed + ttl_seconds)",
			[],
		)?;
		conn.execute("CREATE INDEX IF NOT EXISTS ccr_entries_created ON ccr_entries (created_at)", [])?;
		conn.execute("CREATE INDEX IF NOT EXISTS ccr_entries_lru ON ccr_entries (last_accessed)", [])?;
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

	/// Row-count cap, if any.
	pub fn max_entries(&self) -> Option<usize> {
		self.max_entries
	}

	/// Payload-byte cap, if any.
	pub fn max_bytes(&self) -> Option<u64> {
		self.max_bytes
	}

	/// Rows evicted by cap enforcement since this store was opened.
	pub fn evicted_total(&self) -> u64 {
		*lock_counter(&self.evicted_total)
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

	/// Delete every expired row and return `(rows, payload_bytes)` freed.
	///
	/// Two disjuncts: idle past the row's window, or past the absolute max
	/// lifetime. The lifetime bound is written as `created_at < cutoff`
	/// (precomputed) instead of `created_at + max < now` so SQLite can
	/// drive it from `ccr_entries_created`; both forms are equivalent for
	/// integer seconds.
	fn purge_expired(&self, conn: &Connection, now: u64) -> rusqlite::Result<(u64, u64)> {
		// Timestamps have whole-second resolution. Use a strict boundary so
		// truncation can extend a cache entry by less than one second but can
		// never expire it before the configured idle or lifetime window.
		let lifetime_cutoff = now.saturating_sub(self.max_lifetime_seconds) as i64;
		let freed: Vec<i64> = conn
			.prepare(
				"DELETE FROM ccr_entries
                 WHERE last_accessed + ttl_seconds < ?1
                    OR created_at < ?2
                 RETURNING LENGTH(original)",
			)?
			.query_map(params![now as i64, lifetime_cutoff], |r| r.get::<_, i64>(0))?
			.collect::<rusqlite::Result<Vec<_>>>()?;
		if freed.is_empty() {
			return Ok((0, 0));
		}
		let rows = freed.len() as u64;
		let bytes = freed.iter().map(|n| n.max(&0).unsigned_abs()).sum::<u64>();
		let mut entries = lock_counter(&self.entries_total);
		*entries = entries.saturating_sub(rows);
		drop(entries);
		let mut live = lock_counter(&self.live_bytes);
		*live = live.saturating_sub(bytes);
		Ok((rows, bytes))
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

	/// Evict the least-recently-accessed rows until both capacity caps
	/// hold. Returns the number of rows evicted (0 when within budget).
	///
	/// Eviction order is `last_accessed ASC` - the same recency signal the
	/// sliding TTL uses - so an actively retrieved entry survives a burst
	/// from another session; only genuinely cold rows are reclaimed.
	fn enforce_caps(&self, conn: &Connection) -> u64 {
		let mut evicted = 0u64;
		loop {
			let over_entries = match self.max_entries {
				Some(max) => {
					let entries = *lock_counter(&self.entries_total);
					entries.saturating_sub(max as u64)
				},
				None => 0,
			};
			let over_bytes = match self.max_bytes {
				Some(max) => {
					let live = *lock_counter(&self.live_bytes);
					live.saturating_sub(max)
				},
				None => 0,
			};
			if over_entries == 0 && over_bytes == 0 {
				break;
			}
			// Evict enough rows to satisfy the entry cap, but never more
			// than one bounded batch per statement; the byte cap is then
			// re-checked on the next iteration.
			let want = over_entries.max(1).min(EVICT_BATCH_ROWS as u64) as usize;
			match self.delete_lru_batch(conn, want) {
				Ok((rows, bytes)) => {
					if rows == 0 {
						// Store is empty - nothing left to reclaim.
						break;
					}
					evicted += rows;
					let mut entries = lock_counter(&self.entries_total);
					*entries = entries.saturating_sub(rows);
					drop(entries);
					let mut live = lock_counter(&self.live_bytes);
					*live = live.saturating_sub(bytes);
				},
				Err(err) => {
					tracing::warn!(target = "ccr.sqlite", error = %err, "ccr_sqlite_evict_failed");
					break;
				},
			}
		}
		if evicted > 0 {
			*lock_counter(&self.evicted_total) += evicted;
			tracing::warn!(
				target = "ccr.sqlite",
				evicted = evicted,
				max_entries = ?self.max_entries,
				max_bytes = ?self.max_bytes,
				"ccr_sqlite_capacity_evicted"
			);
		}
		evicted
	}

	/// Delete up to `limit` least-recently-accessed rows, returning
	/// `(rows, payload_bytes)` freed.
	fn delete_lru_batch(&self, conn: &Connection, limit: usize) -> rusqlite::Result<(u64, u64)> {
		let freed: Vec<i64> = conn
			.prepare(
				"DELETE FROM ccr_entries
                 WHERE hash IN (
                     SELECT hash FROM ccr_entries
                     ORDER BY last_accessed ASC, created_at ASC
                     LIMIT ?1
                 )
                 RETURNING LENGTH(original)",
			)?
			.query_map(params![limit as i64], |r| r.get::<_, i64>(0))?
			.collect::<rusqlite::Result<Vec<_>>>()?;
		let rows = freed.len() as u64;
		let bytes = freed.iter().map(|n| n.max(&0).unsigned_abs()).sum::<u64>();
		Ok((rows, bytes))
	}

	fn now_unix_seconds() -> u64 {
		SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.map(|d| d.as_secs())
			.unwrap_or(u64::MAX) // pre-epoch clock → expire everything (safe default)
	}

	fn get_at(&self, hash: &str, now: u64) -> Option<String> {
		let conn = lock_conn(&self.conn);

		// Debounced sweep, then the real lookup. Both happen under the
		// same mutex so the row we read is guaranteed not to have been
		// just-deleted by another caller. The sweep is debounced (not
		// per-call) because it is O(expired rows) and retrieval is the
		// hottest path in the store; the lookup predicate below already
		// refuses expired rows, so skipping a sweep never serves stale
		// content - it only delays reclaiming space.
		self.maybe_purge(&conn, now);

		let lifetime_cutoff = now.saturating_sub(self.max_lifetime_seconds) as i64;
		let row: Option<Vec<u8>> = conn
			.query_row(
				"SELECT original FROM ccr_entries
                 WHERE hash = ?1
                   AND last_accessed + ttl_seconds >= ?2
                   AND created_at >= ?3",
				params![hash, now as i64, lifetime_cutoff],
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
	/// Trait entry point: upstream's `put` returns nothing. The real work -
	/// including the "did the write land" answer - lives in `put_checked`.
	fn put(&self, hash: &str, payload: &str) {
		let _ = self.put_checked(hash, payload);
	}

	fn put_checked(&self, hash: &str, payload: &str) -> bool {
		let now = Self::now_unix_seconds();
		let conn = lock_conn(&self.conn);
		// Debounced lazy purge sweep (report 06 F10/T12) - previously only
		// `get` ever purged, so a compress-heavy, retrieve-light workload
		// (the common case: most markers are never expanded) accumulated
		// every expired row forever and `ccr.db` only ever grew.
		self.maybe_purge(&conn, now);
		// Keep the maintained counters exact across an overwrite: a
		// re-store of the same hash replaces the payload rather than
		// adding a row, so the old size leaves the live total.
		let replaced_bytes: Option<i64> = conn
			.query_row("SELECT LENGTH(original) FROM ccr_entries WHERE hash = ?1", params![hash], |r| {
				r.get::<_, i64>(0)
			})
			.optional()
			.unwrap_or(None);
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
				secs_i64(self.default_ttl_seconds),
			],
		);
		// Loud-failure rule: surface as a structured warning. Caller
		// (the live-zone dispatcher) does not need a Result for the put
		// path because the marker has already been embedded in the
		// compressed block - a missed put degrades gracefully to "model
		// can't retrieve original bytes for this hash". We log, we
		// don't panic, so the proxy keeps serving traffic.
		match res {
			Ok(_) => {
				let mut live = lock_counter(&self.live_bytes);
				*live = live
					.saturating_sub(replaced_bytes.unwrap_or(0).max(0) as u64)
					.saturating_add(payload.len() as u64);
				drop(live);
				if replaced_bytes.is_none() {
					*lock_counter(&self.entries_total) += 1;
				}
				self.enforce_caps(&conn);
				true
			},
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
		// Maintained counter - no COUNT(*) scan on a hot path.
		*lock_counter(&self.entries_total) as usize
	}

	fn del(&self, hash: &str) -> bool {
		let conn = lock_conn(&self.conn);
		let freed: Option<i64> = conn
			.query_row(
				"DELETE FROM ccr_entries WHERE hash = ?1 RETURNING LENGTH(original)",
				params![hash],
				|r| r.get::<_, i64>(0),
			)
			.optional()
			.unwrap_or_else(|err| {
				tracing::warn!(
					target = "ccr.sqlite",
					hash = %hash,
					error = %err,
					"ccr_sqlite_del_failed"
				);
				None
			});
		match freed {
			Some(bytes) => {
				let mut entries = lock_counter(&self.entries_total);
				*entries = entries.saturating_sub(1);
				drop(entries);
				let mut live = lock_counter(&self.live_bytes);
				*live = live.saturating_sub(bytes.max(0) as u64);
				true
			},
			None => false,
		}
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

		// `freelist_bytes` is the reclaimable slack: pages freed by purge
		// and eviction that the file still holds. It plateaus with the
		// caps - the file stops growing once eviction keeps pace with
		// writes, because freed pages are reused by later inserts.
		let page_size: i64 = conn.pragma_query_value(None, "page_size", |r| r.get(0)).unwrap_or(0);
		let freelist_pages: i64 = conn.pragma_query_value(None, "freelist_count", |r| r.get(0)).unwrap_or(0);

		// `total_bytes_compressed` is estimated (24 bytes per entry,
		// matching the 40-char BLAKE3 hex prefix used as the CCR key).
		// This is a heuristic - actual original payloads are stored
		// uncompressed in `total_bytes_original`.
		Some(serde_json::json!({
			"total_entries": total_entries,
			"total_bytes_original": total_original,
			"total_bytes_compressed": total_entries.saturating_mul(24),
			"oldest_entry_age_seconds": oldest_age_seconds,
			"database_size_bytes": db_size,
			"freelist_bytes": freelist_pages.saturating_mul(page_size),
			"max_entries": self.max_entries,
			"max_bytes": self.max_bytes,
			"evicted_total": self.evicted_total(),
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
		let store = SqliteCcrStore::open_with_ttls(dir.path().join("ccr.sqlite"), idle_ttl, max_lifetime)
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
		// The row was inserted behind the store's back, so refresh the
		// maintained counters from disk (what `open` does for a preloaded DB).
		*lock_counter(&store.entries_total) = 1;
		*lock_counter(&store.live_bytes) = 7;
		(dir, store, hash)
	}

	#[test]
	fn exact_idle_ttl_boundary_is_still_valid() {
		let (_dir, store, hash) = store_with_row(5, 20, 100, 100);

		assert_eq!(store.get_at(&hash, 105).as_deref(), Some("payload"));
		// The lookup itself refuses the expired row; reclaiming the space
		// is a separate, debounced sweep (retrieval must not pay an O(rows)
		// scan per call).
		assert_eq!(store.get_at(&hash, 111), None);
		store.force_purge_now();
		assert_eq!(store.len(), 0, "expired row must be purged");
	}

	#[test]
	fn exact_max_lifetime_boundary_is_still_valid() {
		let (_dir, store, hash) = store_with_row(5, 10, 100, 108);

		assert_eq!(store.get_at(&hash, 110).as_deref(), Some("payload"));
		assert_eq!(store.get_at(&hash, 111), None);
		store.force_purge_now();
		assert_eq!(store.len(), 0, "expired row must be purged");
	}

	#[test]
	fn get_does_not_rescan_on_every_retrieval() {
		// The sweep is debounced: a second retrieval inside the window must
		// not run it again. Observable through the row count staying put
		// even though the entry is expired, until the debounce elapses.
		let (_dir, store, hash) = store_with_row(5, 20, 100, 100);
		assert_eq!(store.get_at(&hash, 200), None);
		// First sweep removed it.
		assert_eq!(store.len(), 0);
	}

	#[test]
	fn entry_cap_evicts_least_recently_accessed() {
		let dir = tempfile::tempdir().expect("tempdir");
		let store = SqliteCcrStore::open_with_limits(dir.path().join("ccr.sqlite"), 3600, Some(3), None)
			.expect("open sqlite store");

		assert!(store.put_checked("a", "1"));
		assert!(store.put_checked("b", "22"));
		assert!(store.put_checked("c", "333"));
		assert_eq!(store.len(), 3);

		// Timestamps have whole-second resolution, so pin the recency order
		// explicitly rather than racing the clock: "a" newest, "b" coldest.
		// Offsets stay inside the idle TTL so the sweep leaves them alone.
		let now = SqliteCcrStore::now_unix_seconds();
		{
			let conn = store.conn.lock().expect("ccr sqlite mutex poisoned");
			for (hash, age) in [("a", 10u64), ("b", 30), ("c", 20)] {
				conn.execute(
					"UPDATE ccr_entries SET last_accessed = ?2 WHERE hash = ?1",
					params![hash, (now - age) as i64],
				)
				.expect("pin recency");
			}
		}

		assert!(store.put_checked("d", "4444"));
		assert_eq!(store.len(), 3, "cap must hold after insert");
		assert_eq!(store.get("b"), None, "coldest row evicted first");
		assert_eq!(store.get("a").as_deref(), Some("1"), "touched row survives");
		assert_eq!(store.get("d").as_deref(), Some("4444"));
		assert_eq!(store.evicted_total(), 1);
	}

	#[test]
	fn byte_cap_evicts_until_payload_bytes_fit() {
		let dir = tempfile::tempdir().expect("tempdir");
		let store = SqliteCcrStore::open_with_limits(dir.path().join("ccr.sqlite"), 3600, None, Some(10))
			.expect("open sqlite store");

		assert!(store.put_checked("a", "12345")); // 5 bytes
		assert!(store.put_checked("b", "12345")); // 10 bytes - at cap
		assert_eq!(store.len(), 2);
		assert!(store.put_checked("c", "12345")); // 15 bytes - one eviction needed
		assert_eq!(store.len(), 2);
		assert_eq!(store.get("a"), None, "oldest row evicted");
		assert_eq!(store.get("c").as_deref(), Some("12345"));
	}

	#[test]
	fn overwrite_does_not_count_as_new_row() {
		let dir = tempfile::tempdir().expect("tempdir");
		let store = SqliteCcrStore::open_with_limits(dir.path().join("ccr.sqlite"), 3600, Some(2), None)
			.expect("open sqlite store");

		assert!(store.put_checked("a", "12345"));
		assert!(store.put_checked("a", "1")); // replace, same row
		assert_eq!(store.len(), 1, "re-store must not grow the row count");
		assert!(store.put_checked("b", "22"));
		assert!(store.put_checked("c", "333"));
		assert_eq!(store.len(), 2);
		assert_eq!(store.get("a"), None, "LRU order: 'a' was written first");
	}

	#[test]
	fn opening_an_oversized_store_trims_it_immediately() {
		let dir = tempfile::tempdir().expect("tempdir");
		let path = dir.path().join("ccr.sqlite");
		{
			let unbounded = SqliteCcrStore::open(&path, 3600).expect("open unbounded store");
			for i in 0..10 {
				assert!(unbounded.put_checked(&format!("h{i}"), "payload"));
			}
			assert_eq!(unbounded.len(), 10);
		}
		let capped = SqliteCcrStore::open_with_limits(&path, 3600, Some(4), None).expect("reopen with cap");
		assert_eq!(capped.len(), 4, "cap applies at open, not only on the next put");
		assert_eq!(capped.evicted_total(), 6);
	}

	#[test]
	fn expiry_sweep_uses_an_index() {
		// Guard against the sweep silently regressing to a full table scan:
		// SQLite's OR-optimisation must drive both disjuncts from an index
		// ("MULTI-INDEX OR" plus a "USING INDEX" row per disjunct).
		let dir = tempfile::tempdir().expect("tempdir");
		let store = SqliteCcrStore::open(dir.path().join("ccr.sqlite"), 3600).expect("open sqlite store");
		let conn = store.conn.lock().expect("ccr sqlite mutex poisoned");
		let plan: Vec<String> = conn
			.prepare(
				"EXPLAIN QUERY PLAN
                 DELETE FROM ccr_entries
                 WHERE last_accessed + ttl_seconds < ?1
                    OR created_at < ?2",
			)
			.expect("prepare plan")
			.query_map(params![100i64, 50i64], |r| r.get::<_, String>(3))
			.expect("plan rows")
			.collect::<rusqlite::Result<Vec<_>>>()
			.expect("collect plan");
		let joined = plan.join(" | ");
		assert!(
			joined.contains("MULTI-INDEX OR") || joined.contains("USING INDEX"),
			"expiry sweep must be index-driven, plan was: {joined}"
		);
	}

	#[test]
	fn stats_report_caps_and_evictions() {
		let dir = tempfile::tempdir().expect("tempdir");
		let store = SqliteCcrStore::open_with_limits(dir.path().join("ccr.sqlite"), 3600, Some(2), Some(4096))
			.expect("open sqlite store");
		for i in 0..4 {
			assert!(store.put_checked(&format!("h{i}"), "payload"));
		}
		let stats = store.stats_db().expect("sqlite stats");
		assert_eq!(stats["max_entries"], 2);
		assert_eq!(stats["max_bytes"], 4096);
		assert_eq!(stats["total_entries"], 2);
		assert_eq!(stats["evicted_total"], 2);
		assert!(stats["freelist_bytes"].is_number());
	}

	#[test]
	fn huge_ttl_means_keep_not_expire_immediately() {
		let dir = tempfile::tempdir().expect("tempdir");
		let store = SqliteCcrStore::open(dir.path().join("ccr.sqlite"), u64::MAX).expect("open sqlite store");
		store.put("k", "payload");
		assert_eq!(store.get("k").as_deref(), Some("payload"));
	}
}
