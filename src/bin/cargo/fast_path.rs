//! A conservative, Cargo-backed no-op fast path.
//!
//! The regular Cargo path remains authoritative. After a successful, narrowly
//! shaped `build` or `check`, this module records the input and target-file
//! identities that Cargo just validated. The next identical invocation can
//! validate that receipt before constructing Cargo's global context and return
//! success without walking the manifest and unit graph. Any uncertainty is a
//! normal miss and falls through to Cargo.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use cargo_util_terminal::Shell;
use serde::{Deserialize, Serialize};

const FORMAT_VERSION: u8 = 1;
const STATE_DIRECTORY: &str = ".cargo-cas";
const STATE_FILE: &str = "noop-v1.json";
const TARGET_LOCK: &str = ".cargo-lock";
const DISABLE_VARIABLE: &str = "CARGO_CAS_DISABLE_FAST_NOOP";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommandKind {
    Build,
    Check,
}

impl CommandKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Check => "check",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Receipt {
    format_version: u8,
    project: PathBuf,
    target: PathBuf,
    command: Vec<String>,
    context: String,
    inputs: Vec<FileState>,
    outputs: Vec<FileState>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct FileState {
    path: PathBuf,
    stamp: FileStamp,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct FileStamp {
    size: u64,
    modified_ns: u128,
    #[serde(default)]
    device: u64,
    #[serde(default)]
    inode: u64,
}

/// Returns `true` only when a complete receipt proves that this exact narrow
/// invocation can be treated as Cargo's successful no-op result.
pub(crate) fn try_noop() -> bool {
    if env::var_os(DISABLE_VARIABLE).is_some() {
        return false;
    }
    let Some((kind, command)) = supported_command() else {
        return false;
    };
    let Ok(project) = env::current_dir().and_then(|path| fs::canonicalize(path)) else {
        return false;
    };
    let Some(target) = default_target(&project) else {
        return false;
    };
    let Some(state_path) = state_path(&target) else {
        return false;
    };
    let Ok(_lock) = target_lock(&target) else {
        return false;
    };
    let Ok(receipt) = read_receipt(&state_path) else {
        return false;
    };
    if receipt.format_version != FORMAT_VERSION
        || receipt.project != project
        || receipt.target != target
        || receipt.command != command
        || receipt.context != invocation_context(&project, kind, &command)
    {
        return false;
    }
    if !validate_inputs(&receipt.inputs)
        || !validate_outputs(&receipt.outputs)
        || has_cached_diagnostics(&target)
    {
        return false;
    }

    // Keep the default command's visible completion contract. The elapsed
    // value is intentionally zero: no compiler work was performed. Quiet
    // invocations remain silent, as they are in ordinary Cargo.
    if !command.iter().any(|arg| arg == "-q" || arg == "--quiet") {
        let mut shell = Shell::new();
        let _ = shell.status("Finished", "`dev` profile [unoptimized] target(s) in 0.00s");
    }
    true
}

/// Records a successful ordinary invocation. Recording is best-effort and can
/// never turn a successful Cargo command into a failure.
pub(crate) fn record_success() {
    if env::var_os(DISABLE_VARIABLE).is_some() {
        return;
    }
    let Some((kind, command)) = supported_command() else {
        return;
    };
    let Ok(project) = env::current_dir().and_then(|path| fs::canonicalize(path)) else {
        return;
    };
    let Some(target) = default_target(&project) else {
        return;
    };
    let Some(state_path) = state_path(&target) else {
        return;
    };
    let Ok(_lock) = target_lock(&target) else {
        return;
    };
    let Ok(inputs) = collect_inputs(&project, &target) else {
        return;
    };
    let Ok(outputs) = collect_files(&target, Some(STATE_DIRECTORY)) else {
        return;
    };
    if outputs.is_empty() || has_cached_diagnostics(&target) {
        return;
    }
    let receipt = Receipt {
        format_version: FORMAT_VERSION,
        project: project.clone(),
        target,
        command: command.clone(),
        context: invocation_context(&project, kind, &command),
        inputs,
        outputs,
    };
    let Ok(bytes) = serde_json::to_vec(&receipt) else {
        return;
    };
    let temporary = state_path.with_extension(format!("json.{}.tmp", std::process::id()));
    let Some(state_dir) = state_path.parent() else {
        return;
    };
    if ensure_state_directory(state_dir).is_ok() && fs::write(&temporary, bytes).is_ok() {
        let _ = fs::rename(temporary, state_path);
    }
}

fn supported_command() -> Option<(CommandKind, Vec<String>)> {
    let mut args = env::args_os();
    args.next()?;
    let command = args
        .map(|arg| arg.into_string().ok())
        .collect::<Option<Vec<_>>>()?;
    let kind = match command.first()?.as_str() {
        "build" => CommandKind::Build,
        "check" => CommandKind::Check,
        _ => return None,
    };
    if command.iter().skip(1).any(|arg| {
        !matches!(arg.as_str(), "-q" | "--quiet")
    }) {
        return None;
    }
    Some((kind, command))
}

fn default_target(project: &Path) -> Option<PathBuf> {
    if env::var_os("CARGO_TARGET_DIR").is_some()
        || env::var_os("CARGO_BUILD_DIR").is_some()
        || !project.join("Cargo.toml").is_file()
    {
        return None;
    }
    let manifest = fs::read_to_string(project.join("Cargo.toml")).ok()?;
    if manifest.contains("[workspace") {
        return None;
    }
    if configuration_selects_another_target(project) {
        return None;
    }
    let target = project.join("target");
    let metadata = fs::symlink_metadata(&target).ok()?;
    metadata.file_type().is_dir().then(|| target)
}

fn configuration_selects_another_target(project: &Path) -> bool {
    configuration_paths(project).into_iter().any(|path| {
        fs::read_to_string(path).is_ok_and(|contents| {
            contents.lines().any(|line| {
                let line = line.trim_start();
                line.starts_with("target-dir")
                    || line.starts_with("build-dir")
                    || line.starts_with("target =")
            })
        })
    })
}

fn configuration_paths(project: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut current = Some(project);
    while let Some(directory) = current {
        let cargo = directory.join(".cargo");
        paths.push(cargo.join("config"));
        paths.push(cargo.join("config.toml"));
        current = directory.parent();
    }
    paths
}

fn state_path(target: &Path) -> Option<PathBuf> {
    let state_dir = target.join(STATE_DIRECTORY);
    match fs::symlink_metadata(&state_dir) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => return None,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Some(state_dir.join(STATE_FILE)),
        Err(_) => return None,
    }
    Some(state_dir.join(STATE_FILE))
}

fn ensure_state_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(io::Error::other("fast-path state directory is not a directory")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            match fs::symlink_metadata(path)? {
                metadata if metadata.file_type().is_dir() => Ok(()),
                _ => Err(io::Error::other("fast-path state directory is not a directory")),
            }
        }
        Err(error) => Err(error),
    }
}

