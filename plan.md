# Implement `cargo-cas`: a global compiled-artifact cache for Cargo

You are building an experimental fork of Cargo that proves a long-standing upstream objective:

> **compile an immutable dependency once per semantically identical build configuration on a machine, then reuse that compiled artifact across unrelated Cargo workspaces and concurrent worktrees.**

This is an experimental implementation of the direction tracked by upstream Cargo issue:

```text
#5931 — Per-user compiled artifact cache
```

Do not create a wrapper around Cargo.

Do not build an `sccache` clone.

Modify Cargo itself so its compiler scheduler understands globally reusable compilation units as first-class build results.

The project is temporarily called:

```text
cargo-cas
```

The initial objective is a compelling, correct proof of concept rather than a patch optimized prematurely for upstream review.

However, remain architecturally sympathetic to current upstream Cargo so a successful experiment can later inform or become upstream work.

---

# Why this experiment is timely

Current Cargo has already landed much of the prerequisite architecture.

Before writing code, read and understand these upstream issues and PRs in chronological/design order:

```text
#5931   per-user compiled artifact cache

#14125  split intermediate build-dir from final target/artifact output
#15010  reorganize build-dir into package/build-unit-oriented layout

#16807  first stabilization attempt of build-dir layout v2

#17168  build-dir layout scaling fix
#17191  reduce library search-path explosion
#17236  additional layout scaling mitigation
#17258  re-enable v2 layout on nightly
#17354  re-stabilize build-dir layout v2

#4282   granular locking design
#16089  earlier locking experiment
#16155  merged fine-grained build-unit locking
#16307  artifact-dir/check locking separation

#16147  proposed move of build-dir into cargo cache home
#13663  build-script final-artifact staging blocker

#12633  Cargo global cache garbage collection
#13136  build-artifact GC tracking
```

Also read the current Cargo Book documentation for:

```text
build-dir
build cache
fine-grain-locking
checksum-freshness
gc
unit-graph
build analysis
```

Do not implement from this prompt alone.

The upstream source and design history are part of the specification.

---

# Base branch

Start from current `rust-lang/cargo` master **at or after the merge of #17354**.

At the time this plan was written, #17354 was merged as:

```text
36e1162
```

Do not assume that remains current master.

Fetch current upstream master, verify that the relevant prerequisite changes remain present, and record the exact base SHA in:

```text
CARGO_CAS.md
```

Do not base this experiment on an older stable Cargo release or the old build-directory layout.

---

# North-star behavior

Suppose two unrelated repositories resolve the same immutable registry package:

```text
repo-a
└── dependency foo 1.2.3

repo-b
└── dependency foo 1.2.3
```

and Cargo determines that every semantically relevant compilation input is identical.

Today they commonly contain distinct compiled copies under their own build directories.

`cargo-cas` should eventually behave conceptually as:

```text
                    Cargo unit graph
                          │
                          ▼
                  semantic ActionKey
                          │
                ┌─────────┴─────────┐
                │                   │
              cache hit           cache miss
                │                   │
                │                  rustc
                │                   │
                │              staged outputs
                │                   │
                │              atomic publish
                │                   │
                └─────────┬─────────┘
                          │
                          ▼
                global immutable cache
```

Then:

```text
repo-a ─┐
        ├─ uses one compiled dependency artifact set
repo-b ─┘
```

while their own mutable/local compilation state remains independent.

---

# Fundamental safety rule

A cache miss is a performance problem.

A false cache hit is a correctness bug.

Design everything accordingly.

When uncertain whether an input affects compilation:

> include it in the cache identity or declare the unit ineligible.

Never weaken correctness to improve hit rate.

---

# First major principle: do not invent another Cargo fingerprint model

Cargo already understands far more about compilation identity than a naive cache implementation will.

Do **not** define the key as merely:

```text
hash(
    package name,
    version,
    features,
    target,
    rustc version
)
```

That is insufficient.

Before designing the cache key, trace the existing machinery around:

```text
Unit
UnitGraph
Fingerprint
CompileKind
CompileMode
Profile
PackageId
SourceId
CompilationFiles
BuildRunner / JobQueue
rustc command construction
metadata hashes
freshness checking
dep-info
build-dir layout
artifact uplift
locking
```

Determine precisely:

