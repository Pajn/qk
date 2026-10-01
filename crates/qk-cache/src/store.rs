use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Component, Path};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::{
    Cache, combine_outputs, directory_output, file_output,
    hash::digest_file,
    link_output,
    paths::{self, Outputs},
};

#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u32,
    key: String,
    log: String,
    artifacts: BTreeMap<String, Artifact>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub(crate) enum Artifact {
    File { blob: String, mode: u32 },
    Directory { mode: u32 },
    Symlink { target: String, directory: bool },
}

impl Manifest {
    /// The output fingerprint of the stored artifacts, without reading any file.
    fn output_fingerprint(&self, declared: bool) -> Result<String> {
        let files = self
            .artifacts
            .iter()
            .map(|(path, artifact)| {
                let value = match artifact {
                    Artifact::File { blob, mode } => file_output(blob, *mode),
                    Artifact::Directory { .. } => directory_output(),
                    Artifact::Symlink { target, .. } => link_output(target),
                };
                (path.clone(), value)
            })
            .collect();
        combine_outputs(&self.key, &files, declared)
    }
}

pub(crate) fn mode(metadata: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o777
    }
    #[cfg(not(unix))]
    {
        u32::from(metadata.permissions().readonly())
    }
}

pub(crate) fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))?;
    }
    #[cfg(not(unix))]
    {
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_readonly(mode != 0);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

pub(crate) fn validate_link(path: &str, target: &str) -> Result<()> {
    if target.contains(['\\', ':', '\0']) || Path::new(target).is_absolute() {
        bail!("cache symlinks must be relative");
    }
    let mut depth = Path::new(path)
        .parent()
        .map(|path| path.components().count())
        .unwrap_or_default();
    for part in Path::new(target).components() {
        match part {
            Component::Normal(part) if part != ".git" && part != ".qk" => depth += 1,
            Component::CurDir => {}
            Component::ParentDir if depth > 0 => depth -= 1,
            _ => bail!("cache symlink escapes the workspace or references a reserved directory"),
        }
    }
    Ok(())
}

pub(crate) fn symlink(target: &str, path: &Path, directory: bool) -> Result<()> {
    #[cfg(unix)]
    {
        let _ = directory;
        std::os::unix::fs::symlink(target, path)?;
    }
    #[cfg(windows)]
    {
        if directory {
            std::os::windows::fs::symlink_dir(target, path)?;
        } else {
            std::os::windows::fs::symlink_file(target, path)?;
        }
    }
    Ok(())
}

pub(crate) fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

impl Cache {
    pub(crate) fn initialize(&self) -> Result<()> {
        for directory in ["entries", "blobs", "locks", "tmp"] {
            fs::create_dir_all(self.root.join(directory))?;
        }
        Ok(())
    }

    /// Takes the key's lock, reporting whether another holder made us wait.
    pub(crate) fn lock(&self, key: &str, cancelled: &AtomicBool) -> Result<Option<(File, bool)>> {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join("locks").join(key))?;
        let mut waited = false;
        loop {
            if cancelled.load(Ordering::SeqCst) {
                return Ok(None);
            }
            match FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(Some((file, waited))),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    waited = true;
                    std::thread::sleep(Duration::from_millis(20))
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    pub(crate) fn put_blob(&self, source: &Path) -> Result<String> {
        // Content the store already holds is not copied or flushed again: a
        // rebuild rewrites most outputs with what they held before. A stored
        // blob that no longer holds its digest's content, or cannot be read, is
        // replaced below, so that a rebuild after corruption repairs it.
        let digest = digest_file(source)?;
        let existing = self.root.join("blobs").join(&digest);
        if existing.is_file() && digest_file(&existing).is_ok_and(|held| held == digest) {
            return Ok(digest);
        }
        // fs::copy clones on copy-on-write filesystems when source and cache share
        // a volume, and falls back to a byte copy otherwise. Cloning needs a fresh
        // destination path, so copy into a private directory rather than a temp file.
        let staging = tempfile::tempdir_in(self.root.join("tmp"))?;
        let temporary = staging.path().join("blob");
        fs::copy(source, &temporary)?;
        // Blobs keep no meaningful mode of their own; restores apply the manifest's.
        set_mode(&temporary, if cfg!(unix) { 0o644 } else { 0 })?;
        // Windows flushes only through a handle open for writing.
        fs::OpenOptions::new()
            .write(true)
            .open(&temporary)?
            .sync_all()?;
        // Hash the copy, not the source, so a concurrent write cannot mislabel the blob.
        let hash = digest_file(&temporary)?;
        let target = self.root.join("blobs").join(&hash);
        // Replacing is atomic and cheaper than re-reading an existing blob. Where an
        // open blob cannot be replaced, keep it: restores verify it before use.
        if let Err(error) = fs::rename(&temporary, &target)
            && !target.is_file()
        {
            return Err(error.into());
        }
        Ok(hash)
    }

    pub(crate) fn publish(
        &self,
        root: &Path,
        key: &str,
        outputs: &Outputs,
        log: &Path,
    ) -> Result<String> {
        let mut artifacts = BTreeMap::new();
        for entry in outputs.entries(root)? {
            let (path, metadata) = entry?;
            paths::safe_parents(root, &path)?;
            let absolute = root.join(&path);
            let artifact = if metadata.file_type().is_symlink() {
                let target = fs::read_link(&absolute)?
                    .to_str()
                    .context("symlink target must be UTF-8")?
                    .to_owned();
                validate_link(&path, &target)?;
                Artifact::Symlink {
                    target,
                    directory: absolute.is_dir(),
                }
            } else if metadata.is_file() {
                Artifact::File {
                    blob: self.put_blob(&absolute)?,
                    mode: mode(&metadata),
                }
            } else if metadata.is_dir() {
                Artifact::Directory {
                    mode: mode(&metadata),
                }
            } else {
                bail!("cannot cache special output file {path}");
            };
            artifacts.insert(path, artifact);
        }
        let manifest = Manifest {
            version: 1,
            key: key.into(),
            log: self.put_blob(log)?,
            artifacts,
        };
        let mut file = NamedTempFile::new_in(self.root.join("tmp"))?;
        serde_json::to_writer(file.as_file_mut(), &manifest)?;
        file.flush()?;
        file.as_file().sync_all()?;
        file.persist(self.root.join("entries").join(format!("{key}.json")))?;
        manifest.output_fingerprint(outputs.declared())
    }

    /// Restores an entry's outputs and log, returning its output fingerprint.
    ///
    /// Restored files are stamped with the time the restore starts, which is
    /// after the task's dependencies finished and after its sources were
    /// written, so tools that compare modification times, such as `tsc -b`,
    /// see outputs newer than their inputs and dependents newer than their
    /// dependencies. Outputs a worktree already holds are kept as they are,
    /// unless a dependency's outputs changed in this run; then they are
    /// stamped again to stay newer.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn restore(
        &self,
        root: &Path,
        task: &str,
        dependencies: &std::collections::BTreeSet<String>,
        key: &str,
        outputs: &Outputs,
        display: &qk_executor::Display,
        shown: qk_executor::Shown,
    ) -> Result<Option<String>> {
        // Read whole: a File is unbuffered, and serde_json reads byte by byte.
        let bytes = match fs::read(self.root.join("entries").join(format!("{key}.json"))) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let manifest: Manifest = serde_json::from_slice(&bytes)?;
        if manifest.version != 1 || manifest.key != key || !valid_hash(&manifest.log) {
            bail!("invalid cache manifest");
        }
        let log = self.root.join("blobs").join(&manifest.log);
        if digest_file(&log)? != manifest.log {
            bail!("corrupt cached log");
        }
        qk_executor::read_capture(File::open(&log)?, |_, _| Ok(()))?;
        let started = SystemTime::now();
        // Outputs this worktree already holds for the key are left as they
        // are; when a stamp fails, they are restored anew.
        if outputs_unchanged(root, task, key, outputs) {
            let changed = {
                let kept = self.kept.lock().unwrap();
                dependencies
                    .iter()
                    .any(|dependency| !kept.contains(dependency))
            };
            if !changed || stamp_outputs(root, outputs, started).is_ok() {
                if changed {
                    record_outputs(root, task, key, outputs);
                } else {
                    self.kept.lock().unwrap().insert(task.to_owned());
                }
                qk_executor::replay(File::open(log)?, display, qk_executor::Shown::Kept)?;
                crate::evict::touch(&self.root.join("entries").join(format!("{key}.json")));
                return manifest.output_fingerprint(outputs.declared()).map(Some);
            }
        }
        // Stage on the destination filesystem; no existing output is touched yet.
        let stage_parent = paths::worktree_state(root).join("restore");
        fs::create_dir_all(&stage_parent)?;
        let stage = tempfile::Builder::new()
            .prefix("restore-")
            .tempdir_in(stage_parent)?;
        let mut staging = StagingDirectories::new(stage.path());
        for (path, artifact) in &manifest.artifacts {
            paths::safe_parents(root, path)?;
            staging.parents(path)?;
            if !outputs.matches(path) {
                bail!("cached artifact is outside declared outputs");
            }
            let destination = stage.path().join(path);
            match artifact {
                Artifact::File { blob, mode } => {
                    if !valid_hash(blob) {
                        bail!("invalid blob identifier");
                    }
                    let source = self.root.join("blobs").join(blob);
                    if digest_file(&source)? != *blob {
                        bail!("corrupt cached artifact");
                    }
                    // A clone where supported: edits to the restored file never reach the blob.
                    fs::copy(source, &destination)?;
                    // Before its mode, which may not let it be opened.
                    set_modified(&destination, started)?;
                    set_mode(&destination, *mode)?;
                }
                Artifact::Directory { .. } => staging.directory(path)?,
                Artifact::Symlink { target, directory } => {
                    validate_link(path, target)?;
                    symlink(target, &destination, *directory)?;
                }
            }
        }
        // Eligibility is determined before cleanup: replacing an existing output
        // tree keeps the established per-artifact behavior, even if cleanup empties it.
        let complete = absent_directories(root, outputs, &manifest.artifacts)?;
        let current = outputs.paths(root)?;
        // Validate the complete cleanup set before deleting any output.
        for path in &current {
            paths::safe_parents(root, path)?;
        }
        for path in current.iter().rev() {
            let absolute = root.join(path);
            let metadata = fs::symlink_metadata(&absolute)?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                match fs::remove_dir(&absolute) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
                    Err(error) => return Err(error.into()),
                }
            } else {
                #[cfg(windows)]
                if metadata.permissions().readonly() {
                    let mut permissions = metadata.permissions();
                    // The lint is about Unix modes; on Windows this only
                    // clears the read-only attribute so the file can be removed.
                    #[allow(clippy::permissions_set_readonly_false)]
                    permissions.set_readonly(false);
                    fs::set_permissions(&absolute, permissions)?;
                }
                fs::remove_file(absolute)?;
            }
        }
        let mut promoted = BTreeSet::new();
        for path in complete {
            paths::safe_parents(root, &path)?;
            let destination = root.join(&path);
            fs::create_dir_all(destination.parent().context("output has no parent")?)?;
            if promote_directory(&stage.path().join(&path), &destination)? {
                promoted.insert(path);
            }
        }
        for (path, artifact) in &manifest.artifacts {
            if promoted.iter().any(|parent| {
                path == parent
                    || path
                        .strip_prefix(parent)
                        .is_some_and(|suffix| suffix.starts_with('/'))
            }) {
                continue;
            }
            paths::safe_parents(root, path)?;
            let destination = root.join(path);
            fs::create_dir_all(destination.parent().context("output has no parent")?)?;
            if matches!(artifact, Artifact::Directory { .. }) {
                fs::create_dir_all(&destination)?;
            } else {
                move_staged(&stage.path().join(path), &destination)?;
            }
        }
        for (path, artifact) in manifest.artifacts.iter().rev() {
            if let Artifact::Directory { mode } = artifact {
                set_mode(&root.join(path), *mode)?;
            }
        }
        record_outputs(root, task, key, outputs);
        qk_executor::replay(File::open(log)?, display, shown)?;
        crate::evict::touch(&self.root.join("entries").join(format!("{key}.json")));
        manifest.output_fingerprint(outputs.declared()).map(Some)
    }
}

