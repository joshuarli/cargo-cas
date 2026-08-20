//! Gate 1: prove the existing Cargo artifact set can be reused by another
//! workspace through a shared or manually materialized target directory.
//!
//! This is intentionally a macOS-only experiment for now.  It uses a local
//! registry and an ordinary Rust library so the only state shared between the
//! two unrelated workspaces is the target directory and the immutable registry
//! package.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::prelude::*;
use cargo_test_support::registry::{self, Package};
use cargo_test_support::{Project, RawOutput, paths, project_in};

const DEP_PACKAGE: &str = "gate-one-dep";
const DEP_CRATE: &str = "gate_one_dep";

/// A registry dependency's output and fingerprint files must be stable while
/// a second, unrelated workspace consumes the same target directory.  Cargo
/// may update `invoked.timestamp` while checking freshness, so it is excluded
/// from this snapshot.
fn dependency_files(target_dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();

    for entry in walkdir::WalkDir::new(target_dir) {
        let entry = entry.unwrap();
        if !entry.file_type().is_file() {
            continue;
        }

        let relative = entry.path().strip_prefix(target_dir).unwrap();
        let is_dependency_path = relative.components().any(|component| {
            let component = component.as_os_str().to_string_lossy();
            component == DEP_PACKAGE || component.starts_with(&format!("{DEP_PACKAGE}-"))
        });
        if !is_dependency_path {
            continue;
        }

        let name = entry.file_name().to_string_lossy();
        if name == "invoked.timestamp" {
            continue;
        }

        let extension = entry.path().extension().and_then(|ext| ext.to_str());
        let stable_output = matches!(extension, Some("d" | "rlib" | "rmeta"));
        let fingerprint = matches!(extension, Some("json"))
            || name.starts_with("dep-")
            || name.starts_with("lib-");
        if stable_output || fingerprint {
            files.insert(relative.to_owned(), fs::read(entry.path()).unwrap());
        }
    }

    files
}

fn crate_was_compiled(output: &RawOutput, crate_name: &str) -> bool {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .any(|line| line.contains("rustc") && line.contains(&format!("--crate-name {crate_name}")))
}

fn dependency_was_compiled(output: &RawOutput) -> bool {
    crate_was_compiled(output, DEP_CRATE)
}

/// Materialize only the immutable dependency unit into another target
/// directory.  This models the smallest pre-CAS global-cache experiment:
/// Cargo receives the dependency outputs and the fingerprint metadata it
/// already produced, while none of workspace A's root-package outputs are
/// copied.
fn materialize_dependency(source_target: &Path, destination_target: &Path) {
    let source = source_target.join("debug/build").join(DEP_PACKAGE);
    assert!(source.is_dir(), "missing dependency unit at {source:?}");

    for entry in walkdir::WalkDir::new(&source) {
        let entry = entry.unwrap();
        let relative = entry.path().strip_prefix(&source).unwrap();
        let destination = destination_target
            .join("debug/build")
            .join(DEP_PACKAGE)
            .join(relative);

        if entry.file_type().is_dir() {
            fs::create_dir_all(&destination).unwrap();
        } else {
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            fs::copy(entry.path(), destination).unwrap();
        }
    }
}

fn run(project: &Project, command: &str, target_dir: &Path) -> RawOutput {
    let mut cargo = project.cargo(command);
    cargo.arg("--target-dir").arg(target_dir);
    cargo.run()
}

fn workspace(name: &str, package_name: &str, source: &str) -> Project {
    let package_manifest = format!(
        r#"[package]
name = "{package_name}"
version = "0.1.0"
edition = "2024"

[dependencies]
{DEP_PACKAGE} = "1.0.0"
"#
    );

    project_in(name)
        .file(
            "Cargo.toml",
            r#"[workspace]
members = ["app"]
resolver = "2"
"#,
        )
        .file("app/Cargo.toml", &package_manifest)
        .file("app/src/main.rs", source)
        .build()
}