1. which state controls Cargo's existing fresh/dirty decision;
2. which state controls rustc's artifact identity and metadata;
3. which inputs can change generated bytes or ABI;
4. which inputs only affect local bookkeeping;
5. which existing hashes are stable enough to reuse;
6. which existing hashes are deliberately incomplete and therefore cannot become a global-cache identity.

Document this analysis before implementing the cache.

---

# Phase 0 — architecture archaeology

Create:

```text
CARGO_CAS.md
```

Initially document:

```text
upstream Cargo base SHA

relevant issue/PR design lineage

current build-dir v2 layout

existing freshness algorithm

existing fingerprint inputs

existing Unit identity

existing rustc metadata hash

current fine-grained locking lifecycle

artifact uplift behavior

incremental compilation location/lifecycle

which paths enter compiler arguments or artifact metadata
```

Identify the narrowest insertion points for:

```text
eligibility
cache-key generation
lookup
publish
cache-hit artifact registration
```

Do not refactor unrelated Cargo architecture.

---

# Phase 1 — prove artifact reuse before building the cache

This is the first executable experiment.

Answer:

> **Can an ordinary immutable registry dependency compiled by current Cargo be reused directly across unrelated workspaces when every semantically relevant compilation input is identical?**

Construct two independent synthetic projects:

```text
workspace-a
workspace-b
```

They should share a simple registry dependency with:

```text
no build.rs
no proc macros
no native dependencies
```

Prefer Cargo's own test-support infrastructure and synthetic local registries rather than crates.io/network access.

Compile the dependency in workspace A.

Capture:

```text
.rmeta
.rlib where applicable
dep-info
any Cargo fingerprint metadata needed
```

Then make workspace B consume those outputs while proving that B **does not invoke rustc for that dependency**.

Start with:

```text
cargo check
```

Then test:

```text
cargo build
```

Do not proceed to a generalized cache until this works and the exact path/metadata constraints are understood.

---

# Relocatability experiment matrix

Prove which changes preserve reuse and which require misses.

Test at least:

## Expected reuse

```text
different workspace root

different top-level application source

same dependency source
same dependency graph inputs
same features
same profile
same target
same rustc
same relevant flags/config
```

## Expected miss

Change one at a time:

```text
dependency version/source checksum
feature set
profile
opt-level
debug info
debug assertions
overflow checks
panic strategy
LTO
codegen units where relevant
target triple
custom target
host/target distinction
rustc/toolchain identity
RUSTFLAGS
encoded rustflags
relevant .cargo config
relevant environment
dependency version
dependency features
dependency compiled identity
compile mode
```

This matrix must become permanent regression coverage.

---

# Compare artifact bytes

For controlled identical builds, compare the dependency outputs from two unrelated workspaces.

Determine whether:

```text
.rmeta
.rlib
object members
dep-info
```

are byte-identical.

If not, determine why.

Likely sources to investigate include:

```text
absolute paths
debug info
CARGO_HOME
build directory
dependency artifact paths
rustc metadata
incremental state
```

Do not paper over non-relocatability by copying arbitrary workspace metadata.

Understand it.

If path sanitization/remapping is required for safe global reuse, make that an explicit architectural finding.

Do not silently force `trim-paths` unless evidence establishes that it is necessary and semantically acceptable.

---

# Phase 2 — define cacheability conservatively

For V0, a unit is eligible only when Cargo can make a strong closed-world claim.

Start with:

```text
registry package
immutable source
pure Rust
normal library compilation
no build.rs involvement
no proc-macro involvement
no mutable path source
```

Be deliberately conservative.

### Explicitly ineligible in V0

```text
workspace members
path dependencies
build-script units
packages whose compilation is affected by build-script output
dependents transitively affected by build scripts

proc-macro crates
units whose compilation executes proc macros
proc-macro-dependent units

native-linking packages requiring arbitrary external discovery
custom build outputs

examples
tests
benches
bins
final/root artifacts

rustdoc

incremental state
```

If classification is uncertain:

```text
ineligible → normal Cargo behavior
```

The fallback must remain exactly normal Cargo semantics.

---

# Why build scripts are deferred

Do not solve build scripts in V0.

A `build.rs` can observe arbitrary host state such as:

```text
environment
filesystem
pkg-config
native libraries
compiler
kernel
time
network
git repository
generated files
```

Current Cargo itself treats this as a major obstacle to global build-dir/cache work.

Do not invent an unsound heuristic.

Later work may introduce:

```text
declared inputs
explicit cacheability contracts
semantic build scripts
or other upstream-compatible mechanisms
```

