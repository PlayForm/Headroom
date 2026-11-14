# Release-Cycle Contract - PlayForm/Headroom fork (`vendor/headroom`)

Tracking contract for the fork's publishable crate `aphrodite-headroom-core`
(`vendor/headroom/crates/headroom-core/Cargo.toml`, `[lib] name =
"headroom_core"`). The parent Aphrodite release workflow owns the publishing
gates (`aphrodite-release-workflow` skill, `references/headroom-publish.md`);
this file is the **fork-side ledger**: every release cycle maps crate version
↔ fork gitlink commit ↔ fork release tag ↔ parent pin ↔ crates.io published
state.

## 1. The mapping table (one row per crate version)

| Crate version | Fork commit (gitlink tree) | Fork release tag | Parent pin (`crates/aphrodite/Cargo.toml` :55) | crates.io |
|---|---|---|---|---|
| 0.1.0 | `1946b1cb` (2026-06-20) | `aphrodite-v0.9.0` / `v0.9.2` / `v0.9.3` / `v0.9.4` (all four retroactively at this commit) | - (pre-publish) | never published |
| 0.1.1 | `e6c3df82` (2026-07-13) | - (no fork tag) | `version = "0.1.1"` (historical) | published 2026-07-13 |
| 0.1.2 | `c6b61470` (2026-07-14) | - (no fork tag) | `version = "0.1.2"` (committed through `Aphrodite/v1.5.0`, 2026-09-20) | published 2026-07-14 |
| 0.1.3 | `02706ea1` (2026-09-19) | `aphrodite-v0.10.0` | `version = "0.1.3"` | published 2026-09-20 - **max_version before 0.2.0** |
| 0.2.0 | `d4558b7c` (2026-11-08; merge of upstream `bb8d18a5` carrying store retention + index-driven sweep + fork-workspace build/lint fixes) | - (no fork tag) | `version = "0.2.0"` (committed through `Aphrodite/v1.8.0`) | published 2026-11-08 - **max_version before 0.2.1** |
| 0.2.1 | `SELF` (2026-11-14; `rusqlite` 0.40.2 -> 0.41 / `libsqlite3-sys` 0.39.1 for RUSTSEC-2026-0459; no API change) | `aphrodite-v0.12.0` (to cut) | `version = "0.2.1"` | published with `Aphrodite/v1.8.1` |

Rules:

- **"Published"** = the version is live on crates.io. Check before claiming a
  number: index `https://index.crates.io/ap/hr/aphrodite-headroom-core`
  (404 = not published), API `https://crates.io/api/v1/crates/aphrodite-headroom-core`
  → `max_version`.
- The **fork gitlink commit** is the tree CI actually publishes - not the
  local submodule HEAD (§4 gitlink trap).
- **Fork release tags** (`aphrodite-vX.Y.Z`) are the fork's release-notes
  markers. The four existing tags are retroactive (all point at `1946b1cb`,
  crate 0.1.0); the next release tag must point at the fork commit whose tree
  carries the new crate version.

## 2. Canonical failure example - the 1.5.0 gap (2026-09-20)

The `Aphrodite/v1.5.0` release (tagged 2026-09-20) published `aphrodite
1.5.0` + `aphrodite-hermes 1.5.0` but **skipped headroom**:

- crates.io `max_version` was already `0.1.2`, so CI's headroom
  version-check saw "already live" and skipped the publish step.
- The gitlink recorded at the release tag was `02706ea1` - fork HEAD,
  **18 fork commits + two upstream syncs past the published 0.1.2 tree**
  (`c6b61470`, 2026-07-14): the 87-commit merge `b1932981` and the
  396-commit squash `43dc9836`. `c6b61470..02706ea1` = 107 commits in the
  full DAG.
- The parent pin stayed `0.1.2`, so `cargo publish -p aphrodite` resolved
  its path+version dep against the live `0.1.2` and succeeded - masking the
  gap.

Consequence: registry consumers resolving `aphrodite-headroom-core = "0.1.2"`
get the 2026-07-14 tree; ~2 months of fork changes (ml feature default-off,
ml dependency pins, content-detector fix, nightly-toolchain migration) never
reached crates.io.

Root cause: the fork's **committed** crate version (0.1.2) was never bumped
ahead of the release, so CI correctly treated the crate as already published.
Fix: bump the fork crate version (and float gitlink + pin + tag) in the same
cycle as the parent release - **before** dispatch.

