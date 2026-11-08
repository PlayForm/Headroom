//! CCR (Compress-Cache-Retrieve) storage layer.
//!
//! When a transform compresses data with row-drop or opaque-string
//! substitution, the *original payload* is stashed here keyed by the
//! hash that ends up in the prompt. The runtime later honors retrieval
//! tool calls by looking up the hash in this store and serving back the
//! original. This is the cornerstone of CCR: lossy on the wire, lossless
//! end-to-end.
//!
//! Mirrors the semantics of Python's [`CompressionStore`] (`headroom/
//! cache/compression_store.py`) but stripped down to the contract that
//! actually matters for retrieval - no BM25 search, no retrieval-event
//! feedback, no per-tool metadata. Those live in the runtime layer; this
//! crate only needs put/get.
//!
//! # Backends
//!
//! - [`backends::InMemoryCcrStore`] - process-local, sharded `DashMap`.
//!   Test default; lost on restart, fragmented across workers.
//! - [`backends::SqliteCcrStore`] - production default. Persistent
//!   across worker restarts; shareable across workers via a shared DB
//!   file. WAL-mode, prepared statements, lazy TTL purge on read.
//! - [`backends::RedisCcrStore`] - multi-worker opt-in (cfg-gated
//!   behind `feature = "redis"`). No sticky-session required at the
//!   load balancer.
//!
//! [`backends::from_config`] selects one at startup and surfaces every
//! init error to the caller (per `feedback_no_silent_fallbacks.md`).
//!
//! [`CompressionStore`]: https://github.com/headroomlabs-ai/headroom/blob/main/headroom/cache/compression_store.py

pub mod backends;

use std::time::Duration;

pub use backends::{from_config, CcrBackendConfig, CcrBackendInitError, InMemoryCcrStore};

/// Pluggable CCR storage backend. `Send + Sync` so it can sit behind an
/// `Arc` and be shared across threads in the proxy.
pub trait CcrStore: Send + Sync {
	/// Stash `payload` under `hash`. If the hash already exists, the
	/// new payload overwrites - same hash should mean same content, so
	/// re-storing is idempotent.
	fn put(&self, hash: &str, payload: &str);

	/// Stash `payload` and report whether the write actually landed.
	///
	/// Fork addition (PlayForm): the Aphrodite proxy only replaces original
	/// content with a marker when the store confirmed the write - a marker
	/// whose hash resolves to nothing is permanent data loss (report 02 F4).
	/// Defaults to `put` + `true`; backends that can fail override it.
	fn put_checked(&self, hash: &str, payload: &str) -> bool {
		self.put(hash, payload);
		true
	}

	/// Look up `hash`. Returns `None` if missing or expired.
	fn get(&self, hash: &str) -> Option<String>;

	/// Number of live entries. Informational; used by tests + telemetry.
	/// Some backends (notably Redis) cannot answer this efficiently and
	/// return 0 - see backend-specific docs.
	fn len(&self) -> usize;

	/// Remove `hash`. Returns `true` if the entry existed.
	///
	/// Fork addition (PlayForm): the Aphrodite proxy evicts single entries on
	/// its `/ccr/*` management routes. Defaulted so the upstream backends do
	/// not have to implement it.
	fn del(&self, _hash: &str) -> bool {
		false
	}

	/// Cumulative database-level stats for telemetry.
	///
	/// Fork addition (PlayForm): `GET /stats/db` reports live occupancy and
	/// retention state. Defaulted to `None` for backends without a durable
	/// store.
	fn stats_db(&self) -> Option<serde_json::Value> {
		None
	}

	fn is_empty(&self) -> bool {
		self.len() == 0
	}
}

/// Default capacity - matches Python's `CompressionStore` default.
pub const DEFAULT_CAPACITY: usize = 1000;

/// Default TTL - 30 minutes, matching Python
/// (`CCRConfig.store_ttl_seconds`). Session-scale: agentic sessions
/// routinely outlive the old 5-minute default, and an expired entry
/// silently converts "lossless with retrieval" into "lossy".
pub const DEFAULT_TTL: Duration = Duration::from_secs(1800);

/// The TTL is an **idle window**, not a wall clock: every successful
/// `get` restarts the entry's clock, so an entry a session keeps
/// touching survives a long multi-agent burst (#2604). To keep
/// constant access from pinning an entry forever, an absolute max
/// lifetime of `DEFAULT_MAX_LIFETIME_MULTIPLIER * ttl` (measured from
/// insertion) caps the sliding window. Mirrors the Python
/// `CompressionStore` semantics.
pub const DEFAULT_MAX_LIFETIME_MULTIPLIER: u32 = 8;

/// Absolute max lifetime for an entry with idle window `idle_ttl`.
pub fn max_lifetime_for(idle_ttl: Duration) -> Duration {
	idle_ttl.saturating_mul(DEFAULT_MAX_LIFETIME_MULTIPLIER)
}

/// `u64` seconds -> `i64` for SQLite/Redis. Saturates: `u64::MAX` ("never
/// expire") must not wrap to -1 and expire at once.
pub(crate) fn secs_i64(secs: u64) -> i64 {
	i64::try_from(secs).unwrap_or(i64::MAX)
}

/// Compute the canonical CCR key for `payload`. BLAKE3 → first 40 hex
/// chars (160 bits - collision-resistant for the bounded LRU population
/// the proxy will hold, and the length Aphrodite's marker format
/// documents). Centralized here so every call site (live-zone
/// dispatcher, tests, future Python parity) hashes the same way.
pub fn compute_key(payload: &[u8]) -> String {
	let h = blake3::hash(payload);
	let hex = h.to_hex();
	// Fork: 40 hex chars (160 bits), matching Aphrodite's documented marker
	// format - `docs/ccr/marker-format.md` ("lowercase 40-character hex
	// digests") - and the length its catalog, prefetch and proxy paths assume
	// (`Full 40-char hash: a truncated one is unresolvable`). Upstream
	// truncates to 24 to match its Python tool-injection regex
	// (`[a-f0-9]{24}`); the fork's consumers are Aphrodite, which expects 40.
	hex.as_str()[..40].to_string()
}

/// Standard `<<ccr:HASH>>` marker injected into compressed block content
/// so the runtime can later look up the original bytes when the model
/// calls `headroom_retrieve`. Format is intentionally fixed across
/// proxy code-paths and tests.
pub fn marker_for(hash: &str) -> String {
	format!("<<ccr:{hash}>>")
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn compute_key_is_40_hex_chars() {
		let k = compute_key(b"hello world");
		// Fork contract: Aphrodite's marker format documents 40-char digests.
		assert_eq!(k.len(), 40);
		assert!(k.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
	}

	#[test]
	fn compute_key_is_deterministic() {
		let a = compute_key(b"the same payload");
		let b = compute_key(b"the same payload");
		assert_eq!(a, b);
	}

	#[test]
	fn compute_key_diverges_for_different_payloads() {
		let a = compute_key(b"alpha");
		let b = compute_key(b"beta");
		assert_ne!(a, b);
	}

	#[test]
	fn marker_format_is_pinned() {
		assert_eq!(marker_for("abc123"), "<<ccr:abc123>>");
	}
}
