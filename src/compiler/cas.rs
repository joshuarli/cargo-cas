//! The experimental immutable-source artifact cache.
//!
//! This deliberately materializes a verified cache entry into Cargo's normal
//! build directory.  The local [`fingerprint`](super::fingerprint) and job
//! queue therefore remain the authority for scheduling and freshness; this
//! module only substitutes the work normally performed by `rustc`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use cargo_util::paths;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use super::fingerprint;
use super::job_queue::Work;
use super::{BuildRunner, CompileMode, CrateType, FileFlavor, Lto, Unit};
use crate::util::CargoResult;

const CACHE_FORMAT_VERSION: u8 = 1;
const CACHE_DIRECTORY: &str = "cargo-cas-v1";
const MANIFEST_FILE: &str = "manifest.json";
const ARTIFACTS_DIRECTORY: &str = "artifacts";
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
pub(crate) fn prepare(
    build_runner: &BuildRunner<'_, '_>,
    unit: &Unit,
) -> CargoResult<Option<CacheAction>> {
    let key = match action_key(build_runner, unit) {
        Some(key) => key,
        None => {
            let reason = ineligibility_reason(build_runner, unit)
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
        let root = self.cache.join(self.key.as_str());
        if !is_plain_directory(&root) {
            self.stats.miss();
            debug!(key = self.key.as_str(), "cargo-cas miss: entry absent");
            return None;
        }
        let manifest_path = root.join(MANIFEST_FILE);
        let Ok(manifest_bytes) = read_regular_file(&manifest_path) else {
            self.stats.miss();
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
    pub(crate) fn restore_or_compile(&self, entry: CacheEntry, normal_work: Work) -> Work {
        debug_assert!(manifest_matches_expected(&entry.manifest, &self.artifacts));

        let restores = entry
            .manifest
            .artifacts
            .iter()
            .zip(&self.artifacts)
            .map(|(cached, expected)| {
                (
                    entry.root.join(ARTIFACTS_DIRECTORY).join(&cached.file),
                    expected.destination.clone(),
                    cached.role,
                )
            })
            .collect::<Vec<_>>();
        let stats = self.stats.clone();

        Work::new(move |state| {
            let restored: CargoResult<()> = (|| {
                for (source, destination, role) in restores {
                    let parent = destination.parent().expect("Cargo output path has parent");
                    paths::create_dir_all(parent)?;
                    // Cache entries are immutable. Copy rather than hardlink so a
                    // later local output cleanup or compiler invocation can never
                    // mutate a globally cached inode.
                    fs::copy(source, destination)?;
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
                Ok(()) => Ok(()),
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
    pub(crate) fn coordinate(self, normal_work: Work, allow_hit: bool) -> Work {
        Work::new(move |state| match self.lock() {
            Ok(_lock) => {
                if allow_hit {
                    if let Some(entry) = self.lookup() {
                        self.stats.duplicate_build_avoidance();
                        return self.restore_or_compile(entry, normal_work).call(state);
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
        let lock_path = self
            .cache
            .join(LOCKS_DIRECTORY)
            .join(format!("{}.lock", self.key.as_str()));
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)?;
        }
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
        package_root: unit.pkg.root().to_path_buf(),
        build_root: build_runner.bcx.ws.build_dir().into_path_unlocked(),
        artifacts: artifact_paths(build_runner, unit)?,
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
        .map(|dependency| action_key(build_runner, &dependency.unit).map(|key| key.0))
        .collect::<Option<Vec<_>>>()?;
    dependency_action_keys.sort_unstable();
    Some(ManifestIdentityV1 {
        package_id: unit.pkg.package_id().to_string(),
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
    if source_id.is_path() {
        return Some("path source");
    }
    if !source_id.is_registry() && !source_id.is_git() {
        return Some("source is not an immutable registry or git source");
    }
    if package_source_input(unit).is_none() {
        return Some("source lacks an immutable content identity");
    }
    if unit.is_std {
        return Some("standard library unit");
    }
    if unit.pkg.has_custom_build() {
        return Some("package has a build script");
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
            })
        })
        .collect::<Vec<_>>();
    let dep_info = build_runner.files().fingerprint_file_path(unit, "dep-");
    artifacts.push(ArtifactPath {
        role: ArtifactRole::DepInfo,
        source: dep_info.clone(),
        destination: dep_info,
    });
    // The scheduler may unblock a metadata-only dependent as soon as its
    // `.rmeta` has been restored. Keep that role first; Cargo bookkeeping can
    // follow without delaying the pipeline edge.
    artifacts.sort_unstable_by_key(|artifact| match artifact.role {
        ArtifactRole::Rmeta => 0,
        ArtifactRole::Linkable => 1,
        ArtifactRole::DepInfo => 2,
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

    if let Some(max_age) = max_age {
        let cutoff = now.checked_sub(max_age).unwrap_or(SystemTime::UNIX_EPOCH);
        entries.retain(|entry| {
            if entry.last_used < cutoff {
                remove.push(entry.path.clone());
                if entry.access.is_file() {
                    remove.push(entry.access.clone());
                }
                false
            } else {
                true
            }
        });
        remove_abandoned_temporary_entries(&root, cutoff, &mut remove)?;
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
            if entry.access.is_file() {
                remove.push(entry.access);
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
    access: PathBuf,
    last_used: SystemTime,
    size: u64,
}

fn cache_entries(root: &Path) -> CargoResult<Vec<CacheGcEntry>> {
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
            let access = root.join(ACCESS_DIRECTORY).join(entry.file_name());
            let last_used = fs::metadata(&access)
                .and_then(|metadata| metadata.modified())
                .or_else(|_| fs::metadata(&path).and_then(|metadata| metadata.modified()))
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

fn remove_abandoned_temporary_entries(
    root: &Path,
    cutoff: SystemTime,
    remove: &mut Vec<PathBuf>,
) -> CargoResult<()> {
    let temporary_root = root.join("tmp");
    let entries = match fs::read_dir(&temporary_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries.filter_map(Result::ok) {
        if entry.file_type()?.is_dir()
            && entry
                .metadata()?
                .modified()
                .is_ok_and(|modified| modified < cutoff)
        {
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
    let access = cache.join(ACCESS_DIRECTORY).join(key);
    let result = (|| -> io::Result<()> {
        let parent = access.parent().expect("cargo-cas access path has parent");
        fs::create_dir_all(parent)?;
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

fn manifest_matches_expected(manifest: &CacheManifestV1, expected: &[ArtifactPath]) -> bool {
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
