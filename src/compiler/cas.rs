//! The experimental immutable registry artifact cache.
//!
//! This deliberately materializes a verified cache entry into Cargo's normal
//! build directory.  The local [`fingerprint`](super::fingerprint) and job
//! queue therefore remain the authority for scheduling and freshness; this
//! module only substitutes the work normally performed by `rustc`.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use cargo_util::paths;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use super::fingerprint;
use super::job_queue::Work;
use super::{BuildRunner, CompileMode, FileFlavor, Lto, Unit};
use crate::util::CargoResult;

const CACHE_FORMAT_VERSION: u8 = 0;
const CACHE_DIRECTORY: &str = "cargo-cas-v0";
const MANIFEST_FILE: &str = "manifest.json";
const ARTIFACTS_DIRECTORY: &str = "artifacts";

static TEMPORARY_ENTRY_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A collision-resistant identity for a pre-compilation action.
///
/// The hash is derived from [`CacheKeyInputV0`]'s deliberate, versioned JSON
/// representation.  It is not Cargo's local fingerprint or its output-name
/// metadata hash: both have different lifetime and invalidation contracts.
#[derive(Clone, Debug)]
pub(crate) struct ActionKey(String);

impl ActionKey {
    fn as_str(&self) -> &str {
        &self.0
    }
}

/// A globally stored entry whose files have passed manifest validation.
#[derive(Clone, Debug)]
pub(crate) struct CacheEntry {
    root: PathBuf,
    manifest: CacheManifestV0,
}

/// The stable inputs needed to publish after the `rustc` work has completed.
/// It owns only paths and an already-calculated key so it can safely outlive
/// the mutable [`BuildRunner`] borrow used to construct the compiler job.
#[derive(Clone)]
pub(crate) struct CachePublication {
    key: ActionKey,
    cache: PathBuf,
    package_root: PathBuf,
    build_root: PathBuf,
    artifacts: Vec<ArtifactPath>,
}

#[derive(Serialize)]
struct CacheKeyInputV0<'a> {
    format_version: u8,
    package: PackageInput<'a>,
    target: TargetInput,
    mode: &'static str,
    profile: &'a crate::workspace::profiles::Profile,
    lto: LtoInput,
    rustc_verbose_version: &'a str,
    rustflags: &'a [String],
    extra_args: &'a [String],
    features: Vec<String>,
    dependencies: Vec<DependencyInput>,
}

#[derive(Serialize)]
struct PackageInput<'a> {
    name: &'a str,
    version: String,
    source: String,
    checksum: &'a str,
}

#[derive(Serialize)]
struct TargetInput {
    name: String,
    crate_name: String,
    source_path: String,
    crate_types: Vec<String>,
    compile_kind: &'static str,
}

#[derive(Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct DependencyInput {
    action_key: String,
    extern_crate_name: String,
    public: bool,
    noprelude: bool,
    nounused: bool,
}

