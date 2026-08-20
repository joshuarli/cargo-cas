//! macOS-only acceptance tests for the experimental `-Zcargo-cas` cache.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Child, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::prelude::*;
use cargo_test_support::registry::{self, Package};
use cargo_test_support::{Project, RawOutput, paths, project_in};

const REGISTRY_PACKAGE: &str = "cas-gate-two-dep";
const REGISTRY_CRATE: &str = "cas_gate_two_dep";

fn crate_was_compiled(output: &RawOutput, crate_name: &str) -> bool {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .any(|line| line.contains("rustc") && line.contains(&format!("--crate-name {crate_name}")))
}

fn run_check(project: &Project, target_dir: &Path, extra: &str) -> RawOutput {
    let mut cargo = project.cargo(&format!("check -Zcargo-cas -vv {extra}"));
    cargo
        .arg("--target-dir")
        .arg(target_dir)
        .masquerade_as_nightly_cargo(&["cargo-cas"]);
    cargo.run()
}

fn run_build(project: &Project, target_dir: &Path) -> RawOutput {
    let mut cargo = project.cargo("build -Zcargo-cas -vv");
    cargo
        .arg("--target-dir")
        .arg(target_dir)
        .masquerade_as_nightly_cargo(&["cargo-cas"]);
    cargo.run()
}

fn run_normal_build(project: &Project, target_dir: &Path) -> RawOutput {
    let mut cargo = project.cargo("build -vv");
    cargo.arg("--target-dir").arg(target_dir);
    cargo.run()
}

fn registry_project(name: &str, dependency: &str) -> Project {
    let manifest = format!(
        r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2024"

[dependencies]
{REGISTRY_PACKAGE} = {dependency}
"#,
    );
    project_in(name)
        .file("Cargo.toml", &manifest)
        .file(
            "src/main.rs",
            "fn main() { println!(\"{}\", cas_gate_two_dep::answer()); }\n",
        )
        .build()
}

fn cache_manifest() -> std::path::PathBuf {
    let cache = paths::home().join(".cargo/cache/cargo-cas-v0");
    fs::read_dir(&cache)
        .unwrap()
        .map(Result::unwrap)
        .map(|entry| entry.path().join("manifest.json"))
        .find(|manifest| manifest.is_file())
        .expect("a successful eligible check publishes a cargo-cas manifest")
}

fn gated_rustc(name: &str) -> std::path::PathBuf {
    let path = paths::root().join(name);
    fs::write(
        &path,
        r#"#!/bin/sh
previous=''
for argument in "$@"; do
    if [ "$previous" = '--crate-name' ] && [ "$argument" = "$CAS_TRIGGER_CRATE" ]; then
        printf '%s\n' "$argument" >> "$CAS_LOG"
        while [ ! -f "$CAS_RELEASE" ]; do sleep 0.02; done
        break
    fi
    previous="$argument"
done
exec rustc "$@"
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
    }
    path
}

