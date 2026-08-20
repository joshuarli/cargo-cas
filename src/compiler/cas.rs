//! The experimental immutable-source artifact cache.
//!
//! This deliberately materializes a verified cache entry into Cargo's normal
//! build directory.  The local [`fingerprint`](super::fingerprint) and job
//! queue therefore remain the authority for scheduling and freshness; this
//! module only substitutes the work normally performed by `rustc`.

use std::collections::{BTreeMap, BTreeSet};
#[cfg(target_os = "macos")]
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "macos")]
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use cargo_util::paths;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use super::fingerprint;
use super::job_queue::Work;
use super::{
    BuildRunner, CompileMode, CrateType, FileFlavor, Lto, Unit,
    custom_build::{BuildOutput, BuildScriptOutputs},
};
use crate::util::CargoResult;
use crate::workspace::{PackageId, Target};
use cargo_util_schemas::manifest::RustVersion;

// Version 4 adds a stable identity for local packages checked out as Git
// worktrees. Entries from older formats cannot prove that a path dependency
// still has the same source snapshot or worktree identity, so they must be
// rebuilt rather than accepted as a partial hit.
const CACHE_FORMAT_VERSION: u8 = 4;
const CACHE_DIRECTORY: &str = "cargo-cas-v1";
const MANIFEST_FILE: &str = "manifest.json";
const ARTIFACTS_DIRECTORY: &str = "artifacts";
const BUILD_SCRIPT_MANIFEST_FILE: &str = "build-script.json";
const LOCKS_DIRECTORY: &str = "locks";
const ACCESS_DIRECTORY: &str = "access";

static TEMPORARY_ENTRY_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Per-Cargo-invocation observability for the experimental cache. Counters are
/// intentionally process-local: they explain one scheduler run without adding
/// mutable state to immutable cache entries.
#[derive(Clone, Default)]
pub(crate) struct CacheStats(Arc<CacheStatsInner>);

#[derive(Default)]
struct CacheStatsInner {
    eligible: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    rejects: AtomicU64,
    eligible_rustc: AtomicU64,
    duplicate_build_avoidance: AtomicU64,
    skips: Mutex<BTreeMap<&'static str, u64>>,
}

impl CacheStats {
    fn eligible(&self) {
        self.0.eligible.fetch_add(1, Ordering::Relaxed);
    }

    fn hit(&self) {
        self.0.hits.fetch_add(1, Ordering::Relaxed);
    }

    fn miss(&self) {
        self.0.misses.fetch_add(1, Ordering::Relaxed);
    }

    fn reject(&self) {
        self.0.rejects.fetch_add(1, Ordering::Relaxed);
    }

    fn eligible_rustc(&self) {
        self.0.eligible_rustc.fetch_add(1, Ordering::Relaxed);
    }

    fn duplicate_build_avoidance(&self) {
        self.0
            .duplicate_build_avoidance
            .fetch_add(1, Ordering::Relaxed);
    }

    fn skip(&self, reason: &'static str) {
        let mut skips = self
            .0
            .skips
            .lock()
            .expect("cargo-cas skip counter poisoned");
        *skips.entry(reason).or_default() += 1;
    }

    /// Emits a machine-searchable end-of-build summary only when the cache's
    /// debug tracing target is enabled. Ordinary Cargo output remains quiet.
    pub(crate) fn log_summary(&self) {
        let skips = self
            .0
            .skips
            .lock()
            .expect("cargo-cas skip counter poisoned")
            .clone();
        debug!(
            eligible = self.0.eligible.load(Ordering::Relaxed),
            hits = self.0.hits.load(Ordering::Relaxed),
            misses = self.0.misses.load(Ordering::Relaxed),
            rejects = self.0.rejects.load(Ordering::Relaxed),
            eligible_rustc = self.0.eligible_rustc.load(Ordering::Relaxed),
            duplicate_build_avoidance = self.0.duplicate_build_avoidance.load(Ordering::Relaxed),
            skips = ?skips,
            "cargo-cas summary"
        );
    }
}

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
    manifest: CacheManifestV1,
}

/// The global identity and local output shape of one cacheable compilation.
/// It owns no file descriptor: a per-key lock is opened only by active work,
/// never while Cargo is constructing the complete unit graph.
#[derive(Clone)]
pub(crate) struct CacheAction {
    key: ActionKey,
    identity: ManifestIdentityV1,
    cache: PathBuf,
    artifacts: Vec<ArtifactPath>,
    stats: CacheStats,
}

/// The stable inputs needed to publish after the `rustc` work has completed.
/// It owns only paths and an already-calculated key so it can safely outlive
/// the mutable [`BuildRunner`] borrow used to construct the compiler job.
#[derive(Clone)]
pub(crate) struct CachePublication {
    key: ActionKey,
    identity: ManifestIdentityV1,
    cache: PathBuf,
    artifacts: Vec<ArtifactPath>,
    build_script_outputs: Option<(
        Arc<Mutex<BuildScriptOutputs>>,
        Vec<super::UnitHash>,
    )>,
}

/// A cacheable, non-native build-script execution. The script binary itself
/// still follows Cargo's ordinary build-script compilation path; this action
/// stores only its declared textual output and files created beneath `OUT_DIR`.
#[derive(Clone)]
pub(crate) struct BuildScriptCache {
    action: BuildScriptCacheAction,
    replay: Option<BuildScriptReplay>,
}

#[derive(Clone)]
struct BuildScriptCacheAction {
    key: ActionKey,
    cache: PathBuf,
    identity: BuildScriptIdentity,
    output_file: PathBuf,
    stderr_file: PathBuf,
    root_output_file: PathBuf,
    script_out_dir: PathBuf,
    build_script_outputs: Arc<Mutex<BuildScriptOutputs>>,
    package_id: PackageId,
    metadata: super::UnitHash,
    stats: CacheStats,
}

