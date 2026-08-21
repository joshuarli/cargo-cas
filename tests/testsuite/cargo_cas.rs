//! macOS-only acceptance tests for the always-on cargo-cas cache.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
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
    let mut cargo = project.cargo(&format!("check -vv {extra}"));
    cargo
        .arg("--target-dir")
        .arg(target_dir);
    cargo.run()
}

fn run_check_with_cas_log(project: &Project, target_dir: &Path) -> RawOutput {
    let mut cargo = project.cargo("check -vv");
    cargo
        .arg("--target-dir")
        .arg(target_dir)
        .env("CARGO_LOG", "cargo::compiler::cas=debug");
    cargo.run()
}

fn run_check_with_cas_log_in(project: &Project, cwd: &Path, target_dir: &Path) -> RawOutput {
    let mut cargo = project.cargo("check -vv");
    cargo
        .cwd(cwd)
        .arg("--target-dir")
        .arg(target_dir)
        .env("CARGO_LOG", "cargo::compiler::cas=debug");
    cargo.run()
}

fn run_check_with_rustc(project: &Project, target_dir: &Path, rustc: &Path) -> RawOutput {
    let mut cargo = project.cargo("check -vv");
    cargo
        .arg("--target-dir")
        .arg(target_dir)
        .env("RUSTC", rustc);
    cargo.run()
}

fn run_check_with_config(project: &Project, target_dir: &Path, config: &str) -> RawOutput {
    let mut cargo = project.cargo("check -vv");
    cargo
        .arg("--target-dir")
        .arg(target_dir)
        .arg("--config")
        .arg(config);
    cargo.run()
}

fn run_check_for_explicit_host_target(project: &Project, target_dir: &Path) -> RawOutput {
    let rustc_verbose = Command::new("rustc").arg("-vV").output().unwrap();
    let host = String::from_utf8(rustc_verbose.stdout)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .expect("rustc -vV includes a host triple")
        .to_owned();
    let mut cargo = project.cargo("check -vv");
    cargo
        .arg("--target-dir")
        .arg(target_dir)
        .arg("--target")
        .arg(host);
    cargo.run()
}

fn run_build(project: &Project, target_dir: &Path) -> RawOutput {
    let mut cargo = project.cargo("build -vv");
    cargo
        .arg("--target-dir")
        .arg(target_dir);
    cargo.run()
}

fn run_build_with_rustc(
    project: &Project,
    target_dir: &Path,
    rustc: &Path,
    trigger_crate: &str,
    log: &Path,
    release: &Path,
) -> RawOutput {
    let mut cargo = project.cargo("build -vv");
    cargo
        .arg("--target-dir")
        .arg(target_dir)
        .env("RUSTC", rustc)
        .env("CAS_TRIGGER_CRATE", trigger_crate)
        .env("CAS_LOG", log)
        .env("CAS_RELEASE", release);
    cargo.run()
}

fn run_clean(project: &Project, target_dir: &Path) -> RawOutput {
    let mut cargo = project.cargo("clean -vv");
    cargo.arg("--target-dir").arg(target_dir);
    cargo.run()
}