but V0 simply excludes them and affected units.

---

# Why proc macros are deferred

Separate these two ideas:

```text
compiling a proc-macro dylib
```

and:

```text
executing a proc macro while compiling another crate
```

The former may eventually be safely cacheable.

The latter can observe host/environmental inputs that Cargo does not necessarily model.

For the first implementation, exclude both proc-macro units and proc-macro-dependent compilation.

Widen this only after the basic model is proven.

---

# Phase 3 — define an explicit semantic ActionKey

Do not call an output-content digest the cache lookup key.

The cache needs an identity available *before compilation*:

```text
ActionKey = H(canonical semantic compilation inputs)
```

The exact canonical inputs must be derived from current Cargo's semantics, not guessed from this list.

Expect them to include concepts such as:

```text
immutable package source identity/content checksum

Unit identity
target identity
compile kind
compile mode
profile
resolved features

rustc identity
sysroot/toolchain identity where relevant

all effective compiler arguments
all ABI/codegen-affecting configuration

relevant Cargo configuration

relevant fingerprinted environment

dependency artifact identities
```

Dependency identities should form a DAG:

```text
foo ActionKey
   │
   ├── bar ActionKey
   └── baz ActionKey
```

rather than depending on arbitrary workspace-local output paths.

---

# Canonicalization requirement

The ActionKey representation must be:

```text
deterministic
versioned
unambiguous
order-stable
free of irrelevant workspace-local paths
```

Create something conceptually like:

```rust
struct CacheKeyInputV0 {
    ...
}
```

Do not hash ad-hoc debug strings.

Do not rely on Rust's unstable `Hash` representation for a persistent on-disk key.

Serialize a deliberate canonical representation, then hash it.

Prefer existing Cargo hashing/digest facilities where they satisfy collision and stability requirements.

Do not add a new hashing dependency casually.

For a global correctness cache, collision resistance matters.

---

# Distinguish ActionKey from artifact digest

Architecturally distinguish:

```text
ActionKey
    hash of inputs
    answers: "have we already performed this compilation?"

ArtifactSet
    outputs produced by that compilation

ArtifactDigest
    optional digest(s) of actual artifact bytes
```

V0 may store one immutable artifact directory directly under its ActionKey.

That is acceptable.

Do not prematurely build a deduplicating blob store unless evidence shows it is useful.

The design should permit one later.

---

# Phase 4 — immutable cache storage

Use an experimental cache location under Cargo's user cache home.

Keep it versioned and clearly experimental, for example conceptually:

```text
$CARGO_HOME/cache/cargo-cas-v0/
```

or whatever fits current Cargo's cache layout best after source inspection.

Do not overload current workspace `target/`.

Do not make the normal `build-dir` globally shared by accident.

The initial model is:

```text
workspace build-dir
    mutable/local state

target/artifact-dir
    final user-facing outputs

global cargo-cas
    immutable reusable dependency artifacts
```

---

# Artifact entry

Each entry should contain enough information to reject corruption or incompatible formats.

Conceptually:

```text
<ActionKey>/
    manifest
    artifacts...
```

Manifest should record at least:

```text
cache format version
ActionKey
unit/package identity
toolchain identity
artifact roles
artifact paths relative to entry
artifact sizes

optional/desired:
artifact content hashes
dependency ActionKeys
```

Do not encode unrelated workspace paths into the persistent manifest.

---

# Publication protocol

Never compile directly into a publicly visible final cache entry that readers may observe half-written.

Use:

```text
cache/
    tmp/<unique-writer>
```

Build/stage the complete entry there.

Validate it.

Then atomically publish it on the same filesystem.

Conceptually:

```text
write temporary entry
    ↓
finish compilation
    ↓
write manifest
    ↓
validate
    ↓
atomic rename/publish
```

After publication:

> **the cache entry is immutable.**

Never mutate artifacts in-place.

If state must change, create a new ActionKey/entry.

---

# Interrupted writers

Test:

```text
kill -9 Cargo during publication
```

The next Cargo invocation must:

```text
ignore incomplete temporary state
treat absent final entry as miss
continue correctly
```

No partially published result may become a hit.

---

# Corrupted cache entries

Add tests that intentionally:

```text
truncate artifact
delete artifact
damage manifest
alter size/hash
```

Expected behavior:

```text
detect invalid entry
treat as miss
optionally quarantine/remove entry
rebuild correctly
```