fn read_receipt(path: &Path) -> io::Result<Receipt> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn invocation_context(project: &Path, kind: CommandKind, command: &[String]) -> String {
    let mut values = BTreeMap::new();
    for (key, value) in env::vars_os() {
        let Some(key) = key.into_string().ok() else {
            continue;
        };
        if matches!(key.as_str(), "PWD" | "OLDPWD" | "SHLVL" | "_" | "CARGO_MAKEFLAGS") {
            continue;
        }
        let Some(value) = value.into_string().ok() else {
            continue;
        };
        values.insert(key, value);
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(project.to_string_lossy().as_bytes());
    hasher.update(kind.as_str().as_bytes());
    for arg in command {
        hasher.update(arg.as_bytes());
        hasher.update(&[0]);
    }
    for (key, value) in values {
        hasher.update(key.as_bytes());
        hasher.update(&[0]);
        hasher.update(value.as_bytes());
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex().to_string()
}

fn collect_inputs(project: &Path, target: &Path) -> io::Result<Vec<FileState>> {
    let mut paths = BTreeSet::new();
    // Cargo's source dependency information is the authoritative list of
    // files that rustc actually consumed. Keeping this list dep-info-driven
    // avoids walking an entire repository (and still records the files Cargo
    // uses to resolve the package before rustc runs).
    for name in ["Cargo.toml", "Cargo.lock"] {
        let path = project.join(name);
        if path.is_file() {
            paths.insert(fs::canonicalize(path)?);
        }
    }
    for path in configuration_paths(project) {
        if path.is_file() {
            paths.insert(fs::canonicalize(path)?);
        }
    }
    for path in cargo_home_configuration_paths() {
        if path.is_file() {
            paths.insert(fs::canonicalize(path)?);
        }
    }
    for path in toolchain_paths(project) {
        if path.is_file() {
            paths.insert(fs::canonicalize(path)?);
        }
    }
    for dep_info in collect_files(target, None)? {
        if dep_info.path.extension().and_then(|extension| extension.to_str()) != Some("d") {
            continue;
        }
        let Ok(contents) = fs::read_to_string(&dep_info.path) else {
            continue;
        };
        for path in dep_info_paths(&contents) {
            if path.starts_with(target) || !path.is_file() {
                continue;
            }
            let path = fs::canonicalize(path)?;
            paths.insert(path.clone());
            add_nearby_manifests(&path, &mut paths)?;
        }
    }
    paths
        .into_iter()
        .map(|path| Ok(FileState { stamp: file_stamp(&path)?, path }))
        .collect()
}

fn cargo_home_configuration_paths() -> Vec<PathBuf> {
    let Some(home) = env::var_os("CARGO_HOME")
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo").into_os_string()))
    else {
        return Vec::new();
    };
    let home = PathBuf::from(home);
    ["config", "config.toml"]
        .into_iter()
        .map(|name| home.join(name))
        .collect()
}

fn toolchain_paths(project: &Path) -> Vec<PathBuf> {
    ["rust-toolchain", "rust-toolchain.toml"]
        .into_iter()
        .map(|name| project.join(name))
        .collect()
}

fn add_nearby_manifests(path: &Path, manifests: &mut BTreeSet<PathBuf>) -> io::Result<()> {
    let mut current = path.parent();
    for _ in 0..8 {
        let Some(directory) = current else {
            break;
        };
        let manifest = directory.join("Cargo.toml");
        if manifest.is_file() {
            manifests.insert(fs::canonicalize(manifest)?);
            break;
        }
        current = directory.parent();
    }
    Ok(())
}

fn dep_info_paths(contents: &str) -> impl Iterator<Item = PathBuf> + '_ {
    contents.lines().flat_map(|line| {
        let body = line.split_once(':').map_or(line, |(_, body)| body);
        body.split_whitespace().filter_map(|token| {
            let token = token.replace("\\ ", " ").replace("\\\\", "\\");
            (!token.is_empty()).then(|| PathBuf::from(token))
        })
    })
}

fn collect_files(root: &Path, excluded_directory: Option<&str>) -> io::Result<Vec<FileState>> {
    let mut paths = BTreeSet::new();
    collect_files_into(root, excluded_directory, &mut paths)?;
    paths
        .into_iter()
        .map(|path| {
            Ok(FileState {
                stamp: target_file_stamp(&path)?,
                path,
            })
        })
        .collect()
}

fn collect_files_into(
    root: &Path,
    excluded_directory: Option<&str>,
    paths: &mut BTreeSet<PathBuf>,
) -> io::Result<()> {
    let metadata = fs::symlink_metadata(root)?;
    if metadata.file_type().is_file() {
        paths.insert(fs::canonicalize(root)?);
        return Ok(());
    }
    if !metadata.file_type().is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if excluded_directory.is_some_and(|name| entry.file_name() == name)
            || entry.file_name() == ".git"
        {
            continue;
        }
        collect_files_into(&path, excluded_directory, paths)?;
    }
    Ok(())
}

