# Configuring a repository for cargo-cas

`cargo-cas` is an immutable compiler-artifact cache. It does not replace
Cargo's target directory, fingerprints, final binaries, test harnesses, build
scripts, proc macros, or incremental state. Configure a repository so that the
pure-Rust library units it wants to share have the same compiler contract in
every checkout.

## Repository checklist

1. Disable incremental compilation for the profile you build with. Incremental
   units are deliberately ineligible:

   ```toml
   [profile.dev]
   incremental = false
   ```

2. Use the same effective profile in the producer and consumer. Keep
   `opt-level`, debuginfo, split debuginfo, overflow/debug assertions, and
   package profile overrides aligned. For example:

   ```toml
   [profile.dev]
   opt-level = 2
   debug = "line-tables-only"
   split-debuginfo = "unpacked"
   incremental = false
   ```

3. Make local sibling dependencies explicit and reproducible. A consumer that
   is meant to use the checkout beside it should say:

   ```toml
   epsh = { path = "../epsh" }
   ```

   Commit both repositories. The cache keys local Git worktrees by repository,
   revision, relative package path, and a source snapshot; uncommitted source
   changes therefore make a different action identity.

4. Keep lockfiles and feature sets aligned. A dependency compiled with a
   different locked version or feature union is a different compiler action.
   In the `epsh`/`ish` pair, aligning `bitflags` and enabling the `rustix`
   feature superset used by `ish` made the shared dependency graph reusable.

5. Avoid wrappers and target configurations that change compiler inputs unless
   every build uses the same values. Rustflags, linker selection, toolchain,
   enabled Cargo unstable features, and manifest lint settings are part of the
   cache contract; an unsupported wrapper makes a unit ineligible.

## Build and benchmark

Use the pinned toolchain from the repository:

```sh
cargo build --locked
```

For an apples-to-apples measurement, use fresh target directories and an
isolated `CARGO_HOME` (with the registry source cache symlinked read-only), then
run the same command once with upstream Cargo and once with cargo-cas. Record
elapsed time, each target directory, and `$CARGO_HOME/cache/cargo-cas-v1`.

The four-worktree harness can target another checkout:

```sh
CARGO_CAS_PROJECT_DIR=$HOME/d/ish \
CARGO_CAS_EDIT_FILE=src/main.rs \
CARGO_CAS_REBUILD_ROUNDS=2 \
python3 scripts/benchmark-ish-cas.py
```

The harness builds `test --workspace --all-targets --all-features --no-run`,
which is intentionally broader than `cargo build`. Set `CARGO_CAS_RESULTS` to
keep its JSON history outside the repository. It reports logical size,
per-tree apparent allocation, and the actual allocated footprint with
hardlinked inodes counted once. Its separate CoW-aware estimate is diagnostic
only; do not use it as the storage result.

## Interpreting results

Only native-host, pure-Rust `lib`/`rlib` build units are currently eligible.
Bins, examples, tests, build scripts, proc macros, non-host targets, and root
application outputs remain local. A cache hit materializes the artifact at
Cargo's normal target path, so a successful hit does not make `target/` empty.
On the same filesystem, validated `.rmeta` and `.rlib` files are read-only
hardlinks to the immutable cache entry; Cargo-local dep-info and diagnostics
remain target-local. A target therefore continues to work after cache GC or a
cache-entry deletion, and a later dirty build detaches the readonly output
before rustc writes it. Cache `miss` means no entry was available; `reject`
means a present entry failed validation and Cargo safely compiled the unit
normally.

For a completed linkable `.rlib` entry, Cargo also removes only that library's
matching adjacent `*.rcgu.o` intermediates. Those objects are already members
of the archive; fingerprints, dep-info, metadata, final binaries, and all
non-cacheable output stay local.

Measure storage across the full target-plus-cache set. Report both logical
file length and filesystem-allocated blocks, counting a hardlinked inode once
across that set. Do not use clonefile/CoW estimates as the storage result.
Use explicit GC to bound the global cache, for example:

```sh
cargo clean gc -Zgc --max-cas-size=20GiB
```

## End-of-build statistics

Every completed `build`, `check`, or failed compilation emits one
`cargo-cas summary` event by default. Set `CARGO_LOG=cargo::compiler::cas=debug`
to include the per-action diagnostics as well. In addition to `eligible`,
`hits`, `misses`, `rejects`, `eligible_rustc`, `duplicate_build_avoidance`, and
`skips`, the summary reports the top three miss reasons and logical/allocated
bytes added and removed from both Cargo's target-owned unit paths and the global
CAS store (plus signed `*_delta` values). Rejects have their own top-three reason
list so malformed entries are distinguishable from ordinary absent-entry misses.

Set `CARGO_LOG=off` to disable the summary and all of its additional bookkeeping;
path snapshots, accounting locks, reason maps, and publication byte metadata
are then skipped.

These byte values are mutation deltas, not `du`, `df`, or a directory snapshot.
Target deltas are recorded around each Cargo unit's known output and fingerprint
paths while Cargo's build/artifact or fine-grained unit lock excludes another
writer; shared-lock checks use a dedicated target accounting lock. CAS
publication deltas are committed only by the process whose atomic rename wins;
hardlink materialization adds target logical bytes but no physical allocation.
The counters are therefore safe to compare across overlapping Cargo processes:
a concurrent reader cannot charge the writer's cache entry to its own
invocation.

The checked-in `scripts/demo-path-cas.py` is the reference fresh-run harness
for the committed `epsh`/`ish` pair. On the pinned nightly it measured 100.81
MiB vanilla allocated storage versus 52.85 MiB cargo-cas storage (C = 0.524),
with hardlinked inodes counted once across both targets and the cache. The CAS
targets had 79.81 MiB logical data, the cache had 35.22 MiB logical data, and
their apparent allocations were intentionally higher because both names refer
to some of the same immutable inodes. `cargo clean gc -Zgc --max-cas-size=0`
then reduced that isolated 35.32 MiB cache to zero; existing target artifacts
remain valid. Inspect `CARGO_LOG=cargo::compiler::cas=debug` when a hit rate or
footprint is surprising.
