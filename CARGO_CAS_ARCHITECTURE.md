# cargo-cas architecture

This is the durable description of the `cargo-cas` experiment: what it
considers cacheable, how it integrates with Cargo's compiler scheduler, what
is stored, and what remains deliberately outside the contract. The
implementation and its macOS acceptance tests are authoritative when this
document and an older experiment note disagree.

## Status and scope

`cargo-cas` is a Cargo fork implementing the direction tracked by Cargo issue
[#5931](https://github.com/rust-lang/cargo/issues/5931): reuse an immutable
dependency compilation across unrelated workspaces and concurrent worktrees.
It changes Cargo itself; it is not a wrapper, an `sccache` replacement, a
daemon, or a remote cache client.

The experiment is currently validated on macOS local filesystems. The last
recorded upstream Cargo base is `514c56dd7321eecbfdcf9b6479519cf4edfab906`,
which includes build-directory layout v2 and fine-grained locking. The source
snapshot consolidated here is `a4bfc9c2a25436ae9920ac18e213ab75ac4b8f62`
before documentation-only changes. Future implementation changes must update
the tests first, then revise this document.

The immutable artifact cache is always enabled in this fork; no option or
feature activation is required. Normal Cargo freshness and compilation are
unchanged for any unit the cache does not cover.

The central safety rule is:

> A miss is a performance cost. A false hit is a correctness bug.

When an input cannot be represented completely, the unit falls back to the
ordinary Cargo compiler path.

## Cargo prerequisites and local layout

The cache builds on Cargo changes that already made a compilation unit a
useful boundary: separate intermediate `build-dir` and final `target-dir`,
build-directory layout v2, and fine-grained unit locking. The relevant design
lineage is Cargo issues/PRs [#14125](https://github.com/rust-lang/cargo/issues/14125),
[#15010](https://github.com/rust-lang/cargo/issues/15010),
[#16155](https://github.com/rust-lang/cargo/pull/16155), and
[#16307](https://github.com/rust-lang/cargo/pull/16307); the global-cache goal
is tracked by [#5931](https://github.com/rust-lang/cargo/issues/5931).

With layout v2, a normal host profile is organized approximately as:

```text
<build-dir>/<profile>/
    .cargo-build-lock
    incremental/                 # mutable rustc state
    build/<package>/<unit-hash>/
        .lock
        out/                      # compiler outputs
        fingerprint/             # Cargo freshness/bookkeeping
        run/                      # build-script execution state
        artifact/<kind>/          # artifact dependencies when applicable
<target-dir>/<profile>/
    .cargo-artifact-lock
    <final/root artifacts>
```

`Unit`/`UnitGraph` describe the package target, profile, compile kind, compile
mode, enabled features, flags, build-script state, and dependency edges. They
are process-local scheduler identities, not persistent cache keys. `UnitIndex`
is only a per-invocation graph index and is never part of an `ActionKey`.

Cargo's local `Fingerprint` combines rustc version, target/profile/mode,
features, effective flags, config, source/dependency freshness, and filesystem
checks. It deliberately excludes output paths and remains a best-effort local
freshness mechanism. On a dirty job Cargo clears the old short hash, runs the
unit work, writes the new fingerprint, and then performs target uplift. CAS
preserves that lifecycle; it substitutes only the compiler work in the middle.

## Goal and ownership boundary

For a semantically identical dependency action, two workspaces should be able
to use one immutable compiler artifact set:

```text
Cargo Unit graph
      |
      v
closed semantic ActionKey
      |
  hit / miss
   |     |
restore  rustc -> validate -> atomically publish
   \     /
    normal Cargo job, uplift, and dependency scheduling
```

The global cache owns immutable dependency outputs and narrowly replayable
build-script results. Each consuming workspace continues to own:

* its `target-dir`/`build-dir` layout, Cargo fingerprints, translated dep-info,
  message cache, and final/root artifacts;
* rustc incremental state;
* dependency ordering, metadata pipelining, final artifact uplift, and normal
  Cargo diagnostics.

The cache never replaces Cargo's `Unit`, `UnitGraph`, `Fingerprint`,
`JobQueue`, or artifact-uplift contracts. A hit is a normal dirty Cargo job
whose work happens to materialize already-compiled outputs.

### Validated no-op receipt

The installed `cargo-cas` build also has a deliberately narrow startup fast
path for an unchanged, argument-only `build` or `check` in a non-workspace
package. After a successful ordinary invocation, it records a versioned receipt
under `target/.cargo-cas/noop-v1.json`. The receipt binds the canonical project
and target paths, the exact command shape, the environment context, every
project/configuration input, and every target-directory file identity. Receipts
are not published when Cargo has a cached compiler or build-script diagnostic,
so a hit does not hide warnings that Cargo would otherwise replay. On the next
invocation the receipt is validated while holding Cargo's `target/debug/.cargo-lock`;
missing, malformed, changed, or ambiguous state falls through to the ordinary
Cargo path. The receipt is an optimization only: it does not replace Cargo's
fingerprints, resolution, or diagnostics for unsupported command shapes, and it
is enabled in the locally installed fork. Set
`CARGO_CAS_DISABLE_FAST_NOOP=1` to force the ordinary path while diagnosing a
project. `scripts/benchmark-noop.sh` compares that forced ordinary path with
receipt hits on a selected package. Platforms without the file-lock primitive
used by Cargo take the ordinary path.

## Cargo integration

The relevant implementation is `src/compiler/cas.rs`, called from
`src/compiler/mod.rs`.

For each unit, Cargo first performs its normal fingerprint preparation. Only a
dirty unit is a CAS candidate; a local fresh unit follows the ordinary fresh
path. For a candidate, Cargo:

1. computes a complete eligibility decision and recursive `ActionKey`;
2. checks the immutable cache manifest and validates every required artifact;
3. on a hit, restores the artifact roles into the unit's ordinary v2 output
   directory and replays the normal Cargo message cache when present;
4. on a miss, coordinates only that key, then runs the unchanged rustc job;
5. after successful rustc work has written outputs and translated dep-info,
   stages and publishes an immutable entry best-effort; and
6. runs the existing `link_targets`/uplift work and emits the normal job
   completion events.

`JobQueue` still owns dependency ordering and the distinction between
`Artifact::Metadata` and `Artifact::All`. When a hit restores `.rmeta`, it
calls `JobState::rmeta_produced` immediately, so metadata consumers can start
before the linkable artifact and local bookkeeping finish. A late restore
failure falls back to the already-prepared rustc work.

With `-Zfine-grain-locking`, Cargo's existing per-unit lock protects each
workspace's materialized `build/<package>/<unit-hash>` directory. The CAS adds
one lock per `ActionKey`, acquired only inside active work. Same-key writers
therefore compile once; unrelated keys do not serialize, and the number of
live CAS lock descriptors is bounded by active Cargo jobs rather than graph
size.

## Eligibility contract

Eligibility is intentionally narrower than the full Cargo unit graph. The
current predicate in `ineligibility_reason` and
`ineligibility_reason_in_subgraph` requires all of the following for a compiler
artifact action:

* macOS and the CAS feature enabled;
* a registry package with a verified checksum, a Git package with a full
  resolved revision, or a local package with a complete source snapshot;
* a host unit compiling a linkable `lib`/`rlib` target;
* `check` (without a test harness) or `build` mode;
* no incremental compilation, `trim-paths` profile, rustc wrapper, standard
  library unit, artifact dependency, example, binary, test, bench, rustdoc,
  final/root artifact, or unsupported crate type; and
* no proc-macro execution, native-linking package, or dependency action whose
  inputs cannot be represented recursively.

Path sources are not universally excluded. An ordinary path package is keyed
by its canonical checkout root and a BLAKE3 snapshot of every regular file
under the package root (excluding VCS metadata and Cargo's `target` output).
Symlinks and special files are rejected. A package in a linked Git worktree
uses the common repository identity, full commit, package-relative root, and
the same snapshot, allowing sibling worktrees to share without conflating
separate clones.

Build scripts have a separate, stricter replay contract. A host build script
may be cached only when it has no `links`, no wrapper, no build dependencies,
and its observed output is representable. The cache records declared
environment values, parsed textual output, and regular files below `OUT_DIR`
(currently bounded to 64 MiB). Native library/linker arguments, metadata,
unsafe paths, external `rerun-if-changed` paths, error output, and other
undeclared host effects make the script ineligible. Units affected by an
arbitrary or non-replayable script remain ordinary Cargo work.

The build-script action identity includes the package/source, host, target,
profile, features, rustflags, strict toolchain identity, and inherited UTF-8
environment values. Cargo-local and per-process variables such as `CARGO_HOME`,
`OUT_DIR`, target locations, and the jobserver are omitted because replay
rewrites them for the destination workspace. Any declared `rerun-if-env-changed`
value is recorded in the manifest and must still match at lookup.

Proc macros remain excluded both as compiled units and for every unit whose
compilation executes them. Native discovery and arbitrary build-script effects
are excluded for the same closed-world reason. The implementation does not
guess when a dependency subgraph is unsafe; it records a debug skip reason and
uses normal Cargo semantics.

## Action identity

`ActionKey` is the lowercase hexadecimal BLAKE3 digest of a versioned,
canonical JSON structure (`CacheKeyInputV0` in `src/compiler/cas.rs`). It is
not Cargo's 64-bit local fingerprint hash, a `UnitHash`, a `UnitIndex`, an
artifact filename, or an artifact digest.

The key records:

* package name/version and explicit source identity: registry URL plus
  checksum; canonical Git URL, full revision, and reference; or the path/Git
  worktree identity and source snapshot described above;
* target name, crate name, crate types, host compile kind, and compile mode;
* the complete effective `Profile`, calculated LTO mode, features, unit
  rustflags, and extra compiler arguments;
* a compiler-contract section containing manifest lint flags, generated
  `--check-cfg`, cap-lints, allowed unstable features, Cargo lints,
  dep-info/checksum-freshness switches, metadata embedding, Cargo's
  `-C metadata` and `-C extra-filename` values, and linker path;
* strict toolchain identity: canonical rustc executable path, full `rustc
  -vV` text, and sysroot; and
* recursively sorted dependency action keys plus dependency edge semantics
  (`--extern` name, public/noprelude/nounused bits).

The dependency keys form a DAG. Workspace paths, output directories,
incremental directories, search paths, and `--extern` materialization paths
are not persistent key inputs. If one of those values can affect output and
cannot be normalized safely, eligibility rejects the action.

Three identities must remain distinct:

| Identity | Lifetime | Purpose |
| --- | --- | --- |
| Cargo `Fingerprint` | local target/build directory | freshness, filesystem checks, dirty diagnostics |
| `ActionKey` | global immutable cache | proves a semantic compiler action is reusable |
| artifact BLAKE3 digest | one manifest member | detects corruption or mutation during transport |

Cargo's fingerprint deliberately omits output paths and uses best-effort
filesystem/environment tracking, so it is evidence for local freshness, not a
portable global key. Likewise, rustc metadata hashes distinguish local unit
outputs but do not encode the complete toolchain and source contract needed by
CAS.

## Relocatability and materialization

The pre-CAS Gate 1 test in
`tests/testsuite/gate1_relocatability.rs` establishes the important boundary:
the same immutable registry library built in two unrelated workspaces emits
byte-identical `.rmeta`, `.rlib`, and extracted archive members, while Cargo's
fingerprints and translated dep-info remain destination-local and contain each
workspace's target paths. It also proves that manually materializing only the
dependency subtree into a different target directory lets `check` and `build`
reuse the dependency without invoking its rustc; an empty target directory and
a profile change miss.

CAS therefore transports compiler artifacts, not arbitrary Cargo state. A
compiler entry contains the required `.rmeta` and, for build mode, linkable
`.rlib` role, plus validated translated dep-info and an optional Cargo output
cache. The `.rmeta` and `.rlib` cache files are made read-only after validation.
When the target and cache share a filesystem, restore atomically replaces the
destination compiler outputs with verified hardlinks to those immutable files.
Cargo-local dep-info and diagnostic state remain independent destination files.
Local fingerprints and other destination bookkeeping are established by
Cargo's normal job. Incremental directories and final/root outputs are never
copied from the global entry.

A hardlinked target remains usable if cache GC or a user deletes the cache
entry: the target retains its inode. Before ordinary rustc work starts, Cargo
removes any read-only compiler output in the target, so a source edit or other
dirty rebuild cannot mutate the cache inode. Filesystems where hardlinking is
not available fall back to the verified clone/copy materialization path.

After a complete linkable `.rlib` is validated and materialized, Cargo removes
only that library unit's adjacent `*.rcgu.o` intermediates from its target
directory (matched by rustc's output-name prefix). The `.rlib` archive already
contains those codegen members; keeping both copies consumes space without
contributing to Cargo's fingerprints, dep-info, metadata, or final outputs.
Metadata-only `check` entries do not perform this cleanup.

The relocatability evidence is macOS-only and does not claim cross-OS,
cross-target, network-filesystem, or arbitrary debug/path metadata reuse.

When Cargo is launched directly instead of through rustup's Cargo proxy, the
compiler path is resolved once with `rustup which rustc` from Cargo's
invocation directory. This preserves the project `rust-toolchain.toml`
override even though individual rustc child processes run with package-local
working directories. The resolved absolute path is part of the action identity;
an explicit `RUSTC` or `build.rustc` setting remains authoritative.

## Storage and manifest format

Entries live below:

```text
$CARGO_HOME/cache/cargo-cas-v1/
    <action-key>/
        manifest.json
        artifacts/<ordinal>
        [build-script.json]
    locks/<action-key>.lock
    tmp/<staging-entry>/
    access/<action-key>              # mutable last-use record
```

The current on-disk `CACHE_FORMAT_VERSION` is **6**. The directory name is
kept at `cargo-cas-v1`; the manifest version is the compatibility boundary.
Compiler manifests contain the action key, a duplicate human-readable
identity, and artifact records. Each record has a role (`rmeta`, `linkable`,
`dep-info`, or optional diagnostic output), relative file name, destination
output name, byte length, and BLAKE3 digest. Build-script manifests separately
record the replay identity, declared environment, parsed output, and validated
generated files.

Publication is best-effort and follows this protocol:

1. verify eligibility and required source/output state;
2. copy artifacts into a per-writer directory below `tmp`, then make shared
   compiler-output roles read-only;
3. write the manifest atomically;
4. validate the staged entry, including sizes, digests, roles, and identity; and
5. rename the complete directory into the final key path on the same
   filesystem.

Readers only accept a complete matching format and manifest. Missing,
malformed, unexpected, modified, or disappeared entries are rejects/misses and
fall back to rustc. A successful Cargo build is never made dependent on cache
publication succeeding.

## Concurrency, recovery, and safety

The per-key lock is a regular file under `locks/` and is held only while an
active miss may publish or re-check a key. A waiter revalidates after the lock
is released; if the first writer published successfully, it restores instead
of compiling a duplicate. Different keys use different locks.

Cache infrastructure is treated as untrusted mutable state. Root, `locks`,
`tmp`, `access`, entries, and artifacts are checked with `symlink_metadata`;
lock opens use no-follow flags where available. A shared compiler artifact is
hardlinked first to a temporary target sibling, then rechecked for regular-file
type, digest, and size before the temporary file atomically replaces Cargo's
output path. macOS copy-on-write cloning with a streaming-copy fallback is
used for target-local roles and when hardlinking is unavailable. Symlink
substitution, permission errors, disk-full publication, interrupted writers,
corrupt manifests, and late entry removal all degrade to ordinary Cargo
compilation without escaping `CARGO_HOME`.

`cargo clean` intentionally does not remove immutable global entries. Explicit
`cargo clean gc -Zgc --max-cas-age=...` and `--max-cas-size=...` perform CAS
eviction, clean incomplete staging and inactive lock state, and leave a miss
that can be rebuilt. CAS entries are not part of automatic source-cache GC.

## Observability and verification

`CARGO_LOG=cargo::compiler::cas=debug` reports per-action `hit`, `miss`,
`reject`, and conservative `skip` decisions. Each cache-enabled invocation
also emits one process-local `cargo-cas summary` with eligible units, hits,
misses, rejects, eligible rustc work, same-key duplicate-build avoidance, and
skip counts. These counters are diagnostic only; immutable entries do not gain
mutable statistics.

The macOS acceptance suite `tests/testsuite/cargo_cas.rs` covers the contract,
including:

* source, feature, profile, target, rustflags, config, compiler-path, toolchain,
  and transitive dependency invalidation;
* registry, resolved-Git, and linked-Git-worktree reuse;
* build-script replay boundaries and proc-macro/native exclusions;
* diagnostic replay, separate build directories, metadata pipelining, and
  normal artifact uplift;
* atomic publication, killed writers, partial/corrupt entries, missing or
  substituted cache roots and internal directories;
* same-key single-compilation coordination, different-key parallelism, and
  eight-worktree reuse; and
* explicit age/size GC.

The synthetic benchmark is reproducible with
`scripts/benchmark-cas.sh`; the four-worktree history is append-only in
`benchmarks/cargo-cas-workspace-history.jsonl` and generated by
`scripts/benchmark-ish-cas.py`. Those measurements are provenance-bound to
their recorded commit, toolchain, filesystem, and fixture. They demonstrate
the intended avoided duplicate rustc work, but are not architecture
guarantees.

For continuity, the superseded benchmark report recorded one macOS synthetic
run at cargo-cas commit `531e80be28e926d889db6535eefc8ed087b4e091`: an
unrelated warm workspace avoided one duplicate dependency rustc invocation,
and concurrent cohorts of 2/4/8 worktrees compiled that shared dependency
once instead of 2/4/8 times. The same run measured a 3.3% net footprint
reduction after counting the immutable cache. The binary predates the current
format and eligibility rules, so these figures are historical evidence only;
rerun the scripts for current numbers.

## Upstream relationship and current boundary

The design retains Cargo's build-dir v2 separation, unit-oriented layout,
fine-grained locking, metadata pipelining, and artifact uplift boundaries.
Potentially upstreamable pieces are the explicit action-input audit,
immutable-entry validation, normal-job materialization, and conservative
fallback behavior.

Anything outside the eligibility and storage contracts above is a normal Cargo
compile. Possible expansions and other unimplemented work are tracked in
[`CARGO-CAS-TODO.md`](CARGO-CAS-TODO.md), not in this architecture document.
Any expansion must add a closed-world identity model, relocation evidence,
corruption/crash tests, and explicit invalidation coverage before changing
eligibility.

## Change history of the durable format

The cache directory remains `cargo-cas-v1`, while incompatible manifest
changes advance the format number. Earlier notes refer to formats 2 and 3;
those entries are intentionally rejected by the current format-5 reader. The
format-5 boundary also invalidates entries produced before direct Cargo
invocations resolved rustup's project override once at startup.
The current format includes the strict compiler identity, local source/Git
worktree identity, replayable build-script state, and the hardened validation
and recovery rules described above.

When changing this architecture, update in the same change:

* `src/compiler/cas.rs` and its format constant;
* the focused tests in `tests/testsuite/cargo_cas.rs` and
  `tests/testsuite/gate1_relocatability.rs`;
* the unstable Cargo reference in `doc/book/src/reference/unstable.md`; and
* this document's status, eligibility, manifest, and evidence sections.
