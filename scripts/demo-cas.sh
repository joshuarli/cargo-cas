#!/bin/sh
# Demonstrates the experimental macOS-only cargo-cas workflow without network
# access. Build this checkout first (`cargo build`) or set CARGO_CAS_BIN to the
# Cargo binary that contains the `-Zcargo-cas` experiment.
set -eu

if [ "$(uname -s)" != "Darwin" ]; then
    echo "cargo-cas is currently supported only on macOS" >&2
    exit 1
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
cargo_bin=${CARGO_CAS_BIN:-"$repo_root/target/debug/cargo"}
if [ ! -x "$cargo_bin" ]; then
    echo "cargo-cas binary not found: $cargo_bin" >&2
    echo "build this checkout with 'cargo build' or set CARGO_CAS_BIN" >&2
    exit 1
fi

work_root=$(mktemp -d "${TMPDIR:-/tmp}/cargo-cas-demo.XXXXXX")
cas_home="$work_root/cargo-home"
rustc_log="$work_root/rustc.log"
cleanup() {
    if [ "${CARGO_CAS_DEMO_KEEP:-0}" = 1 ]; then
        echo "cargo-cas demo files retained at: $work_root" >&2
    else
        rm -rf "$work_root"
    fi
}
trap cleanup EXIT INT TERM
mkdir -p "$cas_home"

real_rustc=$(command -v rustc)
export CARGO_HOME="$cas_home"
export CAS_DEMO_REAL_RUSTC="$real_rustc"
export CAS_DEMO_RUSTC_LOG="$rustc_log"
export CARGO_LOG="cargo::compiler::cas=debug"

rustc_proxy="$work_root/counting-rustc"
cat >"$rustc_proxy" <<'EOF'
#!/bin/sh
previous=''
for argument in "$@"; do
    if [ "$previous" = '--crate-name' ] && [ "$argument" = 'cas_demo_dep' ]; then
        printf '%s\n' "$argument" >>"$CAS_DEMO_RUSTC_LOG"
        break
    fi
    previous="$argument"
done
exec "$CAS_DEMO_REAL_RUSTC" "$@"
EOF
chmod +x "$rustc_proxy"

dependency="$work_root/cas-demo-dependency"
mkdir -p "$dependency/src"
cat >"$dependency/Cargo.toml" <<'EOF'
[package]
name = "cas-demo-dep"
version = "1.0.0"
edition = "2024"

[features]
alternate = []
concurrent = []
EOF
cat >"$dependency/src/lib.rs" <<'EOF'
#[cfg(feature = "alternate")]
pub fn answer() -> u32 { 42 }

#[cfg(not(feature = "alternate"))]
pub fn answer() -> u32 { 41 }
EOF
git -C "$dependency" init -q
git -C "$dependency" config user.email cargo-cas-demo@example.invalid
git -C "$dependency" config user.name cargo-cas-demo
git -C "$dependency" add Cargo.toml src/lib.rs
git -C "$dependency" commit -qm initial
revision=$(git -C "$dependency" rev-parse HEAD)
dependency_url="file://$dependency"

make_workspace() {
    workspace=$1
    package_name=$2
    feature_spec=$3
    mkdir -p "$workspace/src"
    cat >"$workspace/Cargo.toml" <<EOF
[package]
name = "$package_name"
version = "0.1.0"
edition = "2024"

[dependencies]
cas-demo-dep = { git = "$dependency_url", rev = "$revision"$feature_spec }
EOF
    cat >"$workspace/src/main.rs" <<'EOF'
fn main() { println!("{}", cas_demo_dep::answer()); }
EOF
}

run_check() {
    workspace=$1
    target_dir=$2
    log=$3
    (
        cd "$workspace"
        RUSTC="$rustc_proxy" "$cargo_bin" check -Zcargo-cas -vv --target-dir "$target_dir"
    ) >"$log" 2>&1
}

count_dependency_rustc() {
    if [ ! -f "$rustc_log" ]; then
        printf '0'
        return
    fi
    grep -c '^cas_demo_dep$' "$rustc_log" || true
}

