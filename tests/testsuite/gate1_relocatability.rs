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
use std::process::Command;

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

/// An `.rlib` is an archive containing metadata and compiler object members.
/// Comparing both the archive bytes and these extracted members makes the
/// relocatability finding explicit instead of assuming an archive-level match
/// explains every compiler output.
fn dependency_archive_members(target_dir: &Path) -> BTreeMap<PathBuf, BTreeMap<String, Vec<u8>>> {
    let mut archives = BTreeMap::new();
    for entry in walkdir::WalkDir::new(target_dir) {
        let entry = entry.unwrap();
        if !entry.file_type().is_file()
            || entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                != Some("rlib")
        {
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

        let members = Command::new("ar")
            .arg("t")
            .arg(entry.path())
            .output()
            .unwrap();
        assert!(
            members.status.success(),
            "failed to list archive members for {:?}: {members:?}",
            entry.path()
        );
        let mut archive_members = BTreeMap::new();
        for member in String::from_utf8(members.stdout).unwrap().lines() {
            let content = Command::new("ar")
                .arg("p")
                .arg(entry.path())
                .arg(member)
                .output()
                .unwrap();
            assert!(
                content.status.success(),
                "failed to extract `{member}` from {:?}: {content:?}",
                entry.path()
            );
            archive_members.insert(member.to_owned(), content.stdout);
        }
        archives.insert(relative.to_owned(), archive_members);
    }
    archives
}

/// Returns the compiler artifacts that a global cache would carry, deliberately
/// excluding Cargo's workspace-local fingerprint and dep-info bookkeeping.
fn dependency_artifacts(target_dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut artifacts = BTreeMap::new();
    for entry in walkdir::WalkDir::new(target_dir) {
        let entry = entry.unwrap();
        if !entry.file_type().is_file() {
            continue;
        }

        let extension = entry
            .path()
            .extension()
            .and_then(|extension| extension.to_str());
        if !matches!(extension, Some("rmeta" | "rlib")) {
            continue;
        }
        let relative = entry.path().strip_prefix(target_dir).unwrap();
        let is_dependency_path = relative.components().any(|component| {
            let component = component.as_os_str().to_string_lossy();
            component == DEP_PACKAGE || component.starts_with(&format!("{DEP_PACKAGE}-"))
        });
        if is_dependency_path {
            artifacts.insert(relative.to_owned(), fs::read(entry.path()).unwrap());
        }
    }
    artifacts
}

/// Cargo's translated dep-info and fingerprints participate in local
/// freshness, but are not portable compiler artifacts. They preserve the
/// target directory that supplied the artifact role.
fn dependency_local_bookkeeping(target_dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut bookkeeping = BTreeMap::new();
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
        let extension = entry
            .path()
            .extension()
            .and_then(|extension| extension.to_str());
        if matches!(extension, Some("d" | "json"))
            || name.starts_with("dep-")
            || name.starts_with("lib-")
        {
            bookkeeping.insert(relative.to_owned(), fs::read(entry.path()).unwrap());
        }
    }
    bookkeeping
}