/// Everything needed to replay a parsed build-script result into Cargo's
/// normal in-memory and on-disk build state.
#[derive(Clone)]
pub(crate) struct BuildScriptReplay {
    entry: BuildScriptCacheEntry,
    output_file: PathBuf,
    stderr_file: PathBuf,
    root_output_file: PathBuf,
    script_out_dir: PathBuf,
    build_script_outputs: Arc<Mutex<BuildScriptOutputs>>,
    package_id: PackageId,
    metadata: super::UnitHash,
    library_name: Option<String>,
    package_description: String,
    nightly_features_allowed: bool,
    targets: Vec<Target>,
    msrv: Option<RustVersion>,
    json_messages: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct BuildScriptIdentity {
    package_id: String,
    package_source: String,
    target: String,
    host: String,
    profile: String,
    features: Vec<String>,
    rustflags: Vec<String>,
    toolchain: ToolchainInput,
    environment: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BuildScriptCacheManifest {
    format_version: u8,
    action_key: String,
    identity: BuildScriptIdentity,
    output: String,
    output_dir: String,
    environment: BTreeMap<String, Option<String>>,
    files: Vec<CachedBuildScriptFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CachedBuildScriptFile {
    file: String,
    artifact: String,
    size: u64,
    digest: String,
}

#[derive(Clone, Debug)]
struct BuildScriptCacheEntry {
    root: PathBuf,
    manifest: BuildScriptCacheManifest,
}

/// Prepares the optional cache for a `RunCustomBuild` unit. The ordinary
/// build-script compiler job remains Cargo-owned; only a validated execution
/// result can be replayed.
pub(crate) fn prepare_build_script(
    build_runner: &BuildRunner<'_, '_>,
    unit: &Unit,
) -> CargoResult<Option<BuildScriptCache>> {
    let Some(key) = build_script_action_key(build_runner, unit) else {
        return Ok(None);
    };
    let Some(identity) = build_script_identity(build_runner, unit) else {
        return Ok(None);
    };
    let run_root = build_runner.files().build_script_run_dir(unit);
    let output_file = if build_runner.bcx.gctx.cli_unstable().build_dir_new_layout {
        run_root.join("stdout")
    } else {
        run_root.join("output")
    };
    let root_output_file = run_root.join("root-output");
    let stderr_file = run_root.join("stderr");
    let script_out_dir = if build_runner.bcx.gctx.cli_unstable().build_dir_new_layout {
        build_runner.files().out_dir_new_layout(unit)
    } else {
        build_runner.files().build_script_out_dir(unit)
    };
    let action = BuildScriptCacheAction {
        key,
        cache: cache_root(build_runner),
        identity,
        output_file,
        stderr_file,
        root_output_file,
        script_out_dir,
        build_script_outputs: Arc::clone(&build_runner.build_script_outputs),
        package_id: unit.pkg.package_id(),
        metadata: build_runner.get_run_build_script_metadata(unit),
        stats: build_runner.cas_stats.clone(),
    };
    action.stats.eligible();
    let replay = action.lookup().map(|entry| BuildScriptReplay {
        entry,
        output_file: action.output_file.clone(),
        stderr_file: action.stderr_file.clone(),
        root_output_file: action.root_output_file.clone(),
        script_out_dir: action.script_out_dir.clone(),
        build_script_outputs: Arc::clone(&action.build_script_outputs),
        package_id: action.package_id,
        metadata: action.metadata,
        library_name: unit.pkg.library().map(|target| target.crate_name()),
        package_description: unit.pkg.to_string(),
        nightly_features_allowed: build_runner.bcx.gctx.nightly_features_allowed,
        targets: unit.pkg.targets().to_vec(),
        msrv: unit.pkg.rust_version().cloned(),
        json_messages: build_runner.bcx.build_config.emit_json(),
    });
    Ok(Some(BuildScriptCache { action, replay }))
}

impl BuildScriptCache {
    pub(crate) fn replay(&self) -> Option<BuildScriptReplay> {
        self.replay.clone()
    }

    pub(crate) fn is_hit(&self) -> bool {
        self.replay.is_some()
    }

    /// Publishes the execution result after Cargo's normal build-script work
    /// has completed and populated `BuildScriptOutputs`.
    pub(crate) fn publication_work(&self) -> Work {
        let action = self.action.clone();
        Work::new(move |_| {
            action.publish();
            Ok(())
        })
    }
}

impl BuildScriptCacheAction {
    fn lookup(&self) -> Option<BuildScriptCacheEntry> {
        if !is_plain_directory(&self.cache) {
            self.stats.miss();
            return None;
        }
        let root = self.cache.join(self.key.as_str());
        if !is_plain_directory(&root) {
            self.stats.miss();
            return None;
        }
        let manifest_path = root.join(BUILD_SCRIPT_MANIFEST_FILE);
        let Ok(bytes) = read_regular_file(&manifest_path) else {
            self.stats.miss();
            return None;
        };
        let Ok(manifest) = serde_json::from_slice::<BuildScriptCacheManifest>(&bytes) else {
            self.stats.reject();
            return None;
        };
        if manifest.format_version != CACHE_FORMAT_VERSION
            || manifest.action_key != self.key.as_str()
            || manifest.identity != self.identity
            || !declared_environment_matches(&manifest.environment)
            || !validate_build_script_entry(&root, &manifest)
        {
            self.stats.reject();
            return None;
        }
        mark_used(&self.cache, self.key.as_str());
        self.stats.hit();
        Some(BuildScriptCacheEntry { root, manifest })
    }

    fn publish(&self) {
        let result = self.publish_inner();
        if let Err(error) = result {
            warn!(error = ?error, "failed to publish cargo-cas build-script entry; continuing without cache");
        }
    }

    fn publish_inner(&self) -> CargoResult<()> {
        let output = self
            .build_script_outputs
            .lock()
            .expect("build script outputs lock poisoned")
            .get(self.metadata)
            .cloned();
        let Some(output) = output else {
            return Ok(());
        };
        if !build_script_output_is_representable(&output) {
            debug!("cargo-cas skips non-replayable build-script output");
            return Ok(());
        }
        let bytes = read_regular_file(&self.output_file)?;
        let output_text = String::from_utf8(bytes.clone()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "build-script output is not UTF-8",
            )
        })?;
        let output_dir = self.script_out_dir.to_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "build-script output path is not UTF-8")
        })?;
        let files = collect_build_script_files(&self.script_out_dir)?;
        let declared_environment = output
            .rerun_if_env_changed
            .iter()
            .map(|name| (name.clone(), std::env::var(name).ok()))
            .collect::<BTreeMap<_, _>>();

        ensure_cache_root(&self.cache)?;
        let final_entry = self.cache.join(self.key.as_str());
        if fs::symlink_metadata(&final_entry).is_ok() {
            if build_script_entry_is_valid(&final_entry, &self.key, &self.identity) {
                mark_used(&self.cache, self.key.as_str());
                return Ok(());
            }
            remove_entry(&final_entry)?;
        }

        let temporary_entry = ensure_cache_subdirectory(&self.cache, "tmp")?.join(format!(
            "build-script-{}-{}-{}",
            self.key.as_str(),
            std::process::id(),
            TEMPORARY_ENTRY_COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        let temporary_artifacts = temporary_entry.join(ARTIFACTS_DIRECTORY);
        fs::create_dir_all(&temporary_artifacts)?;
        let mut manifest_files = Vec::with_capacity(files.len());
        for (index, relative, source) in files {
            let destination = temporary_artifacts.join(index.to_string());
            fs::copy(&source, &destination)?;
            let metadata = fs::metadata(&destination)?;
            manifest_files.push(CachedBuildScriptFile {
                file: format!("{}", relative.display()),
                artifact: index.to_string(),
                size: metadata.len(),
                digest: digest_file(&destination)?,
            });
        }
        let manifest = BuildScriptCacheManifest {
            format_version: CACHE_FORMAT_VERSION,
            action_key: self.key.0.clone(),
            identity: self.identity.clone(),
            output: output_text,
            output_dir: output_dir.to_owned(),
            environment: declared_environment,
            files: manifest_files,
        };
        paths::write_atomic(
            temporary_entry.join(BUILD_SCRIPT_MANIFEST_FILE),
            serde_json::to_vec(&manifest)?,
        )?;
        if !build_script_entry_is_valid(&temporary_entry, &self.key, &self.identity) {
            let _ = fs::remove_dir_all(&temporary_entry);
            return Ok(());
        }
        if let Err(error) = fs::rename(&temporary_entry, &final_entry)
            && error.kind() != io::ErrorKind::AlreadyExists
        {
            return Err(error.into());
        }
        if temporary_entry.exists() {
            let _ = fs::remove_dir_all(&temporary_entry);
        }
        mark_used(&self.cache, self.key.as_str());
        Ok(())
    }
}

impl BuildScriptReplay {
    pub(crate) fn work(self) -> Work {
        Work::new(move |state| self.replay_inner(state))
    }

    fn replay_inner(self, state: &super::job_queue::JobState<'_, '_>) -> CargoResult<()> {
        if fs::symlink_metadata(&self.script_out_dir).is_ok() {
            if !is_plain_directory(&self.script_out_dir) {
                return Err(io::Error::other("build-script output directory is not a directory").into());
            }
            fs::remove_dir_all(&self.script_out_dir)?;
        }
        paths::create_dir_all(&self.script_out_dir)?;
        let artifact_root = self.entry.root.join(ARTIFACTS_DIRECTORY);
        for file in &self.entry.manifest.files {
            let source = artifact_root.join(&file.artifact);
            let destination = safe_join(&self.script_out_dir, &file.file)?;
            if let Some(parent) = destination.parent() {
                paths::create_dir_all(parent)?;
            }
            copy_verified_artifact(&source, &destination, file.size, &file.digest)?;
        }
        if let Some(parent) = self.output_file.parent() {
            paths::create_dir_all(parent)?;
        }
        paths::write(&self.output_file, self.entry.manifest.output.as_bytes())?;
        paths::write(&self.stderr_file, b"")?;
        paths::write(&self.root_output_file, paths::path2bytes(&self.script_out_dir)?)?;
        let parsed = BuildOutput::parse(
            self.entry.manifest.output.as_bytes(),
            self.library_name,
            &self.package_description,
            Path::new(&self.entry.manifest.output_dir),
            &self.script_out_dir,
            self.nightly_features_allowed,
            &self.targets,
            &self.msrv,
        )?;
        if self.json_messages {
            super::custom_build::emit_build_output(
                state,
                &parsed,
                &self.script_out_dir,
                self.package_id,
            )?;
        }
        self.build_script_outputs
            .lock()
            .expect("build script outputs lock poisoned")
            .insert(self.package_id, self.metadata, parsed);
        Ok(())
    }
}


#[derive(Serialize)]
struct CacheKeyInputV0<'a> {
    format_version: u8,
    package: PackageInput<'a>,
    target: TargetInput,
    mode: &'static str,
    profile: &'a crate::workspace::profiles::Profile,
    lto: LtoInput,
    toolchain: ToolchainInput,
    rustflags: &'a [String],
    extra_args: &'a [String],
    compiler_contract: CompilerContractInput,
    features: Vec<String>,
    dependencies: Vec<DependencyInput>,
}

