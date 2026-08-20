# `cargo-cas` Gate 0: Cargo architecture archaeology

This document records the Cargo implementation that the experiment is built
against. It is intentionally a map of the existing contracts, not a proposed
replacement fingerprint implementation.

## Scope and base revision

The verified upstream Cargo master revision is:

```text
514c56dd7321eecbfdcf9b6479519cf4edfab906
```

The Cargo source files in this checkout were compared with that fetched
revision; the repository's `f270c5830` `init` commit is a project snapshot
whose durable addition is `plan.md`, not a substitute for recording the
upstream SHA. The fetched Cargo history contains `36e1162a7` (#17354,
“Re-stabilize build-dir layout v2”). The experiment is currently macOS-only.
Paths, compiler output, file
locking, and artifact-byte observations in later gates must therefore be
validated on the macOS filesystem/toolchain matrix before widening the scope.

The current Cargo source still contains the legacy layout implementation, but
the new layout is the default unless the temporary
`__CARGO_TEMPORARY_BUILD_DIR_NEW_LAYOUT_OPT_OUT=1` escape hatch is set. The
`-Zfine-grain-locking` setting also implicitly enables the new layout.

The base was checked without changing the repository remotes:

```text
git fetch --no-tags https://github.com/rust-lang/cargo.git master
git rev-parse FETCH_HEAD
git diff FETCH_HEAD..HEAD -- ':!plan.md' ':!CARGO_CAS.md'
```

The fetch/rev-parse pair returned the SHA above. The source-tree comparison was
clean for the Cargo files before implementation edits; `HEAD` itself is the
project snapshot and must not be used as the upstream base identifier.

## Design lineage

These are the relevant upstream decisions, in the order that they constrain a
global compiled-artifact cache. Links are included so a future change can
re-read the design discussion rather than infer behavior from this document.

