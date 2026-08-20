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

Use the pinned toolchain from the repository and opt in explicitly:

```sh
cargo build -Zcargo-cas --locked
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
keep its JSON history outside the repository. It reports both raw target-plus-
cache accounting and a CoW-aware estimate; macOS clone-on-write restores make
the raw number look larger than the physical shared footprint.

## Interpreting results

Only native-host, pure-Rust `lib`/`rlib` build units are currently eligible.
Bins, examples, tests, build scripts, proc macros, non-host targets, and root
application outputs remain local. A cache hit materializes the artifact back
into Cargo's normal target directory, so a successful hit does not make
`target/` empty. Cache `miss` means no entry was available; `reject` means a
present entry failed validation and Cargo safely compiled the unit normally.

As a verified reference point, the committed `epsh`/`ish` configuration
produced fresh vanilla targets of about 45 MiB and 56 MiB. The corresponding
cargo-cas targets were about 44 MiB and 55 MiB, with a 29 MiB shared cache;
`epsh` reported 9 eligible misses and `ish` reported 5 hits, 1 miss, and 8
conservative rejects. The smaller cache after profile, lockfile, and feature
alignment is real improvement, but root binaries and artifact-name validation
still bound the savings. Inspect `CARGO_LOG=cargo::compiler::cas=debug` when a
hit rate or footprint is surprising.