Do not allow global-cache corruption to make Cargo unusable.

---

# Phase 5 — integrate lookup into Cargo's scheduler

This should be a first-class Cargo execution path.

Conceptually:

```text
Unit registered
    ↓
eligible?
    ├── no  → ordinary Cargo path
    │
    └── yes
         ↓
      ActionKey
         ↓
     cache lookup
      ├── hit
      │    ↓
      │ register artifact result
      │ skip rustc entirely
      │
      └── miss
           ↓
       ordinary compile
           ↓
        publish
```

A cache hit must integrate into Cargo's dependency scheduling exactly as a completed compilation unit would.

Do not fake hits by invoking rustc and hoping it decides there is nothing to do.

Acceptance criterion:

> **zero rustc process invocation for an eligible cached unit.**

---

# Do not blindly copy Cargo fingerprints

Determine whether a cache hit should:

```text
synthesize local freshness state
reuse part of existing fingerprint metadata
or bypass local fingerprint freshness for the globally cached unit
```

Choose the smallest semantically sound design.

The global ActionKey and Cargo's local incremental/freshness bookkeeping do not have to be identical concepts.

Avoid coupling the new cache format directly to incidental on-disk fingerprint files if a clearer semantic boundary exists.

---

# Prefer direct artifact consumption

If rustc and Cargo can safely consume dependency artifacts directly from the immutable cache:

```text
--extern foo=/.../cargo-cas/.../libfoo.rmeta
```

prefer that over unnecessary copies into every workspace.

Validate this experimentally.

If local materialization is required, treat it as a transport concern.

Possible order:

```text
direct reference
reflink/clone
hardlink
copy
```

depending on platform and semantics.

Do not make correctness depend on hardlinks.

---

# Keep incremental compilation local

This is a hard architectural boundary for V0.

Do not put rustc incremental compilation state into the global cache.

Keep:

```text
workspace/build-dir/.../incremental
```

local and mutable.

The global cache is for reusable completed dependency artifacts.

Conceptually:

```text
GLOBAL IMMUTABLE
    rmeta
    rlib
    eligible compiled dependency outputs

LOCAL MUTABLE
    incremental query/work-product state
    actively edited workspace crates
```

Do not conflate them.

---

# Phase 6 — first real feature: `cargo check`

Make the first integrated cache path support:

```text
cargo check
```

for eligible immutable registry library dependencies.

Why first:

```text
smaller artifact surface
no final binary uplift
excellent development-loop relevance
fewer linker concerns
```

Acceptance test:

```text
project A:
    cargo-cas check
    → dependency rustc executes

project B:
    unrelated directory
    same eligible dependency/config
    cargo-cas check
    → dependency rustc DOES NOT execute
```

Both must produce the same successful semantic result as upstream Cargo.

---

# Cache observability

Under verbose/debug logging, make cache decisions visible.

For example conceptually:

```text
CAS hit: foo v1.2.3 (...)
CAS miss: bar v2.0.0 — key absent
CAS skip: baz — build script
CAS skip: quux — proc-macro dependent
CAS reject: xyz — corrupt entry
```

Do not spam ordinary Cargo output by default.

Tests should be able to assert hit/miss/skip reasons.

---

# Phase 7 — key correctness matrix

Build comprehensive Cargo integration tests.

Use synthetic local registry packages where possible.

## Must hit

Two unrelated workspaces with semantically identical:

```text
registry package
source checksum
features
profile
target
toolchain
compiler flags
dependency inputs
```

## Must miss

Change individually:

```text
crate source/checksum
crate version
features
dependency features
dependency version
profile
opt level
debug settings
panic
LTO
codegen configuration
target
target specification
host-vs-target role
rustc version/commit/toolchain
RUSTFLAGS
relevant Cargo config
relevant environment
compile mode
```

Prefer false misses over false hits.

Every newly discovered cache-key input becomes a permanent test.

---

# Cache poisoning tests

Construct adversarial cases.

Examples:

```text
same package name/version but different source contents
different local registry source
manually altered cache entry
changed compiler
changed target JSON
changed dependency artifact identity
```

No incorrect hit is acceptable.

---

# Phase 8 — `cargo build`

Once `cargo check` is correct, extend eligible normal library units to:

```text
cargo build
```

Reuse:

```text
.rlib
.rmeta
```

and any genuinely required companion artifacts.

Do not cache final root-package binaries yet.

