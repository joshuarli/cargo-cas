#!/bin/sh
# Reproducible macOS benchmark for the conservative cargo-cas V1 experiment.
#
# It compares the system/upstream Cargo binary with a release-built cargo-cas
# binary on the same tiny immutable git dependency, then measures the key
# unrelated-workspace and 2/4/8 concurrent-worktree scenarios. It is kept
# network-free so measurements do not include registry download variance.
set -eu

if [ "$(uname -s)" != "Darwin" ]; then
    echo "cargo-cas is currently benchmarked only on macOS" >&2
    exit 1
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
cas_bin=${CARGO_CAS_BIN:-"$repo_root/target/release/cargo"}
upstream_bin=${CARGO_UPSTREAM_BIN:-"$(command -v cargo)"}
if [ ! -x "$cas_bin" ]; then
    echo "cargo-cas release binary not found: $cas_bin" >&2
    echo "run 'cargo build --release -p cargo' or set CARGO_CAS_BIN" >&2
    exit 1
fi

work_root=$(mktemp -d "${TMPDIR:-/tmp}/cargo-cas-benchmark.XXXXXX")
cleanup() {
    if [ "${CARGO_CAS_BENCHMARK_KEEP:-0}" = 1 ]; then
        echo "cargo-cas benchmark files retained at: $work_root" >&2
    else
        rm -rf "$work_root"
    fi
}
trap cleanup EXIT INT TERM

real_rustc=$(command -v rustc)
rustc_log="$work_root/rustc.log"
rustc_proxy="$work_root/counting-rustc"
cat >"$rustc_proxy" <<'EOF'
#!/bin/sh
previous=''
for argument in "$@"; do
    if [ "$previous" = '--crate-name' ]; then
        printf '%s %s\n' "$CAS_BENCHMARK_LABEL" "$argument" >>"$CAS_BENCHMARK_RUSTC_LOG"
        break
    fi
    previous="$argument"
done
exec "$CAS_BENCHMARK_REAL_RUSTC" "$@"
EOF
chmod +x "$rustc_proxy"
export CAS_BENCHMARK_REAL_RUSTC="$real_rustc"
export CAS_BENCHMARK_RUSTC_LOG="$rustc_log"

dependency="$work_root/dependency"
mkdir -p "$dependency/src"
cat >"$dependency/Cargo.toml" <<'EOF'
[package]
name = "cas-benchmark-dep"
version = "1.0.0"
edition = "2024"

[features]
concurrent-2 = []
concurrent-4 = []
concurrent-8 = []
EOF
cat >"$dependency/src/lib.rs" <<'EOF'
pub fn answer() -> u32 { 42 }
EOF
git -C "$dependency" init -q
git -C "$dependency" config user.email cargo-cas-benchmark@example.invalid
git -C "$dependency" config user.name cargo-cas-benchmark
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
cas-benchmark-dep = { git = "$dependency_url", rev = "$revision"$feature_spec }
EOF
    cat >"$workspace/src/main.rs" <<'EOF'
fn main() { println!("{}", cas_benchmark_dep::answer()); }
EOF
}

count_dependency_rustc() {
    label=$1
    if [ ! -f "$rustc_log" ]; then
        printf '0'
        return
    fi
    grep -c "^$label cas_benchmark_dep$" "$rustc_log" || true
}

count_total_rustc() {
    label=$1
    if [ ! -f "$rustc_log" ]; then
        printf '0'
        return
    fi
    grep -c "^$label " "$rustc_log" || true
}

elapsed_seconds() {
    started=$1
    finished=$(date +%s)
    printf '%s' $((finished - started))
}

directory_bytes() {
    du -sk "$1" | awk '{print $1 * 1024}'
}

run_check() {
    label=$1
    cargo_bin=$2
    cargo_home=$3
    workspace=$4
    target_dir=$5
    mode=$6
    started=$(date +%s)
    if [ "$mode" = cas ]; then
        (
            cd "$workspace"
            CARGO_HOME="$cargo_home" RUSTC="$rustc_proxy" CAS_BENCHMARK_LABEL="$label" \
                "$cargo_bin" check -Zcargo-cas -vv --target-dir "$target_dir"
        ) >"$work_root/$label.log" 2>&1
    else
        (
            cd "$workspace"
            CARGO_HOME="$cargo_home" RUSTC="$rustc_proxy" CAS_BENCHMARK_LABEL="$label" \
                "$cargo_bin" check -vv --target-dir "$target_dir"
        ) >"$work_root/$label.log" 2>&1
    fi
    elapsed_seconds "$started" >"$work_root/$label.seconds"
}

read_metric() {
    label=$1
    kind=$2
    case "$kind" in
        dependency) count_dependency_rustc "$label" ;;
        total) count_total_rustc "$label" ;;
        seconds) cat "$work_root/$label.seconds" ;;
    esac
}

upstream_home="$work_root/upstream-home"
cas_home="$work_root/cas-home"
mkdir -p "$upstream_home" "$cas_home"
make_workspace "$work_root/upstream-a" benchmark-upstream-a ''
make_workspace "$work_root/upstream-b" benchmark-upstream-b ''
make_workspace "$work_root/cas-a" benchmark-cas-a ''
make_workspace "$work_root/cas-b" benchmark-cas-b ''

run_check upstream-cold "$upstream_bin" "$upstream_home" "$work_root/upstream-a" "$work_root/upstream-a-target" upstream
run_check upstream-same-warm "$upstream_bin" "$upstream_home" "$work_root/upstream-a" "$work_root/upstream-a-target" upstream
run_check upstream-unrelated-warm "$upstream_bin" "$upstream_home" "$work_root/upstream-b" "$work_root/upstream-b-target" upstream
run_check cas-cold "$cas_bin" "$cas_home" "$work_root/cas-a" "$work_root/cas-a-target" cas
run_check cas-same-warm "$cas_bin" "$cas_home" "$work_root/cas-a" "$work_root/cas-a-target" cas
run_check cas-unrelated-warm "$cas_bin" "$cas_home" "$work_root/cas-b" "$work_root/cas-b-target" cas