/// The remaining stable portion of Cargo's rustc invocation which is not
/// already represented by [`Profile`], `Unit::rustflags`, or dependency
/// ActionKeys.  Keeping this separate makes the cache-key audit searchable:
/// adding an effective compiler argument requires either an explicit field
/// here or a conservative eligibility exclusion.
#[derive(Serialize)]
struct CompilerContractInput {
    manifest_lint_rustflags: Vec<String>,
    check_cfg_args: Vec<String>,
    cap_lints: &'static str,
    allow_features: Vec<String>,
    cargo_lints: bool,
    binary_dep_depinfo: bool,
    checksum_freshness: bool,
    embeds_metadata: bool,
    linker: Option<String>,
}

#[derive(Serialize)]
struct PackageInput<'a> {
    name: &'a str,
    version: String,
    source: PackageSourceInput<'a>,
}

/// Source content identity is intentionally explicit instead of relying on
/// `SourceId` formatting or hashing. Git's display form truncates its precise
/// revision and its ordinary hash excludes it, either of which would make an
/// unsafe persistent ActionKey.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum PackageSourceInput<'a> {
    Registry {
        source: String,
        checksum: &'a str,
    },
    Git {
        canonical_url: String,
        revision: &'a str,
        reference: String,
    },
    /// A local path source is conservative about relocation: an ordinary
    /// checkout keeps its canonical root in the identity, while a package in
    /// a Git worktree uses the repository's common Git directory, commit, and
    /// path within that repository. In both cases `snapshot` captures every
    /// regular package file. This lets detached worktrees of one repository
    /// share immutable actions without treating an unrelated checkout with
    /// coincident bytes as the same source.
    Path {
        canonical_root: String,
        snapshot: String,
    },
    GitWorktree {
        repository: String,
        revision: String,
        relative_root: String,
        snapshot: String,
    },
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

/// The compiler context must be recorded explicitly because `rustc -vV` is
/// not sufficient when two executable paths deliberately report the same
/// version while compiling differently. The sysroot may likewise contribute
/// crates and linker inputs not represented by the version banner.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct ToolchainInput {
    rustc_path: String,
    rustc_verbose_version: String,
    sysroot: String,
}

/// Human-inspectable identity duplicated in an entry manifest. The ActionKey
/// remains the lookup identity; these fields make a corrupt or incompatible
/// manifest self-describing and independently rejectable.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct ManifestIdentityV1 {
    package_id: String,
    target_name: String,
    crate_name: String,
    compile_mode: String,
    toolchain: ToolchainInput,
    dependency_action_keys: Vec<String>,
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
struct CacheManifestV1 {
    format_version: u8,
    action_key: String,
    identity: ManifestIdentityV1,
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
    Linkable,
    DepInfo,
    OutputCache,
}

#[derive(Clone)]
struct ArtifactPath {
    role: ArtifactRole,
    source: PathBuf,
    destination: PathBuf,
    /// Compiler diagnostics are optional because Cargo creates an output cache
    /// lazily, only when rustc actually emits a cacheable message. All other
    /// entry members are required for a valid reusable action.
    required: bool,
}

/// Finds a complete, valid cache entry for an eligible dirty unit.
///
/// All lookup failures are cache misses.  Cargo can still compile the unit in
/// the normal way, which is safer than allowing cache infrastructure damage to
/// prevent a valid build.
pub(crate) fn prepare(
    build_runner: &BuildRunner<'_, '_>,
    unit: &Unit,
) -> CargoResult<Option<CacheAction>> {
    let key = match action_key(build_runner, unit) {
        Some(key) => key,
        None => {
            let reason = ineligibility_reason_in_subgraph(build_runner, unit)
                .unwrap_or("an ineligible dependency action");
            build_runner.cas_stats.skip(reason);
            debug!(
                package = %unit.pkg.package_id(),
                target = %unit.target.name(),
                "cargo-cas skip: {reason}"
            );
            return Ok(None);
        }
    };
    let Some(identity) = manifest_identity(build_runner, unit) else {
        build_runner
            .cas_stats
            .skip("manifest identity could not be represented");
        debug!(
            package = %unit.pkg.package_id(),
            target = %unit.target.name(),
            "cargo-cas skip: manifest identity could not be represented"
        );
        return Ok(None);
    };
    build_runner.cas_stats.eligible();
    Ok(Some(CacheAction {
        key,
        identity,
        cache: cache_root(build_runner),
        artifacts: artifact_paths(build_runner, unit)?,
        stats: build_runner.cas_stats.clone(),
    }))
}

impl CacheAction {
    /// A lock-free hit check. Immutable entries are published by atomic rename,
    /// so readers never need to serialize with other readers.
    pub(crate) fn lookup(&self) -> Option<CacheEntry> {
        self.lookup_inner(true)
    }

    /// A same-key waiter rechecks after the writer lock becomes available.
    /// That recheck is a coordination detail, not a second cache lookup
    /// attempt in the per-invocation summary.
    fn lookup_after_lock(&self) -> Option<CacheEntry> {
        self.lookup_inner(false)
    }

    fn lookup_inner(&self, count_miss: bool) -> Option<CacheEntry> {
        if !is_plain_directory(&self.cache) {
            if count_miss {
                self.stats.miss();
            }
            debug!(path = %self.cache.display(), "cargo-cas miss: cache root unavailable");
            return None;
        }
        let root = self.cache.join(self.key.as_str());
        if !is_plain_directory(&root) {
            if count_miss {
                self.stats.miss();
            }
            debug!(key = self.key.as_str(), "cargo-cas miss: entry absent");
            return None;
        }
        let manifest_path = root.join(MANIFEST_FILE);
        let Ok(manifest_bytes) = read_regular_file(&manifest_path) else {
            if count_miss {
                self.stats.miss();
            }
            debug!(
                key = self.key.as_str(),
                "cargo-cas miss: manifest unavailable"
            );
            return None;
        };
        let Ok(manifest) = serde_json::from_slice::<CacheManifestV1>(&manifest_bytes) else {
            self.stats.reject();
            warn!(path = %manifest_path.display(), "ignoring malformed cargo-cas cache manifest");
            debug!(
                key = self.key.as_str(),
                "cargo-cas reject: malformed manifest"
            );
            return None;
        };

        if manifest.format_version != CACHE_FORMAT_VERSION
            || manifest.action_key != self.key.as_str()
            || manifest.identity != self.identity
        {
            self.stats.reject();
            warn!(path = %manifest_path.display(), "ignoring incompatible cargo-cas cache manifest");
            debug!(
                key = self.key.as_str(),
                "cargo-cas reject: incompatible manifest"
            );
            return None;
        }
        if !validate_manifest(&root, &manifest) {
            self.stats.reject();
            warn!(path = %manifest_path.display(), "ignoring corrupt cargo-cas cache entry");
            debug!(key = self.key.as_str(), "cargo-cas reject: corrupt entry");
            return None;
        }
        if !manifest_matches_expected(&manifest, &self.artifacts) {
            self.stats.reject();
            warn!(path = %manifest_path.display(), "ignoring cargo-cas entry with unexpected artifacts");
            debug!(
                key = self.key.as_str(),
                "cargo-cas reject: unexpected artifacts"
            );
            return None;
        }

        mark_used(&self.cache, self.key.as_str());
        self.stats.hit();
        debug!(key = self.key.as_str(), "cargo-cas hit");
        Some(CacheEntry { root, manifest })
    }

