# `cargo-cas` V1 storage / V2 manifest results

This report records reproducible macOS measurements for the experimental,
opt-in `-Zcargo-cas` implementation. It is evidence for the conservative V1
subset, not a claim that every Cargo unit is cacheable.

## Provenance

| Field | Value |
| --- | --- |
| Upstream Cargo source | `514c56dd7321eecbfdcf9b6479519cf4edfab906` |
| `cargo-cas` source used for the release binary | `531e80be28e926d889db6535eefc8ed087b4e091` |
| Cache format | `cargo-cas-v1` / manifest format `2` |
| Rustc | `1.97.1 (8bab26f4f 2026-07-14)`, LLVM `22.1.6` |
| OS | macOS `26.5.2` (`25F84`) |
| CPU | Apple M1 Max, 10 logical CPUs |
| Filesystem scope | local macOS temporary volume; no network filesystem claim |

Both comparison binaries were release builds. The upstream binary was built
from the exact source SHA above in a detached worktree; the `cargo-cas` binary
was built with `cargo build --release -p cargo`. The synthetic fixture uses a
local immutable Git dependency, so the numbers do not include registry network
latency or source-download reuse.

## Synthetic benchmark

The following is one run of [`scripts/benchmark-cas.sh`](scripts/benchmark-cas.sh).
It counts every rustc process with a proxy, separately counts the shared
dependency's processes, and measures elapsed wall time at one-second
resolution. The fixture is deliberately small, so process counts—not the
rounded wall time—are the meaningful result.

| Scenario | Shared dependency rustc | Total rustc | Wall time |
| --- | ---: | ---: | ---: |
| Upstream cold | 1 | 3 | 0 s |
| Upstream same workspace warm | 0 | 0 | 0 s |
| Upstream unrelated workspace warm | 1 | 3 | 0 s |
| Upstream concurrent 2 | 2 | 6 | 1 s |
| Upstream concurrent 4 | 4 | 12 | 1 s |
| Upstream concurrent 8 | 8 | 24 | 2 s |
| cargo-cas cold | 1 | 3 | 1 s |
| cargo-cas same workspace warm | 0 | 0 | 0 s |
| cargo-cas unrelated workspace warm | 0 | 2 | 0 s |
| cargo-cas concurrent 2 | 1 | 5 | 1 s |
| cargo-cas concurrent 4 | 1 | 9 | 1 s |
| cargo-cas concurrent 8 | 1 | 17 | 1 s |

The decisive comparisons are the unrelated workspace (one avoided shared
compile) and the 2/4/8 worktree cohorts (1 shared compile instead of 2/4/8).
The remaining rustc processes are independent root crates, which V1 correctly
keeps local.

| Derived comparison | Result |
| --- | --- |
| Cold overhead | 1 s in this rounded run (upstream rounded to 0 s); both invoked the same 3 `rustc` processes. |
| Unrelated workspace | Avoided 1 of 1 duplicate shared-dependency compilations and reduced total `rustc` from 3 to 2. |
| Concurrent 2 / 4 / 8 | Avoided 1 / 3 / 7 duplicate shared compilations, or 50% / 75% / 87.5% of that shared work. |
| Wall time | The small fixture is timer-noise limited; process counts, not rounded seconds, are the meaningful speed metric. |

For equivalent sets of two sequential workspaces plus 2, 4, and 8 worktrees:

| Storage | Bytes |
| --- | ---: |
| Upstream workspace target directories | 1,507,328 |
| cargo-cas workspace target directories | 1,409,024 |
| cargo-cas immutable cache | 49,152 |
| cargo-cas local targets plus cache | 1,458,176 |

This small fixture saves 49,152 bytes (3.3%) after counting the cache itself.
It is a sanity check on the intended ownership boundary, not a claim of large
real-world disk savings. CPU time was not recorded because the synthetic run is
below the timer's useful resolution.

### Lock-scaling check

The benchmark also creates **256** independent immutable Git packages and
pauses their first `rustc` invocations after their CAS locks have been
acquired. With `-j 8`, it observed 256 dependency `rustc` processes (258
total), 23 s wall time, and at most **8** open CAS lock descriptors (the script
fails if the bound is exceeded). The graph therefore has 256 ActionKeys but the
live lock count is bounded by Cargo's job concurrency, not graph size.

It also records the final root compiler invocation: 1,051 argv entries / 93,148
bytes, 256 `-L` entries, 256 `--extern` entries, 24 `PATH` entries, 0 dynamic
library path entries, and 1,550 target files traversed. This makes the direct
dependency search-path cost explicit instead of hiding it behind the cache. The
stress graph added 3,145,728 cache bytes and 6,365,184 local-target bytes;
these are deliberately excluded from the equivalent-workspace storage table
above.