/// Directory creation is deduplicated only inside this restore's private
/// temporary stage. Staging never replaces directories with files or links;
/// such conflicting manifests fail. Destination validation is never cached.
struct StagingDirectories<'a> {
    root: &'a Path,
    directories: BTreeSet<String>,
}

impl<'a> StagingDirectories<'a> {
    fn new(root: &'a Path) -> Self {
        Self {
            root,
            directories: BTreeSet::new(),
        }
    }

    fn parents(&mut self, path: &str) -> Result<()> {
        paths::validate_path(path)?;
        if let Some((parent, _)) = path.rsplit_once('/') {
            self.directory(parent)?;
        }
        Ok(())
    }

    fn directory(&mut self, path: &str) -> Result<()> {
        paths::validate_path(path)?;
        let mut prefix = String::new();
        for part in path.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            if self.directories.contains(&prefix) {
                continue;
            }
            let absolute = self.root.join(&prefix);
            match fs::create_dir(&absolute) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let metadata = fs::symlink_metadata(&absolute)?;
                    if !metadata.is_dir() || metadata.file_type().is_symlink() {
                        bail!(
                            "cache staging parent is not a real directory: {}",
                            absolute.display()
                        );
                    }
                }
                Err(error) => return Err(error.into()),
            }
            self.directories.insert(prefix.clone());
        }
        Ok(())
    }
}

