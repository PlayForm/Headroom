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
//! [`CompressionStore`]: https://github.com/chopratejas/headroom/blob/main/headroom/cache/compression_store.py

pub mod backends;

use std::time::Duration;

pub use backends::{from_config, CcrBackendConfig, CcrBackendInitError, InMemoryCcrStore};

/// Pluggable CCR storage backend. `Send + Sync` so it can sit behind an
/// `Arc` and be shared across threads in the proxy.
pub trait CcrStore: Send + Sync {
    /// Stash `payload` under `hash`. If the hash already exists, the
    /// new payload overwrites - same hash should mean same content, so
    /// re-storing is idempotent.
    /// Returns `true` if the payload was stored, `false` if the backend
    /// failed (e.g. SQLite/Redis connection error).
    fn put(&self, hash: &str, payload: &str) -> bool;

    /// Look up `hash`. Returns `None` if missing or expired.
    fn get(&self, hash: &str) -> Option<String>;

    /// Number of live entries. Informational; used by tests + telemetry.
    /// Some backends (notably Redis) cannot answer this efficiently and
    /// return 0 - see backend-specific docs.
    fn len(&self) -> usize;

    /// Remove `hash` from the store. Returns `true` if the entry existed
    /// and was removed, `false` if it was not found.
    fn del(&self, hash: &str) -> bool;

    /// Cumulative database-level stats for telemetry.
    ///
    /// Returns structured JSON with {total_entries, total_bytes_original,
    /// total_bytes_compressed, oldest_entry_age_seconds, database_size_bytes}.
    ///
    /// Default impl returns `None` - override in backends that can answer
    /// (e.g. [`SqliteCcrStore`](backends/sqlite/struct.SqliteCcrStore.html)).
    fn stats_db(&self) -> Option<serde_json::Value> {
        None
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Default capacity - matches Python's `CompressionStore` default.
pub const DEFAULT_CAPACITY: usize = 1000;

/// Default TTL - 5 minutes, matching Python.
pub const DEFAULT_TTL: Duration = Duration::from_secs(300);

/// Compute the canonical CCR key for `payload`. BLAKE3 → first 40 hex
/// chars (160 bits — safe for persistent SQLite/Redis backends holding
/// millions of entries). Centralized here so every call site hashes the
/// same way.
pub fn compute_key(payload: &[u8]) -> String {
    let h = blake3::hash(payload);
    let hex = h.to_hex();
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
        assert_eq!(k.len(), 40);
        assert!(k
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
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
