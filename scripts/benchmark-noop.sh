#!/bin/sh
# Compare the ordinary Cargo path with cargo-cas's validated no-op path.
#
# Usage: CARGO_CAS_BIN=... scripts/benchmark-noop.sh /path/to/package
set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: $0 /path/to/package" >&2
    exit 2
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
cargo_bin=${CARGO_CAS_BIN:-"$repo_root/target/release/cargo"}
project=$(CDPATH= cd -- "$1" && pwd)
runs=${CARGO_CAS_NOOP_RUNS:-5}

if [ ! -x "$cargo_bin" ]; then
    echo "cargo-cas binary not found: $cargo_bin" >&2
    echo "build it with 'cargo build --release --bin cargo'" >&2
    exit 1
fi

measure() {
    label=$1
    shift
    printf '\n%s\n' "$label"
    (cd "$project" && /usr/bin/time -p "$@")
}

measure baseline env CARGO_CAS_DISABLE_FAST_NOOP=1 "$cargo_bin" build
measure establish "$cargo_bin" build

index=1
while [ "$index" -le "$runs" ]; do
    measure "validated no-op $index" "$cargo_bin" build
    index=$((index + 1))
done

printf '\nReceipt: %s\n' "$project/target/.cargo-cas/noop-v1.json"