/// Complete staged directories whose destinations were absent before cleanup.
fn absent_directories(
    root: &Path,
    outputs: &Outputs,
    artifacts: &BTreeMap<String, Artifact>,
) -> Result<Vec<String>> {
    let mut roots = Vec::new();
    for path in outputs.complete_roots() {
        if !matches!(artifacts.get(path), Some(Artifact::Directory { .. })) {
            continue;
        }
        paths::safe_parents(root, path)?;
        match fs::symlink_metadata(root.join(path)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                roots.push(path.to_owned())
            }
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
    }
    Ok(roots)
}

/// Move a complete verified directory without replacing a destination that
/// appeared during staging. Unsupported or cross-device renames retain the
/// per-artifact path; unexpected I/O errors still fail restoration.
fn promote_directory(staged: &Path, destination: &Path) -> Result<bool> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use rustix::fs::{CWD, RenameFlags, renameat_with};
        match renameat_with(CWD, staged, CWD, destination, RenameFlags::NOREPLACE) {
            Ok(()) => Ok(true),
            Err(
                rustix::io::Errno::EXIST
                | rustix::io::Errno::XDEV
                | rustix::io::Errno::NOSYS
                | rustix::io::Errno::INVAL
                | rustix::io::Errno::OPNOTSUPP,
            ) => Ok(false),
            Err(error) => Err(std::io::Error::from(error).into()),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (staged, destination);
        Ok(false)
    }
}

