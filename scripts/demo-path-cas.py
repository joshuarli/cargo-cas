#!/usr/bin/env python3
"""Compare a local epsh -> ish build with regular Cargo and cargo-cas.

The demo uses a private Cargo home and target directory for each sequence, but
shares the host registry source tree read-only through a symlink.  The ish
manifest is patched through Cargo configuration to use the local epsh checkout;
the lockfile override keeps Cargo from modifying the user's checkout.

Temporary benchmark state is removed on every exit by default.  Set KEEP=1 (or
CARGO_CAS_DEMO_KEEP=1) to retain it for inspection.  Set TRACE=1 (or
CARGO_CAS_DEMO_TRACE=1) to emit cargo-cas hit/skip summaries; tracing adds
logging overhead to the reported CAS timings.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile
import time


REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_TOOLCHAIN = "nightly-2026-07-24"


class DemoError(RuntimeError):
    """A setup or build failure with a user-facing message."""


def env_path(name: str, default: Path) -> Path:
    value = os.environ.get(name)
    return Path(value).expanduser().resolve() if value else default.expanduser().resolve()


def resolve_tool(toolchain: str, name: str) -> Path:
    rustup = shutil.which("rustup")
    if rustup is None:
        raise DemoError("rustup is required to select the pinned demo toolchain")
    result = subprocess.run(
        [rustup, "which", "--toolchain", toolchain, name],
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode:
        detail = result.stderr.strip() or result.stdout.strip()
        raise DemoError(f"cannot resolve {name} for {toolchain}: {detail}")
    return Path(result.stdout.strip()).resolve()


def regular_files(root: Path):
    """Yield regular files without following symlinks into the host cache."""

    if not root.exists():
        return
    for path in root.rglob("*"):
        try:
            stat = path.lstat()
        except FileNotFoundError:
            continue
        if not path.is_symlink() and path.is_file():
            yield path, stat


def allocated_bytes(root: Path) -> int:
    """Return apparent allocated bytes for one directory tree."""

    return sum(getattr(stat, "st_blocks", 0) * 512 or stat.st_size for _, stat in regular_files(root))


def logical_bytes(root: Path) -> int:
    """Return the sum of regular-file lengths for one directory tree."""

    return sum(stat.st_size for _, stat in regular_files(root))


def unique_allocated_bytes(roots: list[Path]) -> int:
    """Return filesystem blocks allocated by a set of trees.

    A cache-backed target can contain hardlinks to immutable cache artifacts.
    Count an inode once across the complete storage set: this is direct
    filesystem allocation, unlike the separate CoW estimate used by the
    four-worktree harness.
    """

    seen: set[tuple[int, int]] = set()
    total = 0
    for root in roots:
        for _, stat in regular_files(root):
            inode = (stat.st_dev, stat.st_ino)
            if inode in seen:
                continue
            seen.add(inode)
            total += getattr(stat, "st_blocks", 0) * 512 or stat.st_size
    return total


def format_bytes(value: int) -> str:
    return f"{value / (1024 * 1024):.2f} MiB"


def cache_manifests(cache_root: Path) -> dict[str, dict[str, object]]:
    entries: dict[str, dict[str, object]] = {}
    if not cache_root.is_dir():
        return entries
    for manifest in cache_root.glob("*/manifest.json"):
        try:
            data = json.loads(manifest.read_text())
        except (OSError, json.JSONDecodeError):
            continue
        action_key = data.get("action_key")
        if isinstance(action_key, str):
            entries[action_key] = data
    return entries


def package_names(entries: dict[str, dict[str, object]]) -> list[str]:
    names: list[str] = []
    for data in entries.values():
        identity = data.get("identity")
        if isinstance(identity, dict):
            package_id = identity.get("package_id")
            if isinstance(package_id, str):
                names.append(package_id)
    return sorted(names)


def run_build(
    *,
    label: str,
    command: list[str],
    cwd: Path,
    env: dict[str, str],
    log_path: Path,
    target_dir: Path,
) -> dict[str, object]:
    started = time.monotonic()
    with log_path.open("w") as log:
        result = subprocess.run(
            command,
            cwd=cwd,
            env=env,
            stdout=log,
            stderr=subprocess.STDOUT,
            check=False,
        )
    elapsed = time.monotonic() - started
    if result.returncode:
        output = log_path.read_text(errors="replace")
        tail = "\n".join(output.splitlines()[-40:])
        raise DemoError(f"{label} failed with status {result.returncode}:\n{tail}")
    return {
        "label": label,
        "seconds": elapsed,
        "target_allocated_bytes": allocated_bytes(target_dir),
        "target_logical_bytes": logical_bytes(target_dir),
        "log": log_path,
    }


def make_env(cargo_home: Path, rustc: Path, rustdoc: Path, *, trace: bool) -> dict[str, str]:
    env = os.environ.copy()
    if trace:
        env["CARGO_LOG"] = "cargo::compiler::cas=debug"
    else:
        env.pop("CARGO_LOG", None)
        env.pop("RUST_LOG", None)
    env.update(
        {
            "CARGO_HOME": str(cargo_home),
            "CARGO_NET_OFFLINE": "true",
            "CARGO_TERM_COLOR": "never",
            "RUSTC": str(rustc),
            "RUSTDOC": str(rustdoc),
        }
    )
    return env


def profile_args() -> list[str]:
    # Match ish's profile.dev settings so dependency action keys can actually
    # be shared while the path/build-script eligibility work is developed.
    return [
        "--config",
        'profile.dev.package."*".opt-level=2',
        "--config",
        'profile.dev.debug="line-tables-only"',
        "--config",
        'profile.dev.split-debuginfo="unpacked"',
    ]


def prepare_home(root: Path, global_registry: Path) -> Path:
    home = root / "cargo-home"
    home.mkdir()
    if not global_registry.is_dir():
        raise DemoError(f"registry source cache not found: {global_registry}")
    (home / "registry").symlink_to(global_registry, target_is_directory=True)
    return home


def run_sequence(
    *,
    name: str,
    root: Path,
    cargo: Path,
    epsh: Path,
    ish: Path,
    rustc: Path,
    rustdoc: Path,
    global_registry: Path,
    trace: bool,
) -> tuple[
    list[dict[str, object]],
    dict[str, dict[str, object]],
    dict[str, dict[str, object]],
    dict[str, int],
]:
    root.mkdir(parents=True)
    home = prepare_home(root, global_registry)
    env = make_env(home, rustc, rustdoc, trace=trace)
    logs = root / "logs"
    logs.mkdir()
    epsh_target = root / "epsh-target"
    ish_target = root / "ish-target"
    cache_root = home / "cache" / "cargo-cas-v1"

    epsh_command = [str(cargo), "build"]
    epsh_command.extend(profile_args())
    epsh_command.extend(
        [
            "--locked",
            "--manifest-path",
            str(epsh / "Cargo.toml"),
            "--target-dir",
            str(epsh_target),
        ]
    )

    epsh_result = run_build(
        label=f"{name} epsh",
        command=epsh_command,
        cwd=epsh,
        env=env,
        log_path=logs / "epsh.log",
        target_dir=epsh_target,
    )
    after_epsh = cache_manifests(cache_root)

    # Cargo's patch and lockfile overrides are scoped to this temporary path;
    # the real ish checkout is never rewritten by the demo.
    lock_dir = root / "lock"
    lock_dir.mkdir()
    lock_path = lock_dir / "Cargo.lock"
    shutil.copy2(ish / "Cargo.lock", lock_path)
    ish_command = [str(cargo), "build"]
    ish_command.extend(
        [
            "--manifest-path",
            str(ish / "Cargo.toml"),
            "--config",
            f"patch.crates-io.epsh.path={json.dumps(str(epsh))}",
            "--config",
            f"resolver.lockfile-path={json.dumps(str(lock_path))}",
            "--target-dir",
            str(ish_target),
        ]
    )
    ish_result = run_build(
        label=f"{name} ish",
        command=ish_command,
        cwd=ish,
        env=env,
        log_path=logs / "ish.log",
        target_dir=ish_target,
    )
    fresh_storage = {
        "epsh_target_logical": logical_bytes(epsh_target),
        "epsh_target_allocated": allocated_bytes(epsh_target),
        "ish_target_logical": logical_bytes(ish_target),
        "ish_target_allocated": allocated_bytes(ish_target),
        "cache_logical": logical_bytes(cache_root),
        "cache_allocated": allocated_bytes(cache_root),
        "total_allocated": unique_allocated_bytes([epsh_target, ish_target, cache_root]),
    }
    # This is a warm cache recovery, not a no-op: the Cargo target directory
    # is gone, so ordinary Cargo recompiles dependencies while cargo-cas must
    # restore its validated global artifacts into an otherwise empty target.
    shutil.rmtree(ish_target)
    warm_ish_result = run_build(
        label=f"{name} ish target-recovery",
        command=ish_command,
        cwd=ish,
        env=env,
        log_path=logs / "ish-target-recovery.log",
        target_dir=ish_target,
    )
    after_ish = cache_manifests(cache_root)
    return [epsh_result, ish_result, warm_ish_result], after_epsh, after_ish, fresh_storage


def print_result(result: dict[str, object]) -> None:
    print(
        f"  {result['label']}: {result['seconds']:.2f}s, "
        f"target logical {format_bytes(int(result['target_logical_bytes']))}, "
        f"allocated {format_bytes(int(result['target_allocated_bytes']))}"
    )


def main() -> int:
    if platform.system() != "Darwin":
        raise DemoError("cargo-cas path demo currently requires macOS")

    epsh = env_path("EPSH_DIR", Path.home() / "d" / "epsh")
    ish = env_path("ISH_DIR", Path.home() / "d" / "ish")
    cargo_cas = env_path("CARGO_CAS_BIN", REPO_ROOT / "target" / "release" / "cargo")
    global_registry = env_path("CARGO_CAS_REGISTRY", Path.home() / ".cargo" / "registry")
    toolchain = os.environ.get("CARGO_CAS_DEMO_TOOLCHAIN", DEFAULT_TOOLCHAIN)
    regular_cargo = resolve_tool(toolchain, "cargo")
    rustc = resolve_tool(toolchain, "rustc")
    rustdoc = resolve_tool(toolchain, "rustdoc")

    for label, path in (("epsh", epsh), ("ish", ish), ("cargo-cas", cargo_cas)):
        if not path.exists():
            raise DemoError(f"{label} not found: {path}")
    if cargo_cas.parent.name != "release":
        raise DemoError(f"cargo-cas path demo requires a release binary, got: {cargo_cas}")
    if not global_registry.is_dir():
        raise DemoError(f"registry source cache not found: {global_registry}")

    keep = os.environ.get("KEEP") == "1" or os.environ.get("CARGO_CAS_DEMO_KEEP") == "1"
    trace = os.environ.get("TRACE") == "1" or os.environ.get("CARGO_CAS_DEMO_TRACE") == "1"
    temp_root = Path(tempfile.mkdtemp(prefix="cargo-cas-path-demo."))
    print("cargo-cas local-path demo")
    print(f"  epsh: {epsh}")
    print(f"  ish: {ish} (patched to local epsh for this run)")
    print(f"  toolchain: {toolchain}")
    if trace:
        print("  CAS tracing: enabled (timings include debug logging)")
    print("  temporary state: " + (str(temp_root) if keep else "cleaned on exit"))

    try:
        regular, _, _, regular_storage = run_sequence(
            name="regular",
            root=temp_root / "regular",
            cargo=regular_cargo,
            epsh=epsh,
            ish=ish,
            rustc=rustc,
            rustdoc=rustdoc,
            global_registry=global_registry,
            trace=trace,
        )
        cas, before_ish, after_ish, cas_storage = run_sequence(
            name="cargo-cas",
            root=temp_root / "cargo-cas",
            cargo=cargo_cas,
            epsh=epsh,
            ish=ish,
            rustc=rustc,
            rustdoc=rustdoc,
            global_registry=global_registry,
            trace=trace,
        )

        print("\nregular Cargo")
        for result in regular:
            print_result(result)
        print("\ncargo-cas")
        for result in cas:
            print_result(result)

        shared = sorted(set(before_ish) & set(after_ish))
        new_entries = sorted(set(after_ish) - set(before_ish))
        print(
            f"  cache entries after epsh: {len(before_ish)} "
            f"({', '.join(package_names(before_ish)) or 'none'})"
        )
        print(
            f"  ish cache candidates from epsh: {len(shared)} "
            f"({', '.join(package_names({key: before_ish[key] for key in shared})) or 'none'})"
        )
        print(
            f"  ish cache entries published: {len(new_entries)} "
            f"({', '.join(package_names({key: after_ish[key] for key in new_entries})) or 'none'})"
        )
        if trace:
            print("  CAS summaries:")
            for result in cas:
                summaries = [
                    line
                    for line in Path(result["log"]).read_text(errors="replace").splitlines()
                    if "cargo-cas summary" in line
                ]
                for summary in summaries:
                    print(f"    {summary}")
        vanilla_logical = (
            regular_storage["epsh_target_logical"] + regular_storage["ish_target_logical"]
        )
        vanilla_allocated = regular_storage["total_allocated"]
        cas_target_logical = (
            cas_storage["epsh_target_logical"] + cas_storage["ish_target_logical"]
        )
        cas_target_allocated = (
            cas_storage["epsh_target_allocated"] + cas_storage["ish_target_allocated"]
        )
        cache_logical = cas_storage["cache_logical"]
        cache_allocated = cas_storage["cache_allocated"]
        cas_logical = cas_target_logical + cache_logical
        cas_allocated = cas_storage["total_allocated"]

        print("\nstorage (fresh epsh then ish, before target recovery)")
        print(
            f"  vanilla targets: logical {format_bytes(vanilla_logical)}, "
            f"allocated {format_bytes(vanilla_allocated)}"
        )
        print(
            f"  cargo-cas targets: logical {format_bytes(cas_target_logical)}, "
            f"apparent allocated {format_bytes(cas_target_allocated)}"
        )
        print(
            f"  cargo-cas cache: logical {format_bytes(cache_logical)}, "
            f"allocated {format_bytes(cache_allocated)}"
        )
        print(
            f"  cargo-cas total: logical {format_bytes(cas_logical)}, "
            f"allocated {format_bytes(cas_allocated)} "
            f"({cas_allocated / vanilla_allocated:.3f}x vanilla)"
        )

        regular_ish = float(regular[1]["seconds"])
        cas_ish = float(cas[1]["seconds"])
        print(f"\nish delta: {cas_ish - regular_ish:+.2f}s ({(cas_ish / regular_ish - 1) * 100:+.1f}%)")
        regular_total = sum(float(result["seconds"]) for result in regular[:2])
        cas_total = sum(float(result["seconds"]) for result in cas[:2])
        print(f"cold sequence delta: {cas_total - regular_total:+.2f}s")
        regular_warm = float(regular[2]["seconds"])
        cas_warm = float(cas[2]["seconds"])
        print(
            f"warm target-recovery delta: {cas_warm - regular_warm:+.2f}s "
            f"({(cas_warm / regular_warm - 1) * 100:+.1f}%)"
        )
    finally:
        if keep:
            print(f"\nkept demo state: {temp_root}")
        else:
            shutil.rmtree(temp_root, ignore_errors=True)
            print("\ntemporary demo state removed")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (DemoError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1)
