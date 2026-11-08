# Changelog - PlayForm/Headroom fork

This is the **fork's release-tracking changelog** for the PlayForm/Headroom
repository (`vendor/headroom` submodule of PlayForm/Aphrodite, branch
`Current`, remote `Source`).

It tracks the fork's **publishable crate**: `aphrodite-headroom-core`
(`crates/headroom-core/Cargo.toml`, `[lib] name = "headroom_core"`), published
to crates.io and pinned by the parent Aphrodite repo
(`crates/aphrodite/Cargo.toml` line 55).

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Each
version entry lists the fork commit range between crate-version bumps; a
version counts as "published" only when it actually reached crates.io. The
release-cycle contract lives in [RELEASE-CYCLE.md](RELEASE-CYCLE.md).

> **Note:** this file replaces the vendored upstream
> `headroomlabs-ai/headroom` CHANGELOG.md that arrived with the Aug 2026
> upstream syncs. The upstream file is preserved in git history
> (`git show HEAD:CHANGELOG.md` inside the fork).

## [0.2.0] - unreleased

Range: `aphrodite-v0.10.0` (`02706ea1`, 2026-09-19) .. working tree. Carries the
two dependency-update commits that followed the 0.1.3 tag plus the CCR-store
retention work (uncommitted at the time of writing).

### Store retention (new API)

- `SqliteCcrStore::open_with_limits(path, default_ttl_seconds, max_entries,
  max_bytes)` - `open` / `open_with_ttls` keep their signatures and apply no
  cap; `None` or `0` means unlimited, normalised inside the constructor.
- Past either cap the store evicts least-recently-accessed rows
  (`ORDER BY last_accessed ASC, created_at ASC`), in bounded batches of 256
  rows per statement, with exact freed rows/bytes from
  `DELETE … RETURNING LENGTH(original)`. An already-oversized DB is trimmed at
  open, not only on the next `put`.
- Accessors `max_entries()`, `max_bytes()`, `evicted_total()`; `stats_db()`
  gained `max_entries`, `max_bytes`, `evicted_total`, `freelist_bytes`.
- Maintained `entries_total` / `live_bytes` counters make `len()` O(1) and let
  cap enforcement skip a `COUNT(*)` per write. Caveat: the counters are
  per-process, so caps are approximate when several processes share one DB
  file; `stats_db` always reports exact `COUNT(*)` / `SUM(LENGTH(original))`.

### Performance

- `get` no longer runs the expiry sweep on every call. It shares the 60 s
  debounce `put` already had, and the lookup predicate keeps refusing expired
  rows, so skipping a sweep delays space reclamation but never serves stale
  content. Measured `get` on a missing hash: 0.15 ms (empty), 27 ms (100k
  rows), 37.40 ms (300k rows) before; 0.22 ms at 300k rows after.

### Schema

- `user_version` 2 → 3. New indexes: `ccr_entries_expiry` (expression index on
  `last_accessed + ttl_seconds`), `ccr_entries_created`, `ccr_entries_lru`.
  The sweep predicate was rewritten as `created_at < cutoff` so SQLite's
  OR-optimisation drives both disjuncts from an index.

### Packaging / deps

- `fc58ee39` - `fix(dependencies): update fastembed version to 5 in Cargo.toml`.
- `41e3602b` - `chore(dependencies): update various crate versions in Cargo.toml`.
- **Fork workspace build fix**: `headroom-parity` imported
  `headroom_core::transforms::kompress` unconditionally while headroom-core
  gates it behind `ml` (default-OFF since `d9f12039`), so a plain
  `cargo build` in `vendor/headroom` failed with `could not find kompress in
  transforms` on every commit after 2026-08-07 - CI never caught it because
  the parent builds only `headroom-core` through a path dependency. The
  comparator and its registration are now `#[cfg(feature = "ml")]`, and
  `headroom-parity` declares `ml = ["headroom-core/ml"]` so the kompress leg
  is opt-in instead of unreachable. Note: `headroom-parity` is still not
  rustfmt-conformant on its own - the crate is space-indented while the
  inherited `rustfmt.toml` sets `hard_tabs = true`, so
  `cargo fmt --check -p aphrodite-headroom-parity` reports the whole file.
  That predates this change and was left alone rather than turning a build
  fix into a 984-line whitespace rewrite.
- **Fix (fork workspace lint)**: `smart_crusher/stats_math.rs` used
  `n.is_multiple_of(2)` (stable since Rust 1.87) under a workspace
  `rust-version = "1.80"`, so `cargo clippy -- -D warnings` in the fork
  workspace failed on `clippy::incompatible_msrv` - invisible from the parent,
  where the same crate inherits the parent's higher MSRV. Now `n % 2 == 0`;
  clippy is clean for `headroom-core` and `headroom-parity` in the fork
  workspace.

## [0.1.3] - 2026-09-20