/// Where a worktree records the outputs it holds for a task, since each
/// worktree has its own outputs.
fn outputs_record(root: &Path, task: &str) -> std::path::PathBuf {
    let name = blake3::hash(task.as_bytes()).to_hex();
    paths::worktree_state(root)
        .join("outputs")
        .join(format!("{}.json", &name[..32]))
}

/// Sets a file's modification time. Unix needs a handle only to read it;
/// Windows one that may write its attributes.
fn set_modified(path: &Path, time: SystemTime) -> Result<()> {
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
        fs::OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .open(path)?
    };
    #[cfg(not(windows))]
    let file = File::open(path)?;
    file.set_modified(time)
        .with_context(|| format!("cannot set the modification time of {}", path.display()))
}

/// Stamps every output file with `time`. Links are left alone, since setting
/// a time through one would change the file it points to.
fn stamp_outputs(root: &Path, outputs: &Outputs, time: SystemTime) -> Result<()> {
    for path in outputs.paths(root)? {
        let absolute = root.join(&path);
        if fs::symlink_metadata(&absolute)?.is_file() {
            set_modified(&absolute, time)?;
        }
    }
    Ok(())
}

/// Moves a staged file or link into place, copying when the worktree's state
/// is on another filesystem than the worktree.
fn move_staged(staged: &Path, destination: &Path) -> Result<()> {
    match fs::rename(staged, destination) {
        Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => {
            let metadata = fs::symlink_metadata(staged)?;
            if metadata.file_type().is_symlink() {
                let target = fs::read_link(staged)?;
                symlink(
                    target.to_str().context("symlink target must be UTF-8")?,
                    destination,
                    fs::metadata(staged).is_ok_and(|metadata| metadata.is_dir()),
                )?;
            } else {
                fs::copy(staged, destination)?;
            }
            Ok(())
        }
        result => Ok(result?),
    }
}