### Reproduce

```sh
cargo build --release -p cargo

git worktree add --detach /tmp/cargo-upstream 514c56dd7321eecbfdcf9b6479519cf4edfab906
(cd /tmp/cargo-upstream && cargo build --release -p cargo)

CARGO_UPSTREAM_BIN=/tmp/cargo-upstream/target/release/cargo \
  CARGO_CAS_BIN="$PWD/target/release/cargo" \
  CARGO_CAS_SCALE_ACTIONS=256 \
  ./scripts/benchmark-cas.sh
```

Set `CARGO_CAS_BENCHMARK_KEEP=1` to retain the isolated inputs, target
directories, logs, and counters for inspection. The default includes the
64-action check; set `CARGO_CAS_SCALE_ACTIONS` and `CARGO_CAS_SCALE_JOBS` to
adjust it. This macOS-only check uses `lsof` to count the Cargo process's live
`cargo-cas-v1/locks` descriptors and records the root argv/search-path
dimensions above.

## Real-world cacheability experiment

Three pinned public repositories were checked sequentially with one fresh
`CARGO_HOME`, a fresh target directory per project, and
`CARGO_LOG=cargo::compiler::cas=debug`. Counts are parsed from the structured
cache-decision logs; `rustc` is the observed number of compiler invocations.

| Repository | Revision | CAS-classified units | Eligible | Hits | Misses | Skips | rustc | Wall time |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| ripgrep 14.1.1 | `4649aa9700619f94cf9c66876e9549d83420e16c` | 39 | 16 | 0 | 16 | 23 | 26 | 3.55 s |
| fd 10.2.0 | `b19136871310b01500b4f09eadd7387b8476be47` | 69 | 36 | 7 | 29 | 33 | 59 | 3.95 s |
| bat 0.25.0 | `25f4f96ea3afb6fe44552f3b38ed8b1540ffa1b3` | 167 | 95 | 9 | 86 | 72 | 134 | 12.44 s |

Each row was rerun from the release binary recorded above. “CAS-classified” is
the structured summary's `eligible + skips`: the units considered by the V0
classifier. The shared cache occupied 97,529,856 bytes across 131 entries
after the three runs. The observed skip reasons were 55 packages with build
scripts, 51 build-script-affected actions, 15 path/workspace units, 6 proc
macros, and 1 proc-macro-affected action. This is the intended conservative
result: cacheability is not expanded merely to improve the hit rate.

The fd and bat rows demonstrate cross-repository warm reuse (7 and 9 hits)
while local and excluded units still compile normally. Every project completed
successfully.

### Reproduce

```sh
git clone --depth 1 --branch 14.1.1 https://github.com/BurntSushi/ripgrep.git
git clone --depth 1 --branch v10.2.0 https://github.com/sharkdp/fd.git
git clone --depth 1 --branch v0.25.0 https://github.com/sharkdp/bat.git

for repo in ripgrep fd bat; do
  (
    cd "$repo"
    CARGO_HOME=/tmp/cargo-cas-real-home \
      CARGO_LOG=cargo::compiler::cas=debug \
      /path/to/cargo-cas check -Zcargo-cas -vv --target-dir "/tmp/$repo-target"
  ) >"/tmp/$repo-cargo-cas.log" 2>&1
done
```

Use a clean target directory for every repository. Preserve the shared
`CARGO_HOME` across the sequence; otherwise no cross-repository cache reuse is
possible. Pinned revisions above keep the experiment repeatable even when the
repositories' default branches advance.

## Completion report

### Thesis and cacheability

Yes: an eligible immutable registry or resolved-Git dependency can be restored
into an unrelated macOS workspace without invoking that dependency's `rustc`.
V0 accepts only native-host, non-incremental `lib`/`rlib` check or build units
whose source has an immutable checksum/revision or a complete local snapshot,
with no build script, proc-macro influence, native linking, wrapper, final
artifact, or unrepresentable local input. Local packages in linked Git
worktrees use a repository/revision/relative-root identity so their build-mode
library artifacts can be shared across sibling worktrees. Everything else is a
normal Cargo compile with an explicit debug skip reason.

### Correctness, artifacts, and recovery

The ActionKey serializes source identity, target/mode/profile/LTO, full
toolchain identity, effective flags/compiler contract, features, and recursive
dependency ActionKeys. Gate 1 proves matching `.rmeta`, `.rlib`, and archive
members across unrelated workspaces; V0 copies only those immutable compiler
artifacts plus validated relative translated dep-info and optional diagnostic
output cache. Fingerprints remain destination-local.