fn start_gated_check(
    project: &Project,
    target_dir: &Path,
    rustc: &Path,
    trigger_crate: &str,
    log: &Path,
    release: &Path,
) -> Child {
    let mut cargo = project.cargo("check -Zcargo-cas -vv");
    cargo
        .arg("--target-dir")
        .arg(target_dir)
        .env("RUSTC", rustc)
        .env("CAS_TRIGGER_CRATE", trigger_crate)
        .env("CAS_LOG", log)
        .env("CAS_RELEASE", release)
        .masquerade_as_nightly_cargo(&["cargo-cas"]);
    cargo
        .build_command()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn wait_for_log_line(log: &Path, line: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .any(|candidate| candidate == line)
        {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

fn wait_for_log_lines(log: &Path, lines: usize) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if fs::read_to_string(log).unwrap_or_default().lines().count() >= lines {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

fn wait_for_child(child: Child) -> Output {
    thread::spawn(move || child.wait_with_output().unwrap())
        .join()
        .unwrap()
}

#[cargo_test]
fn registry_check_cache_reuses_only_matching_action_inputs() {
    registry::init();
    Package::new(REGISTRY_PACKAGE, "1.0.0")
        .edition("2024")
        .feature("alternate", &[])
        .file(
            "src/lib.rs",
            r#"
#[cfg(feature = "alternate")]
pub fn answer() -> u32 { 42 }

#[cfg(not(feature = "alternate"))]
pub fn answer() -> u32 { 41 }
"#,
        )
        .publish();

    let first = registry_project("cas-first", "\"1.0.0\"");
    let exact = registry_project("cas-exact", "\"1.0.0\"");
    let feature = registry_project(
        "cas-feature",
        "{ version = \"1.0.0\", features = [\"alternate\"] }",
    );
    let profile = registry_project("cas-profile", "\"1.0.0\"");
    let flags = registry_project("cas-flags", "\"1.0.0\"");

    let first_target = paths::root().join("cas-first-target");
    let first_output = run_check(&first, &first_target, "");
    assert!(
        crate_was_compiled(&first_output, REGISTRY_CRATE),
        "first eligible registry unit should compile:\n{}",
        String::from_utf8_lossy(&first_output.stderr)
    );
    let manifest = cache_manifest();

    let exact_output = run_check(&exact, &paths::root().join("cas-exact-target"), "");
    assert!(
        !crate_was_compiled(&exact_output, REGISTRY_CRATE),
        "matching registry action should restore from cargo-cas:\n{}",
        String::from_utf8_lossy(&exact_output.stderr)
    );
    assert!(crate_was_compiled(&exact_output, "cas_exact"));

    let feature_output = run_check(&feature, &paths::root().join("cas-feature-target"), "");
    assert!(
        crate_was_compiled(&feature_output, REGISTRY_CRATE),
        "feature changes must not hit a default-feature entry:\n{}",
        String::from_utf8_lossy(&feature_output.stderr)
    );

    let profile_output = run_check(
        &profile,
        &paths::root().join("cas-profile-target"),
        "--release",
    );
    assert!(
        crate_was_compiled(&profile_output, REGISTRY_CRATE),
        "profile changes must not hit a dev-profile entry:\n{}",
        String::from_utf8_lossy(&profile_output.stderr)
    );

    let mut flag_command = flags.cargo("check -Zcargo-cas -vv");
    flag_command
        .arg("--target-dir")
        .arg(paths::root().join("cas-flags-target"))
        .env("RUSTFLAGS", "-C target-cpu=generic")
        .masquerade_as_nightly_cargo(&["cargo-cas"]);
    let flags_output = flag_command.run();
    assert!(
        crate_was_compiled(&flags_output, REGISTRY_CRATE),
        "rustflags changes must not hit a differently flagged entry:\n{}",
        String::from_utf8_lossy(&flags_output.stderr)
    );

    // Cache damage is a performance problem, never a build failure.  A bad
    // manifest must fall back to Cargo's normal rustc work for the exact same
    // semantic action.
    fs::write(&manifest, b"not valid json").unwrap();
    let fallback = registry_project("cas-fallback", "\"1.0.0\"");
    let fallback_output = run_check(&fallback, &paths::root().join("cas-fallback-target"), "");
    assert!(
        crate_was_compiled(&fallback_output, REGISTRY_CRATE),
        "a corrupt cargo-cas entry must fall back to rustc:\n{}",
        String::from_utf8_lossy(&fallback_output.stderr)
    );
}

#[cargo_test]
fn non_registry_dependencies_always_use_normal_rustc_work() {
    let project = project_in("cas-path-fallback")
        .file(
            "Cargo.toml",
            r#"[package]
name = "cas-path-fallback"
version = "0.1.0"
edition = "2024"

[dependencies]
local-dependency = { path = "local-dependency" }
"#,
        )
        .file("src/main.rs", "fn main() { local_dependency::answer(); }\n")
        .file(
            "local-dependency/Cargo.toml",
            r#"[package]
name = "local-dependency"
version = "0.1.0"
edition = "2024"
"#,
        )
        .file("local-dependency/src/lib.rs", "pub fn answer() {}\n")
        .build();

    let output = run_check(&project, &paths::root().join("cas-path-target"), "");
    assert!(
        crate_was_compiled(&output, "local_dependency"),
        "path sources are ineligible and must run normal rustc:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cargo_test]
fn registry_build_cache_restores_metadata_and_linkable_artifacts() {
    const BUILD_PACKAGE: &str = "cas-gate-three-dep";
    const BUILD_CRATE: &str = "cas_gate_three_dep";

    registry::init();
    Package::new(BUILD_PACKAGE, "1.0.0")
        .edition("2024")
        .file("src/lib.rs", "pub fn answer() -> u32 { 42 }\n")
        .publish();

    let first_manifest = format!(
        r#"[package]
name = "cas-build-first"
version = "0.1.0"
edition = "2024"

[dependencies]
{BUILD_PACKAGE} = "1.0.0"
"#,
    );
    let first = project_in("cas-build-first")
        .file("Cargo.toml", &first_manifest)
        .file(
            "src/main.rs",
            "fn main() { println!(\"{}\", cas_gate_three_dep::answer()); }\n",
        )
        .build();
    let second_manifest = format!(
        r#"[package]
name = "cas-build-second"
version = "0.1.0"
edition = "2024"

[dependencies]
{BUILD_PACKAGE} = "1.0.0"
"#,
    );
    let second = project_in("cas-build-second")
        .file("Cargo.toml", &second_manifest)
        .file(
            "src/main.rs",
            "fn main() { println!(\"{}\", cas_gate_three_dep::answer() + 1); }\n",
        )
        .build();

    let first_target = paths::root().join("cas-build-first-target");
    let first_output = run_build(&first, &first_target);
    assert!(crate_was_compiled(&first_output, BUILD_CRATE));
    let manifest = cache_manifest();
    let manifest_text = fs::read_to_string(&manifest).unwrap();
    assert!(manifest_text.contains("\"rmeta\""));
    assert!(manifest_text.contains("\"linkable\""));
    assert!(manifest_text.contains(".rlib"));

    let second_target = paths::root().join("cas-build-second-target");
    let second_output = run_build(&second, &second_target);
    assert!(
        !crate_was_compiled(&second_output, BUILD_CRATE),
        "matching build action should restore .rmeta and .rlib:\n{}",
        String::from_utf8_lossy(&second_output.stderr)
    );
    assert!(crate_was_compiled(&second_output, "cas_build_second"));
    assert!(
        second_target.join("debug/cas-build-second").is_file(),
        "a cache hit must leave Cargo's ordinary final binary intact"
    );

    // The materialized artifacts and normal fingerprint state are also usable
    // by an invocation that does not opt into cargo-cas.  This guards the
    // scheduler boundary: a cache hit is normal local Cargo state, not a
    // separate freshness regime.
    let normal_output = run_normal_build(&second, &second_target);
    assert!(
        !crate_was_compiled(&normal_output, BUILD_CRATE),
        "normal Cargo should accept the materialized dependency artifact:\n{}",
        String::from_utf8_lossy(&normal_output.stderr)
    );
    assert!(
        !crate_was_compiled(&normal_output, "cas_build_second"),
        "the normal final artifact should remain fresh after a cache hit:\n{}",
        String::from_utf8_lossy(&normal_output.stderr)
    );
}

#[cargo_test]
fn cache_ignores_partial_entries_and_repairs_corrupt_writer_state() {
    const PACKAGE: &str = "cas-gate-four-dep";
    const CRATE: &str = "cas_gate_four_dep";

    registry::init();
    Package::new(PACKAGE, "1.0.0")
        .edition("2024")
        .file("src/lib.rs", "pub fn answer() -> u32 { 42 }\n")
        .publish();

    // This is exactly the state left by a process that dies while staging an
    // entry.  `tmp` is never considered by lookup, so it cannot become a hit.
    let cache = paths::cargo_home().join("cache/cargo-cas-v0");
    let abandoned = cache.join("tmp/crashed-writer/artifacts/0");
    fs::create_dir_all(abandoned.parent().unwrap()).unwrap();
    fs::write(&abandoned, b"partial artifact").unwrap();

    let manifest = format!(
        r#"[package]
name = "cas-gate-four-app"
version = "0.1.0"
edition = "2024"

[dependencies]
{PACKAGE} = "1.0.0"
"#,
    );
    let first = project_in("cas-gate-four-first")
        .file("Cargo.toml", &manifest)
        .file(
            "src/main.rs",
            "fn main() { cas_gate_four_dep::answer(); }\n",
        )
        .build();
    let second = project_in("cas-gate-four-second")
        .file("Cargo.toml", &manifest)
        .file(
            "src/main.rs",
            "fn main() { cas_gate_four_dep::answer(); }\n",
        )
        .build();
    let third = project_in("cas-gate-four-third")
        .file("Cargo.toml", &manifest)
        .file(
            "src/main.rs",
            "fn main() { cas_gate_four_dep::answer(); }\n",
        )
        .build();

    let first_output = run_check(
        &first,
        &paths::root().join("cas-gate-four-first-target"),
        "",
    );
    assert!(crate_was_compiled(&first_output, CRATE));
    let cache_manifest = cache_manifest();
    let entry = cache_manifest.parent().unwrap().to_path_buf();
    assert!(abandoned.is_file());

    // A manifest symlink is not a cache manifest.  In particular, lookup must
    // not follow it outside the entry, and successful normal work must replace
    // the invalid entry with a complete immutable one.
    let outside_manifest = paths::root().join("outside-cargo-cas-manifest");
    fs::copy(&cache_manifest, &outside_manifest).unwrap();
    fs::remove_file(&cache_manifest).unwrap();
    symlink(&outside_manifest, &cache_manifest).unwrap();
    let symlink_output = run_check(
        &second,
        &paths::root().join("cas-gate-four-second-target"),
        "",
    );
    assert!(
        crate_was_compiled(&symlink_output, CRATE),
        "a symlinked cache manifest must be rejected:\n{}",
        String::from_utf8_lossy(&symlink_output.stderr)
    );
    assert!(!cache_manifest.is_symlink());

    let repaired_output = run_check(
        &third,
        &paths::root().join("cas-gate-four-third-target"),
        "",
    );
    assert!(
        !crate_was_compiled(&repaired_output, CRATE),
        "a repaired entry should be reusable:\n{}",
        String::from_utf8_lossy(&repaired_output.stderr)
    );

    // A regular artifact whose digest no longer matches the manifest is also
    // corrupt.  Cargo must reject it, rebuild normally, and make the repaired
    // immutable entry available to the following workspace.
    fs::write(entry.join("artifacts/0"), b"corrupt artifact").unwrap();
    let corrupt = project_in("cas-gate-four-corrupt")
        .file("Cargo.toml", &manifest)
        .file(
            "src/main.rs",
            "fn main() { cas_gate_four_dep::answer(); }\n",
        )
        .build();
    let digest_repaired = project_in("cas-gate-four-digest-repaired")
        .file("Cargo.toml", &manifest)
        .file(
            "src/main.rs",
            "fn main() { cas_gate_four_dep::answer(); }\n",
        )
        .build();
    let corrupt_output = run_check(
        &corrupt,
        &paths::root().join("cas-gate-four-corrupt-target"),
        "",
    );
    assert!(crate_was_compiled(&corrupt_output, CRATE));
    let digest_repaired_output = run_check(
        &digest_repaired,
        &paths::root().join("cas-gate-four-digest-repaired-target"),
        "",
    );
    assert!(
        !crate_was_compiled(&digest_repaired_output, CRATE),
        "a digest-repaired entry should be reusable:\n{}",
        String::from_utf8_lossy(&digest_repaired_output.stderr)
    );

    // Simulate a crash after a final directory is created but before a
    // manifest is published.  It is not a hit and the following normal build
    // repairs it; the next workspace then gets a verified hit.
    fs::remove_dir_all(&entry).unwrap();
    fs::create_dir_all(entry.join("artifacts")).unwrap();
    fs::write(entry.join("artifacts/0"), b"partial artifact").unwrap();
    let partial = project_in("cas-gate-four-partial")
        .file("Cargo.toml", &manifest)
        .file(
            "src/main.rs",
            "fn main() { cas_gate_four_dep::answer(); }\n",
        )
        .build();
    let recovered = project_in("cas-gate-four-recovered")
        .file("Cargo.toml", &manifest)
        .file(
            "src/main.rs",
            "fn main() { cas_gate_four_dep::answer(); }\n",
        )
        .build();
    let partial_output = run_check(
        &partial,
        &paths::root().join("cas-gate-four-partial-target"),
        "",
    );
    assert!(crate_was_compiled(&partial_output, CRATE));
    let recovered_output = run_check(
        &recovered,
        &paths::root().join("cas-gate-four-recovered-target"),
        "",
    );
    assert!(
        !crate_was_compiled(&recovered_output, CRATE),
        "a repaired partial entry should be reusable:\n{}",
        String::from_utf8_lossy(&recovered_output.stderr)
    );
}

#[cargo_test]
fn concurrent_same_key_compiles_once_and_waiters_restore() {
    const PACKAGE: &str = "cas-gate-five-same";
    const CRATE: &str = "cas_gate_five_same";

    registry::init();
    Package::new(PACKAGE, "1.0.0")
        .edition("2024")
        .file("src/lib.rs", "pub fn answer() {}\n")
        .publish();

    let manifest = format!(
        r#"[package]
name = "cas-gate-five-app"
version = "0.1.0"
edition = "2024"

[dependencies]
{PACKAGE} = "1.0.0"
"#,
    );
    let first = project_in("cas-gate-five-same-first")
        .file("Cargo.toml", &manifest)
        .file(
            "src/main.rs",
            "fn main() { cas_gate_five_same::answer(); }\n",
        )
        .build();
    let second = project_in("cas-gate-five-same-second")
        .file("Cargo.toml", &manifest)
        .file(
            "src/main.rs",
            "fn main() { cas_gate_five_same::answer(); }\n",
        )
        .build();

    let rustc = gated_rustc("cas-gate-five-rustc");
    let log = paths::root().join("cas-gate-five-same.log");
    let release = paths::root().join("cas-gate-five-same.release");
    let first_child = start_gated_check(
        &first,
        &paths::root().join("cas-gate-five-same-first-target"),
        &rustc,
        CRATE,
        &log,
        &release,
    );
    assert!(wait_for_log_line(&log, CRATE));
    let second_child = start_gated_check(
        &second,
        &paths::root().join("cas-gate-five-same-second-target"),
        &rustc,
        CRATE,
        &log,
        &release,
    );

    // Without a per-key recheck both processes reach this gate and append a
    // second line. The first compiler remains paused long enough for a second
    // cache miss to contend on the same ActionKey lock.
    let duplicate_compiler_started = wait_for_log_lines(&log, 2);
    fs::write(&release, "release").unwrap();
    let first_output = wait_for_child(first_child);
    let second_output = wait_for_child(second_child);
    assert!(first_output.status.success(), "{first_output:?}");
    assert!(second_output.status.success(), "{second_output:?}");
    assert!(
        !duplicate_compiler_started,
        "same-key concurrent cache misses started more than one rustc: {}",
        fs::read_to_string(&log).unwrap_or_default()
    );
    assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 1);
}

#[cargo_test]
fn concurrent_different_keys_do_not_serialize() {
    const LEFT_PACKAGE: &str = "cas-gate-five-left";
    const LEFT_CRATE: &str = "cas_gate_five_left";
    const RIGHT_PACKAGE: &str = "cas-gate-five-right";
    const RIGHT_CRATE: &str = "cas_gate_five_right";

    registry::init();
    for (package, crate_name) in [(LEFT_PACKAGE, LEFT_CRATE), (RIGHT_PACKAGE, RIGHT_CRATE)] {
        Package::new(package, "1.0.0")
            .edition("2024")
            .file("src/lib.rs", "pub fn answer() {}\n")
            .publish();
        assert!(crate_name.starts_with("cas_gate_five_"));
    }

    let left_manifest = format!(
        r#"[package]
name = "cas-gate-five-left-app"
version = "0.1.0"
edition = "2024"

[dependencies]
{LEFT_PACKAGE} = "1.0.0"
"#,
    );
    let right_manifest = format!(
        r#"[package]
name = "cas-gate-five-right-app"
version = "0.1.0"
edition = "2024"

[dependencies]
{RIGHT_PACKAGE} = "1.0.0"
"#,
    );
    let left = project_in("cas-gate-five-left")
        .file("Cargo.toml", &left_manifest)
        .file(
            "src/main.rs",
            "fn main() { cas_gate_five_left::answer(); }\n",
        )
        .build();
    let right = project_in("cas-gate-five-right")
        .file("Cargo.toml", &right_manifest)
        .file(
            "src/main.rs",
            "fn main() { cas_gate_five_right::answer(); }\n",
        )
        .build();

    let rustc = gated_rustc("cas-gate-five-different-rustc");
    let log = paths::root().join("cas-gate-five-different.log");
    let release = paths::root().join("cas-gate-five-different.release");
    let left_child = start_gated_check(
        &left,
        &paths::root().join("cas-gate-five-left-target"),
        &rustc,
        LEFT_CRATE,
        &log,
        &release,
    );
    let right_child = start_gated_check(
        &right,
        &paths::root().join("cas-gate-five-right-target"),
        &rustc,
        RIGHT_CRATE,
        &log,
        &release,
    );

    let left_started = wait_for_log_line(&log, LEFT_CRATE);
    let right_started = wait_for_log_line(&log, RIGHT_CRATE);
    fs::write(&release, "release").unwrap();
    let left_output = wait_for_child(left_child);
    let right_output = wait_for_child(right_child);
    assert!(left_output.status.success(), "{left_output:?}");
    assert!(right_output.status.success(), "{right_output:?}");
    assert!(
        left_started && right_started,
        "different ActionKeys serialized: {}",
        fs::read_to_string(&log).unwrap_or_default()
    );
}
