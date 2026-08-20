#!/usr/bin/env python3
"""Measure cargo-cas storage and parallel rebuilds for four workspace worktrees.

The benchmark uses the current ``~/d/h12tiny`` revision, a private Cargo home
whose registry is read-only through a symlink, and one target directory inside
each temporary Git worktree.  It never invokes upstream Cargo: every build is
run with ``cargo test -Zcargo-cas --workspace --all-targets --all-features
--no-run --profile dev``.  This compiles every workspace package, test,
example, and dev dependency without executing tests.  A single seed build
populates the shared immutable cache; the four worktrees then restore from that
cache in parallel before the workspace library is edited and rebuilt in
parallel.

Set ``CARGO_CAS_KEEP=1`` (or the legacy ``CARGO_CAS_ISH_KEEP=1``/``KEEP=1``) to retain the temporary worktrees,
logs, targets, and cache for inspection.  Set ``CARGO_CAS_ISH_TRACE=0`` to
remove CAS debug logging from the build environment; tracing is enabled by
default because it makes hit/miss/skip counts auditable.  The default toolchain
is pinned to the revision used by the workspace's ``rust-toolchain.toml``.  Rebuild
timings use three paired source-edit rounds by default; override that count
with ``CARGO_CAS_REBUILD_ROUNDS``.  Completed runs append one JSON record to
``benchmarks/cargo-cas-workspace-history.jsonl`` (override with
``CARGO_CAS_RESULTS``), preserving prior project baselines.
"""

from __future__ import annotations

import ast
from collections import Counter
from dataclasses import dataclass
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
from statistics import median
import subprocess
import sys
import tempfile
import time
from typing import Callable


REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_TOOLCHAIN = "nightly-2026-07-24"
DEFAULT_PROJECT_DIR = Path.home() / "d" / "h12tiny"
DEFAULT_EDIT_FILE = Path("src/lib.rs")
DEFAULT_RESULTS_PATH = REPO_ROOT / "benchmarks" / "cargo-cas-workspace-history.jsonl"
WORKTREE_COUNT = 4
MAX_MEASURED_FOOTPRINT_MULTIPLIER = 1.10
MAX_REBUILD_MULTIPLIER = 1.05
DEFAULT_REBUILD_ROUNDS = 3


class BenchmarkError(RuntimeError):
    """A setup or build failure with a user-facing message."""


@dataclass
class BuildResult:
    """Timing and storage observed for one worktree process."""

    index: int
    process_pid: int
    seconds: float
    target_bytes: int
    log_path: Path


@dataclass
class RebuildSeries:
    """Repeated source-edit rebuilds used to make the timing ratio stable."""

    final_results: list[BuildResult]
    wall_seconds: list[float]
    slowest_seconds: list[float]
    counts: Counter[str]
    summary: Counter[str]


def env_path(name: str, default: Path) -> Path:
    value = os.environ.get(name)
    return Path(value).expanduser().resolve() if value else default.expanduser().resolve()