/// Each output path with metadata that changes whenever it is written,
/// replaced, removed or recreated.
fn output_stamps(root: &Path, outputs: &Outputs) -> Result<BTreeMap<String, Vec<i64>>> {
    let mut stamps = BTreeMap::new();
    for entry in outputs.entries(root)? {
        let (path, metadata) = entry?;
        let kind = if metadata.file_type().is_symlink() {
            2
        } else if metadata.is_dir() {
            1
        } else {
            0
        };
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(-1, |duration| duration.as_nanos() as i64);
        // Unix adds the change time, inode and mode below.
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut stamp = vec![
            kind,
            metadata.len() as i64,
            modified,
            i64::from(mode(&metadata)),
        ];
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            stamp.extend([
                metadata.ctime(),
                metadata.ctime_nsec(),
                metadata.ino() as i64,
            ]);
        }
        stamps.insert(path, stamp);
    }
    Ok(stamps)
}

/// Records the outputs a worktree now holds for a task's key.
pub(crate) fn record_outputs(root: &Path, task: &str, key: &str, outputs: &Outputs) {
    let record = output_stamps(root, outputs)
        .map(|stamps| serde_json::json!({"key": key, "outputs": stamps}));
    let path = outputs_record(root, task);
    if let Ok(record) = record
        && fs::create_dir_all(path.parent().expect("records have a directory")).is_ok()
    {
        let _ = fs::write(path, record.to_string());
    }
}

