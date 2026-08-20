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


def allocated_bytes(root: Path) -> int:
    """Return physical usage without following symlinks into the host cache."""

    total = 0
    if not root.exists():
        return 0
    for path in root.rglob("*"):
        try:
            stat = path.lstat()
        except FileNotFoundError:
            continue
        if not path.is_symlink() and path.is_file():
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
        "target_bytes": allocated_bytes(target_dir),
        "log": log_path,
    }


def make_env(cargo_home: Path, rustc: Path, rustdoc: Path, *, cargo_cas: bool, trace: bool) -> dict[str, str]:
    env = os.environ.copy()
    if trace and cargo_cas:
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
    cargo_cas: bool,
    epsh: Path,
    ish: Path,
    rustc: Path,
    rustdoc: Path,
    global_registry: Path,
    trace: bool,
) -> tuple[list[dict[str, object]], dict[str, dict[str, object]], dict[str, dict[str, object]]]:
    root.mkdir(parents=True)
    home = prepare_home(root, global_registry)
    env = make_env(home, rustc, rustdoc, cargo_cas=cargo_cas, trace=trace)
    logs = root / "logs"
    logs.mkdir()
    epsh_target = root / "epsh-target"
    ish_target = root / "ish-target"
    cache_root = home / "cache" / "cargo-cas-v1"

    epsh_command = [str(cargo), "build"]
    if cargo_cas:
        epsh_command.append("-Zcargo-cas")
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
    if cargo_cas:
        ish_command.append("-Zcargo-cas")
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
    after_ish = cache_manifests(cache_root)
    return [epsh_result, ish_result], after_epsh, after_ish


def print_result(result: dict[str, object]) -> None:
    print(
        f"  {result['label']}: {result['seconds']:.2f}s, "
        f"target {format_bytes(int(result['target_bytes']))}"
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
        regular, _, _ = run_sequence(
            name="regular",
            root=temp_root / "regular",
            cargo=regular_cargo,
            cargo_cas=False,
            epsh=epsh,
            ish=ish,
            rustc=rustc,
            rustdoc=rustdoc,
            global_registry=global_registry,
            trace=trace,
        )
        cas, before_ish, after_ish = run_sequence(
            name="cargo-cas",
            root=temp_root / "cargo-cas",
            cargo=cargo_cas,
            cargo_cas=True,
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
        print(f"  final CAS cache: {format_bytes(allocated_bytes(temp_root / 'cargo-cas' / 'cargo-home' / 'cache' / 'cargo-cas-v1'))}")

        regular_ish = float(regular[1]["seconds"])
        cas_ish = float(cas[1]["seconds"])
        print(f"\nish delta: {cas_ish - regular_ish:+.2f}s ({(cas_ish / regular_ish - 1) * 100:+.1f}%)")
        regular_total = sum(float(result["seconds"]) for result in regular)
        cas_total = sum(float(result["seconds"]) for result in cas)
        print(f"sequence delta: {cas_total - regular_total:+.2f}s")
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