def resolve_tool(toolchain: str, name: str) -> Path:
    rustup = shutil.which("rustup")
    if rustup is None:
        raise BenchmarkError("rustup is required to select the pinned workspace toolchain")
    result = subprocess.run(
        [rustup, "which", "--toolchain", toolchain, name],
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode:
        detail = result.stderr.strip() or result.stdout.strip()
        raise BenchmarkError(f"cannot resolve {name} for {toolchain}: {detail}")
    return Path(result.stdout.strip()).resolve()


def run_git(project: Path, *arguments: str) -> str:
    result = subprocess.run(
        ["git", "-C", str(project), *arguments],
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode:
        detail = result.stderr.strip() or result.stdout.strip()
        raise BenchmarkError(f"git {' '.join(arguments)} failed: {detail}")
    return result.stdout.strip()


def cargo_metadata(cargo: Path, project: Path, env: dict[str, str]) -> dict:
    """Return locked workspace metadata using the release cargo-cas binary."""
    result = subprocess.run(
        [
            str(cargo),
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--manifest-path",
            str(project / "Cargo.toml"),
        ],
        cwd=project,
        env=env,
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode:
        detail = result.stderr.strip() or result.stdout.strip()
        raise BenchmarkError(f"cargo metadata failed: {detail}")
    try:
        metadata = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise BenchmarkError(f"cargo metadata returned invalid JSON: {error}") from error
    return metadata


def workspace_target_names(metadata: dict) -> dict[str, set[str]]:
    """Return buildable crate names grouped by workspace package."""

    workspace_members = set(metadata.get("workspace_members", []))
    packages: dict[str, set[str]] = {}
    for package in metadata.get("packages", []):
        if workspace_members and package.get("id") not in workspace_members:
            continue
        package_name = package.get("name")
        if not isinstance(package_name, str):
            continue
        targets = {
            target["name"].replace("-", "_")
            for target in package.get("targets", [])
            if isinstance(target, dict)
            and isinstance(target.get("name"), str)
            and set(target.get("kind", []))
            & {"lib", "bin", "cdylib", "dylib", "staticlib", "proc-macro"}
        }
        if targets:
            packages[package_name] = targets
    if not packages:
        raise BenchmarkError("cargo metadata found no buildable workspace packages")
    return packages


def workspace_dev_dependencies(metadata: dict) -> set[str]:
    """Return all dev-dependency names requested by workspace packages."""

    workspace_members = set(metadata.get("workspace_members", []))
    return {
        dependency["name"]
        for package in metadata.get("packages", [])
        if not workspace_members or package.get("id") in workspace_members
        for dependency in package.get("dependencies", [])
        if dependency.get("kind") == "dev" and isinstance(dependency.get("name"), str)
    }


def external_path_dependencies(
    metadata: dict, project: Path, temp_root: Path
) -> dict[str, Path]:
    """Mirror non-workspace path dependencies into the relocated worktrees."""

    project_root = project.resolve()
    parent = project_root.parent
    paths: dict[str, Path] = {}
    declared_paths: dict[str, Path] = {}
    for package in metadata.get("packages", []):
        for dependency in package.get("dependencies", []):
            raw_path = dependency.get("path")
            name = dependency.get("name")
            if not isinstance(raw_path, str) or not isinstance(name, str):
                continue
            dependency_path = Path(raw_path).resolve()
            try:
                dependency_path.relative_to(project_root)
            except ValueError:
                pass
            else:
                continue
            if name in declared_paths:
                if declared_paths[name] != dependency_path:
                    raise BenchmarkError(
                        f"workspace dependency {name} resolves to multiple paths: "
                        f"{declared_paths[name]} and {dependency_path}"
                    )
                continue
            declared_paths[name] = dependency_path
            try:
                relative = dependency_path.relative_to(parent)
            except ValueError as error:
                raise BenchmarkError(
                    f"external path dependency is outside the workspace parent: {dependency_path}"
                ) from error
            configured = env_path(
                f"{re.sub(r'[^A-Za-z0-9]', '_', name).upper()}_DIR",
                dependency_path,
            )
            destination = temp_root / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            if destination.exists() or destination.is_symlink():
                raise BenchmarkError(f"temporary dependency path already exists: {destination}")
            destination.symlink_to(configured, target_is_directory=True)
            paths[name] = configured
    return paths


def allocated_bytes(root: Path) -> int:
    """Return physical file usage without following symlinks."""

    return sum(allocated_file_bytes(stat) for _, stat in regular_files(root))


def regular_files(root: Path):
    if not root.exists():
        return
    for path in root.rglob("*"):
        try:
            stat = path.lstat()
        except FileNotFoundError:
            continue
        if stat.st_mode & 0o170000 == 0o100000:
            yield path, stat


def allocated_file_bytes(stat: os.stat_result) -> int:
    return getattr(stat, "st_blocks", 0) * 512 or stat.st_size


def logical_bytes(root: Path) -> int:
    return sum(stat.st_size for _, stat in regular_files(root))


def file_digest(path: Path) -> bytes:
    digest = hashlib.blake2b(digest_size=32)
    with path.open("rb") as file:
        while chunk := file.read(1024 * 1024):
            digest.update(chunk)
    return digest.digest()


def cache_signatures(cache_root: Path) -> set[tuple[int, bytes]]:
    if not cache_root.is_dir():
        return set()
    signatures: set[tuple[int, bytes]] = set()
    for entry in cache_root.iterdir():
        artifact_root = entry / "artifacts"
        for path, stat in regular_files(artifact_root):
            signatures.add((stat.st_size, file_digest(path)))
    return signatures


def uncached_target_bytes(worktrees: list[Path], cached: set[tuple[int, bytes]]) -> int:
    """Count target files not shared with the immutable cache.

    Restores use macOS copy-on-write clones, whose inode/block accounting looks
    like a full copy even though the data blocks are shared.  Matching by size
    and digest gives a conservative CoW-aware storage estimate while retaining
    all target-local files and cache entries.
    """

    total = 0
    for worktree in worktrees:
        for path, stat in regular_files(worktree / "target"):
            if (stat.st_size, file_digest(path)) not in cached:
                total += allocated_file_bytes(stat)
    return total


def format_bytes(value: int) -> str:
    units = ("B", "KiB", "MiB", "GiB")
    amount = float(value)
    for unit in units:
        if amount < 1024 or unit == units[-1]:
            return f"{amount:.2f} {unit}"
        amount /= 1024
    raise AssertionError("unreachable")


def append_history(path: Path, record: dict) -> None:
    """Append one completed run without rewriting earlier benchmark evidence."""

    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as output:
        output.write(json.dumps(record, sort_keys=True) + "\n")


def prepare_home(root: Path, global_registry: Path) -> Path:
    home = root / "cargo-home"
    home.mkdir()
    if not global_registry.is_dir():
        raise BenchmarkError(f"registry source cache not found: {global_registry}")
    # Cargo may read registry metadata and sources, but the benchmark must not
    # mutate the user's cache or include its existing bytes in the measurement.
    (home / "registry").symlink_to(global_registry, target_is_directory=True)
    return home


def make_rustc_proxy(root: Path) -> Path:
    proxy = root / "counting-rustc"
    proxy.write_text(
        """#!/bin/sh
set -eu
previous=''
for argument in "$@"; do
    if [ "$previous" = '--crate-name' ]; then
        printf '%s %s\\n' "$PPID" "$argument" >>"$CARGO_CAS_BENCHMARK_RUSTC_LOG"
        break
    fi
    previous="$argument"
done
exec "$CARGO_CAS_BENCHMARK_REAL_RUSTC" "$@"
"""
    )
    proxy.chmod(0o755)
    return proxy


def make_env(
    *,
    home: Path,
    rustc: Path,
    rustdoc: Path,
    rustc_proxy: Path,
    rustc_log: Path,
    trace: bool,
) -> dict[str, str]:
    env = os.environ.copy()
    env.update(
        {
            "CARGO_HOME": str(home),
            "CARGO_NET_OFFLINE": "true",
            "CARGO_TERM_COLOR": "never",
            "CARGO_INCREMENTAL": "0",
            "RUSTC": str(rustc_proxy),
            "RUSTDOC": str(rustdoc),
            "CARGO_CAS_BENCHMARK_REAL_RUSTC": str(rustc),
            "CARGO_CAS_BENCHMARK_RUSTC_LOG": str(rustc_log),
        }
    )
    # Host flags and wrappers are outside the benchmark contract.  Clearing
    # them keeps ActionKeys and generated bytes stable across invocations.
    for variable in (
        "CARGO_BUILD_RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTFLAGS",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
    ):
        env.pop(variable, None)
    env.pop("RUST_LOG", None)
    if trace:
        env["CARGO_LOG"] = "cargo::compiler::cas=debug"
    else:
        env.pop("CARGO_LOG", None)
    return env


def cargo_command(cargo: Path, target: Path) -> list[str]:
    return [
        str(cargo),
        "test",
        "-Zcargo-cas",
        "--workspace",
        "--all-targets",
        "--all-features",
        "--no-run",
        "--profile",
        "dev",
        "--locked",
        "--target-dir",
        str(target),
    ]


def tail(path: Path, lines: int = 40) -> str:
    try:
        content = path.read_text(errors="replace")
    except OSError:
        return "<log unavailable>"
    return "\n".join(content.splitlines()[-lines:])


def run_parallel(
    *,
    phase: str,
    worktrees: list[Path],
    cargo: Path,
    env_base: dict[str, str],
    log_root: Path,
) -> tuple[list[BuildResult], float]:
    processes: list[tuple[int, subprocess.Popen[bytes], Path, float]] = []
    started = time.monotonic()
    try:
        for index, worktree in enumerate(worktrees, start=1):
            log_path = log_root / f"{phase}-worktree-{index}.log"
            target = worktree / "target"
            env = env_base
            log = log_path.open("wb")
            process = subprocess.Popen(
                cargo_command(cargo, target),
                cwd=worktree,
                env=env,
                stdout=log,
                stderr=subprocess.STDOUT,
            )
            processes.append((index, process, log_path, time.monotonic()))
            log.close()
    except OSError as error:
        for _, process, _, _ in processes:
            process.kill()
        for _, process, _, _ in processes:
            process.wait()
        raise BenchmarkError(f"could not start {phase} build: {error}") from error

    results: list[BuildResult] = []
    failures: list[str] = []
    for index, process, log_path, process_started in processes:
        returncode = process.wait()
        elapsed = time.monotonic() - process_started
        if returncode:
            failures.append(
                f"{phase} worktree {index} failed with status {returncode}:\n{tail(log_path)}"
            )
        results.append(
            BuildResult(
                index=index,
                process_pid=process.pid,
                seconds=elapsed,
                target_bytes=allocated_bytes(worktrees[index - 1] / "target"),
                log_path=log_path,
            )
        )
    if failures:
        raise BenchmarkError("\n\n".join(failures))
    return results, time.monotonic() - started


def run_rebuild_series(
    *,
    phase: str,
    rounds: int,
    worktrees: list[Path],
    cargo: Path,
    env_base: dict[str, str],
    log_root: Path,
    rustc_log: Path,
    cache_root: Path,
    preserved_cache_entries: set[str],
    edit: Callable[[int], None],
) -> RebuildSeries:
    final_results: list[BuildResult] = []
    wall_seconds: list[float] = []
    slowest_seconds: list[float] = []
    counts: Counter[str] = Counter()
    summary: Counter[str] = Counter()
    for round_index in range(1, rounds + 1):
        edit(round_index)
        results, wall = run_parallel(
            phase=f"{phase}-{round_index}",
            worktrees=worktrees,
            cargo=cargo,
            env_base=env_base,
            log_root=log_root,
        )
        final_results = results
        wall_seconds.append(wall)
        slowest_seconds.append(max(result.seconds for result in results))
        counts.update(rustc_counts(rustc_log, results))
        summary.update(cas_summaries([result.log_path for result in results]))
        prune_cache_entries(cache_root, preserved_cache_entries)
    return RebuildSeries(final_results, wall_seconds, slowest_seconds, counts, summary)


def rustc_counts(log_path: Path, results: list[BuildResult]) -> Counter[str]:
    counts: Counter[str] = Counter()
    if not log_path.is_file():
        return counts
    process_pids = {str(result.process_pid) for result in results}
    for line in log_path.read_text(errors="replace").splitlines():
        label, separator, crate = line.partition(" ")
        if separator and label in process_pids and crate:
            counts[crate] += 1
    return counts


def cas_summaries(log_paths: list[Path]) -> Counter[str]:
    """Aggregate the structured per-Cargo CAS summary records."""

    counts: Counter[str] = Counter()
    pattern = re.compile(
        r"cargo-cas summary eligible=(\d+) hits=(\d+) misses=(\d+) "
        r"rejects=(\d+) eligible_rustc=(\d+) duplicate_build_avoidance=(\d+) "
        r"skips=(?P<skips>\{.*\})$"
    )
    for path in log_paths:
        if not path.is_file():
            continue
        for line in path.read_text(errors="replace").splitlines():
            match = pattern.search(line)
            if match is None:
                continue
            fields = (
                "eligible",
                "hits",
                "misses",
                "rejects",
                "eligible_rustc",
                "duplicate_build_avoidance",
            )
            for field, value in zip(fields, match.groups()[: len(fields)]):
                counts[field] += int(value)
            try:
                skips = ast.literal_eval(match.group("skips"))
            except (SyntaxError, ValueError):
                skips = {}
            if isinstance(skips, dict):
                counts["skips"] += sum(value for value in skips.values() if isinstance(value, int))
    return counts


def restore_modes(log_paths: list[Path]) -> Counter[str]:
    counts: Counter[str] = Counter()
    for path in log_paths:
        if not path.is_file():
            continue
        for line in path.read_text(errors="replace").splitlines():
            if "cargo-cas restore: copy-on-write clone" in line:
                counts["clone"] += 1
            elif "cargo-cas restore: streaming copy fallback" in line:
                counts["copy"] += 1
    return counts


def cache_entry_count(cache_root: Path) -> int:
    if not cache_root.is_dir():
        return 0
    return sum(
        1
        for entry in cache_root.iterdir()
        if entry.is_dir() and len(entry.name) == 64 and all(c in "0123456789abcdef" for c in entry.name)
    )


def cache_entry_names(cache_root: Path) -> set[str]:
    if not cache_root.is_dir():
        return set()
    return {
        entry.name
        for entry in cache_root.iterdir()
        if entry.is_dir()
        and len(entry.name) == 64
        and all(c in "0123456789abcdef" for c in entry.name)
    }


def prune_cache_entries(cache_root: Path, preserved: set[str]) -> None:
    """Keep timing-only rebuild entries from changing the storage sample."""

    if not cache_root.is_dir():
        return
    for entry in cache_root.iterdir():
        if (
            entry.is_dir()
            and len(entry.name) == 64
            and all(c in "0123456789abcdef" for c in entry.name)
            and entry.name not in preserved
        ):
            shutil.rmtree(entry)


def print_phase(
    phase: str,
    results: list[BuildResult],
    wall_seconds: float,
    counts: Counter[str],
    summary: Counter[str],
) -> None:
    slowest = max(result.seconds for result in results)
    print(f"\n{phase}")
    print("  worktree  seconds  target")
    for result in results:
        print(f"  {result.index:>8}  {result.seconds:>7.2f}  {format_bytes(result.target_bytes)}")
    print(f"  wall time:                 {wall_seconds:.2f}s")
    print(f"  slowest process:           {slowest:.2f}s")
    print(f"  launch/wait overhead:      {max(0.0, wall_seconds - slowest):.2f}s")
    print(f"  parallel wall stretch:     {wall_seconds / slowest:.3f}x")
    print(f"  rustc invocations:         {sum(counts.values())}")
    print(f"  distinct rustc crates:      {len(counts)}")
    crate_counts = list(counts.values())
    print(
        "  crates by invocation count: "
        f"once={sum(count == 1 for count in crate_counts)}, "
        f"twice={sum(count == 2 for count in crate_counts)}, "
        f"three={sum(count == 3 for count in crate_counts)}, "
        f"four={sum(count == 4 for count in crate_counts)}"
    )
    print(
        "  CAS summary (eligible/hits/misses/skips): "
        f"{summary['eligible']}/{summary['hits']}/{summary['misses']}/{summary['skips']}"
    )
    candidates = summary["hits"] + summary["misses"]
    hit_rate = summary["hits"] / candidates * 100 if candidates else 0.0
    print(f"  CAS hit rate:              {hit_rate:.1f}%")
    print(f"  duplicate builds avoided:  {summary['duplicate_build_avoidance']}")


def print_rebuild_series(phase: str, series: RebuildSeries) -> None:
    median_wall = median(series.wall_seconds)
    median_slowest = median(series.slowest_seconds)
    print(f"\n{phase}")
    print("  round  wall time  slowest process")
    for index, (wall, slowest) in enumerate(
        zip(series.wall_seconds, series.slowest_seconds), start=1
    ):
        print(f"  {index:>5}  {wall:>9.2f}s  {slowest:>15.2f}s")
    print(f"  median wall time:          {median_wall:.2f}s")
    print(f"  median parallel stretch:  {median_wall / median_slowest:.3f}x")
    print("  final worktree  target")
    for result in series.final_results:
        print(f"  {result.index:>14}  {format_bytes(result.target_bytes)}")
    print(f"  rustc invocations:         {sum(series.counts.values())}")
    print(f"  distinct rustc crates:      {len(series.counts)}")
    crate_counts = list(series.counts.values())
    print(
        "  crates by invocation count: "
        f"once={sum(count == 1 for count in crate_counts)}, "
        f"twice={sum(count == 2 for count in crate_counts)}, "
        f"three={sum(count == 3 for count in crate_counts)}, "
        f"four={sum(count == 4 for count in crate_counts)}"
    )
    print(
        "  CAS summary (eligible/hits/misses/skips): "
        f"{series.summary['eligible']}/{series.summary['hits']}/"
        f"{series.summary['misses']}/{series.summary['skips']}"
    )
    candidates = series.summary["hits"] + series.summary["misses"]
    hit_rate = series.summary["hits"] / candidates * 100 if candidates else 0.0
    print(f"  CAS hit rate:              {hit_rate:.1f}%")
    print(
        "  duplicate builds avoided:  "
        f"{series.summary['duplicate_build_avoidance']}"
    )


def append_rebuild_marker(
    worktrees: list[Path], edit_file: Path, kind: str, round_index: int
) -> None:
    for index, worktree in enumerate(worktrees, start=1):
        source = worktree / edit_file
        source.write_text(
            source.read_text()
            + f"// cargo-cas benchmark rebuild {kind} {round_index}-{index}\n"
        )


def main() -> int:
    if platform.system() != "Darwin":
        raise BenchmarkError("the cargo-cas workspace benchmark currently requires macOS")

    project = env_path("CARGO_CAS_PROJECT_DIR", DEFAULT_PROJECT_DIR)
    cargo = env_path("CARGO_CAS_BIN", REPO_ROOT / "target" / "release" / "cargo")
    registry = env_path("CARGO_CAS_REGISTRY", Path.home() / ".cargo" / "registry")
    results_path = env_path("CARGO_CAS_RESULTS", DEFAULT_RESULTS_PATH)
    toolchain = os.environ.get(
        "CARGO_CAS_TOOLCHAIN",
        os.environ.get("CARGO_CAS_ISH_TOOLCHAIN", DEFAULT_TOOLCHAIN),
    )
    edit_file = Path(
        os.environ.get("CARGO_CAS_EDIT_FILE", str(DEFAULT_EDIT_FILE))
    )
    if edit_file.is_absolute():
        raise BenchmarkError("CARGO_CAS_EDIT_FILE must be relative to the workspace")
    rustc = resolve_tool(toolchain, "rustc")
    rustdoc = resolve_tool(toolchain, "rustdoc")
    if not project.is_dir():
        raise BenchmarkError(f"workspace checkout not found: {project}")
    if not cargo.is_file():
        raise BenchmarkError(
            f"release cargo-cas binary not found: {cargo}; run 'cargo build -p cargo --release'"
        )
    if cargo.parent.name != "release":
        raise BenchmarkError(f"cargo-cas benchmark requires a release binary, got: {cargo}")
    if not registry.is_dir():
        raise BenchmarkError(f"registry source cache not found: {registry}")
    if run_git(project, "status", "--porcelain", "--untracked-files=no"):
        raise BenchmarkError(
            f"workspace checkout has tracked changes; benchmark requires a clean tree: {project}"
        )
    if not (project / "Cargo.toml").is_file():
        raise BenchmarkError(f"workspace manifest not found: {project / 'Cargo.toml'}")
    if not (project / edit_file).is_file():
        raise BenchmarkError(f"benchmark edit source not found: {project / edit_file}")

    revision = run_git(project, "rev-parse", "HEAD")
    keep = any(
        os.environ.get(name) == "1"
        for name in ("KEEP", "CARGO_CAS_KEEP", "CARGO_CAS_ISH_KEEP")
    )
    trace = os.environ.get(
        "CARGO_CAS_TRACE",
        os.environ.get("CARGO_CAS_ISH_TRACE", "1"),
    ) != "0"
    rebuild_rounds = int(
        os.environ.get(
            "CARGO_CAS_REBUILD_ROUNDS",
            os.environ.get("CARGO_CAS_ISH_REBUILD_ROUNDS", DEFAULT_REBUILD_ROUNDS),
        )
    )
    if rebuild_rounds < 2:
        raise BenchmarkError("CARGO_CAS_REBUILD_ROUNDS must be at least 2")
    temp_root = Path(tempfile.mkdtemp(prefix=f".cargo-cas-{project.name}-", dir=project.parent))
    worktrees: list[Path] = []
    print("cargo-cas workspace four-worktree benchmark")
    print(f"  workspace:              {project}")
    print(f"  cargo-cas binary:       {cargo}")
    print(f"  revision:               {revision}")
    print(f"  toolchain:              {toolchain}")
    print(f"  worktrees:              {WORKTREE_COUNT}")
    print("  workspace build:        test --workspace --all-targets --all-features --no-run")
    print("  workspace profile:      dev (debug)")
    print(f"  rebuild source:         {edit_file}")
    print(f"  rebuild rounds:         {rebuild_rounds}")
    print(f"  CAS tracing:            {'enabled' if trace else 'disabled'}")
    print(f"  results history:        {results_path}")
    print(f"  temporary state:        {temp_root if keep else 'cleaned on exit'}")

    try:
        home = prepare_home(temp_root, registry)
        rustc_log = temp_root / "rustc.log"
        rustc_proxy = make_rustc_proxy(temp_root)
        env_base = make_env(
            home=home,
            rustc=rustc,
            rustdoc=rustdoc,
            rustc_proxy=rustc_proxy,
            rustc_log=rustc_log,
            trace=trace,
        )
        metadata = cargo_metadata(cargo, project, env_base)
        workspace_targets = workspace_target_names(metadata)
        dev_dependencies = workspace_dev_dependencies(metadata)
        external_paths = external_path_dependencies(metadata, project, temp_root)
        for name, path in external_paths.items():
            if not path.is_dir():
                raise BenchmarkError(f"workspace path dependency not found: {path}")
        print(f"  workspace packages:     {len(workspace_targets)}")
        print(f"  dev dependencies:       {len(dev_dependencies)}")
        if external_paths:
            print(
                "  external path deps:     "
                + ", ".join(f"{name}={path}" for name, path in sorted(external_paths.items()))
            )
        for index in range(1, WORKTREE_COUNT + 1):
            worktree = temp_root / f"worktree-{index}"
            run_git(project, "worktree", "add", "--detach", "--quiet", str(worktree), revision)
            worktrees.append(worktree)
            source = worktree / edit_file
            if not source.is_file():
                raise BenchmarkError(f"benchmark edit source not found: {source}")
            source.write_text(source.read_text() + f"\n// cargo-cas benchmark worktree {index}\n")

        logs = temp_root / "logs"
        logs.mkdir()
        seed, seed_wall = run_parallel(
            phase="seed",
            worktrees=[worktrees[0]],
            cargo=cargo,
            env_base=env_base,
            log_root=logs,
        )
        seed_counts = rustc_counts(rustc_log, seed)
        seed_summary = cas_summaries([result.log_path for result in seed])
        print_phase("single-worktree cache seed", seed, seed_wall, seed_counts, seed_summary)
        observed_seed_crates = set(seed_counts)
        missing_packages = sorted(
            package
            for package, targets in workspace_targets.items()
            if not observed_seed_crates.intersection(targets)
        )
        print(f"  workspace packages observed: {len(workspace_targets)}")
        print(f"  workspace package names:    {', '.join(sorted(workspace_targets))}")
        if missing_packages:
            raise BenchmarkError(
                "workspace seed did not compile package targets: "
                + ", ".join(missing_packages)
            )

        for worktree in worktrees:
            target = worktree / "target"
            if target.is_dir():
                shutil.rmtree(target)

        parallel, parallel_wall = run_parallel(
            phase="parallel",
            worktrees=worktrees,
            cargo=cargo,
            env_base=env_base,
            log_root=logs,
        )
        parallel_counts = rustc_counts(rustc_log, parallel)
        parallel_summary = cas_summaries([result.log_path for result in parallel])
        print_phase("four-worktree parallel cache restore", parallel, parallel_wall, parallel_counts, parallel_summary)

        cache_root = home / "cache" / "cargo-cas-v1"
        preserved_cache_entries = cache_entry_names(cache_root)
        restore_counts = restore_modes([result.log_path for result in parallel])

        rebuild_reference = run_rebuild_series(
            phase="rebuild-reference",
            rounds=rebuild_rounds,
            worktrees=worktrees,
            cargo=cargo,
            env_base=env_base,
            log_root=logs,
            rustc_log=rustc_log,
            cache_root=cache_root,
            preserved_cache_entries=preserved_cache_entries,
            edit=lambda round_index: append_rebuild_marker(
                worktrees, edit_file, "reference", round_index
            ),
        )
        print_rebuild_series(
            "parallel source-edit rebuild reference",
            rebuild_reference,
        )

        rebuild = run_rebuild_series(
            phase="rebuild-goal",
            rounds=rebuild_rounds,
            worktrees=worktrees,
            cargo=cargo,
            env_base=env_base,
            log_root=logs,
            rustc_log=rustc_log,
            cache_root=cache_root,
            preserved_cache_entries=preserved_cache_entries,
            edit=lambda round_index: append_rebuild_marker(
                worktrees, edit_file, "goal", round_index
            ),
        )
        print_rebuild_series("parallel source-edit rebuild goal", rebuild)

        rebuild_multiplier = median(rebuild.wall_seconds) / median(rebuild_reference.wall_seconds)
        storage_target_bytes = sum(
            allocated_bytes(worktree / "target") for worktree in worktrees
        )
        storage_target_logical_bytes = sum(
            logical_bytes(worktree / "target") for worktree in worktrees
        )
        storage_cache_bytes = allocated_bytes(cache_root)
        cached_signatures = cache_signatures(cache_root)
        storage_uncached_bytes = uncached_target_bytes(worktrees, cached_signatures)
        storage_cow_total_bytes = storage_cache_bytes + storage_uncached_bytes
        storage_measured_footprint_bytes = storage_target_bytes + storage_cache_bytes
        storage_cow_multiplier = (
            storage_cow_total_bytes / storage_target_logical_bytes
            if storage_target_logical_bytes
            else float("nan")
        )
        storage_measured_multiplier = (
            storage_measured_footprint_bytes / storage_target_bytes
            if storage_target_bytes
            else float("nan")
        )
        storage_cache_entries = cache_entry_count(cache_root)
        print("\nstorage (after final rebuild)")
        print(f"  four local target directories: {format_bytes(storage_target_bytes)}")
        print(f"  shared cargo-cas cache:         {format_bytes(storage_cache_bytes)}")
        print(f"  target-local non-cache bytes:   {format_bytes(storage_uncached_bytes)}")
        print(f"  measured footprint:             {format_bytes(storage_measured_footprint_bytes)}")
        print(f"  measured footprint multiplier:  {storage_measured_multiplier:.3f}x")
        print(f"  CoW-aware shared footprint:     {format_bytes(storage_cow_total_bytes)}")
        print(f"  CoW-aware multiplier:           {storage_cow_multiplier:.3f}x")
        print(f"  cache entries:                  {storage_cache_entries}")
        cache_share = (
            storage_cache_bytes / storage_measured_footprint_bytes * 100
            if storage_measured_footprint_bytes
            else 0.0
        )
        print(f"  cache share of measured footprint: {cache_share:.1f}%")
        print(f"  restore clones/copies:          {restore_counts['clone']}/{restore_counts['copy']}")
        storage_pass = storage_measured_multiplier <= MAX_MEASURED_FOOTPRINT_MULTIPLIER
        rebuild_pass = rebuild_multiplier <= MAX_REBUILD_MULTIPLIER
        history_record = {
            "schema": 1,
            "recorded_at": datetime.now(timezone.utc).isoformat(),
            "project": project.name,
            "project_path": str(project),
            "revision": revision,
            "toolchain": toolchain,
            "cargo_cas_binary": str(cargo),
            "cargo_cas_release": cargo.parent.name == "release",
            "profile": "dev",
            "build_mode": "debug",
            "build_command": [
                "test",
                "-Zcargo-cas",
                "--workspace",
                "--all-targets",
                "--all-features",
                "--no-run",
                "--profile",
                "dev",
                "--locked",
            ],
            "worktrees": WORKTREE_COUNT,
            "workspace_packages": sorted(workspace_targets),
            "dev_dependencies": sorted(dev_dependencies),
            "seed_wall_seconds": seed_wall,
            "parallel_wall_seconds": parallel_wall,
            "parallel_cas": dict(parallel_summary),
            "parallel_restore_modes": dict(restore_counts),
            "reference_wall_seconds": rebuild_reference.wall_seconds,
            "rebuild_wall_seconds": rebuild.wall_seconds,
            "rebuild_reference_median_seconds": median(rebuild_reference.wall_seconds),
            "rebuild_median_seconds": median(rebuild.wall_seconds),
            "rebuild_multiplier": rebuild_multiplier,
            "rebuild_cas": dict(rebuild.summary),
            "storage_target_bytes": storage_target_bytes,
            "storage_cache_bytes": storage_cache_bytes,
            "storage_uncached_target_bytes": storage_uncached_bytes,
            "storage_measured_footprint_bytes": storage_measured_footprint_bytes,
            "storage_measured_multiplier": storage_measured_multiplier,
            "storage_cow_footprint_bytes": storage_cow_total_bytes,
            "storage_cow_multiplier": storage_cow_multiplier,
            "storage_cache_entries": storage_cache_entries,
            "goals": {
                "max_measured_footprint_multiplier": MAX_MEASURED_FOOTPRINT_MULTIPLIER,
                "max_rebuild_multiplier": MAX_REBUILD_MULTIPLIER,
            },
            "status": "pass" if storage_pass and rebuild_pass else "goal-missed",
        }
        print("\ngoal")
        print(
            f"  measured footprint <= {MAX_MEASURED_FOOTPRINT_MULTIPLIER:.2f}x: "
            f"{storage_measured_multiplier:.3f}x "
            f"{'PASS' if storage_pass else 'FAIL'}"
        )
        print(
            f"  rebuild <= {MAX_REBUILD_MULTIPLIER:.2f}x reference: "
            f"{rebuild_multiplier:.3f}x {'PASS' if rebuild_pass else 'FAIL'}"
        )
        print("\nrepeatability notes")
        print("  timing rounds prune their newly published root entries before storage sampling")
        print("  measured footprint sums target and cache file blocks")
        print("  CoW-aware footprint counts cache-matching target files once")
        print("  restores use macOS clonefile, so raw file accounting overcounts shared blocks")
        print("  rustc counts and CAS decisions are read from retained temporary logs")
        append_history(results_path, history_record)
        print(f"  appended result history:        {results_path}")
        if not storage_pass or not rebuild_pass:
            raise BenchmarkError("cargo-cas workspace benchmark goal missed")
        return 0
    finally:
        if keep:
            print(f"\nkept benchmark state: {temp_root}")
        else:
            for worktree in worktrees:
                subprocess.run(
                    ["git", "-C", str(project), "worktree", "remove", "--force", str(worktree)],
                    check=False,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
            shutil.rmtree(temp_root, ignore_errors=True)
            print("\ntemporary benchmark state removed")


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (BenchmarkError, OSError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1)