/// Whether the outputs on disk are exactly those this worktree recorded for
/// the key: the same paths, none written since.
fn outputs_unchanged(root: &Path, task: &str, key: &str, outputs: &Outputs) -> bool {
    let Ok(text) = fs::read(outputs_record(root, task)) else {
        return false;
    };
    let Ok(record) = serde_json::from_slice::<serde_json::Value>(&text) else {
        return false;
    };
    if record.get("key").and_then(serde_json::Value::as_str) != Some(key) {
        return false;
    }
    let Ok(current) = output_stamps(root, outputs) else {
        return false;
    };
    serde_json::to_value(current).ok().as_ref() == record.get("outputs")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Private staging rejects unsafe paths and file/link ancestors rather
    /// than following them, while shared ordinary parents can be reused.
    #[test]
    fn staging_directory_reuse_rejects_conflicting_manifest_parents() {
        let temp = tempfile::tempdir().unwrap();
        let mut staging = StagingDirectories::new(temp.path());
        staging.parents("dist/nested/a").unwrap();
        staging.parents("dist/nested/b").unwrap();
        staging.directory("dist/nested/empty").unwrap();
        assert!(temp.path().join("dist/nested/empty").is_dir());
        fs::write(temp.path().join("file"), "keep").unwrap();
        assert!(staging.parents("file/child").is_err());
        assert!(staging.parents("../escape").is_err());
        assert!(staging.parents("dist/../escape").is_err());
        assert_eq!(
            fs::read_to_string(temp.path().join("file")).unwrap(),
            "keep"
        );
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(outside.path(), temp.path().join("link")).unwrap();
            assert!(staging.parents("link/child").is_err());
            assert!(!outside.path().join("child").exists());
        }
    }

    /// A no-replace move must leave every kind of existing destination intact.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn directory_promotion_never_replaces_a_destination() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("staged");
        let destination = temp.path().join("dist");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("output"), "cached").unwrap();
        fs::create_dir(&destination).unwrap();
        assert!(!promote_directory(&source, &destination).unwrap());
        assert!(destination.is_dir());
        assert!(fs::read_dir(&destination).unwrap().next().is_none());
        fs::write(destination.join("keep"), "unrelated").unwrap();
        assert!(!promote_directory(&source, &destination).unwrap());
        assert_eq!(
            fs::read_to_string(destination.join("keep")).unwrap(),
            "unrelated"
        );
        fs::remove_dir_all(&destination).unwrap();
        fs::write(&destination, "file").unwrap();
        assert!(!promote_directory(&source, &destination).unwrap());
        assert_eq!(fs::read_to_string(&destination).unwrap(), "file");
        fs::remove_file(&destination).unwrap();
        std::os::unix::fs::symlink("absent", &destination).unwrap();
        assert!(!promote_directory(&source, &destination).unwrap());
        assert_eq!(fs::read_link(&destination).unwrap(), Path::new("absent"));
        fs::remove_file(&destination).unwrap();
        assert!(promote_directory(&source, &destination).unwrap());
        assert_eq!(
            fs::read_to_string(destination.join("output")).unwrap(),
            "cached"
        );
        assert!(!source.exists());
    }

    /// Complete roots require a cached directory and an absent destination.
    #[test]
    fn directory_candidates_require_absent_manifest_directories() {
        let temp = tempfile::tempdir().unwrap();
        let outputs = Outputs::from_paths(&[
            "a".into(),
            "ab".into(),
            "file".into(),
            "partial/*.txt".into(),
        ])
        .unwrap();
        let artifacts = BTreeMap::from([
            ("a".into(), Artifact::Directory { mode: 0o755 }),
            ("ab".into(), Artifact::Directory { mode: 0o755 }),
            (
                "file".into(),
                Artifact::File {
                    blob: "0".repeat(64),
                    mode: 0o644,
                },
            ),
            ("partial".into(), Artifact::Directory { mode: 0o755 }),
        ]);
        assert_eq!(
            absent_directories(temp.path(), &outputs, &artifacts).unwrap(),
            ["a", "ab"]
        );
        fs::create_dir(temp.path().join("a")).unwrap();
        assert_eq!(
            absent_directories(temp.path(), &outputs, &artifacts).unwrap(),
            ["ab"]
        );
    }

    /// Every blob is verified before any output is cleaned or promoted.
    #[test]
    fn corrupt_late_blob_leaves_existing_and_absent_roots_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("a")).unwrap();
        fs::create_dir(root.join("z")).unwrap();
        fs::write(root.join("a/value"), "first").unwrap();
        fs::write(root.join("z/value"), "last").unwrap();
        let log = root.join("log");
        fs::write(&log, "").unwrap();
        let cache = Cache::new(root.join(".qk/cache"));
        cache.initialize().unwrap();
        let outputs = Outputs::from_patterns(&["a", "z"]);
        let key = "0".repeat(64);
        cache.publish(root, &key, &outputs, &log).unwrap();
        fs::write(root.join("a/value"), "keep me").unwrap();
        fs::remove_dir_all(root.join("z")).unwrap();
        let blob = blake3::hash(b"last").to_hex().to_string();
        fs::write(cache.root.join("blobs").join(blob), "corrupt").unwrap();
        assert!(
            cache
                .restore(
                    root,
                    "build",
                    &Default::default(),
                    &key,
                    &outputs,
                    &qk_executor::Display::Hidden,
                    qk_executor::Shown::LocalCache
                )
                .is_err()
        );
        assert_eq!(fs::read_to_string(root.join("a/value")).unwrap(), "keep me");
        assert!(!root.join("z").exists());
    }
}
