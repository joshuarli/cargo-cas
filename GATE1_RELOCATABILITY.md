# Gate 1: artifact relocatability findings

The permanent executable proof is
`tests/testsuite/gate1_relocatability.rs`. It is macOS-only while the
experiment is macOS-only.

The test publishes a pure-Rust, no-`build.rs`, no-proc-macro package to Cargo's
local test registry. Two unrelated virtual workspaces use that same immutable
package, but their application package names, roots, and source files differ.

Cargo's existing pre-CAS artifact layout is an immutable dependency unit under
the target directory:

```text
<target-dir>/<profile>/build/gate-one-dep/<unit-hash>/out/libgate_one_dep-<hash>.rmeta
<target-dir>/<profile>/build/gate-one-dep/<unit-hash>/out/libgate_one_dep-<hash>.rlib
<target-dir>/<profile>/build/gate-one-dep/<unit-hash>/out/gate_one_dep-<hash>.d
<target-dir>/<profile>/build/gate-one-dep/<unit-hash>/fingerprint/...
```

The exact unit hash and artifact hash are compiler/fingerprint details and are
therefore asserted by discovery rather than hard-coded. The test snapshots
the dependency's `.rmeta`, `.rlib`, dep-info, and stable fingerprint metadata
before and after workspace B runs. `invoked.timestamp` is intentionally not
compared because Cargo may touch it while checking freshness.

The test first uses an explicitly shared target directory as the baseline,
then manually materializes only the dependency's `debug/build/gate-one-dep`
subtree (outputs plus Cargo fingerprint metadata) into a different target
directory. No workspace A root-package output or source is copied. Observed
behavior covered by the test:

- `cargo check` in workspace B reuses workspace A's dependency metadata and
  emits no rustc invocation for `gate_one_dep`.
- `cargo build` in workspace B reuses A's linkable dependency artifact set and
  emits no rustc invocation for `gate_one_dep`.
- The manually materialized dependency subtree is accepted in a different
  target directory for both `check` and `build`; the copied bytes remain
  unchanged and B emits no rustc invocation for `gate_one_dep`.
- An empty different target directory is a miss because it has no
  corresponding fingerprint/artifact state.
- A profile change (`debug` to `release`) is a miss.

This is a reuse-across-roots and controlled-materialization proof, not yet a
global-cache implementation. A true CAS must make this materialization atomic,
key it by semantic compilation inputs, validate corruption, and integrate it
with Cargo's scheduler. The pre-CAS test deliberately copies Cargo's own
outputs and fingerprint metadata without rewriting them or adding production
CAS behavior.