## 3. Mandatory sequence (per release cycle)

1. **Bump the fork crate version** in
   `vendor/headroom/crates/headroom-core/Cargo.toml` (e.g. `0.1.2` → `0.1.3`)
   - nothing else publishes a new version.
2. **Commit in the fork** (branch `Current`, remote `Source`).
3. **Float the parent gitlink**: `git add vendor/headroom` + commit in the
   parent. CI checks out the submodule at the parent-recorded gitlink, NOT the
   local submodule HEAD - skipping this publishes the previous commit's tree.
4. **Bump the parent pin** (`crates/aphrodite/Cargo.toml` line 55,
   `version = "..."`) to match the new crate version - `cargo publish
   -p aphrodite` fails if that version is not live on crates.io.
5. **Create the fork release tag** `aphrodite-vX.Y.Z` (next proposed:
   `aphrodite-v0.10.0`) at the fork commit carrying the new version; parent
   binary tag (`Aphrodite/vX.Y.Z`) as usual.
6. **Dispatch the publish**: `gh workflow run Publish -f publish_crates=true`.
   `publish_crates=true` is mandatory. Headroom publishes **FIRST** in the
   hard `needs:` chain: Test → Publish-Headroom-Core → Publish-Aphrodite →
   Publish-Hermes. A plain tag push NEVER publishes headroom (publish step is
   gated on `workflow_dispatch && inputs.publish_crates && published == 'false'`).
7. **Verify from the consumer side**: index serves the exact version
   (`curl -fsS https://index.crates.io/ap/hr/aphrodite-headroom-core`), then
   `cargo update -p aphrodite-headroom-core --precise <version>` and
   `cargo tree -i aphrodite-headroom-core` in a clean checkout.

Every irreversible event (commit, push, tag, dispatch, registry publish)
pauses for explicit human approval per the Aphrodite boundaries - technical
readiness never authorizes the side effect.

## 4. Gitlink trap

CI publishes the **parent-recorded gitlink tree**. To publish a changed
headroom tree the parent gitlink must float first (step 3). Skipping it
publishes the previously-recorded commit's tree - the 1.5.0-class gap.

## 5. Current state (live, captured 2026-11-08)

| Item | Value |
|---|---|
| Fork HEAD | `d4558b7c` (2026-11-08; merge commit, parents `62b09ccf` × `bb8d18a5` - the upstream sync carrying the store-retention line) |
| Crate version @ fork HEAD | `0.2.0` (committed in `20be515b`) |
| Crate version, fork working tree | `0.2.0` (clean) |
| Unpublished since 0.1.3 | `02706ea1..HEAD` = 5 commits (`41e3602b`, `fc58ee39` dep updates; `20be515b` store retention / fork-workspace fixes; `152a1760`, `62b09ccf` deps/changelog + `d4558b7c` upstream sync) |
| Parent gitlink (committed) | `62b09ccf` - **one commit behind fork HEAD; the float to `d4558b7c` is pending** |
| Parent pin, committed | `0.2.0` |
| Parent pin, working tree | `0.2.0` (clean) |
| Parent binary | committed `1.8.0` (untagged); `BINARY_VERSION` still `1.7.0` by design |
| Fork tags | last = `aphrodite-v0.10.0` (on `02706ea1`, crate 0.1.3) |
| Next fork tag | `aphrodite-v0.11.0` (proposed, on the commit carrying 0.2.0) |
| crates.io | `max_version = 0.1.3` (`0.1.1`, `0.1.2`, `0.1.3` live; `0.1.0` never published) |
| Next pending cycle | crate `0.2.0` (committed) → publish to crates.io → fork tag `aphrodite-v0.11.0` → float parent gitlink → parent tag. The CI chain never publishes this crate; the publish is manual and must precede the parent tag |

## 6. Non-negotiables

- **crates.io versions are immutable**: never re-tag, never reuse a burned
  version - a wrong published version ⇒ release the next number.
- **Package name stays `aphrodite-headroom-core`**: a rename orphans the
  parent pin, breaks sibling `package =` aliases, and burns the crates.io
  namespace history (the Aug-15 rename-away-and-back, `c18fbbb1` /
  `39ae7079`, is the cautionary tale).
- Check version availability before claiming a number (index 404 or
  `max_version` absent).
- `Cargo.lock` regenerates on the next build; no manual edit needed.