#[derive(Serialize)]
enum LtoInput {
    Run(Option<String>),
    Off,
    OnlyBitcode,
    ObjectAndBitcode,
    OnlyObject,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CacheManifestV0 {
    format_version: u8,
    action_key: String,
    artifacts: Vec<CachedArtifact>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CachedArtifact {
    role: ArtifactRole,
    file: String,
    output_file_name: String,
    size: u64,
    digest: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ArtifactRole {
    Rmeta,
    DepInfo,
}

#[derive(Clone)]
struct ArtifactPath {
    role: ArtifactRole,
    source: PathBuf,
    destination: PathBuf,
}

/// Finds a complete, valid cache entry for an eligible dirty unit.
///
/// All lookup failures are cache misses.  Cargo can still compile the unit in
/// the normal way, which is safer than allowing cache infrastructure damage to
/// prevent a valid build.
pub(crate) fn lookup(
    build_runner: &BuildRunner<'_, '_>,
    unit: &Unit,
) -> CargoResult<Option<CacheEntry>> {
    let Some(key) = action_key(build_runner, unit) else {
        return Ok(None);
    };
    let root = cache_root(build_runner).join(key.as_str());
    let manifest_path = root.join(MANIFEST_FILE);
    let Ok(manifest_bytes) = fs::read(&manifest_path) else {
        return Ok(None);
    };
    let Ok(manifest) = serde_json::from_slice::<CacheManifestV0>(&manifest_bytes) else {
        warn!(path = %manifest_path.display(), "ignoring malformed cargo-cas cache manifest");
        return Ok(None);
    };

    if manifest.format_version != CACHE_FORMAT_VERSION || manifest.action_key != key.as_str() {
        warn!(path = %manifest_path.display(), "ignoring incompatible cargo-cas cache manifest");
        return Ok(None);
    }
    if !validate_manifest(&root, &manifest) {
        warn!(path = %manifest_path.display(), "ignoring corrupt cargo-cas cache entry");
        return Ok(None);
    }
    let expected = artifact_paths(build_runner, unit)?;
    if !manifest_matches_expected(&manifest, &expected) {
        warn!(path = %manifest_path.display(), "ignoring cargo-cas entry with unexpected artifacts");
        return Ok(None);
    }

    Ok(Some(CacheEntry { root, manifest }))
}

/// Returns work that materializes a validated entry at Cargo's usual output
/// paths.  This leaves `extern_args`, `-L` construction, local fingerprints,
/// and final artifact uplift unchanged.
pub(crate) fn restore_work(
    build_runner: &BuildRunner<'_, '_>,
    unit: &Unit,
    entry: CacheEntry,
) -> CargoResult<Work> {
    let expected = artifact_paths(build_runner, unit)?;
    debug_assert!(manifest_matches_expected(&entry.manifest, &expected));

    let restores = entry
        .manifest
        .artifacts
        .iter()
        .zip(expected)
        .map(|(cached, expected)| {
            (
                entry.root.join(ARTIFACTS_DIRECTORY).join(&cached.file),
                expected.destination,
                cached.role,
            )
        })
        .collect::<Vec<_>>();

    Ok(Work::new(move |state| {
        let mut restored_rmeta = false;
        for (source, destination, role) in restores {
            let parent = destination.parent().expect("Cargo output path has parent");
            paths::create_dir_all(parent)?;
            // Cache entries are immutable.  Copy rather than hardlink so a
            // later local output cleanup or compiler invocation can never
            // mutate a globally cached inode.
            fs::copy(source, destination)?;
            if role == ArtifactRole::Rmeta {
                restored_rmeta = true;
            }
        }
        if restored_rmeta {
            state.rmeta_produced();
        }
        Ok(())
    }))
}

/// Captures the immutable state required to publish a successful ordinary
/// compilation after its `rustc` job has completed.
pub(crate) fn publication(
    build_runner: &BuildRunner<'_, '_>,
    unit: &Unit,
) -> CargoResult<Option<CachePublication>> {
    let Some(key) = action_key(build_runner, unit) else {
        return Ok(None);
    };
    Ok(Some(CachePublication {
        key,
        cache: cache_root(build_runner),
        package_root: unit.pkg.root().to_path_buf(),
        build_root: build_runner.bcx.ws.build_dir().into_path_unlocked(),
        artifacts: artifact_paths(build_runner, unit)?,
    }))
}

impl CachePublication {
    /// Stages and atomically publishes the rmeta plus local dep-info produced
    /// by a successful ordinary Cargo compilation.
    ///
    /// Publication is intentionally best-effort.  A cache miss is only a
    /// performance cost, so I/O errors here must not turn a successful compile
    /// into a failed one.
    pub(crate) fn publish(&self) {
        if let Err(error) = self.publish_inner() {
            warn!(error = ?error, "failed to publish cargo-cas entry; continuing without cache");
        }
    }

    fn publish_inner(&self) -> CargoResult<()> {
        // Cargo records any `env!`/`option_env!` inputs in its translated dep-info.
        // V0 does not yet have a pre-compilation identity for arbitrary inherited
        // environment, so such units are deliberately not published.
        let dep_info = self
            .artifacts
            .iter()
            .find(|artifact| artifact.role == ArtifactRole::DepInfo)
            .expect("cargo-cas publication includes dep-info")
            .source
            .clone();
        let Some(dep_info_contents) =
            fingerprint::parse_dep_info(&self.package_root, &self.build_root, &dep_info)?
        else {
            return Ok(());
        };
        if !dep_info_contents.env.is_empty() {
            debug!("cargo-cas skips unit with fingerprinted environment");
            return Ok(());
        }

        if self
            .artifacts
            .iter()
            .any(|artifact| !artifact.source.is_file())
        {
            return Ok(());
        }

        let final_entry = self.cache.join(self.key.as_str());
        if final_entry.exists() {
            return Ok(());
        }

        let temporary_entry = self.cache.join("tmp").join(format!(
            "{}-{}-{}",
            self.key.as_str(),
            std::process::id(),
            TEMPORARY_ENTRY_COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        let temporary_artifacts = temporary_entry.join(ARTIFACTS_DIRECTORY);
        paths::create_dir_all(&temporary_artifacts)?;

        let mut manifest_artifacts = Vec::with_capacity(self.artifacts.len());
        for (index, artifact) in self.artifacts.iter().enumerate() {
            let cache_name = index.to_string();
            let staged = temporary_artifacts.join(&cache_name);
            fs::copy(&artifact.source, &staged)?;
            let metadata = fs::metadata(&staged)?;
            let digest = digest_file(&staged)?;
            manifest_artifacts.push(CachedArtifact {
                role: artifact.role,
                file: cache_name,
                output_file_name: file_name(&artifact.destination),
                size: metadata.len(),
                digest,
            });
        }

        let manifest = CacheManifestV0 {
            format_version: CACHE_FORMAT_VERSION,
            action_key: self.key.0.clone(),
            artifacts: manifest_artifacts,
        };
        paths::write_atomic(
            temporary_entry.join(MANIFEST_FILE),
            serde_json::to_vec(&manifest)?,
        )?;

        // `tmp` and the final entry are both below the same cache root, so rename
        // makes a completed entry visible atomically.  If another writer won the
        // race, its immutable entry is equally valid and ours is discarded.
        if let Err(error) = fs::rename(&temporary_entry, &final_entry)
            && error.kind() != io::ErrorKind::AlreadyExists
        {
            return Err(error.into());
        }
        if temporary_entry.exists() {
            let _ = fs::remove_dir_all(&temporary_entry);
        }
        Ok(())
    }
}

fn action_key(build_runner: &BuildRunner<'_, '_>, unit: &Unit) -> Option<ActionKey> {
    let mut visiting = BTreeSet::new();
    action_key_inner(build_runner, unit, &mut visiting)
}

fn action_key_inner(
    build_runner: &BuildRunner<'_, '_>,
    unit: &Unit,
    visiting: &mut BTreeSet<Unit>,
) -> Option<ActionKey> {
    if !eligible_without_dependencies(build_runner, unit) || !visiting.insert(unit.clone()) {
        return None;
    }

    let mut dependencies = build_runner
        .unit_deps(unit)
        .iter()
        .map(|dependency| {
            let action_key = action_key_inner(build_runner, &dependency.unit, visiting)?;
            Some(DependencyInput {
                action_key: action_key.0,
                extern_crate_name: dependency.extern_crate_name.to_string(),
                public: dependency.public,
                noprelude: dependency.noprelude,
                nounused: dependency.nounused,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    // Unit-graph traversal order is local scheduler state, not part of the
    // persistent action contract.
    dependencies.sort_unstable();
    visiting.remove(unit);

    let target_source = unit
        .target
        .src_path()
        .path()?
        .strip_prefix(unit.pkg.root())
        .ok()?
        .to_str()?
        .to_owned();
    let checksum = unit.pkg.summary().checksum()?;
    let extra_args = build_runner
        .bcx
        .extra_args_for(unit)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let input = CacheKeyInputV0 {
        format_version: CACHE_FORMAT_VERSION,
        package: PackageInput {
            name: unit.pkg.name().as_str(),
            version: unit.pkg.version().to_string(),
            source: unit.pkg.package_id().source_id().to_string(),
            checksum,
        },
        target: TargetInput {
            name: unit.target.name().to_owned(),
            crate_name: unit.target.crate_name(),
            source_path: target_source,
            crate_types: unit
                .target
                .rustc_crate_types()
                .iter()
                .map(|crate_type| crate_type.as_str().to_owned())
                .collect(),
            compile_kind: "host",
        },
        mode: "check",
        profile: &unit.profile,
        lto: lto_input(build_runner.lto[unit]),
        rustc_verbose_version: &build_runner.bcx.rustc().verbose_version,
        rustflags: &unit.rustflags,
        extra_args,
        features: unit
            .features
            .iter()
            .map(|feature| feature.to_string())
            .collect(),
        dependencies,
    };
    let bytes = serde_json::to_vec(&input).ok()?;
    Some(ActionKey(blake3::hash(&bytes).to_hex().to_string()))
}

fn eligible_without_dependencies(build_runner: &BuildRunner<'_, '_>, unit: &Unit) -> bool {
    cfg!(target_os = "macos")
        && build_runner.bcx.gctx.cli_unstable().cargo_cas
        && unit.pkg.package_id().source_id().is_registry()
        && unit.pkg.summary().checksum().is_some()
        && !unit.is_std
        && !unit.pkg.has_custom_build()
        && unit.pkg.manifest().links().is_none()
        && !unit.pkg.proc_macro()
        && !unit.target.proc_macro()
        && unit.target.is_lib()
        && unit.target.is_linkable()
        && !unit.target.is_example()
        && !unit.artifact.is_true()
        && matches!(unit.mode, CompileMode::Check { test: false })
        && !unit.profile.incremental
        // A custom target specification can contain arbitrary local paths and
        // target-specific configuration.  V0 intentionally shares only the
        // native host unit on macOS.
        && unit.kind.is_host()
        // Wrappers can alter generated bytes without Cargo being able to
        // describe their semantics in this cache format.
        && build_runner.bcx.rustc().wrapper.is_none()
}

fn lto_input(lto: Lto) -> LtoInput {
    match lto {
        Lto::Run(value) => LtoInput::Run(value.map(|value| value.to_string())),
        Lto::Off => LtoInput::Off,
        Lto::OnlyBitcode => LtoInput::OnlyBitcode,
        Lto::ObjectAndBitcode => LtoInput::ObjectAndBitcode,
        Lto::OnlyObject => LtoInput::OnlyObject,
    }
}

fn artifact_paths(
    build_runner: &BuildRunner<'_, '_>,
    unit: &Unit,
) -> CargoResult<Vec<ArtifactPath>> {
    let mut artifacts = build_runner
        .outputs(unit)?
        .iter()
        .filter(|output| output.flavor == FileFlavor::Rmeta)
        .map(|output| ArtifactPath {
            role: ArtifactRole::Rmeta,
            source: output.path.clone(),
            destination: output.path.clone(),
        })
        .collect::<Vec<_>>();
    let dep_info = build_runner.files().fingerprint_file_path(unit, "dep-");
    artifacts.push(ArtifactPath {
        role: ArtifactRole::DepInfo,
        source: dep_info.clone(),
        destination: dep_info,
    });
    Ok(artifacts)
}

fn cache_root(build_runner: &BuildRunner<'_, '_>) -> PathBuf {
    build_runner
        .bcx
        .gctx
        .home()
        .join("cache")
        .join(CACHE_DIRECTORY)
        .into_path_unlocked()
}

fn validate_manifest(root: &Path, manifest: &CacheManifestV0) -> bool {
    let mut files = BTreeSet::new();
    manifest.artifacts.iter().all(|artifact| {
        if !is_safe_file_name(&artifact.file) || !files.insert(&artifact.file) {
            return false;
        }
        let path = root.join(ARTIFACTS_DIRECTORY).join(&artifact.file);
        let Ok(metadata) = fs::metadata(&path) else {
            return false;
        };
        metadata.is_file()
            && metadata.len() == artifact.size
            && digest_file(&path).is_ok_and(|digest| digest == artifact.digest)
    })
}

fn manifest_matches_expected(manifest: &CacheManifestV0, expected: &[ArtifactPath]) -> bool {
    manifest.artifacts.len() == expected.len()
        && manifest
            .artifacts
            .iter()
            .zip(expected)
            .all(|(cached, expected)| {
                cached.role == expected.role
                    && cached.output_file_name == file_name(&expected.destination)
            })
}

fn digest_file(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut digest = blake3::Hasher::new();
    let mut buffer = [0; 16 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(digest.finalize().to_hex().to_string());
        }
        digest.update(&buffer[..read]);
    }
}

fn is_safe_file_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
        && Path::new(name).components().count() == 1
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .expect("Cargo output path has file name")
        .to_string_lossy()
        .into_owned()
}