    /// Returns work that materializes a validated entry at Cargo's usual
    /// output paths. This leaves `extern_args`, `-L` construction, local
    /// fingerprints, and final artifact uplift unchanged.
    ///
    /// A hit can still disappear or become unreadable between lookup and the
    /// actual job. That is cache-infrastructure damage, so recover by running
    /// the already-prepared normal compiler work instead of failing Cargo.
    pub(crate) fn restore_or_compile(
        &self,
        entry: CacheEntry,
        normal_work: Work,
        replay_output: Work,
    ) -> Work {
        debug_assert!(manifest_matches_expected(&entry.manifest, &self.artifacts));

        let restores = entry
            .manifest
            .artifacts
            .iter()
            .map(|cached| {
                let expected = self
                    .artifacts
                    .iter()
                    .find(|expected| artifact_matches_expected(cached, expected))
                    .expect("validated cargo-cas manifest matches expected artifacts");
                (
                    entry.root.join(ARTIFACTS_DIRECTORY).join(&cached.file),
                    expected.destination.clone(),
                    cached.role,
                    cached.size,
                    cached.digest.clone(),
                )
            })
            .collect::<Vec<_>>();
        let stats = self.stats.clone();

        Work::new(move |state| {
            let restored: CargoResult<()> = (|| {
                for (source, destination, role, size, digest) in restores {
                    let parent = destination.parent().expect("Cargo output path has parent");
                    paths::create_dir_all(parent)?;
                    // Cache entries are immutable. Copy rather than hardlink so a
                    // later local output cleanup or compiler invocation can never
                    // mutate a globally cached inode.
                    copy_verified_artifact(&source, &destination, size, &digest)?;
                    if role == ArtifactRole::Rmeta {
                        // Pipelined metadata consumers can begin as soon as
                        // the restored `.rmeta` is locally available. The
                        // manifest always places this role before the
                        // linkable artifact and dep-info transport files.
                        state.rmeta_produced();
                        pause_after_rmeta_for_test();
                    }
                }
                Ok(())
            })();
            match restored {
                Ok(()) => match replay_output.call(state) {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        // The output cache is only diagnostic replay state. A
                        // bad or unreadable copy must never make an otherwise
                        // reusable artifact set fail a valid Cargo build.
                        warn!(error = ?error, "failed to replay cargo-cas diagnostics; compiling normally");
                        stats.eligible_rustc();
                        normal_work.call(state)
                    }
                },
                Err(error) => {
                    warn!(error = ?error, "cargo-cas entry disappeared during restore; compiling normally");
                    stats.eligible_rustc();
                    normal_work.call(state)
                }
            }
        })
    }

    /// Holds only this action's lock while a miss is active, then checks the
    /// entry again. A concurrent writer therefore turns a waiter into a local
    /// restore instead of a duplicate rustc invocation.
    pub(crate) fn coordinate(
        self,
        normal_work: Work,
        replay_output: Work,
        allow_hit: bool,
    ) -> Work {
        Work::new(move |state| match self.lock() {
            Ok(_lock) => {
                if allow_hit {
                    if let Some(entry) = self.lookup_after_lock() {
                        self.stats.duplicate_build_avoidance();
                        return self
                            .restore_or_compile(entry, normal_work, replay_output)
                            .call(state);
                    }
                }
                self.stats.eligible_rustc();
                normal_work.call(state)
            }
            Err(error) => {
                warn!(error = ?error, key = self.key.as_str(), "cargo-cas key lock unavailable; compiling normally");
                self.stats.eligible_rustc();
                normal_work.call(state)
            }
        })
    }

    fn lock(&self) -> io::Result<File> {
        let lock_path = ensure_cache_subdirectory(&self.cache, LOCKS_DIRECTORY)?
            .join(format!("{}.lock", self.key.as_str()));
        if fs::symlink_metadata(&lock_path).is_ok_and(|metadata| !metadata.file_type().is_file()) {
            return Err(io::Error::other("cargo-cas key lock is not a regular file"));
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;

            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(lock_path)?;
        crate::util::flock::lock_exclusive(&file)?;
        Ok(file)
    }
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
    let Some(identity) = manifest_identity(build_runner, unit) else {
        return Ok(None);
    };
    Ok(Some(CachePublication {
        key,
        identity,
        cache: cache_root(build_runner),
        artifacts: artifact_paths(build_runner, unit)?,
        build_script_outputs: build_runner
            .find_build_script_metadatas(unit)
            .map(|metadata| (Arc::clone(&build_runner.build_script_outputs), metadata)),
    }))
}

impl CachePublication {
    /// Stages and atomically publishes dependency artifacts plus local dep-info
    /// produced by a successful ordinary Cargo compilation.
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
        if let Some((outputs, metadata)) = &self.build_script_outputs {
            let outputs = outputs.lock().expect("build script outputs lock poisoned");
            if metadata.iter().any(|metadata| {
                outputs
                    .get(*metadata)
                    .is_none_or(|output| !build_script_output_is_representable(output))
            }) {
                debug!("cargo-cas skips unit with non-replayable build-script output");
                return Ok(());
            }
        }
        // Cargo's encoded dep-info has a relative path representation, but a
        // unit may still record an absolute or environment input that V0 does
        // not represent in its pre-compilation identity. Do not publish those
        // units as globally reusable actions.
        let dep_info = self
            .artifacts
            .iter()
            .find(|artifact| artifact.role == ArtifactRole::DepInfo)
            .expect("cargo-cas publication includes dep-info")
            .source
            .clone();
        if !fingerprint::is_relocatable_dep_info(&dep_info) {
            debug!("cargo-cas skips unit with non-relocatable translated dep-info");
            return Ok(());
        }

        ensure_cache_root(&self.cache)?;

        if self
            .artifacts
            .iter()
            .any(|artifact| artifact.required && !artifact.source.is_file())
        {
            return Ok(());
        }

        let final_entry = self.cache.join(self.key.as_str());
        if fs::symlink_metadata(&final_entry).is_ok() {
            if entry_is_valid(&final_entry, &self.key, &self.identity, &self.artifacts) {
                mark_used(&self.cache, self.key.as_str());
                return Ok(());
            }
            // A process that died before publication cannot leave a final
            // entry because publication is a rename.  Still, an interrupted
            // older implementation or local corruption can leave one behind.
            // Do not preserve an entry that will never become a hit.
            warn!(path = %final_entry.display(), "discarding invalid cargo-cas cache entry before republishing");
            remove_entry(&final_entry)?;
        }