The permanent invalidation matrix covers source/checksum, features,
profile/opt-level/debug/debug assertions/overflow/panic/LTO/codegen/split
debuginfo, flags/config/toolchain, target, and dependency identity. Corrupt,
missing, old-format, interrupted, symlink-substituted, or late-disappearing
entries reject or fall back to ordinary rustc; publication is atomic and
best-effort. Cache hits replay Cargo diagnostics and preserve metadata
pipelining.

### Concurrency and upstream direction

Same-key writers coordinate through a per-ActionKey lock; different keys do
not serialize; the eight-worktree test proves one missing action compiles once,
every reader observes a hit, and all manifests/digests validate. Cargo's
build-dir v2, fine-grained-locking, and artifact-uplift boundaries are retained.
This is compatible with the direction of [#5931](https://github.com/rust-lang/cargo/issues/5931),
[#14125](https://github.com/rust-lang/cargo/issues/14125),
[#15010](https://github.com/rust-lang/cargo/issues/15010),
[#16155](https://github.com/rust-lang/cargo/pull/16155), and
[#16147](https://github.com/rust-lang/cargo/issues/16147): upstreamable pieces
are canonical action input auditing, immutable-entry validation, and normal-job
materialization. The fork-local `-Zcargo-cas` storage/coordination policy stays
experimental.

### Precise remaining blockers

Proc macros remain excluded because their execution loads host dylibs and can
observe host/process state; build-script-affected actions remain excluded
because generated files, emitted environment, and native linker inputs lack a
closed ActionKey model. GC is local, explicit age/size eviction only—there is
no automatic global-cache policy. Remote transfer is absent. Path sensitivity
is handled by rejecting any encoded dep-info absolute path, not remapping it.
`cargo clean` intentionally leaves immutable global entries alone; a hit
re-materializes local state. Metadata pipelining is preserved only for the
current rmeta/linkable artifact contract.

## Four-worktree workspace history

The repeatable workspace benchmark is [`scripts/benchmark-ish-cas.py`](scripts/benchmark-ish-cas.py).
It uses the release `cargo-cas` binary, while the measured workspace command is
an explicit debug build:

```text
cargo test -Zcargo-cas --workspace --all-targets --all-features --no-run --profile dev --locked
```

Every completed run appends a machine-readable record to
[`benchmarks/cargo-cas-workspace-history.jsonl`](benchmarks/cargo-cas-workspace-history.jsonl).
The history is append-only so changing the active project or tightening a goal
does not erase earlier baselines.

| Workspace | Worktrees | Measured footprint | Rebuild multiplier | Result |
| --- | ---: | ---: | ---: | --- |
| `ish` (historical) | 4 | 1.235x | 1.018x | pass |
| `pi-agent-core-rs` (historical) | 4 | 1.044x | 1.108x | storage pass; rebuild goal missed |
| `h12tiny` @ `dd20f45` | 4 | 1.069x | 0.971x | pass |
| `pi-agent-core-rs` @ `8be4329` | 4 | 1.033x | 0.939x | pass; instrumented |
| `pi-agent-core-rs` @ `8be4329` | 4 | 1.032x | 1.151x | storage pass; timing goal missed |

The instrumented pi run also records per-`rustc` elapsed time, package, and
category (`rustc`, build script, or proc macro), plus the package names behind
each CAS skip reason. In the parallel restore phase, the largest package totals
were `pi-agent-core` (441.0 s summed across four worktrees), `pi-agent-tui`
(61.0 s), and `rustls` (60.7 s). Native packages accounted for `ring` (20.9 s)
and `mlua-sys` (12.6 s); proc-macro rustc time was 38.6 s. The dominant rebuild
work is therefore the local workspace/test targets, not proc-macro or native
compilation: rebuild rustc time was 1,798.8 s summed across the four worktrees
and three rounds, versus 24.5 s build-script and 15.0 s proc-macro time.

The latest run adds Git-worktree path identity for local package build units.
Parallel restore rose from 373/376 hits to 376/376, and wall time fell from
106.6 s to 88.4 s while the measured footprint stayed at 1.032x. The three
source-edit rebuild rounds were timing-noisy (reference median 92.0 s, goal
median 105.9 s, or 1.151x), so the rebuild threshold missed despite unchanged
540/540 CAS hits and the storage goal passing. Native and proc-macro units remain
explicitly visible and excluded: parallel `ring`/`mlua-sys` skips total 16 and
proc-macro skips total 12.