run_concurrent() {
    count=$1
    mode=$2
    if [ "$mode" = cas ]; then
        cargo_bin=$cas_bin
        cargo_home=$cas_home
        label="cas-concurrent-$count"
    else
        cargo_bin=$upstream_bin
        cargo_home=$upstream_home
        label="upstream-concurrent-$count"
    fi
    agent_repo="$work_root/$label-repository"
    make_workspace "$agent_repo" "$label-agent" ", features = [\"concurrent-$count\"]"
    git -C "$agent_repo" init -q
    git -C "$agent_repo" config user.email cargo-cas-benchmark@example.invalid
    git -C "$agent_repo" config user.name cargo-cas-benchmark
    git -C "$agent_repo" add Cargo.toml src/main.rs
    git -C "$agent_repo" commit -qm initial

    pids=''
    started=$(date +%s)
    index=1
    while [ "$index" -le "$count" ]; do
        worktree="$work_root/$label-worktree-$index"
        git -C "$agent_repo" worktree add -q "$worktree"
        printf 'fn main() { println!("worktree %s: {}", cas_benchmark_dep::answer()); }\n' "$index" >"$worktree/src/main.rs"
        (
            cd "$worktree"
            if [ "$mode" = cas ]; then
                CARGO_HOME="$cargo_home" RUSTC="$rustc_proxy" CAS_BENCHMARK_LABEL="$label" \
                    "$cargo_bin" check -Zcargo-cas -vv --target-dir "$work_root/$label-target-$index"
            else
                CARGO_HOME="$cargo_home" RUSTC="$rustc_proxy" CAS_BENCHMARK_LABEL="$label" \
                    "$cargo_bin" check -vv --target-dir "$work_root/$label-target-$index"
            fi
        ) >"$work_root/$label-$index.log" 2>&1 &
        pids="$pids $!"
        index=$((index + 1))
    done
    for pid in $pids; do
        wait "$pid"
    done
    elapsed_seconds "$started" >"$work_root/$label.seconds"
}

run_concurrent 2 upstream
run_concurrent 4 upstream
run_concurrent 8 upstream
run_concurrent 2 cas
run_concurrent 4 cas
run_concurrent 8 cas

cache_root="$cas_home/cache/cargo-cas-v1"
cache_bytes=$(directory_bytes "$cache_root")
upstream_workspace_bytes=0
for target_dir in "$work_root"/upstream-*-target "$work_root"/upstream-concurrent-*-target-*; do
    [ -d "$target_dir" ] || continue
    upstream_workspace_bytes=$((upstream_workspace_bytes + $(directory_bytes "$target_dir")))
done
cas_workspace_bytes=0
for target_dir in "$work_root"/cas-*-target "$work_root"/cas-concurrent-*-target-*; do
    [ -d "$target_dir" ] || continue
    cas_workspace_bytes=$((cas_workspace_bytes + $(directory_bytes "$target_dir")))
done

cat <<EOF
cargo-cas benchmark complete
binary modes: upstream=$upstream_bin cargo-cas=$cas_bin

scenario                    dependency-rustc total-rustc wall-seconds
upstream cold               $(read_metric upstream-cold dependency) $(read_metric upstream-cold total) $(read_metric upstream-cold seconds)
upstream same-workspace     $(read_metric upstream-same-warm dependency) $(read_metric upstream-same-warm total) $(read_metric upstream-same-warm seconds)
upstream unrelated          $(read_metric upstream-unrelated-warm dependency) $(read_metric upstream-unrelated-warm total) $(read_metric upstream-unrelated-warm seconds)
upstream concurrent 2       $(read_metric upstream-concurrent-2 dependency) $(read_metric upstream-concurrent-2 total) $(read_metric upstream-concurrent-2 seconds)
upstream concurrent 4       $(read_metric upstream-concurrent-4 dependency) $(read_metric upstream-concurrent-4 total) $(read_metric upstream-concurrent-4 seconds)
upstream concurrent 8       $(read_metric upstream-concurrent-8 dependency) $(read_metric upstream-concurrent-8 total) $(read_metric upstream-concurrent-8 seconds)
cargo-cas cold              $(read_metric cas-cold dependency) $(read_metric cas-cold total) $(read_metric cas-cold seconds)
cargo-cas same-workspace    $(read_metric cas-same-warm dependency) $(read_metric cas-same-warm total) $(read_metric cas-same-warm seconds)
cargo-cas unrelated         $(read_metric cas-unrelated-warm dependency) $(read_metric cas-unrelated-warm total) $(read_metric cas-unrelated-warm seconds)
cargo-cas concurrent 2      $(read_metric cas-concurrent-2 dependency) $(read_metric cas-concurrent-2 total) $(read_metric cas-concurrent-2 seconds)
cargo-cas concurrent 4      $(read_metric cas-concurrent-4 dependency) $(read_metric cas-concurrent-4 total) $(read_metric cas-concurrent-4 seconds)
cargo-cas concurrent 8      $(read_metric cas-concurrent-8 dependency) $(read_metric cas-concurrent-8 total) $(read_metric cas-concurrent-8 seconds)

cache bytes:                $cache_bytes
upstream workspace bytes:   $upstream_workspace_bytes
cargo-cas workspace bytes:  $cas_workspace_bytes
EOF