fn run_git(root: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args(arguments)
        .output()
        .unwrap_or_else(|error| panic!("failed to start git {:?}: {error}", arguments));
    assert!(
        output.status.success(),
        "git {:?} failed:\n{}",
        arguments,
        String::from_utf8_lossy(&output.stderr)
    );
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

fn cache_manifest_for_crate(crate_name: &str) -> std::path::PathBuf {
    let cache = paths::home().join(".cargo/cache/cargo-cas-v1");
    fs::read_dir(&cache)
        .unwrap()
        .map(Result::unwrap)
        .map(|entry| entry.path().join("manifest.json"))
        .filter(|manifest| manifest.is_file())
        .find(|manifest| {
            serde_json::from_str::<serde_json::Value>(&fs::read_to_string(manifest).unwrap())
                .ok()
                .and_then(|json| json["identity"]["crate_name"].as_str().map(str::to_owned))
                .is_some_and(|name| name == crate_name)
        })
        .unwrap_or_else(|| panic!("no cargo-cas manifest for crate {crate_name}"))
}

fn build_script_cache_manifest(package_fragment: &str) -> std::path::PathBuf {
    let cache = paths::home().join(".cargo/cache/cargo-cas-v1");
    fs::read_dir(&cache)
        .unwrap()
        .map(Result::unwrap)
        .map(|entry| entry.path().join("build-script.json"))
        .filter(|manifest| manifest.is_file())
        .find(|manifest| fs::read_to_string(manifest).unwrap().contains(package_fragment))
        .unwrap_or_else(|| panic!("no cargo-cas build-script manifest for {package_fragment}"))
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
    let mut cargo = project.cargo("check -Zfine-grain-locking -vv");
    cargo
        .arg("--target-dir")
        .arg(target_dir)
        .env("RUSTC", rustc)
        .env("CAS_TRIGGER_CRATE", trigger_crate)
        .env("CAS_LOG", log)
        .env("CAS_RELEASE", release)
        .masquerade_as_nightly_cargo(&["fine-grain-locking"]);
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
    let mut cargo = driver.cargo("check -Zfine-grain-locking -vv");
    cargo
        .cwd(working_dir)
        .arg("--target-dir")
        .arg(target_dir)
        .env("RUSTC", rustc)
        .env("CARGO_LOG", "cargo::compiler::cas=debug")
        .env("CAS_TRIGGER_CRATE", trigger_crate)
        .env("CAS_LOG", log)
        .env("CAS_RELEASE", release)
        .masquerade_as_nightly_cargo(&["fine-grain-locking"]);
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
    let mut cargo = project.cargo("check -vv");
    cargo
        .arg("--target-dir")
        .arg(target_dir)
        .env("CARGO_CAS_TEST_PAUSE_BEFORE_PUBLISH", pause_signal);
    cargo
        .build_command()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn start_build_paused_after_cas_rmeta(
    project: &Project,
    target_dir: &Path,
    rustc: &Path,
    trigger_crate: &str,
    log: &Path,
    release: &Path,
    pause_signal: &Path,
) -> Child {
    let mut cargo = project.cargo("build -vv");
    cargo
        .arg("--target-dir")
        .arg(target_dir)
        .env("RUSTC", rustc)
        .env("CAS_TRIGGER_CRATE", trigger_crate)
        .env("CAS_LOG", log)
        .env("CAS_RELEASE", release)
        .env("CARGO_CAS_TEST_PAUSE_AFTER_RMETA", pause_signal);
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

fn contains_path(bytes: &[u8], path: &Path) -> bool {
    let path = path.to_str().unwrap().as_bytes();
    bytes.windows(path.len()).any(|window| window == path)
}

fn assert_valid_cache_entry(entry: &Path) {
    let manifest_path = entry.join("manifest.json");
    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
    for artifact in manifest["artifacts"].as_array().unwrap() {
        let path = entry
            .join("artifacts")
            .join(artifact["file"].as_str().unwrap());
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert!(
            metadata.file_type().is_file(),
            "artifact is not a regular file: {path:?}"
        );
        assert_eq!(metadata.len(), artifact["size"].as_u64().unwrap());
        assert_eq!(
            blake3::hash(&fs::read(&path).unwrap()).to_hex().as_str(),
            artifact["digest"].as_str().unwrap(),
            "artifact digest mismatch: {path:?}"
        );
    }
}

/// Compiler artifacts backed by the immutable CAS use one inode in the cache
/// and every Cargo target that consumes that exact action. The manifest keeps
/// the destination-specific filename, so locate it below the target rather
/// than assuming Cargo's build-directory layout.
fn assert_cache_artifacts_share_target(entry: &Path, target: &Path) {
    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(entry.join("manifest.json")).unwrap()).unwrap();
    for artifact in manifest["artifacts"].as_array().unwrap() {
        let role = artifact["role"].as_str().unwrap();
        if !matches!(role, "rmeta" | "linkable") {
            continue;
        }
        let output_name = artifact["output_file_name"].as_str().unwrap();
        let target_artifact = walkdir::WalkDir::new(target)
            .into_iter()
            .filter_map(Result::ok)
            .find(|candidate| {
                candidate.file_type().is_file() && candidate.file_name() == output_name
            })
            .map(|candidate| candidate.into_path())
            .unwrap_or_else(|| {
                panic!("target {target:?} does not contain cached {role} {output_name}")
            });
        let cache_artifact = entry
            .join("artifacts")
            .join(artifact["file"].as_str().unwrap());
        assert!(
            same_file::is_same_file(&cache_artifact, &target_artifact).unwrap(),
            "cache artifact {cache_artifact:?} and target artifact {target_artifact:?} must share one inode"
        );
        assert!(
            cache_artifact.metadata().unwrap().permissions().readonly(),
            "a cache-backed compiler artifact must remain read-only"
        );
        if role == "linkable" {
            let rlib_stem = Path::new(output_name).file_stem().unwrap().to_str().unwrap();
            let codegen_prefix = format!("{}.", rlib_stem.strip_prefix("lib").unwrap());
            let output_directory = target_artifact.parent().unwrap();
            assert!(
                fs::read_dir(output_directory)
                    .unwrap()
                    .filter_map(Result::ok)
                    .all(|entry| {
                        let name = entry.file_name();
                        let name = name.to_string_lossy();
                        !name.starts_with(&codegen_prefix) || !name.ends_with(".rcgu.o")
                    }),
                "a reusable rlib already archives its own codegen objects; Cargo must not retain them beside {target_artifact:?}"
            );
        }
    }
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
    assert_eq!(manifest_json["format_version"], 6);
    assert_eq!(manifest_json["identity"]["target_name"], REGISTRY_CRATE);
    assert_eq!(manifest_json["identity"]["compile_mode"], "check");
    assert!(manifest_json["identity"]["package_id"].is_string());
    assert!(manifest_json["identity"]["toolchain"]["rustc_path"].is_string());
    assert!(manifest_json["identity"]["toolchain"]["rustc_verbose_version"].is_string());
    assert!(manifest_json["identity"]["toolchain"]["sysroot"].is_string());
    assert!(manifest_json["identity"]["dependency_action_keys"].is_array());
    let cached_dep_info = manifest_json["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["role"] == "dep-info")
        .expect("a cache entry includes translated dep-info");
    let cached_dep_info_path = manifest
        .parent()
        .unwrap()
        .join("artifacts")
        .join(cached_dep_info["file"].as_str().unwrap());
    assert!(
        !contains_path(&fs::read(cached_dep_info_path).unwrap(), &first_target),
        "the globally stored dep-info must not retain the publishing target root"
    );

    // A cache-format change is an explicit safe miss boundary. The same
    // ActionKey is allowed to be republished only after ordinary rustc work
    // has rebuilt the older entry in the current format.
    let mut old_format = manifest_json.clone();
    old_format["format_version"] = serde_json::Value::from(1);
    fs::write(&manifest, serde_json::to_vec(&old_format).unwrap()).unwrap();
    let old_format_project = registry_project("cas-old-format", "\"1.0.0\"");
    let old_format_output = run_check(
        &old_format_project,
        &paths::root().join("cas-old-format-target"),
        "",
    );
    assert!(
        crate_was_compiled(&old_format_output, REGISTRY_CRATE),
        "an older cache format must be rebuilt instead of restored:\n{}",
        String::from_utf8_lossy(&old_format_output.stderr)
    );
    let rebuilt_manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&manifest).unwrap()).unwrap();
    assert_eq!(rebuilt_manifest["format_version"], 6);

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

    let mut flag_command = flags.cargo("check -vv");
    flag_command
        .arg("--target-dir")
        .arg(paths::root().join("cas-flags-target"))
        .env("RUSTFLAGS", "-C target-cpu=generic");
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
fn effective_config_profile_and_target_inputs_do_not_reuse_cargo_cas_actions() {
    const PACKAGE: &str = "cas-key-configuration-dep";
    const CRATE: &str = "cas_key_configuration_dep";

    registry::init();
    Package::new(PACKAGE, "1.0.0")
        .edition("2024")
        .file(
            "src/lib.rs",
            r#"
#[cfg(cas_config)]
pub const CONFIG: usize = 1;

#[cfg(cas_encoded)]
pub const ENCODED: usize = 1;

#[cfg(not(any(cas_config, cas_encoded)))]
pub const DEFAULT: usize = 1;
"#,
        )
        .publish();

    let manifest = format!(
        r#"[package]
name = "configuration-action-app"
version = "0.1.0"
edition = "2024"

[dependencies]
{PACKAGE} = "1.0.0"
"#,
    );
    let project = project_in("cas-key-configuration")
        .file("Cargo.toml", &manifest)
        .file("src/main.rs", "fn main() {}\n")
        .build();

    let baseline = run_check(
        &project,
        &paths::root().join("cas-key-configuration-baseline-target"),
        "",
    );
    assert!(crate_was_compiled(&baseline, CRATE));

    let config = run_check_with_config(
        &project,
        &paths::root().join("cas-key-configuration-rustflags-target"),
        r#"build.rustflags=["--cfg","cas_config"]"#,
    );
    assert!(
        crate_was_compiled(&config, CRATE),
        "effective build.rustflags from Cargo config must select a distinct action:\n{}",
        String::from_utf8_lossy(&config.stderr)
    );

    let mut encoded_command = project.cargo("check -vv");
    encoded_command
        .arg("--target-dir")
        .arg(paths::root().join("cas-key-configuration-encoded-target"))
        .env("CARGO_ENCODED_RUSTFLAGS", "--cfg\u{1f}cas_encoded");
    let encoded = encoded_command.run();
    assert!(
        crate_was_compiled(&encoded, CRATE),
        "encoded rustflags must select a distinct action:\n{}",
        String::from_utf8_lossy(&encoded.stderr)
    );

    for (label, config) in [
        ("opt-level", "profile.dev.opt-level=1"),
        ("debug", "profile.dev.debug=0"),
        ("debug-assertions", "profile.dev.debug-assertions=false"),
        ("overflow-checks", "profile.dev.overflow-checks=false"),
        ("panic", r#"profile.dev.panic="abort""#),
        ("lto", "profile.dev.lto=true"),
        ("codegen", "profile.dev.codegen-units=1"),
        ("split-debuginfo", r#"profile.dev.split-debuginfo="packed""#),
    ] {
        let output = run_check_with_config(
            &project,
            &paths::root().join(format!("cas-key-configuration-{label}-target")),
            config,
        );
        assert!(
            crate_was_compiled(&output, CRATE),
            "the `{label}` profile input must select a distinct action:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // This changes a rustc diagnostic argument without changing the package
    // source, profile, or RUSTFLAGS. It must still not reuse an entry made
    // without `-Zcargo-lints`: an ActionKey represents the compiler action,
    // not merely the emitted rmeta bytes in this particular fixture.
    let mut cargo_lints = project.cargo("check -Zcargo-lints -vv");
    cargo_lints
        .arg("--target-dir")
        .arg(paths::root().join("cas-key-configuration-cargo-lints-target"))
        .masquerade_as_nightly_cargo(&["cargo-lints"]);
    let cargo_lints_output = cargo_lints.run();
    assert!(
        crate_was_compiled(&cargo_lints_output, CRATE),
        "-Zcargo-lints must select a distinct compiler action:\n{}",
        String::from_utf8_lossy(&cargo_lints_output.stderr)
    );

    // V1 shares only host units. Even an explicit request for the compiler's
    // native target uses Cargo's target compilation role and must fall back to
    // normal work rather than accidentally sharing a host-role cache entry.
    let explicit_target = run_check_for_explicit_host_target(
        &project,
        &paths::root().join("cas-key-configuration-explicit-target"),
    );
    assert!(
        crate_was_compiled(&explicit_target, CRATE),
        "an explicit target role is intentionally ineligible in macOS V1:\n{}",
        String::from_utf8_lossy(&explicit_target.stderr)
    );
}

#[cargo_test]
fn local_path_dependencies_use_normal_rustc_on_a_cold_build() {
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
        "a cold path-source miss must run normal rustc:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cargo_test]
fn cargo_metadata_arguments_keep_root_and_dependency_actions_distinct() {
    let producer = project_in("cas-metadata-producer")
        .file(
            "Cargo.toml",
            r#"[package]
name = "cas-metadata-producer"
version = "0.1.0"
edition = "2024"
"#,
        )
        .file("src/lib.rs", "pub fn answer() -> u32 { 42 }\n")
        .build();
    let consumer = project_in("cas-metadata-consumer")
        .file(
            "Cargo.toml",
            r#"[package]
name = "cas-metadata-consumer"
version = "0.1.0"
edition = "2024"

[dependencies]
cas-metadata-producer = { path = "../../cas-metadata-producer/foo" }
"#,
        )
        .file(
            "src/main.rs",
            "fn main() { println!(\"{}\", cas_metadata_producer::answer()); }\n",
        )
        .build();

    let producer_output = run_build(
        &producer,
        &paths::root().join("cas-metadata-producer-target"),
    );
    assert!(
        crate_was_compiled(&producer_output, "cas_metadata_producer"),
        "the standalone library should populate its own action:\n{}",
        String::from_utf8_lossy(&producer_output.stderr)
    );

    let consumer_output = run_check_with_cas_log(
        &consumer,
        &paths::root().join("cas-metadata-consumer-target"),
    );
    assert!(
        crate_was_compiled(&consumer_output, "cas_metadata_producer"),
        "a different Cargo metadata argument must compile a distinct action:\n{}",
        String::from_utf8_lossy(&consumer_output.stderr)
    );
    let stderr = String::from_utf8_lossy(&consumer_output.stderr);
    assert!(
        !stderr.contains("cargo-cas reject: unexpected artifacts"),
        "different compiler metadata is an action miss, not an artifact-name rejection:\n{stderr}"
    );

    let cache_root = paths::cargo_home().join("cache/cargo-cas-v1");
    let entries = fs::read_dir(&cache_root)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("manifest.json").is_file())
        .count();
    assert_eq!(
        entries, 2,
        "the root and dependency compiler contracts must retain separate immutable entries"
    );
}

#[cargo_test]
fn source_edit_detaches_readonly_cache_backed_output_before_rustc() {
    const CRATE: &str = "cas_hardlink_source_edit";

    let project = project_in("cas-hardlink-source-edit")
        .file(
            "Cargo.toml",
            r#"[package]
name = "cas-hardlink-source-edit"
version = "0.1.0"
edition = "2024"
"#,
        )
        .file("src/lib.rs", "pub const ANSWER: u32 = 1;\n")
        .build();
    let target = paths::root().join("cas-hardlink-source-edit-target");

    let first_output = run_build(&project, &target);
    assert!(crate_was_compiled(&first_output, CRATE));
    let first_manifest = cache_manifest_for_crate(CRATE);
    let first_entry = first_manifest.parent().unwrap().to_path_buf();
    assert_cache_artifacts_share_target(&first_entry, &target);

    let unrelated_codegen = target
        .join("debug")
        .join("unrelated_unit-1234.cgu.0.rcgu.o");
    fs::write(&unrelated_codegen, "ordinary target-local state").unwrap();

    // Rustc normally overwrites this output path. A cache materialization made
    // it read-only and hardlinked, so `Compiler::rustc` must first detach it.
    // The old entry must remain valid after the new source produces a second
    // immutable action.
    fs::write(project.root().join("src/lib.rs"), "pub const ANSWER: u32 = 2;\n").unwrap();
    let edited_output = run_build(&project, &target);
    assert!(
        crate_was_compiled(&edited_output, CRATE),
        "a source edit must compile a new action rather than reuse stale bytes:\n{}",
        String::from_utf8_lossy(&edited_output.stderr)
    );
    assert_valid_cache_entry(&first_entry);
    assert!(
        unrelated_codegen.is_file(),
        "codegen cleanup must not remove an unrelated Cargo output"
    );

    let cache_root = paths::cargo_home().join("cache/cargo-cas-v1");
    let entries = fs::read_dir(&cache_root)
        .unwrap()
        .map(Result::unwrap)
        .map(|entry| entry.path())
        .filter(|entry| entry.join("manifest.json").is_file())
        .filter(|entry| {
            serde_json::from_str::<serde_json::Value>(
                &fs::read_to_string(entry.join("manifest.json")).unwrap(),
            )
            .unwrap()["identity"]["crate_name"]
                .as_str()
                .is_some_and(|name| name == CRATE)
        })
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 2, "the source edit must publish a new action");
    let edited_entry = entries
        .iter()
        .find(|entry| **entry != first_entry)
        .expect("the edited action has a distinct cache entry");
    assert_cache_artifacts_share_target(edited_entry, &target);
}

#[cargo_test]
fn git_worktree_path_dependencies_share_one_cargo_cas_action() {
    let project = project_in("cas-path-worktree-sharing")
        .file(
            "Cargo.toml",
            r#"[package]
name = "cas-path-worktree-app"
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
        .file(
            "local-dependency/build.rs",
            "fn main() { println!(\"cargo::rerun-if-changed=build.rs\"); }\n",
        )
        .file("local-dependency/src/lib.rs", "pub fn answer() {}\n")
        .build();

    let source = project.root();
    run_git(&source, &["init", "--quiet"]);
    run_git(&source, &["add", "."]);
    run_git(
        &source,
        &[
            "-c",
            "user.name=cargo-cas-tests",
            "-c",
            "user.email=cargo-cas-tests@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "initial",
        ],
    );

    let first_root = paths::root().join("cas-path-worktree-sharing-first");
    let second_root = paths::root().join("cas-path-worktree-sharing-second");
    run_git(
        &source,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            first_root.to_str().unwrap(),
        ],
    );
    run_git(
        &source,
        &["worktree", "add", "--quiet", "--detach", second_root.to_str().unwrap()],
    );
    let first_output = run_check_with_cas_log_in(
        &project,
        &first_root,
        &paths::root().join("cas-path-worktree-sharing-first-target"),
    );
    assert!(crate_was_compiled(&first_output, "local_dependency"));

    let second_output = run_check_with_cas_log_in(
        &project,
        &second_root,
        &paths::root().join("cas-path-worktree-sharing-second-target"),
    );
    assert!(
        !crate_was_compiled(&second_output, "local_dependency"),
        "a matching Git worktree path dependency should restore from cargo-cas:\n{}",
        String::from_utf8_lossy(&second_output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&second_output.stderr).contains("cargo-cas hit"),
        "the worktree-shared path action should report a hit:\n{}",
        String::from_utf8_lossy(&second_output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&second_output.stderr).contains("hits=2"),
        "the path package and its replayable build script should both hit across worktrees:\n{}",
        String::from_utf8_lossy(&second_output.stderr)
    );
    run_git(
        &source,
        &["worktree", "remove", "--force", first_root.to_str().unwrap()],
    );
    run_git(
        &source,
        &["worktree", "remove", "--force", second_root.to_str().unwrap()],
    );
}

#[cargo_test]
fn build_script_and_proc_macro_dependency_subgraphs_use_normal_rustc() {
    const BUILD_SCRIPT_PACKAGE: &str = "cas-build-script-dep";
    const BUILD_SCRIPT_CRATE: &str = "cas_build_script_dep";
    const BUILD_SCRIPT_USER_PACKAGE: &str = "cas-build-script-user";
    const BUILD_SCRIPT_USER_CRATE: &str = "cas_build_script_user";
    const PROC_MACRO_PACKAGE: &str = "cas-proc-macro-dep";
    const PROC_MACRO_CRATE: &str = "cas_proc_macro_dep";
    const PROC_MACRO_USER_PACKAGE: &str = "cas-proc-macro-user";
    const PROC_MACRO_USER_CRATE: &str = "cas_proc_macro_user";
    const UNSAFE_BUILD_PACKAGE: &str = "cas-unsafe-build-dep";
    const UNSAFE_BUILD_CRATE: &str = "cas_unsafe_build_dep";

    registry::init();
    Package::new(BUILD_SCRIPT_PACKAGE, "1.0.0")
        .edition("2024")
        .file(
            "build.rs",
            r#"use std::fs;
use std::path::PathBuf;

fn main() {
    let mut generated = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    generated.push("generated.rs");
    fs::write(generated, "pub const GENERATED: u32 = 42;\n").unwrap();
    println!("cargo::rustc-env=CAS_BUILD_SCRIPT_VALUE=42");
    println!("cargo::rerun-if-changed=build.rs");
}
"#,
        )
        .file(
            "src/lib.rs",
            "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));\npub fn answer() -> u32 { GENERATED + env!(\"CAS_BUILD_SCRIPT_VALUE\").parse::<u32>().unwrap() }\n",
        )
        .publish();
    Package::new(BUILD_SCRIPT_USER_PACKAGE, "1.0.0")
        .edition("2024")
        .dep(BUILD_SCRIPT_PACKAGE, "1.0.0")
        .file(
            "src/lib.rs",
            "pub fn answer() { cas_build_script_dep::answer(); }\n",
        )
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
    Package::new(UNSAFE_BUILD_PACKAGE, "1.0.0")
        .edition("2024")
        .links("cas-unsafe-build")
        .file("build.rs", "fn main() { println!(\"cargo::rustc-link-search=/tmp\"); }\n")
        .file("src/lib.rs", "pub fn answer() {}\n")
        .publish();

    let build_script_first =
        registry_dependency_project("cas-build-script-first", BUILD_SCRIPT_USER_PACKAGE);
    let build_script_second =
        registry_dependency_project("cas-build-script-second", BUILD_SCRIPT_USER_PACKAGE);
    let proc_macro_first =
        registry_dependency_project("cas-proc-macro-first", PROC_MACRO_USER_PACKAGE);
    let proc_macro_second =
        registry_dependency_project("cas-proc-macro-second", PROC_MACRO_USER_PACKAGE);
    let unsafe_build_first = registry_dependency_project(
        "cas-unsafe-build-first",
        UNSAFE_BUILD_PACKAGE,
    );
    let unsafe_build_second = registry_dependency_project(
        "cas-unsafe-build-second",
        UNSAFE_BUILD_PACKAGE,
    );

    let build_script_first_output = run_check(
        &build_script_first,
        &paths::root().join("cas-build-script-first-target"),
        "",
    );
    assert!(crate_was_compiled(
        &build_script_first_output,
        BUILD_SCRIPT_CRATE
    ));
    assert!(crate_was_compiled(
        &build_script_first_output,
        BUILD_SCRIPT_USER_CRATE
    ));
    let build_script_manifest = build_script_cache_manifest(BUILD_SCRIPT_PACKAGE);
    let build_script_manifest_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(build_script_manifest).unwrap()).unwrap();
    assert_eq!(build_script_manifest_json["format_version"], 6);
    assert_eq!(build_script_manifest_json["files"].as_array().unwrap().len(), 1);
    assert!(build_script_manifest_json["output"]
        .as_str()
        .unwrap()
        .contains("rustc-env=CAS_BUILD_SCRIPT_VALUE=42"));
    let build_script_second_output = run_check_with_cas_log(
        &build_script_second,
        &paths::root().join("cas-build-script-second-target"),
    );
    assert!(
        !crate_was_compiled(&build_script_second_output, BUILD_SCRIPT_USER_CRATE),
        "a deterministic build-script-affected library should restore from cargo-cas:\n{}",
        String::from_utf8_lossy(&build_script_second_output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&build_script_second_output.stderr)
            .contains("cargo-cas hit"),
        "a build-script-backed cache hit must remain observable:\n{}",
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
    let proc_macro_second_output = run_check_with_cas_log(
        &proc_macro_second,
        &paths::root().join("cas-proc-macro-second-target"),
    );
    assert!(
        crate_was_compiled(&proc_macro_second_output, PROC_MACRO_CRATE)
            && crate_was_compiled(&proc_macro_second_output, PROC_MACRO_USER_CRATE),
        "a proc-macro-affected registry package must remain a normal compile:\n{}",
        String::from_utf8_lossy(&proc_macro_second_output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&proc_macro_second_output.stderr)
            .contains("cargo-cas skip: proc-macro affected"),
        "a transitive proc-macro exclusion must remain observable:\n{}",
        String::from_utf8_lossy(&proc_macro_second_output.stderr)
    );

    let unsafe_build_first_output = run_check(
        &unsafe_build_first,
        &paths::root().join("cas-unsafe-build-first-target"),
        "",
    );
    assert!(crate_was_compiled(&unsafe_build_first_output, UNSAFE_BUILD_CRATE));
    let unsafe_build_second_output = run_check_with_cas_log(
        &unsafe_build_second,
        &paths::root().join("cas-unsafe-build-second-target"),
    );
    assert!(
        crate_was_compiled(&unsafe_build_second_output, UNSAFE_BUILD_CRATE),
        "an unsafe/native build script must remain a normal compile:\n{}",
        String::from_utf8_lossy(&unsafe_build_second_output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&unsafe_build_second_output.stderr)
            .contains("cargo-cas skips non-replayable build-script output"),
        "the unsafe build-script skip must be explicit:\n{}",
        String::from_utf8_lossy(&unsafe_build_second_output.stderr)
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
    assert!(
        String::from_utf8_lossy(&first_output.stderr).contains("misses=1"),
        "one initial lookup must count as one miss, not a coordination recheck:\n{}",
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

    let path_first_output = run_check_with_cas_log(
        &path_source,
        &paths::root().join("cas-observability-path-target"),
    );
    assert!(crate_was_compiled(&path_first_output, "local_dependency"));
    assert!(
        String::from_utf8_lossy(&path_first_output.stderr).contains("cargo-cas miss"),
        "a local path unit must participate in the cache on a cold build:\n{}",
        String::from_utf8_lossy(&path_first_output.stderr)
    );
    let path_second_output = run_check_with_cas_log(
        &path_source,
        &paths::root().join("cas-observability-path-second-target"),
    );
    assert!(
        !crate_was_compiled(&path_second_output, "local_dependency"),
        "an unchanged local path unit should restore from cargo-cas:\n{}",
        String::from_utf8_lossy(&path_second_output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&path_second_output.stderr).contains("cargo-cas hit"),
        "a local path hit should be observable in the cache trace:\n{}",
        String::from_utf8_lossy(&path_second_output.stderr)
    );
    fs::write(
        path_source
            .root()
            .join("local-dependency/src/lib.rs"),
        "pub fn answer() { let _ = 1u8; }\n",
    )
    .unwrap();
    let path_changed_output = run_check_with_cas_log(
        &path_source,
        &paths::root().join("cas-observability-path-changed-target"),
    );
    assert!(
        crate_was_compiled(&path_changed_output, "local_dependency"),
        "a local path source mutation must invalidate its cache action:\n{}",
        String::from_utf8_lossy(&path_changed_output.stderr)
    );
}

#[cargo_test]
fn cache_hit_replays_cached_dependency_diagnostics() {
    const PACKAGE: &str = "cas-diagnostic-replay-dep";
    const CRATE: &str = "cas_diagnostic_replay_dep";

    registry::init();
    Package::new(PACKAGE, "1.0.0")
        .edition("2024")
        .file(
            "src/lib.rs",
            "#![warn(dead_code)]\nfn deliberately_unused() {}\npub fn answer() {}\n",
        )
        .publish();
    let manifest = format!(
        r#"[package]
name = "cas-diagnostic-replay-app"
version = "0.1.0"
edition = "2024"

[dependencies]
{PACKAGE} = "1.0.0"
"#,
    );
    let first = project_in("cas-diagnostic-replay-first")
        .file("Cargo.toml", &manifest)
        .file(
            "src/main.rs",
            "fn main() { cas_diagnostic_replay_dep::answer(); }\n",
        )
        .build();
    let second = project_in("cas-diagnostic-replay-second")
        .file("Cargo.toml", &manifest)
        .file(
            "src/main.rs",
            "fn main() { cas_diagnostic_replay_dep::answer(); }\n",
        )
        .build();

    let first_output = run_check(
        &first,
        &paths::root().join("cas-diagnostic-replay-first-target"),
        "",
    );
    assert!(crate_was_compiled(&first_output, CRATE));
    assert!(
        String::from_utf8_lossy(&first_output.stderr).contains("deliberately_unused"),
        "the cold compiler invocation must emit the dependency warning:\n{}",
        String::from_utf8_lossy(&first_output.stderr)
    );
    let manifest_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(cache_manifest()).unwrap()).unwrap();
    assert!(
        manifest_json["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|artifact| artifact["role"] == "output-cache"),
        "a diagnostic-producing action must publish its output cache"
    );

    let second_output = run_check(
        &second,
        &paths::root().join("cas-diagnostic-replay-second-target"),
        "",
    );
    assert!(
        !crate_was_compiled(&second_output, CRATE),
        "a diagnostic cache hit must not re-run dependency rustc:\n{}",
        String::from_utf8_lossy(&second_output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&second_output.stderr).contains("deliberately_unused"),
        "a diagnostic cache hit must replay the dependency warning:\n{}",
        String::from_utf8_lossy(&second_output.stderr)
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
    let manifest_json: serde_json::Value = serde_json::from_str(&manifest_text).unwrap();
    let roles = manifest_json["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|artifact| artifact["role"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        roles,
        ["rmeta", "linkable", "dep-info"],
        "a cache hit must restore metadata before linkable and local bookkeeping files"
    );
    let cache_entry = manifest.parent().unwrap();
    assert_cache_artifacts_share_target(cache_entry, &first_target);

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
    assert_cache_artifacts_share_target(cache_entry, &second_target);

    // A dependency hit must also preserve Cargo's ordinary artifact export
    // path. The root binary is always built and exported locally; only the
    // immutable dependency artifacts are restored from cargo-cas.
    let artifact_target = paths::root().join("cas-build-artifact-dir-target");
    let artifact_dir = paths::root().join("cas-build-artifact-dir-export");
    let mut artifact_command = second.cargo("build -Zunstable-options -vv");
    artifact_command
        .arg("--target-dir")
        .arg(&artifact_target)
        .arg("--artifact-dir")
        .arg(&artifact_dir)
        .masquerade_as_nightly_cargo(&["unstable-options"]);
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

    // A target directory is purely a materialization. Removing it must not
    // affect a valid cache entry, and the next CAS build should restore the
    // dependency without compiling it again.
    fs::remove_dir_all(&second_target).unwrap();
    let after_target_removal = run_build(&second, &second_target);
    assert!(
        !crate_was_compiled(&after_target_removal, BUILD_CRATE),
        "removing a target directory must still permit a dependency cache hit:\n{}",
        String::from_utf8_lossy(&after_target_removal.stderr)
    );
    assert_cache_artifacts_share_target(cache_entry, &second_target);

    // The materialized artifacts and normal fingerprint state are also usable
    // by any Cargo invocation, including a binary without the cache. This
    // guards the scheduler boundary: a cache hit is normal local Cargo state,
    // not a separate freshness regime.
    let normal_output = run_build(&second, &second_target);
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

    // A target owns a hardlink, not a reference which becomes dangling when
    // explicit cache GC or a user removes the backing entry. Normal Cargo
    // must therefore still accept the target's complete local fingerprint and
    // artifact state after the cache itself is gone.
    fs::remove_dir_all(cache_entry).unwrap();
    let after_cache_removal = run_build(&second, &second_target);
    assert!(
        !crate_was_compiled(&after_cache_removal, BUILD_CRATE),
        "removing a cache entry must not invalidate a materialized target:\n{}",
        String::from_utf8_lossy(&after_cache_removal.stderr)
    );
}

#[cargo_test]
fn cache_hit_releases_pipelined_dependents_after_rmeta_materialization() {
    const PACKAGE: &str = "cas-pipeline-dep";
    const CRATE: &str = "cas_pipeline_dep";
    const ROOT_CRATE: &str = "cas_pipeline_second";

    registry::init();
    Package::new(PACKAGE, "1.0.0")
        .edition("2024")
        .file("src/lib.rs", "pub fn answer() -> u32 { 42 }\n")
        .publish();

    let manifest = format!(
        r#"[package]
name = "cas-pipeline-second"
version = "0.1.0"
edition = "2024"

[dependencies]
{PACKAGE} = "1.0.0"
"#,
    );
    let first = project_in("cas-pipeline-first")
        .file("Cargo.toml", &manifest)
        .file(
            "src/lib.rs",
            "pub fn answer() -> u32 { cas_pipeline_dep::answer() }\n",
        )
        .file(
            "src/main.rs",
            "fn main() { println!(\"{}\", cas_pipeline_second::answer()); }\n",
        )
        .build();
    let second = project_in("cas-pipeline-second")
        .file("Cargo.toml", &manifest)
        .file(
            "src/lib.rs",
            "pub fn answer() -> u32 { cas_pipeline_dep::answer() + 1 }\n",
        )
        .file(
            "src/main.rs",
            "fn main() { println!(\"{}\", cas_pipeline_second::answer()); }\n",
        )
        .build();

    // The cache key includes the canonical RUSTC executable, so populate and
    // consume the entry with the same transparent proxy.  The release file
    // exists from the start; it makes this proxy an observer, not a source of
    // artificial scheduling delay.
    let rustc = gated_rustc("cas-pipeline-rustc");
    let log = paths::root().join("cas-pipeline-rustc.log");
    let release = paths::root().join("cas-pipeline-rustc.release");
    fs::write(&release, "observe only").unwrap();
    let first_output = run_build_with_rustc(
        &first,
        &paths::root().join("cas-pipeline-first-target"),
        &rustc,
        ROOT_CRATE,
        &log,
        &release,
    );
    assert!(crate_was_compiled(&first_output, CRATE));
    assert!(cache_manifest().is_file());
    fs::remove_file(&log).unwrap_or(());

    let pause_signal = paths::root().join("cas-pipeline-rmeta-ready");
    let child = start_build_paused_after_cas_rmeta(
        &second,
        &paths::root().join("cas-pipeline-second-target"),
        &rustc,
        ROOT_CRATE,
        &log,
        &release,
        &pause_signal,
    );
    let rmeta_ready = wait_for_path(&pause_signal);
    let dependent_started = rmeta_ready && wait_for_log_line(&log, ROOT_CRATE);
    // Always release the child before asserting. A failed scheduler
    // regression must fail the test without leaking a paused Cargo process
    // into the rest of the test suite.
    if pause_signal.exists() {
        fs::remove_file(&pause_signal).unwrap();
    }
    let output = wait_for_child(child);
    assert!(
        rmeta_ready,
        "the cache hit did not reach its rmeta-ready boundary"
    );
    assert!(
        dependent_started,
        "the dependent rustc did not start while cache transport was paused"
    );
    assert!(output.status.success(), "{output:?}");
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains(&format!("--crate-name {CRATE}")),
        "the cached dependency must not run rustc:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cargo_test]
fn cache_restore_falls_back_after_metadata_without_duplicate_pipeline_edges() {
    const PACKAGE: &str = "cas-restore-fallback-dep";
    const CRATE: &str = "cas_restore_fallback_dep";
    const ROOT_CRATE: &str = "cas_restore_fallback_second";

    registry::init();
    Package::new(PACKAGE, "1.0.0")
        .edition("2024")
        .file("src/lib.rs", "pub fn answer() -> u32 { 42 }\n")
        .publish();
    let manifest = format!(
        r#"[package]
name = "cas-restore-fallback-second"
version = "0.1.0"
edition = "2024"

[dependencies]
{PACKAGE} = "1.0.0"
"#,
    );
    let first = project_in("cas-restore-fallback-first")
        .file("Cargo.toml", &manifest)
        .file(
            "src/lib.rs",
            "pub fn answer() -> u32 { cas_restore_fallback_dep::answer() }\n",
        )
        .build();
    let second = project_in("cas-restore-fallback-second")
        .file("Cargo.toml", &manifest)
        .file(
            "src/lib.rs",
            "pub fn answer() -> u32 { cas_restore_fallback_dep::answer() + 1 }\n",
        )
        .build();

    let rustc = gated_rustc("cas-restore-fallback-rustc");
    let log = paths::root().join("cas-restore-fallback-rustc.log");
    let release = paths::root().join("cas-restore-fallback-rustc.release");
    fs::write(&release, "observe only").unwrap();
    let first_output = run_build_with_rustc(
        &first,
        &paths::root().join("cas-restore-fallback-first-target"),
        &rustc,
        ROOT_CRATE,
        &log,
        &release,
    );
    assert!(crate_was_compiled(&first_output, CRATE));
    let cache_manifest_path = cache_manifest_for_crate(CRATE);
    let manifest_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cache_manifest_path).unwrap()).unwrap();
    let linkable = manifest_json["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["role"] == "linkable")
        .expect("a build cache entry includes its linkable artifact");
    let linkable_path = cache_manifest_path
        .parent()
        .unwrap()
        .join("artifacts")
        .join(linkable["file"].as_str().unwrap());
    fs::remove_file(&log).unwrap_or(());

    let pause_signal = paths::root().join("cas-restore-fallback-rmeta-ready");
    let child = start_build_paused_after_cas_rmeta(
        &second,
        &paths::root().join("cas-restore-fallback-second-target"),
        &rustc,
        ROOT_CRATE,
        &log,
        &release,
        &pause_signal,
    );
    assert!(wait_for_path(&pause_signal));
    // The manifest was valid at lookup. Simulate a concurrent cache file loss
    // after the cache hit has released its metadata pipeline edge.
    fs::remove_file(&linkable_path).unwrap();
    fs::remove_file(&pause_signal).unwrap();
    let output = wait_for_child(child);
    assert!(
        output.status.success(),
        "a late cache restore failure must fall back to rustc: {output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .any(|line| line.contains("rustc") && line.contains(&format!("--crate-name {CRATE}"))),
        "the missing linkable artifact must trigger ordinary dependency rustc:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cargo_test]
fn cargo_cas_reuses_across_separate_build_directories() {
    const PACKAGE: &str = "cas-build-dir-dep";
    const CRATE: &str = "cas_build_dir_dep";

    registry::init();
    Package::new(PACKAGE, "1.0.0")
        .edition("2024")
        .file("src/lib.rs", "pub fn answer() {}\n")
        .publish();
    let manifest = format!(
        r#"[package]
name = "cas-build-dir-app"
version = "0.1.0"
edition = "2024"

[dependencies]
{PACKAGE} = "1.0.0"
"#,
    );
    let first = project_in("cas-build-dir-first")
        .file("Cargo.toml", &manifest)
        .file(
            "src/main.rs",
            "fn main() { cas_build_dir_dep::answer(); }\n",
        )
        .build();
    let second = project_in("cas-build-dir-second")
        .file("Cargo.toml", &manifest)
        .file(
            "src/main.rs",
            "fn main() { cas_build_dir_dep::answer(); }\n",
        )
        .build();

    let first_output = run_check(
        &first,
        &paths::root().join("cas-build-dir-first-target"),
        "",
    );
    assert!(crate_was_compiled(&first_output, CRATE));

    let separate_build_dir = paths::root().join("cas-build-dir-second-intermediates");
    let second_output = run_check_with_config(
        &second,
        &paths::root().join("cas-build-dir-second-target"),
        &format!(r#"build.build-dir="{}""#, separate_build_dir.display()),
    );
    assert!(
        !crate_was_compiled(&second_output, CRATE),
        "a workspace-local build-dir path must not prevent a matching global action hit:\n{}",
        String::from_utf8_lossy(&second_output.stderr)
    );
    assert!(
        separate_build_dir.is_dir(),
        "the configured build-dir must receive the restored local compiler state"
    );
}

#[cargo_test]
fn cargo_clean_keeps_global_cargo_cas_entries() {
    const PACKAGE: &str = "cas-clean-dep";
    const CRATE: &str = "cas_clean_dep";

    registry::init();
    Package::new(PACKAGE, "1.0.0")
        .edition("2024")
        .file("src/lib.rs", "pub fn answer() {}\n")
        .publish();
    let manifest = format!(
        r#"[package]
name = "cas-clean-app"
version = "0.1.0"
edition = "2024"

[dependencies]
{PACKAGE} = "1.0.0"
"#,
    );
    let first = project_in("cas-clean-first")
        .file("Cargo.toml", &manifest)
        .file("src/main.rs", "fn main() { cas_clean_dep::answer(); }\n")
        .build();
    let second = project_in("cas-clean-second")
        .file("Cargo.toml", &manifest)
        .file("src/main.rs", "fn main() { cas_clean_dep::answer(); }\n")
        .build();

    let first_target = paths::root().join("cas-clean-first-target");
    let first_output = run_check(&first, &first_target, "");
    assert!(crate_was_compiled(&first_output, CRATE));
    let entry_manifest = cache_manifest();

    run_clean(&first, &first_target);
    assert!(
        entry_manifest.is_file(),
        "ordinary cargo clean must only remove local build state, not the global immutable entry"
    );

    let second_output = run_check(&second, &paths::root().join("cas-clean-second-target"), "");
    assert!(
        !crate_was_compiled(&second_output, CRATE),
        "an entry retained across cargo clean must remain reusable:\n{}",
        String::from_utf8_lossy(&second_output.stderr)
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
    let corrupt_artifact = entry.join("artifacts/0");
    let mut corrupt_permissions = fs::metadata(&corrupt_artifact).unwrap().permissions();
    corrupt_permissions.set_readonly(false);
    fs::set_permissions(&corrupt_artifact, corrupt_permissions).unwrap();
    fs::write(&corrupt_artifact, b"corrupt artifact").unwrap();
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

    let mut truncated_permissions = fs::metadata(&artifact).unwrap().permissions();
    truncated_permissions.set_readonly(false);
    fs::set_permissions(&artifact, truncated_permissions).unwrap();
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
fn unavailable_or_substituted_cache_root_falls_back_without_escaping_cargo_home() {
    const PACKAGE: &str = "cas-cache-root-fallback-dep";
    const CRATE: &str = "cas_cache_root_fallback_dep";

    registry::init();
    Package::new(PACKAGE, "1.0.0")
        .edition("2024")
        .file("src/lib.rs", "pub fn answer() {}\n")
        .publish();
    let manifest = format!(
        r#"[package]
name = "cas-cache-root-fallback-app"
version = "0.1.0"
edition = "2024"

[dependencies]
{PACKAGE} = "1.0.0"
"#,
    );
    let unavailable = project_in("cas-cache-root-unavailable")
        .file("Cargo.toml", &manifest)
        .file(
            "src/main.rs",
            "fn main() { cas_cache_root_fallback_dep::answer(); }\n",
        )
        .build();
    let recovered = project_in("cas-cache-root-recovered")
        .file("Cargo.toml", &manifest)
        .file(
            "src/main.rs",
            "fn main() { cas_cache_root_fallback_dep::answer(); }\n",
        )
        .build();
    let hit = project_in("cas-cache-root-hit")
        .file("Cargo.toml", &manifest)
        .file(
            "src/main.rs",
            "fn main() { cas_cache_root_fallback_dep::answer(); }\n",
        )
        .build();

    let cache = paths::cargo_home().join("cache/cargo-cas-v1");
    fs::create_dir_all(cache.parent().unwrap()).unwrap();
    let outside = paths::root().join("outside-cargo-cas-root");
    fs::create_dir_all(&outside).unwrap();
    symlink(&outside, &cache).unwrap();

    let mut malformed_gc = unavailable.cargo("clean gc -Zgc --max-cas-size=0");
    malformed_gc.masquerade_as_nightly_cargo(&["gc"]);
    let malformed_gc_output = malformed_gc.build_command().output().unwrap();
    assert!(
        !malformed_gc_output.status.success(),
        "GC must reject a substituted cache root instead of traversing it"
    );
    assert!(
        String::from_utf8_lossy(&malformed_gc_output.stderr)
            .contains("cargo-cas cache root is not a directory"),
        "GC should clearly identify the malformed cache root:\n{}",
        String::from_utf8_lossy(&malformed_gc_output.stderr)
    );
    assert!(fs::read_dir(&outside).unwrap().next().is_none());

    let unavailable_output = run_check(
        &unavailable,
        &paths::root().join("cas-cache-root-unavailable-target"),
        "",
    );
    assert!(
        crate_was_compiled(&unavailable_output, CRATE),
        "an unavailable cache root must fall back to normal rustc:\n{}",
        String::from_utf8_lossy(&unavailable_output.stderr)
    );
    assert!(
        fs::read_dir(&outside).unwrap().next().is_none(),
        "cache locks, staging, and artifacts must never be created through a substituted root"
    );

    fs::remove_file(&cache).unwrap();
    let recovered_output = run_check(
        &recovered,
        &paths::root().join("cas-cache-root-recovered-target"),
        "",
    );
    assert!(crate_was_compiled(&recovered_output, CRATE));
    let hit_output = run_check(&hit, &paths::root().join("cas-cache-root-hit-target"), "");
    assert!(
        !crate_was_compiled(&hit_output, CRATE),
        "a recovered ordinary cache root must publish a reusable entry:\n{}",
        String::from_utf8_lossy(&hit_output.stderr)
    );
}

#[cargo_test]
fn cache_internal_directories_never_follow_substituted_symlinks() {
    const LOCK_PACKAGE: &str = "cas-internal-lock-dep";
    const LOCK_CRATE: &str = "cas_internal_lock_dep";
    const TMP_PACKAGE: &str = "cas-internal-tmp-dep";
    const TMP_CRATE: &str = "cas_internal_tmp_dep";
    const STAGE_PACKAGE: &str = "cas-internal-stage-dep";
    const STAGE_CRATE: &str = "cas_internal_stage_dep";

    registry::init();
    for package in [LOCK_PACKAGE, TMP_PACKAGE, STAGE_PACKAGE] {
        Package::new(package, "1.0.0")
            .edition("2024")
            .file("src/lib.rs", "pub fn answer() {}\n")
            .publish();
    }
    let seed = registry_dependency_project("cas-internal-seed", LOCK_PACKAGE);
    let lock_project = registry_dependency_project("cas-internal-lock", TMP_PACKAGE);
    let tmp_project = registry_dependency_project("cas-internal-tmp", STAGE_PACKAGE);
    let seed_output = run_check(&seed, &paths::root().join("cas-internal-seed-target"), "");
    assert!(crate_was_compiled(&seed_output, LOCK_CRATE));

    let cache = paths::cargo_home().join("cache/cargo-cas-v1");
    let outside = paths::root().join("cas-internal-outside");
    fs::create_dir_all(&outside).unwrap();

    let locks = cache.join("locks");
    fs::remove_dir_all(&locks).unwrap();
    symlink(&outside, &locks).unwrap();
    let lock_output = run_check(
        &lock_project,
        &paths::root().join("cas-internal-lock-target"),
        "",
    );
    assert!(
        crate_was_compiled(&lock_output, TMP_CRATE),
        "a substituted lock directory must fall back to normal rustc:\n{}",
        String::from_utf8_lossy(&lock_output.stderr)
    );
    assert!(
        fs::read_dir(&outside).unwrap().next().is_none(),
        "cache lock creation must not write through a symlink"
    );

    fs::remove_file(&locks).unwrap();
    fs::create_dir(&locks).unwrap();
    let temporary = cache.join("tmp");
    fs::remove_dir_all(&temporary).unwrap();
    symlink(&outside, &temporary).unwrap();
    let tmp_output = run_check(
        &tmp_project,
        &paths::root().join("cas-internal-tmp-target"),
        "",
    );
    assert!(
        crate_was_compiled(&tmp_output, STAGE_CRATE),
        "a substituted staging directory must leave a successful normal build:\n{}",
        String::from_utf8_lossy(&tmp_output.stderr)
    );
    assert!(
        fs::read_dir(&outside).unwrap().next().is_none(),
        "cache staging must not write through a symlink"
    );

    fs::remove_file(&temporary).unwrap();
    fs::create_dir(&temporary).unwrap();
    let access = cache.join("access");
    fs::remove_dir_all(&access).unwrap();
    symlink(&outside, &access).unwrap();
    let access_output = run_check(&seed, &paths::root().join("cas-internal-access-target"), "");
    assert!(
        !crate_was_compiled(&access_output, LOCK_CRATE),
        "an unavailable access directory must not discard a valid cache hit:\n{}",
        String::from_utf8_lossy(&access_output.stderr)
    );
    assert!(
        fs::read_dir(&outside).unwrap().next().is_none(),
        "last-use tracking must not write through a substituted directory"
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

    // Cache readers use separate target directories, so all eight roots still
    // need normal local scheduling work.  None may invoke rustc for the
    // already-published shared dependency, even while the readers overlap.
    let reader_outputs = worktrees
        .iter()
        .enumerate()
        .map(|(index, worktree)| {
            start_gated_check_in_dir(
                &driver,
                worktree,
                &paths::root().join(format!("cas-gate-six-reader-target-{index}")),
                &rustc,
                CRATE,
                &log,
                &release,
            )
        })
        .map(wait_for_child)
        .collect::<Vec<_>>();
    assert!(
        reader_outputs.iter().all(|output| output.status.success()),
        "one of the concurrent cache-reader builds failed: {reader_outputs:#?}"
    );
    assert!(
        reader_outputs
            .iter()
            .all(|output| { String::from_utf8_lossy(&output.stderr).contains("cargo-cas hit") }),
        "every reader must observe a verified cache hit: {reader_outputs:#?}"
    );
    assert_eq!(
        fs::read_to_string(&log).unwrap().lines().count(),
        1,
        "concurrent cache readers must not rebuild the shared action: {}",
        fs::read_to_string(&log).unwrap_or_default()
    );

    let cache_root = paths::cargo_home().join("cache/cargo-cas-v1");
    let cache_entries = fs::read_dir(&cache_root)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("manifest.json").is_file())
        .inspect(|entry| assert_valid_cache_entry(&entry.path()))
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

    // A process killed before atomic publication can leave a staged directory
    // behind, and per-key lock files are intentionally created lazily. An
    // explicit GC policy owns the package-cache mutation lock, so it must
    // account for and clean both rather than leaving bytes outside the size
    // policy indefinitely.
    let abandoned = cache_root.join("tmp/abandoned-publication");
    fs::create_dir_all(&abandoned).unwrap();
    fs::write(abandoned.join("artifact"), b"abandoned cache bytes").unwrap();
    let stale_lock = cache_root.join("locks/stale-action.lock");
    fs::write(&stale_lock, b"stale lock").unwrap();

    // An explicit size policy removes entire entries. The next use is an
    // ordinary cache miss and must rebuild rather than observing partial
    // state.
    run_cas_gc(&first, "--max-cas-size=0");
    assert!(
        !first_manifest.exists(),
        "the zero-size policy must evict the immutable entry"
    );
    assert!(!first_access.exists());
    assert!(!abandoned.exists(), "GC must remove abandoned staging");
    assert!(!stale_lock.exists(), "GC must remove inactive CAS locks");
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
