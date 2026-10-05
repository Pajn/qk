use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::{
    Cache,
    hash::digest_file,
    paths::{self, Outputs},
    record::{self, Artifact, ResultRecord, valid_hash, validate_link},
};

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

// Keep each worker's snapshot bounded, including when a source grows during a save.
const SMALL_BLOB_LIMIT: usize = 64 * 1024;

fn small_blob_snapshot(reader: impl Read) -> Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    reader
        .take(SMALL_BLOB_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok((bytes.len() <= SMALL_BLOB_LIMIT).then_some(bytes))
}

fn copy_blob(source: &Path, destination: &Path) -> Result<String> {
    let hash = copy_blob_contents(source, destination)?;
    // Blobs carry no meaningful mode; restores apply the manifest's mode.
    set_mode(destination, if cfg!(unix) { 0o644 } else { 0 })?;
    Ok(hash)
}

fn copy_blob_contents(source: &Path, destination: &Path) -> Result<String> {
    let bytes = if fs::metadata(source)?.len() <= SMALL_BLOB_LIMIT as u64 {
        small_blob_snapshot(File::open(source)?)?
    } else {
        None
    };
    let hash = if let Some(bytes) = bytes {
        // Small files cost more to clone and re-read than to copy from a bounded
        // snapshot. Hash exactly the bytes written, even if the source changes.
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)?;
        file.write_all(&bytes)?;
        Some(blake3::hash(&bytes).to_hex().to_string())
    } else {
        // Larger files retain copy-on-write cloning where the filesystem supports it.
        fs::copy(source, destination)?;
        None
    };
    // Disk flushing stays with the OS. Hash the large-file copy, never its source.
    match hash {
        Some(hash) => Ok(hash),
        None => digest_file(destination),
    }
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
        self.put_blob_in(source, &mut None, 0)
    }

    fn put_blob_in(
        &self,
        source: &Path,
        staging: &mut Option<tempfile::TempDir>,
        index: usize,
    ) -> Result<String> {
        // Content the store already holds is not copied again: a
        // rebuild rewrites most outputs with what they held before. A stored
        // blob that no longer holds its digest's content, or cannot be read, is
        // replaced below, so that a rebuild after corruption repairs it.
        let digest = digest_file(source)?;
        let existing = self.root.join("blobs").join(&digest);
        if existing.is_file() && digest_file(&existing).is_ok_and(|held| held == digest) {
            return Ok(digest);
        }
        // Stage each snapshot privately before atomic publication. Both cloning
        // and small-file creation need a fresh destination path.
        if staging.is_none() {
            *staging = Some(tempfile::tempdir_in(self.root.join("tmp"))?);
        }
        // A worker keeps one private directory, but every clone needs a fresh
        // destination even when an earlier copy or rename failed.
        let temporary = staging.as_ref().unwrap().path().join(index.to_string());
        let hash = copy_blob(source, &temporary)?;
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

    /// Stores independent blobs with bounded I/O concurrency, retaining input order.
    pub(crate) fn put_blobs(&self, sources: &[PathBuf]) -> Vec<Result<String>> {
        // Bound parallel copies and avoid threads for small saves.
        let workers = sources.len().div_ceil(64).clamp(1, 8);
        if workers == 1 {
            return sources.iter().map(|source| self.put_blob(source)).collect();
        }
        std::thread::scope(|scope| {
            let handles: Vec<_> = sources
                .chunks(sources.len().div_ceil(workers))
                .map(|chunk| {
                    scope.spawn(move || {
                        let mut staging = None;
                        chunk
                            .iter()
                            .enumerate()
                            .map(|(index, source)| self.put_blob_in(source, &mut staging, index))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|handle| handle.join().expect("blob writer panicked"))
                .collect()
        })
    }

    pub(crate) fn publish(
        &self,
        root: &Path,
        key: &str,
        outputs: &Outputs,
        log: &Path,
    ) -> Result<String> {
        let mut artifacts = BTreeMap::new();
        let mut pending = Vec::new();
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
                pending.push((path, absolute, mode(&metadata)));
                continue;
            } else if metadata.is_dir() {
                Artifact::Directory {
                    mode: mode(&metadata),
                }
            } else {
                bail!("cannot cache special output file {path}");
            };
            artifacts.insert(path, artifact);
        }
        let mut sources: Vec<_> = pending
            .iter()
            .map(|(_, source, _)| source.clone())
            .collect();
        sources.push(log.to_owned());
        // Every worker finishes before any manifest is published.
        let mut blobs = self.put_blobs(&sources).into_iter();
        for (path, _, mode) in pending {
            artifacts.insert(
                path,
                Artifact::File {
                    blob: blobs.next().expect("output blob result")?,
                    mode,
                },
            );
        }
        let manifest = ResultRecord::new(
            key.into(),
            blobs.next().expect("log blob result")?,
            artifacts,
        );
        record::publish(
            &self.root,
            &self.root.join("entries").join(format!("{key}.json")),
            &manifest,
        )?;
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
        let manifest = ResultRecord::read(&bytes, key)?;
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
        let mut files = Vec::new();
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
                    files.push((blob.as_str(), *mode, destination));
                }
                Artifact::Directory { .. } => staging.directory(path)?,
                Artifact::Symlink { target, directory } => {
                    validate_link(path, target)?;
                    symlink(target, &destination, *directory)?;
                }
            }
        }
        self.stage_files(&files, started)?;
        // Only complete directory selections can promote a staged tree.
        // NOREPLACE below still checks that cleanup left the destination absent.
        let complete = complete_directories(root, outputs, &manifest.artifacts)?;
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
    /// Fills an already validated private stage before output cleanup begins.
    fn stage_files(&self, files: &[(&str, u32, PathBuf)], time: SystemTime) -> Result<()> {
        let stage = |files: &[(&str, u32, PathBuf)]| -> Result<()> {
            for (blob, mode, destination) in files {
                let source = self.root.join("blobs").join(blob);
                if copy_blob_contents(&source, destination)? != *blob {
                    bail!("corrupt cached artifact");
                }
                // Before its mode, which may prevent opening the file.
                set_modified(destination, time)?;
                set_mode(destination, *mode)?;
            }
            Ok(())
        };
        let workers = files.len().div_ceil(64).clamp(1, 4);
        if workers == 1 {
            return stage(files);
        }
        std::thread::scope(|scope| {
            let handles: Vec<_> = files
                .chunks(files.len().div_ceil(workers))
                .map(|chunk| scope.spawn(move || stage(chunk)))
                .collect();
            // Join every worker, including after an error, before the private
            // stage can be removed or any destination output can be changed.
            let results: Vec<_> = handles
                .into_iter()
                .map(|handle| handle.join().expect("cache staging worker panicked"))
                .collect();
            for result in results {
                result?;
            }
            Ok(())
        })
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

/// Complete staged directories whose destinations are absent or real directories.
fn complete_directories(
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
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                roots.push(path.to_owned());
            }
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

#[derive(Serialize, Deserialize)]
struct OutputRecord {
    key: String,
    outputs: BTreeMap<String, Vec<i64>>,
}

/// Records the outputs a worktree now holds for a task's key.
pub(crate) fn record_outputs(root: &Path, task: &str, key: &str, outputs: &Outputs) {
    let record = output_stamps(root, outputs).and_then(|stamps| {
        Ok(serde_json::to_vec(&OutputRecord {
            key: key.to_owned(),
            outputs: stamps,
        })?)
    });
    let path = outputs_record(root, task);
    if let Ok(record) = record
        && fs::create_dir_all(path.parent().expect("records have a directory")).is_ok()
    {
        let _ = fs::write(path, record);
    }
}

/// Whether the outputs on disk are exactly those this worktree recorded for
/// the key: the same paths, none written since.
fn outputs_unchanged(root: &Path, task: &str, key: &str, outputs: &Outputs) -> bool {
    let Ok(text) = fs::read(outputs_record(root, task)) else {
        return false;
    };
    let Ok(record) = serde_json::from_slice::<OutputRecord>(&text) else {
        return false;
    };
    if record.key != key {
        return false;
    }
    let Ok(current) = output_stamps(root, outputs) else {
        return false;
    };
    current == record.outputs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_snapshot_rejects_growth_without_reading_the_entire_stream() {
        let mut reader = std::io::Cursor::new(vec![42; SMALL_BLOB_LIMIT * 4]);
        assert!(small_blob_snapshot(&mut reader).unwrap().is_none());
        assert_eq!(reader.position(), SMALL_BLOB_LIMIT as u64 + 1);
    }

    #[test]
    fn both_blob_copy_paths_preserve_content_and_normalize_permissions() {
        let scratch = tempfile::tempdir().unwrap();
        for length in [
            0,
            1,
            SMALL_BLOB_LIMIT - 1,
            SMALL_BLOB_LIMIT,
            SMALL_BLOB_LIMIT + 1,
            SMALL_BLOB_LIMIT * 4,
        ] {
            let source = scratch.path().join(format!("source-{length}"));
            let destination = scratch.path().join(format!("copy-{length}"));
            let bytes: Vec<_> = (0..length).map(|index| (index % 251) as u8).collect();
            fs::write(&source, &bytes).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&source, fs::Permissions::from_mode(0o754)).unwrap();
            }
            #[cfg(windows)]
            {
                let mut permissions = fs::metadata(&source).unwrap().permissions();
                permissions.set_readonly(true);
                fs::set_permissions(&source, permissions).unwrap();
            }
            let digest = copy_blob(&source, &destination).unwrap();
            assert_eq!(fs::read(&destination).unwrap(), bytes);
            assert_eq!(digest_file(&destination).unwrap(), digest);
            assert_eq!(
                mode(&fs::metadata(&destination).unwrap()),
                if cfg!(unix) { 0o644 } else { 0 }
            );
            // Subsequent source writes cannot change the stored snapshot.
            #[cfg(windows)]
            fs::set_permissions(&source, fs::metadata(&destination).unwrap().permissions())
                .unwrap();
            fs::write(&source, b"changed after save").unwrap();
            assert_eq!(fs::read(&destination).unwrap(), bytes);
        }
    }

    #[test]
    fn changing_small_sources_never_mislabels_a_published_blob() {
        let scratch = tempfile::tempdir().unwrap();
        let source = scratch.path().join("source");
        fs::write(&source, vec![1; 8192]).unwrap();
        let cache = Cache::new(scratch.path().join("cache"));
        cache.initialize().unwrap();
        let stop = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                let mut value = 2;
                while !stop.load(Ordering::Relaxed) {
                    fs::write(&source, vec![value; 8192]).unwrap();
                    value = if value == 2 { 3 } else { 2 };
                }
            });
            let results: Vec<_> = (0..64).map(|_| cache.put_blob(&source)).collect();
            stop.store(true, Ordering::Relaxed);
            writer.join().unwrap();
            for result in results {
                let digest = result.unwrap();
                assert_eq!(
                    digest_file(&cache.root.join("blobs").join(&digest)).unwrap(),
                    digest
                );
            }
        });
    }

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

    /// Files and symlinks cannot be promoted as complete directories.
    #[test]
    fn directory_candidates_require_manifest_directories() {
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
            complete_directories(temp.path(), &outputs, &artifacts).unwrap(),
            ["a", "ab"]
        );
        fs::create_dir(temp.path().join("a")).unwrap();
        assert_eq!(
            complete_directories(temp.path(), &outputs, &artifacts).unwrap(),
            ["a", "ab"]
        );
        fs::remove_dir(temp.path().join("a")).unwrap();
        fs::write(temp.path().join("a"), "existing file").unwrap();
        assert_eq!(
            complete_directories(temp.path(), &outputs, &artifacts).unwrap(),
            ["ab"]
        );
    }

    #[test]
    fn parallel_staging_preserves_contents_modes_links_and_shared_restore_time() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("out/empty")).unwrap();
        for index in 0..130 {
            let path = root.join(format!("out/file-{index:03}"));
            fs::write(&path, format!("content-{}", index % 65)).unwrap();
            #[cfg(unix)]
            set_mode(&path, if index % 2 == 0 { 0o755 } else { 0o640 }).unwrap();
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink("file-000", root.join("out/link")).unwrap();
        let log = root.join("log");
        fs::write(&log, "").unwrap();
        let cache = Cache::new(root.join("cache"));
        cache.initialize().unwrap();
        let outputs = Outputs::from_patterns(&["out"]);
        let key = "a".repeat(64);
        let expected = cache.publish(root, &key, &outputs, &log).unwrap();
        fs::write(root.join("unselected"), "keep").unwrap();
        for existing in [false, true] {
            if existing {
                fs::write(root.join("out/file-000"), "edited").unwrap();
                fs::create_dir_all(root.join("out/stale/nested")).unwrap();
                fs::write(root.join("out/stale/nested/value"), "stale").unwrap();
            } else {
                fs::remove_dir_all(root.join("out")).unwrap();
            }
            let before = SystemTime::now();
            assert_eq!(
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
                    .unwrap(),
                Some(expected.clone())
            );
            let mut common_time = None;
            for index in 0..130 {
                let path = root.join(format!("out/file-{index:03}"));
                assert_eq!(
                    fs::read_to_string(&path).unwrap(),
                    format!("content-{}", index % 65)
                );
                let metadata = fs::metadata(&path).unwrap();
                let modified = metadata.modified().unwrap();
                assert!(modified >= before);
                assert_eq!(*common_time.get_or_insert(modified), modified);
                #[cfg(unix)]
                assert_eq!(mode(&metadata), if index % 2 == 0 { 0o755 } else { 0o640 });
            }
            assert!(root.join("out/empty").is_dir());
            #[cfg(unix)]
            assert_eq!(
                fs::read_link(root.join("out/link")).unwrap(),
                Path::new("file-000")
            );
            assert!(!root.join("out/stale").exists());
            assert_eq!(fs::read_to_string(root.join("unselected")).unwrap(), "keep");
        }
    }

    #[test]
    fn a_corrupt_parallel_blob_leaves_all_existing_and_absent_outputs_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("a")).unwrap();
        fs::create_dir(root.join("z")).unwrap();
        for index in 0..130 {
            fs::write(
                root.join(format!("a/file-{index:03}")),
                format!("content-{index}"),
            )
            .unwrap();
        }
        fs::write(root.join("z/value"), "last").unwrap();
        let log = root.join("log");
        fs::write(&log, "").unwrap();
        let cache = Cache::new(root.join("cache"));
        cache.initialize().unwrap();
        let outputs = Outputs::from_patterns(&["a", "z"]);
        let key = "b".repeat(64);
        cache.publish(root, &key, &outputs, &log).unwrap();
        for index in 0..130 {
            fs::write(
                root.join(format!("a/file-{index:03}")),
                format!("keep-{index}"),
            )
            .unwrap();
        }
        fs::remove_dir_all(root.join("z")).unwrap();
        fs::write(
            cache
                .root
                .join("blobs")
                .join(blake3::hash(b"last").to_hex().to_string()),
            "corrupt",
        )
        .unwrap();
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
        for index in 0..130 {
            assert_eq!(
                fs::read_to_string(root.join(format!("a/file-{index:03}"))).unwrap(),
                format!("keep-{index}")
            );
        }
        assert!(!root.join("z").exists());
        assert_eq!(
            fs::read_dir(paths::worktree_state(root).join("restore"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn delayed_file_staging_rejects_file_parent_conflicts_without_cleaning_outputs() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("out")).unwrap();
        fs::write(root.join("out/keep"), "existing").unwrap();
        let source = root.join("source");
        fs::write(&source, "cached").unwrap();
        let log = root.join("log");
        fs::write(&log, "").unwrap();
        let cache = Cache::new(root.join("cache"));
        cache.initialize().unwrap();
        let blob = cache.put_blob(&source).unwrap();
        let key = "c".repeat(64);
        let manifest = ResultRecord::new(
            key.clone(),
            cache.put_blob(&log).unwrap(),
            BTreeMap::from([
                (
                    "out/file".to_owned(),
                    Artifact::File {
                        blob: blob.clone(),
                        mode: 0o644,
                    },
                ),
                (
                    "out/file/child".to_owned(),
                    Artifact::File { blob, mode: 0o644 },
                ),
            ]),
        );
        fs::write(
            cache.root.join("entries").join(format!("{key}.json")),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let outputs = Outputs::from_patterns(&["out"]);
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
        assert_eq!(
            fs::read_to_string(root.join("out/keep")).unwrap(),
            "existing"
        );
        assert!(!root.join("out/file").exists());
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

    #[test]
    fn parallel_publication_keeps_artifacts_log_and_fingerprints_deterministic() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("dist/empty")).unwrap();
        for index in 0..130 {
            let path = root.join(format!("dist/file-{index:03}"));
            fs::write(&path, format!("content-{}", index % 65)).unwrap();
            #[cfg(unix)]
            set_mode(&path, if index % 2 == 0 { 0o755 } else { 0o640 }).unwrap();
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink("file-000", root.join("dist/link")).unwrap();
        let log = root.join("task.log");
        let payload = b"saved task output\n";
        let mut capture = vec![0];
        capture.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        capture.extend_from_slice(payload);
        fs::write(&log, &capture).unwrap();
        let cache = Cache::new(root.join("cache"));
        cache.initialize().unwrap();
        let outputs = Outputs::from_patterns(&["dist"]);
        let key = "a".repeat(64);
        let first = cache.publish(root, &key, &outputs, &log).unwrap();
        let entry = cache.root.join("entries").join(format!("{key}.json"));
        let manifest = fs::read(&entry).unwrap();
        assert_eq!(cache.publish(root, &key, &outputs, &log).unwrap(), first);
        assert_eq!(fs::read(&entry).unwrap(), manifest);
        // Concurrent publishers share duplicate blobs but keep independent entries.
        std::thread::scope(|scope| {
            for digit in ['b', 'c'] {
                let (cache, outputs, log) = (&cache, &outputs, &log);
                scope.spawn(move || {
                    cache
                        .publish(root, &digit.to_string().repeat(64), outputs, log)
                        .unwrap()
                });
            }
        });
        fs::remove_dir_all(root.join("dist")).unwrap();
        assert_eq!(
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
                .unwrap(),
            Some(first)
        );
        for index in 0..130 {
            let path = root.join(format!("dist/file-{index:03}"));
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                format!("content-{}", index % 65)
            );
            #[cfg(unix)]
            assert_eq!(
                mode(&fs::metadata(&path).unwrap()),
                if index % 2 == 0 { 0o755 } else { 0o640 }
            );
        }
        assert!(root.join("dist/empty").is_dir());
        #[cfg(unix)]
        assert_eq!(
            fs::read_link(root.join("dist/link")).unwrap(),
            Path::new("file-000")
        );
        let parsed: ResultRecord = serde_json::from_slice(&manifest).unwrap();
        assert_eq!(
            fs::read(cache.root.join("blobs").join(parsed.log)).unwrap(),
            capture
        );
    }

    #[test]
    fn failed_blob_batch_never_publishes_or_replaces_a_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("dist")).unwrap();
        for index in 0..130 {
            fs::write(
                root.join(format!("dist/file-{index}")),
                format!("value-{index}"),
            )
            .unwrap();
        }
        let cache = Cache::new(root.join("cache"));
        cache.initialize().unwrap();
        let outputs = Outputs::from_patterns(&["dist"]);
        let log = root.join("task.log");
        let key = "d".repeat(64);
        assert!(cache.publish(root, &key, &outputs, &log).is_err());
        assert!(
            !cache
                .root
                .join("entries")
                .join(format!("{key}.json"))
                .exists()
        );
        fs::write(&log, "complete\n").unwrap();
        cache.publish(root, &key, &outputs, &log).unwrap();
        let entry = cache.root.join("entries").join(format!("{key}.json"));
        let original = fs::read(&entry).unwrap();
        fs::remove_file(&log).unwrap();
        fs::write(root.join("dist/file-0"), "changed").unwrap();
        assert!(cache.publish(root, &key, &outputs, &log).is_err());
        assert_eq!(fs::read(&entry).unwrap(), original);
    }
    #[test]
    fn worker_staging_is_cleaned_after_a_failed_blob_without_affecting_later_copies() {
        let temp = tempfile::tempdir().unwrap();
        let cache = Cache::new(temp.path().join("cache"));
        cache.initialize().unwrap();
        let sources: Vec<_> = (0..130)
            .map(|index| {
                let source = temp.path().join(format!("source-{index}"));
                fs::write(&source, format!("content-{index}")).unwrap();
                source
            })
            .collect();
        let blocked = digest_file(&sources[64]).unwrap();
        fs::create_dir(cache.root.join("blobs").join(&blocked)).unwrap();
        let results = cache.put_blobs(&sources);
        assert_eq!(results.len(), sources.len());
        for (index, result) in results.into_iter().enumerate() {
            if index == 64 {
                assert!(result.is_err());
                continue;
            }
            let blob = result.unwrap();
            assert_eq!(
                fs::read(cache.root.join("blobs").join(blob)).unwrap(),
                fs::read(&sources[index]).unwrap()
            );
        }
        assert_eq!(fs::read_dir(cache.root.join("tmp")).unwrap().count(), 0);
        // Existing blobs do not require a staging directory, and the failed
        // source can be saved normally once its target is available.
        fs::remove_dir(cache.root.join("blobs").join(blocked)).unwrap();
        assert!(
            cache
                .put_blobs(&sources)
                .into_iter()
                .all(|result| result.is_ok())
        );
        assert_eq!(fs::read_dir(cache.root.join("tmp")).unwrap().count(), 0);
    }
}
