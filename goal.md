Goal: make cargo-cas materially reduce real disk usage in everyday Cargo builds.

Work in /Users/josh/d/cargo-cas. Read CARGO-CAS-GUIDE.md and the existing benchmark harness before changing code.

Current verified state:

- epsh and ish are configured with:
  - local epsh path dependency
  - incremental = false
  - aligned dev profiles
  - aligned bitflags lock version
  - aligned rustix feature sets
- Fresh vanilla targets are approximately:
  - epsh: 45 MiB
  - ish: 56 MiB
- Fresh cargo-cas targets are approximately:
  - epsh: 44 MiB
  - ish: 55 MiB
  - shared CAS cache: 29 MiB
- Therefore current cargo-cas uses about 128 MiB total versus about 101 MiB for vanilla.
- The four-worktree harness reports excellent hit rates, but raw target-plus-cache storage is still 1.158× vanilla. Its CoW-aware estimate is 0.723×, which is not sufficient evidence of real user-visible disk savings.
- Some standalone-vs-dependency artifacts are rejected as “unexpected artifacts” because Cargo expects different materialized filenames.

Primary objective:

Reduce actual on-disk storage, not merely compiler time or a theoretical CoW estimate.

Define:

- B = allocated disk usage of fresh vanilla epsh + ish target directories.
- C = allocated disk usage of cargo-cas epsh target + ish target + cargo-cas cache.

Success criteria:

1. C <= 0.60 × B for the fresh epsh-then-ish workflow.
2. Warm rebuilds must remain no slower than vanilla within 10%.
3. The cache must not grow without bound; add a safe configurable size limit or eviction policy if necessary.
4. Measure both logical bytes and filesystem-allocated bytes. Do not claim success based only on clonefile/CoW estimates.
5. Preserve correctness and Cargo compatibility:
   - clean builds
   - warm builds
   - source edits
   - deleting target directories
   - deleting or corrupting cache entries
   - concurrent builds
   - read-only cache entries
   - normal Cargo metadata/fingerprint behavior
6. Preserve cache safety: immutable validated artifacts, conservative rejection, relocatability, and no unsafe sharing across compiler contracts.

Investigate the storage architecture first. In particular, determine whether the main win should come from:

- avoiding duplicate cache-plus-target materialization;
- using safe hardlink, reflink, clonefile, or symlink strategies;
- letting Cargo consume immutable cached artifacts without copying them;
- caching additional root/library artifacts safely;
- deduplicating target metadata or dependency artifacts;
- pruning cache entries by size/LRU;
- fixing the standalone-vs-dependency artifact-name rejection;
- or a combination of these.

Do not game the measurement by deleting required Cargo state, excluding target files, or counting only CoW-aware bytes. The resulting target directories must still support ordinary Cargo commands.

Use the existing epsh/ish pair as the primary real-world fixture. Add focused regression tests for every storage/materialization behavior changed. Run the narrowest tests first, then the relevant cargo-cas benchmark harness, and report:

- vanilla and cargo-cas logical/allocated bytes;
- target bytes versus cache bytes;
- cold and warm timings;
- hit/miss/reject/skip counts;
- cache size before and after eviction;
- correctness results.

If the 0.60× target cannot be reached without a Cargo change or a broader architectural change, explain the hard bound with measurements and implement the best safe improvement rather than stopping at a high hit rate.

Keep repository changes coherent and document the final storage contract in CARGO-CAS-GUIDE.md.

Make one commit per significant win or group of logically related changes.