#[cargo_test]
fn registry_dependency_reuses_across_unrelated_workspaces() {
    registry::init();
    Package::new(DEP_PACKAGE, "1.0.0")
        .edition("2024")
        .file("src/lib.rs", "pub fn answer() -> u32 { 41 }\n")
        .publish();

    let workspace_a = workspace(
        "workspace-a",
        "workspace-a-app",
        "fn main() { println!(\"{}\", gate_one_dep::answer()); }\n",
    );
    let workspace_b = workspace(
        "workspace-b",
        "workspace-b-app",
        "fn main() { println!(\"{}\", gate_one_dep::answer() + 1); }\n",
    );

    let shared_target = paths::root().join("shared-target");

    // `check` produces the dependency metadata artifact.  B is a different
    // workspace and has different root-package source, but it must not invoke
    // rustc for the immutable registry dependency.
    let a_check = run(&workspace_a, "check -vv", &shared_target);
    assert!(dependency_was_compiled(&a_check));
    let check_before = dependency_files(&shared_target);
    assert!(
        check_before
            .keys()
            .any(|path| path.extension().and_then(|ext| ext.to_str()) == Some("rmeta")),
        "expected a registry dependency .rmeta in {shared_target:?}; files: {check_before:?}"
    );

    let b_check = run(&workspace_b, "check -vv", &shared_target);
    assert!(
        !dependency_was_compiled(&b_check),
        "workspace B recompiled {DEP_PACKAGE}:\n{}",
        String::from_utf8_lossy(&b_check.stderr)
    );
    assert_eq!(check_before, dependency_files(&shared_target));

    // The same dependency unit can be manually materialized into a different
    // target directory.  This is the controlled relocation proof: only the
    // dependency's output and Cargo fingerprint subtree crosses the boundary,
    // and workspace B still executes its own root-package rustc invocation.
    let materialized_check_target = paths::root().join("materialized-check-target");
    materialize_dependency(&shared_target, &materialized_check_target);
    assert_eq!(
        check_before,
        dependency_files(&materialized_check_target),
        "materialization changed dependency bytes"
    );
    let b_materialized_check = run(&workspace_b, "check -vv", &materialized_check_target);
    assert!(crate_was_compiled(&b_materialized_check, "workspace_b_app"));
    assert!(
        !dependency_was_compiled(&b_materialized_check),
        "workspace B recompiled materialized {DEP_PACKAGE}:\n{}",
        String::from_utf8_lossy(&b_materialized_check.stderr)
    );

    // `build` needs the linkable artifact set.  The first build invocation
    // creates it; the unrelated workspace then consumes it from the same
    // target directory without another dependency rustc invocation.
    let a_build = run(&workspace_a, "build -vv", &shared_target);
    assert!(dependency_was_compiled(&a_build));
    let build_before = dependency_files(&shared_target);
    assert!(
        build_before
            .keys()
            .any(|path| path.extension().and_then(|ext| ext.to_str()) == Some("rlib")),
        "expected a registry dependency .rlib in {shared_target:?}; files: {build_before:?}"
    );

    let b_build = run(&workspace_b, "build -vv", &shared_target);
    assert!(
        !dependency_was_compiled(&b_build),
        "workspace B recompiled {DEP_PACKAGE}:\n{}",
        String::from_utf8_lossy(&b_build.stderr)
    );
    assert_eq!(build_before, dependency_files(&shared_target));

    let materialized_build_target = paths::root().join("materialized-build-target");
    materialize_dependency(&shared_target, &materialized_build_target);
    assert_eq!(
        build_before,
        dependency_files(&materialized_build_target),
        "materialization changed dependency bytes"
    );
    let b_materialized_build = run(&workspace_b, "build -vv", &materialized_build_target);
    assert!(crate_was_compiled(&b_materialized_build, "workspace_b_app"));
    assert!(
        !dependency_was_compiled(&b_materialized_build),
        "workspace B recompiled materialized {DEP_PACKAGE}:\n{}",
        String::from_utf8_lossy(&b_materialized_build.stderr)
    );

    // A separate target directory has no reusable Cargo fingerprint state;
    // this is the controlled miss that motivates a future global CAS lookup.
    let isolated_target = paths::root().join("isolated-target");
    let isolated_check = run(&workspace_b, "check -vv", &isolated_target);
    assert!(dependency_was_compiled(&isolated_check));

    // A profile change also changes the compilation identity and must miss,
    // even though workspace B continues to use the shared target directory.
    let b_release = run(&workspace_b, "build --release -vv", &shared_target);
    assert!(dependency_was_compiled(&b_release));
}