Range: `c6b61470..HEAD` (fork HEAD `02706ea1`, 2026-09-19) - 18 fork
commits on the fork mainline (107 commits in the full DAG, incl. upstream
syncs). Published to crates.io 2026-09-20 (`max_version` = 0.1.3); fork tag
`aphrodite-v0.10.0`.

### Upstream sync (headroomlabs-ai/headroom@main)
- `b1932981` (2026-07-14) - merge upstream/main `c365c7ff`: 87-commit sync
  (228 files, +14718/−4187): proxy policy extraction/refactor batch, OpenAI
  Responses output shaping, input-savings-rate `/stats`, provider simulators
  (new `headroom-simulators` crate), C# CodeAwareCompressor, semantic-cache
  context-hash keys, magika detector rework.
- `43dc9836` (2026-08-07) - squash-sync of upstream main (fork was 396 behind
  / 31 ahead; 786 files, +107713/−25067): upstream CHANGELOG (+959), workspace
  Cargo.toml churn, CI/workflow updates, detection and proxy changes.

### Packaging / deps
- `682ebd0d` (2026-07-19) - exact version ranges in `package.json` files
  (docs, sdk/typescript, opencode/openclaw plugins).
- `d9f12039` (2026-08-07) - default the `ml` feature OFF in
  `headroom-core`'s Cargo.toml.
- `c18fbbb1` + `39ae7079` (2026-08-15) - package-name churn: temporary rename
  to `aphrodite-headroom` across all five crates, immediately reverted to
  `aphrodite-headroom-core` (no version bump; nothing published under the
  wrong name).
- `84c8d117` (2026-09-16) - pin the ml cluster to known-good (hf-hub 0.5,
  fastembed 5, ort rc.12); fastembed 7 / ort rc.13 conflict with magika ^1.
- `1f80236c` (2026-09-18) - remove the nested vendored headroom submodule.

### Nightly-toolchain migration (Sep 2026 batch)
- `4bc1d366` (2026-09-19) - `chore(format)`: 26 files, +7623/−8218 format
  normalization across smart_crusher, text_crusher, transforms, tests.
- `3a3573a1` (2026-09-19) - normalize em-dashes and clippy pattern bindings
  across headroom-core transforms (anchor_selector, kompress,
  search_compressor, smart_crusher/outliers).
- `be61a6de` (2026-09-19) - restore closure signatures in kompress and
  search_compressor (fixes behavior broken by the earlier syntax migration).
- `2c6c68b3` (2026-09-19) - migrate `no_mangle` attributes to edition-2024
  unsafe syntax in headroom-ffi.
- `02706ea1` (2026-09-19) - suppress nightly clippy lints in the vendored
  fork (headroom-core lib.rs).

### Tests
- `5fda221a` (2026-09-18) - restructure kompress parity tests for readability
  (`kompress_parity.rs`, ~±180 lines).

### Fixes
- `4385995a` (2026-09-16) - content_detector no longer misclassifies idiomatic
  Go source as build output (Rust + Python).

### Misc / format
- `a2b90b65` (2026-07-14) - merge fork `Current` ↔ remote `Current`.
- `fb4b7d98` (2026-07-14) - formatting consistency in vulnerability scan
  reports (SBOM tables, pyproject reflow).
- `b1b135a6` (2026-07-14) - `chore(format)`: 104 files, ~±25k format sweep.

## [0.1.2] - 2026-07-14

Range: `e6c3df82..c6b61470` (4 commits). Published to crates.io 2026-07-14 as
`aphrodite-headroom-core`; repository URL fixed to
`https://github.com/PlayForm/Headroom`.

- `71e7fc23` (2026-07-13) - sqlite CCR backend: `put`-side lazy purge, schema
  versioning, busy timeout.
- `eba5380f` (2026-07-14) - standardize punctuation in comments across
  in-memory, SQLite, and stats_math modules.
- `661bd292` (2026-07-14) - refactor tests for consistency and readability.
- `c6b61470` (2026-07-14) - update repository URL and bump version to 0.1.2.

## [0.1.1] - 2026-07-13

Range: `ee943971..e6c3df82` (fork inception 2026-06-19 → 0.1.1 bump; 16 fork
mainline commits, 356 commits in the full DAG). First crates.io publish of
the fork crate, as `aphrodite-headroom-core`.

- Initial fork (`ee943971`, 2026-06-19): PlayForm fork of
  headroomlabs-ai/headroom with the Rust compression engine
  (`headroom-core` + `headroom-ffi` C ABI, `headroom-py`, `headroom-proxy`,
  `headroom-parity`).
- Upstream sync `e3fe15c3` (2026-06-22): v0.27.0 - Cortex Code, ONNX embedder,
  tokenizer fix, tree-sitter pin, Anthropic forwarding.
- `b8885d7a` (2026-07-03) - rename packages to the `aphrodite-` prefix (crate
  becomes publishable as `aphrodite-headroom-core`).
- Config/TTL/savings-ratio fixes and test updates (2026-07-05/10/11).
- `e6c3df82` (2026-07-13) - bump version to 0.1.1.