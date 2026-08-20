# `cargo-cas` V1 results

This report records reproducible macOS measurements for the experimental,
opt-in `-Zcargo-cas` implementation. It is evidence for the conservative V1
subset, not a claim that every Cargo unit is cacheable.

## Provenance

| Field | Value |
| --- | --- |
| Upstream Cargo source | `514c56dd7321eecbfdcf9b6479519cf4edfab906` |
| `cargo-cas` source used for the release binary | `e30c34ede51454552e083008aff806bacb350962` |
| Cache format | `cargo-cas-v1` / manifest format `1` |
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
| Upstream cold | 1 | 3 | 1 s |
| Upstream same workspace warm | 0 | 0 | 0 s |
| Upstream unrelated workspace warm | 1 | 3 | 0 s |
| Upstream concurrent 2 | 2 | 6 | 1 s |
| Upstream concurrent 4 | 4 | 12 | 1 s |
| Upstream concurrent 8 | 8 | 24 | 2 s |
| cargo-cas cold | 1 | 3 | 1 s |
| cargo-cas same workspace warm | 0 | 0 | 0 s |
| cargo-cas unrelated workspace warm | 0 | 2 | 1 s |
| cargo-cas concurrent 2 | 1 | 5 | 1 s |
| cargo-cas concurrent 4 | 1 | 9 | 1 s |
| cargo-cas concurrent 8 | 1 | 17 | 3 s |

The decisive comparisons are the unrelated workspace (one avoided shared
compile) and the 2/4/8 worktree cohorts (1 shared compile instead of 2/4/8).
The remaining rustc processes are independent root crates, which V1 correctly
keeps local.

For equivalent sets of two sequential workspaces plus 2, 4, and 8 worktrees:

| Storage | Bytes |
| --- | ---: |
| Upstream workspace target directories | 1,507,328 |
| cargo-cas workspace target directories | 1,409,024 |
| cargo-cas immutable cache | 49,152 |
| cargo-cas local targets plus cache | 1,458,176 |

This small fixture saves 49,152 bytes after counting the cache itself. It is a
sanity check on the intended ownership boundary, not a claim of large
real-world disk savings. CPU time was not recorded because the synthetic run is
below the timer's useful resolution.

### Reproduce

```sh
cargo build --release -p cargo

git worktree add --detach /tmp/cargo-upstream 514c56dd7321eecbfdcf9b6479519cf4edfab906
(cd /tmp/cargo-upstream && cargo build --release -p cargo)

CARGO_UPSTREAM_BIN=/tmp/cargo-upstream/target/release/cargo \
  CARGO_CAS_BIN="$PWD/target/release/cargo" \
  ./scripts/benchmark-cas.sh
```

Set `CARGO_CAS_BENCHMARK_KEEP=1` to retain the isolated inputs, target
directories, logs, and counters for inspection.

## Real-world cacheability experiment

Three pinned public repositories were checked sequentially with one fresh
`CARGO_HOME`, a fresh target directory per project, and
`CARGO_LOG=cargo::compiler::cas=debug`. Counts are parsed from the structured
cache-decision logs; `rustc` is the observed number of compiler invocations.

| Repository | Revision | rustc | Hits | Misses | Skips | Wall time |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| ripgrep 14.1.1 | `4649aa9700619f94cf9c66876e9549d83420e16c` | 22 | 0 | 32 | 23 | 5.55 s |
| fd 10.2.0 | `b19136871310b01500b4f09eadd7387b8476be47` | 52 | 7 | 58 | 33 | 6.89 s |
| bat 0.25.0 | `25f4f96ea3afb6fe44552f3b38ed8b1540ffa1b3` | 55 | 95 | 0 | 72 | 15.85 s |

The shared cache occupied 95,969,280 bytes across 131 entries after the three
runs. The observed skip reasons were 55 build-script units, 52 units with an
ineligible dependency action, 15 path/workspace units, and 6 proc-macro units.
This is the intended conservative result: cacheability is not expanded merely
to improve the hit rate.

The bat run is especially useful as a cross-repository warm case: it restored
95 eligible actions without a new eligible miss, while its local and excluded
units still compiled normally. Every project completed successfully.

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
