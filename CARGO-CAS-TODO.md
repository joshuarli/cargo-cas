# cargo-cas TODO

This file is deliberately separate from [`CARGO_CAS_ARCHITECTURE.md`](CARGO_CAS_ARCHITECTURE.md).
The architecture document describes implemented behavior and the current
fallback boundary; this file records work that is not implemented and must not
be treated as an available capability.

## Cacheability expansions

- Define and test a closed-world identity for arbitrary build scripts,
  including filesystem, environment, tool discovery, generated files, and
  network observations. The current implementation supports only the narrow
  replayable subset described in the architecture document.
- Establish a safe model for proc-macro compilation and execution, including
  host-process state and loaded dynamic libraries.
- Model native-linking packages and external linker/tool discovery.
- Decide whether and how to share final/root artifacts, binaries, examples,
  tests, benches, rustdoc output, and non-host targets.
- Investigate a separate contract for rustc incremental state; it must not be
  mixed into the immutable artifact entry used today.

## Portability and distribution

- Re-run the relocatability, locking, crash, and symlink-safety matrix on
  supported non-macOS platforms and filesystems before claiming portability.
- Design remote transfer, authentication, trust, and cache poisoning policy if
  entries are ever exchanged between machines. The current cache is local-only.
- Define an automatic global CAS eviction policy, if desired. Current GC is
  explicit age/size eviction only.

## Architecture and upstreaming

- Compare the fork-local scheduler/materialization seams with current upstream
  Cargo as the build-dir and locking designs evolve.
- Separate stable upstream contracts from the experimental `-Zcargo-cas`
  storage and activation policy.
- Add format migration tooling only if retaining old entries becomes useful;
  current format changes intentionally produce safe misses.

Every TODO item that changes eligibility, identity, storage, or scheduling must
first add an observable regression test and update the architecture document in
the same change.