Preserve normal:

```text
target-dir / artifact-dir
```

behavior for user-facing outputs.

---

# Respect pipelining

Study Cargo/rustc pipelined compilation carefully.

On a cache miss, normal compilation should retain current pipelining behavior.

Do not delay `.rmeta` unnecessarily merely because the final cache entry cannot be published until the unit completes.

On a cache hit, required metadata/library artifacts are immediately available.

Do not redesign pipelining unless absolutely necessary.

Document any interaction with the open question in #5931.

---

# Phase 9 — concurrency correctness

Start simple.

Upstream #5931 explicitly allows the first shared-cache experiment to use coarse locking.

For the earliest correct implementation:

```text
one conservative cache lock
```

is acceptable.

Prove correctness first.

Then implement the design needed for the real value proposition.

---

# Per-ActionKey coordination

Eventually two processes requesting the same missing ActionKey should not independently compile it.

Desired behavior:

```text
process A            process B

lookup MISS          lookup MISS
    │                    │
acquire key lock      waits
    │                    │
recheck MISS             │
    │                    │
compile                   │
publish                   │
release                   │
                         wakes
                         recheck HIT
                         reuse
```

Different ActionKeys must proceed concurrently.

Do not introduce a global serialization point.

---

# Reuse upstream locking lessons

Study #16155 before implementing new locking.

Important lessons include:

```text
per-build-unit state
shared vs exclusive locks
pipelined compilation
FD pressure
fallback to coarse locking
cargo clean interaction
network filesystems
```

Do not blindly duplicate #16155's exact mechanism if immutable CAS semantics allow something simpler.

An immutable cache should make reader concurrency significantly easier.

But reuse upstream infrastructure where appropriate rather than creating a second unrelated lock framework.

---

# Avoid FD explosion

Upstream already encountered thousands of simultaneously held file descriptors with per-unit locks on large graphs.

Do not repeat that mistake.

Design per-key cache coordination so:

```text
number of open lock descriptors
```

does not scale pathologically with the entire dependency graph.

Prefer locks held only when needed.

Benchmark large graphs.

---

# Phase 10 — north-star worktree test

Create an integration/stress harness modeling agentic development.

Conceptually:

```text
repo
├── worktree-1
├── worktree-2
├── worktree-3
├── worktree-4
├── worktree-5
├── worktree-6
├── worktree-7
└── worktree-8
```

Each worktree has different local/root source changes.

They share immutable registry dependencies.

Launch builds concurrently.

Required behavior:

```text
eligible shared dependencies:
    built at most once per ActionKey
    reused by all worktrees

local crates:
    compiled independently

unrelated ActionKeys:
    compile concurrently

global cache:
    no corruption

Cargo:
    no global build serialization
```

This is the project's flagship demonstration.

---

# Measure process counts, not just elapsed time

Instrumentation must report:

```text
rustc invocations
cache hits
cache misses
cache skips by reason
duplicate-build avoidance
wall time
CPU time where useful
cache disk usage
workspace build-dir size
```

Wall-clock time alone can conceal an incorrect implementation.

A cache-hit dependency must demonstrably avoid rustc.

---

# Phase 11 — git dependencies

After registry dependencies are proven, add immutable git dependencies.

Eligibility requires a resolved immutable source identity, normally including the exact revision and content semantics Cargo already uses.

Do not treat:

```text
branch = "main"
```

as the identity.

Use the resolved immutable source.

Retain all build-script/proc-macro restrictions.

---

# Proc macros later

After the fundamental design is proven, investigate whether **compiling the proc-macro crate itself** can be globally cached independently from the nondeterminism of executing it.

Do not automatically broaden cacheability to its dependents.

Build an explicit correctness model first.

This is a future vertical slice, not required for the core PoC.

---

# Build scripts much later

Do not attempt arbitrary build-script caching until there is a defensible input model.

Potential future directions include:

```text
explicitly declared inputs
semantic/declarative build scripts
trusted cacheable contracts
sandboxed observation
```

but none belong in V0.

The cache should deliver enormous value before solving arbitrary `build.rs`.

---

# Phase 12 — GC only after reuse works

Do not block the core experiment on garbage collection.

Initially allow an explicit experimental cache directory that grows.

Provide at most a simple safe:

```text
clear cargo-cas cache
```

developer mechanism if needed.

Once caching is proven, investigate integration with Cargo's existing global-cache usage tracking and GC architecture.