        let temporary_entry = ensure_cache_subdirectory(&self.cache, "tmp")?.join(format!(
            "{}-{}-{}",
            self.key.as_str(),
            std::process::id(),
            TEMPORARY_ENTRY_COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        let temporary_artifacts = temporary_entry.join(ARTIFACTS_DIRECTORY);
        fs::create_dir(&temporary_entry)?;
        fs::create_dir(&temporary_artifacts)?;

        let mut manifest_artifacts = Vec::with_capacity(self.artifacts.len());
        for (index, artifact) in self.artifacts.iter().enumerate() {
            if !artifact.required && !artifact.source.is_file() {
                continue;
            }
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

        let manifest = CacheManifestV1 {
            format_version: CACHE_FORMAT_VERSION,
            action_key: self.key.0.clone(),
            identity: self.identity.clone(),
            artifacts: manifest_artifacts,
        };
        paths::write_atomic(
            temporary_entry.join(MANIFEST_FILE),
            serde_json::to_vec(&manifest)?,
        )?;
        if !entry_is_valid(&temporary_entry, &self.key, &self.identity, &self.artifacts) {
            let _ = fs::remove_dir_all(&temporary_entry);
            return Err(io::Error::other("staged cargo-cas entry failed validation").into());
        }

        pause_before_publish_for_test();

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
        mark_used(&self.cache, self.key.as_str());
        Ok(())
    }
}

/// Provides an integration-test-only process boundary immediately before the
/// atomic publish. The variable is intentionally undocumented and requires a
/// path controlled by the test harness; normal Cargo processes never enter
/// this branch. Keeping the boundary here lets the crash test exercise the
/// actual staged-entry protocol instead of approximating it with hand-written
/// cache files.
fn pause_before_publish_for_test() {
    let Some(signal_path) = std::env::var_os("CARGO_CAS_TEST_PAUSE_BEFORE_PUBLISH") else {
        return;
    };
    let signal_path = PathBuf::from(signal_path);
    if fs::write(&signal_path, std::process::id().to_string()).is_err() {
        return;
    }
    while signal_path.exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Provides an integration-test-only boundary after a cache hit has made its
/// metadata artifact available to Cargo's normal scheduler.  The pipelining
/// regression keeps this boundary closed until a dependent's rustc proxy has
/// started, proving that `rmeta_produced` is not deferred behind linkable or
/// dep-info materialization.
fn pause_after_rmeta_for_test() {
    let Some(signal_path) = std::env::var_os("CARGO_CAS_TEST_PAUSE_AFTER_RMETA") else {
        return;
    };
    let signal_path = PathBuf::from(signal_path);
    if fs::write(&signal_path, std::process::id().to_string()).is_err() {
        return;
    }
    while signal_path.exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn action_key(build_runner: &BuildRunner<'_, '_>, unit: &Unit) -> Option<ActionKey> {
    let mut visiting = BTreeSet::new();
    action_key_inner(build_runner, unit, &mut visiting)
}

fn build_script_action_key(
    build_runner: &BuildRunner<'_, '_>,
    unit: &Unit,
) -> Option<ActionKey> {
    if !unit.mode.is_run_custom_build() || !build_script_is_representable(build_runner, unit) {
        return None;
    }
    let identity = build_script_identity(build_runner, unit)?;
    let bytes = serde_json::to_vec(&identity).ok()?;
    let mut input = b"cargo-cas-build-script-v1\0".to_vec();
    input.extend_from_slice(&bytes);
    Some(ActionKey(blake3::hash(&input).to_hex().to_string()))
}

fn build_script_identity(
    build_runner: &BuildRunner<'_, '_>,
    unit: &Unit,
) -> Option<BuildScriptIdentity> {
    let source = package_source_input(unit)?;
    let package_source = serde_json::to_string(&source).ok()?;
    let environment = build_script_environment_input()?;
    Some(BuildScriptIdentity {
        package_id: unit.pkg.package_id().to_string(),
        package_source,
        target: unit.target.name().to_owned(),
        host: build_runner.bcx.host_triple().to_string(),
        profile: serde_json::to_string(&unit.profile).ok()?,
        features: unit.features.iter().map(ToString::to_string).collect(),
        rustflags: unit.rustflags.to_vec(),
        toolchain: toolchain_input(build_runner)?,
        environment,
    })
}

fn build_script_is_representable(
    build_runner: &BuildRunner<'_, '_>,
    unit: &Unit,
) -> bool {
    if !cfg!(target_os = "macos")
        || !build_runner.bcx.gctx.cli_unstable().cargo_cas
        || !unit.mode.is_run_custom_build()
        || !unit.kind.is_host()
        || unit.pkg.manifest().links().is_some()
        || build_runner.bcx.rustc().wrapper.is_some()
    {
        return false;
    }
    // A build dependency can execute arbitrary code and feed undeclared
    // values into the script. Keep this first replay model closed over the
    // package's own build script and Cargo's declared inputs.
    !build_runner
        .unit_deps(unit)
        .iter()
        .any(|dependency| dependency.unit.pkg.package_id() != unit.pkg.package_id())
}

fn build_script_output_is_representable(output: &BuildOutput) -> bool {
    output.library_paths.is_empty()
        && output.library_links.is_empty()
        && output.linker_args.is_empty()
        && output.metadata.is_empty()
        && output
            .rerun_if_changed
            .iter()
            .all(|path| safe_relative_path(path).is_some())
        && !output
            .log_messages
            .iter()
            .any(|(severity, _)| matches!(severity, super::custom_build::Severity::Error))
}

/// Capture inherited variables that a build script can observe. Cargo's
/// target/output-location variables and Cargo's per-process jobserver value are
/// deliberately omitted because they are workspace-local or ephemeral and are
/// rewritten during replay; all other UTF-8 values participate in the action
/// identity so an environment change cannot reuse a stale script result.
fn build_script_environment_input() -> Option<BTreeMap<String, String>> {
    std::env::vars_os()
        .filter(|(key, _)| {
            !matches!(
                key.to_str(),
                Some(
                    "CARGO_HOME"
                        | "CARGO_TARGET_DIR"
                        | "CARGO_TARGET_TMPDIR"
                        | "CARGO_MAKEFLAGS"
                        | "CARGO_LOG"
                        | "RUST_LOG"
                        | "PWD"
                        | "OLDPWD"
                        | "SHLVL"
                        | "_"
                        | "OUT_DIR"
                )
            )
        })
        .map(|(key, value)| Some((key.to_str()?.to_owned(), value.to_str()?.to_owned())))
        .collect()
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
            let action_key = if dependency.unit.mode.is_run_custom_build() {
                build_script_action_key(build_runner, &dependency.unit)?
            } else {
                action_key_inner(build_runner, &dependency.unit, visiting)?
            };
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
    let source = package_source_input(unit)?;
    let extra_args = build_runner
        .bcx
        .extra_args_for(unit)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let toolchain = toolchain_input(build_runner)?;
    let compiler_contract = compiler_contract_input(build_runner, unit)?;
    let input = CacheKeyInputV0 {
        format_version: CACHE_FORMAT_VERSION,
        package: PackageInput {
            name: unit.pkg.name().as_str(),
            version: unit.pkg.version().to_string(),
            source,
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
        mode: match unit.mode {
            CompileMode::Check { test: false } => "check",
            CompileMode::Build => "build",
            _ => return None,
        },
        profile: &unit.profile,
        lto: lto_input(build_runner.lto[unit]),
        toolchain,
        rustflags: &unit.rustflags,
        extra_args,
        compiler_contract,
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

/// Captures every effective compiler setting that V0 can represent without
/// serializing workspace-local paths.  The caller has already limited V0 to
/// native-host pure-Rust libraries, so the selected linker is still recorded
/// as a strict identity even though ordinary rlib/rmeta compilation normally
/// does not invoke it.
fn compiler_contract_input(
    build_runner: &BuildRunner<'_, '_>,
    unit: &Unit,
) -> Option<CompilerContractInput> {
    let gctx = build_runner.bcx.gctx;
    let linker_path = if unit.target.for_host() && !gctx.target_applies_to_host().ok()? {
        build_runner.compilation.host_linker()
    } else {
        build_runner.compilation.target_linker(unit.kind)
    };
    let linker = match linker_path {
        Some(path) => Some(path.to_str()?.to_owned()),
        None => None,
    };
    let allow_features = gctx
        .cli_unstable()
        .allow_features
        .as_ref()
        .map(|features| features.iter().map(ToString::to_string).collect())
        .unwrap_or_default();
    let check_cfg_args = super::check_cfg_args(unit)
        .into_iter()
        .map(|argument| argument.into_string().ok())
        .collect::<Option<Vec<_>>>()?;
    Some(CompilerContractInput {
        manifest_lint_rustflags: unit
            .pkg
            .manifest()
            .lint_rustflags()
            .iter()
            .map(ToString::to_string)
            .collect(),
        check_cfg_args,
        // Registry and immutable-git packages are never local V0 units, so
        // this exactly matches `compute_cap_lints` in `compiler::mod`.
        cap_lints: if unit.show_warnings(gctx) {
            "warn"
        } else {
            "allow"
        },
        allow_features,
        cargo_lints: gctx.cli_unstable().cargo_lints,
        binary_dep_depinfo: gctx.cli_unstable().binary_dep_depinfo,
        checksum_freshness: gctx.cli_unstable().checksum_freshness,
        embeds_metadata: build_runner
            .bcx
            .target_data
            .info(unit.kind)
            .should_embed_metadata(),
        linker,
    })
}

fn manifest_identity(
    build_runner: &BuildRunner<'_, '_>,
    unit: &Unit,
) -> Option<ManifestIdentityV1> {
    let mut dependency_action_keys = build_runner
        .unit_deps(unit)
        .iter()
        .map(|dependency| {
            if dependency.unit.mode.is_run_custom_build() {
                build_script_action_key(build_runner, &dependency.unit).map(|key| key.0)
            } else {
                action_key(build_runner, &dependency.unit).map(|key| key.0)
            }
        })
        .collect::<Option<Vec<_>>>()?;
    dependency_action_keys.sort_unstable();
    Some(ManifestIdentityV1 {
        package_id: manifest_package_id(unit)?,
        target_name: unit.target.name().to_owned(),
        crate_name: unit.target.crate_name(),
        compile_mode: match unit.mode {
            CompileMode::Check { test: false } => "check",
            CompileMode::Build => "build",
            _ => return None,
        }
        .to_owned(),
        toolchain: toolchain_input(build_runner)?,
        dependency_action_keys,
    })
}

/// Package IDs for local sources normally contain the absolute checkout path.
/// That would make an otherwise identical action from a sibling Git worktree
/// fail manifest validation even when its ActionKey uses the stable worktree
/// source identity. Keep registry and immutable-Git IDs in Cargo's familiar
/// display form, while serializing the already-validated local source identity
/// for path packages.
fn manifest_package_id(unit: &Unit) -> Option<String> {
    if unit.pkg.package_id().source_id().is_path() {
        return Some(serde_json::to_string(&package_source_input(unit)?).ok()?);
    }
    Some(unit.pkg.package_id().to_string())
}

fn toolchain_input(build_runner: &BuildRunner<'_, '_>) -> Option<ToolchainInput> {
    let rustc_path = paths::resolve_executable(&build_runner.bcx.rustc().path)
        .ok()?
        .canonicalize()
        .ok()?
        .to_str()?
        .to_owned();
    Some(ToolchainInput {
        rustc_path,
        rustc_verbose_version: build_runner.bcx.rustc().verbose_version.clone(),
        sysroot: build_runner.bcx.get_sysroot().to_str()?.to_owned(),
    })
}

fn eligible_without_dependencies(build_runner: &BuildRunner<'_, '_>, unit: &Unit) -> bool {
    ineligibility_reason(build_runner, unit).is_none()
}

/// Distinguishes a direct V0 exclusion from a conservative exclusion inherited
/// through an otherwise cacheable dependency action. The scheduler still falls
/// back in both cases, but the latter tells users which future model (build
/// scripts or proc macros) would be required to broaden the subgraph safely.
fn ineligibility_reason_in_subgraph(
    build_runner: &BuildRunner<'_, '_>,
    unit: &Unit,
) -> Option<&'static str> {
    let mut visiting = BTreeSet::new();
    ineligibility_reason_in_subgraph_inner(build_runner, unit, &mut visiting)
}

fn ineligibility_reason_in_subgraph_inner(
    build_runner: &BuildRunner<'_, '_>,
    unit: &Unit,
    visiting: &mut BTreeSet<Unit>,
) -> Option<&'static str> {
    if let Some(reason) = ineligibility_reason(build_runner, unit) {
        return Some(reason);
    }
    if !visiting.insert(unit.clone()) {
        return Some("an ineligible dependency action");
    }
    let reason = build_runner.unit_deps(unit).iter().find_map(|dependency| {
        if dependency.unit.mode.is_run_custom_build() {
            return build_script_action_key(build_runner, &dependency.unit)
                .is_none()
                .then_some("build-script affected");
        }
        let reason =
            ineligibility_reason_in_subgraph_inner(build_runner, &dependency.unit, visiting)?;
        Some(match reason {
            "package has a build script" | "build-script affected" => "build-script affected",
            "proc macro" | "proc-macro affected" => "proc-macro affected",
            _ => "an ineligible dependency action",
        })
    });
    visiting.remove(unit);
    reason
}

/// Explains why V0 refuses an otherwise ordinary dirty unit. These messages
/// are emitted only through debug tracing, keeping default Cargo output
/// unchanged while making conservative eligibility inspectable in tests and
/// real-world experiments.
fn ineligibility_reason(build_runner: &BuildRunner<'_, '_>, unit: &Unit) -> Option<&'static str> {
    let source_id = unit.pkg.package_id().source_id();
    if !cfg!(target_os = "macos") {
        return Some("unsupported platform");
    }
    if !build_runner.bcx.gctx.cli_unstable().cargo_cas {
        return Some("-Zcargo-cas is not enabled");
    }
    if !source_id.is_path() && !source_id.is_registry() && !source_id.is_git() {
        return Some("source is not an immutable registry or git source");
    }
    if package_source_input(unit).is_none() {
        return Some("source lacks an immutable content identity");
    }
    if unit.is_std {
        return Some("standard library unit");
    }
    if unit.pkg.manifest().links().is_some() {
        return Some("package links native code");
    }
    if unit.pkg.proc_macro() || unit.target.proc_macro() {
        return Some("proc macro");
    }
    if !unit.target.is_lib() || !unit.target.is_linkable() {
        return Some("unsupported target kind");
    }
    // A normal `lib`/`rlib` emits only Rust dependency artifacts. Dynamic
    // libraries introduce platform-linker and debug-bundle inputs, which are
    // intentionally outside the Gate 3 cache contract.
    if unit
        .target
        .rustc_crate_types()
        .iter()
        .any(|crate_type| !matches!(crate_type, CrateType::Lib | CrateType::Rlib))
    {
        return Some("unsupported crate type");
    }
    if unit.target.is_example() {
        return Some("example target");
    }
    if unit.artifact.is_true() {
        return Some("artifact dependency");
    }
    if !matches!(
        unit.mode,
        CompileMode::Check { test: false } | CompileMode::Build
    ) {
        return Some("unsupported compile mode");
    }
    if unit.profile.incremental {
        return Some("incremental compilation");
    }
    if unit.profile.trim_paths.is_some() {
        return Some("trim-paths profile");
    }
    // A custom target specification can contain arbitrary local paths and
    // target-specific configuration. V0 intentionally shares only the native
    // host unit on macOS.
    if !unit.kind.is_host() {
        return Some("non-host compile kind");
    }
    // Wrappers can alter generated bytes without Cargo being able to describe
    // their semantics in this cache format.
    if build_runner.bcx.rustc().wrapper.is_some() {
        return Some("rustc wrapper");
    }
    None
}

fn package_source_input(unit: &Unit) -> Option<PackageSourceInput<'_>> {
    let source_id = unit.pkg.package_id().source_id();
    if source_id.is_path() {
        let root = unit.pkg.root().canonicalize().ok()?;
        let snapshot = path_source_snapshot(&root)?;
        if let Some(worktree) = git_worktree_source_input(&root, snapshot.clone()) {
            return Some(worktree);
        }
        let canonical_root = root.to_str()?.to_owned();
        return Some(PackageSourceInput::Path {
            canonical_root,
            snapshot,
        });
    }

    if source_id.is_registry() {
        return Some(PackageSourceInput::Registry {
            source: source_id.as_encoded_url().to_string(),
            checksum: unit.pkg.summary().checksum()?,
        });
    }

    if source_id.is_git() {
        let revision = source_id.precise_git_fragment()?;
        if is_full_git_oid(revision) {
            return Some(PackageSourceInput::Git {
                canonical_url: source_id
                    .canonical_url()
                    .raw_canonicalized_url()
                    .as_str()
                    .to_owned(),
                revision,
                // Cargo's local artifact metadata also distinguishes the
                // declared reference form. Keep that invocation input while
                // requiring the full resolved revision above; a branch name
                // alone can never select a shared cache entry.
                reference: source_id
                    .git_reference()
                    .and_then(|reference| reference.pretty_ref(true))
                    .map(|reference| reference.to_string())
                    .unwrap_or_else(|| "default-branch".to_owned()),
            });
        }
    }

    None
}

/// Returns a stable identity for a package in a Git checkout or linked
/// worktree. `Repository::path` points at the per-worktree administrative
/// directory for linked worktrees, so resolve its optional `commondir` file
/// before recording the repository identity. Separate clones retain distinct
/// common Git directories and therefore do not collide merely because their
/// checked-out bytes and commits happen to match.
fn git_worktree_source_input(
    root: &Path,
    snapshot: String,
) -> Option<PackageSourceInput<'static>> {
    let repository = git2::Repository::discover(root).ok()?;
    let workdir = repository.workdir()?.canonicalize().ok()?;
    let relative_root = root.strip_prefix(&workdir).ok()?.to_str()?.to_owned();
    let revision = repository.head().ok()?.target()?.to_string();
    let git_dir = repository.path().canonicalize().ok()?;
    let common_dir = match fs::read_to_string(git_dir.join("commondir")) {
        Ok(relative) => git_dir.join(relative.trim()).canonicalize().ok()?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => git_dir,
        Err(_) => return None,
    };
    Some(PackageSourceInput::GitWorktree {
        repository: common_dir.to_str()?.to_owned(),
        revision,
        relative_root,
        snapshot,
    })
}

/// Computes a deterministic snapshot of a local package source.
///
/// Cargo's ordinary path fingerprint follows the compiler dep-info after the
/// fact. A persistent cache key must instead cover the complete package input
/// before rustc runs, including files a build script or proc-macro could read.
/// V0 therefore includes every regular file below the package root, excluding
/// only VCS metadata and Cargo's own `target` output directory. Symlinks and
/// other special files are deliberately unsupported: following them would
/// make the snapshot depend on an undeclared path outside the package.
fn path_source_snapshot(root: &Path) -> Option<String> {
    let mut files = Vec::new();
    collect_path_source_files(root, root, &mut files)?;
    files.sort_unstable_by(|left, right| left.0.cmp(&right.0));

    let mut digest = blake3::Hasher::new();
    for (relative, bytes) in files {
        let relative = relative.to_str()?;
        digest.update(&(relative.len() as u64).to_le_bytes());
        digest.update(relative.as_bytes());
        digest.update(&(bytes.len() as u64).to_le_bytes());
        digest.update(&bytes);
    }
    Some(digest.finalize().to_hex().to_string())
}

fn collect_build_script_files(
    root: &Path,
) -> CargoResult<Vec<(usize, PathBuf, PathBuf)>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    if !is_plain_directory(root) {
        return Err(io::Error::other("build-script output is not a directory").into());
    }
    let mut files = Vec::new();
    collect_build_script_files_inner(root, root, &mut files)?;
    files.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    let total = files.iter().try_fold(0_u64, |total, (_, path)| {
        Ok::<_, io::Error>(total.saturating_add(fs::metadata(path)?.len()))
    })?;
    if total > 64 * 1024 * 1024 {
        return Err(io::Error::other("build-script generated files exceed cargo-cas limit").into());
    }
    Ok(files
        .into_iter()
        .enumerate()
        .map(|(index, (relative, path))| (index, relative, path))
        .collect())
}

fn collect_build_script_files_inner(
    root: &Path,
    directory: &Path,
    files: &mut Vec<(PathBuf, PathBuf)>,
) -> CargoResult<()> {
    let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_unstable_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|_| io::Error::other("build-script output escaped OUT_DIR"))?
            .to_path_buf();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_build_script_files_inner(root, &path, files)?;
        } else if file_type.is_file() {
            if safe_relative_path(&relative).is_none() {
                return Err(io::Error::other("build-script generated path is unsafe").into());
            }
            files.push((relative, path));
        } else {
            return Err(io::Error::other("build-script generated a symlink or special file").into());
        }
    }
    Ok(())
}