fn contains_path(bytes: &[u8], path: &Path) -> bool {
    let path = path.to_str().unwrap().as_bytes();
    bytes.windows(path.len()).any(|window| window == path)
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

/// Rustc emits byte-identical compiler artifacts and archive members in two
/// independent workspaces. Cargo's local dep-info and fingerprint files are
/// intentionally tested separately: they contain target-directory paths and
/// are transport metadata, not global artifact identity.
#[cargo_test]
fn registry_dependency_artifacts_record_workspace_paths() {
    registry::init();
    Package::new(DEP_PACKAGE, "1.0.0")
        .edition("2024")
        .file("src/lib.rs", "pub fn answer() -> u32 { 41 }\n")
        .publish();

    let workspace_a = workspace(
        "independent-workspace-a",
        "independent-workspace-a-app",
        "fn main() { println!(\"{}\", gate_one_dep::answer()); }\n",
    );
    let workspace_b = workspace(
        "independent-workspace-b",
        "independent-workspace-b-app",
        "fn main() { println!(\"{}\", gate_one_dep::answer() + 1); }\n",
    );
    let a_target = paths::root().join("independent-a-target");
    let b_target = paths::root().join("independent-b-target");

    let a_check = run(&workspace_a, "check -vv", &a_target);
    let b_check = run(&workspace_b, "check -vv", &b_target);
    assert!(dependency_was_compiled(&a_check));
    assert!(dependency_was_compiled(&b_check));
    let a_check_artifacts = dependency_artifacts(&a_target);
    let b_check_artifacts = dependency_artifacts(&b_target);
    assert_eq!(
        a_check_artifacts.keys().collect::<Vec<_>>(),
        b_check_artifacts.keys().collect::<Vec<_>>(),
        "independent check builds must produce the same compiler artifact roles"
    );
    assert!(
        !a_check_artifacts.is_empty(),
        "the check experiment must include the dependency .rmeta artifact"
    );
    assert_eq!(
        a_check_artifacts, b_check_artifacts,
        "independent check builds must produce byte-identical reusable compiler artifacts"
    );
    let a_check_bookkeeping = dependency_local_bookkeeping(&a_target);
    let b_check_bookkeeping = dependency_local_bookkeeping(&b_target);
    assert_eq!(
        a_check_bookkeeping.keys().collect::<Vec<_>>(),
        b_check_bookkeeping.keys().collect::<Vec<_>>(),
        "independent check builds must produce the same local bookkeeping roles"
    );
    assert_ne!(
        a_check_bookkeeping, b_check_bookkeeping,
        "Cargo bookkeeping unexpectedly became target-directory independent; update this relocatability finding"
    );
    assert!(
        a_check_bookkeeping
            .values()
            .any(|bytes| contains_path(bytes, &a_target)),
        "workspace A bookkeeping must record its target directory"
    );
    assert!(
        b_check_bookkeeping
            .values()
            .any(|bytes| contains_path(bytes, &b_target)),
        "workspace B bookkeeping must record its target directory"
    );

    let a_build = run(&workspace_a, "build -vv", &a_target);
    let b_build = run(&workspace_b, "build -vv", &b_target);
    assert!(dependency_was_compiled(&a_build));
    assert!(dependency_was_compiled(&b_build));
    let a_build_artifacts = dependency_artifacts(&a_target);
    let b_build_artifacts = dependency_artifacts(&b_target);
    assert_eq!(
        a_build_artifacts.keys().collect::<Vec<_>>(),
        b_build_artifacts.keys().collect::<Vec<_>>(),
        "independent build directories must produce the same compiler artifact roles"
    );
    assert!(
        a_build_artifacts.keys().any(|path| {
            path.extension().and_then(|extension| extension.to_str()) == Some("rlib")
        }),
        "the build experiment must include the dependency .rlib archive"
    );
    assert_eq!(
        a_build_artifacts, b_build_artifacts,
        "independent build directories must produce byte-identical reusable compiler artifacts"
    );
    let a_members = dependency_archive_members(&a_target);
    let b_members = dependency_archive_members(&b_target);
    assert!(
        !a_members.is_empty(),
        "the build experiment must include the dependency .rlib archive"
    );
    assert_eq!(
        a_members.keys().collect::<Vec<_>>(),
        b_members.keys().collect::<Vec<_>>(),
        "independent builds must produce the same .rlib archive roles"
    );
    for archive in a_members.keys() {
        assert_eq!(
            a_members[archive].keys().collect::<Vec<_>>(),
            b_members[archive].keys().collect::<Vec<_>>(),
            "the archive members must have stable names for {archive:?}"
        );
    }
    assert_eq!(
        a_members, b_members,
        "every extracted .rlib member must be byte-identical across workspaces"
    );
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