Do not independently invent a database-heavy GC system if Cargo already has suitable infrastructure.

Desired eventual semantics:

```text
last-used tracking
size/age policy
safe deletion of immutable entries
```

Deleting an old entry should never affect correctness:

```text
next use → cache miss → rebuild
```

---

# Remote cache is a non-goal

Do not implement:

```text
HTTP cache server
S3
distributed execution
authentication
remote CAS
cache federation
```

The architecture should not deliberately prevent a future remote action cache, but remote operation is irrelevant until local correctness is proven.

---

# Do not build a daemon

No persistent service should be required.

The local cache should work using:

```text
filesystem
immutable entries
small locking/coördination layer
```

A daemon can be evaluated only if evidence later demonstrates a need.

---

# Do not build a database

Do not introduce SQLite or another database for the artifact cache.

For V0:

```text
filesystem hierarchy
versioned manifests
atomic publication
locks
```

are sufficient.

Cargo's existing cache metadata/GC implementation may later justify additional state.

---

# Phase 13 — performance/scaling discipline

The new build-dir layout has already taught upstream an important lesson:

> per-unit layouts can accidentally explode compiler/linker/search-path argument lists.

Do not regress this.

Study the fixes around:

```text
#17168
#17191
#17236
```

Benchmark with dependency graphs large enough to expose:

```text
-L explosion
LD_LIBRARY_PATH explosion
PATH explosion
command-line length limits
filesystem traversal costs
too many open files
scheduler overhead
```

A cache architecture that works for 20 crates but fails for Zed-scale graphs is not successful.

---

# Benchmark against current upstream, not old Cargo

Record:

```text
upstream Cargo base SHA
cargo-cas SHA
rustc -Vv
machine
```

Compare:

## Cold

```text
upstream cold build
cargo-cas empty-cache build
```

The experimental cache must not introduce an absurd cold-build regression.

## Same workspace warm

Ensure ordinary Cargo behavior is not materially degraded.

## Unrelated workspace warm

This is the key case.

```text
workspace A builds dependency set
workspace B builds same eligible dependency set
```

Measure avoided rustc work.

## Concurrent worktrees

Measure:

```text
1
2
4
8
```

concurrent worktrees.

Report:

```text
rustc invocation count
wall time
CPU use if practical
disk use
```

---

# Disk-space measurement

Compare:

```text
N isolated ordinary build dirs
```

against:

```text
N local build dirs
+
one shared immutable cache
```

Do not count source-download cache as a cargo-cas benefit.

Report only compiled-artifact savings attributable to the implementation.

---

# V0 user interface

Keep the feature experimental and opt-in.

Use a fork-local unstable flag such as:

```text
-Zcargo-cas
```

unless current Cargo architecture suggests a better non-conflicting experimental name.

Do not claim this is the eventual upstream spelling.

The purpose is to isolate experimental semantics cleanly.

Without the flag:

> Cargo behavior must remain unchanged.

---

# Suggested V0 cacheability diagnostics

With Cargo verbosity or tracing enabled, expose an eligibility reason.

Examples:

```text
eligible: immutable registry unit

ineligible:
    workspace/path source
    build script
    build-script affected
    proc macro
    proc-macro affected
    unsupported target kind
    unsupported compile mode
```

This will be invaluable for understanding real-world hit rate.

---

# Test strategy

Use Cargo's existing integration-test infrastructure heavily.

Do not construct most tests by shelling out to crates.io.

Create tiny deterministic fixture packages and local registries.

Every bug found during real-world testing should become a reduced Cargo integration test.

Required test categories:

```text
cache hit
cache miss
eligibility
key invalidation
corrupt cache
interrupted writer
concurrent readers
concurrent same-key writers
concurrent different-key writers
workspace separation
clean behavior
build-dir behavior
target/artifact output behavior
fine-grain-lock interaction
pipelining
```

Run the normal upstream Cargo test suite throughout development.

Do not allow cargo-cas changes to silently alter non-CAS builds.

---

# Failure behavior

The cache is an optimization.

Where possible, a cache-infrastructure failure should safely degrade to:

```text
miss / normal rebuild
```

rather than preventing a valid build.

However, do not hide real filesystem errors that ordinary Cargo would report.

Classify errors intentionally.

Examples:

```text
entry absent        → miss
entry incompatible  → miss
entry corrupt       → reject + rebuild
writer died         → recover
permission failure  → clear diagnostic
disk full           → clear diagnostic / appropriate fallback
```

