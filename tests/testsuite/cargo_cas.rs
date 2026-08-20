//! macOS-only acceptance tests for the experimental `-Zcargo-cas` cache.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Child, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

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

fn run_check_with_cas_log(project: &Project, target_dir: &Path) -> RawOutput {
    let mut cargo = project.cargo("check -Zcargo-cas -vv");
    cargo
        .arg("--target-dir")
        .arg(target_dir)
        .env("CARGO_LOG", "cargo::compiler::cas=debug")
        .masquerade_as_nightly_cargo(&["cargo-cas"]);
    cargo.run()
}

fn run_check_with_rustc(project: &Project, target_dir: &Path, rustc: &Path) -> RawOutput {
    let mut cargo = project.cargo("check -Zcargo-cas -vv");
    cargo
        .arg("--target-dir")
        .arg(target_dir)
        .env("RUSTC", rustc)
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

fn run_cas_gc(project: &Project, options: &str) -> RawOutput {
    let mut cargo = project.cargo(&format!("clean gc -Zgc {options}"));
    cargo.masquerade_as_nightly_cargo(&["gc"]);
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

fn registry_dependency_project(name: &str, dependency: &str) -> Project {
    project_in(name)
        .file(
            "Cargo.toml",
            &format!(
                r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2024"

[dependencies]
{dependency} = "1.0.0"
"#,
            ),
        )
        .file("src/main.rs", "fn main() {}\n")
        .build()
}

fn cache_manifest() -> std::path::PathBuf {
    let cache = paths::home().join(".cargo/cache/cargo-cas-v1");
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

fn rustc_proxy(name: &str, extra_cfg: Option<&str>) -> std::path::PathBuf {
    let path = paths::root().join(name);
    let extra_cfg = extra_cfg
        .map(|cfg| format!("--cfg {cfg} "))
        .unwrap_or_default();
    fs::write(&path, format!("#!/bin/sh\nexec rustc {extra_cfg}\"$@\"\n")).unwrap();
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
    // Exercise Cargo's per-unit lock lifecycle together with the CAS action
    // lock. The latter is acquired only inside active work, after Cargo has
    // upgraded its own dirty unit lock, so same-key coordination must not turn
    // unrelated units into a global serialization point.
    let mut cargo = project.cargo("check -Zcargo-cas -Zfine-grain-locking -vv");
    cargo
        .arg("--target-dir")
        .arg(target_dir)
        .env("RUSTC", rustc)
        .env("CAS_TRIGGER_CRATE", trigger_crate)
        .env("CAS_LOG", log)
        .env("CAS_RELEASE", release)
        .masquerade_as_nightly_cargo(&["cargo-cas", "fine-grain-locking"]);
    cargo
        .build_command()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn start_gated_check_in_dir(
    driver: &Project,
    working_dir: &Path,
    target_dir: &Path,
    rustc: &Path,
    trigger_crate: &str,
    log: &Path,
    release: &Path,
) -> Child {
    let mut cargo = driver.cargo("check -Zcargo-cas -Zfine-grain-locking -vv");
    cargo
        .cwd(working_dir)
        .arg("--target-dir")
        .arg(target_dir)
        .env("RUSTC", rustc)
        .env("CAS_TRIGGER_CRATE", trigger_crate)
        .env("CAS_LOG", log)
        .env("CAS_RELEASE", release)
        .masquerade_as_nightly_cargo(&["cargo-cas", "fine-grain-locking"]);
    cargo
        .build_command()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn start_check_paused_before_cas_publish(
    project: &Project,
    target_dir: &Path,
    pause_signal: &Path,
) -> Child {
    let mut cargo = project.cargo("check -Zcargo-cas -vv");
    cargo
        .arg("--target-dir")
        .arg(target_dir)
        .env("CARGO_CAS_TEST_PAUSE_BEFORE_PUBLISH", pause_signal)
        .masquerade_as_nightly_cargo(&["cargo-cas"]);
    cargo
        .build_command()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn wait_for_path(path: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
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

fn directory_file_size(path: &Path) -> u64 {
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_type().is_file().then(|| entry.metadata().ok()))
        .flatten()
        .map(|metadata| metadata.len())
        .sum()
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
    let manifest_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&manifest).unwrap()).unwrap();
    assert_eq!(manifest_json["format_version"], 1);
    assert_eq!(manifest_json["identity"]["target_name"], REGISTRY_CRATE);
    assert_eq!(manifest_json["identity"]["compile_mode"], "check");
    assert!(manifest_json["identity"]["package_id"].is_string());
    assert!(manifest_json["identity"]["toolchain"]["rustc_path"].is_string());
    assert!(manifest_json["identity"]["toolchain"]["rustc_verbose_version"].is_string());
    assert!(manifest_json["identity"]["toolchain"]["sysroot"].is_string());
    assert!(manifest_json["identity"]["dependency_action_keys"].is_array());

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

    // The ActionKey remains the lookup address, but its semantic identity is
    // duplicated in the manifest so an altered package/unit/toolchain record
    // is independently rejected before artifact materialization.
    let mut poisoned_manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&manifest).unwrap()).unwrap();
    poisoned_manifest["identity"]["toolchain"]["rustc_path"] =
        serde_json::Value::String("/poisoned/rustc".to_owned());
    fs::write(&manifest, serde_json::to_vec(&poisoned_manifest).unwrap()).unwrap();
    let identity_fallback = registry_project("cas-identity-fallback", "\"1.0.0\"");
    let identity_fallback_output = run_check(
        &identity_fallback,
        &paths::root().join("cas-identity-fallback-target"),
        "",
    );
    assert!(
        crate_was_compiled(&identity_fallback_output, REGISTRY_CRATE),
        "an entry with mismatched manifest identity must be rebuilt:\n{}",
        String::from_utf8_lossy(&identity_fallback_output.stderr)
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
fn build_script_and_proc_macro_dependency_subgraphs_use_normal_rustc() {
    const BUILD_SCRIPT_PACKAGE: &str = "cas-build-script-dep";
    const BUILD_SCRIPT_CRATE: &str = "cas_build_script_dep";
    const PROC_MACRO_PACKAGE: &str = "cas-proc-macro-dep";
    const PROC_MACRO_CRATE: &str = "cas_proc_macro_dep";
    const PROC_MACRO_USER_PACKAGE: &str = "cas-proc-macro-user";
    const PROC_MACRO_USER_CRATE: &str = "cas_proc_macro_user";

    registry::init();
    Package::new(BUILD_SCRIPT_PACKAGE, "1.0.0")
        .edition("2024")
        .file("build.rs", "fn main() {}\n")
        .file("src/lib.rs", "pub fn answer() {}\n")
        .publish();
    Package::new(PROC_MACRO_PACKAGE, "1.0.0")
        .edition("2024")
        .proc_macro(true)
        .file(
            "src/lib.rs",
            r#"extern crate proc_macro;
use proc_macro::TokenStream;

#[proc_macro]
pub fn noop(_input: TokenStream) -> TokenStream { TokenStream::new() }
"#,
        )
        .publish();
    Package::new(PROC_MACRO_USER_PACKAGE, "1.0.0")
        .edition("2024")
        .dep(PROC_MACRO_PACKAGE, "1.0.0")
        .file(
            "src/lib.rs",
            "use cas_proc_macro_dep::noop;\nnoop!();\npub fn answer() {}\n",
        )
        .publish();

    let build_script_first =
        registry_dependency_project("cas-build-script-first", BUILD_SCRIPT_PACKAGE);
    let build_script_second =
        registry_dependency_project("cas-build-script-second", BUILD_SCRIPT_PACKAGE);
    let proc_macro_first =
        registry_dependency_project("cas-proc-macro-first", PROC_MACRO_USER_PACKAGE);
    let proc_macro_second =
        registry_dependency_project("cas-proc-macro-second", PROC_MACRO_USER_PACKAGE);

    let build_script_first_output = run_check(
        &build_script_first,
        &paths::root().join("cas-build-script-first-target"),
        "",
    );
    assert!(crate_was_compiled(
        &build_script_first_output,
        BUILD_SCRIPT_CRATE
    ));
    let build_script_second_output = run_check(
        &build_script_second,
        &paths::root().join("cas-build-script-second-target"),
        "",
    );
    assert!(
        crate_was_compiled(&build_script_second_output, BUILD_SCRIPT_CRATE),
        "a build-script-affected registry package must remain a normal compile:\n{}",
        String::from_utf8_lossy(&build_script_second_output.stderr)
    );

    let proc_macro_first_output = run_check(
        &proc_macro_first,
        &paths::root().join("cas-proc-macro-first-target"),
        "",
    );
    assert!(crate_was_compiled(
        &proc_macro_first_output,
        PROC_MACRO_CRATE
    ));
    assert!(crate_was_compiled(
        &proc_macro_first_output,
        PROC_MACRO_USER_CRATE
    ));
    let proc_macro_second_output = run_check(
        &proc_macro_second,
        &paths::root().join("cas-proc-macro-second-target"),
        "",
    );
    assert!(
        crate_was_compiled(&proc_macro_second_output, PROC_MACRO_CRATE)
            && crate_was_compiled(&proc_macro_second_output, PROC_MACRO_USER_CRATE),
        "a proc-macro-affected registry package must remain a normal compile:\n{}",
        String::from_utf8_lossy(&proc_macro_second_output.stderr)
    );
}

#[cargo_test]
fn debug_logging_explains_cargo_cas_hit_miss_and_skip_decisions() {
    const PACKAGE: &str = "cas-observability-dep";
    const CRATE: &str = "cas_observability_dep";

    registry::init();
    Package::new(PACKAGE, "1.0.0")
        .edition("2024")
        .file("src/lib.rs", "pub fn answer() {}\n")
        .publish();

    let manifest = format!(
        r#"[package]
name = "cas-observability-app"
version = "0.1.0"
edition = "2024"

[dependencies]
{PACKAGE} = "1.0.0"
"#,
    );
    let first = project_in("cas-observability-first")
        .file("Cargo.toml", &manifest)
        .file(
            "src/main.rs",
            "fn main() { cas_observability_dep::answer(); }\n",
        )
        .build();
    let second = project_in("cas-observability-second")
        .file("Cargo.toml", &manifest)
        .file(
            "src/main.rs",
            "fn main() { cas_observability_dep::answer(); }\n",
        )
        .build();
    let path_source = project_in("cas-observability-path")
        .file(
            "Cargo.toml",
            r#"[package]
name = "cas-observability-path"
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

    let first_output = run_check_with_cas_log(
        &first,
        &paths::root().join("cas-observability-first-target"),
    );
    assert!(crate_was_compiled(&first_output, CRATE));
    assert!(
        String::from_utf8_lossy(&first_output.stderr).contains("cargo-cas miss: entry absent"),
        "an eligible cold unit must report an actionable miss:\n{}",
        String::from_utf8_lossy(&first_output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&first_output.stderr).contains("cargo-cas summary"),
        "a completed cache-enabled build must report aggregate cache metrics:\n{}",
        String::from_utf8_lossy(&first_output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&first_output.stderr).contains("eligible=1"),
        "the cold build should report its one eligible dependency:\n{}",
        String::from_utf8_lossy(&first_output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&first_output.stderr).contains("eligible_rustc=1"),
        "the cold build should report its one eligible compiler invocation:\n{}",
        String::from_utf8_lossy(&first_output.stderr)
    );

    let second_output = run_check_with_cas_log(
        &second,
        &paths::root().join("cas-observability-second-target"),
    );
    assert!(!crate_was_compiled(&second_output, CRATE));
    assert!(
        String::from_utf8_lossy(&second_output.stderr).contains("cargo-cas hit"),
        "an eligible warm unit must report a cache hit:\n{}",
        String::from_utf8_lossy(&second_output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&second_output.stderr).contains("hits=1"),
        "the warm build summary should report its cache hit:\n{}",
        String::from_utf8_lossy(&second_output.stderr)
    );

    let skip_output = run_check_with_cas_log(
        &path_source,
        &paths::root().join("cas-observability-path-target"),
    );
    assert!(crate_was_compiled(&skip_output, "local_dependency"));
    assert!(
        String::from_utf8_lossy(&skip_output.stderr).contains("cargo-cas skip: path source"),
        "an ineligible unit must report why it was skipped:\n{}",
        String::from_utf8_lossy(&skip_output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&skip_output.stderr).contains("skips={\"path source\":"),
        "the summary should aggregate skip reasons:\n{}",
        String::from_utf8_lossy(&skip_output.stderr)
    );
}

#[cargo_test]
fn compiler_path_is_part_of_the_cargo_cas_action_identity() {
    const PACKAGE: &str = "cas-toolchain-identity-dep";
    const CRATE: &str = "cas_toolchain_identity_dep";

    registry::init();
    Package::new(PACKAGE, "1.0.0")
        .edition("2024")
        .file(
            "src/lib.rs",
            r#"
#[cfg(cas_toolchain_b)]
pub const VALUE: usize = 2;

#[cfg(not(cas_toolchain_b))]
pub const VALUE: usize = 1;
"#,
        )
        .publish();

    let manifest = format!(
        r#"[package]
name = "cas-toolchain-identity-app"
version = "0.1.0"
edition = "2024"

[dependencies]
{PACKAGE} = "1.0.0"
"#,
    );
    let first = project_in("cas-toolchain-identity-first")
        .file("Cargo.toml", &manifest)
        .file(
            "src/main.rs",
            "fn main() { let _ = cas_toolchain_identity_dep::VALUE; }\n",
        )
        .build();
    let second = project_in("cas-toolchain-identity-second")
        .file("Cargo.toml", &manifest)
        .file(
            "src/main.rs",
            "const _: [(); 2] = [(); cas_toolchain_identity_dep::VALUE];\nfn main() {}\n",
        )
        .build();

    // Both proxies report the same `rustc -vV` data. The second one changes
    // actual compiler behavior, so accepting the first entry by verbose
    // version alone would produce an invalid cross-crate constant here.
    let rustc_a = rustc_proxy("cas-rustc-a", None);
    let rustc_b = rustc_proxy("cas-rustc-b", Some("cas_toolchain_b"));
    let first_output = run_check_with_rustc(
        &first,
        &paths::root().join("cas-toolchain-identity-first-target"),
        &rustc_a,
    );
    assert!(crate_was_compiled(&first_output, CRATE));
    let second_output = run_check_with_rustc(
        &second,
        &paths::root().join("cas-toolchain-identity-second-target"),
        &rustc_b,
    );
    assert!(
        crate_was_compiled(&second_output, CRATE),
        "a distinct compiler path must not reuse an entry from another compiler:\n{}",
        String::from_utf8_lossy(&second_output.stderr)
    );
}

#[cargo_test]
fn registry_source_and_transitive_action_identity_never_share_entries() {
    const SOURCE_PACKAGE: &str = "cas-key-source";
    const SOURCE_CRATE: &str = "cas_key_source";
    const CHILD_PACKAGE: &str = "cas-key-child";
    const PARENT_PACKAGE: &str = "cas-key-parent";
    const PARENT_CRATE: &str = "cas_key_parent";

    registry::alt_init();
    // The two registry archives deliberately have the same package name and
    // version but different content. A package name/version is never enough
    // to address a global artifact; source URL and registry checksum are part
    // of the ActionKey.
    Package::new(SOURCE_PACKAGE, "1.0.0")
        .edition("2024")
        .file("src/lib.rs", "pub const VALUE: usize = 1;\n")
        .publish();
    Package::new(SOURCE_PACKAGE, "1.0.0")
        .edition("2024")
        .file("src/lib.rs", "pub const VALUE: usize = 2;\n")
        .alternative(true)
        .publish();

    let first_source = project_in("cas-key-main-registry")
        .file(
            "Cargo.toml",
            &format!(
                r#"[package]
name = "main-registry-app"
version = "0.1.0"
edition = "2024"

[dependencies]
{SOURCE_PACKAGE} = "1.0.0"
"#,
            ),
        )
        .file(
            "src/main.rs",
            "const _: [(); 1] = [(); cas_key_source::VALUE];\nfn main() {}\n",
        )
        .build();
    let alternate_source = project_in("cas-key-alternative-registry")
        .file(
            "Cargo.toml",
            &format!(
                r#"[package]
name = "alternative-registry-app"
version = "0.1.0"
edition = "2024"

[dependencies]
{SOURCE_PACKAGE} = {{ version = "1.0.0", registry = "alternative" }}
"#,
            ),
        )
        .file(
            "src/main.rs",
            "const _: [(); 2] = [(); cas_key_source::VALUE];\nfn main() {}\n",
        )
        .build();

    let first_source_output = run_check(
        &first_source,
        &paths::root().join("cas-key-main-registry-target"),
        "",
    );
    assert!(crate_was_compiled(&first_source_output, SOURCE_CRATE));
    let alternate_source_output = run_check(
        &alternate_source,
        &paths::root().join("cas-key-alternative-registry-target"),
        "",
    );
    assert!(
        crate_was_compiled(&alternate_source_output, SOURCE_CRATE),
        "a same-name/version package from another registry must not reuse the first source:\n{}",
        String::from_utf8_lossy(&alternate_source_output.stderr)
    );

    Package::new(CHILD_PACKAGE, "1.0.0")
        .edition("2024")
        .file("src/lib.rs", "pub const VALUE: usize = 1;\n")
        .publish();
    Package::new(PARENT_PACKAGE, "1.0.0")
        .edition("2024")
        .dep(CHILD_PACKAGE, "1")
        .file(
            "src/lib.rs",
            "pub const VALUE: usize = cas_key_child::VALUE;\n",
        )
        .publish();
    let parent_manifest = format!(
        r#"[package]
name = "transitive-action-app"
version = "0.1.0"
edition = "2024"

[dependencies]
{PARENT_PACKAGE} = "1.0.0"
"#,
    );
    let old_child = project_in("cas-key-transitive-old")
        .file("Cargo.toml", &parent_manifest)
        .file(
            "src/main.rs",
            "const _: [(); 1] = [(); cas_key_parent::VALUE];\nfn main() {}\n",
        )
        .build();
    let old_child_output = run_check(
        &old_child,
        &paths::root().join("cas-key-transitive-old-target"),
        "",
    );
    assert!(crate_was_compiled(&old_child_output, PARENT_CRATE));

    // The parent source is unchanged, but a fresh resolution selects a new
    // child action. The parent's ActionKey must include that dependency DAG
    // edge rather than reuse metadata compiled against child 1.0.0.
    Package::new(CHILD_PACKAGE, "1.1.0")
        .edition("2024")
        .file("src/lib.rs", "pub const VALUE: usize = 2;\n")
        .publish();
    let new_child = project_in("cas-key-transitive-new")
        .file("Cargo.toml", &parent_manifest)
        .file(
            "src/main.rs",
            "const _: [(); 2] = [(); cas_key_parent::VALUE];\nfn main() {}\n",
        )
        .build();
    let new_child_output = run_check(
        &new_child,
        &paths::root().join("cas-key-transitive-new-target"),
        "",
    );
    assert!(
        crate_was_compiled(&new_child_output, PARENT_CRATE),
        "a changed direct dependency action must invalidate its parent:\n{}",
        String::from_utf8_lossy(&new_child_output.stderr)
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

    // A dependency hit must also preserve Cargo's ordinary artifact export
    // path. The root binary is always built and exported locally; only the
    // immutable dependency artifacts are restored from cargo-cas.
    let artifact_target = paths::root().join("cas-build-artifact-dir-target");
    let artifact_dir = paths::root().join("cas-build-artifact-dir-export");
    let mut artifact_command = second.cargo("build -Zcargo-cas -Zunstable-options -vv");
    artifact_command
        .arg("--target-dir")
        .arg(&artifact_target)
        .arg("--artifact-dir")
        .arg(&artifact_dir)
        .masquerade_as_nightly_cargo(&["cargo-cas", "unstable-options"]);
    let artifact_output = artifact_command.run();
    assert!(
        !crate_was_compiled(&artifact_output, BUILD_CRATE),
        "an artifact-dir build should still restore the matching dependency:\n{}",
        String::from_utf8_lossy(&artifact_output.stderr)
    );
    assert!(
        artifact_dir.join("cas-build-second").is_file(),
        "the root binary must be exported through Cargo's normal artifact-dir path"
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
fn killed_cargo_before_atomic_publish_leaves_no_cache_hit() {
    const PACKAGE: &str = "cas-gate-four-killed-writer";
    const CRATE: &str = "cas_gate_four_killed_writer";

    registry::init();
    Package::new(PACKAGE, "1.0.0")
        .edition("2024")
        .file("src/lib.rs", "pub fn answer() -> u32 { 42 }\n")
        .publish();

    let manifest = format!(
        r#"[package]
name = "crash-before-publish-app"
version = "0.1.0"
edition = "2024"

[dependencies]
{PACKAGE} = "1.0.0"
"#,
    );
    let killed_writer = project_in("cas-gate-four-killed-writer")
        .file("Cargo.toml", &manifest)
        .file(
            "src/main.rs",
            "fn main() { cas_gate_four_killed_writer::answer(); }\n",
        )
        .build();
    let recovery = project_in("cas-gate-four-killed-writer-recovery")
        .file("Cargo.toml", &manifest)
        .file(
            "src/main.rs",
            "fn main() { cas_gate_four_killed_writer::answer(); }\n",
        )
        .build();
    let hit = project_in("cas-gate-four-killed-writer-hit")
        .file("Cargo.toml", &manifest)
        .file(
            "src/main.rs",
            "fn main() { cas_gate_four_killed_writer::answer(); }\n",
        )
        .build();

    let pause_signal = paths::root().join("cas-gate-four-before-publish");
    let mut child = start_check_paused_before_cas_publish(
        &killed_writer,
        &paths::root().join("cas-gate-four-killed-writer-target"),
        &pause_signal,
    );
    assert!(
        wait_for_path(&pause_signal),
        "the writer did not reach the pre-publish crash boundary"
    );

    // `kill` is SIGKILL on macOS. At this point `tmp/<unique-writer>` is
    // complete but has not been renamed into the ActionKey directory.
    child.kill().unwrap();
    let killed = wait_for_child(child);
    assert!(
        !killed.status.success(),
        "a killed Cargo process unexpectedly exited successfully: {killed:?}"
    );
    fs::remove_file(&pause_signal).unwrap();

    let recovery_output = run_check(
        &recovery,
        &paths::root().join("cas-gate-four-killed-writer-recovery-target"),
        "",
    );
    assert!(
        crate_was_compiled(&recovery_output, CRATE),
        "an unpublished staged entry must be ignored and rebuilt:\n{}",
        String::from_utf8_lossy(&recovery_output.stderr)
    );
    let hit_output = run_check(
        &hit,
        &paths::root().join("cas-gate-four-killed-writer-hit-target"),
        "",
    );
    assert!(
        !crate_was_compiled(&hit_output, CRATE),
        "the successful recovery must publish a reusable entry:\n{}",
        String::from_utf8_lossy(&hit_output.stderr)
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
    let cache = paths::cargo_home().join("cache/cargo-cas-v1");
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

    // Each manifest field is validation data, not merely descriptive state.
    // Removing or truncating an artifact, lying about its size/digest, or
    // replacing its relative name with a traversal path must all rebuild.
    let artifact = entry.join("artifacts/0");
    fs::remove_file(&artifact).unwrap();
    let missing_output = run_check(
        &partial,
        &paths::root().join("cas-gate-four-missing-artifact-target"),
        "",
    );
    assert!(crate_was_compiled(&missing_output, CRATE));
    let missing_repaired_output = run_check(
        &recovered,
        &paths::root().join("cas-gate-four-missing-artifact-repaired-target"),
        "",
    );
    assert!(!crate_was_compiled(&missing_repaired_output, CRATE));

    fs::write(&artifact, b"truncated").unwrap();
    let truncated_output = run_check(
        &partial,
        &paths::root().join("cas-gate-four-truncated-artifact-target"),
        "",
    );
    assert!(crate_was_compiled(&truncated_output, CRATE));
    let truncated_repaired_output = run_check(
        &recovered,
        &paths::root().join("cas-gate-four-truncated-artifact-repaired-target"),
        "",
    );
    assert!(!crate_was_compiled(&truncated_repaired_output, CRATE));

    let mut altered_size: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cache_manifest).unwrap()).unwrap();
    let size = altered_size["artifacts"][0]["size"].as_u64().unwrap();
    altered_size["artifacts"][0]["size"] = serde_json::Value::from(size + 1);
    fs::write(&cache_manifest, serde_json::to_vec(&altered_size).unwrap()).unwrap();
    let size_output = run_check(
        &partial,
        &paths::root().join("cas-gate-four-altered-size-target"),
        "",
    );
    assert!(crate_was_compiled(&size_output, CRATE));
    let size_repaired_output = run_check(
        &recovered,
        &paths::root().join("cas-gate-four-altered-size-repaired-target"),
        "",
    );
    assert!(!crate_was_compiled(&size_repaired_output, CRATE));

    let mut altered_digest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cache_manifest).unwrap()).unwrap();
    altered_digest["artifacts"][0]["digest"] = serde_json::Value::String("00".to_owned());
    fs::write(
        &cache_manifest,
        serde_json::to_vec(&altered_digest).unwrap(),
    )
    .unwrap();
    let digest_output = run_check(
        &partial,
        &paths::root().join("cas-gate-four-altered-digest-target"),
        "",
    );
    assert!(crate_was_compiled(&digest_output, CRATE));
    let digest_repaired_output = run_check(
        &recovered,
        &paths::root().join("cas-gate-four-altered-digest-repaired-target"),
        "",
    );
    assert!(!crate_was_compiled(&digest_repaired_output, CRATE));

    let outside_artifact = paths::root().join("outside-cargo-cas-artifact");
    fs::write(&outside_artifact, b"outside artifact remains untouched").unwrap();
    let mut traversal_manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cache_manifest).unwrap()).unwrap();
    traversal_manifest["artifacts"][0]["file"] = serde_json::Value::String("../outside".to_owned());
    fs::write(
        &cache_manifest,
        serde_json::to_vec(&traversal_manifest).unwrap(),
    )
    .unwrap();
    let traversal_output = run_check(
        &partial,
        &paths::root().join("cas-gate-four-path-traversal-target"),
        "",
    );
    assert!(crate_was_compiled(&traversal_output, CRATE));
    assert_eq!(
        fs::read(&outside_artifact).unwrap(),
        b"outside artifact remains untouched"
    );
    let traversal_repaired_output = run_check(
        &recovered,
        &paths::root().join("cas-gate-four-path-traversal-repaired-target"),
        "",
    );
    assert!(!crate_was_compiled(&traversal_repaired_output, CRATE));

    fs::remove_file(&artifact).unwrap();
    symlink(&outside_artifact, &artifact).unwrap();
    let artifact_symlink_output = run_check(
        &partial,
        &paths::root().join("cas-gate-four-artifact-symlink-target"),
        "",
    );
    assert!(crate_was_compiled(&artifact_symlink_output, CRATE));
    assert_eq!(
        fs::read(&outside_artifact).unwrap(),
        b"outside artifact remains untouched"
    );
    let artifact_symlink_repaired_output = run_check(
        &recovered,
        &paths::root().join("cas-gate-four-artifact-symlink-repaired-target"),
        "",
    );
    assert!(!crate_was_compiled(
        &artifact_symlink_repaired_output,
        CRATE
    ));

    // Last-use tracking is mutable, but it must not be an escaping write
    // primitive. A bad access symlink can lose usage accounting, never a cache
    // hit or bytes outside the cache root.
    let access = cache.join("access").join(entry.file_name().unwrap());
    let outside_access = paths::root().join("outside-cargo-cas-access");
    fs::write(&outside_access, b"outside access remains untouched").unwrap();
    fs::remove_file(&access).unwrap();
    symlink(&outside_access, &access).unwrap();
    let access_symlink_output = run_check(
        &recovered,
        &paths::root().join("cas-gate-four-access-symlink-target"),
        "",
    );
    assert!(
        !crate_was_compiled(&access_symlink_output, CRATE),
        "last-use tracking failure must not discard a valid cache hit:\n{}",
        String::from_utf8_lossy(&access_symlink_output.stderr)
    );
    assert_eq!(
        fs::read(&outside_access).unwrap(),
        b"outside access remains untouched"
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

#[cargo_test]
fn eight_worktrees_share_one_registry_action_without_rebuilding_it() {
    const PACKAGE: &str = "cas-gate-six-dep";
    const CRATE: &str = "cas_gate_six_dep";
    const ROOT_CRATE: &str = "cas_gate_six_app";
    const WORKTREE_COUNT: usize = 8;

    registry::init();
    Package::new(PACKAGE, "1.0.0")
        .edition("2024")
        .file("src/lib.rs", "pub fn answer() -> u32 { 42 }\n")
        .publish();

    let repository_root = paths::root().join("cas-gate-six-repository");
    let manifest = format!(
        r#"[package]
name = "cas-gate-six-app"
version = "0.1.0"
edition = "2024"

[dependencies]
{PACKAGE} = "1.0.0"
"#,
    );
    let source = cargo_test_support::git::repo(&repository_root)
        .file("Cargo.toml", &manifest)
        .file("src/main.rs", "fn main() { println!(\"seed\"); }\n")
        .build();
    let repository = git2::Repository::open(source.root()).unwrap();
    let worktrees_root = paths::root().join("cas-gate-six-worktrees");
    fs::create_dir_all(&worktrees_root).unwrap();

    let mut worktrees = vec![source.root().to_path_buf()];
    for index in 1..WORKTREE_COUNT {
        let path = worktrees_root.join(format!("worktree-{index}"));
        let options = git2::WorktreeAddOptions::new();
        repository
            .worktree(&format!("worktree-{index}"), &path, Some(&options))
            .unwrap();
        worktrees.push(path);
    }
    for (index, worktree) in worktrees.iter().enumerate() {
        fs::write(
            worktree.join("src/main.rs"),
            format!("fn main() {{ println!(\"{{}}\", cas_gate_six_dep::answer() + {index}); }}\n"),
        )
        .unwrap();
    }

    let driver = project_in("cas-gate-six-driver").no_manifest().build();
    let rustc = gated_rustc("cas-gate-six-rustc");
    let log = paths::root().join("cas-gate-six.log");
    let release = paths::root().join("cas-gate-six.release");
    let start = Instant::now();
    let children = worktrees
        .iter()
        .enumerate()
        .map(|(index, worktree)| {
            start_gated_check_in_dir(
                &driver,
                worktree,
                &paths::root().join(format!("cas-gate-six-target-{index}")),
                &rustc,
                CRATE,
                &log,
                &release,
            )
        })
        .collect::<Vec<_>>();

    assert!(wait_for_log_line(&log, CRATE));
    let duplicate_compiler_started = wait_for_log_lines(&log, 2);
    fs::write(&release, "release").unwrap();
    let outputs = children.into_iter().map(wait_for_child).collect::<Vec<_>>();
    assert!(
        outputs.iter().all(|output| output.status.success()),
        "one of the concurrent worktree builds failed: {outputs:#?}"
    );
    assert!(
        !duplicate_compiler_started,
        "the shared action compiled more than once: {}",
        fs::read_to_string(&log).unwrap_or_default()
    );
    assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 1);
    assert!(
        outputs.iter().all(|output| {
            String::from_utf8_lossy(&output.stderr).contains(&format!("--crate-name {ROOT_CRATE}"))
        }),
        "every distinct worktree root must compile independently"
    );

    let cache_root = paths::cargo_home().join("cache/cargo-cas-v1");
    let cache_entries = fs::read_dir(&cache_root)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("manifest.json").is_file())
        .count();
    let elapsed = start.elapsed();
    let workspace_bytes = (0..WORKTREE_COUNT)
        .map(|index| {
            directory_file_size(&paths::root().join(format!("cas-gate-six-target-{index}")))
        })
        .sum::<u64>();
    eprintln!(
        "cargo-cas Gate 6: worktrees={WORKTREE_COUNT}, shared-rustc=1, \
         local-rustc={WORKTREE_COUNT}, cache-hits={}, cache-misses=1, \
         duplicate-builds-avoided={}, cache-entries={cache_entries}, \
         cache-bytes={}, workspace-bytes={workspace_bytes}, elapsed-ms={}",
        WORKTREE_COUNT - 1,
        WORKTREE_COUNT - 1,
        directory_file_size(&cache_root),
        elapsed.as_millis(),
    );
}

#[cargo_test]
fn resolved_git_revisions_are_reused_without_treating_branches_as_identity() {
    const PACKAGE: &str = "cas-gate-seven-dep";
    const CRATE: &str = "cas_gate_seven_dep";

    let (git_project, repository) = cargo_test_support::git::new_repo(PACKAGE, |project| {
        project
            .file(
                "Cargo.toml",
                r#"[package]
name = "cas-gate-seven-dep"
version = "1.0.0"
edition = "2024"
"#,
            )
            .file("src/lib.rs", "pub fn answer() -> u32 { 1 }\n")
    });
    let first_revision = repository.head().unwrap().target().unwrap().to_string();
    let url = git_project.url();

    let branch_manifest = format!(
        r#"[package]
name = "cas-gate-seven-branch-app"
version = "0.1.0"
edition = "2024"

[dependencies]
{PACKAGE} = {{ git = "{url}", branch = "master" }}
"#,
    );
    let first = project_in("cas-gate-seven-first")
        .file("Cargo.toml", &branch_manifest)
        .file(
            "src/main.rs",
            "fn main() { println!(\"{}\", cas_gate_seven_dep::answer()); }\n",
        )
        .build();
    let same_revision = project_in("cas-gate-seven-same-revision")
        .file("Cargo.toml", &branch_manifest)
        .file(
            "src/main.rs",
            "fn main() { println!(\"{}\", cas_gate_seven_dep::answer()); }\n",
        )
        .build();

    let first_output = run_check(
        &first,
        &paths::root().join("cas-gate-seven-first-target"),
        "",
    );
    assert!(crate_was_compiled(&first_output, CRATE));
    let first_lockfile = first.read_lockfile();
    let same_output = run_check(
        &same_revision,
        &paths::root().join("cas-gate-seven-same-revision-target"),
        "",
    );
    assert!(
        !crate_was_compiled(&same_output, CRATE),
        "the same resolved git revision should reuse the cached action:\n{}",
        String::from_utf8_lossy(&same_output.stderr)
    );

    // Move the branch after the first cache publication. A new workspace that
    // resolves the branch must compile the new commit, proving that the branch
    // label itself is not an ActionKey input.
    git_project.change_file("src/lib.rs", "pub fn answer() -> u32 { 2 }\n");
    cargo_test_support::git::add(&repository);
    let second_revision = cargo_test_support::git::commit(&repository).to_string();
    assert_ne!(first_revision, second_revision);
    let moved_branch = project_in("cas-gate-seven-moved-branch")
        .file("Cargo.toml", &branch_manifest)
        .file(
            "src/main.rs",
            "fn main() { println!(\"{}\", cas_gate_seven_dep::answer()); }\n",
        )
        .build();
    let moved_output = run_check(
        &moved_branch,
        &paths::root().join("cas-gate-seven-moved-branch-target"),
        "",
    );
    assert!(
        crate_was_compiled(&moved_output, CRATE),
        "a moved git branch must miss because its resolved revision changed:\n{}",
        String::from_utf8_lossy(&moved_output.stderr)
    );

    // An explicit revision is also immutable, but Cargo's compiler metadata
    // distinguishes that declaration from a branch dependency. It receives a
    // separate ActionKey instead of overwriting the branch entry.
    let explicit_manifest = format!(
        r#"[package]
name = "cas-gate-seven-explicit-app"
version = "0.1.0"
edition = "2024"

[dependencies]
{PACKAGE} = {{ git = "{url}", rev = "{first_revision}" }}
"#,
    );
    let explicit = project_in("cas-gate-seven-explicit")
        .file("Cargo.toml", &explicit_manifest)
        .file(
            "src/main.rs",
            "fn main() { println!(\"{}\", cas_gate_seven_dep::answer()); }\n",
        )
        .build();
    let explicit_output = run_check(
        &explicit,
        &paths::root().join("cas-gate-seven-explicit-target"),
        "",
    );
    assert!(crate_was_compiled(&explicit_output, CRATE));

    // Reuse the old branch resolution from a lockfile. The branch label is
    // still present in the package source, but its immutable locked revision
    // is restored exactly, so this action must recover the original hit.
    let pinned = project_in("cas-gate-seven-locked-old-revision")
        .file("Cargo.toml", &branch_manifest)
        .file("Cargo.lock", &first_lockfile)
        .file(
            "src/main.rs",
            "fn main() { println!(\"{}\", cas_gate_seven_dep::answer()); }\n",
        )
        .build();
    let pinned_output = run_check(
        &pinned,
        &paths::root().join("cas-gate-seven-locked-old-revision-target"),
        "",
    );
    assert!(
        !crate_was_compiled(&pinned_output, CRATE),
        "a lockfile-pinned old revision should recover its prior cached action:\n{}",
        String::from_utf8_lossy(&pinned_output.stderr)
    );
}

#[cargo_test]
fn cargo_cas_gc_evicts_by_size_and_last_use_age() {
    const PACKAGE: &str = "cas-gate-eight-dep";
    const CRATE: &str = "cas_gate_eight_dep";

    registry::init();
    Package::new(PACKAGE, "1.0.0")
        .edition("2024")
        .file("src/lib.rs", "pub fn answer() -> u32 { 42 }\n")
        .publish();

    let manifest = format!(
        r#"[package]
name = "cas-gate-eight-app"
version = "0.1.0"
edition = "2024"

[dependencies]
{PACKAGE} = "1.0.0"
"#,
    );
    let first = project_in("cas-gate-eight-first")
        .file("Cargo.toml", &manifest)
        .file(
            "src/main.rs",
            "fn main() { println!(\"{}\", cas_gate_eight_dep::answer()); }\n",
        )
        .build();
    let after_size_gc = project_in("cas-gate-eight-after-size-gc")
        .file("Cargo.toml", &manifest)
        .file(
            "src/main.rs",
            "fn main() { println!(\"{}\", cas_gate_eight_dep::answer()); }\n",
        )
        .build();
    let after_age_gc = project_in("cas-gate-eight-after-age-gc")
        .file("Cargo.toml", &manifest)
        .file(
            "src/main.rs",
            "fn main() { println!(\"{}\", cas_gate_eight_dep::answer()); }\n",
        )
        .build();

    let first_output = run_check(
        &first,
        &paths::root().join("cas-gate-eight-first-target"),
        "",
    );
    assert!(crate_was_compiled(&first_output, CRATE));
    let first_manifest = cache_manifest();
    let cache_root = paths::cargo_home().join("cache/cargo-cas-v1");
    let first_access = cache_root
        .join("access")
        .join(first_manifest.parent().unwrap().file_name().unwrap());
    assert!(
        first_access.is_file(),
        "a published cache entry records its last use separately from immutable artifacts"
    );

    // An explicit size policy removes entire entries. The next use is an
    // ordinary cache miss and must rebuild rather than observing partial
    // state.
    run_cas_gc(&first, "--max-cas-size=0");
    assert!(
        !first_manifest.exists(),
        "the zero-size policy must evict the immutable entry"
    );
    assert!(!first_access.exists());
    let after_size_output = run_check(
        &after_size_gc,
        &paths::root().join("cas-gate-eight-after-size-gc-target"),
        "",
    );
    assert!(
        crate_was_compiled(&after_size_output, CRATE),
        "a size-evicted entry must rebuild normally:\n{}",
        String::from_utf8_lossy(&after_size_output.stderr)
    );

    let regenerated_manifest = cache_manifest();
    let regenerated_access = cache_root
        .join("access")
        .join(regenerated_manifest.parent().unwrap().file_name().unwrap());
    let access_file = fs::OpenOptions::new()
        .write(true)
        .open(&regenerated_access)
        .unwrap();
    access_file
        .set_times(fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
        .unwrap();

    run_cas_gc(&after_size_gc, "--max-cas-age=1day");
    assert!(
        !regenerated_manifest.exists(),
        "an old last-use timestamp must evict the immutable entry"
    );
    assert!(!regenerated_access.exists());
    let after_age_output = run_check(
        &after_age_gc,
        &paths::root().join("cas-gate-eight-after-age-gc-target"),
        "",
    );
    assert!(
        crate_was_compiled(&after_age_output, CRATE),
        "an age-evicted entry must rebuild normally:\n{}",
        String::from_utf8_lossy(&after_age_output.stderr)
    );
}