fn safe_relative_path(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        return None;
    }
    let mut safe = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(component) => safe.push(component),
            _ => return None,
        }
    }
    (!safe.as_os_str().is_empty()).then_some(safe)
}

fn safe_join(root: &Path, relative: &str) -> CargoResult<PathBuf> {
    let relative = PathBuf::from(relative);
    let Some(relative) = safe_relative_path(&relative) else {
        return Err(io::Error::other("unsafe build-script cache path").into());
    };
    Ok(root.join(relative))
}

fn declared_environment_matches(environment: &BTreeMap<String, Option<String>>) -> bool {
    environment.iter().all(|(name, expected)| {
        let actual = std::env::var(name).ok();
        actual == *expected
    })
}

fn validate_build_script_entry(root: &Path, manifest: &BuildScriptCacheManifest) -> bool {
    if !is_plain_directory(root) || !is_plain_directory(&root.join(ARTIFACTS_DIRECTORY)) {
        return false;
    }
    if manifest.output_dir.is_empty() {
        return false;
    }
    let mut files = BTreeSet::new();
    manifest.files.iter().all(|file| {
        let Some(relative) = safe_relative_path(Path::new(&file.file)) else {
            return false;
        };
        if !files.insert(relative) || !is_safe_file_name(&file.artifact) {
            return false;
        }
        let path = root.join(ARTIFACTS_DIRECTORY).join(&file.artifact);
        fs::symlink_metadata(&path).is_ok_and(|metadata| {
            metadata.file_type().is_file()
                && metadata.len() == file.size
                && digest_file(&path).is_ok_and(|digest| digest == file.digest)
        })
    })
}

