//! headroom-core: foundation crate for the Rust port of Headroom.

// This crate has never needed `unsafe`; keep it that way. The FFI shim lives
// in `headroom-py`, which is deliberately not under this lint.
//
// Fork: `deny` rather than `forbid`, because edition 2024 made
// `std::env::{set_var,remove_var}` unsafe and the upstream tests mutate the
// environment (offline switch, HF cache paths). Exactly two functions carry a
// scoped `#[allow(unsafe_code)]` - the `test_support` shims below - each with
// a SAFETY note. Everything else in the crate is still denied.
#![deny(unsafe_code)]

// Vendored fork: style lints suppressed under nightly clippy. This crate is
// kept in sync with upstream PlayForm/Headroom, and these mechanical lints
// (`collapsible_if`: nested `if let` chains whose let-chain rewrites need
// edition 2024; `question_mark`: `if let` + early-return rewrites;
// `unnecessary_map_or`: `map_or(true, ..)` -> `is_none_or`) fire only when the
// crate is compiled at edition 2024 through workspace inheritance - which is
// exactly how the Aphrodite parent builds it. Suppressed here rather than
// rewriting vendored sources, to keep the fork diff against upstream minimal.
// The fork carried this same suppression before the upstream sync dropped it.
#![allow(clippy::collapsible_if, clippy::question_mark, clippy::unnecessary_map_or)]

pub mod auth_mode;
pub mod cache_control;
pub mod ccr;
pub mod compression_policy;
mod hf_cache;
pub mod offline;
#[cfg(feature = "ml")]
mod onnx_cpu;
pub mod relevance;
pub mod rollout;
pub mod signals;
pub mod tokenizer;
pub mod transforms;

// Re-exports for the live-zone dispatcher (Phase B PR-B2 consumes this).
// Hoisted to the crate root so the proxy crate gets one stable import
// path: `use headroom_core::compute_frozen_count;`. Keeping the
// `cache_control` module public too means downstream code can reach
// the helper types directly when needed.
pub use cache_control::compute_frozen_count;

/// Identity stub used by downstream crates and the Python binding to verify
/// linkage end-to-end.
pub fn hello() -> &'static str {
	"headroom-core"
}

#[cfg(test)]
pub(crate) mod test_support {
	use std::sync::{Mutex, MutexGuard};

	/// The process environment is global while `cargo test` runs tests in
	/// parallel threads inside one process, so a test that sets an env var can
	/// be observed by an unrelated sibling mid-assertion. Every test that
	/// mutates the environment holds this for its whole body. Same shape as
	/// `REGISTRY_LOCK` in `tokenizer::registry`.
	static ENV_LOCK: Mutex<()> = Mutex::new(());

	/// Recovers from poisoning on purpose: one panicking test should not
	/// cascade into every later test that touches the environment.
	pub(crate) fn env_lock() -> MutexGuard<'static, ()> {
		ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
	}

	/// Edition-2024 shim: `std::env::set_var` is `unsafe` there, because
	/// mutating the process environment can race another thread reading it.
	/// Tests that mutate the environment hold [`env_lock`] for their whole
	/// body, which is the invariant these wrappers rely on.
	#[allow(unsafe_code)]
	pub(crate) fn set_var<K: AsRef<std::ffi::OsStr>, V: AsRef<std::ffi::OsStr>>(key: K, value: V) {
		// SAFETY: callers hold `env_lock()`.
		unsafe { std::env::set_var(key, value) }
	}

	/// Edition-2024 shim for `std::env::remove_var`; see [`set_var`].
	#[allow(unsafe_code)]
	pub(crate) fn remove_var<K: AsRef<std::ffi::OsStr>>(key: K) {
		// SAFETY: callers hold `env_lock()`.
		unsafe { std::env::remove_var(key) }
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn hello_returns_crate_name() {
		assert_eq!(hello(), "headroom-core");
	}
}