assert_equal() {
    expected=$1
    actual=$2
    description=$3
    if [ "$expected" != "$actual" ]; then
        echo "demo assertion failed: $description (expected $expected, got $actual)" >&2
        exit 1
    fi
}

workspace_a="$work_root/workspace-a"
workspace_b="$work_root/workspace-b"
workspace_feature="$work_root/workspace-feature"
workspace_restore="$work_root/workspace-restore"
make_workspace "$workspace_a" cas-demo-a ''
make_workspace "$workspace_b" cas-demo-b ''
make_workspace "$workspace_feature" cas-demo-feature ', features = ["alternate"]'
make_workspace "$workspace_restore" cas-demo-restore ''

run_check "$workspace_a" "$work_root/target-a" "$work_root/a.log"
cold_rustc=$(count_dependency_rustc)
assert_equal 1 "$cold_rustc" 'cold workspace compiles the shared dependency once'

run_check "$workspace_b" "$work_root/target-b" "$work_root/b.log"
warm_rustc=$(count_dependency_rustc)
assert_equal "$cold_rustc" "$warm_rustc" 'unrelated workspace reuses the base action'
second_workspace_rustc=$((warm_rustc - cold_rustc))

run_check "$workspace_feature" "$work_root/target-feature" "$work_root/feature.log"
feature_rustc=$(count_dependency_rustc)
assert_equal 2 "$feature_rustc" 'feature change creates a distinct action'

run_check "$workspace_restore" "$work_root/target-restore" "$work_root/restore.log"
restore_rustc=$(count_dependency_rustc)
assert_equal "$feature_rustc" "$restore_rustc" 'restored feature selection reuses the prior action'

agent_repo="$work_root/agent-repo"
make_workspace "$agent_repo" cas-demo-agent ', features = ["concurrent"]'
git -C "$agent_repo" init -q
git -C "$agent_repo" config user.email cargo-cas-demo@example.invalid
git -C "$agent_repo" config user.name cargo-cas-demo
git -C "$agent_repo" add Cargo.toml src/main.rs
git -C "$agent_repo" commit -qm initial

pids=''
for index in 1 2 3 4 5 6 7 8; do
    worktree="$work_root/worktree-$index"
    git -C "$agent_repo" worktree add -q "$worktree"
    printf 'fn main() { println!("agent %s: {}", cas_demo_dep::answer()); }\n' "$index" >"$worktree/src/main.rs"
    (
        cd "$worktree"
        RUSTC="$rustc_proxy" "$cargo_bin" check -Zcargo-cas -vv \
            --target-dir "$work_root/worktree-target-$index"
    ) >"$work_root/worktree-$index.log" 2>&1 &
    pids="$pids $!"
done
for pid in $pids; do
    wait "$pid"
done

worktree_rustc=$(count_dependency_rustc)
assert_equal 3 "$worktree_rustc" 'eight concurrent worktrees compile one new action once'
avoided_invocations=$((1 + 1 + 7))

cache_root="$CARGO_HOME/cache/cargo-cas-v1"
cache_entries=$(find "$cache_root" -mindepth 1 -maxdepth 1 -type d -print \
    | awk -F/ '$NF ~ /^[0-9a-f]{64}$/ { count += 1 } END { print count + 0 }')
cache_bytes=$(du -sk "$cache_root" | awk '{print $1 * 1024}')
cache_hits=$(grep -h 'cargo-cas hit' "$work_root"/*.log 2>/dev/null | wc -l | tr -d ' ')
cache_misses=$(grep -h 'cargo-cas miss' "$work_root"/*.log 2>/dev/null | wc -l | tr -d ' ')

cat <<EOF
cargo-cas demo complete
  cold rustc invocations:       $cold_rustc
  second-workspace invocations: $second_workspace_rustc
  avoided invocations:          $avoided_invocations
  cache hits:                   $cache_hits
  cache misses:                 $cache_misses
  cache entries:                $cache_entries
  cache bytes:                  $cache_bytes
  concurrent worktrees:         8
  shared concurrent rustc:      $((worktree_rustc - feature_rustc))
EOF