fn build_script_entry_is_valid(
    root: &Path,
    key: &ActionKey,
    identity: &BuildScriptIdentity,
) -> bool {
    let manifest_path = root.join(BUILD_SCRIPT_MANIFEST_FILE);
    let Ok(bytes) = read_regular_file(&manifest_path) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_slice::<BuildScriptCacheManifest>(&bytes) else {
        return false;
    };
    manifest.format_version == CACHE_FORMAT_VERSION
        && manifest.action_key == key.as_str()
        && manifest.identity == *identity
        && validate_build_script_entry(root, &manifest)
}

fn collect_path_source_files(
    root: &Path,
    directory: &Path,
    files: &mut Vec<(PathBuf, Vec<u8>)>,
) -> Option<()> {
    let mut entries = fs::read_dir(directory)
        .ok()?
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    entries.sort_unstable_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let relative = path.strip_prefix(root).ok()?.to_path_buf();
        let name = entry.file_name();
        if name == ".git" || name == ".hg" || name == ".svn" || name == "target" {
            continue;
        }
        if entry.file_type().ok()?.is_dir() {
            collect_path_source_files(root, &path, files)?;
        } else if entry.file_type().ok()?.is_file() {
            files.push((relative, fs::read(path).ok()?));
        } else {
            // A symlink or special file could expose an input outside the
            // package root. A conservative miss is safer than a false hit.
            return None;
        }
    }
    Some(())
}

fn is_full_git_oid(revision: &str) -> bool {
    matches!(revision.len(), 40 | 64) && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
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
        .filter_map(|output| {
            let role = match output.flavor {
                FileFlavor::Rmeta => ArtifactRole::Rmeta,
                FileFlavor::Linkable if unit.mode == CompileMode::Build => ArtifactRole::Linkable,
                _ => return None,
            };
            Some(ArtifactPath {
                role,
                source: output.path.clone(),
                destination: output.path.clone(),
                required: true,
            })
        })
        .collect::<Vec<_>>();
    let dep_info = build_runner.files().fingerprint_file_path(unit, "dep-");
    artifacts.push(ArtifactPath {
        role: ArtifactRole::DepInfo,
        source: dep_info.clone(),
        destination: dep_info,
        required: true,
    });
    let output_cache = build_runner.files().message_cache_path(unit);
    artifacts.push(ArtifactPath {
        role: ArtifactRole::OutputCache,
        source: output_cache.clone(),
        destination: output_cache,
        required: false,
    });
    // The scheduler may unblock a metadata-only dependent as soon as its
    // `.rmeta` has been restored. Keep that role first; Cargo bookkeeping can
    // follow without delaying the pipeline edge.
    artifacts.sort_unstable_by_key(|artifact| match artifact.role {
        ArtifactRole::Rmeta => 0,
        ArtifactRole::Linkable => 1,
        ArtifactRole::DepInfo => 2,
        ArtifactRole::OutputCache => 3,
    });
    Ok(artifacts)
}

fn cache_root(build_runner: &BuildRunner<'_, '_>) -> PathBuf {
    cache_root_for_gctx(build_runner.bcx.gctx)
}

fn cache_root_for_gctx(gctx: &crate::GlobalContext) -> PathBuf {
    gctx.home()
        .join("cache")
        .join(CACHE_DIRECTORY)
        .into_path_unlocked()
}

/// Removes whole immutable entries based on their access age or aggregate
/// size. Cargo's normal compilation path holds the package-cache shared lock;
/// `cargo clean gc` holds its mutate-exclusive counterpart, so this sweep
/// cannot race an active lookup, restore, publication, or per-key writer.
pub(crate) fn gc(
    clean_ctx: &mut crate::ops::CleanContext<'_>,
    max_age: Option<Duration>,
    max_size: Option<u64>,
) -> CargoResult<()> {
    if max_age.is_none() && max_size.is_none() {
        return Ok(());
    }

    let root = cache_root_for_gctx(clean_ctx.gctx);
    let mut entries = cache_entries(&root)?;
    let now = SystemTime::now();
    let mut remove = Vec::new();
    // GC owns Cargo's package-cache mutation lock, so no cache action is
    // active. Clear incomplete publication directories and per-key lock files
    // before applying a size policy; otherwise a killed writer could retain
    // unaccounted cache bytes forever.
    remove_temporary_entries(&root, &mut remove)?;
    remove_cache_lock_files(&root, &mut remove)?;

    if let Some(max_age) = max_age {
        let cutoff = now.checked_sub(max_age).unwrap_or(SystemTime::UNIX_EPOCH);
        entries.retain(|entry| {
            if entry.last_used < cutoff {
                remove.push(entry.path.clone());
                if let Some(access) = &entry.access {
                    remove.push(access.clone());
                }
                false
            } else {
                true
            }
        });
    }

    if let Some(max_size) = max_size {
        entries.sort_unstable_by_key(|entry| entry.last_used);
        let mut total_size = entries.iter().map(|entry| entry.size).sum::<u64>();
        for entry in entries {
            if total_size <= max_size {
                break;
            }
            total_size = total_size.saturating_sub(entry.size);
            remove.push(entry.path);
            if let Some(access) = entry.access {
                remove.push(access);
            }
        }
    }

    remove.sort();
    remove.dedup();
    clean_ctx.remove_paths(&remove)?;
    Ok(())
}

struct CacheGcEntry {
    path: PathBuf,
    access: Option<PathBuf>,
    last_used: SystemTime,
    size: u64,
}

fn cache_entries(root: &Path) -> CargoResult<Vec<CacheGcEntry>> {
    match fs::symlink_metadata(root) {
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(io::Error::other("cargo-cas cache root is not a directory").into());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    let access_root = cache_subdirectory(root, ACCESS_DIRECTORY)?;
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_type().is_ok_and(|kind| kind.is_dir())
                && is_action_key_name(&entry.file_name().to_string_lossy())
        })
        .map(|entry| {
            let path = entry.path();
            let access = access_root
                .as_ref()
                .map(|access_root| access_root.join(entry.file_name()));
            let last_used = access
                .as_ref()
                .and_then(|access| {
                    fs::symlink_metadata(access)
                        .ok()
                        .filter(|metadata| metadata.file_type().is_file())
                        .and_then(|metadata| metadata.modified().ok())
                })
                .or_else(|| {
                    fs::metadata(&path)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                })
                // Unknown timestamps must be retained; the cache is only an
                // optimization, but GC should prefer a harmless miss delay to
                // deleting an entry it cannot age safely.
                .unwrap_or(SystemTime::now());
            Ok(CacheGcEntry {
                size: directory_size(&path)?,
                path,
                access,
                last_used,
            })
        })
        .collect()
}