---

# Cache-format versioning

From day one include a cache format version.

For example:

```text
cargo-cas-v0
```

Changing cache semantics should permit wholesale invalidation.

Never preserve an unsafe cache hit merely for format compatibility.

A global build cache must be aggressively versionable.

---

# Security/correctness posture

Treat globally reusable compiler artifacts as potentially sensitive correctness state.

Consider:

```text
malicious/corrupt local entry
symlink attacks
path traversal in manifests
partial writes
hash collisions
wrong ownership/permissions
cache path substitution
```

The threat model is primarily a user-local cache, not an untrusted remote cache, but basic filesystem safety still matters.

Never follow arbitrary manifest paths outside the cache entry.

---

# Stage gates

Do not attack all of Cargo caching simultaneously.

## Gate 0 — upstream archaeology

Complete when:

```text
current master based on post-#17354 Cargo
CARGO_CAS.md documents current architecture
fingerprint/key inputs understood
cache insertion points identified
```

## Gate 1 — relocatability proof

Complete when:

```text
one simple registry dependency
compiled in workspace A
is consumed in workspace B
without recompiling it
```

and invalidation experiments are documented.

If this fails fundamentally, stop and explain exactly what rustc/Cargo changes are required.

Do not bluff forward.

## Gate 2 — `cargo check` action cache

Complete when:

```text
eligible registry deps are globally reused
second unrelated workspace executes zero rustc for hits
incorrect-hit matrix is green
normal Cargo fallback remains intact
```

## Gate 3 — `cargo build`

Complete when:

```text
eligible .rmeta/.rlib artifact sets are reusable
normal final-artifact behavior remains unchanged
```

## Gate 4 — crash/corruption safety

Complete when:

```text
partial writes cannot become hits
corrupt entries are detected
writer crashes recover
```

## Gate 5 — concurrent CAS

Complete when:

```text
same key coordinates correctly
different keys build concurrently
no global serialization
no FD explosion
```

## Gate 6 — worktree proof

Complete when eight concurrent worktrees can share eligible dependencies safely and avoid redundant rustc invocations.

At this point the PoC has proven the central thesis.

## Gate 7 — broaden immutable sources

Add safe git dependencies.

## Gate 8 — GC

Only after the cache has proven valuable.

## Future gates

```text
proc-macro compilation
build-script-safe models
remote cache
```

These are explicitly not required for the initial success.

---

# Flagship acceptance scenario

Create a reproducible demonstration script.

Something conceptually like:

```sh
./scripts/demo-cas.sh
```

It should create isolated workspaces/worktrees and demonstrate:

```text
1. Empty global cache

2. Workspace A:
   eligible deps compile normally
   cache populated

3. Unrelated workspace B:
   same eligible deps
   zero rustc invocations for cache hits

4. Change application source:
   deps still hit

5. Change an eligible dependency feature:
   correct miss

6. Restore feature:
   previous cache entry hits again

7. Launch eight concurrent worktrees:
   common missing ActionKey compiled once
   common existing ActionKeys reused
   local crates compile independently
   no corruption
```

Print a concise summary:

```text
cold rustc invocations:       N
second-workspace invocations: M
avoided invocations:          X
cache hits:                   X
cache misses:                 Y
cache size:                   Z
```

---

# Real-world experiment

After synthetic correctness is established, test against several large real Rust repositories.

Do not require their entire dependency graph to be eligible.

Instrument:

```text
total units
eligible units
cache hits
cache misses
skip reasons
```

This tells us how much benefit the conservative V0 subset already provides.

Do not expand eligibility merely to make the benchmark look good.

---

# Benchmark report

Create:

```text
CARGO_CAS_RESULTS.md
```

containing reproducible measurements.

Include:

```text
Cargo upstream SHA
cargo-cas SHA
rustc version
OS/CPU
cache format version
```

and tables for:

```text
cold build
warm same-workspace
warm unrelated-workspace
concurrent 2/4/8 worktrees
disk usage
rustc invocation counts
```

The most important metric is:

> **duplicate rustc compilation work eliminated safely.**

---

# Upstream compatibility mindset

This is an experiment, but do not gratuitously diverge from Cargo.

Prefer:

```text
existing Unit semantics
existing fingerprint machinery
existing build-dir v2 layout concepts
existing locks where suitable
existing Cargo cache home
existing test-support infrastructure
existing GC infrastructure later
```