fn file_stamp(path: &Path) -> io::Result<FileStamp> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::other("fast-path input is not a regular file"));
    }
    let modified = metadata.modified()?.duration_since(UNIX_EPOCH).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "fast-path file has pre-epoch mtime")
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        return Ok(FileStamp {
            size: metadata.len(),
            modified_ns: modified.as_nanos(),
            device: metadata.dev(),
            inode: metadata.ino(),
        });
    }
    #[cfg(not(unix))]
    Ok(FileStamp {
        size: metadata.len(),
        modified_ns: modified.as_nanos(),
        device: 0,
        inode: 0,
    })
}

fn validate_inputs(expected: &[FileState]) -> bool {
    validate_files(expected)
}

fn validate_outputs(expected: &[FileState]) -> bool {
    expected.iter().all(|file| {
        target_file_stamp(&file.path).is_ok_and(|stamp| stamp == file.stamp)
    })
}

fn validate_files(expected: &[FileState]) -> bool {
    expected.iter().all(|file| {
        file_stamp(&file.path).is_ok_and(|stamp| stamp == file.stamp)
    })
}

fn target_file_stamp(path: &Path) -> io::Result<FileStamp> {
    let mut stamp = file_stamp(path)?;
    // Cargo rewrites dep-info files during an otherwise fresh build. Their
    // contents and size are the useful evidence; mtime alone should not make
    // a proven receipt miss after that harmless rewrite.
    if path.extension().and_then(|extension| extension.to_str()) == Some("d") {
        stamp.modified_ns = 0;
    }
    Ok(stamp)
}