fn remove_temporary_entries(root: &Path, remove: &mut Vec<PathBuf>) -> CargoResult<()> {
    let Some(temporary_root) = cache_subdirectory(root, "tmp")? else {
        return Ok(());
    };
    let entries = fs::read_dir(&temporary_root)?;
    for entry in entries.filter_map(Result::ok) {
        if entry.file_type()?.is_dir() {
            remove.push(entry.path());
        }
    }
    Ok(())
}

fn remove_cache_lock_files(root: &Path, remove: &mut Vec<PathBuf>) -> CargoResult<()> {
    let Some(locks_root) = cache_subdirectory(root, LOCKS_DIRECTORY)? else {
        return Ok(());
    };
    for entry in fs::read_dir(locks_root)?.filter_map(Result::ok) {
        if entry.file_type()?.is_file() {
            remove.push(entry.path());
        }
    }
    Ok(())
}

fn directory_size(path: &Path) -> CargoResult<u64> {
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .try_fold(0_u64, |size, entry| {
            Ok(size.saturating_add(entry.metadata()?.len()))
        })
}

fn is_action_key_name(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn mark_used(cache: &Path, key: &str) {
    let result = (|| -> io::Result<()> {
        let access = ensure_cache_subdirectory(cache, ACCESS_DIRECTORY)?.join(key);
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;

            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(access)?;
        if !file.metadata()?.file_type().is_file() {
            return Err(io::Error::other(
                "cargo-cas access entry is not a regular file",
            ));
        }
        file.set_times(fs::FileTimes::new().set_modified(SystemTime::now()))
    })();
    if let Err(error) = result {
        debug!(error = ?error, "failed to record cargo-cas last use");
    }
}

/// Creates the experimental cache root only when its final component is an
/// ordinary directory. In particular, never follow a symlink substituted at
/// `$CARGO_HOME/cache/cargo-cas-v1`: cache infrastructure failure must fall
/// back to normal compilation, not redirect locks or staged artifacts outside
/// the configured Cargo home.
fn ensure_cache_root(cache: &Path) -> io::Result<()> {
    match fs::symlink_metadata(cache) {
        Ok(metadata) if metadata.file_type().is_dir() => return Ok(()),
        Ok(_) => return Err(io::Error::other("cargo-cas cache root is not a directory")),
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        Err(_) => {}
    }
    let parent = cache
        .parent()
        .expect("cargo-cas cache root has a cache-directory parent");
    fs::create_dir_all(parent)?;
    match fs::create_dir(cache) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    if is_plain_directory(cache) {
        Ok(())
    } else {
        Err(io::Error::other("cargo-cas cache root is not a directory"))
    }
}

/// Returns a direct child of the cache root only when it is an ordinary
/// directory. Cache-internal mutable directories are write boundaries just as
/// much as the root itself: following a substituted `locks`, `tmp`, or
/// `access` symlink would redirect an otherwise best-effort cache operation.
fn cache_subdirectory(cache: &Path, name: &str) -> io::Result<Option<PathBuf>> {
    let path = cache.join(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(Some(path)),
        Ok(_) => Err(io::Error::other(format!(
            "cargo-cas {name} directory is not a directory"
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn ensure_cache_subdirectory(cache: &Path, name: &str) -> io::Result<PathBuf> {
    ensure_cache_root(cache)?;
    if let Some(path) = cache_subdirectory(cache, name)? {
        return Ok(path);
    }
    let path = cache.join(name);
    match fs::create_dir(&path) {
        Ok(()) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            cache_subdirectory(cache, name)?.ok_or_else(|| {
                io::Error::other(format!(
                    "cargo-cas {name} directory disappeared during creation"
                ))
            })
        }
        Err(error) => Err(error),
    }
}

fn validate_manifest(root: &Path, manifest: &CacheManifestV1) -> bool {
    if !is_plain_directory(root) || !is_plain_directory(&root.join(ARTIFACTS_DIRECTORY)) {
        return false;
    }
    let mut files = BTreeSet::new();
    manifest.artifacts.iter().all(|artifact| {
        if !is_safe_file_name(&artifact.file) || !files.insert(&artifact.file) {
            return false;
        }
        let path = root.join(ARTIFACTS_DIRECTORY).join(&artifact.file);
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            return false;
        };
        metadata.is_file()
            && metadata.len() == artifact.size
            && digest_file(&path).is_ok_and(|digest| digest == artifact.digest)
    })
}

/// Checks every entry component before accepting a cache hit.  The manifest
/// only stores relative artifact names, but a symlink at the entry boundary or
/// in its artifact set could otherwise redirect Cargo outside its cache root.
fn entry_is_valid(
    root: &Path,
    key: &ActionKey,
    identity: &ManifestIdentityV1,
    expected: &[ArtifactPath],
) -> bool {
    if !is_plain_directory(root) {
        return false;
    }
    let manifest_path = root.join(MANIFEST_FILE);
    let Ok(manifest_bytes) = read_regular_file(&manifest_path) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_slice::<CacheManifestV1>(&manifest_bytes) else {
        return false;
    };
    manifest.format_version == CACHE_FORMAT_VERSION
        && manifest.action_key == key.as_str()
        && manifest.identity == *identity
        && validate_manifest(root, &manifest)
        && manifest_matches_expected(&manifest, expected)
}

fn is_plain_directory(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

fn remove_entry(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn read_regular_file(path: &Path) -> io::Result<Vec<u8>> {
    let mut file = open_regular_file(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn open_regular_file(path: &Path) -> io::Result<File> {
    if !fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(io::Error::other("cargo-cas entry is not a regular file"));
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(io::Error::other("cargo-cas entry is not a regular file"));
    }
    Ok(file)
}

/// Restores an entry artifact through a no-follow descriptor and proves that
/// the bytes observed at restore time are still the manifest bytes validated at
/// lookup time. macOS first uses its copy-on-write clone primitive so each
/// worktree shares immutable cache blocks; other filesystems use the streaming
/// copy fallback. A clone never shares a mutable inode with Cargo's target.
fn copy_verified_artifact(
    source: &Path,
    destination: &Path,
    expected_size: u64,
    expected_digest: &str,
) -> io::Result<()> {
    let mut source = open_regular_file(source)?;

    if fs::symlink_metadata(destination).is_ok() {
        fs::remove_file(destination)?;
    }
    if try_clone_from_open_file(&source, destination)? {
        if verify_artifact(destination, expected_size, expected_digest).is_ok() {
            debug!("cargo-cas restore: copy-on-write clone");
            return Ok(());
        }
        let _ = fs::remove_file(destination);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cargo-cas artifact changed after manifest validation",
        ));
    }

    let mut destination = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(destination)?;
    let mut digest = blake3::Hasher::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        destination.write_all(&buffer[..count])?;
        digest.update(&buffer[..count]);
        size = size.saturating_add(count as u64);
    }
    destination.flush()?;
    debug!("cargo-cas restore: streaming copy fallback");
    if size != expected_size || digest.finalize().to_hex().as_str() != expected_digest {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cargo-cas artifact changed after manifest validation",
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn try_clone_from_open_file(source: &File, destination: &Path) -> io::Result<bool> {
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let result = unsafe {
        libc::fclonefileat(
            source.as_raw_fd(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            0,
        )
    };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ENOTSUP | libc::EEXIST | libc::EXDEV) => Ok(false),
        _ => Err(error),
    }
}

#[cfg(not(target_os = "macos"))]
fn try_clone_from_open_file(_source: &File, _destination: &Path) -> io::Result<bool> {
    Ok(false)
}

fn verify_artifact(path: &Path, expected_size: u64, expected_digest: &str) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.len() != expected_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cargo-cas artifact changed after manifest validation",
        ));
    }
    if digest_file(path)? != expected_digest {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cargo-cas artifact changed after manifest validation",
        ));
    }
    Ok(())
}

fn manifest_matches_expected(manifest: &CacheManifestV1, expected: &[ArtifactPath]) -> bool {
    manifest
        .artifacts
        .iter()
        .enumerate()
        .all(|(index, cached)| {
            expected
                .iter()
                .any(|expected| artifact_matches_expected(cached, expected))
                && manifest.artifacts[..index]
                    .iter()
                    .all(|previous| previous.role != cached.role)
        })
        && expected
            .iter()
            .filter(|expected| expected.required)
            .all(|expected| {
                manifest
                    .artifacts
                    .iter()
                    .any(|cached| artifact_matches_expected(cached, expected))
            })
}

fn artifact_matches_expected(cached: &CachedArtifact, expected: &ArtifactPath) -> bool {
    cached.role == expected.role && cached.output_file_name == file_name(&expected.destination)
}

fn digest_file(path: &Path) -> io::Result<String> {
    let mut file = open_regular_file(path)?;
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