| Area | Upstream issue/PR | Revision or status | What it establishes |
| --- | --- | --- | --- |
| Global compiled-artifact cache | [#5931](https://github.com/rust-lang/cargo/issues/5931) | Tracking issue | The north-star goal: reuse compiled dependencies across workspaces. |
| Separate intermediate and final output | [#14125](https://github.com/rust-lang/cargo/issues/14125) | `5874b6f34` | `build-dir` is intermediate compiler/Cargo state; `target-dir` is user-facing output. |
| Build-dir layout v2 | [#15010](https://github.com/rust-lang/cargo/issues/15010), [#15947](https://github.com/rust-lang/cargo/pull/15947) | `c169b66ab` | Organizes intermediate files by Cargo build unit (`name/hash`), making a unit a useful locking/cache boundary. |
| First stabilization | [#16807](https://github.com/rust-lang/cargo/pull/16807) | `b26e1b2f7` | Stabilized the layout while retaining legacy code and a transition escape hatch. |
| Search-path scaling | [#17168](https://github.com/rust-lang/cargo/pull/17168), [#17191](https://github.com/rust-lang/cargo/pull/17191), [#17236](https://github.com/rust-lang/cargo/pull/17236) | `dd5697130`, `6ff645cb1`, `da97fca14` | v2 recursively supplies only needed `-L` paths, avoids the current unit/build-script output, and avoids proc-macro dependencies in the path list. |
| Nightly rollout and restabilization | [#17258](https://github.com/rust-lang/cargo/pull/17258), [#17354](https://github.com/rust-lang/cargo/pull/17354) | `95303e660`, `36e1162a7` | v2 was enabled on nightly, scaling fixes landed, and v2 was re-stabilized. |
| Fine-grained locking | [#4282](https://github.com/rust-lang/cargo/issues/4282), [#16089](https://github.com/rust-lang/cargo/issues/16089), [#16155](https://github.com/rust-lang/cargo/pull/16155) | `abaa830fa` | Shared read lock for freshness, exclusive per-unit lock for a dirty build, then shared read lock after publication. The earlier experiment is superseded. |
| Artifact/build lock split | [#16307](https://github.com/rust-lang/cargo/pull/16307), [#16708](https://github.com/rust-lang/cargo/pull/16708) | `2a1789fcc`, `6cea00ce0` | `artifact-dir` and `build-dir` have separate lock files; check builds do not need the artifact lock. |
| Moving build state to Cargo home | [#16147](https://github.com/rust-lang/cargo/issues/16147) | Proposal/tracking discussion | A possible future location for shared build state; this experiment must not silently turn the normal workspace `build-dir` into a global mutable directory. |
| Build-script final-artifact staging | [#13663](https://github.com/rust-lang/cargo/issues/13663) | Open design constraint | Build scripts produce arbitrary host-observed/generated state; they are not a safe V0 global-cache unit. |
| Global source-cache GC | [#12633](https://github.com/rust-lang/cargo/issues/12633) | Implemented in `9a1b0924c`/#12634 | `GlobalCacheTracker` tracks Cargo home source-cache use. This is not artifact GC. |
| Build-artifact GC | [#13136](https://github.com/rust-lang/cargo/issues/13136) | Tracking issue | Build artifacts are not covered by the existing global-cache tracker; CAS GC is later work. |

The current docs make the same boundary explicit: `build-cache.md` describes
final artifacts in `target-dir`, intermediate artifacts in `build-dir`, and
incremental output under the build directory; the `[cache]` docs say artifact
tracking is not yet implemented. `-Zchecksum-freshness`, `--unit-graph`,
`-Zfine-grain-locking`, and the build-dir v2 documentation are in
`doc/book/src/reference/unstable.md` and `doc/book/src/reference/build-cache.md`.

## Build-dir v2 layout

`Workspace::target_dir` and `Workspace::build_dir` are resolved in
`src/workspace/workspace.rs`. Without explicit configuration, normal workspaces
use `<workspace-root>/target` for both. `build.build-dir` can separate them and
supports `{workspace-root}`, `{cargo-cache-home}`, and
`{workspace-path-hash}`. The latter is a workspace-identity layout aid, not a
compiled-artifact semantic key.

`src/compiler/layout.rs::Layout::new` then adds the optional target short name
and profile directory. In the v2 layout, the effective tree is:

```text
<build-dir>/
    .rustc_info.json                 # cached rustc -vV information
    [<target>/]<profile>/
        .cargo-build-lock            # build-dir compatibility/concurrency lock
        incremental/                  # mutable rustc incremental state
        build/                        # v2 build-unit tree
            <package-name>/
                <unit-hash>/
                    .lock             # one fine-grained unit lock
                    out/              # rustc output for this unit
                    fingerprint/     # Cargo fingerprint and translated dep-info
                    run/              # build-script execution state
                    artifact/<kind>/ # artifact dependency output, when applicable
    package/                          # package/publish intermediates
    .metabuild/                       # generated build-script support

<target-dir>/
    [<target>/]<profile>/
        .cargo-lock                   # artifact-dir compatibility lock
        .cargo-artifact-lock          # exclusive lock for commands that uplift
        <final artifacts>
    examples/                         # final examples
    doc/                              # rustdoc output
```

The comments in `src/compiler/layout.rs` are the authoritative tree
description. Important implementation details are in
`src/compiler/build_runner/compilation_files.rs`:

* `CompilationFiles::pkg_dir` uses `name/unit-hash` in v2 (a hyphen in the
  legacy layout). `unit_hash` is the `Metadata::unit_id` when v2 is enabled.
* `BuildDirLayout::deps` maps a unit to `build/<name>/<hash>/out` in v2.
  This is the unit's compiler output directory; v2 does not use one giant
  `build-dir/deps` directory for normal units.
* `BuildDirLayout::fingerprint` maps to that unit's `fingerprint` directory.
* `BuildDirLayout::build_script` maps a compiled build script to `out`, and
  `build_script_execution` maps the execution unit to `run`.
* `BuildDirLayout::artifact` maps artifact-dependency output to
  `artifact/<kind>` inside the unit directory.
* `CompilationFiles::build_unit_lock` maps to the unit directory's `.lock`.
* `BuildDirLayout::incremental` remains the shared
  `<target>/<profile>/incremental` root, because rustc owns the internal
  crate/session subdirectories.

`CompilationFiles::output_dir` chooses the output class. Normal v2 compilation
outputs go to the unit's `out`; final root artifacts and other special outputs
are handled separately. `BuildRunner::prepare_units` creates `Layout`s and
`BuildRunner::prepare` creates the directories. `BuildRunner::check_collisions`
checks both intermediate output paths and final hardlink/export paths before
jobs run.

The old layout code remains in `Layout` for compatibility, but Gate 0 and all
initial CAS experiments target v2. Do not use a path from the legacy layout as
the cache contract.

## Cargo `Unit` and unit graph identity

`src/compiler/unit.rs` defines `UnitInner` as the information needed to invoke a
compiler for one package target. Its fields are:

* `pkg: Package`, including `PackageId`, manifest/target declarations,
  dependency and feature information;
* `target: Target`, including target name, crate name/type, source path,
  edition, harness and host/build-script properties;
* `profile: Profile`, including optimization, debuginfo, assertions,
  overflow, panic, LTO-related settings, linker/codegen settings, trim paths,
  and incremental state;
* `kind: CompileKind` (host or a target triple/custom JSON target);
* `mode: CompileMode` (`Build`, `Check`, `Test`, `Doc`, `RunCustomBuild`,
  etc.);
* sorted enabled `features`;
* unit `rustflags` and `rustdocflags`;
* `links_overrides`, artifact-dependency state, `is_std`, `dep_hash`,
  target-dependent feature state, and the compile-time-dependency skip bit.

`UnitInner` derives `Hash`, `Eq`, and ordering over those fields. `Unit` itself
is an interned `Rc<UnitInner>`: its `Hash` and equality intentionally use the
pointer for fast graph-local lookup. `UnitInterner::intern` guarantees that
equivalent `UnitInner` values share one pointer. This is an implementation
identity for one Cargo process, not a persistent cross-workspace cache key.

The unit graph is a `HashMap<Unit, Vec<UnitDep>>` in
`src/compiler/unit_graph.rs`. A `UnitDep` carries the dependency unit, the
edge purpose, extern crate name, manifest dependency name, public/noprelude/
nounused flags, and an unhashed manifest-dependency bookkeeping field. The
extern name and visibility are semantically relevant to the parent invocation.

`src/ops/cargo_compile/mod.rs::rebuild_unit_graph_shared` recursively traverses
the graph. `traverse_and_share` hashes the resulting dependency `Unit`s into
`dep_hash`, canonicalizes target-host units to `CompileKind::Host` when safe,
and reinterns each `Unit`. This is why a dependency's features or dependency
kind can produce distinct units even when package/version are equal. The graph
is then frozen and `UnitIndex` values are assigned by sorted graph order.

`UnitIndex` is only a per-invocation diagnostic/scheduling index. It must never
be part of an ActionKey. The `--unit-graph` JSON is useful for inspection but
is versioned output, not a complete persistent compiler invocation contract:
it intentionally omits internal fields such as effective flags and hashes.

`PackageId` is name + version + `SourceId`. Registry packages have a stable
source URL/kind and an index-provided crate checksum. `SourceId::stable_hash`
removes the workspace prefix only for path sources; registry and git identity is
based on source URL/revision semantics. The registry source's
`Source::fingerprint` currently returns only the package version. In particular,
the registry checksum is used to verify downloads but is not directly included
in Cargo's normal `Fingerprint`; a global key must include the immutable source
checksum (or an equivalent content identity) explicitly.

## Existing freshness algorithm

The compile scheduler is assembled by `src/compiler/mod.rs::compile`:

1. It de-duplicates the unit in `BuildRunner::compiled`.
2. With fine-grained locking, it takes a shared unit lock before inspecting
   freshness.
3. `fingerprint::prepare_init` creates the unit fingerprint directory.
4. `fingerprint::prepare_target` calls `calculate`, compares the current
   fingerprint against the short hash at the fingerprint path, and checks the
   filesystem state.
5. A fresh unit gets a no-op job that replays cached diagnostics and runs the
   normal target-uplift work. A dirty unit gets a rustc/rustdoc job followed by
   fingerprint persistence and target uplift.
6. Dependencies are recursively enqueued into `JobQueue`. `JobQueue` preserves
   `Metadata` versus `All` dependency edges so a downstream rlib can start once
   an upstream `.rmeta` exists while a binary waits for the full linkable
   artifact.

`src/compiler/fingerprint/mod.rs::calculate_normal` constructs a
`Fingerprint` with these inputs:

* full `rustc -vV` text;
* target hash (`Target`, including package-relative source path, edition, and
  target properties);
* profile/mode/extra compiler args/LTO/manifest lint flags and trim-paths
  workspace-remap state;
* source path hash from `path_args`;
* enabled and declared feature sets;
* recursive dependency fingerprints (binaries are skipped except artifact
  dependencies; dependency edges carry package id, extern name, public bit,
  and the recursive fingerprint hash);
* effective rustflags/rustdocflags;
* a config hash for linker, rustdoc extern mapping, allow-features,
  public-dependency behavior, and embedded-metadata mode;
* `CompileKind` fingerprint hash.

`Fingerprint::hash` deliberately excludes fields that are not inputs to the
semantic rebuild decision: output paths, filesystem status, unit index, and the
memoization field. The persisted representation is:

```text
<fingerprint-dir>/<kind>-<target>             # 16-hex short hash
<fingerprint-dir>/<kind>-<target>.json       # detail for dirty diagnostics
<fingerprint-dir>/dep-<kind>-<target>        # Cargo-translated dep-info
<fingerprint-dir>/invoked.timestamp          # build start timestamp
<fingerprint-dir>/output-<kind>-<target>     # cached compiler messages
```

The actual names are generated by `CompilationFiles::fingerprint_file_path`;
the target-kind/flavor prefix avoids collisions between lib/bin/test/doc/run
units.

Freshness requires both conditions in `_compare_old_fingerprint`:

```text
stored short hash == new Fingerprint::hash_u64()
and Fingerprint::fs_status == UpToDate
```

If the hash is different or the filesystem is stale, Cargo loads the old JSON
only to explain the dirty reason. On a dirty build, Cargo truncates the old
short-hash file before compilation so a failed/partial output cannot be
mistaken for a valid fresh unit. After successful work, `write_fingerprint`
writes the new short hash and JSON.

`Fingerprint::check_filesystem` first requires every recorded output to exist,
then compares the newest output with dependency outputs. For an rmeta-only edge
it checks the dependency's `.rmeta`; otherwise it checks the newest dependency
output. It then evaluates each `LocalFingerprint`:

* `CheckDepInfo` parses rustc's dep-info translated by
  `translate_dep_info`. For local packages it checks source files against the
  dep-info mtime (or size + checksum with `-Zchecksum-freshness`) and compares
  recorded environment variables. For registry and git packages Cargo omits
  source paths from translated dep-info because those sources are treated as
  immutable.
* `Precalculated` is used for rustdoc and some build-script modes.
* `RerunIfChanged` and `RerunIfEnvChanged` are generated from build-script
  output. Build scripts have a separate `calculate_run_custom_build` path and
  can change their local fingerprint after execution.

The mtime reference is intentionally the invocation-start timestamp, and
Cargo rewinds the translated dep-info mtime to that timestamp after a build.
This catches source edits made during compilation. macOS HFS's coarse timestamp
resolution is called out by Cargo as a freshness hazard; APFS and HFS behavior
must be covered by the macOS test matrix. `-Zchecksum-freshness` is currently
unstable, uses rustc-emitted checksums and a Cargo-selected algorithm, and does
not make build-script inputs closed-world.

The existing fingerprint is therefore useful evidence, but it is not a global
ActionKey:

* it is a 64-bit value (`StableHasher` is a stable SipHash implementation, not
  a cryptographic content digest);
* its path and environment tracking is intentionally best effort and partly
  dynamic;
* it intentionally omits some fields from its hash (for example output paths
  and mtimes) and treats registry source contents as immutable by convention;
* the registry source checksum is not in it;
* it can contain workspace-local paths in `path_args`, dep-info, and outputs;
* its rustc identity is full `-vV` text, while Cargo's artifact metadata hash
  uses a different, intentionally coarser rule described below.

## Cargo metadata hashes and rustc artifact identity

`src/compiler/build_runner/compilation_files.rs::compute_metadata` calculates a
`Metadata` for every unit. It produces three related `UnitHash` values:

* `unit_id`: identifies a unit for the build graph and v2 directory name;
* `c_metadata`: the value passed to rustc as `-C metadata=...`, affecting the
  crate disambiguator and symbol identity;
* `c_extra_filename`: when enabled, `-C extra-filename=-<unit_id>` keeps output
  filenames separate. `pkg_dir` controls whether the v2 directory includes the
  unit hash.

The shared base hasher includes:

* `METADATA_VERSION` (currently `2`);
* stable `PackageId` hash (workspace-relative only for path packages);
* enabled features;
* `Profile`, `CompileMode`, and calculated LTO state;
* host/target `CompileKind` fingerprint hash;
* target name and `TargetKind`;
* `hash_rustc_version` output;
* workspace wrapper path for workspace members;
* `__CARGO_DEFAULT_LIB_METADATA` when set;
* `is_std`; and
* the host-config distinction used when target configuration does not apply
  to host units.

`c_metadata` adds the sorted `c_metadata` values of all dependencies.
`unit_id` adds the sorted dependency `unit_id` values plus extra Cargo args and
the unit's rustflags/rustdocflags, except when a remap-path-prefix flag may be
present. That exception is deliberate: including arbitrary absolute remap
paths in symbol metadata would make otherwise reproducible artifacts vary.

`hash_rustc_version` is not the same as a safe toolchain identity. For stable
releases it hashes full verbose-version lines (excluding the host line for a
target unit); for nightly/beta/dev it hashes the channel and host rather than
the date/commit, to avoid invalidating the local target directory on every
nightly update. A CAS key must use a stricter compiler identity, including the
effective rustc/toolchain and wrapper identity, unless the experiment has
evidence that a narrower identity is safe.

`src/util/rustc.rs::Rustc` records the rustc path, optional rustc wrapper,
workspace wrapper, full `-vV`, semantic version, host, and commit hash. The
`.rustc_info.json` cache is only a performance cache for `-vV`/other queries.
Its `rustc_fingerprint` hashes executable/wrapper metadata and rustup context;
that is not a published artifact identity and should not be reused as the
entire CAS key.

## Compiler command and path inventory

`prepare_rustc` in `src/compiler/mod.rs` builds the static part of the command.
`rustc` adds build-script-derived arguments at execution time. The following
paths and values can affect compiler behavior or output bytes:

| Input | Current insertion point | Why it matters to CAS |
| --- | --- | --- |
| Source filename and rustc cwd | `util::workspace::path_args` / `add_path_args` | Path dependencies normally use a workspace-relative source argument and workspace cwd; registry/git sources use an absolute package source path and package cwd. Debug info, diagnostics, and macros can observe these. |
| Target identity | `CompileKind::add_target_arg`; `CompileTarget` canonicalizes custom JSON paths and hashes JSON contents for fingerprinting | Built-in target triple, host/target distinction, and custom target contents/ABI must be keyed. Never key custom targets by only their short filename. |
| Output directory | `build_base_args` `--out-dir` | This is workspace-local v2 state. It must be rewritten/materialized into each consuming workspace, not stored as an entry-relative absolute path. |
| Dependency search paths | `lib_search_paths` (`-L dependency=...`) | v2 recursively lists dependency-unit `out` directories in sorted order, plus host deps where needed. Paths are local and must not become persistent manifest paths. |
| Dependency artifact paths | `extern_args` (`--extern name=...`) | The exact `.rmeta` or linkable `.rlib` role is selected based on pipeline edge and embed-metadata mode. A hit must materialize the same output roles before dependents run. |
| Profile/codegen | `build_base_args` | Opt level, panic, LTO, backend, codegen units, debuginfo/split-debuginfo, trim paths, assertions, overflow, rpath, strip, crate types, and embed-metadata affect bytes/ABI. |
| Features and checks | `features_args`, `check_cfg_args`, manifest lint flags, `-Zallow-features`, cargo-lints | These alter conditional compilation or compiler diagnostics and must be in the semantic input set where applicable. |
| Linker/toolchain config | `add_codegen_linker`, target data, `Rustc` process selection | Absolute linker/ar paths and wrapper behavior affect output; arbitrary external tool discovery is outside V0. |
| Cargo/rustc flags | `extra_args_for`, unit flags, effective `RUSTFLAGS`/`RUSTDOCFLAGS`, `-Zbinary-dep-depinfo`, `-Zchecksum-freshness` | Flags are not all represented by one existing hash; include all semantically relevant effective arguments or make the unit ineligible. |
| Incremental state | `add_codegen_incremental` (`-C incremental=<build-dir>/.../incremental`) | Mutable and workspace-local; never publish it as an immutable V0 artifact. |
| Build-script outputs | `build_deps_args`, `add_custom_flags`, `add_native_deps` | `OUT_DIR`, cfg/env, `-L`, `-l`, linker args and generated files can observe arbitrary host state. Exclude build scripts and affected dependents in V0. |
| Generated/build environment | `CARGO_*`, package metadata env comments in dep-info, `CARGO_TARGET_TMPDIR`, SBOM/unremap paths | Some values are runtime-only or local bookkeeping; some change compiler input. The classifier must exclude uncertain cases rather than guess. |
| Path remapping | `trim_paths_args`, `trim_paths_remap`, `__CARGO_RUSTC_BOOTSTRAP_WS_REMAP` | Remapping can make source/build/sysroot paths portable, but changes artifact content. Include effective remap semantics in the key; do not force trim-paths before Gate 1 proves the need. |

The command always passes `--emit` variants based on `CompileMode` and
`should_embed_metadata`. Check units emit metadata/dep-info without a linkable
artifact. Full library builds can emit `.rmeta` plus `.rlib`; a dependent that
only needs metadata receives `.rmeta`, while a binary/full edge receives the
linkable output (and `.rmeta` as needed for `-Zembed-metadata=no`).

Rustc-generated dep-info initially uses compiler paths. Cargo translates it in
`rustc`/`rustdoc` after successful execution with
`fingerprint::translate_dep_info`; the translated fingerprint dep-info is a
local Cargo freshness artifact, not automatically a portable CAS file.

Absolute paths can also be present in object/debug metadata. `trim_paths` maps
registry, git, workspace, build-dir, and sysroot prefixes to stable virtual
prefixes and can emit an unremap companion file. This is the primary existing
mechanism to investigate for relocatability, but byte identity still requires
an experiment: path remapping may affect diagnostics/debug info and its own
flags are semantically relevant.

## Incremental compilation

Cargo's profile documentation says incremental compilation is used for
workspace members and path dependencies; registry dependencies are not the
normal incremental scope. When a unit's profile has `incremental = true`,
`add_codegen_incremental` adds:

```text
-C incremental=<build-dir>/[<target>/]<profile>/incremental
```

Rustc creates and manages crate/session subdirectories under that root. Cargo
does not treat incremental files as `OutputFile`s, and they are not represented
by the final artifact uplift. They are mutable optimization state that can be
invalidated, garbage-collected, or partially written independently of the
`.rmeta`/`.rlib` set.

Therefore incremental units are ineligible for the initial global cache. A
CAS hit must never copy incremental directories or make one workspace's
incremental state visible to another. A future design could separately cache
compiler incremental state, but that is a different contract.

## Fine-grained locking lifecycle

`src/compiler/layout.rs::Layout::new` establishes the coarse compatibility
locks:

* `build-dir/<profile>/.cargo-build-lock` is shared for ordinary v2 builds when
  fine-grain locking is enabled, exclusive for operations that require an
  exclusive build-dir lock;
* `target-dir/<profile>/.cargo-lock` remains a shared compatibility lock with
  older Cargo versions;
* `target-dir/<profile>/.cargo-artifact-lock` is exclusive only when the
  command can uplift/write final artifacts. `BuildRunner::prepare_units` skips
  this lock for ordinary `cargo check`.

On NFS, Cargo disables the lock attempt because the Unix `flock` behavior is
known to be unsafe there. On macOS, the normal Unix `File::lock`/`lock_shared`
shim is used; V0 should use a local APFS/HFS filesystem and explicitly test
contention and crash recovery.

`src/compiler/locking.rs::LockManager` keeps one `FileLock` per
`build/<package>/<unit-hash>/.lock` in a process-local map:

1. `compile` calls `lock_shared` before fingerprint inspection. This prevents
   a reader from observing a fingerprint/output set while another Cargo is
   writing it.
2. If the unit is dirty, Cargo releases that shared lock while assembling the
   queued operation, then wraps the job with `prebuild_lock_exclusive` before
   execution. Releasing before acquiring exclusive avoids a cross-process
   shared-to-exclusive upgrade deadlock.
3. The job includes rustc/rustdoc, fingerprint persistence, and target uplift.
   Its final `downgrade_lock_to_shared` allows other Cargo processes to read
   the completed unit while this Cargo process continues using it.
4. The lock handle remains held until the process/build runner drops it.

`JobQueue`/`JobState` carry the lock manager and preserve the separate metadata
and full-artifact completion events. A CAS implementation should keep this
unit scheduling contract: a cache hit is a normal job that produces the
expected `Metadata`/`All` completion event, not a special global scheduler
barrier. The global cache needs an additional per-ActionKey coordination and
publication protocol; the existing unit lock protects only each workspace's
materialized build-unit directory.

`src/compiler/cas.rs::CacheAction::coordinate` intentionally opens its
ActionKey lock only inside active `Work` and drops it as the work closure
returns. The cache action itself owns no descriptor while Cargo constructs the
unit graph. Consequently live CAS lock descriptors are bounded by Cargo's
active job count, rather than the number of units. The reproducible macOS
64-action benchmark in `scripts/benchmark-cas.sh` pauses active dependency
compilers and verifies this directly with `lsof`: 64 ActionKeys at `-j 8`
produce eight live cache lock descriptors.

## Artifact outputs and uplift

`CompilationFiles::calc_outputs_rustc` turns target information into
`OutputFile` records:

```text
OutputFile {
    path: build-unit output path,
    hardlink: optional final target path,
    export_path: optional --artifact-dir path,
    flavor: rmeta/linkable/normal/debug/auxiliary/etc.
}
```

`CompilationFiles::uplift_to` returns no final path for tests, check/doc units,
`.rmeta`, artifact dependencies, or v2 build-script binaries. It may return a
final target path for a root, binary, dylib, or explicitly requested build unit.
Examples have their own final examples directory. `BuildRunner::check_collisions`
checks intermediate path, hardlink path, and export path collisions before
execution.

`link_targets` in `src/compiler/mod.rs` runs for both dirty and fresh jobs. For
each existing output it either leaves the unit-local output in place or calls
`cargo_util::paths::link_or_copy` to place the final hardlink/export. This is
the “uplift” step. It is deliberately downstream of compiler execution and of
freshness analysis.

The global cache should contain only eligible dependency artifact sets, not
final/root artifacts. On a hit, it must materialize each expected unit-local
`OutputFile::path` (including the correct `.rmeta`/`.rlib` roles) before a
dependent's `--extern` arguments are evaluated. Existing `extern_args`,
`JobQueue` metadata/full completion, and `link_targets` should then continue to
operate normally. A CAS hit must also establish the local Cargo fingerprint and
translated dep-info state needed for the next invocation; copying only an rlib
is not sufficient.

## Gate 0 insertion points

These are the narrowest seams found in the current architecture. They are
ordered according to the existing compile flow and deliberately avoid changing
the resolver, unit graph, or job queue contracts.

| Concern | Narrow insertion point | Required behavior/constraint |
| --- | --- | --- |
| Eligibility | `src/compiler/mod.rs::compile`, after the complete `Unit` and `BuildRunner::unit_deps` are known and before the dirty work is selected | Make a conservative closed-world decision. Start with immutable registry package, normal pure-Rust library, no build script/proc-macro/native/generated input, no incremental, no path/workspace source, no final/root target. Unknown means normal Cargo fallback. |
| Semantic key generation | A helper called from the same compile path, using `CompilationFiles::metadata`, effective profile/kind/mode/flags, source checksum, strict rustc/toolchain identity, and recursively computed dependency keys | Use a versioned canonical structure, not `Fingerprint::hash_u64`, `UnitHash`, `UnitIndex`, Rust's generic `Hash` serialization, or workspace-local paths. Include every effective compiler input that can affect bytes/ABI. Dependency keys form a DAG. |
| Lookup | In `compile`, after `fingerprint::prepare_target` identifies a dirty eligible unit and before choosing `rustc(...)` | A local fresh unit still follows the ordinary fresh path. A global hit must be validated as a complete entry before scheduling its materialization work. A miss must select the unchanged rustc work. |
| Publish | Chain a CAS publish `Work` after successful `rustc` work (which has translated dep-info and completed outputs) and before `link_targets`, or place the equivalent finalization at the end of the `rustc` work closure | Stage all artifacts and a validated manifest in a per-writer temporary directory; atomically rename/publish on the same filesystem. Never expose a partial entry as a hit. Coordinate same-key writers without serializing different keys. |
| Cache-hit artifact registration | The hit materialization `Work` should populate `BuildRunner::outputs(unit)` paths, establish local fingerprint/dep-info state, and emit `JobState::rmeta_produced` when the unit is a pipelined metadata producer | Preserve `Artifact::Metadata` vs `Artifact::All`, `--extern` role selection, existing target uplift, diagnostic replay, and fine-grained lock lifecycle. Rewrite or regenerate workspace-local dep-info rather than copying absolute paths. |

`prepare_rustc` is the best command-construction seam for key auditing, but it
must not be treated as the key by blindly hashing its rendered string: it
contains local `--out-dir`, `-L`, `--extern`, incremental, linker, cwd, and
possibly runtime build-script values. Canonicalize semantic values and omit or
normalize only values proven not to affect output. For V0, exclusion is safer
than an incomplete normalizer.

Do not put the first CAS lookup in `JobQueue`: that layer owns dependency
ordering, token management, metadata pipelining, and diagnostic messages. It
should receive a normal `Job` whose work happens to materialize immutable
outputs.

## V0 cacheability boundary and macOS note

The initial cacheability predicate should be narrower than the eventual
ActionKey schema:

```text
eligible iff
    source is an immutable registry package with a verified checksum
    && unit is a normal pure-Rust library build
    && compile mode is Build or the explicitly supported Check form
    && no build.rs / RunCustomBuild affects this package or its inputs
    && no proc-macro unit or proc-macro execution affects this unit
    && no native-linking or generated external input is present
    && profile.incremental is false
    && no path/workspace source, final/root artifact, bin, test, bench, example,
       rustdoc, or build-script output is being requested
    && all effective inputs can be represented without workspace-local paths
```

In practice, a registry dependency with a transitive build script or proc
macro should be excluded until the entire affected subgraph has a closed-world
model. A cache miss is always safe normal Cargo behavior; a false hit is a
correctness bug.

For macOS-only work:

* Begin with host compilation on local APFS (and add HFS/coarse-mtime coverage
  if that filesystem is available). Do not claim cross-target or cross-OS
  reuse.
* Keep the existing v2 host/profile partitioning and unit locks. Validate
  `flock` contention using the macOS file-lock shim and avoid NFS/network
  volumes for the first proof.
* Treat source/build/dependency paths, debug info, dSYM-related output, linker
  paths, and case/normalization behavior as possible byte-identity inputs.
  Record observed behavior in the Gate 1 relocation matrix; do not force
  `trim-paths` merely to make a hit happen.
* Do not include incremental directories or final target artifacts in the V0
  cache entry. The entry should be an immutable set of dependency outputs with
  relative artifact roles and a versioned manifest.

## Findings that Gate 1 must verify

The architecture narrows the unknowns but does not prove relocatability. Gate 1
must compile one simple immutable registry library in two unrelated macOS
workspaces and compare, at minimum, `.rmeta`, `.rlib` (when emitted), object
members, and dep-info. It must also prove that the second workspace invokes no
rustc for that dependency.

The specific hypotheses to test are:

1. Registry source extraction is checksum-stable but `path_args` still passes
   an absolute source path to rustc. Does the resulting `.rmeta`/`.rlib` contain
   that path, or does rustc omit/remap it under the chosen profile?
2. `-L`/`--extern` paths point into each workspace's build-dir. Are they only
   compiler lookup inputs, or do they appear in artifact metadata/debug info?
3. `-C metadata` and dependency metadata are byte-identical when the registry
   package checksum, dependency keys, features, profile, mode, target, and
   strict compiler identity are identical.
4. `dep-info` is local bookkeeping that can be regenerated/translated on
   materialization, or whether it contains information needed for a safe hit.
5. macOS filesystem timestamp precision causes any current Cargo freshness
   behavior that a CAS hit must preserve.

The V0 action implementation therefore records both Cargo's full `rustc -vV`
output and the canonical compiler executable path plus sysroot path. The latter
two are intentionally distinct inputs: a toolchain shim can report an
unchanged version banner while changing code generation, and a sysroot can
provide compilation inputs not represented in that banner. The corresponding
regression test uses two such shims and requires a cache miss.

`CacheKeyInputV0` also records an explicit compiler-contract section for the
effective pieces of `prepare_rustc` that are not covered by `Profile`, unit
`rustflags`, or the dependency ActionKey DAG: manifest lint flags, generated
`--check-cfg` arguments, the effective cap-lints mode, `-Zallow-features`,
`-Zcargo-lints`, binary-dependency dep-info and checksum-freshness switches,
metadata embedding, and the selected linker path. This avoids treating a
matching artifact filename as proof that the preceding compiler action was
the same. The regression matrix changes opt level, debug/debug-assertion and
overflow settings, panic, LTO, codegen units, split debuginfo, effective Cargo
`build.rustflags`, encoded Rustflags, and `-Zcargo-lints` independently.

## Gate 1 observed relocation contract

`tests/testsuite/gate1_relocatability.rs` compiles the same local-registry
library independently in two unrelated macOS workspaces. For matching source,
features, profile, target, and toolchain, its `.rmeta`, `.rlib`, and every
extracted `.rlib` archive member are byte-identical. This establishes that the
compiler artifact set is suitable for the controlled V0 reuse experiment.

The same test also establishes the boundary around Cargo's local state:
fingerprint files differ and contain each workspace's absolute target
directory. They are destination-local and are regenerated by the normal dirty
job after a hit. The translated dep-info is a different, intentionally narrow
case: Cargo's encoded format classifies each path as package-root or
build-root-relative, so its bytes can be copied only after V0 verifies that it
contains no absolute path or tracked environment. It then rebases naturally
when Cargo parses it against the destination build root. It is transport for
local freshness, never an ActionKey input or arbitrary workspace bookkeeping.

## Cache storage V1 and manifest format V2

The current experiment stores entries under
`$CARGO_HOME/cache/cargo-cas-v1`. A manifest records its format version and
ActionKey, plus a separately validated identity containing the package ID,
target/crate, compile mode, full toolchain identity (compiler path, `-vV`, and
sysroot), and direct dependency ActionKeys. Artifact paths remain relative to
the entry; each artifact has a role, filename, size, and BLAKE3 digest. The
compiler output cache is an optional artifact role: Cargo creates it only
when rustc emitted replayable diagnostics, but a hit that contains it restores
and replays those messages through Cargo's ordinary fresh-unit path.

The duplicate identity is intentionally not an alternate lookup key. It lets a
reader reject malformed, stale, or locally modified metadata before it copies
an artifact into Cargo's ordinary build directory. Changing the required
manifest schema bumps the manifest format (or, if the on-disk layout itself
changes, creates a new cache-format directory) rather than interpreting an
older entry permissively.

The current manifest format is `2`. It invalidates format `1` entries because
the older artifact set could not replay compiler diagnostics on a cache hit.
The cache root remains `cargo-cas-v1` because the manifest version already
makes the change a complete, safe miss-and-rebuild boundary.

If artifacts are not relocatable, record the exact rustc/Cargo path input that
requires remapping or exclusion. Do not paper over it by copying arbitrary
workspace metadata into the global entry.

## Local path demonstration

`make demo` runs the focused `epsh` → `ish` experiment used while developing
local-path and build-script support. It builds each checkout first with regular
Cargo and then with the debug `cargo-cas` binary, using isolated target
directories and private Cargo homes. The `ish` invocation is temporarily
patched to depend on `~/d/epsh`; `resolver.lockfile-path` points Cargo at a
temporary lockfile so neither checkout is modified.

The demo reports per-package timings, target usage, cache entries available from
the first build, and entries published by the second build. All temporary state
is removed on success, failure, or interruption. Set `KEEP=1 make demo` to keep
the logs and targets for inspection. Set `TRACE=1 KEEP=1 make demo` to include
the structured CAS hit/skip summaries in the output; traced timings include
debug-logging overhead.

## Cache observability

`CARGO_LOG=cargo::compiler::cas=debug` exposes per-action `hit`, `miss`,
`reject`, and `skip` decisions without changing Cargo's ordinary output. At
the successful end of a cache-enabled invocation it also emits a structured
`cargo-cas summary`: eligible units, hits, initial lookup misses, rejects,
eligible `rustc` work, same-key duplicate-build avoidance, and a
reason-counted skip map. A same-key waiter's lock-held recheck is deliberately
not a second miss. The summary is process-local observability, not mutable
entry metadata; cache entries remain immutable after publication.

## Pipelined cache hits

`artifact_paths` orders an eligible cache entry as `.rmeta`, linkable artifact,
then translated dep-info and an optional diagnostic output cache.
`CacheAction::restore_or_compile` copies that
metadata role first and calls `JobState::rmeta_produced` immediately after it
is materialized. A metadata-only dependent can therefore use the normal Cargo
pipeline edge without waiting for linkable-artifact or local-bookkeeping
transport. The Gate 3 manifest regression asserts this directly: it pauses a
cache hit immediately after `.rmeta` transport and observes the dependent
root's rustc proxy start before linkable/dep-info transport is released. The
same test also keeps the normal build/artifact-dir behavior covered. A
separate Gate 3 regression publishes an eligible dependency warning, confirms
that the warm workspace does not invoke that dependency's rustc, and confirms
that Cargo replays the warning from the restored output cache.

## Cache-infrastructure failure boundary

The cache root and its mutable `locks`, `tmp`, and `access` children are each
checked with `symlink_metadata` before use. A missing directory is created as
an ordinary directory; a regular file, permission error, or substituted
symlink makes that action a normal local compile and logs the cache failure at
debug/warn level. Artifact restore copies through a no-follow descriptor and
rechecks the manifest size/digest while copying. Publication is still best
effort, so an I/O failure such as disk exhaustion after successful rustc work
cannot turn a valid Cargo build into a cache-only failure. `cargo clean gc`
refuses a malformed root instead of traversing it and, while holding Cargo's
package-cache mutation lock, removes incomplete staging directories and stale
per-key lock files before enforcing its entry age/size policy. The integration
regressions substitute the root and internal children with symlinks to an
outside sentinel, prove no locks/staging/artifacts escape into that directory,
then prove normal publish/hit recovery.

## Exact archaeology commands and checks

The following read-only commands were used for this document:

```text
git fetch --no-tags https://github.com/rust-lang/cargo.git master
git rev-parse FETCH_HEAD
git diff FETCH_HEAD..HEAD -- ':!plan.md' ':!CARGO_CAS.md'
git log --oneline --decorate -8
git log --oneline --all --grep='#(14125|15010|16807|17168|17191|17236|17258|17354|4282|16089|16155|16307|16147|13663|12633|13136|5931)' -i --reverse
git show -s --format='%h %ad %s' --date=short <lineage-revision>
rg -n 'build_dir_new_layout|fine_grain_locking|prepare_target|calculate_normal|compute_metadata|prepare_rustc|extern_args|add_codegen_incremental|uplift_to|lock_shared|lock_exclusive|downgrade_to_shared' src doc/book
sed -n '<relevant range>p' <source-file>
git diff --check -- CARGO_CAS.md
```

No Cargo/compiler test suite was run for Gate 0 because this milestone changes
only architecture documentation. Before handing off, `git diff --check --
CARGO_CAS.md` must pass and `git status --short` must show only this document
as the subtask's edit. The executable relocation and invalidation evidence
belongs to Gates 1–3.