struct TargetLock {
    _file: File,
}

fn target_lock(target: &Path) -> io::Result<TargetLock> {
    // The default Cargo layout places the artifact-directory lock in
    // `target/debug`. This is the lock Cargo itself holds while checking the
    // artifacts, so taking it here closes the check/publish race with Cargo.
    let path = target.join("debug").join(TARGET_LOCK);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)?;
    #[cfg(not(unix))]
    {
        let _ = file;
        return Err(io::Error::other("no-op receipt locking is unsupported on this platform"));
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;

        // Cargo uses the same lock file for its target-directory lifecycle.
        // Holding it exclusively makes the receipt check/publish atomic with
        // respect to ordinary Cargo builds and cleans.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(TargetLock { _file: file })
}

fn has_cached_diagnostics(target: &Path) -> bool {
    let Ok(files) = collect_files(target, Some(STATE_DIRECTORY)) else {
        return true;
    };
    files.into_iter().any(|file| {
        let name = file.path.file_name().and_then(|name| name.to_str());
        let diagnostic = name.is_some_and(|name| name == "output" || name.starts_with("output-"));
        diagnostic && file.stamp.size != 0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_shape_is_narrow() {
        assert_eq!(CommandKind::Build.as_str(), "build");
    }

    #[test]
    fn dep_info_parser_ignores_the_target_side() {
        let paths = dep_info_paths("target/libfoo.rlib: src/lib.rs src/module.rs")
            .collect::<Vec<_>>();
        assert_eq!(paths, vec![PathBuf::from("src/lib.rs"), PathBuf::from("src/module.rs")]);
    }

    #[test]
    fn receipt_validation_rejects_a_missing_file() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("artifact");
        fs::write(&path, b"artifact").expect("artifact");
        let expected = vec![FileState {
            stamp: file_stamp(&path).expect("artifact stamp"),
            path: path.clone(),
        }];
        assert!(validate_files(&expected));
        fs::remove_file(path).expect("remove artifact");
        assert!(!validate_files(&expected));
    }
}
