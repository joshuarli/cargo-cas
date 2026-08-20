//! macOS-only acceptance tests for the experimental `-Zcargo-cas` cache.

use std::fs;
use std::path::Path;

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