over parallel reinvention.

Document where cargo-cas deliberately differs from the design implied by #5931.

If experimentation proves an upstream assumption wrong, document the evidence.

---

# What not to do

Do not:

```text
create a Cargo wrapper
depend on sccache
share one mutable target directory
globally share rustc incremental state
cache path/workspace packages
cache arbitrary build scripts
cache proc-macro dependents initially
implement remote caching
build a daemon
introduce a database
make hardlinks a correctness requirement
use the existing output filename hash blindly as ActionKey
assume mtimes define artifact identity
force cache hits when uncertain
optimize for symbolically "100% cacheable" Cargo
```

A limited cache that is provably correct is vastly more valuable than an ambitious cache that occasionally returns the wrong artifact.

---

# Engineering workflow for the coding agent

Work in vertical slices.

For each capability:

```text
inspect existing Cargo semantics
    ↓
write reduced test
    ↓
implement smallest integration
    ↓
prove hit
    ↓
prove required misses
    ↓
prove normal fallback
    ↓
prove concurrency/failure semantics
    ↓
benchmark
```

Do not build the full storage abstraction before proving a single artifact can actually be safely reused.

Do not spend the whole run writing design documents.

The first important executable result is:

> **Workspace B successfully consumes Workspace A's registry dependency artifact without invoking rustc.**

Get there quickly and rigorously.

---

# Commit discipline

Keep commits reviewable, for example:

```text
docs: map Cargo fingerprint and build-dir cache semantics

test: prove registry artifact reuse across workspaces

feat: add conservative cargo-cas eligibility classification

feat: derive versioned ActionKey for registry units

feat: add immutable artifact cache lookup

feat: publish completed unit artifacts atomically

test: invalidate cache on feature/profile/toolchain changes

feat: reuse cached rmeta for cargo check

feat: reuse rlib artifacts for cargo build

test: recover from interrupted cache writer

feat: coordinate concurrent same-key compilations

bench: add multi-worktree cargo-cas demonstration
```

Avoid giant mixed patches.

---

# Final deliverable

At the end of the coding run, provide a technical report answering:

## Does the thesis work?

Can Cargo safely reuse compiled immutable dependencies across unrelated workspaces without invoking rustc again?

## What is cacheable?

Give the exact V0 eligibility rules.

## What constitutes an ActionKey?

Document every semantic input and where it came from in Cargo's existing model.

## Artifact relocatability

Which artifacts are directly reusable?

What path/toolchain constraints exist?

## Correctness

Report the cache invalidation test matrix.

## Concurrency

Report:

```text
same-key behavior
different-key behavior
locking design
FD usage
eight-worktree test
```

## Failure recovery

Report behavior for:

```text
crashed writer
corrupt entry
missing artifact
disk/full permission failures
```

## Performance

Report:

```text
rustc invocations avoided
cold overhead
warm unrelated-workspace speedup
multi-worktree speedup
disk savings
```

## Upstream delta

Compare the implementation directly to the direction in:

```text
#5931
#14125
#15010
#16155
#16147
```

Identify what could plausibly become upstream work and what remains intentionally experimental.

## Remaining blockers

Especially:

```text
proc macros
build scripts
GC
remote cache
path sensitivity
pipelining
cargo clean semantics
```

Do not describe future work vaguely.

State the precise unresolved mechanism.

---

# End-state vision

The eventual architecture should look roughly like:

```text
                         Cargo
                           │
                       UnitGraph
                           │
                     ActionKey(unit)
                           │
             ┌─────────────┴─────────────┐
             │                           │
       immutable global CAS          local build
             │                           │
      registry/git deps          workspace/path crates
             │                    incremental state
             │                           │
             └─────────────┬─────────────┘
                           │
                        linker
                           │
                    final artifact-dir
```

The development experience we are trying to prove is:

```text
clone project
     ↓
cargo check
     ↓
compile only what this machine
has genuinely never compiled
under these semantics before
```

And under agentic multi-worktree development:

```text
8 worktrees
     │
     ├── same immutable dependency → compile once
     ├── same immutable dependency → reuse
     ├── same immutable dependency → reuse
     └── local edits → independent compilation
```

The cache is not primarily about clever storage.

It is about recognizing that:

> **an immutable Rust compilation unit with identical semantic inputs is work the machine should never have to perform twice.**

Prove that proposition first.

Everything else follows.
